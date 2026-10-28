//! Прогон `scripts/install-macos-svc.sh` на стенде из заглушек (XR-309).
//!
//! Настоящий cargo и sudo звать нельзя: сборка тяжёлая, а sudo спросит
//! пароль. Заглушки пишут вызовы в журнал стенда, и по журналу видно то,
//! ради чего скрипт переписывали: сборка идёт от вызывающего пользователя,
//! а sudo получает только шаг install с профилем и хвостом аргументов.
//! Заглушка `id` печатает uid из переменной стенда, поэтому отказ под
//! root проверяется без настоящего повышения прав.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const STUB_CARGO: &str = r#"#!/bin/sh
echo "cargo $*" >> "$STAND/calls.log"
"#;

const STUB_SUDO: &str = r#"#!/bin/sh
echo "sudo $*" >> "$STAND/calls.log"
"#;

// Без STUB_ID_U отвечает uid обычного пользователя.
const STUB_ID: &str = r#"#!/bin/sh
echo "${STUB_ID_U:-501}"
"#;

struct Stand {
    dir: tempfile::TempDir,
}

impl Stand {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("каталог стенда");
        let bin = dir.path().join("bin");
        fs::create_dir(&bin).expect("каталог заглушек");
        for (name, body) in [("cargo", STUB_CARGO), ("sudo", STUB_SUDO), ("id", STUB_ID)] {
            let path = bin.join(name);
            fs::write(&path, body).expect("заглушка");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
                .expect("права заглушки");
        }
        Self { dir }
    }

    fn profile(&self) -> PathBuf {
        let path = self.dir.path().join("profile.json");
        fs::write(&path, "{}\n").expect("профиль стенда");
        path
    }

    fn run(&self, args: &[&str], root: bool) -> Output {
        let mut cmd = Command::new(script());
        cmd.args(args)
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.dir.path().join("bin").display()),
            )
            .env("STAND", self.dir.path());
        if root {
            cmd.env("STUB_ID_U", "0");
        } else {
            cmd.env_remove("STUB_ID_U");
        }
        cmd.output().expect("прогон скрипта")
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.dir.path().join("calls.log")).unwrap_or_default()
    }
}

fn repo() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("корень репозитория")
}

fn script() -> PathBuf {
    repo().join("scripts/install-macos-svc.sh")
}

fn text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr)
}

#[test]
fn build_runs_as_user_and_sudo_gets_only_install() {
    let stand = Stand::new();
    let profile = stand.profile();
    let out = stand.run(
        &[profile.to_str().unwrap(), "--config", "configs/macos.toml"],
        false,
    );
    assert!(out.status.success(), "скрипт упал: {}", text(&out));

    let expected = format!(
        "sudo {} install --profile {} --config configs/macos.toml",
        repo().join("target/debug/xr-macos").display(),
        profile.display(),
    );
    assert_eq!(
        stand.calls().lines().collect::<Vec<_>>(),
        vec!["cargo build -p xr-macos", expected.as_str()],
        "вызовы должны быть ровно эти: сборка от пользователя, sudo только на install"
    );
}

#[test]
fn root_invocation_refused_before_build() {
    let stand = Stand::new();
    let out = stand.run(&[stand.profile().to_str().unwrap()], true);
    assert!(!out.status.success(), "под root скрипт обязан отказать");
    assert!(
        text(&out).contains("sudo не нужен"),
        "причина отказа не названа: {}",
        text(&out)
    );
    assert_eq!(stand.calls(), "", "под root скрипт не должен дойти до сборки");
}
