//! Харнесс совместного редактирования: `xr-share sync` (LLD-33 п. 2.5).
//!
//! Долгоживущий цикл на стороне соавтора: рабочая папка это обычный клон шары
//! с `.git`, локальные правки сами уезжают наверх, встречные сами приезжают.
//! Внутри libgit2 (`git2` с vendored-сборкой), поэтому git в `PATH` соавтору
//! не нужен: свой репозиторий он не обслуживает руками, как владелец шары не
//! обслуживает агентский.
//!
//! Симметрия с агентским контуром (`gitrepo.rs`) намеренная: тот же watcher с
//! дебаунсом и страховочным сканом, тот же колпак размера и тот же служебный
//! неймспейс `.xr-*` вне истории. Отличия два. Первое: merge живёт здесь, а не
//! у агента (LLD-33 п. 3.3, мирится опоздавший), и пересечение по строкам
//! кладётся конфликт-копией рядом, а не маркерами в файл (п. 3.6). Второе:
//! транспорт перебирает адреса как `pull`/`push` (LAN раньше публичного), а
//! шару за NAT берёт через relay-мост: локальный сокет на loopback, за ним
//! identity-TLS с пиннингом на ключ агента, и libgit2 о relay не знает.
//!
//! Авторитет данных на прямом plain-HTTP пути даёт подписанный HEAD (п. 2.4):
//! кандидат адреса принимается только с подписью, сходящейся с `agent_pubkey`
//! из гранта, и после каждого fetch сверяется, что приехавший `main` это он.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use clap::Args;
use serde::Deserialize;
use xr_proto::share::{parse_agent_pubkey, verify_git_head, GIT_MAX_FILE_MB};

use crate::gitrepo::{author_pair, commit_subject, scan_oversize_paths, write_managed_exclude};
use crate::pull::{InviteShareDto, HUB_DEFAULT};
use crate::push::{ensure_writable_grant, select_share};

/// Дебаунс локальных правок: столько тишины после последнего события файловой
/// системы, и папка коммитится. То же окно, что у агента.
const LOCAL_DEBOUNCE: Duration = Duration::from_secs(2);
/// Страховочный проход: watcher ненадёжен на сетевых ФС (LLD-33 риск 5), раз в
/// это время папка пересматривается целиком.
const SAFETY_SCAN_EVERY: Duration = Duration::from_secs(5 * 60);
/// Сколько секунд висит один long-poll запроса HEAD. Потолок ручки минута,
/// берём заметно меньше: обрыв на середине стоит одного лишнего запроса.
const LONGPOLL_WAIT: u64 = 45;
/// Бэкофы сетевых сбоев (агент офлайн, relay недоступен, отказ гейта).
const BACKOFF_MIN: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// Сколько подряд неудачных проходов терпит выбранный адрес, прежде чем
/// транспорт выбирается заново (агент переехал, LAN сменилась, relay поднялся).
const RECHOOSE_AFTER_FAILURES: u32 = 5;

#[derive(Args)]
pub struct SyncArgs {
    /// Invite token granting access (the access anchor, LLD-19 п. 9.5).
    #[arg(long)]
    pub invite: String,
    /// Hub base URL (default https://xr-hub.zoobr.top).
    #[arg(long)]
    pub hub: Option<String>,
    /// Which share to sync, by its share_id or name.
    #[arg(long)]
    pub share: String,
    /// Local folder: an existing clone of this share, an empty folder, or a
    /// path that does not exist yet (then it is cloned).
    pub dir: String,
    /// Author name for the commits this harness makes (default: hostname).
    #[arg(long)]
    pub name: Option<String>,
    /// One pass instead of the loop: commit, fetch, merge, push, exit.
    #[arg(long)]
    pub once: bool,
    /// Reach the agent over https (default http; the distributed agent serves
    /// HTTP and the relay path is TLS of its own).
    #[arg(long)]
    pub https: bool,
}

/// Ответ ручки `/{share_id}/git/head` (LLD-33 п. 2.4).
#[derive(Deserialize)]
struct HeadResp {
    head: String,
    signed_at: u64,
    #[serde(default)]
    sig: Option<String>,
}

/// Подписанный HEAD агента, проверенный по `agent_pubkey` из гранта. Грант без
/// ключа (хаб до XR-046) принимается без подписи, как и на манифестном пути.
fn fetch_head(base: &str, token: &str, agent_pubkey: &str, share_id: &str, wait: u64, since: &str) -> Result<String> {
    let url = if wait > 0 {
        format!("{base}/git/head?wait={wait}&since={since}")
    } else {
        format!("{base}/git/head")
    };
    // Дедлайн чтения с запасом над самим ожиданием: висящий запрос это штатное
    // поведение ручки, а не зависший агент.
    // Короткий дедлайн соединения отделяет мёртвого кандидата от живого, но
    // медленного: неотвечающий LAN-адрес обязан отвалиться за секунды, а не
    // выжечь весь бюджет чтения (XR-050).
    let agent = ureq::builder().timeout_connect(Duration::from_secs(6)).build();
    let resp = agent
        .get(&url)
        .set("Authorization", &format!("Bearer {token}"))
        .timeout(Duration::from_secs(wait + 30))
        .call();
    let body = match resp {
        Ok(r) => r.into_string().context("чтение ответа ручки HEAD")?,
        Err(ureq::Error::Status(code, r)) => {
            bail!("HTTP {code}: {}", r.into_string().unwrap_or_default())
        }
        Err(e) => bail!("сеть: {e}"),
    };
    let parsed: HeadResp = serde_json::from_str(&body).context("разбор ответа ручки HEAD")?;
    if !agent_pubkey.is_empty() {
        let key = parse_agent_pubkey(agent_pubkey)
            .map_err(|e| anyhow!("agent_pubkey из гранта: {e}"))?;
        let sig = parsed
            .sig
            .as_deref()
            .ok_or_else(|| anyhow!("агент не подписал HEAD: обнови xr-share на стороне агента"))?;
        verify_git_head(sig, &key, share_id, parsed.signed_at, &parsed.head).map_err(|e| {
            anyhow!("подпись HEAD не сошлась ({e}): по этому адресу отвечает не тот агент")
        })?;
    }
    Ok(parsed.head)
}

/// Выбранный путь до агента: база вида `http://<адрес>:<порт>/{share_id}`, под
/// которой лежат и git-роуты, и ручка HEAD. Relay-путь держит за собой мост:
/// пока `Transport` жив, жив и loopback-сокет, на который смотрит libgit2.
struct Transport {
    base: String,
    /// Мост relay и его рантайм. Прямой путь их не заводит вовсе, поэтому
    /// обычный синк соавтора в LAN не платит за tokio ни потоком.
    relay: Option<RelayLeg>,
}

impl Transport {
    fn via_relay(&self) -> bool {
        self.relay.is_some()
    }
}

/// Живой relay-мост вместе с рантаймом, который его крутит. Порядок полей это
/// порядок дроппинга: мост снимается раньше рантайма, иначе его задача
/// оборвётся вместе с пулом и обрыв уехал бы в лог как ошибка.
struct RelayLeg {
    _bridge: RelayBridge,
    _rt: tokio::runtime::Runtime,
}

/// Выбрать путь до агента (LLD-33 п. 2.5): прямые адреса в порядке гранта (LAN
/// раньше публичного, XR-050), relay последним. Кандидат принимается только
/// если ручка HEAD ответила подписью, сходящейся с `agent_pubkey`: это и
/// проверка достижимости, и anti-wrong-host (п. 2.4).
fn choose_transport(share: &InviteShareDto, https: bool) -> Result<Transport> {
    let scheme = if https { "https" } else { "http" };
    let mut last_err: Option<anyhow::Error> = None;
    for base in share.candidate_bases(scheme) {
        match fetch_head(&base, &share.token, &share.agent_pubkey, &share.share_id, 0, "") {
            Ok(_) => {
                println!("шара «{}»: прямой путь {base}", share.name);
                return Ok(Transport { base, relay: None });
            }
            Err(e) => last_err = Some(e),
        }
    }
    if let Some(relay) = &share.relay {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .context("рантайм relay-моста")?;
        let bridge = rt
            .block_on(RelayBridge::spawn(relay))
            .context("relay-мост не поднялся")?;
        let base = format!("http://{}/{}", bridge.local_addr(), share.share_id);
        match fetch_head(&base, &share.token, &share.agent_pubkey, &share.share_id, 0, "") {
            Ok(_) => {
                println!("шара «{}»: путь через relay ({})", share.name, bridge.local_addr());
                return Ok(Transport {
                    base,
                    relay: Some(RelayLeg { _bridge: bridge, _rt: rt }),
                });
            }
            Err(e) => last_err = Some(e.context("relay")),
        }
    }
    Err(last_err.unwrap_or_else(|| {
        anyhow!("у шары «{}» нет ни одного адреса в гранте", share.name)
    }))
}

/// Мост relay для libgit2 (LLD-33 п. 2.5): сокет на loopback, за которым
/// identity-TLS-стрим через relay до агента. libgit2 говорит с ним обычным
/// plain HTTP и о relay не знает, а пиннинг живёт в одном месте, в
/// `relay_tls` (п. 3.2), а не дублируется в C-библиотеке.
///
/// Зеркало `LoopbackForwarder` из `xr-proto`, но с обратным распределением
/// ролей: там TLS терминирует сам потребитель, а мост слепо носит шифртекст;
/// здесь TLS терминируем мы, потому что потребитель это libgit2 без
/// TLS-бэкендов вовсе.
struct RelayBridge {
    local_addr: std::net::SocketAddr,
    handle: tokio::task::JoinHandle<()>,
}

impl RelayBridge {
    async fn spawn(grant: &xr_proto::share::RelayGrant) -> Result<Self> {
        use std::net::Ipv4Addr;
        let endpoint = Arc::new(
            xr_proto::relay_client::RelayEndpoint::from_grant(grant)
                .map_err(|e| anyhow!("relay-грант: {e}"))?,
        );
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .context("сокет relay-моста на loopback")?;
        let local_addr = listener.local_addr().context("адрес relay-моста")?;
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { break };
                let endpoint = endpoint.clone();
                tokio::spawn(async move {
                    match xr_proto::relay_client::relay_tls_connect(&endpoint).await {
                        Ok(mut tls) => {
                            if let Err(e) =
                                tokio::io::copy_bidirectional(&mut sock, &mut tls).await
                            {
                                tracing::debug!("relay-мост: соединение кончилось: {e}");
                            }
                        }
                        Err(e) => tracing::warn!("relay-мост: транзит не открылся: {e}"),
                    }
                });
            }
        });
        Ok(Self { local_addr, handle })
    }

    fn local_addr(&self) -> std::net::SocketAddr {
        self.local_addr
    }
}

impl Drop for RelayBridge {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Живая сессия синка: клон в рабочей папке, текущая база транспорта и грант.
/// Токен гранта живёт неделю, а цикл дольше, поэтому он перезапрашивается по
/// инвайту и лежит под локом: его читает и long-poll в своём потоке.
struct Session {
    repo: git2::Repository,
    worktree: PathBuf,
    share: InviteShareDto,
    token: Arc<Mutex<String>>,
    hub: String,
    invite: String,
    author: (String, String),
    transport: Transport,
}

impl Session {
    fn token(&self) -> String {
        self.token.lock().expect("token lock poisoned").clone()
    }

    /// Открыть рабочую папку: существующий клон этой шары переиспользуется,
    /// пустая или отсутствующая папка клонируется. Непустая посторонняя папка
    /// это отказ: молча смешивать локальные файлы с чужой историей нельзя
    /// (LLD-33 п. 2.5).
    fn open(dir: &Path, share: &InviteShareDto, transport: Transport, args: &SyncArgs, hub: String) -> Result<Self> {
        let token = Arc::new(Mutex::new(share.token.clone()));
        let url = format!("{}/git", transport.base);
        let repo = if dir.join(".git").exists() {
            let repo = git2::Repository::open(dir)
                .with_context(|| format!("открытие клона в {}", dir.display()))?;
            let origin = repo
                .find_remote("origin")
                .context("у клона нет origin: это не клон шары")?;
            let origin_url = origin.url().unwrap_or_default().to_string();
            // Адрес меняется от сессии к сессии (LAN, relay-мост на случайном
            // порту), а вот шара обязана быть той же: иначе синк начал бы
            // мешать в одну папку истории двух разных шар.
            if !origin_url.contains(&format!("/{}/git", share.share_id)) {
                bail!(
                    "папка {} это клон другой шары ({origin_url}), синк отказывается мешать истории",
                    dir.display()
                );
            }
            drop(origin);
            repo.remote_set_url("origin", &url).context("обновление адреса origin")?;
            repo
        } else {
            if dir.exists() && dir.read_dir().map(|mut d| d.next().is_some()).unwrap_or(false) {
                bail!(
                    "папка {} не пуста и не является клоном шары: выбери пустую папку \
                     либо синкай в неё то, что уже клонировано",
                    dir.display()
                );
            }
            clone(&url, dir, &token.lock().expect("token lock poisoned").clone())?
        };
        // Свежий клон пустовавшей шары приезжает с нерождённым HEAD, и указывать
        // он может на ветку по умолчанию сборки libgit2. Ветка контура одна,
        // `main`, поэтому HEAD ставится на неё явно.
        if repo.head().is_err() {
            repo.set_head("refs/heads/main").context("ветка main в свежем клоне")?;
        }
        let worktree = repo
            .workdir()
            .context("клон без рабочей папки (bare): синку он не годится")?
            .to_path_buf();
        ensure_exclude(repo.path())?;
        Ok(Self {
            repo,
            worktree,
            share: share.clone(),
            token,
            hub,
            invite: args.invite.clone(),
            author: author_pair(args.name.as_deref()),
            transport,
        })
    }

    /// Заголовок авторизации для libgit2. Токен едет заголовком, как на
    /// манифестном пути (`custom_headers` у fetch и push), а не в URL: в URL он
    /// осел бы в `.git/config` рабочей папки соавтора.
    fn auth_header(&self) -> String {
        format!("Authorization: Bearer {}", self.token())
    }

    /// Один проход синка: локальные правки в коммит, встречные из туннеля в
    /// merge, свой результат наверх. Отвергнутый push это не ошибка, а сигнал
    /// повторить: пока повторов не больше `PUSH_RETRIES`, разбор идёт здесь же.
    fn pass(&mut self) -> Result<()> {
        if let Some(oid) = self.commit_local()? {
            println!("локальная правка: коммит {}", short(&oid.to_string()));
        }
        for attempt in 1..=PUSH_RETRIES {
            self.fetch()?;
            self.merge_remote()?;
            match self.push() {
                Ok(PushOutcome::Nothing) | Ok(PushOutcome::Pushed) => return Ok(()),
                // Кто-то успел раньше (п. 3.3) либо у владельца грязная папка
                // (п. 3.4): и то и другое лечится повтором цикла.
                Ok(PushOutcome::Retry(reason)) => {
                    if attempt == PUSH_RETRIES {
                        bail!("push отвергнут {PUSH_RETRIES} раз подряд: {reason}");
                    }
                    println!("push отвергнут ({reason}), повторяю с fetch");
                    std::thread::sleep(BACKOFF_MIN);
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

/// Сколько раз проход повторяет цепочку fetch-merge-push, прежде чем признать
/// отказ ошибкой. Гонка сходится за конечное число повторов (LLD-33 п. 3.3),
/// потолок отделяет её от отказа, который повтором не лечится.
const PUSH_RETRIES: u32 = 5;

/// Клон шары в пустую папку. Транспорт plain HTTP, авторитет у подписи HEAD,
/// уже проверенной при выборе адреса.
fn clone(url: &str, dir: &Path, token: &str) -> Result<git2::Repository> {
    let header = format!("Authorization: Bearer {token}");
    let mut fo = git2::FetchOptions::new();
    fo.custom_headers(&[&header]);
    fo.remote_callbacks(auth_callbacks(token));
    let mut builder = git2::build::RepoBuilder::new();
    builder.fetch_options(fo);
    match builder.clone(url, dir) {
        Ok(repo) => Ok(repo),
        // Пустовавшая шара: истории ещё нет, и клонировать нечего. Это не
        // ошибка, а первый соавтор: заводим репозиторий сами и вешаем на него
        // тот же origin, дальше первая же правка создаст `main` push'ем.
        Err(e) if is_empty_remote(&e) => {
            let repo = git2::Repository::init(dir)
                .map_err(|e| anyhow!("создание клона в {}: {}", dir.display(), git_err(&e)))?;
            repo.remote("origin", url)
                .map_err(|e| anyhow!("origin у клона: {}", git_err(&e)))?;
            Ok(repo)
        }
        Err(e) => Err(anyhow!("клон шары в {}: {}", dir.display(), git_err(&e))),
    }
}

/// Клон не удался потому, что на той стороне пустой репозиторий: веток нет, и
/// libgit2 не нашёл, на что ставить HEAD.
fn is_empty_remote(e: &git2::Error) -> bool {
    let m = e.message().to_ascii_lowercase();
    m.contains("not found") && m.contains("refs/remotes/origin/")
        || m.contains("remote head") && m.contains("not found")
        || m.contains("empty")
}

/// Креды для libgit2 на случай, когда агент всё же спросил basic-авторизацию
/// (заголовок потерялся на промежуточном узле): тот же токен-блоб паролем.
fn auth_callbacks(token: &str) -> git2::RemoteCallbacks<'static> {
    let token = token.to_string();
    let mut cb = git2::RemoteCallbacks::new();
    cb.credentials(move |_url, _user, _allowed| {
        git2::Cred::userpass_plaintext("xr-share", &token)
    });
    cb
}

/// Текст ошибки libgit2 без обёртки: у git2 в Display уже лежит сообщение
/// сервера, но без класса оно читается лучше.
fn git_err(e: &git2::Error) -> String {
    e.message().trim().to_string()
}

/// Первые семь символов SHA, как их печатает git.
fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// Служебный неймспейс вне истории (LLD-29): недозаливки контура записи и
/// каталоги импорт-джоб. Строка ставится в `info/exclude` клона один раз, как
/// её ставит агент своему репозиторию.
fn ensure_exclude(git_dir: &Path) -> Result<()> {
    let info = git_dir.join("info");
    std::fs::create_dir_all(&info).with_context(|| format!("создание {}", info.display()))?;
    let exclude = info.join("exclude");
    let current = std::fs::read_to_string(&exclude).unwrap_or_default();
    if !current.lines().any(|l| l.trim() == ".xr-*") {
        let mut text = current;
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(".xr-*\n");
        std::fs::write(&exclude, text)
            .with_context(|| format!("запись {}", exclude.display()))?;
    }
    Ok(())
}

/// Путь в нотации git: разделители `/` и относительность корня.
fn git_path(rel: &Path) -> String {
    rel.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/")
}

/// Что не едет в историю: свой `.git`, служебный неймспейс `.xr-*` и файлы
/// крупнее колпака (LLD-33 п. 2.6). Колпак у харнесса это общая константа
/// `xr-proto`, тот же дефолт, что у агента без настройки.
fn skipped(rel: &Path, oversize: &[String]) -> bool {
    let path = git_path(rel);
    let mut parts = path.split('/');
    if parts.next() == Some(".git") {
        return true;
    }
    if path.split('/').any(|c| c.starts_with(".xr-")) {
        return true;
    }
    oversize.iter().any(|o| *o == path)
}

impl Session {
    /// Один коммит локальных правок, если они есть. Индекс собирается из
    /// рабочей папки целиком (`update_all` подхватывает удаления, `add_all`
    /// новое и изменённое), сверхколпачные файлы отсеиваются и колбэком, и
    /// ведомым блоком `info/exclude`: первое чтобы не попасть в индекс, второе
    /// чтобы их не показывал штатный `git status` в той же папке.
    fn commit_local(&self) -> Result<Option<git2::Oid>> {
        let oversize = scan_oversize_paths(&self.worktree, GIT_MAX_FILE_MB)?;
        write_managed_exclude(self.repo.path(), &oversize)?;
        let tree_oid = {
            let mut index = self.repo.index().context("индекс клона")?;
            let mut filter = |path: &Path, _matched: &[u8]| -> i32 {
                if skipped(path, &oversize) {
                    1
                } else {
                    0
                }
            };
            index
                .update_all(["*"].iter(), Some(&mut filter))
                .map_err(|e| anyhow!("обновление индекса: {}", git_err(&e)))?;
            index
                .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, Some(&mut filter))
                .map_err(|e| anyhow!("наполнение индекса: {}", git_err(&e)))?;
            index.write().map_err(|e| anyhow!("запись индекса: {}", git_err(&e)))?;
            index.write_tree().map_err(|e| anyhow!("дерево индекса: {}", git_err(&e)))?
        };

        let head = self.head_commit();
        if let Some(parent) = &head {
            if parent.tree_id() == tree_oid {
                return Ok(None);
            }
        }
        let tree = self.repo.find_tree(tree_oid).context("дерево коммита")?;
        let names = self.changed_paths(head.as_ref(), &tree)?;
        if names.is_empty() {
            return Ok(None);
        }
        let sig = self.signature()?;
        let parents: Vec<&git2::Commit> = head.iter().collect();
        let oid = self
            .repo
            .commit(Some("HEAD"), &sig, &sig, &commit_subject(&names), &tree, &parents)
            .map_err(|e| anyhow!("коммит: {}", git_err(&e)))?;
        Ok(Some(oid))
    }

    /// Коммит, на который смотрит HEAD; `None` у нерождённой ветки (пустая
    /// шара, из которой только что сделан клон).
    fn head_commit(&self) -> Option<git2::Commit<'_>> {
        self.repo.head().ok()?.peel_to_commit().ok()
    }

    /// Подпись автора для коммитов этой сессии: имя из `--name`, иначе имя
    /// машины, как у агентского авто-коммита.
    fn signature(&self) -> Result<git2::Signature<'_>> {
        git2::Signature::now(&self.author.0, &self.author.1)
            .map_err(|e| anyhow!("подпись автора: {}", git_err(&e)))
    }

    /// Пути, которыми новое дерево отличается от родительского: из них
    /// собирается subject коммита, тот же, что у агента.
    fn changed_paths(&self, parent: Option<&git2::Commit>, tree: &git2::Tree) -> Result<Vec<String>> {
        let old = match parent {
            Some(c) => Some(c.tree().context("дерево родителя")?),
            None => None,
        };
        let diff = self
            .repo
            .diff_tree_to_tree(old.as_ref(), Some(tree), None)
            .map_err(|e| anyhow!("сравнение деревьев: {}", git_err(&e)))?;
        let mut names = Vec::new();
        for delta in diff.deltas() {
            let path = delta
                .new_file()
                .path()
                .or_else(|| delta.old_file().path())
                .map(git_path);
            if let Some(p) = path {
                if !names.contains(&p) {
                    names.push(p);
                }
            }
        }
        Ok(names)
    }
}

/// Чем кончился push: пусто (нечего отдавать), прошёл, либо отвергнут по
/// причине, которая лечится повтором цикла fetch-merge-push.
enum PushOutcome {
    Nothing,
    Pushed,
    Retry(String),
}

impl Session {
    /// Забрать историю агента в `refs/remotes/origin/*`. Рефспек со звёздочкой
    /// нарочно: у пустовавшей шары ветки `main` ещё нет, и точный рефспек дал
    /// бы отказ «нет такого рефа» вместо пустого fetch, то есть первый push в
    /// пустую шару стал бы невозможен.
    fn fetch(&self) -> Result<()> {
        let header = self.auth_header();
        let mut fo = git2::FetchOptions::new();
        fo.custom_headers(&[&header]);
        fo.remote_callbacks(auth_callbacks(&self.token()));
        fo.prune(git2::FetchPrune::On);
        let mut remote = self.repo.find_remote("origin").context("origin клона")?;
        remote
            .fetch(&["+refs/heads/*:refs/remotes/origin/*"], Some(&mut fo), None)
            .map_err(|e| anyhow!("fetch: {}", git_err(&e)))?;
        // Приехавший `main` обязан быть тем, что агент подписал (LLD-33 п. 2.4):
        // хеш-связность git тянет авторитет с подписанного HEAD на всю историю,
        // но только если сверить сам HEAD.
        if let Some(remote_oid) = self.remote_main()? {
            let signed = fetch_head(
                &self.transport.base,
                &self.token(),
                &self.share.agent_pubkey,
                &self.share.share_id,
                0,
                "",
            )?;
            // Расхождение это штатная гонка: агент успел закоммитить между
            // fetch и этим запросом. Ошибкой считаем только случай, когда
            // приехавший коммит агенту вообще неизвестен.
            if !signed.is_empty()
                && signed != remote_oid.to_string()
                && self.repo.find_commit(remote_oid).is_err()
            {
                bail!("приехавший main {} не сходится с подписанным HEAD {signed}", remote_oid);
            }
        }
        Ok(())
    }

    /// `refs/remotes/origin/main`, если он вообще есть.
    fn remote_main(&self) -> Result<Option<git2::Oid>> {
        match self.repo.find_reference("refs/remotes/origin/main") {
            Ok(r) => Ok(r.target()),
            Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(e) => Err(anyhow!("чтение origin/main: {}", git_err(&e))),
        }
    }

    /// Слить встречную историю в свою (LLD-33 п. 2.5). Fast-forward просто
    /// выставляет рабочую копию; настоящий merge идёт диффом по строкам, и
    /// непересекающиеся правки сливаются сами. Пересечение по строкам не
    /// сливается: файл остаётся локальной версией, встречная кладётся рядом
    /// конфликт-копией, и обе версии доезжают до всех участников (п. 3.6).
    fn merge_remote(&self) -> Result<()> {
        let Some(remote_oid) = self.remote_main()? else { return Ok(()) };
        let annotated = self
            .repo
            .find_annotated_commit(remote_oid)
            .map_err(|e| anyhow!("встречный коммит: {}", git_err(&e)))?;
        let (analysis, _pref) = self
            .repo
            .merge_analysis(&[&annotated])
            .map_err(|e| anyhow!("разбор слияния: {}", git_err(&e)))?;
        if analysis.is_up_to_date() {
            return Ok(());
        }
        if analysis.is_fast_forward() || analysis.is_unborn() {
            self.fast_forward(remote_oid)?;
            println!("встречная правка: {} (fast-forward)", short(&remote_oid.to_string()));
            return Ok(());
        }
        if !analysis.is_normal() {
            bail!("слияние невозможно: git не считает встречную историю сливаемой");
        }
        self.merge_normal(remote_oid, &annotated)
    }

    /// Перевести `main` и рабочую папку на встречный коммит.
    fn fast_forward(&self, target: git2::Oid) -> Result<()> {
        match self.repo.find_reference("refs/heads/main") {
            Ok(mut r) => {
                r.set_target(target, "xr-share sync: fast-forward")
                    .map_err(|e| anyhow!("перевод main: {}", git_err(&e)))?;
            }
            Err(_) => {
                self.repo
                    .reference("refs/heads/main", target, true, "xr-share sync: первый клон")
                    .map_err(|e| anyhow!("создание main: {}", git_err(&e)))?;
            }
        }
        self.repo.set_head("refs/heads/main").context("HEAD на main")?;
        let mut co = git2::build::CheckoutBuilder::new();
        co.force();
        self.repo
            .checkout_head(Some(&mut co))
            .map_err(|e| anyhow!("выкладка рабочей папки: {}", git_err(&e)))?;
        Ok(())
    }

    /// Настоящий merge с разбором конфликтов. Слияние делается в индексе и
    /// рабочей папке репозитория, дальше конфликтные пути переписываются
    /// политикой конфликт-копий, и результат уезжает одним merge-коммитом:
    /// копия входит в тот же коммит, поэтому встречная версия доезжает до
    /// остальных участников обычным push.
    fn merge_normal(&self, remote_oid: git2::Oid, annotated: &git2::AnnotatedCommit) -> Result<()> {
        let mut co = git2::build::CheckoutBuilder::new();
        co.force();
        let mut mo = git2::MergeOptions::new();
        // Стандартный трёхсторонний diff3 по строкам, как у git: соседние
        // строки сливаются сами, пересечение остаётся конфликтом.
        mo.file_favor(git2::FileFavor::Normal);
        self.repo
            .merge(&[annotated], Some(&mut mo), Some(&mut co))
            .map_err(|e| anyhow!("слияние: {}", git_err(&e)))?;

        let remote_commit = self.repo.find_commit(remote_oid).context("встречный коммит")?;
        let their_author = remote_commit
            .author()
            .name()
            .unwrap_or("неизвестный")
            .to_string();
        let their_short = short(&remote_oid.to_string());
        let copies = self.resolve_conflicts(&their_author, &their_short)?;

        let tree_oid = {
            let mut index = self.repo.index().context("индекс слияния")?;
            if index.has_conflicts() {
                bail!("в индексе остались конфликты: слияние прервано, разбери папку руками");
            }
            index.write().map_err(|e| anyhow!("запись индекса: {}", git_err(&e)))?;
            index.write_tree().map_err(|e| anyhow!("дерево слияния: {}", git_err(&e)))?
        };
        let tree = self.repo.find_tree(tree_oid).context("дерево слияния")?;
        let local = self.head_commit().context("слияние без локального HEAD")?;
        let sig = self.signature()?;
        let subject = if copies.is_empty() {
            format!("слияние {their_short}")
        } else {
            format!("слияние {their_short}, конфликт-копии: {}", copies.len())
        };
        let merged = self
            .repo
            .commit(Some("HEAD"), &sig, &sig, &subject, &tree, &[&local, &remote_commit])
            .map_err(|e| anyhow!("merge-коммит: {}", git_err(&e)))?;
        self.repo.cleanup_state().context("снятие состояния слияния")?;
        let mut co = git2::build::CheckoutBuilder::new();
        co.force();
        self.repo
            .checkout_head(Some(&mut co))
            .map_err(|e| anyhow!("выкладка после слияния: {}", git_err(&e)))?;
        for copy in &copies {
            println!("КОНФЛИКТ: обе стороны правили одни строки, встречная версия рядом: {copy}");
        }
        println!("слияние {their_short} -> {}", short(&merged.to_string()));
        Ok(())
    }
}

/// Имя конфликт-копии: `<имя> (конфликт <автор> <sha7>)<расширение>` (LLD-33
/// п. 2.5). Имя несёт автора и короткий SHA встречного коммита, поэтому
/// повторный разбор той же развилки даёт то же имя и дубли не плодятся.
fn conflict_copy_name(path: &str, author: &str, sha7: &str) -> String {
    let (dir, file) = match path.rsplit_once('/') {
        Some((d, f)) => (Some(d), f),
        None => (None, path),
    };
    // Расширение отделяется по последней точке, но только если она не первый
    // символ: у файла вида `.editorconfig` расширения нет, есть имя.
    let (stem, ext) = match file.rfind('.') {
        Some(i) if i > 0 => (&file[..i], &file[i..]),
        _ => (file, ""),
    };
    let author = sanitize_component(author);
    let name = format!("{stem} (конфликт {author} {sha7}){ext}");
    match dir {
        Some(d) => format!("{d}/{name}"),
        None => name,
    }
}

/// Имя автора внутри имени файла: разделители путей и управляющие символы
/// заменяются, иначе автор с `/` в имени уводил бы копию в другой каталог.
fn sanitize_component(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c == ':' || c.is_control() {
                '-'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() {
        "неизвестный".to_string()
    } else {
        cleaned
    }
}

impl Session {
    /// Политика конфликтов (LLD-33 п. 3.6): для каждого конфликтного пути файл
    /// остаётся локальной версией, а встречная кладётся рядом целым файлом.
    /// Маркеров `<<<<<<<` в папке не появляется вовсе: в самосинкающейся
    /// системе они разъехались бы по участникам как обычный контент.
    ///
    /// Асимметричные случаи разбираются так же честно. Файл, удалённый у нас и
    /// правленный встречной стороной, оставляет удаление и кладёт копию: правка
    /// не теряется. Файл, удалённый встречной стороной и правленный нами,
    /// остаётся нашей версией: удаление не молчаливо отменяет чужую работу,
    /// а становится видимым файлом в папке у всех.
    fn resolve_conflicts(&self, their_author: &str, their_short: &str) -> Result<Vec<String>> {
        let mut index = self.repo.index().context("индекс слияния")?;
        if !index.has_conflicts() {
            return Ok(Vec::new());
        }
        let conflicts: Vec<git2::IndexConflict> = index
            .conflicts()
            .map_err(|e| anyhow!("список конфликтов: {}", git_err(&e)))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| anyhow!("разбор конфликтов: {}", git_err(&e)))?;

        let mut copies = Vec::new();
        for c in conflicts {
            let path = c
                .our
                .as_ref()
                .or(c.their.as_ref())
                .or(c.ancestor.as_ref())
                .map(|e| String::from_utf8_lossy(&e.path).into_owned())
                .context("конфликт без пути")?;
            let abs = self.worktree.join(&path);

            // Наша версия ложится в сам файл, встречная в копию рядом. Порядок
            // важен: merge оставил в файле маркеры, и до перезаписи его нельзя
            // ни добавлять в индекс, ни считать разрешённым.
            match (&c.our, &c.their) {
                (Some(our), their) => {
                    self.write_blob(&abs, our.id)?;
                    if let Some(their) = their {
                        let copy = conflict_copy_name(&path, their_author, their_short);
                        self.write_blob(&self.worktree.join(&copy), their.id)?;
                        copies.push(copy);
                    }
                }
                (None, Some(their)) => {
                    // Удалено у нас: удаление остаётся, встречная правка едет
                    // копией, чтобы её было видно.
                    let copy = conflict_copy_name(&path, their_author, their_short);
                    self.write_blob(&self.worktree.join(&copy), their.id)?;
                    copies.push(copy);
                    if abs.exists() {
                        std::fs::remove_file(&abs)
                            .with_context(|| format!("удаление {}", abs.display()))?;
                    }
                }
                (None, None) => {}
            }

            index
                .remove_path(Path::new(&path))
                .map_err(|e| anyhow!("снятие конфликта {path}: {}", git_err(&e)))?;
            if abs.exists() {
                index
                    .add_path(Path::new(&path))
                    .map_err(|e| anyhow!("индексация {path}: {}", git_err(&e)))?;
            }
        }
        for copy in &copies {
            index
                .add_path(Path::new(copy))
                .map_err(|e| anyhow!("индексация {copy}: {}", git_err(&e)))?;
        }
        index.write().map_err(|e| anyhow!("запись индекса: {}", git_err(&e)))?;
        Ok(copies)
    }

    /// Записать содержимое блоба в файл рабочей папки, создав каталоги.
    fn write_blob(&self, abs: &Path, id: git2::Oid) -> Result<()> {
        let blob = self.repo.find_blob(id).map_err(|e| anyhow!("блоб: {}", git_err(&e)))?;
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("создание {}", parent.display()))?;
        }
        std::fs::write(abs, blob.content())
            .with_context(|| format!("запись {}", abs.display()))
    }

    /// Отдать свою `main` агенту. Отказ гейта по протухшему токену лечится
    /// перезапросом гранта по инвайту, отказ ветки (успели раньше, грязная
    /// папка владельца) уезжает наверх повтором.
    fn push(&mut self) -> Result<PushOutcome> {
        let Some(local) = self.head_commit().map(|c| c.id()) else {
            return Ok(PushOutcome::Nothing);
        };
        if self.remote_main()? == Some(local) {
            return Ok(PushOutcome::Nothing);
        }
        let rejected: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let header = self.auth_header();
        let mut po = git2::PushOptions::new();
        po.custom_headers(&[&header]);
        let mut cb = auth_callbacks(&self.token());
        let sink = rejected.clone();
        // Отказ ветки приезжает не ошибкой push, а этим колбэком: без него
        // отвергнутый non-fast-forward выглядел бы как успешный проход.
        cb.push_update_reference(move |refname, status| {
            if let Some(msg) = status {
                *sink.lock().expect("push status lock poisoned") =
                    Some(format!("{refname}: {msg}"));
            }
            Ok(())
        });
        po.remote_callbacks(cb);
        let mut remote = self.repo.find_remote("origin").context("origin клона")?;
        let outcome = remote.push(&["refs/heads/main:refs/heads/main"], Some(&mut po));
        drop(remote);

        if let Some(reason) = rejected.lock().expect("push status lock poisoned").clone() {
            return Ok(PushOutcome::Retry(reason));
        }
        match outcome {
            Ok(()) => {
                self.fetch()?;
                println!("отдано наверх: {}", short(&local.to_string()));
                Ok(PushOutcome::Pushed)
            }
            Err(e) => {
                let text = git_err(&e);
                if is_auth_error(&e) {
                    self.refresh_grant()?;
                    return Ok(PushOutcome::Retry(format!("{text} (грант перезапрошен)")));
                }
                if is_retryable_push(&text) {
                    Ok(PushOutcome::Retry(text))
                } else {
                    Err(anyhow!("push: {text}"))
                }
            }
        }
    }

    /// Перезапросить грант по инвайту: токен шары живёт неделю, а цикл дольше.
    /// Адрес при этом не меняется: путь выбирается один раз на сессию и
    /// перевыбирается по обрыву, а не по смене токена.
    fn refresh_grant(&mut self) -> Result<()> {
        let fresh = select_share(&self.hub, &self.invite, &self.share.share_id)
            .or_else(|_| select_share(&self.hub, &self.invite, &self.share.name))
            .context("перезапрос гранта по инвайту")?;
        ensure_writable_grant(&fresh)?;
        *self.token.lock().expect("token lock poisoned") = fresh.token.clone();
        self.share = fresh;
        Ok(())
    }
}

/// Отказ авторизации у libgit2: гейт агента ответил 401/403, то есть токен
/// протух или потерял скоуп.
fn is_auth_error(e: &git2::Error) -> bool {
    matches!(e.class(), git2::ErrorClass::Http) && {
        let m = e.message().to_ascii_lowercase();
        m.contains("401") || m.contains("403") || m.contains("authentication")
    }
}

/// Отказ push, который лечится повтором цикла fetch-merge-push: успел чужой
/// push (non-fast-forward, LLD-33 п. 3.3) либо у владельца грязная рабочая
/// папка и хук приёма отбил материализацию (п. 3.4).
fn is_retryable_push(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    t.contains("non-fast-forward")
        || t.contains("fetch first")
        || t.contains("uncommitted changes")
        || t.contains("would overwrite a local file")
        || t.contains("pre-receive hook declined")
}

/// Событие, поднимающее проход синка: правка в рабочей папке либо смена HEAD у
/// агента. Страховочный проход (watcher ненадёжен, LLD-33 риск 5) события не
/// требует: это истёкшее ожидание канала.
enum Event {
    Local,
    Remote,
}

/// `xr-share sync`: подкоманда целиком. Грант проверяется до сети, транспорт
/// выбирается один раз на сессию, дальше либо один проход (`--once`), либо
/// цикл до Ctrl-C.
pub fn sync(args: SyncArgs) -> Result<()> {
    let hub = args.hub.clone().unwrap_or_else(|| HUB_DEFAULT.to_string());
    let share = select_share(&hub, &args.invite, &args.share)?;
    // Отказ до сети: без write-скоупа push не пройдёт, и узнать это лучше
    // сразу, а не после клона (LLD-33 п. 4, test_sync_no_write_scope).
    ensure_writable_grant(&share)?;
    let dir = PathBuf::from(&args.dir);
    let transport = choose_transport(&share, args.https)?;
    let mut session = Session::open(&dir, &share, transport, &args, hub)?;

    if args.once {
        return session.pass();
    }
    run_loop(&mut session)
}

/// Долгоживущий цикл (LLD-33 п. 2.5). Три источника событий сходятся в один
/// канал, и проход синка идёт по одному в момент: watcher рабочей папки с
/// дебаунсом, long-poll HEAD агента в своём потоке и страховочный тик.
///
/// Цикл блокирующий нарочно: git2 синхронен, и async-обвязка вокруг него дала
/// бы только лишний слой. Единственная задача, которой нужен рантайм, это
/// relay-мост, и он живёт в своём (`RelayLeg`).
fn run_loop(session: &mut Session) -> Result<()> {
    let (tx, rx) = std::sync::mpsc::channel::<Event>();
    let _watcher = spawn_watcher(&session.worktree, tx.clone());
    spawn_longpoll(session, tx.clone());
    println!(
        "синк идёт: папка {}, шара «{}». Ctrl-C останавливает.",
        session.worktree.display(),
        session.share.name
    );

    // Первый проход сразу: папка и шара могли разъехаться, пока синка не было.
    let mut failures = 0u32;
    report(session.pass(), &mut failures);

    let mut pending: Option<Instant> = None;
    loop {
        // Пока есть накопленная локальная правка, ждём не дольше остатка окна
        // тишины: правка коммитится через дебаунс после последнего события, а
        // не через дебаунс после первого.
        let wait = match pending {
            Some(since) => LOCAL_DEBOUNCE.saturating_sub(since.elapsed()),
            None => SAFETY_SCAN_EVERY,
        };
        match rx.recv_timeout(wait) {
            Ok(Event::Local) => {
                pending = Some(Instant::now());
                continue;
            }
            Ok(Event::Remote) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            // Оба конца канала умерли: watcher и long-poll ушли, цикл больше
            // ничего не узнает и держать его смысла нет.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                bail!("источники событий синка кончились");
            }
        }
        pending = None;
        report(session.pass(), &mut failures);
        if failures >= RECHOOSE_AFTER_FAILURES {
            match rechoose(session) {
                Ok(()) => failures = 0,
                Err(e) => {
                    eprintln!("синк: путь до агента не выбран заново: {e:#}");
                    std::thread::sleep(BACKOFF_MAX);
                }
            }
        }
    }
}

/// Итог прохода в вывод и в счётчик неудач: сбой это не конец цикла (агент
/// офлайн, LAN сменилась), а повод подождать и попробовать снова.
fn report(outcome: Result<()>, failures: &mut u32) {
    match outcome {
        Ok(()) => *failures = 0,
        Err(e) => {
            *failures += 1;
            eprintln!("синк: проход не удался: {e:#}");
            let backoff = (BACKOFF_MIN * 2u32.saturating_pow((*failures).min(5))).min(BACKOFF_MAX);
            std::thread::sleep(backoff);
        }
    }
}

/// Перевыбрать путь до агента и перевести на него origin: адрес мог поменяться
/// (переехали в другую сеть, у шары поднялся relay), и цикл не обязан из-за
/// этого умирать.
fn rechoose(session: &mut Session) -> Result<()> {
    session.refresh_grant()?;
    let share = session.share.clone();
    let https = session.transport.base.starts_with("https://");
    let transport = choose_transport(&share, https)?;
    session.repo.remote_set_url("origin", &format!("{}/git", transport.base))?;
    let via = if transport.via_relay() { " (через relay)" } else { "" };
    println!("синк: путь до агента выбран заново{via}: {}", transport.base);
    session.transport = transport;
    Ok(())
}

/// Watcher рабочей папки. Событие интересно только фактом: проход всё равно
/// пересматривает папку целиком. Watcher, который не встал (экзотическая ФС),
/// оставляет цикл на страховочном скане, а не выключает синк.
fn spawn_watcher(worktree: &Path, tx: std::sync::mpsc::Sender<Event>) -> Option<notify::RecommendedWatcher> {
    use notify::Watcher as _;
    let mut watcher = match notify::recommended_watcher(move |_: notify::Result<notify::Event>| {
        let _ = tx.send(Event::Local);
    }) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("синк: watcher не встал ({e}), остаётся страховочный скан");
            return None;
        }
    };
    if let Err(e) = watcher.watch(worktree, notify::RecursiveMode::Recursive) {
        eprintln!("синк: watcher не встал ({e}), остаётся страховочный скан");
        return None;
    }
    Some(watcher)
}

/// Поток long-poll ручки HEAD (LLD-33 п. 3.7): один висящий запрос вместо
/// частого polling, ответ приходит в момент коммита у агента. Обрывы ждут с
/// бэкофом, отказ гейта по протухшему токену перезапрашивает грант.
fn spawn_longpoll(session: &Session, tx: std::sync::mpsc::Sender<Event>) {
    let base = session.transport.base.clone();
    let token = session.token.clone();
    let agent_pubkey = session.share.agent_pubkey.clone();
    let share_id = session.share.share_id.clone();
    let hub = session.hub.clone();
    let invite = session.invite.clone();
    let name = session.share.name.clone();
    std::thread::spawn(move || {
        let mut since = String::new();
        let mut backoff = BACKOFF_MIN;
        loop {
            let current = token.lock().expect("token lock poisoned").clone();
            match fetch_head(&base, &current, &agent_pubkey, &share_id, LONGPOLL_WAIT, &since) {
                Ok(head) => {
                    backoff = BACKOFF_MIN;
                    if head != since {
                        since = head;
                        if tx.send(Event::Remote).is_err() {
                            break;
                        }
                    }
                }
                Err(e) => {
                    let text = format!("{e:#}");
                    // Протухший токен: перезапросить грант тем же инвайтом и
                    // продолжить, не роняя поток.
                    if text.contains("HTTP 401") || text.contains("HTTP 403") {
                        match select_share(&hub, &invite, &share_id)
                            .or_else(|_| select_share(&hub, &invite, &name))
                        {
                            Ok(fresh) => {
                                *token.lock().expect("token lock poisoned") = fresh.token;
                                continue;
                            }
                            Err(e) => eprintln!("синк: грант не перезапрошен: {e:#}"),
                        }
                    } else {
                        tracing::debug!("long-poll HEAD: {text}");
                    }
                    std::thread::sleep(backoff);
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use ed25519_dalek::SigningKey;
    use xr_proto::share::{sign_share_token, ShareToken, SCOPE_READ, SCOPE_WRITE};

    /// Стенд фазы 2: живой агент с git-контуром на своём порту плюс подставной
    /// хаб, отдающий грант по инвайту. Харнесс гоняется целиком, от `select_share`
    /// до push, поэтому проверяется настоящая лестница гейтов, а не её макет.
    struct Stand {
        share_dir: tempfile::TempDir,
        _state_dir: tempfile::TempDir,
        hub: String,
        git: Arc<crate::gitrepo::GitManager>,
        state: Arc<crate::server::AgentState>,
    }

    impl Stand {
        /// Рабочая папка шары на стороне агента.
        fn worktree(&self) -> PathBuf {
            self.share_dir.path().canonicalize().unwrap()
        }

        /// Прогнать агентский авто-коммит здесь и сейчас: в стенде watcher не
        /// ждём, шаги сценария должны быть детерминированными.
        fn agent_commit(&self) -> Option<String> {
            self.git.get("W").unwrap().commit_all_blocking().unwrap()
        }

        fn args(&self, dir: &Path, name: &str) -> SyncArgs {
            SyncArgs {
                invite: "inv".into(),
                hub: Some(self.hub.clone()),
                share: "W".into(),
                dir: dir.to_string_lossy().into_owned(),
                name: Some(name.into()),
                once: true,
                https: false,
            }
        }
    }

    fn unix_now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_secs()
    }

    fn blob(t: &ShareToken) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(t).unwrap())
    }

    /// Поднять агента с одной git-шарой и подставной хаб рядом. `scope` это
    /// скоуп токена в гранте: read-only грант нужен тесту отказа до сети.
    async fn stand(scope: &str) -> Stand {
        let hub_key = SigningKey::from_bytes(&[91u8; 32]);
        let identity = SigningKey::from_bytes(&[92u8; 32]);
        let share_dir = tempfile::tempdir().unwrap();
        let state_dir = tempfile::tempdir().unwrap();
        let worktree = share_dir.path().canonicalize().unwrap();

        let mut shares = crate::server::SharesMap::new();
        shares.insert(
            "W".into(),
            crate::server::ShareRoot {
                path: worktree.clone(),
                is_file: false,
                writable: true,
                import: false,
                git: true,
            },
        );
        let cache = Arc::new(crate::manifest::HashCache::new());
        let git = Arc::new(crate::gitrepo::GitManager::new(state_dir.path()));
        git.rebuild(
            &shares,
            &crate::gitrepo::GitSettings {
                author: ("агент".into(), "agent@xr-share".into()),
                max_file_mb: GIT_MAX_FILE_MB,
            },
        );
        let state = Arc::new(crate::server::AgentState {
            shares: std::sync::RwLock::new(Arc::new(shares)),
            hub_key: hub_key.verifying_key(),
            hash_cache: cache.clone(),
            identity: Some(identity.clone()),
            max_file_mb: None,
            import: crate::import::ImportManager::new(None, cache),
            git: git.clone(),
            expose: std::sync::RwLock::new(Arc::new(Vec::new())),
        });

        let agent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let agent_addr = agent.local_addr().unwrap();
        let app = crate::server::router(state.clone());
        tokio::spawn(async move {
            let _ = axum::serve(agent, app).await;
        });

        let token = sign_share_token(&hub_key, "W", scope, unix_now() + 3600);
        let dto = InviteShareDto {
            share_id: "W".into(),
            name: "W".into(),
            addr: agent_addr.ip().to_string(),
            addrs: Vec::new(),
            port: agent_addr.port(),
            agent_pubkey: base64::engine::general_purpose::STANDARD
                .encode(identity.verifying_key().as_bytes()),
            token: blob(&token),
            relay: None,
        };
        let hub_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let hub_addr = hub_listener.local_addr().unwrap();
        let dtos = vec![dto];
        let hub_app = axum::Router::new().route(
            "/api/v1/invite/{token}/shares",
            axum::routing::get(move || {
                let dtos = dtos.clone();
                async move { axum::Json(dtos) }
            }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(hub_listener, hub_app).await;
        });

        Stand {
            share_dir,
            _state_dir: state_dir,
            hub: format!("http://{hub_addr}"),
            git,
            state,
        }
    }

    /// Один проход харнесса: `sync` синхронен, а тест живёт в рантайме, где
    /// крутятся агент и подставной хаб. Поэтому проход уходит на blocking-пул
    /// (`spawn_blocking`), иначе ожидание съело бы поток рантайма и стенд
    /// перестал бы отвечать самому же проходу. Внутри ещё один обычный поток:
    /// на нём нет tokio-контекста, и `sync` свободен поднять свой рантайм под
    /// relay-мост, как он делает это в бою.
    async fn pass(args: SyncArgs) -> Result<()> {
        tokio::task::spawn_blocking(move || {
            std::thread::spawn(move || sync(args)).join().expect("проход синка не паниковал")
        })
        .await
        .expect("blocking-таск синка не паниковал")
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap_or_default()
    }

    #[tokio::test]
    async fn test_sync_roundtrip() {
        // Две машины против одного агента: правка на A доезжает до B без
        // действий, встречная правка другого файла едет обратно, а правки в
        // разных местах одного файла сливаются сами (LLD-33 п. 2.5).
        let st = stand(&format!("{SCOPE_READ} {SCOPE_WRITE}")).await;
        std::fs::write(st.worktree().join("doc.md"), "первая\nвторая\nтретья\n").unwrap();
        st.agent_commit().expect("первый коммит агента");

        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let dir_a = a.path().join("clone");
        let dir_b = b.path().join("clone");
        pass(st.args(&dir_a, "машина-A")).await.expect("клон на A");
        pass(st.args(&dir_b, "машина-B")).await.expect("клон на B");
        assert_eq!(read(&dir_a.join("doc.md")), "первая\nвторая\nтретья\n");
        assert_eq!(read(&dir_b.join("doc.md")), "первая\nвторая\nтретья\n");

        // Правка на A уезжает наверх и материализуется в папке агента.
        std::fs::write(dir_a.join("doc.md"), "ПЕРВАЯ\nвторая\nтретья\n").unwrap();
        std::fs::write(dir_a.join("новый.txt"), "от A\n").unwrap();
        pass(st.args(&dir_a, "машина-A")).await.expect("push с A");
        assert_eq!(read(&st.worktree().join("doc.md")), "ПЕРВАЯ\nвторая\nтретья\n");
        assert_eq!(read(&st.worktree().join("новый.txt")), "от A\n");
        // Файл виден и манифестным контуром: git-шара остаётся обычной шарой.
        let manifest = st
            .state
            .shares
            .read()
            .unwrap()
            .get("W")
            .unwrap()
            .path
            .join("новый.txt");
        assert!(manifest.exists());

        // B её забирает, ничего своего не делая.
        pass(st.args(&dir_b, "машина-B")).await.expect("fetch на B");
        assert_eq!(read(&dir_b.join("doc.md")), "ПЕРВАЯ\nвторая\nтретья\n");
        assert_eq!(read(&dir_b.join("новый.txt")), "от A\n");

        // Непересекающиеся правки одного файла сливаются сами: B правит третью
        // строку, A первую, обе доезжают до всех.
        std::fs::write(dir_b.join("doc.md"), "ПЕРВАЯ\nвторая\nТРЕТЬЯ\n").unwrap();
        std::fs::write(dir_a.join("doc.md"), "первая-A\nвторая\nтретья\n").unwrap();
        pass(st.args(&dir_b, "машина-B")).await.expect("push с B");
        pass(st.args(&dir_a, "машина-A")).await.expect("merge на A");
        let merged = read(&dir_a.join("doc.md"));
        assert_eq!(merged, "первая-A\nвторая\nТРЕТЬЯ\n", "обе правки на месте: {merged}");
        // Конфликт-копий тут быть не должно: строки не пересекались.
        let copies: Vec<_> = std::fs::read_dir(&dir_a)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("конфликт"))
            .collect();
        assert!(copies.is_empty(), "лишние конфликт-копии: {copies:?}");
    }

    #[tokio::test]
    async fn test_sync_conflict_copy() {
        // Пересечение по строкам не сливается: файл остаётся локальной версией,
        // встречная кладётся рядом целым файлом, и обе версии доезжают до всех
        // участников (LLD-33 п. 3.6). Маркеров в папке не появляется.
        let st = stand(&format!("{SCOPE_READ} {SCOPE_WRITE}")).await;
        std::fs::write(st.worktree().join("lines.txt"), "один\nдва\nтри\n").unwrap();
        st.agent_commit().expect("первый коммит агента");

        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let dir_a = a.path().join("clone");
        let dir_b = b.path().join("clone");
        pass(st.args(&dir_a, "машина-A")).await.expect("клон на A");
        pass(st.args(&dir_b, "машина-B")).await.expect("клон на B");

        // Обе стороны правят одну и ту же строку; A успевает первым.
        std::fs::write(dir_a.join("lines.txt"), "один\nверсия-A\nтри\n").unwrap();
        pass(st.args(&dir_a, "машина-A")).await.expect("push с A");
        std::fs::write(dir_b.join("lines.txt"), "один\nверсия-B\nтри\n").unwrap();
        pass(st.args(&dir_b, "машина-B")).await.expect("разбор конфликта на B");

        // У опоздавшего: свой файл цел, встречная версия рядом копией.
        assert_eq!(read(&dir_b.join("lines.txt")), "один\nверсия-B\nтри\n");
        let copy_b = conflict_copy_of(&dir_b);
        assert!(copy_b.starts_with("lines (конфликт машина-A "), "имя копии: {copy_b}");
        assert!(copy_b.ends_with(").txt"), "расширение у копии: {copy_b}");
        assert_eq!(read(&dir_b.join(&copy_b)), "один\nверсия-A\nтри\n");
        assert!(
            !read(&dir_b.join("lines.txt")).contains("<<<<<<<"),
            "маркеры merge в самосинкающейся папке недопустимы"
        );

        // Разбор уехал наверх: у первой стороны то же состояние и то же имя
        // копии, руками ничего переносить не надо.
        pass(st.args(&dir_a, "машина-A")).await.expect("fast-forward на A");
        assert_eq!(read(&dir_a.join("lines.txt")), "один\nверсия-B\nтри\n");
        assert_eq!(read(&dir_a.join(&copy_b)), "один\nверсия-A\nтри\n");
        assert_eq!(read(&st.worktree().join(&copy_b)), "один\nверсия-A\nтри\n");

        // Цикл не остановился: следующая правка едет как обычно.
        std::fs::write(dir_a.join("lines.txt"), "один\nпосле разбора\nтри\n").unwrap();
        pass(st.args(&dir_a, "машина-A")).await.expect("push после конфликта");
        assert_eq!(read(&st.worktree().join("lines.txt")), "один\nпосле разбора\nтри\n");
    }

    /// Единственная конфликт-копия в папке (тесты ждут ровно одну).
    fn conflict_copy_of(dir: &Path) -> String {
        let mut found: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("конфликт"))
            .collect();
        assert_eq!(found.len(), 1, "ожидалась одна конфликт-копия: {found:?}");
        found.pop().unwrap()
    }

    #[tokio::test]
    async fn test_sync_refuses_nonclone_dir() {
        // Непустая посторонняя папка: молча мешать локальные файлы с историей
        // шары нельзя, отказ с понятным текстом (LLD-33 п. 2.5).
        let st = stand(&format!("{SCOPE_READ} {SCOPE_WRITE}")).await;
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("моё.txt"), "личное\n").unwrap();
        let err = pass(st.args(d.path(), "машина-A")).await.expect_err("отказ");
        let text = format!("{err:#}");
        assert!(text.contains("не пуста"), "текст отказа: {text}");
        // Файл на месте, папку не тронули.
        assert_eq!(read(&d.path().join("моё.txt")), "личное\n");
    }

    #[tokio::test]
    async fn test_sync_no_write_scope() {
        // Грант без share:write это отказ до сети: git-контур целиком под
        // write-скоупом (LLD-33 п. 3.8), и узнать это надо раньше клона.
        let st = stand(SCOPE_READ).await;
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("clone");
        let err = pass(st.args(&dir, "машина-A")).await.expect_err("отказ");
        let text = format!("{err:#}");
        assert!(text.contains("нет права записи"), "текст отказа: {text}");
        assert!(!dir.exists(), "клон не должен появиться");
    }

    #[tokio::test]
    async fn test_sync_size_cap() {
        // Колпак общий с агентом (LLD-33 п. 2.6): файл крупнее не едет в
        // историю, но остаётся в папке и раздаётся манифестным контуром.
        let st = stand(&format!("{SCOPE_READ} {SCOPE_WRITE}")).await;
        std::fs::write(st.worktree().join("doc.md"), "текст\n").unwrap();
        st.agent_commit().expect("первый коммит агента");
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("clone");
        pass(st.args(&dir, "машина-A")).await.expect("клон");

        let big = dir.join("видео.bin");
        std::fs::write(&big, vec![7u8; (GIT_MAX_FILE_MB as usize + 1) * 1024 * 1024]).unwrap();
        std::fs::write(dir.join("мелкий.txt"), "влезает\n").unwrap();
        pass(st.args(&dir, "машина-A")).await.expect("проход с большим файлом");

        // Мелкий уехал, крупный остался только на диске у автора.
        assert_eq!(read(&st.worktree().join("мелкий.txt")), "влезает\n");
        assert!(!st.worktree().join("видео.bin").exists(), "крупный файл в историю не едет");
        assert!(big.exists(), "крупный файл остаётся в папке автора");
    }

    #[test]
    fn conflict_copy_names_carry_author_and_sha() {
        // Имя копии несёт автора и короткий SHA встречного коммита, поэтому
        // повторный разбор той же развилки даёт то же имя (LLD-33 п. 3.6).
        assert_eq!(
            conflict_copy_name("doc.md", "макбук", "abc1234"),
            "doc (конфликт макбук abc1234).md"
        );
        assert_eq!(
            conflict_copy_name("папка/вложенный.txt", "макбук", "abc1234"),
            "папка/вложенный (конфликт макбук abc1234).txt"
        );
        // Файл без расширения и dotfile: точка в начале это имя, не расширение.
        assert_eq!(
            conflict_copy_name("README", "макбук", "abc1234"),
            "README (конфликт макбук abc1234)"
        );
        assert_eq!(
            conflict_copy_name(".editorconfig", "макбук", "abc1234"),
            ".editorconfig (конфликт макбук abc1234)"
        );
        // Автор со слэшем не уводит копию в другой каталог.
        assert_eq!(
            conflict_copy_name("doc.md", "их/машина", "abc1234"),
            "doc (конфликт их-машина abc1234).md"
        );
    }

    #[tokio::test]
    async fn test_sync_first_push_into_empty_share() {
        // Пустовавшая шара: клон приезжает с нерождённым HEAD, и первая же
        // правка соавтора создаёт `main` на агенте. Тот случай, из-за которого
        // fetch ходит рефспеком со звёздочкой, а не за точной веткой.
        let st = stand(&format!("{SCOPE_READ} {SCOPE_WRITE}")).await;
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("clone");
        pass(st.args(&dir, "машина-A")).await.expect("клон пустой шары");
        assert!(dir.join(".git").exists(), "клон на месте даже у пустой шары");

        std::fs::write(dir.join("первый.md"), "начало\n").unwrap();
        pass(st.args(&dir, "машина-A")).await.expect("первый push");
        assert_eq!(read(&st.worktree().join("первый.md")), "начало\n");
    }

    #[test]
    fn rejected_push_tells_retry_from_real_failure() {
        // Отказ, который лечится повтором цикла: успел чужой push (LLD-33
        // п. 3.3) либо у владельца грязная папка и хук приёма отбил
        // материализацию (п. 3.4).
        assert!(is_retryable_push("failed to push some refs: non-fast-forward"));
        assert!(is_retryable_push("Updates were rejected, fetch first"));
        assert!(is_retryable_push(
            "xr-share: working folder has uncommitted changes, push rejected"
        ));
        assert!(is_retryable_push(
            "xr-share: the push would overwrite a local file outside the history"
        ));
        assert!(is_retryable_push("pre-receive hook declined"));
        // А это не гонка, а поломка: повтор её не вылечит, и она обязана
        // доехать наверх ошибкой.
        assert!(!is_retryable_push("could not resolve host"));
        assert!(!is_retryable_push("request failed with status code: 500"));
        assert!(!is_retryable_push("pack exceeds maximum allowed size"));
    }

    #[test]
    fn skipped_covers_service_namespace_and_own_git() {
        let oversize = vec!["видео.bin".to_string()];
        assert!(skipped(Path::new(".git/config"), &oversize));
        assert!(skipped(Path::new(".xr-part-1"), &oversize));
        assert!(skipped(Path::new("папка/.xr-import-7/файл"), &oversize));
        assert!(skipped(Path::new("видео.bin"), &oversize));
        assert!(!skipped(Path::new("doc.md"), &oversize));
        // `.gitignore` это обычный файл, а не служебный каталог.
        assert!(!skipped(Path::new(".gitignore"), &oversize));
    }
}
