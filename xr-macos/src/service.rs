//! Демон сервиса: сетевая обвязка macOS вокруг общего ядра xr-core.
//! Последовательность старта закрывает окно прямой утечки как можно раньше:
//! kill-switch взводится до подъёма туннеля, падение движка не снимает
//! правила, а следующая итерация сторожа повторяет попытку. Штатная
//! остановка и восстановление откатывают правки сети в обратном порядке:
//! маршруты, DNS и лишь затем правила PF.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::watch;
use xr_core::engine::VpnEngine;
use xr_core::ip_stack::PacketQueue;
use xr_core::session::ProtectSocketFn;
use xr_core::state::VpnState;

use crate::config::ServiceConfig;
use crate::dnsconf::{self, ServiceDns};
use crate::exec::{self, run_capture, run_quiet};
use crate::pf::{self, KillSwitch};
use crate::profile::{self, EngineSetup};
use crate::routes::{self, DefaultRoute, RouteSpec};
use crate::state::{StateDir, StatusFile};
use crate::utun::Utun;

const IP_BOUND_IF: libc::c_int = 25;
const IPV6_BOUND_IF: libc::c_int = 125;

pub struct Daemon {
    cfg: ServiceConfig,
    state: StateDir,
    /// Профиль и резолверы системы: конфиг движка пересобирается из них на
    /// каждый перезапуск, `VpnConfig` не клонируется.
    profile_json: String,
    dns_resolvers: Vec<String>,
    /// Серверы профиля для планирования маршрутов и правил.
    endpoints: Vec<profile::Endpoint>,
    relay_port: u16,
    engine: Option<Arc<Mutex<VpnEngine>>>,
    queue: PacketQueue,
    tun: Option<Arc<Utun>>,
    /// Физический интерфейс по умолчанию для защищённых сокетов движка.
    phys: Arc<Mutex<String>>,
    servers: Vec<(IpAddr, u16)>,
    dns_applied: bool,
    last_error: String,
    routes_applied: Vec<RouteSpec>,
    pf_loaded: bool,
    pf_ref: Option<String>,
}

impl Daemon {
    pub fn new(
        cfg: ServiceConfig,
        setup: EngineSetup,
        profile_json: String,
        dns_resolvers: Vec<String>,
        state: StateDir,
    ) -> Self {
        let queue_capacity = setup.config.tuning.queue_capacity;
        let relay_port = setup.config.udp_relay_port;
        Self {
            cfg,
            state,
            profile_json,
            dns_resolvers,
            endpoints: setup.endpoints,
            relay_port,
            engine: None,
            queue: PacketQueue::with_capacity(queue_capacity),
            tun: None,
            phys: Arc::new(Mutex::new(String::new())),
            servers: Vec::new(),
            dns_applied: false,
            last_error: String::new(),
            routes_applied: Vec::new(),
            pf_loaded: false,
            pf_ref: None,
        }
    }

    pub fn tun_handle(&self) -> Option<Arc<Utun>> {
        self.tun.clone()
    }

    pub fn queue_handle(&self) -> PacketQueue {
        self.queue.clone()
    }

    /// Правила PF стоят в анкоре. Отсутствие туннеля не отменяет признак:
    /// правила ожидания тоже режут прямой выход.
    pub fn killswitch_armed(&self) -> bool {
        self.pf_loaded
    }

    /// Полный подъём сети: kill-switch до туннеля, туннель до маршрутов,
    /// маршруты до DNS. Ошибки шага не роняют демон, сторож повторяет.
    pub fn setup_network(&mut self) {
        self.arm_killswitch();
        if self.tun.is_none() {
            match Utun::open() {
                Ok(tun) => {
                    tracing::info!("utun поднят: {}", tun.ifname);
                    self.tun = Some(Arc::new(tun));
                }
                Err(e) => {
                    self.fail(&format!("utun не открылся: {e}"));
                    return;
                }
            }
        }
        let tun = self.tun.as_ref().unwrap().ifname.clone();
        for cmd in routes::ifconfig_cmds(
            &tun,
            &self.cfg.tun_address_v4.to_string(),
            &self.cfg.dns_server.to_string(),
            &self.cfg.tun_address_v6.to_string(),
            self.cfg.tun_mtu,
        ) {
            if let Err(e) = cmd.run_checked() {
                self.fail(&format!("адрес на {}: {e}", cmd.display()));
            }
        }
        self.apply_routes();
        self.apply_dns();
        self.arm_killswitch();
    }

    /// Взвести kill-switch. Разрешения серверам берутся из разрешённых IP;
    /// пока адресов нет, правила уходят в режим ожидания с DNS наружу,
    /// иначе имя хоста из профиля не разрешить никогда.
    pub fn arm_killswitch(&mut self) {
        let tun = self
            .tun
            .as_ref()
            .map(|t| t.ifname.clone())
            .unwrap_or_default();
        let mut by_ip: Vec<(IpAddr, Vec<u16>)> = Vec::new();
        for (ip, port) in &self.servers {
            match by_ip.iter_mut().find(|(i, _)| i == ip) {
                Some((_, ports)) => ports.push(*port),
                None => by_ip.push((*ip, vec![*port, self.relay_port])),
            }
        }
        let ks = KillSwitch {
            tun,
            servers: by_ip,
            allow_direct_dns: self.cfg.allow_direct_dns,
            dns_servers: self.dns_resolver_ips(),
            pending: self.servers.is_empty(),
        };
        let rules_path = self.state.path.join("kill-switch.conf");
        if let Err(e) = std::fs::create_dir_all(&self.state.path)
            .and_then(|_| std::fs::write(&rules_path, ks.render_rules()))
        {
            self.fail(&format!("файл правил PF: {e}"));
            return;
        }
        if self.pf_ref.is_none() {
            if let Some(reference) = self.state.load_pf_ref() {
                self.pf_ref = Some(reference);
            }
        }
        if self.pf_ref.is_none() {
            if let Ok(out) = pf::enable_cmd().run() {
                let text = format!(
                    "{}{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                );
                if let Some(reference) = pf::parse_token(&text) {
                    let _ = self.state.save_pf_ref(&reference);
                    self.pf_ref = Some(reference);
                }
            }
        }
        let path = rules_path.to_string_lossy().to_string();
        match pf::load_cmd(&path).run_checked() {
            Ok(_) => {
                self.pf_loaded = true;
                tracing::info!("kill-switch взведён");
            }
            Err(e) => {
                self.pf_loaded = false;
                self.fail(&format!("правила PF: {e}"));
            }
        }
    }

    fn dns_resolver_ips(&self) -> Vec<IpAddr> {
        self.dns_resolvers
            .iter()
            .filter_map(|s| s.trim().parse::<IpAddr>().ok())
            .collect()
    }

    /// Маршруты: пересобираются целиком, при смене шлюза старые записи
    /// снимаются. Спецификации пишутся на диск до применения, поэтому
    /// восстановление после аварии снимает ровно то, что ставилось. Уже
    /// стоящие записи не трогаются, чтобы каждый тик не ронял трафик.
    fn apply_routes(&mut self) {
        let tun = match self.tun.as_ref() {
            Some(t) => t.ifname.clone(),
            None => return,
        };
        let v4 = self.default_route(false);
        let v6 = self.default_route(true);
        if let Some(dr) = &v4 {
            *self.phys.lock().unwrap() = dr.interface.clone();
        }
        let new = routes::plan(
            &tun,
            IpAddr::V4(self.cfg.dns_server),
            v4.as_ref(),
            v6.as_ref(),
            &self.servers,
        );
        for old in &self.routes_applied {
            if !new.contains(old) {
                run_quiet(&old.del_cmd());
            }
        }
        let mut applied = Vec::new();
        for spec in &new {
            if self.routes_applied.contains(spec) {
                applied.push(spec.clone());
                continue;
            }
            if apply_route(spec) {
                applied.push(spec.clone());
            } else {
                self.fail(&format!("маршрут {}: не встал", spec.dst));
            }
        }
        let _ = self.state.save_routes(&applied);
        self.routes_applied = applied;
    }

    fn default_route(&self, v6: bool) -> Option<DefaultRoute> {
        let cmd = if v6 {
            exec::Cmd::new(exec::ROUTE, &["-n", "get", "-inet6", "default"])
        } else {
            exec::Cmd::new(exec::ROUTE, &["-n", "get", "default"])
        };
        run_capture(&cmd).and_then(|out| routes::parse_default_route(&out))
    }

    /// DNS: снимок исходных значений делается один раз, повторные старты
    /// после аварии читают его из состояния и не затирают собой.
    fn apply_dns(&mut self) {
        if !self.cfg.dns_apply || self.dns_applied {
            return;
        }
        let ours = self.cfg.dns_server.to_string();
        let snapshot = match self.state.load_dns() {
            Some(saved) => {
                tracing::info!("исходный DNS взят из сохранённого снимка");
                saved
            }
            None => match snapshot_dns() {
                Ok(mut snap) => {
                    // Сервис, чей DNS до старта уже стоял в туннель, не даёт
                    // исходного значения: в снимок идёт «от DHCP», и откат
                    // вернёт его, а не мёртвый адрес туннеля.
                    if dnsconf::strip_stale_tunnel_dns(&mut snap, &ours) {
                        tracing::warn!(
                            "у некоторых сервисов DNS уже указывал в туннель до старта: \
                             исходные значения неизвестны, восстановление вернёт DHCP"
                        );
                    }
                    if let Err(e) = self.state.save_dns(&snap) {
                        self.fail(&format!("снимок DNS не сохранён: {e}"));
                    }
                    snap
                }
                Err(e) => {
                    self.fail(&format!("снимок DNS: {e}"));
                    return;
                }
            },
        };
        for svc in &snapshot {
            let cmd = dnsconf::set_dns_cmd(&svc.service, &[ours.clone()]);
            if let Err(e) = cmd.run_checked() {
                self.fail(&format!("DNS сервиса {}: {e}", svc.service));
            }
        }
        self.dns_applied = true;
    }

    /// Разрешить адреса серверов: свежий опрос системного резолвера, при
    /// отказе кэш прошлого прогона. Вызывается до взвода kill-switch и
    /// повторяется сторожем, пока адрес не станет известен.
    pub fn resolve_servers(&mut self) {
        if !self.servers.is_empty() {
            return;
        }
        match profile::resolve_endpoints(&self.endpoints) {
            Ok(ips) if !ips.is_empty() => {
                tracing::info!("серверы: {}", summarize_ips(&ips));
                self.servers = ips;
                let _ = self.state.save_servers(&self.servers);
            }
            _ => {
                if let Some(cached) = self.state.load_servers() {
                    if !cached.is_empty() {
                        tracing::warn!("резолвер недоступен, беру кэш серверов");
                        self.servers = cached;
                    }
                }
            }
        }
    }

    /// Запуск движка. Ошибка соединения не снимает правки сети: движок
    /// перезапустится на следующем тике сторожа, прямой выход закрыт всё
    /// это время.
    pub fn start_engine(&mut self) {
        if self.tun.is_none() || self.engine.is_some() {
            return;
        }
        let setup = match profile::build_engine_setup(&self.profile_json, &self.dns_resolvers) {
            Ok(s) => s,
            Err(e) => {
                self.fail(&format!("конфиг движка: {e}"));
                return;
            }
        };
        let mut engine = VpnEngine::new(setup.config);
        let protect = self.make_protect();
        match engine.start(self.queue.clone(), protect) {
            Ok(()) => {
                self.engine = Some(Arc::new(Mutex::new(engine)));
                tracing::info!("движок запущен");
            }
            Err(e) => self.fail(&format!("движок не поднялся: {e}")),
        }
    }

    fn make_protect(&self) -> ProtectSocketFn {
        let phys = self.phys.clone();
        Arc::new(move |fd: i32| {
            let name = phys.lock().unwrap().clone();
            if name.is_empty() {
                return true;
            }
            let idx = unsafe { libc::if_nametoindex(name.as_ptr() as *const libc::c_char) };
            if idx == 0 {
                return true;
            }
            let value = idx as libc::c_int;
            // Сокеты движка прилипают к физическому интерфейсу: без этого
            // собственные маршруты /1 завели бы туннель в самого себя. Уровень
            // неродного семейства молча отказывает, ошибка игнорируется.
            unsafe {
                let ptr = &value as *const libc::c_int as *const libc::c_void;
                libc::setsockopt(
                    fd,
                    libc::IPPROTO_IP,
                    IP_BOUND_IF,
                    ptr,
                    std::mem::size_of::<libc::c_int>() as u32,
                );
                libc::setsockopt(
                    fd,
                    libc::IPPROTO_IPV6,
                    IPV6_BOUND_IF,
                    ptr,
                    std::mem::size_of::<libc::c_int>() as u32,
                );
            }
            true
        })
    }

    fn restart_engine_if_failed(&mut self) {
        let Some(engine) = self.engine.clone() else { return };
        let state = engine.lock().unwrap().state().get();
        match state {
            VpnState::Error(msg) => {
                self.fail(&msg);
                engine.lock().unwrap().stop();
                self.engine = None;
            }
            VpnState::Disconnected => {
                engine.lock().unwrap().stop();
                self.engine = None;
            }
            _ => {}
        }
    }

    /// Одна итерация сторожа. Правила PF переустанавливаются целиком:
    /// внешний перегруз PF сносит динамические анкоры. Сеть сверяется на
    /// смену шлюза, движок перезапускается после отказа. Пропажа utun
    /// возвращает управление без отката сети: kill-switch остаётся
    /// взведённым и закрывает прямой выход на окно перезапуска, полный
    /// повторный подъём делает свежий процесс launchd.
    pub fn tick(&mut self) -> TickOutcome {
        self.resolve_servers();
        let Some(tun) = self.tun.as_ref() else {
            return TickOutcome::TunGone;
        };
        let ifname = tun.ifname.clone();
        if !self.tun_still_alive() {
            // Отката сети нет: анкор держит прямой выход закрытым, пока
            // launchd ждёт ThrottleInterval и поднимает свежий процесс.
            // Маршруты на сгинувший интерфейс ядро снимает само, снимки
            // состояния переживут аварию, а откат при неудачном подъёме
            // выполнит сам свежий процесс.
            self.fail(&format!("интерфейс {ifname} исчез"));
            self.write_status();
            return TickOutcome::TunGone;
        }
        let before = self.phys.lock().unwrap().clone();
        self.arm_killswitch();
        self.apply_routes();
        let after = self.phys.lock().unwrap().clone();
        if before != after {
            tracing::info!("сеть сменилась: {} -> {}", before, after);
            if let Some(engine) = self.engine.clone() {
                engine.lock().unwrap().on_network_changed();
            }
        }
        self.restart_engine_if_failed();
        self.start_engine();
        self.write_status();
        TickOutcome::Alive
    }

    fn tun_still_alive(&self) -> bool {
        let Some(tun) = self.tun.as_ref() else { return false };
        let list = run_capture(&exec::Cmd::new(exec::IFCONFIG, &["-l"])).unwrap_or_default();
        list.split_whitespace().any(|i| i == tun.ifname)
    }

    pub fn write_status(&self) {
        let mut status = StatusFile {
            state: "Stopped".to_string(),
            tun: self
                .tun
                .as_ref()
                .map(|t| t.ifname.clone())
                .unwrap_or_default(),
            gateway: self.phys.lock().unwrap().clone(),
            dns_applied: self.dns_applied,
            killswitch_armed: self.pf_loaded,
            servers: self.servers.iter().map(|(ip, _)| ip.to_string()).collect(),
            last_error: self.last_error.clone(),
            ..Default::default()
        };
        if let Some(engine) = self.engine.clone() {
            let engine = engine.lock().unwrap();
            status.state = engine.state().get().to_string();
            let snap = engine.stats().snapshot();
            status.uptime_seconds = snap.uptime_seconds;
            status.bytes_up = snap.bytes_up;
            status.bytes_down = snap.bytes_down;
            status.active_connections = snap.active_connections;
            status.quic_dropped = snap.quic_dropped;
        }
        if let Err(e) = self.state.save_status(&status) {
            tracing::warn!("статус не записан: {e}");
        }
    }

    fn fail(&mut self, msg: &str) {
        if self.last_error != msg {
            tracing::error!("{}", msg);
        }
        self.last_error = msg.to_string();
    }

    /// Откат правок сети: маршруты, DNS, kill-switch. Порядок держит окно
    /// прямой утечки закрытым до последнего шага.
    pub fn teardown_network(&mut self) {
        let specs = self
            .state
            .load_routes()
            .unwrap_or_else(|| self.routes_applied.clone());
        for spec in &specs {
            run_quiet(&spec.del_cmd());
        }
        self.state.clear_routes();
        self.routes_applied.clear();

        if let Some(snapshot) = self.state.load_dns() {
            for svc in &snapshot {
                let cmd = dnsconf::set_dns_cmd(&svc.service, &svc.servers);
                if let Err(e) = cmd.run_checked() {
                    tracing::error!("возврат DNS {}: {e}", svc.service);
                }
            }
            self.state.clear_dns();
        } else if self.dns_applied {
            tracing::warn!("снимок DNS не найден, сервисы остаются на DNS туннеля");
        }
        self.dns_applied = false;

        run_quiet(&pf::flush_cmd());
        self.pf_loaded = false;
        if let Some(reference) = self
            .pf_ref
            .clone()
            .or_else(|| self.state.load_pf_ref())
        {
            run_quiet(&pf::release_cmd(&reference));
            self.state.clear_pf_ref();
        }
        self.pf_ref = None;

        if let Some(engine) = self.engine.clone() {
            engine.lock().unwrap().stop();
        }
        self.engine = None;
        self.tun = None;
        self.write_status();
    }

}

pub enum TickOutcome {
    Alive,
    TunGone,
}

/// Откат правок сети на выходе демона: только штатная остановка. Потеря
/// туннеля уходит с взведённым kill-switch, иначе на окно ThrottleInterval
/// launchd и подъём свежего процесса прямой выход был бы открыт. Откат на
/// пути потери сделает сам свежий процесс, если его сеть не поднимется,
/// либо команда recover.
pub fn rollback_on_exit(exit_code: i32) -> bool {
    exit_code == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tunnel_loss_exits_with_armed_killswitch() {
        // Штатная остановка откатывает правки сети.
        assert!(rollback_on_exit(0));
        // Потеря туннеля уходит без отката: окно перезапуска launchd
        // закрыто взведённым kill-switch.
        assert!(!rollback_on_exit(1));
    }
}

/// Петля пакетов: отдельная от `Daemon`, чтобы сторож мог править сеть,
/// пока петля держит заимствованные ресурсы.
pub async fn packet_loops(
    tun: Arc<Utun>,
    queue: PacketQueue,
    mut shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    let (dead_tx, mut dead_rx) = watch::channel(false);

    let tun_read = tun.clone();
    let queue_read = queue.clone();
    tokio::task::spawn_blocking(move || {
        let mut buf = vec![0u8; 65536];
        loop {
            match tun_read.read_frame(&mut buf) {
                Ok(Some((packet, _))) => queue_read.push_inbound(packet),
                Ok(None) => {}
                Err(e) => {
                    tracing::error!("чтение utun: {e}");
                    let _ = dead_tx.send(true);
                    return;
                }
            }
        }
    });

    let notifier = queue.notifier();
    loop {
        tokio::select! {
            _ = notifier.notified() => {}
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
            _ = shutdown.changed() => return Ok(()),
            _ = dead_rx.changed() => {
                if *dead_rx.borrow() {
                    return Err(std::io::Error::other("интерфейс туннеля закрылся"));
                }
            }
        }
        while let Some(packet) = queue.pop_outbound() {
            let family = if packet.first().map(|b| b >> 4) == Some(6) {
                crate::frame::AF_INET6_DARWIN
            } else {
                crate::frame::AF_INET_DARWIN
            };
            if let Err(e) = tun.write_packet(&packet, family) {
                tracing::warn!("запись utun: {e}");
                break;
            }
        }
    }
}

/// Снять DNS всех включённых сервисов системы. Строгая версия для демона:
/// пустой список сервисов это отказ, тихо продолжать нельзя.
pub fn snapshot_dns() -> Result<Vec<ServiceDns>, String> {
    let list = run_capture(&exec::Cmd::new(
        exec::NETWORKSETUP,
        &["-listallnetworkservices"],
    ))
    .ok_or("networksetup -listallnetworkservices не ответил")?;
    let mut out = Vec::new();
    for service in dnsconf::parse_services(&list) {
        let dns = run_capture(&dnsconf::get_dns_cmd(&service)).unwrap_or_default();
        out.push(ServiceDns {
            service,
            servers: dnsconf::parse_dns(&dns),
        });
    }
    if out.is_empty() {
        return Err("сетевые сервисы не найдены".into());
    }
    Ok(out)
}

fn apply_route(spec: &RouteSpec) -> bool {
    if run_quiet(&spec.add_cmd()) {
        return true;
    }
    // Запись уже стоит: снимаем и ставим заново, чтобы не расходиться
    // со старым прогоном после аварии.
    run_quiet(&spec.del_cmd()) && run_quiet(&spec.add_cmd())
}

fn summarize_ips(ips: &[(IpAddr, u16)]) -> String {
    ips.iter()
        .map(|(ip, port)| format!("{}:{}", ip, port))
        .collect::<Vec<_>>()
        .join(", ")
}
