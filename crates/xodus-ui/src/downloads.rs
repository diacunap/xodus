//! Download manager: runs `xodus-cli streaming` one job at a time (FIFO queue),
//! tracks its JSON progress lines, persists the job list and scans installed games.
//!
//! `streaming` has no resume: a cancelled/failed job restarts from zero.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Notify;

use crate::catalog::{self, Error};

/// stderr/stdout lines kept to explain a failure.
const TAIL: usize = 20;
/// Written by `streaming` only once the whole download succeeded.
const DONE_MARKER: &str = ".xodus-streaming.msixvc";
const TMP_MARKER: &str = ".xodus-streaming-tmp.msixvc";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Downloading,
    Done,
    Failed,
    Cancelled,
    Interrupted,
}

impl JobState {
    fn is_pending(self) -> bool {
        matches!(self, JobState::Queued | JobState::Downloading)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Progress {
    pub done: u64,
    pub total: u64,
    pub status: String,
    /// Smoothed bytes per second.
    pub speed: f64,
    pub eta_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub product_id: String,
    pub title: String,
    pub image: Option<String>,
    pub dest: PathBuf,
    pub state: JobState,
    pub error: Option<String>,
    /// Unix seconds.
    pub started_at: Option<u64>,
    pub finished_at: Option<u64>,
    #[serde(default)]
    pub progress: Progress,
}

#[derive(Deserialize)]
pub struct StartRequest {
    pub product_id: String,
    pub title: String,
    pub image: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Installed {
    pub product_id: Option<String>,
    pub title: String,
    pub image: Option<String>,
    pub path: PathBuf,
    pub size_bytes: u64,
}

#[derive(Default)]
struct State {
    jobs: Vec<Job>,
    /// Signals the running job to stop.
    cancel: Option<Arc<Notify>>,
    /// Installed-game cache: dir -> (marker mtime, entry), to avoid re-walking
    /// big folders on every poll.
    installed: HashMap<PathBuf, (SystemTime, Installed)>,
    /// product id -> (title, image) from the public catalog.
    titles: HashMap<String, (String, Option<String>)>,
}

pub struct Downloads {
    cli: PathBuf,
    root: PathBuf,
    store: PathBuf,
    state: Mutex<State>,
}

impl Downloads {
    /// Load the persisted job list; jobs that were running (or waiting) when
    /// the app quit become "interrupted" so nothing big starts on launch.
    pub fn load(cli: PathBuf) -> Arc<Self> {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let store = home.join("Library/Application Support/Xodus/downloads.json");
        let mut jobs: Vec<Job> = std::fs::read(&store)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        for job in &mut jobs {
            if job.state.is_pending() {
                job.state = JobState::Interrupted;
                job.progress.speed = 0.0;
                job.progress.eta_secs = None;
            }
        }
        let this = Arc::new(Self {
            cli,
            root: home.join("Games/xodus"),
            store,
            state: Mutex::new(State { jobs, ..Default::default() }),
        });
        this.persist(&this.state.lock().unwrap());
        this
    }

    pub async fn list(&self, client: &reqwest::Client) -> Result<Value, Error> {
        let jobs = self.state.lock().unwrap().jobs.clone();
        let installed = self.installed(client).await;
        tracing::debug!(
            jobs = jobs.len(),
            installed = ?installed.iter().map(|i| (&i.product_id, &i.path)).collect::<Vec<_>>(),
            "downloads listed"
        );
        Ok(json!({ "jobs": jobs, "installed": installed, "root": self.root }))
    }

    /// Queue a download (or restart a finished/failed one from zero).
    pub fn start(self: &Arc<Self>, req: StartRequest) -> Result<(), Error> {
        {
            let mut st = self.state.lock().unwrap();
            if let Some(pos) = st.jobs.iter().position(|j| j.product_id == req.product_id) {
                if st.jobs[pos].state.is_pending() {
                    return Err(format!("{} is already queued", req.title).into());
                }
                st.jobs.remove(pos);
            }
            let folder: String = req.title.chars().filter(char::is_ascii_alphanumeric).collect();
            let folder = if folder.is_empty() { req.product_id.clone() } else { folder };
            st.jobs.push(Job {
                dest: self.root.join(folder),
                product_id: req.product_id,
                title: req.title,
                image: req.image,
                state: JobState::Queued,
                error: None,
                started_at: None,
                finished_at: None,
                progress: Progress::default(),
            });
            self.persist(&st);
        }
        self.pump();
        Ok(())
    }

    pub fn cancel(&self, product_id: &str) -> Result<(), Error> {
        let mut st = self.state.lock().unwrap();
        let job = st
            .jobs
            .iter_mut()
            .find(|j| j.product_id == product_id)
            .ok_or("no such download")?;
        match job.state {
            JobState::Queued => {
                job.state = JobState::Cancelled;
                job.finished_at = Some(now());
                self.persist(&st);
            }
            // The runner marks it cancelled once the process is gone.
            JobState::Downloading => {
                if let Some(cancel) = &st.cancel {
                    cancel.notify_one();
                }
            }
            _ => return Err("download is not running".into()),
        }
        Ok(())
    }

    /// Forget a finished job. Never touches files on disk.
    pub fn remove(&self, product_id: &str) -> Result<(), Error> {
        let mut st = self.state.lock().unwrap();
        let pos = st
            .jobs
            .iter()
            .position(|j| j.product_id == product_id)
            .ok_or("no such download")?;
        if st.jobs[pos].state.is_pending() {
            return Err("cancel the download before removing it".into());
        }
        st.jobs.remove(pos);
        self.persist(&st);
        Ok(())
    }

    /// Open a game folder in Finder. Only folders we manage are allowed.
    pub async fn reveal(&self, path: &str) -> Result<(), Error> {
        let path = Path::new(path);
        let known = path.starts_with(&self.root)
            || self.state.lock().unwrap().jobs.iter().any(|j| j.dest == path);
        if !known || !path.is_dir() {
            return Err(format!("not a game folder: {}", path.display()).into());
        }
        let status = Command::new("open").arg(path).status().await?;
        if !status.success() {
            return Err(format!("open exited with {status}").into());
        }
        Ok(())
    }

    /// Start the next queued job if nothing is running.
    fn pump(self: &Arc<Self>) {
        let mut st = self.state.lock().unwrap();
        if st.cancel.is_some() {
            return;
        }
        let Some(job) = st.jobs.iter_mut().find(|j| j.state == JobState::Queued) else {
            return;
        };
        job.state = JobState::Downloading;
        job.error = None;
        job.started_at = Some(now());
        job.finished_at = None;
        job.progress = Progress { status: "Starting".into(), ..Default::default() };
        let (product_id, dest) = (job.product_id.clone(), job.dest.clone());
        let cancel = Arc::new(Notify::new());
        st.cancel = Some(cancel.clone());
        self.persist(&st);
        drop(st);

        let this = self.clone();
        tokio::spawn(async move {
            let (state, error) = this.run(&product_id, &dest, &cancel).await;
            tracing::info!(product_id, ?state, ?error, "download finished");
            {
                let mut st = this.state.lock().unwrap();
                st.cancel = None;
                if let Some(job) = st.jobs.iter_mut().find(|j| j.product_id == product_id) {
                    job.state = state;
                    job.error = error;
                    job.finished_at = Some(now());
                    job.progress.speed = 0.0;
                    job.progress.eta_secs = None;
                }
                this.persist(&st);
            }
            this.pump();
        });
    }

    /// Run one `xodus-cli streaming` process to completion.
    async fn run(&self, product_id: &str, dest: &Path, cancel: &Notify) -> (JobState, Option<String>) {
        let started = SystemTime::now();
        if let Err(err) = std::fs::create_dir_all(dest) {
            return (JobState::Failed, Some(format!("cannot create {}: {err}", dest.display())));
        }
        let spawned = Command::new(&self.cli)
            .arg("streaming")
            .arg(product_id)
            .arg(dest)
            .env("XODUS_PROGRESS_JSON", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(false)
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(err) => return (JobState::Failed, Some(format!("cannot start {}: {err}", self.cli.display()))),
        };
        tracing::info!(product_id, pid = child.id(), dest = %dest.display(), "download started");

        let tail = Arc::new(Mutex::new(VecDeque::new()));
        let stderr = child.stderr.take().map(|err| {
            let tail = tail.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(err).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    push_tail(&tail, line);
                }
            })
        });

        let mut lines = BufReader::new(child.stdout.take().expect("piped stdout")).lines();
        let mut meter = SpeedMeter::default();
        let mut cancelled = false;
        loop {
            tokio::select! {
                _ = cancel.notified() => { cancelled = true; break; }
                line = lines.next_line() => match line {
                    Ok(Some(line)) => match serde_json::from_str::<Value>(&line) {
                        Ok(v) if v.get("done").is_some() => self.on_progress(product_id, &v, &mut meter),
                        _ => push_tail(&tail, line),
                    },
                    _ => break,
                },
            }
        }
        let status = if cancelled {
            stop(&mut child).await
        } else {
            tokio::select! {
                status = child.wait() => status,
                _ = cancel.notified() => { cancelled = true; stop(&mut child).await }
            }
        };
        if let Some(stderr) = stderr {
            // The pipe closes with the process; don't hang on grandchildren.
            tokio::time::timeout(Duration::from_secs(2), stderr).await.ok();
        }
        if cancelled {
            return (JobState::Cancelled, None);
        }

        let tail = tail.lock().unwrap().iter().cloned().collect::<Vec<_>>().join("\n");
        match status {
            // `streaming` exits 0 even when it bails out early (license errors,
            // disk space...), so success also requires the completion marker.
            Ok(s) if s.success() && completed(dest, started) => (JobState::Done, None),
            Ok(s) => {
                let why = if tail.is_empty() {
                    format!("xodus-cli exited ({s}) without completing the download")
                } else {
                    tail
                };
                (JobState::Failed, Some(why))
            }
            Err(err) => (JobState::Failed, Some(format!("waiting for xodus-cli failed: {err}"))),
        }
    }

    fn on_progress(&self, product_id: &str, v: &Value, meter: &mut SpeedMeter) {
        let done = v["done"].as_u64().unwrap_or(0);
        let total = v["total"].as_u64().unwrap_or(0);
        let speed = meter.sample(done);
        let mut st = self.state.lock().unwrap();
        if let Some(job) = st.jobs.iter_mut().find(|j| j.product_id == product_id) {
            job.progress = Progress {
                done,
                total,
                status: v["status"].as_str().unwrap_or("").to_owned(),
                speed,
                eta_secs: (speed > 1.0 && total > done).then(|| ((total - done) as f64 / speed) as u64),
            };
        }
    }

    /// Persist the job list (state changes only, not progress ticks).
    fn persist(&self, st: &State) {
        let write = || -> std::io::Result<()> {
            if let Some(dir) = self.store.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = self.store.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec_pretty(&st.jobs)?)?;
            std::fs::rename(&tmp, &self.store)
        };
        if let Err(err) = write() {
            tracing::warn!("could not save {}: {err}", self.store.display());
        }
    }

    /// Folders under ~/Games/xodus that hold a completed streaming download.
    async fn installed(&self, client: &reqwest::Client) -> Vec<Installed> {
        let busy: Vec<PathBuf> = {
            let st = self.state.lock().unwrap();
            st.jobs.iter().filter(|j| j.state.is_pending()).map(|j| j.dest.clone()).collect()
        };
        let mut dirs: Vec<(PathBuf, SystemTime)> = std::fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| !busy.contains(p))
            .filter_map(|p| {
                let mtime = std::fs::metadata(p.join(DONE_MARKER)).ok()?.modified().ok()?;
                Some((p, mtime))
            })
            .collect();
        dirs.sort();

        let mut out = Vec::new();
        let mut missing = Vec::new();
        for (dir, mtime) in dirs {
            let cached = self.state.lock().unwrap().installed.get(&dir).cloned();
            let entry = match cached {
                Some((m, entry)) if m == mtime => entry,
                _ => {
                    let scan_dir = dir.clone();
                    let (product_id, size_bytes) = tokio::task::spawn_blocking(move || {
                        (store_id(&scan_dir), dir_size(&scan_dir))
                    })
                    .await
                    .unwrap_or((None, 0));
                    let entry = Installed {
                        title: dir.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                        product_id,
                        image: None,
                        path: dir.clone(),
                        size_bytes,
                    };
                    self.state.lock().unwrap().installed.insert(dir, (mtime, entry.clone()));
                    entry
                }
            };
            if let Some(id) = &entry.product_id
                && !self.state.lock().unwrap().titles.contains_key(id)
            {
                missing.push(id.clone());
            }
            out.push(entry);
        }

        if !missing.is_empty() {
            match catalog::products(client, &missing).await {
                Ok(games) => {
                    let mut st = self.state.lock().unwrap();
                    for g in games {
                        st.titles.insert(g.product_id, (g.title, g.image));
                    }
                }
                Err(err) => tracing::warn!("catalog lookup for installed games failed: {err}"),
            }
        }
        let st = self.state.lock().unwrap();
        for entry in &mut out {
            if let Some((title, image)) = entry.product_id.as_ref().and_then(|id| st.titles.get(id)) {
                entry.title = title.clone();
                entry.image = image.clone();
            }
        }
        out
    }
}

/// Exponential moving average of bytes/sec over samples >= 0.5s apart.
#[derive(Default)]
struct SpeedMeter {
    last: Option<(Instant, u64)>,
    speed: f64,
}

impl SpeedMeter {
    fn sample(&mut self, done: u64) -> f64 {
        let now = Instant::now();
        match self.last {
            None => self.last = Some((now, done)),
            Some((at, prev)) => {
                let dt = now.duration_since(at).as_secs_f64();
                if dt >= 0.5 {
                    let rate = done.saturating_sub(prev) as f64 / dt;
                    self.speed = if self.speed == 0.0 { rate } else { 0.7 * self.speed + 0.3 * rate };
                    self.last = Some((now, done));
                }
            }
        }
        self.speed
    }
}

/// SIGINT, then SIGKILL if the process is still alive after a few seconds.
async fn stop(child: &mut Child) -> std::io::Result<std::process::ExitStatus> {
    if let Some(pid) = child.id() {
        Command::new("kill").arg("-INT").arg(pid.to_string()).status().await.ok();
        if let Ok(status) = tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
            return status;
        }
    }
    child.kill().await.ok();
    child.wait().await
}

fn push_tail(tail: &Mutex<VecDeque<String>>, line: String) {
    let mut tail = tail.lock().unwrap();
    if tail.len() == TAIL {
        tail.pop_front();
    }
    tail.push_back(line);
}

fn completed(dest: &Path, started: SystemTime) -> bool {
    let fresh = std::fs::metadata(dest.join(DONE_MARKER))
        .and_then(|m| m.modified())
        .is_ok_and(|m| m + Duration::from_secs(2) >= started);
    fresh && !dest.join(TMP_MARKER).exists()
}

/// `<StoreId>` from the package's MicrosoftGame.Config (tag case-insensitive).
fn store_id(dir: &Path) -> Option<String> {
    let config = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case("MicrosoftGame.Config"))?;
    let text = std::fs::read_to_string(config.path()).ok()?;
    let lower = text.to_ascii_lowercase();
    let start = lower.find("<storeid>")? + "<storeid>".len();
    let end = start + lower[start..].find('<')?;
    let id = text[start..end].trim();
    (!id.is_empty()).then(|| id.to_owned())
}

/// Bytes on disk under `dir` (symlinks not followed).
fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total += meta.len();
            }
        }
    }
    total
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_cli(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("fake-cli");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    async fn settle(dl: &Downloads, id: &str) -> Job {
        for _ in 0..100 {
            let job = dl.state.lock().unwrap().jobs.iter().find(|j| j.product_id == id).cloned().unwrap();
            if !job.state.is_pending() {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("job {id} did not finish");
    }

    fn req(id: &str) -> StartRequest {
        StartRequest { product_id: id.into(), title: format!("Game {id}!"), image: None }
    }

    /// One test so the HOME override is not shared across threads.
    #[tokio::test]
    async fn runs_queue_and_detects_outcomes() {
        let home = std::env::temp_dir().join(format!("xodus-ui-test-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        unsafe { std::env::set_var("HOME", &home) };

        // $2 = product id, $3 = dest.
        let cli = fake_cli(&home, r#"
case "$2" in
  ok) echo '{"done":50,"total":100,"status":"Downloading"}'; echo '{"done":100,"total":100,"status":"Downloading"}'
      touch "$3/.xodus-streaming.msixvc" ;;
  bad) echo 'not entitled to this content' >&2 ;;
  slow) exec sleep 30 ;;
esac"#);
        let dl = Downloads::load(cli);

        dl.start(req("slow")).unwrap();
        dl.start(req("ok")).unwrap();
        assert_eq!(dl.state.lock().unwrap().jobs[1].state, JobState::Queued);
        tokio::time::sleep(Duration::from_millis(300)).await;
        dl.cancel("slow").unwrap();
        assert_eq!(settle(&dl, "slow").await.state, JobState::Cancelled);

        let ok = settle(&dl, "ok").await;
        assert_eq!(ok.state, JobState::Done, "{:?}", ok.error);
        assert_eq!(ok.progress.done, 100);
        assert_eq!(ok.dest, home.join("Games/xodus/Gameok"));

        dl.start(req("bad")).unwrap();
        let bad = settle(&dl, "bad").await;
        assert_eq!(bad.state, JobState::Failed);
        assert!(bad.error.unwrap().contains("not entitled"));

        let saved: Vec<Job> =
            serde_json::from_slice(&std::fs::read(&dl.store).unwrap()).unwrap();
        assert_eq!(saved.len(), 3);
        dl.remove("bad").unwrap();
        assert!(home.join("Games/xodus/Gamebad").is_dir(), "remove never deletes files");

        std::fs::remove_dir_all(&home).unwrap();
    }
}
