//! Взаимодействие с пользователем в интерактивных командах (`connect`, `auth`).
//!
//! Вынесено в трейт, чтобы сценарии можно было тестировать без терминала.

use std::io::{self, BufRead, Write};

use sourcecraft_api::ApiToken;

pub trait Ui {
    /// Выводит строку пользователю.
    fn say(&mut self, line: &str);
    /// Скрытый ввод токена: значение не отображается в терминале.
    fn read_token(&mut self, prompt: &str) -> io::Result<ApiToken>;
    /// Вопрос «да/нет». Ответ по умолчанию — «нет».
    fn confirm(&mut self, question: &str) -> io::Result<bool>;
    /// Открывает ссылку в браузере по умолчанию, не дожидаясь его.
    fn open_url(&mut self, url: &str) -> io::Result<()>;
}

/// Терминал: вывод в stdout, токен через `rpassword` (читает напрямую из TTY без эха).
#[derive(Debug, Default)]
pub struct TerminalUi;

impl Ui for TerminalUi {
    fn say(&mut self, line: &str) {
        println!("{line}");
    }

    fn read_token(&mut self, prompt: &str) -> io::Result<ApiToken> {
        let value = rpassword::prompt_password(prompt)?;
        Ok(ApiToken::new(value.trim()))
    }

    fn confirm(&mut self, question: &str) -> io::Result<bool> {
        print!("{question} [y/N] ");
        io::stdout().flush()?;
        let mut answer = String::new();
        // EOF (например, stdin не терминал) считается отказом.
        io::stdin().lock().read_line(&mut answer)?;
        Ok(matches!(
            answer.trim().to_ascii_lowercase().as_str(),
            "y" | "yes"
        ))
    }

    fn open_url(&mut self, url: &str) -> io::Result<()> {
        crate::browser::open_in_browser(url)
    }
}

#[cfg(test)]
pub mod testing {
    use std::collections::VecDeque;

    use super::*;

    /// Сценарий ввода для тестов: заранее заданные токены и ответы, весь вывод сохраняется.
    #[derive(Debug, Default)]
    pub struct ScriptedUi {
        pub tokens: VecDeque<String>,
        pub answers: VecDeque<bool>,
        pub output: Vec<String>,
        pub questions: Vec<String>,
        /// Ссылки, которые сценарий «открыл» в браузере (настоящий браузер не запускается).
        pub opened: Vec<String>,
        /// Открытие браузера завершается ошибкой.
        pub open_fails: bool,
    }

    impl ScriptedUi {
        pub fn text(&self) -> String {
            self.output.join("\n")
        }
    }

    impl Ui for ScriptedUi {
        fn say(&mut self, line: &str) {
            self.output.push(line.to_owned());
        }

        fn read_token(&mut self, _prompt: &str) -> io::Result<ApiToken> {
            self.tokens
                .pop_front()
                .map(|value| ApiToken::new(value.trim()))
                .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "no token scripted"))
        }

        fn confirm(&mut self, question: &str) -> io::Result<bool> {
            self.questions.push(question.to_owned());
            self.answers
                .pop_front()
                .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "no answer scripted"))
        }

        fn open_url(&mut self, url: &str) -> io::Result<()> {
            if self.open_fails {
                return Err(io::Error::new(io::ErrorKind::NotFound, "no browser"));
            }
            self.opened.push(url.to_owned());
            Ok(())
        }
    }
}
