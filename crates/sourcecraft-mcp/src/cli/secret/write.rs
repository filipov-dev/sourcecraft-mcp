//! `secret write`: запись значения секрета в новый файл с правами 0600.
//!
//! Файл создаётся с `O_EXCL` (`create_new`): существующий файл или символическая ссылка
//! не перезаписываются и не разыменовываются. `--force` сначала удаляет существующую запись
//! каталога (ссылку — саму ссылку, не цель) и снова создаёт файл с `create_new`.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, bail};

/// Разбирает права в восьмеричном виде: `600`, `0600`, `0o600`.
pub fn parse_mode(text: &str) -> Result<u32, String> {
    let digits = text.strip_prefix("0o").unwrap_or(text);
    let mode = u32::from_str_radix(digits, 8)
        .map_err(|_| format!("`{text}` is not an octal file mode such as 600"))?;
    if mode > 0o777 {
        return Err(format!("`{text}`: the mode must be at most 777"));
    }
    Ok(mode)
}

/// Пишет `value` в `path` с правами `mode`. Без `force` существующий путь — ошибка.
pub fn write_secret_file(path: &Path, value: &[u8], mode: u32, force: bool) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if !force => bail!(
            "`{}` already exists{}; pass --force to replace it",
            path.display(),
            if meta.is_dir() {
                " and is a directory"
            } else {
                ""
            }
        ),
        Ok(meta) if meta.is_dir() => bail!("`{}` is a directory", path.display()),
        Ok(_) => std::fs::remove_file(path)
            .with_context(|| format!("cannot replace `{}`", path.display()))?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).with_context(|| format!("cannot access `{}`", path.display()));
        }
    }

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("cannot create `{}`", path.display()))?;
    // Права задаются явно ещё раз: `mode` при создании урезается umask.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(mode))
            .with_context(|| format!("cannot set the mode of `{}`", path.display()))?;
    }
    // На Windows прав Unix нет: файл наследует ACL каталога (каталоги профиля пользователя
    // по умолчанию закрыты от других пользователей). Это сказано в `--help` команды.
    #[cfg(not(unix))]
    let _ = mode;
    let written = file.write_all(value).and_then(|()| file.sync_all());
    if let Err(err) = written {
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(err).with_context(|| format!("cannot write `{}`", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Права по умолчанию (`--mode 600`).
    const DEFAULT_MODE: u32 = 0o600;

    #[test]
    fn parses_modes() {
        assert_eq!(parse_mode("600"), Ok(0o600));
        assert_eq!(parse_mode("0600"), Ok(0o600));
        assert_eq!(parse_mode("0o640"), Ok(0o640));
        for bad in ["", "abc", "800", "1000", "-1"] {
            assert!(parse_mode(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn writes_new_file_and_refuses_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        write_secret_file(&path, b"value\n", DEFAULT_MODE, false).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"value\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        let err = write_secret_file(&path, b"other", DEFAULT_MODE, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("--force"), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), b"value\n");

        write_secret_file(&path, b"other", 0o400, true).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"other");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o400);
        }

        assert!(write_secret_file(dir.path(), b"x", DEFAULT_MODE, true).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn does_not_follow_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, "keep").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert!(write_secret_file(&link, b"secret", DEFAULT_MODE, false).is_err());
        write_secret_file(&link, b"secret", DEFAULT_MODE, true).unwrap();
        // Заменена сама ссылка, цель не тронута.
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
        assert!(!std::fs::symlink_metadata(&link).unwrap().is_symlink());
        assert_eq!(std::fs::read(&link).unwrap(), b"secret");
    }
}
