use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::path::Path;

use rand::RngCore;

pub fn load_or_create_token(path: &Path) -> io::Result<String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    match open_new_token_file(path) {
        Ok(mut file) => {
            let mut raw = [0_u8; 32];
            rand::rng().fill_bytes(&mut raw);
            let token = hex::encode(raw);
            file.write_all(token.as_bytes())?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            Ok(token)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => read_token(path),
        Err(error) => Err(error),
    }
}

fn open_new_token_file(path: &Path) -> io::Result<std::fs::File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

pub fn read_token(path: &Path) -> io::Result<String> {
    let mut text = String::new();
    OpenOptions::new()
        .read(true)
        .open(path)?
        .read_to_string(&mut text)?;
    let token = text.trim();
    if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid server auth token in {}", path.display()),
        ));
    }
    Ok(token.to_ascii_lowercase())
}

/// 固定时间比较，避免 token 前缀信息通过响应耗时泄漏。
pub fn constant_time_token_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    let mut difference = left.len() ^ right.len();
    let length = left.len().max(right.len());
    for index in 0..length {
        let a = left.get(index).copied().unwrap_or(0);
        let b = right.get(index).copied().unwrap_or(0);
        difference |= usize::from(a ^ b);
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_stable_and_valid() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("token");
        let first = load_or_create_token(&path).unwrap();
        let second = load_or_create_token(&path).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.len(), 64);
        assert!(constant_time_token_eq(&first, &second));
        assert!(!constant_time_token_eq(&first, "bad"));
    }

    #[test]
    fn malformed_token_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("token");
        std::fs::write(&path, "not-a-token").unwrap();
        assert_eq!(
            read_token(&path).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
