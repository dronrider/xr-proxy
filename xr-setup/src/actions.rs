//! Переиспользуемые шаги установки. Профили собирают из них план;
//! router-профиль (XR-177) добавит свои поверх этих же примитивов.

use crate::fetch::{sha256_hex, BinSource};
use crate::steps::Step;
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

/// Запуск системной команды с внятной ошибкой (stderr в текст).
pub fn run_cmd(argv: &[&str]) -> Result<()> {
    let out = Command::new(argv[0])
        .args(&argv[1..])
        .output()
        .with_context(|| format!("запуск {}", argv[0]))?;
    if !out.status.success() {
        anyhow::bail!(
            "{} завершился с {}: {}",
            argv.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn systemctl_quiet(args: &[&str]) -> bool {
    Command::new("systemctl")
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn unit_active(unit: &str) -> bool {
    systemctl_quiet(&["is-active", "--quiet", unit])
}

fn unit_enabled(unit: &str) -> bool {
    systemctl_quiet(&["is-enabled", "--quiet", unit])
}

/// Атомарная запись: файл появляется целиком или не появляется вовсе,
/// а работающий процесс при замене бинаря продолжает жить со старым inode.
fn write_atomic(path: &PathBuf, bytes: &[u8], mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("создание {}", parent.display()))?;
    }
    let tmp = path.with_extension("xr-setup.new");
    std::fs::write(&tmp, bytes).with_context(|| format!("запись {}", tmp.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
    std::fs::rename(&tmp, path).with_context(|| format!("замена {}", path.display()))?;
    Ok(())
}

fn restart_if_active(unit: Option<&str>) -> Result<()> {
    if let Some(unit) = unit {
        if unit_active(unit) {
            run_cmd(&["systemctl", "restart", unit])?;
        }
    }
    Ok(())
}

/// Доставка бинаря из источника. Без источника уже стоящий бинарь считается
/// целевым состоянием: доустановка на месте работает без сети.
pub struct InstallBinary {
    /// Имя файла в раскладке setup-dist, например `xr-server-linux-x86_64`.
    pub file: String,
    pub dest: PathBuf,
    pub source: Option<Arc<BinSource>>,
    /// Какой сервис перезапустить, если бинарь заменён под работающим юнитом.
    pub restart_unit: Option<String>,
}

impl InstallBinary {
    fn expected_sha(&self) -> Result<Option<String>> {
        let Some(src) = &self.source else {
            return Ok(None);
        };
        match src.expected_sha(&self.file)? {
            Some(sha) => Ok(Some(sha)),
            // Директория без SHA256SUMS: эталоном служит сам файл источника.
            None => Ok(Some(sha256_hex(&src.fetch(&self.file)?))),
        }
    }
}

impl Step for InstallBinary {
    fn name(&self) -> String {
        format!(
            "binary:{}",
            self.dest.file_name().unwrap_or_default().to_string_lossy()
        )
    }

    fn check(&self) -> Result<bool> {
        if !self.dest.exists() {
            return Ok(false);
        }
        match self.expected_sha()? {
            None => Ok(true),
            Some(expected) => {
                let current = sha256_hex(&std::fs::read(&self.dest)?);
                Ok(current == expected)
            }
        }
    }

    fn apply(&self) -> Result<()> {
        let src = self.source.as_ref().with_context(|| {
            format!(
                "{} не установлен, а источник бинарей не задан (--dist-url или --from-dir)",
                self.dest.display()
            )
        })?;
        let bytes = src.fetch(&self.file)?;
        write_atomic(&self.dest, &bytes, 0o755)?;
        restart_if_active(self.restart_unit.as_deref())
    }
}

/// Запись конфига. Существующий файл не перетирается (идемпотентность,
/// LLD-13 п. 5.1), кроме запуска с --force: тогда файл приводится к
/// отрендеренному содержимому.
pub struct WriteConfig {
    pub label: String,
    pub path: PathBuf,
    pub content: String,
    pub mode: u32,
    pub overwrite: bool,
    pub restart_unit: Option<String>,
    /// Файл-спутник, живущий только вместе со свежим конфигом
    /// (пароль админа хаба рядом с его хешем).
    pub extra: Option<(PathBuf, String, u32)>,
}

impl Step for WriteConfig {
    fn name(&self) -> String {
        format!("config:{}", self.label)
    }

    fn check(&self) -> Result<bool> {
        if !self.path.exists() {
            return Ok(false);
        }
        if !self.overwrite {
            return Ok(true);
        }
        Ok(std::fs::read_to_string(&self.path)? == self.content)
    }

    fn apply(&self) -> Result<()> {
        write_atomic(&self.path, self.content.as_bytes(), self.mode)?;
        if let Some((path, content, mode)) = &self.extra {
            write_atomic(path, content.as_bytes(), *mode)?;
        }
        restart_if_active(self.restart_unit.as_deref())
    }
}

/// Закрепить sysctl в sysctl.d и попробовать применить сразу. Отказ
/// `sysctl --system` (ядро без bbr и т.п.) не валит установку: файл на
/// месте и применится на подходящем ядре при загрузке.
pub struct Sysctl {
    pub path: PathBuf,
    pub content: String,
}

impl Step for Sysctl {
    fn name(&self) -> String {
        "sysctl".into()
    }

    fn check(&self) -> Result<bool> {
        Ok(std::fs::read_to_string(&self.path)
            .map(|cur| cur == self.content)
            .unwrap_or(false))
    }

    fn apply(&self) -> Result<()> {
        write_atomic(&self.path, self.content.as_bytes(), 0o644)?;
        if let Err(e) = run_cmd(&["sysctl", "--system"]) {
            println!("      предупреждение: sysctl применится после перезагрузки ({e})");
        }
        Ok(())
    }
}

/// Юнит systemd: файл, daemon-reload, enable и (пере)запуск.
pub struct SystemdUnit {
    pub unit: String,
    pub content: String,
}

impl SystemdUnit {
    fn unit_path(&self) -> PathBuf {
        PathBuf::from(format!("/etc/systemd/system/{}.service", self.unit))
    }
}

impl Step for SystemdUnit {
    fn name(&self) -> String {
        format!("service:{}", self.unit)
    }

    fn check(&self) -> Result<bool> {
        let same = std::fs::read_to_string(self.unit_path())
            .map(|cur| cur == self.content)
            .unwrap_or(false);
        Ok(same && unit_enabled(&self.unit) && unit_active(&self.unit))
    }

    fn apply(&self) -> Result<()> {
        write_atomic(&self.unit_path(), self.content.as_bytes(), 0o644)?;
        run_cmd(&["systemctl", "daemon-reload"])?;
        run_cmd(&["systemctl", "enable", &self.unit])?;
        run_cmd(&["systemctl", "restart", &self.unit])
    }
}

/// Ключ подписи хаба: генерируется однажды, дальше только охраняется.
/// Перегенерация сломала бы доверие уже розданных токенов, поэтому её нет
/// даже под --force.
pub struct SigningKey {
    pub path: PathBuf,
}

impl Step for SigningKey {
    fn name(&self) -> String {
        "hub:signing-key".into()
    }

    fn check(&self) -> Result<bool> {
        Ok(self.path.exists())
    }

    fn apply(&self) -> Result<()> {
        write_atomic(
            &self.path,
            crate::secrets::gen_signing_key().as_bytes(),
            0o600,
        )
    }
}
