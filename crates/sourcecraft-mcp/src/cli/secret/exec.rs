//! `secret exec`: запуск команды с секретами в переменных окружения и маскированием вывода.
//!
//! Оболочка не используется: программа запускается напрямую с аргументами как есть.
//! stdin наследуется, stdout и stderr читаются параллельно и проходят через [`Masker`].

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::process::Stdio;

use anyhow::{Context, bail};
use tokio::io::AsyncWrite;
use tokio::process::Command;

use super::fetch::Secrets;
use super::mask::{MIN_MASK_LEN, Masker, needles_for, pump};

/// Привязка переменной окружения к ключу секрета.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub name: String,
    pub key: String,
}

/// Разбирает `--env`: `NAME=KEY` или `KEY` (имя переменной — ключ с префиксом `--prefix`).
pub fn parse_env_arg(arg: &str, prefix: &str) -> anyhow::Result<Binding> {
    let (name, key) = match arg.split_once('=') {
        Some((name, key)) => (name.to_owned(), key.to_owned()),
        None => (format!("{prefix}{arg}"), arg.to_owned()),
    };
    if key.is_empty() {
        bail!("--env `{arg}`: the secret key is empty; use NAME=KEY or KEY");
    }
    if !is_env_name(&name) {
        bail!(
            "--env `{arg}`: `{name}` is not a valid environment variable name \
             (letters, digits and `_`, not starting with a digit)"
        );
    }
    Ok(Binding { name, key })
}

/// Допустимое имя переменной окружения: `[A-Za-z_][A-Za-z0-9_]*`.
pub fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Итоговые привязки: `--all` (каждый секрет под своим ключом с префиксом) и явные `--env`,
/// которые важнее `--all` при совпадении имени. Ключи, которых нет в репозитории, — ошибка:
/// иначе опечатка в ключе тихо запустила бы команду без переменной.
pub fn resolve_bindings(
    explicit: &[Binding],
    all: bool,
    prefix: &str,
    secrets: &Secrets,
    warn: &mut dyn FnMut(String),
) -> anyhow::Result<Vec<Binding>> {
    let mut by_name: BTreeMap<String, String> = BTreeMap::new();
    if all {
        for key in secrets.keys() {
            let name = format!("{prefix}{key}");
            if is_env_name(&name) {
                by_name.insert(name, key.to_owned());
            } else {
                warn(format!(
                    "secret `{key}` is skipped: `{name}` is not a valid environment variable \
                     name; bind it explicitly with --env NAME={key}"
                ));
            }
        }
    }
    let mut seen = BTreeSet::new();
    for binding in explicit {
        if !seen.insert(binding.name.as_str()) {
            bail!("environment variable `{}` is bound twice", binding.name);
        }
        by_name.insert(binding.name.clone(), binding.key.clone());
    }

    let missing: BTreeSet<&str> = by_name
        .values()
        .map(String::as_str)
        .filter(|key| secrets.get(key).is_none())
        .collect();
    if !missing.is_empty() {
        let list: Vec<String> = missing.iter().map(|k| format!("`{k}`")).collect();
        bail!(
            "secret(s) not found in the repository: {}. See the keys with `sourcecraft-mcp \
             secret list`",
            list.join(", ")
        );
    }
    Ok(by_name
        .into_iter()
        .map(|(name, key)| Binding { name, key })
        .collect())
}

/// Переменные окружения для команды и образцы для маскирования. Не `Debug`: внутри значения.
pub struct PreparedEnv {
    pub env: Vec<(String, OsString)>,
    pub needles: Vec<Vec<u8>>,
}

/// Готовит переменные окружения для команды и образцы для маскирования. Предупреждения (пустое или
/// слишком короткое значение) называют ключ, но не значение.
pub fn prepare_env(
    bindings: &[Binding],
    secrets: &Secrets,
    warn: &mut dyn FnMut(String),
) -> anyhow::Result<PreparedEnv> {
    let mut env = Vec::new();
    let mut needles = Vec::new();
    let mut warned = BTreeSet::new();
    for binding in bindings {
        let Some(value) = secrets.get(&binding.key) else {
            bail!("secret `{}` not found", binding.key);
        };
        if value.contains(&0) {
            bail!(
                "secret `{}` contains a NUL byte and cannot be passed in an environment \
                 variable; use `sourcecraft-mcp secret write` to put it into a file",
                binding.key
            );
        }
        if warned.insert(binding.key.as_str()) {
            if value.is_empty() {
                warn(format!("secret `{}` is empty", binding.key));
            } else if value.len() < MIN_MASK_LEN {
                warn(format!(
                    "secret `{}` is shorter than {MIN_MASK_LEN} bytes and is not masked in \
                     the command output",
                    binding.key
                ));
            }
        }
        needles.extend(needles_for(value));
        env.push((binding.name.clone(), env_value(&binding.key, value)?));
    }
    Ok(PreparedEnv { env, needles })
}

#[cfg(unix)]
fn env_value(_key: &str, value: &[u8]) -> anyhow::Result<OsString> {
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt};
    Ok(OsStr::from_bytes(value).to_os_string())
}

#[cfg(not(unix))]
fn env_value(key: &str, value: &[u8]) -> anyhow::Result<OsString> {
    match std::str::from_utf8(value) {
        Ok(text) => Ok(OsString::from(text)),
        Err(_) => bail!(
            "secret `{key}` is not UTF-8 text and cannot be passed in an environment variable \
             on this platform; use `sourcecraft-mcp secret write`"
        ),
    }
}

/// Запускает команду и копирует её вывод с маскированием. Возвращает код завершения
/// (для завершения сигналом на Unix — `128 + номер сигнала`, как в оболочке).
pub async fn run_masked<O, E>(
    command: &[OsString],
    env: &[(String, OsString)],
    needles: &[Vec<u8>],
    stdout: O,
    stderr: E,
) -> anyhow::Result<i32>
where
    O: AsyncWrite + Unpin,
    E: AsyncWrite + Unpin,
{
    let Some((program, args)) = command.split_first() else {
        bail!("no command given: pass it after `--`");
    };
    let mut child = Command::new(program)
        .args(args)
        .envs(env.iter().map(|(name, value)| (name, value)))
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("cannot run `{}`", program.to_string_lossy()))?;
    let child_out = child.stdout.take().context("child stdout is not piped")?;
    let child_err = child.stderr.take().context("child stderr is not piped")?;

    let (status, out_result, err_result) = tokio::join!(
        child.wait(),
        pump(child_out, stdout, Masker::new(needles.iter().cloned())),
        pump(child_err, stderr, Masker::new(needles.iter().cloned())),
    );
    for (stream, result) in [("stdout", out_result), ("stderr", err_result)] {
        if let Err(err) = result {
            // Закрытый канал (`| head`) — обычная ситуация конвейера.
            if err.kind() != std::io::ErrorKind::BrokenPipe {
                eprintln!("sourcecraft-mcp: warning: copying the command {stream} failed: {err}");
            }
        }
    }
    let status = status.context("waiting for the command failed")?;
    Ok(exit_code(&status))
}

fn exit_code(status: &std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secrets(pairs: &[(&str, &[u8])]) -> Secrets {
        Secrets::from_pairs(pairs)
    }

    fn binding(name: &str, key: &str) -> Binding {
        Binding {
            name: name.into(),
            key: key.into(),
        }
    }

    #[test]
    fn parses_env_args() {
        assert_eq!(
            parse_env_arg("TOKEN=API_TOKEN", "X_").unwrap(),
            binding("TOKEN", "API_TOKEN")
        );
        assert_eq!(
            parse_env_arg("API_TOKEN", "X_").unwrap(),
            binding("X_API_TOKEN", "API_TOKEN")
        );
        assert_eq!(
            parse_env_arg("_A1=k-ey", "").unwrap(),
            binding("_A1", "k-ey")
        );
        for bad in ["=KEY", "1A=KEY", "A-B=KEY", "NAME=", "a-key"] {
            assert!(parse_env_arg(bad, "").is_err(), "{bad}");
        }
    }

    #[test]
    fn resolves_bindings() {
        let s = secrets(&[("A", b"aaaa"), ("B", b"bbbb"), ("bad-key", b"cccc")]);
        let mut warnings = Vec::new();
        let mut warn = |w: String| warnings.push(w);

        let all = resolve_bindings(&[], true, "SC_", &s, &mut warn).unwrap();
        assert_eq!(all, [binding("SC_A", "A"), binding("SC_B", "B")]);

        // Явная привязка важнее `--all`.
        let mixed = resolve_bindings(&[binding("SC_A", "B")], true, "SC_", &s, &mut warn).unwrap();
        assert_eq!(mixed, [binding("SC_A", "B"), binding("SC_B", "B")]);

        let err = resolve_bindings(&[binding("X", "MISSING")], false, "", &s, &mut warn)
            .unwrap_err()
            .to_string();
        assert!(err.contains("`MISSING`"), "{err}");
        let err = resolve_bindings(
            &[binding("X", "A"), binding("X", "B")],
            false,
            "",
            &s,
            &mut warn,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("bound twice"), "{err}");
        drop(warn);
        assert!(
            warnings.iter().all(|w| w.contains("bad-key")),
            "{warnings:?}"
        );
        assert!(!warnings.is_empty());
    }

    #[test]
    fn warns_without_values() {
        let s = secrets(&[("EMPTY", b""), ("SHORT", b"abc"), ("LONG", b"long-value")]);
        let mut warnings = Vec::new();
        let PreparedEnv { env, needles } = prepare_env(
            &[
                binding("E", "EMPTY"),
                binding("S", "SHORT"),
                binding("L", "LONG"),
            ],
            &s,
            &mut |w| warnings.push(w),
        )
        .unwrap();
        assert_eq!(env.len(), 3);
        assert!(needles.iter().any(|n| n == b"long-value"));
        assert!(!needles.iter().any(|n| n == b"abc"));
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].contains("`EMPTY` is empty"));
        assert!(warnings[1].contains("`SHORT` is shorter"));
        for w in &warnings {
            assert!(!w.contains("abc"), "{w}");
        }

        let nul = secrets(&[("NUL", b"a\0bc")]);
        let err = prepare_env(&[binding("N", "NUL")], &nul, &mut |_| {})
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("NUL byte"), "{err}");
    }

    #[cfg(unix)]
    async fn exec(script: &str, s: &Secrets, bindings: &[Binding]) -> (i32, String, String) {
        let PreparedEnv { env, needles } = prepare_env(bindings, s, &mut |_| {}).unwrap();
        let command: Vec<OsString> = ["sh", "-c", script].iter().map(OsString::from).collect();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_masked(&command, &env, &needles, &mut out, &mut err)
            .await
            .unwrap();
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn injects_and_masks() {
        let s = secrets(&[("API_TOKEN", b"s3cret-value"), ("OTHER", b"other-value")]);
        let bindings = [binding("X", "API_TOKEN"), binding("Y", "OTHER")];
        let (code, out, err) = exec(
            r#"printf %s "$X"; printf ' len=%s ' "${#X}"; printf '%s' "$Y" >&2; printf %s "$X" | base64"#,
            &s,
            &bindings,
        )
        .await;
        assert_eq!(code, 0);
        // Значение дошло до команды (длина видна), но в выводе скрыто, как и его base64.
        assert!(out.starts_with("*** len=12 "), "{out}");
        assert!(!out.contains("s3cret"), "{out}");
        assert!(!out.contains("czNjcmV0LXZhbHVl"), "{out}");
        assert_eq!(err, "***");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn propagates_exit_code() {
        let s = secrets(&[("K", b"value-1234")]);
        let (code, out, _) = exec(
            r#"test "$V" = value-1234 && exit 7"#,
            &s,
            &[binding("V", "K")],
        )
        .await;
        assert_eq!(code, 7);
        assert_eq!(out, "");
        let (code, _, _) = exec("kill -TERM $$", &s, &[]).await;
        assert_eq!(code, 128 + 15);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reports_missing_program() {
        let err = run_masked(
            &[OsString::from("/nonexistent/program")],
            &[],
            &[],
            tokio::io::sink(),
            tokio::io::sink(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("cannot run `/nonexistent/program`"), "{err}");
        let err = run_masked(&[], &[], &[], tokio::io::sink(), tokio::io::sink())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no command"), "{err}");
    }
}
