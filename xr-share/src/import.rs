//! URL-import jobs (LLD-29): the agent downloads a page's content into a
//! writable share by running an owner-configured external plugin (the reference
//! is a yt-dlp wrapper). The core stays a thin file server: this module only
//! gates the URL, runs the process, tracks progress and publishes the result
//! through the same temp + rename + hash-seed contour as an upload.
//!
//! Safety model (LLD-29 п. 3.4): the URL comes from a device holder but runs on
//! the owner's machine inside their LAN, so SSRF is treated as a read primitive.
//! Layer one is [`check_url`] before the process starts (scheme, resolve, no
//! private ranges); layer two is a systemd-run network sandbox on Linux
//! ([`sandbox_wrap`]) that keeps kernel-level deny rules for the same ranges, so
//! a redirect or DNS rebinding after the check still cannot reach the LAN.
//!
//! Jobs are ephemeral (LLD-29 п. 3.7): an in-memory table, one running job,
//! a short queue, finished entries visible for an hour. A restart forgets them
//! and [`sweep_share_root`] removes leftover job dirs.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::sync::Notify;

use crate::config::{ImportConfig, ImportPlugin};
use crate::manifest::HashCache;

/// Prefix of a job's private working directory in the share root, inside the
/// reserved `.xr-` namespace (LLD-29 п. 3.8).
pub const JOB_DIR_PREFIX: &str = ".xr-import-";

/// Requested/effective frame height bounds (LLD-29 п. 2.5).
pub const HEIGHT_MIN: u32 = 144;
pub const HEIGHT_MAX: u32 = 4320;

/// Queued jobs beyond the running one; the fifth POST gets a `429`.
const QUEUE_DEPTH: usize = 4;
/// How long a finished job stays visible to polls before the lazy sweep.
const DONE_TTL: Duration = Duration::from_secs(3600);
/// Cadence of the timeout / total-size watchdog while a job runs. Tests kill
/// misbehaving fake plugins in milliseconds instead of waiting real seconds.
#[cfg(not(test))]
const WATCH_TICK: Duration = Duration::from_secs(3);
#[cfg(test)]
const WATCH_TICK: Duration = Duration::from_millis(50);
/// How much of the stderr tail is kept for a failed job's error text.
const STDERR_TAIL: usize = 4096;

/// The private/special ranges refused by the URL gate and denied to the
/// sandboxed process. One list, two enforcement points (LLD-29 п. 3.5).
const DENY_RANGES: &str = "0.0.0.0/8 127.0.0.0/8 10.0.0.0/8 172.16.0.0/12 \
    192.168.0.0/16 169.254.0.0/16 100.64.0.0/10 224.0.0.0/4 240.0.0.0/4 \
    ::/128 ::1/128 fe80::/10 fc00::/7 ff00::/8";

// -- job table -------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum JobState {
    Queued,
    Running,
    Done,
    Failed,
}

impl JobState {
    fn name(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
}

/// Everything the runner needs to execute one job, captured at enqueue time so
/// a config hot-swap mid-flight cannot change a job's limits under it.
#[derive(Clone)]
pub struct JobSpec {
    /// Canonical share root; the job dir is created here (same filesystem as
    /// the target, so the final rename is atomic).
    pub share_root: PathBuf,
    /// Destination directory, share-relative ("" = the root). Already safepath-
    /// cleaned by the route handler.
    pub dest_rel: String,
    pub url: String,
    /// Effective height, `min(request, plugin cap)`; substituted for `{height}`.
    pub height: u32,
    pub plugin: ImportPlugin,
    pub timeout: Duration,
    pub max_total_bytes: Option<u64>,
    pub max_file_bytes: Option<u64>,
    /// `auto` | `none` from the config, resolved to a wrapper in [`sandbox_wrap`].
    pub sandbox: String,
}

struct Job {
    state: JobState,
    progress: Option<f64>,
    files: Vec<String>,
    error: Option<String>,
    /// Process-group leader of the running plugin, for cancellation.
    pid: Option<u32>,
    finished: Option<Instant>,
    /// Present while queued; the runner takes it when the job starts.
    spec: Option<JobSpec>,
}

/// One job's externally visible status (`GET .../import/{job_id}`).
#[derive(Serialize)]
pub struct JobStatusDto {
    pub state: &'static str,
    pub progress: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

type JobsMap = Arc<Mutex<HashMap<String, Job>>>;

/// The in-memory job registry plus the single-worker queue (LLD-29 п. 2.5).
/// Config is swappable for hot reload; the hash cache is the same one the
/// manifest builders use, so a published file serves already hashed.
pub struct ImportManager {
    config: RwLock<Option<Arc<ImportConfig>>>,
    jobs: JobsMap,
    queue: Mutex<VecDeque<String>>,
    notify: Notify,
    cache: Arc<HashCache>,
}

impl ImportManager {
    pub fn new(config: Option<ImportConfig>, cache: Arc<HashCache>) -> Arc<Self> {
        Arc::new(Self {
            config: RwLock::new(config.map(Arc::new)),
            jobs: Arc::new(Mutex::new(HashMap::new())),
            queue: Mutex::new(VecDeque::new()),
            notify: Notify::new(),
            cache,
        })
    }

    /// Swap the `[import]` block on config hot-reload. Running jobs keep the
    /// limits they were enqueued with.
    pub fn set_config(&self, config: Option<ImportConfig>) {
        *self.config.write().expect("import config lock") = config.map(Arc::new);
    }

    pub fn config(&self) -> Option<Arc<ImportConfig>> {
        self.config.read().expect("import config lock").clone()
    }

    /// True when at least one plugin is configured: without any, import answers
    /// `403` like a share without the flag (LLD-29 п. 2.6).
    pub fn has_plugins(&self) -> bool {
        self.config().is_some_and(|c| !c.plugins.is_empty())
    }

    /// Add a job to the queue. `None` when the queue is full (`429`).
    pub fn enqueue(&self, spec: JobSpec) -> Option<String> {
        let mut jobs = self.jobs.lock().expect("import jobs lock");
        sweep_finished(&mut jobs);
        let waiting = jobs.values().filter(|j| j.state == JobState::Queued).count();
        if waiting >= QUEUE_DEPTH {
            return None;
        }
        let id = format!("{:016x}", rand::random::<u64>());
        jobs.insert(
            id.clone(),
            Job {
                state: JobState::Queued,
                progress: None,
                files: Vec::new(),
                error: None,
                pid: None,
                finished: None,
                spec: Some(spec),
            },
        );
        drop(jobs);
        self.queue.lock().expect("import queue lock").push_back(id.clone());
        self.notify.notify_one();
        Some(id)
    }

    /// `None` for an unknown (or swept, or forgotten-by-restart) job: the
    /// consumer turns the `404` into "the job got lost" (LLD-29 п. 3.7).
    pub fn status(&self, job_id: &str) -> Option<JobStatusDto> {
        let mut jobs = self.jobs.lock().expect("import jobs lock");
        sweep_finished(&mut jobs);
        jobs.get(job_id).map(|j| JobStatusDto {
            state: j.state.name(),
            progress: j.progress,
            files: (j.state == JobState::Done).then(|| j.files.clone()),
            error: j.error.clone(),
        })
    }

    /// Cancel: kill the process group (if running), drop the job from the table
    /// so later polls see `404`. The runner cleans the job dir up when the kill
    /// lands. True if the job existed.
    pub fn cancel(&self, job_id: &str) -> bool {
        let Some(job) = self.jobs.lock().expect("import jobs lock").remove(job_id) else {
            return false;
        };
        if let Some(pid) = job.pid {
            kill_group(pid);
        }
        true
    }

    /// Start the single worker that executes queued jobs one at a time
    /// (LLD-29 п. 3.2: one process per job, no plugin daemon).
    pub fn spawn_runner(self: &Arc<Self>) {
        let mgr = self.clone();
        tokio::spawn(async move {
            loop {
                let id = loop {
                    match mgr.pop_queued() {
                        Some(id) => break id,
                        None => mgr.notify.notified().await,
                    }
                };
                mgr.run_job(&id).await;
            }
        });
    }

    /// Next queued id whose job still exists (a cancelled one is skipped).
    fn pop_queued(&self) -> Option<String> {
        let mut queue = self.queue.lock().expect("import queue lock");
        let jobs = self.jobs.lock().expect("import jobs lock");
        while let Some(id) = queue.pop_front() {
            if jobs.contains_key(&id) {
                return Some(id);
            }
        }
        None
    }

    fn with_job<T>(&self, id: &str, f: impl FnOnce(&mut Job) -> T) -> Option<T> {
        self.jobs.lock().expect("import jobs lock").get_mut(id).map(f)
    }

    /// Execute one job to completion: job dir, plugin process (sandboxed where
    /// possible), progress/stderr readers, timeout + size watchdog, then
    /// publication. The job dir is removed on every outcome.
    async fn run_job(&self, id: &str) {
        let Some(spec) = self
            .with_job(id, |j| {
                j.state = JobState::Running;
                j.spec.take()
            })
            .flatten()
        else {
            return; // cancelled while queued
        };

        let job_dir = spec.share_root.join(format!("{JOB_DIR_PREFIX}{:016x}", rand::random::<u64>()));
        let outcome = self.run_in_dir(id, &spec, &job_dir).await;
        let _ = tokio::fs::remove_dir_all(&job_dir).await;

        let gone = self
            .with_job(id, |j| {
                match &outcome {
                    Ok(files) => {
                        j.state = JobState::Done;
                        j.files = files.clone();
                    }
                    Err(e) => {
                        j.state = JobState::Failed;
                        j.error = Some(e.clone());
                    }
                }
                j.pid = None;
                j.finished = Some(Instant::now());
            })
            .is_none();
        match &outcome {
            _ if gone => tracing::info!("import job {id}: cancelled"),
            Ok(files) => tracing::info!("import job {id}: done, {} file(s)", files.len()),
            Err(e) => tracing::warn!("import job {id}: failed: {e}"),
        }
    }

    async fn run_in_dir(&self, id: &str, spec: &JobSpec, job_dir: &Path) -> Result<Vec<String>, String> {
        tokio::fs::create_dir_all(job_dir)
            .await
            .map_err(|e| format!("не удалось создать рабочую папку: {e}"))?;
        // Parents of the destination exist before the plugin starts, so the
        // final rename cannot fail on a missing directory (LLD-29 п. 2.5).
        let dest_dir = spec.share_root.join(&spec.dest_rel);
        tokio::fs::create_dir_all(&dest_dir)
            .await
            .map_err(|e| format!("не удалось создать папку назначения: {e}"))?;

        let (cmd, args) = build_argv(&spec.plugin, &spec.url, spec.height);
        let (cmd, args) = sandbox_wrap(cmd, args, &spec.sandbox);
        let mut command = tokio::process::Command::new(&cmd);
        command
            .args(&args)
            .current_dir(job_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // The plugin leads its own process group, so cancellation kills its
        // whole tree (yt-dlp spawns ffmpeg), not just the direct child.
        #[cfg(unix)]
        {
            command.process_group(0);
        }
        let mut child = command
            .spawn()
            .map_err(|e| format!("не удалось запустить плагин {}: {e}", spec.plugin.name))?;
        let pid = child.id();
        self.with_job(id, |j| j.pid = pid);

        // Progress lines land in the table as they arrive; stderr keeps only
        // its tail for the error text (LLD-29 п. 2.4).
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let progress_task = {
            let jobs = self.jobs.clone();
            let id = id.to_string();
            tokio::spawn(async move {
                let Some(out) = stdout else { return };
                let mut lines = BufReader::new(out).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Some(p) = parse_progress(&line) {
                        if let Some(j) = jobs.lock().expect("import jobs lock").get_mut(&id) {
                            j.progress = Some(p);
                        }
                    }
                }
            })
        };
        let stderr_task = tokio::spawn(async move {
            let mut tail = VecDeque::with_capacity(STDERR_TAIL);
            let Some(mut err) = stderr else { return String::new() };
            let mut buf = [0u8; 1024];
            while let Ok(n) = err.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                for b in &buf[..n] {
                    if tail.len() == STDERR_TAIL {
                        tail.pop_front();
                    }
                    tail.push_back(*b);
                }
            }
            String::from_utf8_lossy(&Vec::from(tail)).into_owned()
        });

        // Wait with a watchdog: kill on the lifetime cap or on the job dir
        // outgrowing max_total_mb (checked every few seconds, LLD-29 п. 2.7).
        let started = Instant::now();
        let mut tick = tokio::time::interval(WATCH_TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut kill_reason: Option<String> = None;
        let exit = loop {
            tokio::select! {
                status = child.wait() => break status,
                _ = tick.tick() => {
                    if started.elapsed() >= spec.timeout {
                        kill_reason = Some(format!(
                            "джоба превысила предел времени ({} мин)",
                            spec.timeout.as_secs() / 60
                        ));
                    } else if let Some(cap) = spec.max_total_bytes {
                        let dir = job_dir.to_path_buf();
                        let size = tokio::task::spawn_blocking(move || dir_size(&dir))
                            .await
                            .unwrap_or(0);
                        if size > cap {
                            kill_reason = Some(format!(
                                "выхлоп джобы превысил max_total_mb ({} МиБ)",
                                cap / (1024 * 1024)
                            ));
                        }
                    }
                    if kill_reason.is_some() {
                        if let Some(pid) = pid {
                            kill_group(pid);
                        }
                        let _ = child.kill().await;
                    }
                }
            }
        };
        let _ = progress_task.await;
        let stderr_tail = stderr_task.await.unwrap_or_default();

        // A cancelled job is already out of the table; skip publishing.
        if self.with_job(id, |_| ()).is_none() {
            return Err("отменено".into());
        }
        if let Some(reason) = kill_reason {
            return Err(reason);
        }
        let status = exit.map_err(|e| format!("ожидание плагина: {e}"))?;
        if !status.success() {
            let tail = stderr_tail.trim();
            return Err(if tail.is_empty() {
                format!("плагин завершился с ошибкой ({status})")
            } else {
                format!("плагин завершился с ошибкой ({status}): {tail}")
            });
        }

        // Exit 0: publish the job dir's top-level regular files (LLD-29 п. 2.7).
        let publish = PublishSpec {
            job_dir: job_dir.to_path_buf(),
            dest_dir,
            dest_rel: spec.dest_rel.clone(),
            max_file_bytes: spec.max_file_bytes,
        };
        let cache = self.cache.clone();
        tokio::task::spawn_blocking(move || publish_files(&publish, &cache))
            .await
            .map_err(|_| "публикация не завершилась".to_string())?
    }
}

/// Drop finished jobs older than [`DONE_TTL`] (called lazily on any access).
fn sweep_finished(jobs: &mut HashMap<String, Job>) {
    jobs.retain(|_, j| match j.finished {
        Some(t) => t.elapsed() < DONE_TTL,
        None => true,
    });
}

// -- argv, sandbox, process control ----------------------------------

/// Substitute the template: `{url}` as a whole literal argv element (validated
/// at startup), `{height}` inside any element with the validated number.
fn build_argv(plugin: &ImportPlugin, url: &str, height: u32) -> (String, Vec<String>) {
    let args = plugin
        .args
        .iter()
        .map(|a| {
            if a == "{url}" {
                url.to_string()
            } else {
                a.replace("{height}", &height.to_string())
            }
        })
        .collect();
    (plugin.cmd.clone(), args)
}

/// Wrap the command in a systemd-run scope with kernel-level private-range
/// deny rules where possible (Linux with systemd, `sandbox = "auto"`); anywhere
/// else run it bare and log once (LLD-29 п. 3.5).
fn sandbox_wrap(cmd: String, args: Vec<String>, sandbox: &str) -> (String, Vec<String>) {
    if sandbox != "auto" {
        return (cmd, args);
    }
    if !systemd_run_usable() {
        warn_no_sandbox_once();
        return (cmd, args);
    }
    let mut wrapped = vec![
        "--scope".to_string(),
        "--collect".to_string(),
        "--quiet".to_string(),
        "--property=IPAddressAllow=any".to_string(),
        format!("--property=IPAddressDeny={DENY_RANGES}"),
        "--property=MemoryMax=2G".to_string(),
        "--".to_string(),
        cmd,
    ];
    wrapped.extend(args);
    ("systemd-run".to_string(), wrapped)
}

/// systemd-run exists and systemd is actually PID 1 here.
fn systemd_run_usable() -> bool {
    cfg!(target_os = "linux")
        && Path::new("/run/systemd/system").exists()
        && which("systemd-run").is_some()
}

/// First `PATH` hit for `name`, like the shell would resolve it.
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let candidate = dir.join(name);
        candidate.is_file().then_some(candidate)
    })
}

fn warn_no_sandbox_once() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        tracing::warn!(
            "import runs WITHOUT a network sandbox (no systemd here): \
             the pre-start URL gate is the only SSRF barrier"
        );
    });
}

/// SIGKILL the whole process group led by `pid` (unix). On Windows only the
/// direct child is killed by the caller; the accepted platform gap.
fn kill_group(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = pid;
}

/// Parse a plugin progress line: `xr-progress <number>`, tolerant of `%` and
/// spaces around the number (LLD-29 п. 2.4). Anything else is `None`.
fn parse_progress(line: &str) -> Option<f64> {
    let rest = line.trim_start().strip_prefix("xr-progress")?;
    let number: String = rest
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    number.parse::<f64>().ok().map(|p| p.clamp(0.0, 100.0))
}

/// Recursive size of a directory in bytes (best effort).
fn dir_size(dir: &Path) -> u64 {
    walkdir::WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .flatten()
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

// -- publication -----------------------------------------------------

struct PublishSpec {
    job_dir: PathBuf,
    dest_dir: PathBuf,
    dest_rel: String,
    max_file_bytes: Option<u64>,
}

/// Publish the job dir's output into the share: top-level regular files only,
/// hidden names skipped (that also covers `.xr-*`), each checked against
/// `max_file_mb`, hashed, fsync'd and renamed over the target, hash seeded
/// (LLD-29 п. 2.7). A mid-way failure reports what did get published.
fn publish_files(spec: &PublishSpec, cache: &HashCache) -> Result<Vec<String>, String> {
    let mut names: Vec<String> = Vec::new();
    let entries = std::fs::read_dir(&spec.job_dir).map_err(|e| format!("чтение рабочей папки: {e}"))?;
    for entry in entries.flatten() {
        let is_file = entry.file_type().map(|t| t.is_file()).unwrap_or(false);
        let name = entry.file_name().to_string_lossy().into_owned();
        // Hidden files are the tools' own caches and part-files; subdirs are
        // not published either (LLD-29 п. 2.4).
        if !is_file || name.starts_with('.') {
            continue;
        }
        names.push(name);
    }
    names.sort();
    if names.is_empty() {
        return Err("плагин завершился успешно, но не оставил ни одного файла".into());
    }

    let mut published: Vec<String> = Vec::new();
    for name in names {
        let src = spec.job_dir.join(&name);
        let result = publish_one(&src, spec, &name, cache);
        if let Err(e) = result {
            let suffix = if published.is_empty() {
                String::new()
            } else {
                format!("; уже опубликовано: {}", published.join(", "))
            };
            return Err(format!("{name}: {e}{suffix}"));
        }
        published.push(rel_path(&spec.dest_rel, &name));
    }
    Ok(published)
}

fn publish_one(src: &Path, spec: &PublishSpec, name: &str, cache: &HashCache) -> Result<(), String> {
    let meta = std::fs::metadata(src).map_err(|e| format!("stat: {e}"))?;
    if let Some(cap) = spec.max_file_bytes {
        if meta.len() > cap {
            return Err(format!("файл больше лимита агента (max_file_mb): {} байт", meta.len()));
        }
    }
    let sha = sha256_file(src).map_err(|e| format!("хеш: {e}"))?;
    let file = std::fs::File::open(src).map_err(|e| format!("открытие: {e}"))?;
    file.sync_all().map_err(|e| format!("fsync: {e}"))?;
    drop(file);

    let target = spec.dest_dir.join(name);
    rename_replace_sync(src, &target).map_err(|e| format!("rename: {e}"))?;
    if let Ok(meta) = std::fs::metadata(&target) {
        cache.seed(&target, meta.len(), mtime_secs(&meta), sha);
    }
    Ok(())
}

fn rel_path(dest_rel: &str, name: &str) -> String {
    if dest_rel.is_empty() {
        name.to_string()
    } else {
        format!("{dest_rel}/{name}")
    }
}

/// Rename over an existing target: atomic on unix; Windows removes first (the
/// same accepted tiny window as the upload path, LLD-28 risk 2).
fn rename_replace_sync(from: &Path, to: &Path) -> std::io::Result<()> {
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(e) => {
            #[cfg(windows)]
            {
                let _ = std::fs::remove_file(to);
                return std::fs::rename(from, to);
            }
            #[cfg(not(windows))]
            Err(e)
        }
    }
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    use std::fmt::Write;
    let mut s = String::with_capacity(64);
    for b in digest {
        let _ = write!(s, "{b:02x}");
    }
    Ok(s)
}

fn mtime_secs(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// -- startup sweep ---------------------------------------------------

/// Remove leftover service files from a share root at startup: `.xr-import-*`
/// job dirs of a previous run (the table died with the process, LLD-29 п. 3.7)
/// and orphaned `.xr-part-*` upload temps, wherever they sit in the tree.
pub fn sweep_share_root(root: &Path) {
    let mut it = walkdir::WalkDir::new(root).follow_links(false).into_iter();
    while let Some(Ok(entry)) = it.next() {
        if entry.depth() == 0 {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type().is_dir() && name.starts_with(JOB_DIR_PREFIX) {
            let _ = std::fs::remove_dir_all(entry.path());
            it.skip_current_dir();
        } else if entry.file_type().is_file()
            && name.starts_with(crate::manifest::UPLOAD_TEMP_PREFIX)
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

// -- URL gate --------------------------------------------------------

/// Scheme + host of an http(s) URL, without pulling a URL crate: enough for the
/// gate, which only ever routes by host. Userinfo, port, path and query are
/// tolerated and ignored; a v6 literal comes bracketed.
pub fn parse_url(url: &str) -> Result<(String, String), String> {
    let url = url.trim();
    let (scheme, rest) = url.split_once("://").ok_or("не URL: нет схемы")?;
    let scheme = scheme.to_ascii_lowercase();
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    // Strip userinfo; the *last* @ splits it from the host (RFC 3986).
    let host_port = authority.rsplit_once('@').map(|(_, h)| h).unwrap_or(authority);
    let host = if let Some(v6) = host_port.strip_prefix('[') {
        v6.split_once(']').ok_or("не URL: незакрытый IPv6-литерал")?.0.to_string()
    } else {
        match host_port.rsplit_once(':') {
            Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => h.to_string(),
            _ => host_port.to_string(),
        }
    };
    if host.is_empty() {
        return Err("не URL: пустой хост".into());
    }
    Ok((scheme, host.to_ascii_lowercase()))
}

/// The pre-start SSRF gate (LLD-29 п. 2.6): http/https only; the host resolves,
/// and **every** address is outside the private/special ranges. Returns the
/// host on success (for plugin routing).
pub async fn check_url(url: &str) -> Result<String, String> {
    let (scheme, host) = parse_url(url)?;
    if scheme != "http" && scheme != "https" {
        return Err(format!("схема {scheme:?} не поддерживается, только http/https"));
    }
    let addrs: Vec<IpAddr> = if let Ok(ip) = host.parse::<IpAddr>() {
        vec![ip]
    } else {
        tokio::net::lookup_host((host.as_str(), 80))
            .await
            .map_err(|_| format!("хост не резолвится: {host}"))?
            .map(|sa| sa.ip())
            .collect()
    };
    if addrs.is_empty() {
        return Err(format!("хост не резолвится: {host}"));
    }
    for ip in addrs {
        if is_private_ip(ip) {
            return Err(format!("адрес {ip} в приватном или специальном диапазоне"));
        }
    }
    Ok(host)
}

/// True for every range the import gate refuses (mirrors [`DENY_RANGES`]).
fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || o[0] == 0
                // CGNAT 100.64.0.0/10
                || (o[0] == 100 && (o[1] & 0xC0) == 64)
                // 240.0.0.0/4 (reserved, includes broadcast)
                || o[0] >= 240
        }
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_private_ip(IpAddr::V4(mapped));
            }
            let seg = v6.segments();
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // link-local fe80::/10
                || (seg[0] & 0xFFC0) == 0xFE80
                // ULA fc00::/7
                || (seg[0] & 0xFE00) == 0xFC00
        }
    }
}

// -- plugin routing and quality --------------------------------------

/// Pick the plugin for `host`: longest matching host suffix at a label boundary
/// wins, `"*"` catches the rest (LLD-29 п. 3.3). `None` -> `422`.
pub fn route_plugin<'a>(plugins: &'a [ImportPlugin], host: &str) -> Option<&'a ImportPlugin> {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let mut best: Option<(&ImportPlugin, usize)> = None;
    let mut catch_all: Option<&ImportPlugin> = None;
    for plugin in plugins {
        for pattern in &plugin.patterns {
            if pattern == "*" {
                catch_all.get_or_insert(plugin);
                continue;
            }
            let p = pattern.trim_end_matches('.').to_ascii_lowercase();
            let matches = host == p || host.ends_with(&format!(".{p}"));
            if matches && best.map_or(true, |(_, len)| p.len() > len) {
                best = Some((plugin, p.len()));
            }
        }
    }
    best.map(|(p, _)| p).or(catch_all)
}

/// Effective frame height: the requester's wish clamped to the owner's cap
/// (LLD-29 п. 3.9). A request outside the sane range is a `400`.
pub fn effective_height(requested: Option<u32>, plugin: &ImportPlugin) -> Result<u32, String> {
    match requested {
        None => Ok(plugin.max_height),
        Some(h) if (HEIGHT_MIN..=HEIGHT_MAX).contains(&h) => Ok(h.min(plugin.max_height)),
        Some(h) => Err(format!("height {h} вне диапазона {HEIGHT_MIN}..{HEIGHT_MAX}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plugin(name: &str, patterns: &[&str], max_height: u32) -> ImportPlugin {
        ImportPlugin {
            name: name.into(),
            patterns: patterns.iter().map(|s| s.to_string()).collect(),
            max_height,
            cmd: "true".into(),
            args: vec!["{url}".into()],
        }
    }

    #[tokio::test]
    async fn test_url_guard() {
        // Non-http schemes are refused outright.
        for bad in ["file:///etc/passwd", "ftp://host/x", "gopher://host", "not a url"] {
            assert!(check_url(bad).await.is_err(), "must reject {bad}");
        }
        // Literal private/special addresses, v4 and v6.
        for bad in [
            "http://127.0.0.1/admin",
            "http://10.1.2.3/x",
            "http://192.168.1.1/router",
            "http://172.16.5.5/x",
            "http://169.254.169.254/metadata",
            "http://100.64.0.1/cgnat",
            "http://0.0.0.0/x",
            "http://[::1]/x",
            "http://[fe80::1]/x",
            "http://[fc00::1]/x",
            "http://[::ffff:192.168.0.1]/x",
        ] {
            assert!(check_url(bad).await.is_err(), "must reject {bad}");
        }
        // A host that resolves to loopback (no external DNS needed).
        assert!(check_url("http://localhost/x").await.is_err());
        // Public literals pass without touching DNS; port/userinfo are tolerated.
        assert_eq!(check_url("https://93.184.216.34/video").await.unwrap(), "93.184.216.34");
        assert_eq!(check_url("http://user:pw@93.184.216.34:8080/v?a=1").await.unwrap(), "93.184.216.34");
        assert_eq!(check_url("https://[2606:4700::6810:84e5]/x").await.unwrap(), "2606:4700::6810:84e5");
    }

    #[test]
    fn parse_url_extracts_scheme_and_host() {
        assert_eq!(parse_url("https://YouTube.com/watch?v=1").unwrap(), ("https".into(), "youtube.com".into()));
        assert_eq!(parse_url("http://a.b.c:8080/p").unwrap(), ("http".into(), "a.b.c".into()));
        assert_eq!(parse_url("http://user@host/p#f").unwrap(), ("http".into(), "host".into()));
        assert_eq!(parse_url("http://[::1]:443/p").unwrap(), ("http".into(), "::1".into()));
        assert!(parse_url("youtube.com/watch").is_err());
        assert!(parse_url("http:///nohost").is_err());
    }

    #[test]
    fn test_plugin_routing() {
        let plugins = vec![
            plugin("yt", &["youtube.com", "youtu.be"], 1080),
            plugin("yt-long", &["music.youtube.com"], 720),
            plugin("rest", &["*"], 480),
        ];
        let pick = |host: &str| route_plugin(&plugins, host).map(|p| p.name.as_str());

        // Suffix at a label boundary, subdomains included.
        assert_eq!(pick("youtube.com"), Some("yt"));
        assert_eq!(pick("www.YouTube.com"), Some("yt"));
        assert_eq!(pick("youtu.be"), Some("yt"));
        // The longest suffix wins over a shorter one.
        assert_eq!(pick("music.youtube.com"), Some("yt-long"));
        // No label boundary, no match: evil-youtube.com is not youtube.com.
        assert_eq!(pick("evil-youtube.com"), Some("rest"));
        // Catch-all takes the rest.
        assert_eq!(pick("example.org"), Some("rest"));

        // Without a catch-all an unmatched host routes nowhere (-> 422).
        let strict = vec![plugin("yt", &["youtube.com"], 1080)];
        assert!(route_plugin(&strict, "evil-youtube.com").is_none());
        assert!(route_plugin(&strict, "example.org").is_none());
    }

    #[test]
    fn effective_height_clamps_to_owner_cap() {
        let p = plugin("yt", &["*"], 1080);
        // The wish is clamped to the owner's cap; no wish takes the cap itself.
        assert_eq!(effective_height(Some(4000), &p).unwrap(), 1080);
        assert_eq!(effective_height(Some(720), &p).unwrap(), 720);
        assert_eq!(effective_height(None, &p).unwrap(), 1080);
        // Out-of-range wishes are a 400, not a silent clamp.
        assert!(effective_height(Some(1), &p).is_err());
        assert!(effective_height(Some(100_000), &p).is_err());
    }

    #[test]
    fn parse_progress_is_lenient() {
        // The yt-dlp template prints "xr-progress  42.5%"; junk lines are None.
        assert_eq!(parse_progress("xr-progress 42.5"), Some(42.5));
        assert_eq!(parse_progress("xr-progress   7.0%"), Some(7.0));
        assert_eq!(parse_progress("  xr-progress 100"), Some(100.0));
        assert_eq!(parse_progress("xr-progress 250"), Some(100.0));
        assert_eq!(parse_progress("[download] 42% of ~3MiB"), None);
        assert_eq!(parse_progress("xr-progress"), None);
    }

    #[test]
    fn build_argv_substitutes_literally() {
        let mut p = plugin("yt", &["*"], 1080);
        p.args = vec![
            "-f".into(),
            "b[height<={height}]".into(),
            "{url}".into(),
        ];
        let url = "https://x/watch?v=1&a=$(rm -rf /)"; // stays one literal arg
        let (cmd, args) = build_argv(&p, url, 720);
        assert_eq!(cmd, "true");
        assert_eq!(args, vec!["-f".to_string(), "b[height<=720]".into(), url.into()]);
    }

    #[test]
    fn sweep_removes_only_service_leftovers() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep.txt"), b"x").unwrap();
        std::fs::write(dir.path().join(".xr-part-1"), b"x").unwrap();
        std::fs::create_dir(dir.path().join(".xr-import-9")).unwrap();
        std::fs::write(dir.path().join(".xr-import-9/half.mp4"), b"x").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/.xr-part-2"), b"x").unwrap();
        std::fs::write(dir.path().join("sub/data.bin"), b"x").unwrap();

        sweep_share_root(dir.path());

        assert!(dir.path().join("keep.txt").exists());
        assert!(dir.path().join("sub/data.bin").exists());
        assert!(!dir.path().join(".xr-part-1").exists());
        assert!(!dir.path().join(".xr-import-9").exists());
        assert!(!dir.path().join("sub/.xr-part-2").exists());
    }
}
