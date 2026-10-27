//! launchd-обвязка сервиса: системный демон, стартующий при загрузке до
//! подъёма сети и перезапускаемый при аварии. Плист рендерится целиком,
//! повторная установка перезаписывает файл.

use crate::exec::Cmd;

pub const LABEL: &str = "dev.xr.macos.svc";
pub const PLIST_PATH: &str = "/Library/LaunchDaemons/dev.xr.macos.svc.plist";
pub const CONFIG_PATH: &str = "/etc/xr-proxy/macos.toml";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServicePaths {
    pub binary: String,
    pub config: String,
    pub log: String,
}

impl ServicePaths {
    pub fn default_paths() -> Self {
        Self {
            binary: "/usr/local/bin/xr-macos".into(),
            config: CONFIG_PATH.into(),
            log: "/var/log/xr-macos.log".into(),
        }
    }
}

pub fn render_plist(paths: &ServicePaths) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{binary}</string>
        <string>run</string>
        <string>--config</string>
        <string>{config}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ThrottleInterval</key>
    <integer>10</integer>
    <key>ProcessType</key>
    <string>Background</string>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#,
        label = LABEL,
        binary = paths.binary,
        config = paths.config,
        log = paths.log,
    )
}

/// Загрузить плист в систему. Современный `bootstrap` и запасной `load -w`
/// для старых сборок: путь одинаковый, разница только в наборе аргументов.
pub fn load_cmd() -> Cmd {
    Cmd::new(crate::exec::LAUNCHCTL, &["bootstrap", "system", PLIST_PATH])
}

pub fn load_legacy_cmd() -> Cmd {
    Cmd::new(crate::exec::LAUNCHCTL, &["load", "-w", PLIST_PATH])
}

pub fn unload_cmd() -> Cmd {
    Cmd::new(crate::exec::LAUNCHCTL, &["bootout", "system", PLIST_PATH])
}

pub fn unload_legacy_cmd() -> Cmd {
    Cmd::new(crate::exec::LAUNCHCTL, &["unload", "-w", PLIST_PATH])
}

pub fn kickstart_cmd() -> Cmd {
    Cmd::new(
        crate::exec::LAUNCHCTL,
        &["kickstart", "-k", &format!("system/{LABEL}")],
    )
}

pub fn state_cmd() -> Cmd {
    Cmd::new(crate::exec::LAUNCHCTL, &["print", &format!("system/{LABEL}")])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_names_paths_and_restart_policy() {
        let text = render_plist(&ServicePaths::default_paths());
        assert!(text.contains("<string>dev.xr.macos.svc</string>"));
        assert!(text.contains("<string>/usr/local/bin/xr-macos</string>"));
        assert!(text.contains("<string>/etc/xr-proxy/macos.toml</string>"));
        assert!(text.contains("<string>/var/log/xr-macos.log</string>"));
        assert!(text.contains("<key>RunAtLoad</key>"));
        assert!(text.contains("<key>KeepAlive</key>"));
        // Аварийный перезапуск не чаще раза в 10 секунд.
        assert!(text.contains("<integer>10</integer>"));
    }

    #[test]
    fn plist_keeps_spaces_in_paths() {
        let paths = ServicePaths {
            binary: "/usr/local/bin/xr macos".into(),
            config: "/etc/xr-proxy/macos.toml".into(),
            log: "/var/log/xr-macos.log".into(),
        };
        let text = render_plist(&paths);
        assert!(text.contains("<string>/usr/local/bin/xr macos</string>"));
    }

    #[test]
    fn cmds_target_system_domain() {
        assert_eq!(
            load_cmd().args,
            vec!["bootstrap", "system", "/Library/LaunchDaemons/dev.xr.macos.svc.plist"]
        );
        assert_eq!(unload_cmd().args[0], "bootout");
        assert_eq!(state_cmd().args, vec!["print", "system/dev.xr.macos.svc"]);
    }
}
