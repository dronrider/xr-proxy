use std::path::PathBuf;
use std::process::Command;

/// Конфиг во временном файле, убирается за собой.
struct TempConfig {
    path: PathBuf,
}

impl TempConfig {
    fn new(tag: &str, body: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "xr-client-validate-{tag}-{}.toml",
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
    Command::new(env!("CARGO_BIN_EXE_xr-client"))
        .arg("validate")
        .arg("-c")
        .arg(path)
        .output()
        .expect("запуск xr-client validate")
}

fn stderr(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

const GOOD: &str = r#"
[obfuscation]
key = "dGVzdA=="
modifier = "positional_xor_rotate"
salt = 3735928559

[routing]
default_action = "direct"

[[servers]]
name = "primary"
address = "192.0.2.10"
port = 8443
"#;

/// Годный конфиг отвечает ok и нулём, не трогая файрвол и листенеры:
/// тест гоняется без root, любая попытка поставить правила роняла бы его.
#[test]
fn good_config_answers_ok_without_touching_the_system() {
    let cfg = TempConfig::new("good", GOOD);
    let out = validate(&cfg.path);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ok");
}

#[test]
fn toml_garbage_is_a_named_failure() {
    let cfg = TempConfig::new("parse", "[obfuscation\nkey = \"dGVzdA==\"");
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
    assert!(err.contains("servers 'primary'.key"), "{err}");
    assert!(err.contains("key must not be empty"), "{err}");
}

#[test]
fn missing_server_pool_is_a_named_failure() {
    let body = GOOD.replace("[[servers]]\nname = \"primary\"\naddress = \"192.0.2.10\"\nport = 8443\n", "");
    let cfg = TempConfig::new("no-servers", &body);
    let out = validate(&cfg.path);
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(err.contains("[[servers]]"), "{err}");
}

/// Пул подключается по разобранному SocketAddr, домен в address роняет
/// старт: validate обязан поймать это до того, как правила встали.
#[test]
fn domain_server_address_is_a_named_failure() {
    let body = GOOD.replace("192.0.2.10", "vps.example.net");
    let cfg = TempConfig::new("domain", &body);
    let out = validate(&cfg.path);
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(err.contains("servers 'primary'"), "{err}");
    assert!(err.contains("invalid address 'vps.example.net'"), "{err}");
}

#[test]
fn bad_override_key_names_the_server() {
    let body = GOOD.replace("port = 8443", "port = 8443\nkey = \"not base64!!\"");
    let cfg = TempConfig::new("override", &body);
    let out = validate(&cfg.path);
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(err.contains("servers 'primary'.key"), "{err}");
}
