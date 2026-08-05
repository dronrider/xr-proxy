//! Прогон `scripts/release-apk.sh` на стенде из заглушек (XR-109).
//!
//! Настоящие ssh, rsync и curl звать нельзя, а хабы стенда это просто два
//! каталога. Заглушки пишут вызовы в лог и работают с этими каталогами,
//! поэтому проверяется не текст скрипта, а то, что после прогона реально
//! лежит на «хабах»: подписанный манифест, APK, бэкапы прежней пары.
//! Подпись и проверка идут настоящим `xr-hub`, поэтому краснота ловится и на
//! подмене ключа.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const STUB_SSH: &str = r#"#!/usr/bin/env python3
import os, subprocess, sys
stand = os.environ["STAND"]
with open(os.path.join(stand, "calls.log"), "a") as f:
    f.write("ssh " + " ".join(sys.argv[1:]) + "\n")
sys.exit(subprocess.run(["sh", "-c", sys.argv[-1]]).returncode)
"#;

const STUB_RSYNC: &str = r#"#!/usr/bin/env python3
import os, shutil, sys
stand = os.environ["STAND"]
with open(os.path.join(stand, "calls.log"), "a") as f:
    f.write("rsync " + " ".join(sys.argv[1:]) + "\n")
args, paths, i = sys.argv[1:], [], 0
while i < len(args):
    a = args[i]
    if a == "-e":
        i += 2
        continue
    if a.startswith("-"):
        i += 1
        continue
    paths.append(a)
    i += 1
dest = paths.pop()
# Без user@host слева заливка легла бы в локальный каталог с тем же путём,
# и на хаб не уехало бы ничего.
if ":" not in dest:
    sys.stderr.write("rsync: цель без user@host: " + dest + "\n")
    sys.exit(1)
dest = dest.split(":", 1)[1]
os.makedirs(dest, exist_ok=True)
for p in paths:
    shutil.copy2(p, dest)
"#;

// Хаб стенда это каталог: latest собирается из лежащих там manifest.json и
// manifest.sig, download отвечает по наличию файла APK.
const STUB_CURL: &str = r#"#!/usr/bin/env python3
import json, os, sys
stand = os.environ["STAND"]
args = sys.argv[1:]
with open(os.path.join(stand, "calls.log"), "a") as f:
    f.write("curl " + " ".join(args) + "\n")
urls = [a for a in args if a.startswith("http")]
if not urls:
    sys.exit(2)
url = urls[-1]
hub = os.path.join(stand, "secondary" if "127.0.0.1" in url else "primary")
if "/app/latest" in url:
    try:
        manifest = open(os.path.join(hub, "manifest.json")).read()
        signature = open(os.path.join(hub, "manifest.sig")).read()
    except FileNotFoundError:
        sys.exit(22)
    sys.stdout.write(json.dumps({"manifest": manifest, "signature": signature}))
elif "/app/download/" in url:
    ok = os.path.exists(os.path.join(hub, url.rsplit("/", 1)[1] + ".apk"))
    if "-w" in args:
        sys.stdout.write("200" if ok else "404")
    sys.exit(0 if ok else 22)
else:
    sys.exit(2)
"#;

const STUB_BUILD: &str = r#"#!/bin/sh
echo "build $ORG_GRADLE_PROJECT_xrVersionName $ORG_GRADLE_PROJECT_xrVersionCode $ORG_GRADLE_PROJECT_xrReleasePublicKey" >> "$STAND/calls.log"
mkdir -p "$(dirname "$APK_PATH")"
printf 'apk %s\n' "$ORG_GRADLE_PROJECT_xrVersionName" > "$APK_PATH"
"#;

struct Stand {
    dir: tempfile::TempDir,
    pubkey: String,
}

fn write_exec(path: &Path, body: &str) {
    fs::write(path, body).expect("файл стенда");
    let mut perms = fs::metadata(path).expect("метаданные").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).expect("права");
}

fn xr_hub() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_xr-hub"))
}

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("корень репозитория")
        .join("scripts/release-apk.sh")
}

impl Stand {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("временный каталог");
        let root = dir.path();
        fs::create_dir(root.join("bin")).expect("каталог заглушек");
        for (name, body) in [
            ("ssh", STUB_SSH),
            ("rsync", STUB_RSYNC),
            ("curl", STUB_CURL),
        ] {
            write_exec(&root.join("bin").join(name), body);
        }
        write_exec(&root.join("build.sh"), STUB_BUILD);
        fs::create_dir(root.join("primary")).expect("первичный хаб");
        fs::create_dir(root.join("secondary")).expect("второй хаб");

        // Ключ настоящий: обе половины берём у самого xr-hub, чтобы стенд не
        // повторял криптографию.
        let out = Command::new(xr_hub())
            .arg("gen-release-key")
            .output()
            .expect("gen-release-key");
        let text = String::from_utf8_lossy(&out.stdout);
        let keys: Vec<&str> = text
            .lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
            .collect();
        let (private, pubkey) = (keys[0].to_string(), keys[1].to_string());
        fs::write(root.join("release.key"), &private).expect("ключ");

        let stand = Stand { dir, pubkey };
        stand.write_env();
        stand
    }

    fn at(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn write_env(&self) {
        let body = format!(
            "RELEASE_KEY={key}\n\
             RELEASE_PUBKEY={pubkey}\n\
             HUB_URL=https://hub.stand\n\
             PRIMARY_HOST=hub-primary\n\
             PRIMARY_RELEASES_DIR={primary}\n\
             SECONDARY_HOST=hub-secondary\n\
             SECONDARY_RELEASES_DIR={secondary}\n\
             SECONDARY_HOST_HEADER=hub.stand\n",
            key = self.at("release.key").display(),
            pubkey = self.pubkey,
            primary = self.at("primary").display(),
            secondary = self.at("secondary").display(),
        );
        fs::write(self.at("release.env"), body).expect("env стенда");
    }

    /// Выложить на оба хаба уже подписанный релиз: так выглядит хаб к моменту
    /// очередного выпуска, и на нём проверяются бэкап и рост versionCode.
    fn publish(&self, version: &str, code: u64) {
        let apk = self.at("old.apk");
        fs::write(&apk, format!("apk {version}")).expect("старый apk");
        let staging = self.at("old-staging");
        let status = Command::new(xr_hub())
            .args([
                "sign-release",
                "--apk",
                &apk.display().to_string(),
                "--version",
                version,
                "--version-code",
                &code.to_string(),
                "--key",
                &self.at("release.key").display().to_string(),
                "--base-url",
                "https://hub.stand",
                "--out",
                &staging.display().to_string(),
            ])
            .status()
            .expect("подпись старого релиза");
        assert!(status.success());
        for hub in ["primary", "secondary"] {
            for name in ["manifest.json", "manifest.sig"] {
                fs::copy(staging.join(name), self.at(hub).join(name)).expect("выкладка");
            }
            fs::copy(&apk, self.at(hub).join(format!("{version}.apk"))).expect("выкладка apk");
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(script())
            .args(args)
            .env("PATH", format!("{}:/usr/bin:/bin", self.at("bin").display()))
            .env("STAND", self.dir.path())
            .env("RELEASE_ENV", self.at("release.env"))
            .env("XR_HUB_BIN", xr_hub())
            .env("ANDROID_BUILD", self.at("build.sh"))
            .env("APK_PATH", self.at("build/app-release.apk"))
            .env("STAGING_ROOT", self.at("staging"))
            .output()
            .expect("прогон скрипта")
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.at("calls.log")).unwrap_or_default()
    }
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// Полный проход: собрали, подписали, залили на оба хаба, оба отдают новую
/// версию. Прежняя пара манифестов ложится бэкапом, staging-каталог за собой
/// не остаётся.
#[test]
fn full_release_lands_on_both_hubs() {
    let stand = Stand::new();
    stand.publish("0.5.0", 5);

    let out = stand.run(&["--version", "0.6.0", "--version-code", "6"]);
    let log = text(&out);
    assert!(out.status.success(), "{log}");

    for hub in ["primary", "secondary"] {
        let dir = stand.at(hub);
        assert!(dir.join("0.6.0.apk").is_file(), "{hub}: нет нового APK");
        let manifest = fs::read_to_string(dir.join("manifest.json")).expect("манифест");
        assert!(manifest.contains("\"version_code\": 6"), "{hub}: {manifest}");
        assert!(dir.join("0.5.0.apk").is_file(), "{hub}: старый APK удалён");
        let backups: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("manifest.json.bak."))
            .collect();
        assert_eq!(backups.len(), 1, "{hub}: бэкап манифеста {backups:?}");
        let staged: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".staging-"))
            .collect();
        assert!(staged.is_empty(), "{hub}: остался staging {staged:?}");
    }

    // Версия и ключ уехали в сборку, иначе gradle поставил бы versionCode=1
    // и самообновление бы молчало.
    assert!(
        stand
            .calls()
            .contains(&format!("build 0.6.0 6 {}", stand.pubkey)),
        "{}",
        stand.calls()
    );
    assert!(log.contains("0.6.0 (code 6)"), "{log}");
    assert!(log.contains("sha256 APK:"), "{log}");
    // Живой APK это полсотни мегабайт, и диапазоны хаб игнорирует: проверять
    // отдачу можно только HEAD, иначе каждый релиз качает его обратно.
    assert!(
        stand.calls().contains("curl -fsS -I"),
        "download проверен не HEAD: {}",
        stand.calls()
    );
}

/// Живой релиз читается с первичного хаба, и `--bump` берёт следующий код по
/// нашему соглашению об имени версии.
#[test]
fn bump_takes_the_next_code() {
    let stand = Stand::new();
    stand.publish("0.5.0", 5);

    let out = stand.run(&["--bump"]);
    let log = text(&out);
    assert!(out.status.success(), "{log}");
    assert!(log.contains("0.6.0 (code 6)"), "{log}");
    assert!(stand.at("primary").join("0.6.0.apk").is_file(), "{log}");
}

/// versionCode не больше живого значит обновление, которого приложение не
/// увидит. До сборки такое останавливается.
#[test]
fn refuses_version_code_that_does_not_grow() {
    let stand = Stand::new();
    stand.publish("0.5.0", 5);

    let out = stand.run(&["--version", "0.5.1", "--version-code", "5"]);
    let log = text(&out);
    assert!(!out.status.success(), "{log}");
    assert!(log.contains("не больше живого"), "{log}");
    assert!(!stand.calls().contains("build "), "сборка стартовала: {log}");
}

/// Подписать чужим ключом значит выкатить OTA, которое установленные
/// приложения отвергнут. Ключ судится по подписи живого манифеста.
#[test]
fn refuses_a_foreign_signing_key() {
    let stand = Stand::new();
    stand.publish("0.5.0", 5);

    let out = Command::new(xr_hub())
        .arg("gen-release-key")
        .output()
        .expect("второй ключ");
    let text_out = String::from_utf8_lossy(&out.stdout);
    let other = text_out
        .lines()
        .find(|l| !l.starts_with('#') && !l.trim().is_empty())
        .expect("приватная половина")
        .to_string();
    fs::write(stand.at("release.key"), other).expect("подмена ключа");

    let out = stand.run(&["--version", "0.6.0", "--version-code", "6"]);
    let log = text(&out);
    assert!(!out.status.success(), "{log}");
    assert!(log.contains("это не тот ключ"), "{log}");
    assert!(!stand.calls().contains("build "), "сборка стартовала: {log}");
}

/// Перезапуск после обрыва заливки: сборка долгая, повторять её не нужно, а
/// на хаб уезжает уже подписанный каталог.
#[test]
fn only_upload_skips_build_and_signing() {
    let stand = Stand::new();
    stand.publish("0.5.0", 5);
    let first = stand.run(&["--version", "0.6.0", "--version-code", "6", "--primary-only"]);
    assert!(first.status.success(), "{}", text(&first));
    for name in ["manifest.json", "manifest.sig", "0.6.0.apk"] {
        fs::remove_file(stand.at("secondary").join(name)).ok();
    }
    fs::write(stand.at("calls.log"), "").expect("сброс лога");

    let out = stand.run(&["--only-upload", "--version", "0.6.0", "--version-code", "6"]);
    let log = text(&out);
    assert!(out.status.success(), "{log}");
    assert!(!stand.calls().contains("build "), "пересобрал: {log}");
    assert!(stand.at("secondary").join("0.6.0.apk").is_file(), "{log}");
}

/// Сухой прогон печатает шаги и не трогает ни хабы, ни сборку.
#[test]
fn dry_run_touches_nothing() {
    let stand = Stand::new();
    stand.publish("0.5.0", 5);

    let out = stand.run(&["--version", "0.6.0", "--version-code", "6", "--dry-run"]);
    let log = text(&out);
    assert!(out.status.success(), "{log}");
    assert!(log.contains("[dry-run]"), "{log}");
    assert!(!stand.at("primary").join("0.6.0.apk").exists(), "{log}");
    assert!(!stand.calls().contains("build "), "{log}");
    assert!(!stand.calls().contains("rsync "), "{log}");
}

/// Без env-файла скрипт не гадает адреса, а печатает шаблон.
#[test]
fn missing_env_prints_the_template() {
    let stand = Stand::new();
    fs::remove_file(stand.at("release.env")).expect("удаление env");

    let out = stand.run(&["--version", "0.6.0", "--version-code", "6"]);
    let log = text(&out);
    assert!(!out.status.success(), "{log}");
    assert!(log.contains("RELEASE_KEY="), "{log}");
    assert!(log.contains("SECONDARY_HOST_HEADER="), "{log}");
}
