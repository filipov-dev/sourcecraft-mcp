//! Маскирование значений секретов в потоковом выводе дочерней команды (`secret exec`).
//!
//! Алгоритм ([`Masker`]):
//!
//! - Ищутся все вхождения всех строк-образцов, в том числе перекрывающиеся. Пересекающиеся
//!   и соседние вхождения сливаются в одну область, и вся область заменяется одним [`MASK`]:
//!   так при перекрытии двух значений не остаётся видимого «хвоста» ни одного из них.
//! - Вывод приходит кусками. Позиция в буфере «решена», если любой образец, начинающийся
//!   в ней, либо целиком помещается в буфер, либо уже не совпадает. Нерешённый хвост
//!   (он короче самого длинного образца и совпадает с началом какого-то образца) переносится
//!   в следующий кусок, поэтому значение, разрезанное границей кусков, всё равно маскируется.
//!   Хвост, который не может быть началом значения, выводится сразу: приглашения без
//!   перевода строки не задерживаются.
//! - Открытая область маски тоже переносится между кусками: [`MASK`] выводится один раз.
//!
//! Маскирование — страховка от случайной утечки, а не граница безопасности: значение,
//! преобразованное дочерней командой (hex, URL-кодирование, по символу в строке), не
//! распознаётся.

use base64::Engine;
use base64::engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Чем заменяется значение секрета в выводе.
pub const MASK: &[u8] = b"***";

/// Значения короче этого не маскируются: короткая строка совпадёт с чем угодно и испортит
/// вывод (как `MIN_SCRUB_LEN` в ядре).
pub const MIN_MASK_LEN: usize = 4;

/// Минимальная длина отдельной строки многострочного значения, которая маскируется сама
/// по себе (строки ключа PEM и т. п.).
const MIN_LINE_LEN: usize = 8;

/// Размер буфера чтения вывода дочерней команды.
const READ_BUF: usize = 8 * 1024;

/// Строки, которые нужно маскировать для одного значения: само значение, оно же без
/// завершающих пробельных символов, их base64 (стандартный и URL-safe алфавиты без `=`)
/// и отдельные строки многострочного значения. Для значения короче [`MIN_MASK_LEN`] —
/// пусто.
pub fn needles_for(value: &[u8]) -> Vec<Vec<u8>> {
    if value.len() < MIN_MASK_LEN {
        return Vec::new();
    }
    let trimmed = value.trim_ascii_end();
    let mut needles = vec![value.to_vec(), trimmed.to_vec()];
    for form in [value, trimmed] {
        // Без `=`: так находится и base64 внутри более длинной строки, и с паддингом.
        needles.push(STANDARD_NO_PAD.encode(form).into_bytes());
        needles.push(URL_SAFE_NO_PAD.encode(form).into_bytes());
    }
    if trimmed.contains(&b'\n') {
        for line in trimmed.split(|b| *b == b'\n') {
            let line = line.trim_ascii();
            if line.len() >= MIN_LINE_LEN {
                needles.push(line.to_vec());
            }
        }
    }
    needles.retain(|needle| needle.len() >= MIN_MASK_LEN);
    needles.sort_unstable();
    needles.dedup();
    needles
}

/// Потоковый маскировщик. Не `Clone`/`Debug`: внутри значения секретов.
pub struct Masker {
    needles: Vec<Vec<u8>>,
    /// Байты, с которых начинается хотя бы один образец (быстрый пропуск остальных).
    starts: [bool; 256],
    max_len: usize,
    /// Нерешённый хвост предыдущего куска.
    carry: Vec<u8>,
    /// До какой позиции (относительно начала `carry`) байты уже скрыты открытой областью.
    masked_until: usize,
    /// Область маски открыта: [`MASK`] для неё уже выведен.
    region_open: bool,
}

impl Masker {
    pub fn new(needles: impl IntoIterator<Item = Vec<u8>>) -> Self {
        let needles: Vec<Vec<u8>> = needles.into_iter().filter(|n| !n.is_empty()).collect();
        let mut starts = [false; 256];
        for needle in &needles {
            starts[usize::from(needle[0])] = true;
        }
        let max_len = needles.iter().map(Vec::len).max().unwrap_or(0);
        Self {
            needles,
            starts,
            max_len,
            carry: Vec::new(),
            masked_until: 0,
            region_open: false,
        }
    }

    /// Обрабатывает очередной кусок; возвращает то, что уже можно вывести.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.process(chunk, false)
    }

    /// Конец потока: выводит остаток.
    pub fn finish(&mut self) -> Vec<u8> {
        self.process(&[], true)
    }

    fn process(&mut self, chunk: &[u8], eof: bool) -> Vec<u8> {
        let mut buf = std::mem::take(&mut self.carry);
        buf.extend_from_slice(chunk);
        let limit = if eof {
            buf.len()
        } else {
            self.decided_len(&buf)
        };

        let mut out = Vec::with_capacity(limit + MASK.len());
        for pos in 0..limit {
            if self.starts[usize::from(buf[pos])] {
                let rest = &buf[pos..];
                for needle in &self.needles {
                    if rest.starts_with(needle) {
                        if !self.region_open {
                            out.extend_from_slice(MASK);
                            self.region_open = true;
                        }
                        self.masked_until = self.masked_until.max(pos + needle.len());
                    }
                }
            }
            if pos < self.masked_until {
                continue;
            }
            self.region_open = false;
            out.push(buf[pos]);
        }

        if eof {
            self.masked_until = 0;
            self.region_open = false;
        } else {
            self.masked_until = self.masked_until.saturating_sub(limit);
            self.carry = buf.split_off(limit);
        }
        out
    }

    /// Длина решённой части буфера: первая позиция, с которой хвост буфера — собственный
    /// префикс какого-то образца (дальше без следующего куска решить нельзя).
    fn decided_len(&self, buf: &[u8]) -> usize {
        let from = buf.len().saturating_sub(self.max_len.saturating_sub(1));
        (from..buf.len())
            .find(|&pos| {
                let tail = &buf[pos..];
                self.needles
                    .iter()
                    .any(|needle| needle.len() > tail.len() && needle.starts_with(tail))
            })
            .unwrap_or(buf.len())
    }
}

/// Копирует поток `reader` в `writer`, маскируя значения. Ошибка записи (например, закрытый
/// канал у `| head`) прекращает копирование: `reader` закрывается, как в обычном конвейере.
pub async fn pump<R, W>(mut reader: R, mut writer: W, mut masker: Masker) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; READ_BUF];
    loop {
        let read = reader.read(&mut buf).await?;
        if read == 0 {
            break;
        }
        let out = masker.push(&buf[..read]);
        if !out.is_empty() {
            writer.write_all(&out).await?;
            writer.flush().await?;
        }
    }
    let out = masker.finish();
    writer.write_all(&out).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn masker(values: &[&str]) -> Masker {
        Masker::new(values.iter().flat_map(|v| needles_for(v.as_bytes())))
    }

    /// Прогоняет текст кусками заданного размера.
    fn run(masker: &mut Masker, text: &str, chunk: usize) -> String {
        let mut out = Vec::new();
        for part in text.as_bytes().chunks(chunk) {
            out.extend(masker.push(part));
        }
        out.extend(masker.finish());
        String::from_utf8(out).unwrap()
    }

    /// Результат должен совпадать при любом разбиении на куски.
    fn all_chunkings(values: &[&str], text: &str) -> String {
        let whole = run(&mut masker(values), text, text.len().max(1));
        for size in 1..=text.len().max(1) {
            assert_eq!(
                run(&mut masker(values), text, size),
                whole,
                "chunk size {size}"
            );
        }
        whole
    }

    #[test]
    fn masks_single_value() {
        assert_eq!(
            all_chunkings(
                &["s3cret-value"],
                "token=s3cret-value; again s3cret-value\n"
            ),
            "token=***; again ***\n"
        );
    }

    #[test]
    fn masks_value_split_across_chunks() {
        let mut m = masker(&["s3cret-value"]);
        let mut out = m.push(b"before s3cr");
        // Начало значения задержано, остальное выведено сразу.
        assert_eq!(out, b"before ");
        out.extend(m.push(b"et-val"));
        out.extend(m.push(b"ue after"));
        out.extend(m.finish());
        assert_eq!(String::from_utf8(out).unwrap(), "before *** after");
    }

    #[test]
    fn does_not_hold_unrelated_tail() {
        let mut m = masker(&["s3cret-value"]);
        assert_eq!(m.push(b"Password: "), b"Password: ");
        assert_eq!(m.push(b"xyz"), b"xyz");
        // Хвост, похожий на начало значения, ждёт; на конце потока выводится как есть.
        assert_eq!(m.push(b" s3c"), b" ");
        assert_eq!(m.finish(), b"s3c");
    }

    #[test]
    fn masks_multiple_values() {
        assert_eq!(
            all_chunkings(
                &["alpha-1234", "beta-5678"],
                "a=alpha-1234 b=beta-5678 c=alpha-12"
            ),
            "a=*** b=*** c=alpha-12"
        );
    }

    #[test]
    fn masks_overlapping_values_as_one_region() {
        // Значения перекрываются: не должно остаться видимой части ни одного из них.
        assert_eq!(
            all_chunkings(&["abcdefgh", "efghijkl"], "x abcdefghijkl y"),
            "x *** y"
        );
        // Одно значение — префикс другого.
        assert_eq!(
            all_chunkings(&["pass", "password-long"], "1 password-long 2 pass 3 passw"),
            "1 *** 2 *** 3 ***w"
        );
        // Соседние вхождения сливаются.
        assert_eq!(all_chunkings(&["abcd"], "abcdabcd!"), "***!");
        // Повторяющийся символ: области перекрываются и сливаются.
        assert_eq!(all_chunkings(&["aaaa"], "aaaaaaaaa b"), "*** b");
    }

    #[test]
    fn masks_base64_forms() {
        let value = "token>>>?value";
        let standard = base64::engine::general_purpose::STANDARD.encode(value);
        let url_safe = base64::engine::general_purpose::URL_SAFE.encode(value);
        assert_ne!(standard, url_safe);
        let text = format!("std={standard} url={url_safe} raw={value}");
        let out = all_chunkings(&[value], &text);
        // Паддинг `=` не входит в образец и может остаться.
        assert!(!out.contains(standard.trim_end_matches('=')), "{out}");
        assert!(!out.contains(url_safe.trim_end_matches('=')), "{out}");
        assert!(!out.contains(value), "{out}");
    }

    #[test]
    fn masks_trimmed_value_and_lines() {
        let key = "-----BEGIN KEY-----\nAAAABBBBCCCCDDDD\nEEEEFFFF\n-----END KEY-----\n";
        // Значение без завершающего перевода строки.
        assert_eq!(
            all_chunkings(&[key], &format!("[{}]", key.trim_end())),
            "[***]"
        );
        // Отдельная строка многострочного значения.
        assert_eq!(
            all_chunkings(&[key], "line: AAAABBBBCCCCDDDD\n"),
            "line: ***\n"
        );
    }

    #[test]
    fn short_values_are_not_masked() {
        assert!(needles_for(b"abc").is_empty());
        assert!(needles_for(b"").is_empty());
        assert!(!needles_for(b"abcd").is_empty());
        assert_eq!(all_chunkings(&["abc"], "abc abcd"), "abc abcd");
    }

    #[test]
    fn passes_through_without_values() {
        let mut m = Masker::new(Vec::new());
        assert_eq!(m.push(b"plain"), b"plain");
        assert_eq!(m.finish(), b"");
    }

    #[test]
    fn handles_binary_output() {
        let value = [0xffu8, 0x00, 0x10, 0x80, 0x7f];
        let mut m = Masker::new(needles_for(&value));
        let mut input = vec![1u8, 2];
        input.extend_from_slice(&value);
        input.push(3);
        let mut out = Vec::new();
        for byte in &input {
            out.extend(m.push(std::slice::from_ref(byte)));
        }
        out.extend(m.finish());
        assert_eq!(out, [1, 2, b'*', b'*', b'*', 3]);
    }

    #[tokio::test]
    async fn pump_masks_stream() {
        let input: &[u8] = b"line one s3cret-value\nline two\n";
        let mut output = Vec::new();
        pump(input, &mut output, masker(&["s3cret-value"]))
            .await
            .unwrap();
        assert_eq!(output, b"line one ***\nline two\n");
    }
}
