//! Источник бинарей: локальная директория (накидали scp) или setup-dist
//! хаба. Скачанное сверяется с SHA256SUMS; для директории сверка тоже
//! делается, если файл сумм лежит рядом, это ловит битую доставку.

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::PathBuf;

/// Потолок на один файл из setup-dist: бинари весят мегабайты, сотни
/// мегабайт значат, что раздача отдаёт что-то не то.
const MAX_FETCH_BYTES: u64 = 256 * 1024 * 1024;

pub enum BinSource {
    Dir(PathBuf),
    Url(String),
}

impl BinSource {
    /// Забрать файл и проверить его хеш, когда есть чем проверять.
    pub fn fetch(&self, file: &str) -> Result<Vec<u8>> {
        match self {
            BinSource::Dir(dir) => {
                let bytes = std::fs::read(dir.join(file))
                    .with_context(|| format!("чтение {}", dir.join(file).display()))?;
                if let Some(expected) = self.expected_sha(file)? {
                    verify_sha256(&bytes, &expected, file)?;
                }
                Ok(bytes)
            }
            BinSource::Url(base) => {
                let bytes = http_get(&format!("{}/{file}", base.trim_end_matches('/')))?;
                let expected = self
                    .expected_sha(file)?
                    .ok_or_else(|| anyhow!("в {base} нет SHA256SUMS с записью для {file}"))?;
                verify_sha256(&bytes, &expected, file)?;
                Ok(bytes)
            }
        }
    }

    /// Ожидаемый хеш файла по SHA256SUMS источника; None, если у локальной
    /// директории файла сумм нет.
    pub fn expected_sha(&self, file: &str) -> Result<Option<String>> {
        let sums = match self {
            BinSource::Dir(dir) => match std::fs::read_to_string(dir.join("SHA256SUMS")) {
                Ok(s) => s,
                Err(_) => return Ok(None),
            },
            BinSource::Url(base) => {
                let bytes = http_get(&format!("{}/SHA256SUMS", base.trim_end_matches('/')))?;
                String::from_utf8(bytes).context("SHA256SUMS не в UTF-8")?
            }
        };
        Ok(parse_sha256sums(&sums, file))
    }
}

/// Разбор формата sha256sum: `<hex>  <имя>` построчно (звёздочка бинарного
/// режима перед именем допустима).
pub fn parse_sha256sums(sums: &str, file: &str) -> Option<String> {
    for line in sums.lines() {
        let mut parts = line.split_whitespace();
        let (Some(hash), Some(name)) = (parts.next(), parts.next()) else {
            continue;
        };
        if name.trim_start_matches('*') == file {
            return Some(hash.to_ascii_lowercase());
        }
    }
    None
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

fn verify_sha256(bytes: &[u8], expected: &str, file: &str) -> Result<()> {
    let actual = sha256_hex(bytes);
    if actual != expected {
        bail!("хеш {file} не совпал: ожидался {expected}, получен {actual}");
    }
    Ok(())
}

fn http_get(url: &str) -> Result<Vec<u8>> {
    let resp = ureq::get(url)
        .timeout(std::time::Duration::from_secs(120))
        .call()
        .with_context(|| format!("GET {url}"))?;
    let mut bytes = Vec::new();
    resp.into_reader()
        .take(MAX_FETCH_BYTES)
        .read_to_end(&mut bytes)
        .with_context(|| format!("чтение тела {url}"))?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sums_and_ignores_other_lines() {
        let sums = "\
abc123  xr-server-linux-x86_64
DEF456 *xr-hub-linux-x86_64
garbage-line
";
        assert_eq!(
            parse_sha256sums(sums, "xr-server-linux-x86_64").as_deref(),
            Some("abc123")
        );
        assert_eq!(
            parse_sha256sums(sums, "xr-hub-linux-x86_64").as_deref(),
            Some("def456"),
            "звёздочка бинарного режима и регистр хеша не мешают"
        );
        assert_eq!(parse_sha256sums(sums, "нет-такого"), None);
    }

    #[test]
    fn dir_source_verifies_when_sums_present() {
        let dir = std::env::temp_dir().join(format!("xr-setup-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("bin"), b"payload").unwrap();
        std::fs::write(
            dir.join("SHA256SUMS"),
            format!("{}  bin\n", sha256_hex(b"payload")),
        )
        .unwrap();
        let src = BinSource::Dir(dir.clone());
        assert_eq!(src.fetch("bin").unwrap(), b"payload");

        std::fs::write(dir.join("SHA256SUMS"), "0000  bin\n").unwrap();
        let err = src.fetch("bin").unwrap_err();
        assert!(err.to_string().contains("не совпал"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
