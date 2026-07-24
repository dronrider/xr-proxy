//! Определение архитектуры цели: несовместимый бинарь не должен доехать
//! (LLD-13 п. 5.4). Имена суффиксов совпадают с раскладкой setup-dist.

use anyhow::{Context, Result};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    X86_64,
    Aarch64,
}

impl Arch {
    /// Разбор вывода `uname -m` (и его синонимов из мира deb/macOS).
    pub fn from_uname(machine: &str) -> Option<Arch> {
        match machine.trim() {
            "x86_64" | "amd64" => Some(Arch::X86_64),
            "aarch64" | "arm64" => Some(Arch::Aarch64),
            _ => None,
        }
    }

    /// Суффикс файлов в setup-dist: `xr-server-<суффикс>`.
    pub fn dist_suffix(&self) -> &'static str {
        match self {
            Arch::X86_64 => "linux-x86_64",
            Arch::Aarch64 => "linux-aarch64",
        }
    }
}

pub fn detect() -> Result<Arch> {
    let machine = Command::new("uname")
        .arg("-m")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_else(|| std::env::consts::ARCH.to_string());
    Arch::from_uname(&machine)
        .with_context(|| format!("архитектура '{}' не поддерживается", machine.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_machines() {
        assert_eq!(Arch::from_uname("x86_64\n"), Some(Arch::X86_64));
        assert_eq!(Arch::from_uname("amd64"), Some(Arch::X86_64));
        assert_eq!(Arch::from_uname("aarch64"), Some(Arch::Aarch64));
        assert_eq!(Arch::from_uname("arm64"), Some(Arch::Aarch64));
    }

    #[test]
    fn rejects_unknown_machine() {
        assert_eq!(Arch::from_uname("mips"), None);
        assert_eq!(Arch::from_uname(""), None);
    }

    #[test]
    fn dist_suffix_matches_layout() {
        assert_eq!(Arch::X86_64.dist_suffix(), "linux-x86_64");
        assert_eq!(Arch::Aarch64.dist_suffix(), "linux-aarch64");
    }
}
