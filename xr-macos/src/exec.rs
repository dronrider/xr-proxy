//! Запуск системных утилит macOS. Под launchd PATH бывает пустым, поэтому
//! все программы ходят абсолютными путями и собираются в `Cmd`, а не в
//! сырую командную строку: `Cmd` печатается в журнал и сравнивается
//! в тестах.

use std::io;
use std::process::{Command, Output};

pub const ROUTE: &str = "/sbin/route";
pub const IFCONFIG: &str = "/sbin/ifconfig";
pub const PFCTL: &str = "/sbin/pfctl";
pub const NETWORKSETUP: &str = "/usr/sbin/networksetup";
pub const LAUNCHCTL: &str = "/bin/launchctl";
pub const SCUTIL: &str = "/usr/sbin/scutil";
pub const NETSTAT: &str = "/usr/sbin/netstat";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
}

impl Cmd {
    pub fn new(program: &str, args: &[&str]) -> Self {
        Self {
            program: program.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
        }
    }

    pub fn display(&self) -> String {
        format!("{} {}", self.program, self.args.join(" "))
    }

    pub fn run(&self) -> io::Result<Output> {
        Command::new(&self.program).args(&self.args).output()
    }

    /// Прогон с успешным кодом возврата, иначе ошибка с захватом вывода.
    pub fn run_checked(&self) -> io::Result<String> {
        let out = self.run()?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).to_string())
        } else {
            Err(io::Error::other(format!(
                "{}: {}",
                self.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            )))
        }
    }
}

/// Прогнать команду, проглотив отказ. Снос маршрута, которого уже нет в
/// таблице, это не ошибка восстановления: главное, что после прогона
/// маршрута нет.
pub fn run_quiet(cmd: &Cmd) -> bool {
    cmd.run().map(|o| o.status.success()).unwrap_or(false)
}

pub fn run_capture(cmd: &Cmd) -> Option<String> {
    cmd.run()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_joins_program_and_args() {
        let cmd = Cmd::new(ROUTE, &["-n", "add", "-inet", "0.0.0.0/1"]);
        assert_eq!(cmd.display(), "/sbin/route -n add -inet 0.0.0.0/1");
    }

    #[test]
    fn run_checked_reports_stderr_on_failure() {
        let cmd = Cmd::new("/usr/bin/false", &[]);
        let err = cmd.run_checked().unwrap_err();
        assert!(err.to_string().starts_with("/usr/bin/false"));
    }
}
