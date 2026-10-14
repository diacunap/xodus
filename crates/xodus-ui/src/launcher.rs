//! Launch installed GDK games with a Wine engine (generic version of
//! scripts/macos/run-nms.sh). For an install folder `G`:
//!
//! 1. `G-decrypted/`: `xodus-cli run G /usr/bin/false` with `XODUS_DUMP_DIR` (once).
//! 2. `G-run/`: APFS clone of `G` with the decrypted files copied over it (once).
//! 3. `~/Games/xodus/prefixes/<Name>`: Wine prefix (`wineboot -u`, once), plus the
//!    engine's builtin `xgameruntime.dll` copied into system32 before every launch.
//! 4. `xodus-service` on /tmp/xodus.sock (started if needed, never stopped by us).
//! 5. `wine <exe>` from the exe's folder inside `G-run`, output to `logs/<Name>.log`.
//!
//! One game at a time. The UI polls `status()`.

use std::collections::VecDeque;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;

use crate::catalog::{self, Error};
use crate::downloads::{self, DONE_MARKER};

const DECRYPTED_MARKER: &str = ".xodus-decrypted";
const SOCKET: &str = "/tmp/xodus.sock";
/// How long to wait for xodus-service's socket (first start may sit on a Keychain prompt).
const SERVICE_WAIT: Duration = Duration::from_secs(20);
const TAIL: usize = 15;

/// Sibling folders the launcher derives from an install; not installs themselves.
pub fn is_derived_dir(path: &Path) -> bool {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    name.ends_with("-run") || name.ends_with("-decrypted") || name.ends_with("-run.partial")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub engine: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Preparing,
    Running,
    Exited,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct Run {
    pub product_id: Option<String>,
    pub title: String,
    /// Install folder (`G`).
    pub path: PathBuf,
    pub state: RunState,
    /// Current step while preparing.
    pub status: String,
    pub error: Option<String>,
    pub exit_code: Option<i32>,
    pub stopped: bool,
    /// Unix seconds when the game process started.
    pub started_at: Option<u64>,
    pub log: PathBuf,
    #[serde(skip)]
    prefix: PathBuf,
}

#[derive(Deserialize)]
struct LaunchRequest {
    /// Product id or install path.
    target: String,
    #[serde(default)]
    hud: bool,
    #[serde(default)]
    debug: bool,
}

pub struct Launcher {
    /// Folder holding xodus-cli / xodus-service (Contents/MacOS in the bundle).
    app_dir: PathBuf,
    root: PathBuf,
    settings_path: PathBuf,
    settings: Mutex<Settings>,
    run: Mutex<Option<Run>>,
    /// A save restore is in progress: Play waits for it.
    saves_busy: AtomicBool,
}

/// Held while saves are being restored; see [`Launcher::saves_guard`].
pub struct SavesGuard<'a>(&'a AtomicBool);

impl Drop for SavesGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl Launcher {
    pub fn load(app_dir: PathBuf) -> Arc<Self> {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let settings_path = home.join("Library/Application Support/Xodus/settings.json");
        let settings = std::fs::read(&settings_path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_else(|| Settings { engine: home.join("Games/xodus/engines/r4/wswine.bundle") });
        Arc::new(Self {
            app_dir,
            root: home.join("Games/xodus"),
            settings_path,
            settings: Mutex::new(settings),
            run: Mutex::new(None),
            saves_busy: AtomicBool::new(false),
        })
    }

    pub fn settings(&self) -> Value {
        let s = self.settings.lock().unwrap().clone();
        let ok = s.engine.join("bin/wine").is_file();
        json!({ "engine": s.engine, "engine_ok": ok })
    }

    pub fn set_settings(&self, arg: &str) -> Result<Value, Error> {
        let mut new: Settings = serde_json::from_str(arg)?;
        if let Ok(rest) = new.engine.strip_prefix("~") {
            new.engine = PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(rest);
        }
        if let Some(dir) = self.settings_path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&self.settings_path, serde_json::to_vec_pretty(&new)?)?;
        *self.settings.lock().unwrap() = new;
        Ok(self.settings())
    }

    pub fn status(&self) -> Value {
        json!(*self.run.lock().unwrap())
    }

    /// Why saves cannot be restored right now (a game is preparing or running).
    pub fn busy_reason(&self) -> Option<String> {
        busy(&self.run.lock().unwrap())
    }

    /// Refuse if a game is preparing/running, else block Play until the guard is dropped.
    /// Checked and set under the run lock, like `launch`, so the two cannot interleave.
    pub fn saves_guard(&self) -> Result<SavesGuard<'_>, Error> {
        let slot = self.run.lock().unwrap();
        if let Some(why) = busy(&slot) {
            return Err(format!("Cannot restore saves while {why}. Quit the game first.").into());
        }
        if self.saves_busy.swap(true, Ordering::SeqCst) {
            return Err("another save restore is in progress".into());
        }
        Ok(SavesGuard(&self.saves_busy))
    }

    /// Validate quickly, then prepare and run the game in the background.
    pub async fn launch(self: &Arc<Self>, client: &reqwest::Client, arg: &str) -> Result<(), Error> {
        let req = serde_json::from_str::<LaunchRequest>(arg)
            .unwrap_or_else(|_| LaunchRequest { target: arg.to_owned(), hud: false, debug: false });
        if let Some(run) = &*self.run.lock().unwrap()
            && matches!(run.state, RunState::Preparing | RunState::Running)
        {
            return Err(format!("{} is already running", run.title).into());
        }

        let game = self.resolve(&req.target)?;
        let product_id = downloads::store_id(&game);
        let config = downloads::find_config(&game)
            .ok_or_else(|| format!("no MicrosoftGame.Config in {}", game.display()))?;
        let exe = parse_executable(&std::fs::read_to_string(&config)?)
            .ok_or("MicrosoftGame.Config lists no PC executable")?;

        let engine = self.settings.lock().unwrap().engine.clone();
        if !engine.join("bin/wine").is_file() {
            return Err(format!(
                "Wine engine not found at {}. Extract the wine-cx-sikarugir engine there \
                 (so that bin/wine exists) or set its path in Settings.",
                engine.display()
            )
            .into());
        }
        let dll = engine.join("lib/wine/x86_64-windows/xgameruntime.dll");
        if !dll.is_file() {
            return Err(format!("the engine has no builtin xgameruntime.dll ({})", dll.display()).into());
        }

        // Folder names are the alphanumeric title (see downloads), so this is stable offline.
        let name = safe_name(&game.file_name().unwrap_or_default().to_string_lossy());
        let mut title = name.clone();
        if let Some(id) = &product_id
            && let Ok(games) = catalog::products(client, std::slice::from_ref(id)).await
            && let Some(g) = games.into_iter().next()
        {
            title = g.title;
        }

        let run = Run {
            product_id,
            title,
            path: game.clone(),
            state: RunState::Preparing,
            status: "Preparing".into(),
            error: None,
            exit_code: None,
            stopped: false,
            started_at: None,
            log: self.root.join("logs").join(format!("{name}.log")),
            prefix: self.root.join("prefixes").join(&name),
        };
        {
            let mut slot = self.run.lock().unwrap();
            if slot.as_ref().is_some_and(|r| matches!(r.state, RunState::Preparing | RunState::Running)) {
                return Err("another game is already starting".into());
            }
            if self.saves_busy.load(Ordering::SeqCst) {
                return Err("saves are being restored; try again in a moment".into());
            }
            *slot = Some(run.clone());
        }
        tracing::info!(game = %game.display(), exe, "launch requested");

        let this = self.clone();
        tokio::spawn(async move {
            let opts = Opts { engine, exe, hud: req.hud, debug: req.debug };
            let result = this.prepare_and_run(&run, &opts).await;
            let mut slot = this.run.lock().unwrap();
            if let Some(r) = slot.as_mut() {
                match result {
                    Ok(code) => {
                        r.state = RunState::Exited;
                        r.exit_code = code;
                    }
                    Err(err) => {
                        tracing::error!("launch failed: {err}");
                        r.state = RunState::Failed;
                        r.error = Some(err.to_string());
                    }
                }
            }
        });
        Ok(())
    }

    /// `wineserver -k` for the running game's prefix.
    pub async fn stop(&self) -> Result<(), Error> {
        let prefix = {
            let mut slot = self.run.lock().unwrap();
            let run = slot.as_mut().filter(|r| r.state == RunState::Running).ok_or("no game is running")?;
            run.stopped = true;
            run.prefix.clone()
        };
        let engine = self.settings.lock().unwrap().engine.clone();
        let status = Command::new(engine.join("bin/wineserver"))
            .arg("-k")
            .env("WINEPREFIX", &prefix)
            .status()
            .await?;
        tracing::info!(?status, "wineserver -k");
        Ok(())
    }

    pub async fn open_log(&self) -> Result<(), Error> {
        let log = self.run.lock().unwrap().as_ref().map(|r| r.log.clone()).ok_or("no game was launched")?;
        if !log.is_file() {
            return Err(format!("no log yet at {}", log.display()).into());
        }
        let status = Command::new("open").arg(&log).status().await?;
        if !status.success() {
            return Err(format!("open exited with {status}").into());
        }
        Ok(())
    }

    /// Install path, or the install whose MicrosoftGame.Config has this StoreId.
    fn resolve(&self, target: &str) -> Result<PathBuf, Error> {
        let installed = |p: &Path| p.join(DONE_MARKER).is_file() && !is_derived_dir(p);
        let path = Path::new(target);
        if path.is_absolute() {
            return if path.starts_with(&self.root) && installed(path) {
                Ok(path.to_path_buf())
            } else {
                Err(format!("not an installed game: {target}").into())
            };
        }
        std::fs::read_dir(&self.root)?
            .flatten()
            .map(|e| e.path())
            .find(|p| installed(p) && downloads::store_id(p).as_deref() == Some(target))
            .ok_or_else(|| format!("{target} is not installed in {}", self.root.display()).into())
    }

    fn set_status(&self, status: &str) {
        tracing::info!("launch: {status}");
        if let Some(r) = self.run.lock().unwrap().as_mut() {
            r.status = status.to_owned();
        }
    }

    /// Returns the game's exit code.
    async fn prepare_and_run(&self, run: &Run, opts: &Opts) -> Result<Option<i32>, Error> {
        let game = &run.path;
        let run_dir = sibling(game, "-run");
        if !run_dir.is_dir() {
            self.decrypt(game).await?;
            self.set_status("Creating the game folder (APFS clone)…");
            make_run_tree(game, &sibling(game, "-decrypted"), &run_dir).await?;
        }

        let wine = opts.engine.join("bin/wine");
        let logs = self.root.join("logs");
        std::fs::create_dir_all(&logs)?;
        if !run.prefix.exists() {
            self.set_status("Creating the Wine prefix (first launch, takes a minute)…");
            std::fs::create_dir_all(run.prefix.parent().unwrap())?;
            let log = std::fs::File::create(logs.join("wineboot.log"))?;
            let status = Command::new(&wine)
                .args(["wineboot", "-u"])
                .env("WINEPREFIX", &run.prefix)
                .env("WINEDLLOVERRIDES", "mscoree,mshtml=")
                .stdin(Stdio::null())
                .stdout(log.try_clone()?)
                .stderr(log)
                .status()
                .await?;
            if !status.success() {
                return Err(format!(
                    "wineboot failed ({status}); see {}. Delete {} before retrying.",
                    logs.join("wineboot.log").display(),
                    run.prefix.display()
                )
                .into());
            }
        }
        // GDK titles load xgameruntime from system32 (docs/macos blocker 4). Remove first so
        // a symlink there is replaced instead of written through.
        let dll = run.prefix.join("drive_c/windows/system32/xgameruntime.dll");
        let _ = std::fs::remove_file(&dll);
        std::fs::copy(opts.engine.join("lib/wine/x86_64-windows/xgameruntime.dll"), &dll)
            .map_err(|e| format!("cannot copy xgameruntime.dll into {}: {e}", dll.display()))?;

        self.ensure_service(&logs).await?;

        let exe = run_dir.join(&opts.exe);
        let cwd = exe.parent().ok_or("bad executable path")?;
        let exe_name = exe.file_name().ok_or("bad executable path")?;
        if !exe.is_file() {
            return Err(format!("executable not found: {}", exe.display()).into());
        }
        self.set_status("Starting");
        let log = std::fs::File::create(&run.log)?;
        let mut cmd = Command::new(&wine);
        cmd.arg(exe_name)
            .current_dir(cwd)
            .env("WINEPREFIX", &run.prefix)
            .env("XODUS_SOCKET", SOCKET)
            .env("WINEDLLOVERRIDES", "xgameruntime=b;vulkan-1=b")
            .env("WINE_VK_MASK_UNSUPPORTED_FEATURES", "1")
            .env("WINEMSYNC", "1")
            .env_remove("MTL_HUD_ENABLED")
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        if opts.debug {
            cmd.env("WINEDEBUG", "+loaddll,err+all,fixme+all,warn+vulkan").env("MVK_CONFIG_LOG_LEVEL", "3");
        } else {
            cmd.env("WINEDEBUG", "-all").env("MVK_CONFIG_LOG_LEVEL", "1");
        }
        if opts.hud {
            cmd.env("MTL_HUD_ENABLED", "1");
        }
        let mut child = cmd.spawn().map_err(|e| format!("cannot start {}: {e}", wine.display()))?;
        tracing::info!(pid = child.id(), exe = %exe.display(), "game started");
        if let Some(r) = self.run.lock().unwrap().as_mut() {
            r.state = RunState::Running;
            r.status = "Running".into();
            r.started_at = Some(downloads::now());
        }
        let status = child.wait().await?;
        tracing::info!(?status, "game exited");
        Ok(status.code())
    }

    /// Dump the decrypted executables once (marker written on success).
    async fn decrypt(&self, game: &Path) -> Result<(), Error> {
        let out = sibling(game, "-decrypted");
        if out.join(DECRYPTED_MARKER).is_file() {
            return Ok(());
        }
        self.set_status("Decrypting executables…");
        let cli = self.app_dir.join("xodus-cli");
        let mut child = Command::new(&cli)
            .arg("run")
            .arg(game)
            .arg("/usr/bin/false")
            .env("XODUS_DUMP_DIR", &out)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot start {}: {e}", cli.display()))?;
        let tail = Arc::new(Mutex::new(VecDeque::new()));
        let readers = [
            collect(child.stdout.take(), tail.clone()),
            collect(child.stderr.take(), tail.clone()),
        ];
        let status = child.wait().await?;
        for r in readers {
            let _ = tokio::time::timeout(Duration::from_secs(2), r).await;
        }
        if !status.success() {
            let tail = tail.lock().unwrap().iter().cloned().collect::<Vec<_>>().join("\n");
            return Err(format!("decrypting failed ({status}):\n{tail}").into());
        }
        std::fs::create_dir_all(&out)?;
        std::fs::write(out.join(DECRYPTED_MARKER), b"")?;
        Ok(())
    }

    /// Make sure xodus-service answers on /tmp/xodus.sock; start it if not.
    async fn ensure_service(&self, logs: &Path) -> Result<(), Error> {
        if tokio::net::UnixStream::connect(SOCKET).await.is_ok() {
            return Ok(());
        }
        self.set_status("Starting xodus-service…");
        // A stale socket file makes the service's bind() fail.
        let _ = std::fs::remove_file(SOCKET);
        let service = self.app_dir.join("xodus-service");
        let log_path = logs.join("xodus-service.log");
        let log = std::fs::File::create(&log_path)?;
        let mut child = Command::new(&service)
            .env("XODUS_LOG", "info")
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()
            .map_err(|e| format!("cannot start {}: {e}", service.display()))?;
        let started = Instant::now();
        loop {
            if tokio::net::UnixStream::connect(SOCKET).await.is_ok() {
                // Reap it whenever it exits; it outlives the game on purpose.
                tokio::spawn(async move { child.wait().await });
                return Ok(());
            }
            if let Ok(Some(status)) = child.try_wait() {
                return Err(format!("xodus-service exited ({status}); see {}", log_path.display()).into());
            }
            if started.elapsed() > SERVICE_WAIT {
                tokio::spawn(async move { child.wait().await });
                return Err("xodus-service is not ready. Approve the Keychain prompt (Always Allow), \
                            then press Play again."
                    .into());
            }
            if started.elapsed() > Duration::from_secs(2) {
                self.set_status("Waiting for Keychain approval…");
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

struct Opts {
    engine: PathBuf,
    /// Relative path of the exe inside the install (forward slashes).
    exe: String,
    hud: bool,
    debug: bool,
}

fn busy(run: &Option<Run>) -> Option<String> {
    let run = run.as_ref()?;
    match run.state {
        RunState::Preparing => Some(format!("{} is starting", run.title)),
        RunState::Running => Some(format!("{} is running", run.title)),
        RunState::Exited | RunState::Failed => None,
    }
}

/// `G` + suffix, next to `G`.
fn sibling(game: &Path, suffix: &str) -> PathBuf {
    let mut name = game.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    game.with_file_name(name)
}

pub fn safe_name(s: &str) -> String {
    let name: String = s.chars().filter(char::is_ascii_alphanumeric).collect();
    if name.is_empty() { "Game".into() } else { name }
}

/// Clone `game` (copy-on-write) and lay the decrypted files over it. Built under
/// `<run>.partial` and renamed at the end so an interrupted copy is never used.
async fn make_run_tree(game: &Path, decrypted: &Path, run: &Path) -> Result<(), Error> {
    let partial = sibling(run, ".partial");
    if partial.exists() {
        tokio::fs::remove_dir_all(&partial).await?;
    }
    let out = Command::new("cp").arg("-cR").arg(game).arg(&partial).output().await?;
    if !out.status.success() {
        return Err(format!("cp -cR failed: {}", String::from_utf8_lossy(&out.stderr).trim()).into());
    }
    let (decrypted, partial2) = (decrypted.to_path_buf(), partial.clone());
    tokio::task::spawn_blocking(move || overlay(&decrypted, &partial2)).await??;
    tokio::fs::rename(&partial, run).await?;
    Ok(())
}

/// Copy every file under `src` to the same relative path under `dst` (except the marker).
fn overlay(src: &Path, dst: &Path) -> std::io::Result<()> {
    let mut stack = vec![src.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)?.flatten() {
            let path = entry.path();
            let rel = path.strip_prefix(src).expect("walked under src");
            if entry.file_type()?.is_dir() {
                stack.push(path);
            } else if rel != Path::new(DECRYPTED_MARKER) {
                let target = dst.join(rel);
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let _ = std::fs::remove_file(&target);
                std::fs::copy(&path, &target)?;
            }
        }
    }
    Ok(())
}

fn collect<R: AsyncRead + Unpin + Send + 'static>(
    reader: Option<R>,
    tail: Arc<Mutex<VecDeque<String>>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let Some(reader) = reader else { return };
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let mut tail = tail.lock().unwrap();
            if tail.len() == TAIL {
                tail.pop_front();
            }
            tail.push_back(line);
        }
    })
}

/// Relative path (forward slashes) of the first PC `<Executable Name=...>` in
/// `<ExecutableList>`. Tags and attribute names are case-insensitive; comments ignored.
pub fn parse_executable(config: &str) -> Option<String> {
    // Blank out comments, keeping byte offsets so `lower` and `text` stay aligned.
    let mut text = config.to_owned();
    while let Some(start) = text.find("<!--") {
        let end = text[start..].find("-->").map_or(text.len(), |e| start + e + 3);
        text.replace_range(start..end, &" ".repeat(end - start));
    }
    let lower = text.to_ascii_lowercase();
    let list_start = lower.find("<executablelist")?;
    let list_end = lower[list_start..].find("</executablelist").map_or(lower.len(), |e| list_start + e);

    let mut pos = list_start + "<executablelist".len();
    let mut first = None;
    while let Some(found) = lower[pos..list_end].find("<executable") {
        let start = pos + found;
        let end = start + lower[start..list_end].find('>')?;
        pos = end;
        let tag = &text[start + "<executable".len()..end];
        if !tag.starts_with(|c: char| c.is_ascii_whitespace() || c == '/') {
            continue;
        }
        let Some(name) = attr(tag, "name") else { continue };
        let Some(rel) = relative_exe(&name) else { continue };
        match attr(tag, "targetdevicefamily") {
            Some(f) if f.eq_ignore_ascii_case("pc") => return Some(rel),
            None if first.is_none() => first = Some(rel),
            _ => {}
        }
    }
    first
}

/// Value of `key="..."` (or single quotes) in a tag body; `key` is lowercase.
fn attr(tag: &str, key: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find(key) {
        let at = from + i;
        from = at + key.len();
        let before_ok = at == 0 || lower.as_bytes()[at - 1].is_ascii_whitespace();
        let rest = lower[from..].trim_start();
        if !before_ok || !rest.starts_with('=') {
            continue;
        }
        let value_start = lower.len() - rest[1..].trim_start().len();
        let quote = tag[value_start..].chars().next()?;
        if quote != '"' && quote != '\'' {
            return None;
        }
        let value = &tag[value_start + 1..];
        return Some(value[..value.find(quote)?].to_owned());
    }
    None
}

/// `Binaries\NMS.exe` -> `Binaries/NMS.exe`; rejects absolute paths and `..`.
fn relative_exe(name: &str) -> Option<String> {
    let rel = name.trim().replace('\\', "/");
    let ok = !rel.is_empty() && Path::new(&rel).components().all(|c| matches!(c, Component::Normal(_)));
    ok.then_some(rel)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Head of No Man's Sky's MicrosoftGame.Config (Store id BQVQTL3PCH05).
    const NMS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<Game configVersion="1">
  <Identity Name="HelloGames.NoMansSky"
            Publisher="CN=E17FC9C0-77E1-4CDD-8AE0-E634942431EE"
            Version="7.6.0.0" />

  <ExecutableList>
    <!-- final release build -->
    <Executable Name="Binaries\NMS.exe"
              TargetDeviceFamily="PC"
              IsDevOnly="false"
              Id="NoMansSky"/>
  </ExecutableList>

  <ShellVisuals DefaultDisplayName="No Man's Sky"
                  PublisherDisplayName="Hello Games"/>
  <StoreId>BQVQTL3PCH05</StoreId>
</Game>"#;

    #[test]
    fn parses_nms_executable() {
        assert_eq!(parse_executable(NMS).as_deref(), Some("Binaries/NMS.exe"));
    }

    #[test]
    fn picks_first_pc_entry_case_insensitively() {
        let cfg = r#"<game><executablelist>
            <!-- <Executable Name="Old.exe" TargetDeviceFamily="PC"/> -->
            <EXECUTABLE overrideDisplayName="x" NAME='Xbox\Game.exe' TARGETDEVICEFAMILY="Scarlett"/>
            <executable targetdevicefamily="pc" name="bin\Game.exe" Id="A"/>
            <executable name="bin\Other.exe" targetdevicefamily="PC"/>
        </executablelist></game>"#;
        assert_eq!(parse_executable(cfg).as_deref(), Some("bin/Game.exe"));
    }

    #[test]
    fn rejects_missing_or_unsafe_entries() {
        assert_eq!(parse_executable("<Game><StoreId>X</StoreId></Game>"), None);
        assert_eq!(
            parse_executable(r#"<ExecutableList><Executable Name="..\evil.exe" TargetDeviceFamily="PC"/></ExecutableList>"#),
            None
        );
        // No family attribute at all: fall back to the first entry.
        assert_eq!(
            parse_executable(r#"<ExecutableList><Executable Name="a.exe"/></ExecutableList>"#).as_deref(),
            Some("a.exe")
        );
    }

    #[test]
    fn derived_dirs_are_not_installs() {
        assert!(is_derived_dir(Path::new("/g/NoMansSky-run")));
        assert!(is_derived_dir(Path::new("/g/NoMansSky-decrypted")));
        assert!(is_derived_dir(Path::new("/g/NoMansSky-run.partial")));
        assert!(!is_derived_dir(Path::new("/g/NoMansSky")));
        assert_eq!(sibling(Path::new("/g/NoMansSky"), "-run"), Path::new("/g/NoMansSky-run"));
    }

    #[test]
    fn saves_guard_refuses_while_a_game_runs_and_blocks_play() {
        let base = std::env::temp_dir().join(format!("xodus-launcher-guard-{}", std::process::id()));
        let launcher = Launcher {
            app_dir: base.clone(),
            root: base.clone(),
            settings_path: base.join("settings.json"),
            settings: Mutex::new(Settings { engine: base.join("engine") }),
            run: Mutex::new(None),
            saves_busy: AtomicBool::new(false),
        };
        let mut run = Run {
            product_id: None,
            title: "No Man's Sky".into(),
            path: base.join("NoMansSky"),
            state: RunState::Running,
            status: "Running".into(),
            error: None,
            exit_code: None,
            stopped: false,
            started_at: None,
            log: base.join("x.log"),
            prefix: base.join("prefix"),
        };
        for state in [RunState::Preparing, RunState::Running] {
            run.state = state;
            *launcher.run.lock().unwrap() = Some(run.clone());
            assert!(launcher.busy_reason().is_some());
            assert!(launcher.saves_guard().is_err());
        }
        run.state = RunState::Exited;
        *launcher.run.lock().unwrap() = Some(run);
        assert_eq!(launcher.busy_reason(), None);
        let guard = launcher.saves_guard().unwrap();
        assert!(launcher.saves_guard().is_err(), "one restore at a time");
        assert!(launcher.saves_busy.load(Ordering::SeqCst));
        drop(guard);
        assert!(!launcher.saves_busy.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn run_tree_is_clone_plus_decrypted_files() {
        let base = std::env::temp_dir().join(format!("xodus-launcher-test-{}", std::process::id()));
        let (game, dec) = (base.join("Game"), base.join("Game-decrypted"));
        std::fs::create_dir_all(game.join("Bin")).unwrap();
        std::fs::create_dir_all(dec.join("Bin")).unwrap();
        std::fs::write(game.join("Bin/Game.exe"), "encrypted").unwrap();
        std::fs::write(game.join("data.pak"), "data").unwrap();
        std::fs::write(dec.join("Bin/Game.exe"), "MZ").unwrap();
        std::fs::write(dec.join(DECRYPTED_MARKER), "").unwrap();
        std::fs::create_dir_all(base.join("Game-run.partial")).unwrap(); // leftover from a crash

        let run = sibling(&game, "-run");
        make_run_tree(&game, &dec, &run).await.unwrap();
        assert_eq!(std::fs::read_to_string(run.join("Bin/Game.exe")).unwrap(), "MZ");
        assert_eq!(std::fs::read_to_string(run.join("data.pak")).unwrap(), "data");
        assert_eq!(std::fs::read_to_string(game.join("Bin/Game.exe")).unwrap(), "encrypted");
        assert!(!run.join(DECRYPTED_MARKER).exists());
        assert!(!base.join("Game-run.partial").exists());
        std::fs::remove_dir_all(&base).unwrap();
    }
}
