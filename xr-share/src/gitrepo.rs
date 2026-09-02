//! The agent-side repository of a share's git contour (LLD-33 п. 2.2).
//!
//! The repository lives in the agent's state dir (`<state_dir>/git/<share_id>`
//! as `GIT_DIR` with `core.worktree` pointed at the share folder), so the
//! working folder stays clean: no `.git`, no service files, and the owner can
//! keep editing it in any editor without ever seeing git. Every change of the
//! folder passes through the same loop no matter where it came from (an edit,
//! `PUT`/`DELETE`, an import publish, a future move): a `notify` watcher with
//! a debounce plus a safety scan every few minutes (watchers on network
//! filesystems miss events), then `git add -A` + `git commit` by spawning the
//! system git.
//!
//! Excluded from history: the reserved `.xr-` service namespace and files over
//! the `git_max_file_mb` cap (they keep flowing through the manifest surface,
//! they just never enter history). Auto-commit and an incoming push serialize
//! on one mutex per share, and every HEAD change feeds a `watch` channel the
//! `/git/head` long-poll rides on (LLD-33 п. 2.4).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use notify::Watcher;
use tokio::sync::{watch, Mutex};

use crate::config::AgentConfig;
use crate::server::SharesMap;

/// Quiet period after the last filesystem event before a commit runs
/// (LLD-33 п. 2.2). Long enough to let an editor finish a save (write + rename
/// + fsync), short enough that a co-author's fetch sees the change in seconds.
pub const AUTOCOMMIT_DEBOUNCE: Duration = Duration::from_secs(2);
/// Safety scan period: the watcher is not trustworthy on network filesystems,
/// so a periodic full pass catches anything it missed (LLD-33 risk 5).
pub const SAFETY_SCAN_EVERY: Duration = Duration::from_secs(5 * 60);
/// Marker line of the agent-managed block in `$GIT_DIR/info/exclude`. Lines
/// below it are rewritten from the oversize scan on every commit; lines above
/// it (`.xr-*`) belong to the repo setup and are never touched again.
const EXCLUDE_MANAGED_MARKER: &str = "# xr-share: files over git_max_file_mb, managed";
/// Marker of the whole reserved service namespace (LLD-29): unfinished uploads,
/// import job dirs. Never enters history.
const EXCLUDE_SERVICE: &str = ".xr-*";

/// Where the repository of `share_id` lives under the agent's state dir. The
/// id comes from the hub (base64url of random bytes), but the config is
/// hand-editable, so it still has to prove it makes a safe path component
/// before it becomes one.
pub fn git_dir_for(state_dir: &Path, share_id: &str) -> Result<PathBuf> {
    let ok = !share_id.is_empty()
        && share_id.len() <= 64
        && share_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !ok {
        bail!("share_id {share_id:?} does not work as a repository directory name");
    }
    Ok(state_dir.join("git").join(share_id))
}

/// Author identity of the agent's auto-commits and the size cap, resolved once
/// from the config so every commit sees the same values.
#[derive(Debug, Clone, PartialEq)]
pub struct GitSettings {
    /// `(name, email)` in git's terms.
    pub author: (String, String),
    /// Per-file history cap in mebibytes (LLD-33 п. 2.6).
    pub max_file_mb: u64,
}

impl GitSettings {
    pub fn from_config(cfg: &AgentConfig) -> Self {
        Self {
            author: author_pair(cfg.git_author.as_deref()),
            max_file_mb: cfg.effective_git_max_file_mb(),
        }
    }
}

/// The commit author: `git_author` from the config if present (either a bare
/// name or a full `Name <email>` string), else the hostname with a fixed
/// domain. Co-authors should see whose machine pushed a change without anyone
/// setting anything up.
fn author_pair(configured: Option<&str>) -> (String, String) {
    let host = hostname();
    let Some(given) = configured.map(str::trim).filter(|s| !s.is_empty()) else {
        return (host.clone(), format!("{host}@xr-share"));
    };
    if let (Some(start), Some(end)) = (given.find('<'), given.rfind('>')) {
        let name = given[..start].trim();
        let email = &given[start + 1..end];
        if !name.is_empty() && !email.trim().is_empty() {
            return (name.to_string(), email.trim().to_string());
        }
    }
    let name = given.to_string();
    let user: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c.to_lowercase().next().unwrap_or('-')
            } else {
                '-'
            }
        })
        .collect();
    let user = user.trim_matches('-').to_string();
    let user = if user.is_empty() { host.clone() } else { user };
    (name, format!("{user}@xr-share"))
}

/// The machine's hostname, best effort: `gethostname` on unix, `COMPUTERNAME`
/// on Windows, a stable fallback when neither works. Only used as a display
/// name in history, never as an address.
fn hostname() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        if unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) } == 0 {
            let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
            if let Ok(name) = std::str::from_utf8(&buf[..end]) {
                if !name.is_empty() {
                    return name.trim_start_matches("ipv6:").to_string();
                }
            }
        }
    }
    #[cfg(windows)]
    {
        if let Ok(name) = std::env::var("COMPUTERNAME") {
            if !name.is_empty() {
                return name;
            }
        }
    }
    "xr-share".to_string()
}

/// One share's git contour: the repository, its settings and the HEAD channel.
/// Cheap to hold behind an `Arc`; every spawned git command is synchronous and
/// short-lived.
pub struct GitShare {
    pub share_id: String,
    /// `GIT_DIR` of the repository (inside the agent state dir, not the share).
    pub git_dir: PathBuf,
    /// The share's canonical root, wired into the repo as `core.worktree`.
    pub worktree: PathBuf,
    settings: GitSettings,
    /// Current `refs/heads/main` SHA (empty string before the first commit).
    /// Fed by auto-commit and receive-pack, drained by the `/git/head`
    /// long-poll (LLD-33 п. 2.4).
    pub head_tx: watch::Sender<String>,
    /// Serializes the commit loop against an incoming push (LLD-33 п. 2.2):
    /// while receive-pack materializes into the working folder the committer
    /// waits, and vice versa.
    pub op_lock: Arc<Mutex<()>>,
    /// Cleared when the share leaves the config (hot reload), stopping its
    /// autocommit task.
    alive_tx: watch::Sender<bool>,
}

impl GitShare {
    /// Open the repository of a git-enabled share, creating it on first sight.
    /// Re-running on an existing repo re-points `core.worktree` (the share path
    /// may have been re-registered) and refreshes the settings-derived config,
    /// then reads the current HEAD into the channel.
    pub fn open(
        state_dir: &Path,
        share_id: &str,
        worktree: &Path,
        settings: GitSettings,
    ) -> Result<Arc<Self>> {
        let git_dir = git_dir_for(state_dir, share_id)?;
        let share = Arc::new(Self {
            share_id: share_id.to_string(),
            git_dir: git_dir.clone(),
            worktree: worktree.to_path_buf(),
            settings,
            head_tx: watch::Sender::new(String::new()),
            op_lock: Arc::new(Mutex::new(())),
            alive_tx: watch::Sender::new(true),
        });
        if !is_repo(&git_dir) {
            share.init_repo()?;
            tracing::info!("share {share_id}: git repository created at {}", git_dir.display());
        }
        share.refresh_repo_config()?;
        share.publish_head(&share.current_head_blocking());
        Ok(share)
    }

    /// A minimal structural check: `git init` leaves `HEAD`, `objects` and
    /// `refs` behind, and an incomplete directory from a crashed first init is
    /// not a repo we can serve.
    fn is_repo_check(&self) -> bool {
        is_repo(&self.git_dir)
    }

    /// Create the repository outside the working folder (LLD-33 п. 2.2): a
    /// bare-style `GIT_DIR` layout with `core.bare = false` and
    /// `core.worktree` pointed at the share, so the folder carries no `.git`.
    /// Push acceptance and materialization are wired through receive hooks
    /// (see [`Self::write_hooks`]), not `receive.denyCurrentBranch`.
    fn init_repo(&self) -> Result<()> {
        std::fs::create_dir_all(self.git_dir.parent().context("git dir has no parent")?)
            .context("creating git state dir")?;
        // `git -C` needs an existing directory, and the bare-style layout is
        // created in place: an empty dir of our own, not the share folder.
        std::fs::create_dir_all(&self.git_dir)
            .with_context(|| format!("creating {}", self.git_dir.display()))?;
        self.git(&["init", "--bare", "--quiet", "."])
            .with_context(|| format!("git init {}", self.git_dir.display()))?;
        self.git(&["symbolic-ref", "HEAD", "refs/heads/main"])?;
        let exclude = self.git_dir.join("info").join("exclude");
        if let Some(dir) = exclude.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        std::fs::write(&exclude, format!("{EXCLUDE_SERVICE}\n")).with_context(|| {
            format!("writing {}", exclude.display())
        })?;
        self.refresh_repo_config()
    }

    /// Settings-derived repo config: worktree pointer, push policy and the
    /// receive size ceiling. Rewritten on every open so a config change
    /// (a moved share, a new cap) reaches an existing repository without a
    /// re-init. `receive.denyCurrentBranch` stays `ignore`: git's own
    /// `updateInstead` resolves the deploy worktree as the `GIT_DIR` itself
    /// when receive-pack takes the repo by path (enter_repo semantics), so for
    /// the bare-style layout it would refuse every push with «unstaged
    /// changes» measured against the service files. The cleanliness check and
    /// the materialization are ours instead, in the receive hooks below.
    fn refresh_repo_config(&self) -> Result<()> {
        let worktree = self
            .worktree
            .to_str()
            .context("share path is not valid UTF-8, the git contour cannot follow it")?;
        // Push acceptance ceiling with headroom over the per-file cap: a pack
        // legitimately carries many files, so it is sized in multiples of the
        // cap plus a constant for the object framing, while a runaway push of
        // something huge is still cut before it eats the disk. The setting is
        // counted in bytes, so the mebibytes turn into bytes here.
        let max_input = self
            .settings
            .max_file_mb
            .saturating_mul(8)
            .saturating_add(64)
            .saturating_mul(1024 * 1024);
        self.git(&["config", "core.bare", "false"])?;
        self.git(&["config", "core.worktree", worktree])?;
        self.git(&["config", "receive.denyCurrentBranch", "ignore"])?;
        self.git(&["config", "receive.denyNonFastForwards", "true"])?;
        self.git(&["config", "receive.denyDeletes", "true"])?;
        self.git(&["config", "receive.maxInputSize", &max_input.to_string()])?;
        self.write_hooks()
    }

    /// The push half of the contour (LLD-33 п. 3.4), as receive hooks inside
    /// the repository: `pre-receive` refuses a push into a dirty working
    /// folder (git's `updateInstead` semantics, which git itself cannot run on
    /// this layout), `post-receive` materializes the accepted `main` into the
    /// folder with `read-tree -u`. Both run with `GIT_DIR` set by receive-pack
    /// and the worktree taken from `core.worktree`, so plain `git` calls see
    /// the share folder. Rewritten on every open, like the config above.
    fn write_hooks(&self) -> Result<()> {
        let hooks = self.git_dir.join("hooks");
        std::fs::create_dir_all(&hooks)
            .with_context(|| format!("creating {}", hooks.display()))?;
        let pre = "#!/bin/sh\n\
             # xr-share: push materializes into the working folder; a dirty one refuses.\n\
             while read -r old new ref; do\n\
             \x20   case \"$ref\" in refs/heads/*) ;; *) continue ;; esac\n\
             \x20   if ! git diff-files --quiet --ignore-submodules -- || \\\n\
             \x20      ! git diff-index --quiet --cached --ignore-submodules HEAD --; then\n\
             \x20       echo \"xr-share: working folder has uncommitted changes, push rejected\" >&2\n\
             \x20       exit 1\n\
             \x20   fi\n\
             done\n\
             exit 0\n";
        let post = "#!/bin/sh\n\
             # xr-share: materialize the accepted main into the working folder.\n\
             while read -r old new ref; do\n\
             \x20   case \"$ref\" in\n\
             \x20       refs/heads/main) exec git read-tree -u --reset HEAD ;;\n\
             \x20   esac\n\
             done\n\
             exit 0\n";
        for (name, body) in [("pre-receive", pre), ("post-receive", post)] {
            let path = hooks.join(name);
            std::fs::write(&path, body)
                .with_context(|| format!("writing {}", path.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                    .with_context(|| format!("chmod {}", path.display()))?;
            }
        }
        Ok(())
    }

    /// Run git against this repository synchronously and fail on a nonzero
    /// exit. Every call is short-lived; the long-running transports live in
    /// `server.rs` and spawn their own processes.
    fn git(&self, args: &[&str]) -> Result<std::process::Output> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&self.git_dir)
            .args(args)
            .output()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    anyhow::anyhow!("git not found in PATH, the git contour needs it on the agent")
                } else {
                    anyhow::Error::new(e).context("spawning git")
                }
            })?;
        if !out.status.success() {
            bail!(
                "git {} failed: {}",
                args.first().unwrap_or(&""),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(out)
    }

    /// Current `HEAD` SHA, or an empty string before the first commit (an
    /// unborn `main`): the channel's neutral element, signed and served like
    /// any other so the long-poll handshake works on a fresh share.
    pub fn current_head_blocking(&self) -> String {
        self.git(&["rev-parse", "--verify", "HEAD"])
            .ok()
            .and_then(|out| String::from_utf8(out.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    }

    /// Move the HEAD channel if the value actually changed, waking exactly the
    /// long-polls that wait for it.
    pub fn publish_head(&self, head: &str) {
        self.head_tx.send_if_modified(|old| {
            if old != head {
                *old = head.to_string();
                true
            } else {
                false
            }
        });
    }

    /// One full commit pass over the working folder, blocking (LLD-33 п. 2.2).
    /// Stage everything except the exclusions, commit if anything is staged,
    /// return the new HEAD (`None` when there was nothing to commit). Called
    /// with `op_lock` held: from the autocommit loop via [`Self::commit_scan`]
    /// and after a receive-pack has finished.
    pub fn commit_all_blocking(&self) -> Result<Option<String>> {
        if !self.is_repo_check() {
            bail!("repository at {} is gone", self.git_dir.display());
        }
        let oversize = self.scan_oversize()?;
        self.write_managed_exclude(&oversize)?;

        // Candidates come from `git status`, not a blanket `add .`: naming the
        // changed paths explicitly keeps git's "explicit pathspec matches an
        // ignored file" refusal out of the way (the excluded files are ignored
        // by `info/exclude`, so `status` already hides the untracked ones).
        // A tracked file that grew over the cap still shows up here and is
        // dropped by the oversize filter, so only its old size stays indexed.
        let status = self.git(&["status", "--porcelain", "-z", "--untracked-files=all"])?;
        let mut candidates: Vec<String> = Vec::new();
        for record in status.stdout.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            // Record layout: `XY <path>` (raw path, `-z` disables quoting); a
            // rename carries `orig -> path`, the right side is the live one.
            if record.len() < 4 {
                continue;
            }
            let path = String::from_utf8_lossy(&record[3..]).into_owned();
            let path = path.rsplit(" -> ").next().unwrap_or(&path);
            if !oversize.iter().any(|o| o == path) {
                candidates.push(path.to_string());
            }
        }
        if candidates.is_empty() {
            return Ok(None);
        }
        // Chunked explicit adds keep argv within sane bounds on a big folder.
        for chunk in candidates.chunks(500) {
            let mut args: Vec<String> = vec!["add".into(), "-A".into(), "--".into()];
            args.extend(chunk.iter().cloned());
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            self.git(&args)?;
        }

        let staged = self.git(&["diff", "--cached", "--name-only", "-z"])?;
        let names: Vec<String> = staged
            .stdout
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect();
        if names.is_empty() {
            return Ok(None);
        }

        let subject = commit_subject(&names);
        let (name, email) = self.settings.author.clone();
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&self.git_dir)
            .arg("-c")
            .arg("commit.gpgsign=false")
            .arg("commit")
            .arg("--no-verify")
            .arg("-m")
            .arg(&subject)
            .arg("--author")
            .arg(format!("{name} <{email}>"))
            .env("GIT_COMMITTER_NAME", &name)
            .env("GIT_COMMITTER_EMAIL", &email)
            .output()
            .context("spawning git commit")?;
        if !out.status.success() {
            bail!("git commit failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        let head = self.current_head_blocking();
        Ok(Some(head))
    }

    /// Files over the cap, as `/`-separated paths relative to the share root
    /// (git's notation). The `.xr-` namespace is excluded wholesale by
    /// `info/exclude`, so it never even reaches this scan.
    fn scan_oversize(&self) -> Result<Vec<String>> {
        let cap_bytes = self.settings.max_file_mb.saturating_mul(1024 * 1024);
        let mut oversize = Vec::new();
        for entry in walkdir::WalkDir::new(&self.worktree).follow_links(false) {
            let entry = entry.with_context(|| format!("walking {}", self.worktree.display()))?;
            if !entry.file_type().is_file() {
                continue;
            }
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else { continue };
            if meta.len() > cap_bytes {
                let rel = entry
                    .path()
                    .strip_prefix(&self.worktree)
                    .expect("walkdir yields paths under the root");
                oversize.push(rel.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/"));
            }
        }
        Ok(oversize)
    }

    /// Rewrite the managed block of `$GIT_DIR/info/exclude` from the oversize
    /// scan. Hand-written lines above the marker survive; the managed list
    /// itself is regenerated, so a file that shrank back under the cap returns
    /// to history on the next commit.
    fn write_managed_exclude(&self, oversize: &[String]) -> Result<()> {
        let exclude = self.git_dir.join("info").join("exclude");
        let current = std::fs::read_to_string(&exclude).context("reading info/exclude")?;
        let mut kept: Vec<&str> = current
            .lines()
            .take_while(|l| l.trim() != EXCLUDE_MANAGED_MARKER)
            .collect();
        if !kept.iter().any(|l| l.trim() == EXCLUDE_SERVICE) {
            kept.push(EXCLUDE_SERVICE);
        }
        let mut text = kept.join("\n");
        if oversize.is_empty() {
            // Drop a stale managed block entirely: nothing is over the cap.
            let after_marker: Vec<&str> = current
                .lines()
                .skip_while(|l| l.trim() != EXCLUDE_MANAGED_MARKER)
                .collect();
            if !after_marker.is_empty() {
                text.push('\n');
            }
        } else {
            text.push('\n');
            text.push('\n');
            text.push_str(EXCLUDE_MANAGED_MARKER);
            for path in oversize {
                text.push('\n');
                text.push_str(path);
            }
            text.push('\n');
        }
        std::fs::write(&exclude, text).with_context(|| format!("writing {}", exclude.display()))
    }

    /// A commit pass under the share lock, off the async executor. Publishing
    /// the new HEAD wakes the long-polls (LLD-33 п. 2.4).
    pub async fn commit_scan(self: Arc<Self>) -> Result<Option<String>> {
        let _guard = self.op_lock.lock().await;
        let share = Arc::clone(&self);
        let head = tokio::task::spawn_blocking(move || share.commit_all_blocking())
            .await
            .context("commit task failed")??;
        if let Some(head) = &head {
            self.publish_head(head);
        }
        Ok(head)
    }

    /// Housekeeping after a finished receive-pack (LLD-33 п. 2.2):
    /// `gc --auto` keeps the repository compact amortized, and the (possibly
    /// moved) HEAD wakes the long-polls. Runs with `op_lock` still held by the
    /// transport, keeping the commit loop out until the push is fully done.
    pub async fn after_receive(self: Arc<Self>) -> Result<()> {
        let share = Arc::clone(&self);
        let head = tokio::task::spawn_blocking(move || -> Result<String> {
            share.git(&["gc", "--auto"]).ok(); // best effort, never fails the push
            Ok(share.current_head_blocking())
        })
        .await
        .context("gc task failed")??;
        self.publish_head(&head);
        Ok(())
    }

    /// Run the autocommit loop for this share: watcher events with a debounce
    /// plus the safety scan, until the share leaves the config. A watcher that
    /// cannot be installed (exotic filesystem) degrades to scan-only instead
    /// of silently stopping the contour.
    pub fn spawn_autocommit(self: &Arc<Self>, debounce: Duration, scan_every: Duration) {
        let share = Arc::clone(self);
        let mut alive = share.alive_tx.subscribe();
        tokio::spawn(async move {
            // Events cross the sync/async boundary through a std channel
            // drained by a plain thread: notify delivers synchronously and a
            // blocking read has no place on the runtime. The watcher guard
            // lives as long as this task, so the thread ends with it.
            let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(64);
            // The guard must outlive the loop below: dropping it stops event
            // delivery, so park it in an underscore binding (which keeps the
            // value, unlike a bare `_`).
            let mut watcher = None;
            match notify::recommended_watcher(move |_: notify::Result<notify::Event>| {
                // Only the fact of an event matters, not its payload: the
                // commit pass rescans the folder anyway.
                let _ = tx.blocking_send(());
            }) {
                Ok(mut w) => {
                    if let Err(e) = w.watch(&share.worktree, notify::RecursiveMode::Recursive) {
                        tracing::warn!(
                            "share {}: fs watcher unavailable ({}), falling back to periodic scan",
                            share.share_id,
                            e
                        );
                    } else {
                        watcher = Some(w);
                    }
                }
                Err(e) => tracing::warn!(
                    "share {}: fs watcher unavailable ({e}), falling back to periodic scan",
                    share.share_id
                ),
            }
            let _watcher = watcher;
            let mut pending = false;
            let mut quiet = Box::pin(tokio::time::sleep(debounce));
            let mut scan = tokio::time::interval_at(tokio::time::Instant::now() + scan_every, scan_every);
            loop {
                tokio::select! {
                    ev = rx.recv() => match ev {
                        Some(()) => {
                            pending = true;
                            quiet.as_mut().reset(tokio::time::Instant::now() + debounce);
                        }
                        None => break,
                    },
                    _ = &mut quiet, if pending => {
                        pending = false;
                        if let Err(e) = share.clone().commit_scan().await {
                            tracing::warn!("share {}: auto-commit failed: {e:#}", share.share_id);
                        }
                    }
                    _ = scan.tick() => {
                        if let Err(e) = share.clone().commit_scan().await {
                            tracing::warn!("share {}: safety scan commit failed: {e:#}", share.share_id);
                        }
                    }
                    changed = alive.changed() => match changed {
                        Ok(()) if !*alive.borrow() => break,
                        Ok(()) => {}
                        Err(_) => break,
                    },
                }
            }
            tracing::debug!("share {}: auto-commit loop stopped", share.share_id);
        });
    }
}

fn is_repo(dir: &Path) -> bool {
    dir.join("HEAD").is_file() && dir.join("objects").is_dir() && dir.join("refs").is_dir()
}

/// Commit subject from the staged paths: up to three named, the rest counted
/// (LLD-33 п. 2.2). Paths are squashed to one line so the subject stays one.
fn commit_subject(paths: &[String]) -> String {
    let clean = |p: &str| p.replace(['\n', '\r'], " ");
    let shown: Vec<String> = paths.iter().take(3).map(|p| clean(p)).collect();
    let mut subject = shown.join(", ");
    if paths.len() > 3 {
        subject.push_str(&format!(" и ещё {}", paths.len() - 3));
    }
    subject
}

/// The table of live git shares. Rebuilt on every config (re)load: new
/// repositories open, new autocommit loops spawn, retired shares stop. A
/// repository that fails to open logs and stays absent: its routes answer 500
/// and the rest of the agent keeps working.
pub struct GitManager {
    state_dir: PathBuf,
    settings: GitSettings,
    shares: RwLock<HashMap<String, Arc<GitShare>>>,
}

impl GitManager {
    pub fn new(state_dir: &Path, cfg: &AgentConfig) -> Self {
        Self::with_settings(state_dir, GitSettings::from_config(cfg))
    }

    /// The same manager with settings resolved by hand (tests build the table
    /// without a full agent config).
    pub fn with_settings(state_dir: &Path, settings: GitSettings) -> Self {
        Self { state_dir: state_dir.to_path_buf(), settings, shares: RwLock::new(HashMap::new()) }
    }

    /// Sync the live table with a freshly built share map: open/init the
    /// repositories of git-enabled shares, spawn their autocommit loops, stop
    /// the loops of shares that dropped out (a disabled or unshared share
    /// stops committing; its repository stays on disk, LLD-33 п. 2.1).
    ///
    /// An unchanged share keeps its running instance: the config is rewritten
    /// by any `share`/`expose` call, and a re-opened instance would detach the
    /// live contour, leaving the transport and the `/git/head` long-poll on a
    /// fresh HEAD channel while the old autocommit loop keeps feeding the
    /// dropped one. Only a changed path or changed settings restart the loop.
    pub fn rebuild(&self, map: &SharesMap) {
        let current: HashMap<String, Arc<GitShare>> = {
            let live = self.shares.read().expect("git table lock poisoned");
            live.clone()
        };
        let mut next: HashMap<String, Arc<GitShare>> = HashMap::new();
        for (id, root) in map.iter().filter(|(_, root)| root.git) {
            if let Some(old) = current.get(id) {
                if old.worktree == root.path && old.settings == self.settings {
                    next.insert(id.clone(), Arc::clone(old));
                    continue;
                }
                tracing::info!("share {id}: git contour reconfigured, auto-commit restarted");
                let _ = old.alive_tx.send(false);
            }
            match GitShare::open(&self.state_dir, id, &root.path, self.settings.clone()) {
                Ok(share) => {
                    tracing::info!("share {id}: git contour on, auto-commit running");
                    share.spawn_autocommit(AUTOCOMMIT_DEBOUNCE, SAFETY_SCAN_EVERY);
                    next.insert(id.clone(), share);
                }
                Err(e) => tracing::error!("share {id}: git repository unavailable: {e:#}"),
            }
        }
        let mut live = self.shares.write().expect("git table lock poisoned");
        for (id, old) in live.iter() {
            if !next.contains_key(id) {
                tracing::info!("share {id}: git contour off, auto-commit stopped");
                let _ = old.alive_tx.send(false);
            }
        }
        *live = next;
    }

    pub fn get(&self, share_id: &str) -> Option<Arc<GitShare>> {
        self.shares
            .read()
            .expect("git table lock poisoned")
            .get(share_id)
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> GitSettings {
        GitSettings {
            author: ("тест".into(), "test@xr-share".into()),
            max_file_mb: 1,
        }
    }

    fn open_share(dir: &Path, share_id: &str) -> Arc<GitShare> {
        let worktree = dir.join("worktree");
        std::fs::create_dir_all(&worktree).unwrap();
        GitShare::open(dir, share_id, &worktree, settings()).expect("open")
    }

    #[test]
    fn test_git_share_optin_init() {
        let dir = tempfile::tempdir().unwrap();
        let share = open_share(dir.path(), "abc123");
        assert!(is_repo(&share.git_dir));
        // Bare-style layout outside the folder, the folder itself stays clean.
        assert!(!share.worktree.join(".git").exists());
        let cfg = std::fs::read_to_string(share.git_dir.join("config")).unwrap();
        assert!(cfg.contains("worktree"));
        // Push policy: dirty-refusal and materialization live in the receive
        // hooks, not in updateInstead (it cannot run on this layout).
        assert!(cfg.contains("denyCurrentBranch = ignore"));
        assert!(cfg.contains("denyNonFastForwards = true"));
        assert!(cfg.contains("denyDeletes = true"));
        assert!(cfg.contains("maxInputSize"));
        let pre = std::fs::read_to_string(share.git_dir.join("hooks/pre-receive")).unwrap();
        assert!(pre.contains("push rejected"));
        let post = std::fs::read_to_string(share.git_dir.join("hooks/post-receive")).unwrap();
        assert!(post.contains("read-tree"));
        // The service namespace is excluded from the very start.
        let exclude = std::fs::read_to_string(share.git_dir.join("info/exclude")).unwrap();
        assert!(exclude.contains(".xr-*"));
        // Branch is main.
        let head = std::fs::read_to_string(share.git_dir.join("HEAD")).unwrap();
        assert_eq!(head.trim(), "ref: refs/heads/main");
    }

    #[test]
    fn test_git_share_optin_first_commit() {
        let dir = tempfile::tempdir().unwrap();
        let share = open_share(dir.path(), "abc123");
        std::fs::write(share.worktree.join("заметки.md"), "привет").unwrap();
        let head = share.commit_all_blocking().expect("commit").expect("head");
        assert!(!head.is_empty());
        assert_eq!(share.current_head_blocking(), head);
        // Nothing new -> no commit.
        assert!(share.commit_all_blocking().unwrap().is_none());
    }

    #[test]
    fn test_autocommit_filters() {
        let dir = tempfile::tempdir().unwrap();
        let share = open_share(dir.path(), "abc123");
        std::fs::write(share.worktree.join("a.md"), "a").unwrap();
        share.commit_all_blocking().unwrap().unwrap();

        // Service namespace: never in history.
        std::fs::write(share.worktree.join(".xr-part-tmp"), "half").unwrap();
        // Oversize: 1 MiB cap, write 2 MiB.
        let big = vec![b'x'; 2 * 1024 * 1024];
        std::fs::write(share.worktree.join("big.bin"), &big).unwrap();
        std::fs::write(share.worktree.join("b.md"), "b").unwrap();
        share.commit_all_blocking().unwrap().unwrap();

        let out = share.git(&["ls-files"]).unwrap();
        let listed = String::from_utf8(out.stdout).unwrap();
        assert!(listed.contains("a.md") && listed.contains("b.md"), "{listed}");
        assert!(!listed.contains("big.bin"), "{listed}");
        assert!(!listed.contains(".xr-part-tmp"), "{listed}");
        // The cap landed in the managed exclude block.
        let exclude = std::fs::read_to_string(share.git_dir.join("info/exclude")).unwrap();
        assert!(exclude.contains(EXCLUDE_MANAGED_MARKER), "{exclude}");
        assert!(exclude.contains("big.bin"), "{exclude}");

        // Subject names paths and counts the rest.
        for i in 0..5 {
            std::fs::write(share.worktree.join(format!("f{i}.md")), i.to_string()).unwrap();
        }
        share.commit_all_blocking().unwrap().unwrap();
        let out = share.git(&["log", "-1", "--pretty=%s"]).unwrap();
        let subject = String::from_utf8(out.stdout).unwrap();
        assert!(subject.contains("f0.md") && subject.contains("f2.md"), "{subject}");
        assert!(subject.contains("и ещё 2"), "{subject}");

        // A file that shrank back under the cap returns to history.
        std::fs::write(share.worktree.join("big.bin"), "small now").unwrap();
        share.commit_all_blocking().unwrap().unwrap();
        let out = share.git(&["ls-files"]).unwrap();
        let listed = String::from_utf8(out.stdout).unwrap();
        assert!(listed.contains("big.bin"), "{listed}");
    }

    #[test]
    fn test_git_share_reopen_reuses_repo() {
        let dir = tempfile::tempdir().unwrap();
        {
            let share = open_share(dir.path(), "abc123");
            std::fs::write(share.worktree.join("a.md"), "a").unwrap();
            share.commit_all_blocking().unwrap().unwrap();
        }
        let again = open_share(dir.path(), "abc123");
        assert!(!again.current_head_blocking().is_empty(), "история пережила переоткрытие");
        // HEAD channel is primed with the persisted head right away.
        assert_eq!(*again.head_tx.borrow(), again.current_head_blocking());
    }

    #[test]
    fn test_git_dir_rejects_unsafe_ids() {
        let dir = tempfile::tempdir().unwrap();
        assert!(git_dir_for(dir.path(), "ok_id-123").is_ok());
        assert!(git_dir_for(dir.path(), "").is_err());
        assert!(git_dir_for(dir.path(), "../escape").is_err());
        assert!(git_dir_for(dir.path(), "a/b").is_err());
        let long = "x".repeat(65);
        assert!(git_dir_for(dir.path(), &long).is_err());
    }

    #[test]
    fn author_pair_shapes() {
        assert_eq!(
            author_pair(Some("Вася <vasya@example.org>")),
            ("Вася".into(), "vasya@example.org".into())
        );
        assert_eq!(author_pair(Some("Вася")), ("Вася".into(), "вася@xr-share".into()));
        let (name, email) = author_pair(None);
        assert!(!name.is_empty());
        assert!(email.ends_with("@xr-share"), "{email}");
    }

    #[tokio::test]
    async fn commit_scan_updates_head_channel() {
        let dir = tempfile::tempdir().unwrap();
        let share = open_share(dir.path(), "abc123");
        assert!(share.head_tx.borrow().is_empty(), "пустой репозиторий начинается с пустым HEAD");
        std::fs::write(share.worktree.join("a.md"), "a").unwrap();
        let head = share.clone().commit_scan().await.unwrap().expect("committed");
        assert_eq!(*share.head_tx.borrow(), head);
    }

    /// Клиентский git с герметичным конфигом: ни глобального конфига машины,
    /// ни её identity, чтобы тест не зависел от окружения.
    fn client_git(dir: &Path, args: &[&str]) -> (bool, String) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "client")
            .env("GIT_AUTHOR_EMAIL", "client@example.com")
            .env("GIT_COMMITTER_NAME", "client")
            .env("GIT_COMMITTER_EMAIL", "client@example.com")
            .output()
            .expect("git runs");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.success(), text)
    }

    #[test]
    fn test_updateinstead_dirty_rejects_push() {
        let dir = tempfile::tempdir().unwrap();
        let client = tempfile::tempdir().unwrap();
        let share = open_share(dir.path(), "abc123");
        std::fs::write(share.worktree.join("a.txt"), "v1").unwrap();
        let _head = share.commit_all_blocking().unwrap().expect("first commit");

        // Клон прямо из GIT_DIR: file-транспорт зовёт тот же receive-pack,
        // что и HTTP-контур, watcher не запускался и рабочую папку не чистит.
        let repo = client.path().join("repo");
        let url = share.git_dir.display().to_string();
        let (ok, text) = client_git(client.path(), &["clone", &url, "repo"]);
        assert!(ok, "{text}");

        // Рабочая папка агента грязная по отслеживаемому файлу: pre-receive
        // обязан отказать, а не перетирать чужие правки материаллизацией.
        std::fs::write(share.worktree.join("a.txt"), "черновик").unwrap();
        std::fs::write(repo.join("b.txt"), "от клиента").unwrap();
        client_git(&repo, &["add", "-A"]);
        let (ok, text) = client_git(&repo, &["commit", "-m", "client edit"]);
        assert!(ok, "{text}");
        let (ok, text) = client_git(&repo, &["push", "origin", "HEAD:refs/heads/main"]);
        assert!(!ok, "push в грязную папку прошёл: {text}");
        assert_eq!(std::fs::read(share.worktree.join("a.txt")).unwrap(), "черновик".as_bytes());
        assert!(!share.worktree.join("b.txt").exists());
    }

    #[test]
    fn rebuild_keeps_channel_across_unrelated_config_reload() {
        let state = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        std::fs::write(work.path().join("a.md"), "заметка").unwrap();
        let root = |path: &Path| crate::server::ShareRoot {
            path: path.to_path_buf(),
            is_file: false,
            writable: true,
            import: false,
            git: true,
        };
        let mgr = GitManager::with_settings(state.path(), settings());
        let mut map = crate::server::SharesMap::new();
        map.insert("W".into(), root(work.path()));

        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            mgr.rebuild(&map);
        });
        let w1 = mgr.get("W").unwrap();
        let rx = w1.head_tx.subscribe();

        // Конфиг переписала чужая шара: W обязана остаться тем же экземпляром,
        // иначе long-poll (подписка на прежний канал) глухнет, а авто-коммит
        // кормит уже снятый с таблицы объект.
        let other = tempfile::tempdir().unwrap();
        map.insert("X".into(), root(other.path()));
        rt.block_on(async {
            mgr.rebuild(&map);
        });
        let w2 = mgr.get("W").unwrap();
        assert!(
            Arc::ptr_eq(&w1, &w2),
            "перезагрузка конфига подменила живой экземпляр шары W"
        );

        // Коммит после перезагрузки будит подписку, взятую до неё.
        std::fs::write(work.path().join("b.md"), "вторая").unwrap();
        rt.block_on(async {
            let head = w2.clone().commit_scan().await.unwrap();
            assert!(head.is_some(), "правка b.md обязана коммититься");
        });
        assert!(rx.has_changed().unwrap(), "канал HEAD не двинулся");

        // Смена папки это сознательный рестарт: новый экземпляр, не тот же.
        let moved = tempfile::tempdir().unwrap();
        map.remove("X");
        map.insert("W".into(), root(moved.path()));
        rt.block_on(async {
            mgr.rebuild(&map);
        });
        let w3 = mgr.get("W").unwrap();
        assert!(!Arc::ptr_eq(&w1, &w3), "смена папки должна перезапустить контур");
    }
}
