//! Клиент админ-API хаба ровно на две операции: логин и минт одноразового
//! инвайта (LLD-01). Ходит на localhost свежепоставленного хаба, поэтому
//! plain HTTP здесь норма.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::time::{Duration, Instant};

pub struct HubClient {
    base: String,
}

#[derive(Debug, Deserialize)]
pub struct LoginResponse {
    pub token: String,
}

#[derive(Debug, Deserialize)]
pub struct InviteResponse {
    pub token: String,
    pub expires_at: String,
}

impl HubClient {
    pub fn new(base: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
        }
    }

    /// Подождать, пока свежезапущенный хаб начнёт отвечать.
    pub fn wait_ready(&self, timeout: Duration) -> Result<()> {
        let url = format!("{}/api/v1/presets", self.base);
        let deadline = Instant::now() + timeout;
        loop {
            match ureq::get(&url).timeout(Duration::from_secs(2)).call() {
                Ok(_) => return Ok(()),
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(500));
                }
                Err(e) => bail!("хаб на {} так и не поднялся: {e}", self.base),
            }
        }
    }

    pub fn login(&self, username: &str, password: &str) -> Result<String> {
        let resp: LoginResponse = ureq::post(&format!("{}/api/v1/auth/login", self.base))
            .timeout(Duration::from_secs(10))
            .send_json(ureq::json!({ "username": username, "password": password }))
            .context("логин в хаб")?
            .into_json()
            .context("разбор ответа логина")?;
        Ok(resp.token)
    }

    /// Одноразовый инвайт с дефолтами хаба (TTL из конфига).
    pub fn create_invite(&self, session: &str, comment: &str) -> Result<InviteResponse> {
        ureq::post(&format!("{}/api/v1/admin/invites", self.base))
            .timeout(Duration::from_secs(10))
            .set("Authorization", &format!("Bearer {session}"))
            .send_json(ureq::json!({ "comment": comment }))
            .context("создание инвайта")?
            .into_json()
            .context("разбор ответа создания инвайта")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hub_responses() {
        let login: LoginResponse =
            serde_json::from_str(r#"{"token":"abc","username":"admin"}"#).unwrap();
        assert_eq!(login.token, "abc");

        // Ответ хаба несёт весь Invite; клиенту нужны токен и срок.
        let invite: InviteResponse = serde_json::from_str(
            r#"{"token":"abcdefghij0123456789AB","created_at":"2026-01-01T00:00:00Z",
                "expires_at":"2026-01-02T00:00:00Z","consumed_at":null,"one_time":true,
                "comment":"", "payload":{}, "share_ids":[]}"#,
        )
        .unwrap();
        assert_eq!(invite.token, "abcdefghij0123456789AB");
        assert_eq!(invite.expires_at, "2026-01-02T00:00:00Z");
    }
}
