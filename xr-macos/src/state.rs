//! Персистентное состояние сервиса. Каждая правка сети (DNS, маршруты,
//! ссылка PF, кэш IP серверов) оседает файлом до применения: штатная
//! остановка и аварийное восстановление читают одно и то же место, а
//! повторный старт после аварии не теряет, что именно было изменено.

use std::fs;
use std::path::PathBuf;

use serde::{de::DeserializeOwned, Deserialize, Serialize};

pub const STATE_DIR: &str = "/var/db/xr-proxy/macos";
const DNS_FILE: &str = "dns-saved.json";
const ROUTES_FILE: &str = "routes.json";
const PF_TOKEN_FILE: &str = "pf-enable-ref";
const SERVERS_FILE: &str = "servers.json";
const STATUS_FILE: &str = "status.json";

#[derive(Debug, Clone)]
pub struct StateDir {
    pub path: PathBuf,
}

impl StateDir {
    pub fn system() -> Self {
        Self { path: PathBuf::from(STATE_DIR) }
    }

    /// Каталог для тестов: то же поведение, путь во временном дереве.
    #[cfg(test)]
    pub fn at(path: &std::path::Path) -> Self {
        Self { path: path.to_path_buf() }
    }

    fn file(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    fn ensure_dir(&self) -> std::io::Result<()> {
        fs::create_dir_all(&self.path)
    }

    fn write_json<T: Serialize>(&self, name: &str, value: &T) -> std::io::Result<()> {
        self.ensure_dir()?;
        let text = serde_json::to_string_pretty(value)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        // Временный файл и переименование: чтение восстановления никогда не
        // видит полупустой JSON после аварии между записью и синком.
        let tmp = self.file(&format!("{name}.tmp"));
        fs::write(&tmp, text)?;
        fs::rename(&tmp, self.file(name))
    }

    fn read_json<T: DeserializeOwned>(&self, name: &str) -> Option<T> {
        let text = fs::read_to_string(self.file(name)).ok()?;
        serde_json::from_str(&text).ok()
    }

    fn remove(&self, name: &str) {
        let _ = fs::remove_file(self.file(name));
    }

    pub fn save_dns(&self, snapshot: &[crate::dnsconf::ServiceDns]) -> std::io::Result<()> {
        self.write_json(DNS_FILE, &snapshot)
    }

    pub fn load_dns(&self) -> Option<Vec<crate::dnsconf::ServiceDns>> {
        self.read_json(DNS_FILE)
    }

    pub fn clear_dns(&self) {
        self.remove(DNS_FILE);
    }

    pub fn save_routes(&self, specs: &[crate::routes::RouteSpec]) -> std::io::Result<()> {
        self.write_json(ROUTES_FILE, &specs)
    }

    pub fn load_routes(&self) -> Option<Vec<crate::routes::RouteSpec>> {
        self.read_json(ROUTES_FILE)
    }

    pub fn clear_routes(&self) {
        self.remove(ROUTES_FILE);
    }

    pub fn save_pf_ref(&self, reference: &str) -> std::io::Result<()> {
        self.ensure_dir()?;
        fs::write(self.file(PF_TOKEN_FILE), reference)
    }

    pub fn load_pf_ref(&self) -> Option<String> {
        fs::read_to_string(self.file(PF_TOKEN_FILE))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    pub fn clear_pf_ref(&self) {
        self.remove(PF_TOKEN_FILE);
    }

    pub fn save_servers(&self, servers: &[(std::net::IpAddr, u16)]) -> std::io::Result<()> {
        let list: Vec<(String, u16)> = servers
            .iter()
            .map(|(ip, port)| (ip.to_string(), *port))
            .collect();
        self.write_json(SERVERS_FILE, &list)
    }

    pub fn load_servers(&self) -> Option<Vec<(std::net::IpAddr, u16)>> {
        let list: Vec<(String, u16)> = self.read_json(SERVERS_FILE)?;
        Some(
            list.into_iter()
                .filter_map(|(ip, port)| Some((ip.parse().ok()?, port)))
                .collect(),
        )
    }

    pub fn save_status(&self, status: &StatusFile) -> std::io::Result<()> {
        self.write_json(STATUS_FILE, status)
    }

    pub fn load_status(&self) -> Option<StatusFile> {
        self.read_json(STATUS_FILE)
    }
}

/// Живой статус демона для команды `status`. Пишется сторожем каждые
/// несколько секунд, чтение идёт отдельным процессом.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct StatusFile {
    pub state: String,
    pub uptime_seconds: u64,
    pub bytes_up: u64,
    pub bytes_down: u64,
    pub active_connections: u32,
    pub quic_dropped: u64,
    pub tun: String,
    pub gateway: String,
    pub dns_applied: bool,
    pub killswitch_armed: bool,
    pub servers: Vec<String>,
    /// Человекочитаемая причина последней ошибки, она же попадает в вывод
    /// диагностики.
    pub last_error: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dnsconf::ServiceDns;
    use crate::routes::RouteSpec;

    #[test]
    fn dns_snapshot_roundtrips_and_clears() {
        let dir = StateDir::at(&tempfile::tempdir().unwrap().path().to_path_buf());
        let snapshot = vec![ServiceDns {
            service: "Wi-Fi".into(),
            servers: vec!["192.0.2.1".into()],
        }];
        dir.save_dns(&snapshot).unwrap();
        assert_eq!(dir.load_dns(), Some(snapshot));
        dir.clear_dns();
        assert_eq!(dir.load_dns(), None);
    }

    #[test]
    fn routes_roundtrip_through_json() {
        let dir = StateDir::at(&tempfile::tempdir().unwrap().path().to_path_buf());
        let specs = vec![RouteSpec::via_tun("0.0.0.0/1", "v4", "utun9")];
        dir.save_routes(&specs).unwrap();
        assert_eq!(dir.load_routes(), Some(specs));
        dir.clear_routes();
        assert_eq!(dir.load_routes(), None);
    }

    #[test]
    fn pf_ref_survives_trim_and_clear() {
        let dir = StateDir::at(&tempfile::tempdir().unwrap().path().to_path_buf());
        dir.save_pf_ref("2967317173964597489").unwrap();
        assert_eq!(dir.load_pf_ref(), Some("2967317173964597489".to_string()));
        dir.clear_pf_ref();
        assert_eq!(dir.load_pf_ref(), None);
    }

    #[test]
    fn servers_cache_roundtrips() {
        let dir = StateDir::at(&tempfile::tempdir().unwrap().path().to_path_buf());
        let servers = vec![("203.0.113.10".parse().unwrap(), 8443u16)];
        dir.save_servers(&servers).unwrap();
        assert_eq!(dir.load_servers(), Some(servers));
    }

    #[test]
    fn status_roundtrips_with_error_reason() {
        let dir = StateDir::at(&tempfile::tempdir().unwrap().path().to_path_buf());
        let status = StatusFile {
            state: "Error".into(),
            last_error: "сервер недоступен: timeout".into(),
            dns_applied: true,
            ..Default::default()
        };
        dir.save_status(&status).unwrap();
        assert_eq!(dir.load_status(), Some(status));
    }

    #[test]
    fn corrupt_files_read_as_absent() {
        let dir = StateDir::at(&tempfile::tempdir().unwrap().path().to_path_buf());
        dir.ensure_dir().unwrap();
        fs::write(dir.file(ROUTES_FILE), "{ битый").unwrap();
        assert_eq!(dir.load_routes(), None);
    }
}
