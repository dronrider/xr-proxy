//! Конфиг браузерного фронта (LLD-38 п. 3.3, п. 3.5).
//!
//! Web-домен и общий секрет к хабу приходят отсюда, в коде их нет: домен
//! выбирает владелец, а секрет генерирует установщик. Блок `[tls]`
//! необязателен: по умолчанию фронт слушает HTTP на локальном порту за тем же
//! фронтом, что выводит наружу хаб, и терминацией занимается тот.

use std::path::{Path, PathBuf};

use serde::Deserialize;

fn default_bind() -> String {
    "127.0.0.1:8090".to_string()
}
fn default_session_ttl() -> u64 {
    7 * 24 * 3600
}
fn default_log_level() -> String {
    "info".to_string()
}

#[derive(Debug, Deserialize)]
pub struct WebConfig {
    /// Локальный адрес листенера. По умолчанию только loopback: наружу фронт
    /// выводит nginx или Cloudflare, слушать мир самому ему незачем.
    #[serde(default = "default_bind")]
    pub bind: String,
    /// Домен, на поддоменах которого живут публикации: `<имя>.<domain>`.
    pub domain: String,
    /// База хаба, например `https://xr-hub.example.com`.
    pub hub_url: String,
    /// Общий секрет служебных ручек хаба. Прав админки он не даёт, ключа
    /// подписи за ним нет (LLD-38 п. 3.5).
    pub shared_secret: String,
    /// Сколько живёт сессия браузера без активности, секунды.
    #[serde(default = "default_session_ttl")]
    pub session_ttl_secs: u64,
    #[serde(default = "default_log_level")]
    pub log_level: String,
}

/// Собственная терминация TLS: вариант установки без фронта. С фронтом блока
/// нет, и сертификатами `xr-web` не занимается вовсе.
#[derive(Debug, Clone, Deserialize)]
pub struct TlsConfig {
    pub cert: PathBuf,
    pub key: PathBuf,
}

#[derive(Debug, Deserialize)]
pub struct WebFile {
    pub web: WebConfig,
    #[serde(default)]
    pub tls: Option<TlsConfig>,
}

impl WebFile {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))
    }

    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let file: WebFile = toml::from_str(text)?;
        if file.web.domain.trim().is_empty() {
            anyhow::bail!("[web] domain пуст: без web-домена поддомен публикации не разобрать");
        }
        if file.web.shared_secret.trim().is_empty() {
            anyhow::bail!("[web] shared_secret пуст: служебные ручки хаба закрыты секретом");
        }
        Ok(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_config_with_defaults() {
        let cfg = WebFile::parse(
            r#"
            [web]
            domain = "web.example.com"
            hub_url = "https://xr-hub.example.com"
            shared_secret = "s3cret"
            "#,
        )
        .expect("минимального блока хватает");
        assert_eq!(cfg.web.bind, "127.0.0.1:8090");
        assert_eq!(cfg.web.session_ttl_secs, 7 * 24 * 3600);
        assert!(cfg.tls.is_none(), "без блока [tls] терминирует фронт");
    }

    #[test]
    fn tls_block_is_read_when_present() {
        let cfg = WebFile::parse(
            r#"
            [web]
            bind = "0.0.0.0:443"
            domain = "web.example.com"
            hub_url = "https://xr-hub.example.com"
            shared_secret = "s3cret"

            [tls]
            cert = "/etc/xr-web/tls/fullchain.pem"
            key = "/etc/xr-web/tls/privkey.pem"
            "#,
        )
        .unwrap();
        let tls = cfg.tls.expect("блок [tls] обязан читаться");
        assert!(tls.cert.ends_with("fullchain.pem"));
        assert_eq!(cfg.web.bind, "0.0.0.0:443");
    }

    #[test]
    fn empty_domain_or_secret_is_refused() {
        // Пустой домен превратил бы любой Host в публикацию, пустой секрет
        // отправил бы фронт в хаб без мандата. И то, и другое лучше поймать
        // на старте, чем первым запросом браузера.
        for bad in [
            "[web]\ndomain = \"\"\nhub_url = \"https://h\"\nshared_secret = \"s\"\n",
            "[web]\ndomain = \"web.example.com\"\nhub_url = \"https://h\"\nshared_secret = \"  \"\n",
        ] {
            assert!(WebFile::parse(bad).is_err(), "конфиг обязан быть отвергнут: {bad}");
        }
    }
}
