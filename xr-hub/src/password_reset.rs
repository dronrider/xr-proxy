//! `xr-hub reset-password` — сброс пароля админки правкой конфиг-файла.
//!
//! `HubConfig` десериализуется, но не сериализуется: полная пересериализация
//! TOML затёрла бы комментарии и форматирование владельца. Поэтому правим
//! хирургически — заменяем единственную строку `password_hash` в блоке
//! `[[admin.users]]` нужного пользователя, не трогая остальной файл.

/// Заменить `password_hash` пользователя `user` на `new_hash` в тексте
/// конфига. Возвращает новый текст файла или ошибку, если пользователь
/// не найден / в его блоке нет строки `password_hash`.
pub fn replace_password_hash(
    content: &str,
    user: &str,
    new_hash: &str,
) -> Result<String, String> {
    let lines: Vec<&str> = content.lines().collect();

    // Границы блоков [[admin.users]]: от заголовка до следующего заголовка
    // таблицы (строка, начинающаяся с '[').
    let mut blocks: Vec<(usize, usize)> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if line.trim() == "[[admin.users]]" {
            let end = lines[i + 1..]
                .iter()
                .position(|l| l.trim_start().starts_with('['))
                .map(|off| i + 1 + off)
                .unwrap_or(lines.len());
            blocks.push((i + 1, end));
        }
    }

    if blocks.is_empty() {
        return Err("no [[admin.users]] blocks found in config".into());
    }

    let mut found_users = Vec::new();
    for &(start, end) in &blocks {
        let block = &lines[start..end];
        let username = block.iter().find_map(|l| key_value(l, "username"));
        let Some(username) = username else { continue };
        if username != user {
            found_users.push(username);
            continue;
        }

        let hash_idx = block
            .iter()
            .position(|l| key_value(l, "password_hash").is_some())
            .ok_or_else(|| {
                format!("user '{user}' found, but no password_hash line in its block")
            })?;

        let abs_idx = start + hash_idx;
        let indent: String = lines[abs_idx]
            .chars()
            .take_while(|c| c.is_whitespace())
            .collect();

        let mut out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        out[abs_idx] = format!("{indent}password_hash = \"{new_hash}\"");

        let mut result = out.join("\n");
        if content.ends_with('\n') {
            result.push('\n');
        }
        return Ok(result);
    }

    Err(format!(
        "user '{}' not found in config (users: {})",
        user,
        found_users.join(", ")
    ))
}

/// Если строка — присваивание `key = "value"` для данного ключа, вернуть
/// значение без кавычек. Комментарии и чужие ключи дают None.
fn key_value(line: &str, key: &str) -> Option<String> {
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') {
        return None;
    }
    let rest = trimmed.strip_prefix(key)?.trim_start();
    let value = rest.strip_prefix('=')?.trim_start();
    if let Some(quoted) = value.strip_prefix('"') {
        let end = quoted.find('"')?;
        Some(quoted[..end].to_string())
    } else {
        // bare-значение — до пробела или начала комментария
        Some(
            value
                .split(|c: char| c.is_whitespace() || c == '#')
                .next()
                .unwrap_or("")
                .to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"# xr-hub config
[server]
bind = "0.0.0.0:443"
data_dir = "/var/lib/xr-hub"

[[admin.users]]
username = "admin"
password_hash = "$argon2id$old-admin-hash"

[[admin.users]]
# второй оператор
password_hash = "$argon2id$old-op-hash"
username = "operator"

[invites]
dev_mode = false
"#;

    #[test]
    fn replaces_hash_for_named_user() {
        let out = replace_password_hash(CONFIG, "admin", "$argon2id$NEW").unwrap();
        assert!(out.contains("password_hash = \"$argon2id$NEW\""));
        assert!(!out.contains("old-admin-hash"));
        // чужой блок не тронут
        assert!(out.contains("old-op-hash"));
        // комментарии и прочие строки сохранены
        assert!(out.starts_with("# xr-hub config"));
        assert!(out.contains("# второй оператор"));
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn handles_hash_line_before_username_line() {
        let out = replace_password_hash(CONFIG, "operator", "$argon2id$NEW2").unwrap();
        assert!(out.contains("$argon2id$NEW2"));
        assert!(!out.contains("old-op-hash"));
        assert!(out.contains("old-admin-hash"));
    }

    #[test]
    fn unknown_user_lists_available() {
        let err = replace_password_hash(CONFIG, "ghost", "x").unwrap_err();
        assert!(err.contains("ghost"));
        assert!(err.contains("admin"));
    }

    #[test]
    fn missing_admin_section_is_error() {
        let err = replace_password_hash("[server]\nbind = \"x\"\n", "admin", "x").unwrap_err();
        assert!(err.contains("no [[admin.users]]"));
    }

    #[test]
    fn ignores_commented_lines_and_trailing_comments() {
        let cfg = "[[admin.users]]\n# password_hash = \"decoy\"\nusername = \"admin\" # primary\npassword_hash = \"old\"\n";
        let out = replace_password_hash(cfg, "admin", "NEW").unwrap();
        assert!(out.contains("password_hash = \"NEW\""));
        assert!(!out.contains("password_hash = \"old\""));
        assert!(out.contains("# password_hash = \"decoy\""));
    }

    #[test]
    fn result_still_parses_as_toml() {
        let out = replace_password_hash(CONFIG, "admin", "$argon2id$NEW").unwrap();
        let parsed: toml::Value = toml::from_str(&out).unwrap();
        let users = parsed["admin"]["users"].as_array().unwrap();
        assert_eq!(users[0]["password_hash"].as_str().unwrap(), "$argon2id$NEW");
    }
}
