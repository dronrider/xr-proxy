use std::path::PathBuf;
use std::process::Command;

/// Конфиг во временном файле, убирается за собой.
struct TempConfig {
    path: PathBuf,
}

impl TempConfig {
    fn new(tag: &str, body: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "xr-server-validate-{tag}-{}.toml",
            std::process::id()
        ));
        std::fs::write(&path, body).expect("запись тестового конфига");
        Self { path }
    }
}

impl Drop for TempConfig {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn validate(path: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_xr-server"))
        .arg("validate")
        .arg("-c")
        .arg(path)
        .output()
        .expect("запуск xr-server validate")
}

fn stderr(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

const GOOD: &str = r#"
[server]
listen = "0.0.0.0"
port = 8443

[obfuscation]
key = "dGVzdA=="
modifier = "positional_xor_rotate"
salt = 3735928559
"#;

/// Годный конфиг отвечает ok и нулевым кодом: это контракт, на который
/// опираются init и deploy, проверяя конфиг до старта сервиса.
#[test]
fn good_config_answers_ok() {
    let cfg = TempConfig::new("good", GOOD);
    let out = validate(&cfg.path);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ok");
}

#[test]
fn toml_garbage_is_a_named_failure() {
    let cfg = TempConfig::new("parse", "[server\nlisten = \"0.0.0.0\"");
    let out = validate(&cfg.path);
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(err.starts_with("config invalid:"), "{err}");
    assert!(err.contains("TOML parse error"), "{err}");
}

#[test]
fn empty_key_is_a_named_failure() {
    let body = GOOD.replace("dGVzdA==", "");
    let cfg = TempConfig::new("empty-key", &body);
    let out = validate(&cfg.path);
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(err.contains("config invalid:"), "{err}");
    assert!(err.contains("obfuscation.key"), "{err}");
    assert!(err.contains("key must not be empty"), "{err}");
}

/// Соль вне u32 молча обрезается рантаймом: единственное место, где
/// несоответствие конфига и того, что реально работает, видно до старта.
#[test]
fn oversize_salt_is_a_named_failure() {
    let body = GOOD.replace("3735928559", "99999999999");
    let cfg = TempConfig::new("salt", &body);
    let out = validate(&cfg.path);
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(err.contains("obfuscation.salt"), "{err}");
    assert!(err.contains("does not fit in u32"), "{err}");
}

#[test]
fn bad_listen_address_is_a_named_failure() {
    let body = GOOD.replace("0.0.0.0", "not an address");
    let cfg = TempConfig::new("listen", &body);
    let out = validate(&cfg.path);
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(err.contains("invalid listen address"), "{err}");
}

#[test]
fn missing_config_file_is_a_named_failure() {
    let missing = std::env::temp_dir().join("xr-server-validate-no-such-file.toml");
    let out = validate(&missing);
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(err.contains("config invalid:"), "{err}");
    assert!(err.contains(&missing.display().to_string()), "{err}");
    assert!(err.contains("os error 2"), "{err}");
}
