//! Генерация секретов установки. Ключ обфускации того же размера, что даёт
//! generate-key.sh (64 байта); хеш пароля админа тем же argon2id, которым
//! xr-hub проверяет логин.

use anyhow::{anyhow, Result};
use base64::Engine;
use rand::RngCore;

pub fn gen_obfuscation_key() -> String {
    let mut bytes = [0u8; 64];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub fn gen_salt() -> u32 {
    // Нулевой salt выглядит как «не задан», не отдаём его никогда.
    loop {
        let s = rand::thread_rng().next_u32();
        if s != 0 {
            return s;
        }
    }
}

/// Пароль админа хаба: 16 случайных байт в base64url, без спецсимволов,
/// чтобы без сюрпризов жил в shell и в JSON.
pub fn gen_password() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Ключ подписи хаба: 32 сырых байта в base64, формат SigningContext.
pub fn gen_signing_key() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub fn hash_password(password: &str) -> Result<String> {
    use argon2::password_hash::rand_core::OsRng;
    use argon2::password_hash::SaltString;
    use argon2::{Argon2, PasswordHasher};

    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow!("хеширование пароля: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use argon2::{Argon2, PasswordVerifier};
    use base64::Engine;

    #[test]
    fn key_is_64_bytes_of_base64() {
        let key = gen_obfuscation_key();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&key)
            .unwrap();
        assert_eq!(decoded.len(), 64);
    }

    #[test]
    fn signing_key_is_32_bytes() {
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(gen_signing_key())
            .unwrap();
        assert_eq!(decoded.len(), 32);
    }

    #[test]
    fn password_hash_verifies_like_the_hub_does() {
        let pass = gen_password();
        let hash = hash_password(&pass).unwrap();
        let parsed = argon2::password_hash::PasswordHash::new(&hash).unwrap();
        assert!(Argon2::default()
            .verify_password(pass.as_bytes(), &parsed)
            .is_ok());
    }
}
