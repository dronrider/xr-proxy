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

/// Тихий запуск: важен только код возврата (is-active, running и т.п.).
pub fn cmd_ok(argv: &[&str]) -> bool {
    Command::new(argv[0])
        .args(&argv[1..])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn unit_active(unit: &str) -> bool {
    cmd_ok(&["systemctl", "is-active", "--quiet", unit])
}

fn unit_enabled(unit: &str) -> bool {
    cmd_ok(&["systemctl", "is-enabled", "--quiet", unit])
}

/// Чей сервис перезапускать после замены бинаря или конфига: юнит systemd
/// на VPS либо procd-скрипт init.d на OpenWRT. Неработающий сервис не
/// трогается, его поднимет свой шаг плана.
#[derive(Clone)]
pub enum Restart {
    Unit(String),
    Initd(PathBuf),
}

impl Restart {
    pub fn kick(&self) -> Result<()> {
        match self {
            Restart::Unit(unit) => {
                if unit_active(unit) {
                    run_cmd(&["systemctl", "restart", unit])?;
                }
            }
            Restart::Initd(init) => {
                let init = init.to_string_lossy().into_owned();
                if cmd_ok(&[&init, "running"]) {
                    run_cmd(&[&init, "restart"])?;
                }
            }
        }
        Ok(())
    }
}

/// Атомарная запись: файл появляется целиком или не появляется вовсе,
/// а работающий процесс при замене бинаря продолжает жить со старым inode.
/// Права выставляются при создании, чтобы секрет ни мгновения не лежал
/// с правами по umask.
fn write_atomic(path: &PathBuf, bytes: &[u8], mode: u32) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("создание {}", parent.display()))?;
    }
    let tmp = path.with_extension("xr-setup.new");
    std::fs::remove_file(&tmp).ok();
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)
        .with_context(|| format!("создание {}", tmp.display()))?;
    f.write_all(bytes)
        .with_context(|| format!("запись {}", tmp.display()))?;
    drop(f);
    std::fs::rename(&tmp, path).with_context(|| format!("замена {}", path.display()))?;
    Ok(())
}

fn restart_if_active(restart: Option<&Restart>) -> Result<()> {
    if let Some(restart) = restart {
        restart.kick()?;
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
    /// Какой сервис перезапустить, если бинарь заменён под работающим.
    pub restart: Option<Restart>,
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
        restart_if_active(self.restart.as_ref())
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
    pub restart: Option<Restart>,
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
        // Спутник первым: обрыв между записями не должен оставить конфиг
        // хаба с хешем пароля, которого нигде больше нет.
        if let Some((path, content, mode)) = &self.extra {
            write_atomic(path, content.as_bytes(), *mode)?;
        }
        write_atomic(&self.path, self.content.as_bytes(), self.mode)?;
        restart_if_active(self.restart.as_ref())
    }
}

/// Исполняемый скрипт установки (watchdog, nftables-обвязка). В отличие от
/// конфига это код: содержимое всегда приводится к вшитому в установщик,
/// правки на месте перетираются.
pub struct InstallScript {
    pub label: String,
    pub path: PathBuf,
    pub content: String,
}

impl Step for InstallScript {
    fn name(&self) -> String {
        format!("script:{}", self.label)
    }

    fn check(&self) -> Result<bool> {
        Ok(std::fs::read_to_string(&self.path)
            .map(|cur| cur == self.content)
            .unwrap_or(false))
    }

    fn apply(&self) -> Result<()> {
        write_atomic(&self.path, self.content.as_bytes(), 0o755)
    }
}

/// Закрепить sysctl в sysctl.d и попробовать применить сразу. Отказ
/// применения (ядро без bbr и т.п.) не валит установку: файл на месте и
/// применится при загрузке.
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
        // busybox-sysctl на OpenWRT не знает --system, ему нужен -p <файл>.
        let path = self.path.to_string_lossy().into_owned();
        if run_cmd(&["sysctl", "--system"]).is_err() {
            if let Err(e) = run_cmd(&["sysctl", "-p", &path]) {
                println!("      предупреждение: sysctl применится после перезагрузки ({e})");
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::BinSource;
    use std::os::unix::fs::PermissionsExt;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xr-setup-actions-{tag}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn write_config_keeps_existing_without_overwrite_and_rewrites_with_it() {
        let dir = tmpdir("conf");
        let path = dir.join("etc/server.toml");
        let step = WriteConfig {
            label: "server".into(),
            path: path.clone(),
            content: "новое".into(),
            mode: 0o600,
            overwrite: false,
            restart: None,
            extra: None,
        };
        assert!(!step.check().unwrap());
        step.apply().unwrap();
        assert!(step.check().unwrap());
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        // Существующий файл с другим содержимым: без overwrite целевое
        // состояние достигнуто, с overwrite шаг требует перезаписи.
        std::fs::write(&path, "старое").unwrap();
        assert!(step.check().unwrap(), "чужой конфиг не трогаем");
        let force = WriteConfig { overwrite: true, ..step };
        assert!(!force.check().unwrap());
        force.apply().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "новое");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_config_extra_lands_with_the_config() {
        let dir = tmpdir("extra");
        let step = WriteConfig {
            label: "hub".into(),
            path: dir.join("config.toml"),
            content: "conf".into(),
            mode: 0o600,
            overwrite: false,
            restart: None,
            extra: Some((dir.join("admin.pass"), "pass\n".into(), 0o600)),
        };
        step.apply().unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("admin.pass")).unwrap(), "pass\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn install_binary_checks_against_source_and_requires_one_when_missing() {
        let dir = tmpdir("bin");
        let src_dir = dir.join("dist");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(src_dir.join("xr-server-linux-x86_64"), b"v2").unwrap();
        let dest = dir.join("bin/xr-server");

        let no_source = InstallBinary {
            file: "xr-server-linux-x86_64".into(),
            dest: dest.clone(),
            source: None,
            restart: None,
        };
        assert!(!no_source.check().unwrap());
        assert!(
            no_source.apply().unwrap_err().to_string().contains("источник"),
            "без бинаря и без источника нужен внятный отказ"
        );

        let with_source = InstallBinary {
            source: Some(std::sync::Arc::new(BinSource::Dir(src_dir.clone()))),
            ..no_source
        };
        with_source.apply().unwrap();
        assert!(with_source.check().unwrap());
        assert_eq!(
            std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777,
            0o755
        );

        // Источник обновился: check видит расхождение, apply доводит.
        std::fs::write(src_dir.join("xr-server-linux-x86_64"), b"v3").unwrap();
        assert!(!with_source.check().unwrap());
        with_source.apply().unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"v3");

        // Установленный бинарь без источника это целевое состояние.
        let offline = InstallBinary {
            file: "xr-server-linux-x86_64".into(),
            dest,
            source: None,
            restart: None,
        };
        assert!(offline.check().unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sysctl_check_compares_content() {
        let dir = tmpdir("sysctl");
        let step = Sysctl {
            path: dir.join("99-xr-proxy.conf"),
            content: "net.core.default_qdisc=fq\n".into(),
        };
        assert!(!step.check().unwrap());
        std::fs::write(&step.path, "другое\n").unwrap();
        assert!(!step.check().unwrap());
        std::fs::write(&step.path, &step.content).unwrap();
        assert!(step.check().unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }
}
