//! Versions of a game's XGameSaveFiles folders (`files`), per `<xuid>/<scid>`:
//!
//! - current: `<prefix>/drive_c/users/<u>/AppData/Local/XodusGameSave/<xuid>/<scid>/files`
//! - cloud sync backups made by the engine: `files.bak-YYYYmmdd-HHMMSS[-N]` next to `files`
//! - folders replaced by a restore: `files.replaced-YYYYmmdd-HHMMSS[-N]` next to `files`
//! - snapshots: `~/Games/xodus/save-snapshots/<Name>/<xuid>-<scid>/<stamp>[-label]/files`
//!   plus `snapshot.json`
//! - manual backups made by hand: `~/Games/xodus/save-backups/<Name>-*` (contents of `files`)
//!
//! Nothing here deletes user data: a restore snapshots the current folder first and keeps
//! it as `files.replaced-*`. Copies keep file mtimes (the cloud sync compares them).

use std::fs::{self, FileTimes};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use chrono::TimeZone;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::catalog::Error;
use crate::downloads::DONE_MARKER;
use crate::launcher::{is_derived_dir, safe_name};

const LIVE: &str = "files";
const BAK: &str = "files.bak-";
const REPLACED: &str = "files.replaced-";
const RESTORE_TMP: &str = "files.restore-tmp";
const PARTIAL: &str = ".partial";
const META: &str = "snapshot.json";
const STAMP: &str = "%Y%m%d-%H%M%S";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Current,
    Snapshot,
    CloudBackup,
    ManualBackup,
    Replaced,
}

#[derive(Debug, Clone, Serialize)]
pub struct Version {
    /// `<group id>/<kind>/<folder name>`; only ever compared, never turned into a path.
    pub id: String,
    pub kind: Kind,
    /// Folder name.
    pub name: String,
    pub label: Option<String>,
    /// When this version was made (unix secs): from the name / snapshot.json.
    pub time: Option<i64>,
    /// Newest file mtime inside (unix secs).
    pub saved: Option<i64>,
    pub size: u64,
    /// Top-level entries (e.g. AccountData, Slot1Auto).
    pub entries: Vec<String>,
    /// Folder holding the save files.
    pub path: PathBuf,
}

/// Entry next to `files` that is not a version (e.g. older container-based saves).
#[derive(Debug, Serialize)]
pub struct Other {
    pub name: String,
    pub size: u64,
}

#[derive(Debug, Serialize)]
pub struct Group {
    /// `<user>/<xuid>/<scid>`.
    pub id: String,
    pub xuid: String,
    pub scid: String,
    pub dir: PathBuf,
    pub versions: Vec<Version>,
    pub other: Vec<Other>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SnapshotMeta {
    created: i64,
    label: Option<String>,
    source: PathBuf,
    size: u64,
}

#[derive(Deserialize)]
pub struct SnapshotRequest {
    pub path: String,
    #[serde(default)]
    pub label: Option<String>,
    /// Group id; optional when the game has a single save folder.
    #[serde(default)]
    pub group: Option<String>,
}

#[derive(Deserialize)]
pub struct VersionRequest {
    pub path: String,
    pub version_id: String,
}

struct Game {
    name: String,
    /// Resolved (symlinks followed); None if the game has no prefix yet.
    prefix: Option<PathBuf>,
}

pub struct Saves {
    /// ~/Games/xodus
    root: PathBuf,
    /// Where wineserver keeps `.wine-<uid>/server-<dev>-<ino>/socket` (/tmp).
    wine_tmp: PathBuf,
}

impl Saves {
    pub fn new(root: PathBuf) -> Self {
        Self { root, wine_tmp: PathBuf::from("/tmp") }
    }

    /// `busy`: why restoring is not allowed right now (a game is preparing/running).
    pub fn list(&self, install: &str, busy: Option<String>) -> Result<Value, Error> {
        let game = self.game(install)?;
        let mut groups = Vec::new();
        if let Some(prefix) = &game.prefix {
            for (id, xuid, scid, dir) in save_dirs(prefix) {
                let (versions, other) = self.versions(&game, &id, &xuid, &scid, &dir);
                groups.push(Group { id, xuid, scid, dir, versions, other });
            }
        }
        let blocked = busy.or_else(|| {
            let sock = self.wineserver(game.prefix.as_deref()?)?;
            Some(format!("Wine is running in this prefix ({})", sock.display()))
        });
        Ok(json!({
            "game": game.name,
            "prefix": game.prefix,
            "groups": groups,
            "restore_blocked": blocked,
            "snapshots": self.snap_root(&game.name),
        }))
    }

    pub fn snapshot(&self, req: &SnapshotRequest) -> Result<Version, Error> {
        let game = self.game(&req.path)?;
        let prefix = game.prefix.as_deref().ok_or("this game has no Wine prefix yet")?;
        let dirs = save_dirs(prefix);
        let (_, xuid, scid, dir) = match &req.group {
            Some(g) => dirs.into_iter().find(|d| &d.0 == g).ok_or("unknown save folder")?,
            None if dirs.len() == 1 => dirs.into_iter().next().unwrap(),
            None if dirs.is_empty() => return Err("no saves found for this game".into()),
            None => return Err("this game has several save folders; pick one".into()),
        };
        self.snapshot_dir(&game, &xuid, &scid, &dir, req.label.as_deref())
    }

    /// Replace `files` with a copy of another version. `busy` = a game is preparing/running.
    pub fn restore(&self, req: &VersionRequest, busy: Option<String>) -> Result<Value, Error> {
        if let Some(why) = busy {
            return Err(format!("Cannot restore saves while {why}. Quit the game first.").into());
        }
        let game = self.game(&req.path)?;
        let prefix = game.prefix.clone().ok_or("this game has no Wine prefix yet")?;
        if let Some(sock) = self.wineserver(&prefix) {
            return Err(format!(
                "Wine is still running in this prefix ({}). Quit the game (or wait for wineserver to exit) first.",
                sock.display()
            )
            .into());
        }
        let (xuid, scid, dir, version) = self.find(&game, &req.version_id)?;
        if version.kind == Kind::Current {
            return Err("that is already the current save".into());
        }
        let live = dir.join(LIVE);
        let tmp = dir.join(RESTORE_TMP);
        if fs::symlink_metadata(&tmp).is_ok() {
            return Err(format!(
                "{} is left over from an interrupted restore; move it out of the way first",
                tmp.display()
            )
            .into());
        }

        // 1. Snapshot the current saves; abort if that fails.
        let before = if is_real_dir(&live) {
            let snap = self
                .snapshot_dir(&game, &xuid, &scid, &dir, Some("before-restore"))
                .map_err(|e| format!("could not snapshot the current saves, nothing was changed: {e}"))?;
            Some(snap.name)
        } else {
            None
        };

        // 2. Copy the chosen version next to `files`.
        if let Err(e) = copy_tree(&version.path, &tmp) {
            let _ = fs::remove_dir_all(&tmp); // our own incomplete copy
            return Err(format!("copying {} failed, nothing was changed: {e}", version.path.display()).into());
        }

        // 3. Swap: files -> files.replaced-<ts>, files.restore-tmp -> files.
        let replaced = if fs::symlink_metadata(&live).is_ok() {
            let target = unique(&dir, &format!("{REPLACED}{}", stamp()));
            if let Err(e) = fs::rename(&live, &target) {
                let _ = fs::remove_dir_all(&tmp);
                return Err(format!("could not move the current saves aside, nothing was changed: {e}").into());
            }
            Some(target)
        } else {
            None
        };
        if let Err(e) = fs::rename(&tmp, &live) {
            let rollback = replaced.as_ref().map(|r| fs::rename(r, &live));
            return Err(match rollback {
                Some(Err(e2)) => format!(
                    "restore failed ({e}) and so did putting the old saves back ({e2}): they are in {}, \
                     the restored copy in {}",
                    replaced.unwrap().display(),
                    tmp.display()
                ),
                _ => {
                    let _ = fs::remove_dir_all(&tmp);
                    format!("restore failed, the current saves were left in place: {e}")
                }
            }
            .into());
        }
        tracing::info!(from = %version.path.display(), to = %live.display(), ?before, ?replaced, "saves restored");
        Ok(json!({
            "restored": version.id,
            "restored_name": version.label.clone().unwrap_or(version.name.clone()),
            "auto_snapshot": before,
            "replaced": replaced.as_ref().and_then(|r| r.file_name()).map(|n| n.to_string_lossy().into_owned()),
        }))
    }

    /// Folder of a version (for "Show in Finder").
    pub fn version_path(&self, req: &VersionRequest) -> Result<PathBuf, Error> {
        let game = self.game(&req.path)?;
        Ok(self.find(&game, &req.version_id)?.3.path)
    }

    /// Install folder -> safe name and resolved prefix. Only direct children of the root
    /// holding a completed download are accepted.
    fn game(&self, install: &str) -> Result<Game, Error> {
        let path = Path::new(install);
        let plain = path.components().all(|c| matches!(c, Component::RootDir | Component::Normal(_)));
        if !path.is_absolute()
            || !plain
            || path.parent() != Some(self.root.as_path())
            || is_derived_dir(path)
            || !path.join(DONE_MARKER).is_file()
        {
            return Err(format!("not an installed game: {install}").into());
        }
        let name = safe_name(&path.file_name().unwrap_or_default().to_string_lossy());
        let prefix = match fs::canonicalize(self.root.join("prefixes").join(&name)) {
            Ok(p) => {
                let root = fs::canonicalize(&self.root)?;
                if !p.starts_with(&root) {
                    return Err(format!("the prefix {} is outside {}", p.display(), root.display()).into());
                }
                Some(p)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        Ok(Game { name, prefix })
    }

    fn snap_root(&self, name: &str) -> PathBuf {
        self.root.join("save-snapshots").join(name)
    }

    /// Group dir and version for an id, looked up in a fresh listing.
    fn find(&self, game: &Game, version_id: &str) -> Result<(String, String, PathBuf, Version), Error> {
        let prefix = game.prefix.as_deref().ok_or("this game has no Wine prefix yet")?;
        for (id, xuid, scid, dir) in save_dirs(prefix) {
            let (versions, _) = self.versions(game, &id, &xuid, &scid, &dir);
            if let Some(v) = versions.into_iter().find(|v| v.id == version_id) {
                return Ok((xuid, scid, dir, v));
            }
        }
        Err(format!("save version not found: {version_id}").into())
    }

    fn versions(&self, game: &Game, gid: &str, xuid: &str, scid: &str, dir: &Path) -> (Vec<Version>, Vec<Other>) {
        let mut out = Vec::new();
        let mut other = Vec::new();
        let make = |kind: Kind, tag: &str, name: &str, path: PathBuf, time: Option<i64>, label: Option<String>| {
            let (size, saved, entries) = describe(&path);
            Version { id: format!("{gid}/{tag}/{name}"), kind, name: name.to_owned(), label, time, saved, size, entries, path }
        };
        for (name, path) in children(dir) {
            if !is_real_dir(&path) {
                other.push(Other { size: tree_size(&path), name });
            } else if name == LIVE {
                out.push(make(Kind::Current, "current", &name, path, None, None));
            } else if name.starts_with(BAK) {
                let t = parse_stamp(&name);
                out.push(make(Kind::CloudBackup, "bak", &name, path, t, None));
            } else if name.starts_with(REPLACED) {
                let t = parse_stamp(&name);
                out.push(make(Kind::Replaced, "replaced", &name, path, t, None));
            } else if name != RESTORE_TMP {
                other.push(Other { size: tree_size(&path), name });
            }
        }
        let snaps = self.snap_root(&game.name).join(format!("{xuid}-{scid}"));
        for (name, path) in children(&snaps) {
            if name.ends_with(PARTIAL) || !is_real_dir(&path.join(LIVE)) {
                continue;
            }
            let meta: Option<SnapshotMeta> =
                fs::read(path.join(META)).ok().and_then(|b| serde_json::from_slice(&b).ok());
            let time = meta.as_ref().map(|m| m.created).or_else(|| parse_stamp(&name));
            let label = meta.and_then(|m| m.label);
            out.push(make(Kind::Snapshot, "snapshot", &name, path.join(LIVE), time, label));
        }
        let prefix = format!("{}-", game.name);
        for (name, path) in children(&self.root.join("save-backups")) {
            if name.starts_with(&prefix) && is_real_dir(&path) {
                let t = parse_stamp(&name);
                out.push(make(Kind::ManualBackup, "manual", &name, path, t, None));
            }
        }
        // Current first, then newest first.
        out.sort_by_key(|v| (v.kind != Kind::Current, std::cmp::Reverse(v.time.or(v.saved))));
        (out, other)
    }

    fn snapshot_dir(&self, game: &Game, xuid: &str, scid: &str, dir: &Path, label: Option<&str>) -> Result<Version, Error> {
        let src = dir.join(LIVE);
        if !is_real_dir(&src) {
            return Err("there is no current save folder to snapshot".into());
        }
        let parent = self.snap_root(&game.name).join(format!("{xuid}-{scid}"));
        fs::create_dir_all(&parent)?;
        let label = label.and_then(clean_label);
        let base = match &label {
            Some(l) => format!("{}-{l}", stamp()),
            None => stamp(),
        };
        for n in 1..100 {
            let name = if n == 1 { base.clone() } else { format!("{base}-{n}") };
            let (target, partial) = (parent.join(&name), parent.join(format!("{name}{PARTIAL}")));
            if fs::symlink_metadata(&target).is_ok() {
                continue;
            }
            match fs::create_dir(&partial) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
            let made = (|| -> io::Result<u64> {
                copy_tree(&src, &partial.join(LIVE))?;
                let size = tree_size(&partial.join(LIVE));
                let meta = SnapshotMeta { created: chrono::Local::now().timestamp(), label: label.clone(), source: src.clone(), size };
                fs::write(partial.join(META), serde_json::to_vec_pretty(&meta)?)?;
                if fs::symlink_metadata(&target).is_ok() {
                    return Err(io::Error::new(io::ErrorKind::AlreadyExists, "snapshot folder appeared meanwhile"));
                }
                fs::rename(&partial, &target)?;
                Ok(size)
            })();
            if let Err(e) = made {
                let _ = fs::remove_dir_all(&partial); // our own incomplete copy
                return Err(format!("snapshot failed: {e}").into());
            }
            tracing::info!(from = %src.display(), to = %target.display(), "saves snapshot");
            let (size, saved, entries) = describe(&target.join(LIVE));
            let gid = format!("{}/{xuid}/{scid}", user_of(dir));
            return Ok(Version {
                id: format!("{gid}/snapshot/{name}"),
                kind: Kind::Snapshot,
                name,
                label,
                time: Some(chrono::Local::now().timestamp()),
                saved,
                size,
                entries,
                path: target.join(LIVE),
            });
        }
        Err("could not pick a free snapshot name".into())
    }

    /// wineserver's socket for this prefix, if it exists (Wine is running there).
    fn wineserver(&self, prefix: &Path) -> Option<PathBuf> {
        let m = fs::metadata(prefix).ok()?;
        let sock = self
            .wine_tmp
            .join(format!(".wine-{}", m.uid()))
            .join(format!("server-{:x}-{:x}", m.dev(), m.ino()))
            .join("socket");
        fs::symlink_metadata(&sock).is_ok().then_some(sock)
    }
}

/// `(group id, xuid, scid, dir)` for every `users/<u>/AppData/Local/XodusGameSave/<xuid>/<scid>`
/// made of real directories (symlinks inside the prefix are not followed).
fn save_dirs(prefix: &Path) -> Vec<(String, String, String, PathBuf)> {
    let mut out = Vec::new();
    for (user, udir) in children(&prefix.join("drive_c/users")) {
        let mut base = udir.clone();
        let mut ok = is_real_dir(&udir);
        for part in ["AppData", "Local", "XodusGameSave"] {
            base.push(part);
            ok = ok && is_real_dir(&base);
        }
        if !ok {
            continue;
        }
        for (xuid, xdir) in children(&base).into_iter().filter(|(_, p)| is_real_dir(p)) {
            for (scid, sdir) in children(&xdir).into_iter().filter(|(_, p)| is_real_dir(p)) {
                out.push((format!("{user}/{xuid}/{scid}"), xuid.clone(), scid, sdir));
            }
        }
    }
    out
}

/// `.../users/<user>/AppData/Local/XodusGameSave/<xuid>/<scid>` -> `<user>`.
fn user_of(dir: &Path) -> String {
    dir.ancestors().nth(5).and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Sorted `(name, path)` of a folder's entries with UTF-8 names (empty if unreadable).
fn children(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out: Vec<_> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| Some((e.file_name().into_string().ok()?, e.path())))
        .collect();
    out.sort();
    out
}

fn is_real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
}

/// (bytes, newest file mtime, top-level entry names); symlinks not followed.
fn describe(dir: &Path) -> (u64, Option<i64>, Vec<String>) {
    let entries = children(dir).into_iter().map(|(n, _)| n).filter(|n| !n.starts_with('.')).collect();
    let (mut size, mut newest) = (0, None::<i64>);
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).into_iter().flatten().flatten() {
            let Ok(m) = e.metadata() else { continue };
            if m.is_dir() {
                stack.push(e.path());
            } else if m.is_file() {
                size += m.len();
                let t = m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64);
                newest = newest.max(t);
            }
        }
    }
    (size, newest, entries)
}

fn tree_size(path: &Path) -> u64 {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => describe(path).0,
        Ok(m) => m.len(),
        Err(_) => 0,
    }
}

/// Copy `src` into a new folder `dst` (must not exist), keeping access/modification times.
/// Symlinks and special files are skipped.
fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let (from, to) = (entry.path(), dst.join(entry.file_name()));
        let ft = entry.file_type()?;
        if ft.is_dir() {
            copy_tree(&from, &to)?;
        } else if ft.is_file() {
            fs::copy(&from, &to)?;
            copy_times(&from, &to)?;
        } else {
            tracing::warn!(path = %from.display(), "not copying a symlink/special file");
        }
    }
    copy_times(src, dst)
}

fn copy_times(from: &Path, to: &Path) -> io::Result<()> {
    let m = fs::metadata(from)?;
    let times = FileTimes::new().set_accessed(m.accessed()?).set_modified(m.modified()?);
    fs::File::open(to)?.set_times(times)
}

/// `base`, `base-2`, `base-3`… whichever does not exist in `dir`.
fn unique(dir: &Path, base: &str) -> PathBuf {
    (1..)
        .map(|n| dir.join(if n == 1 { base.to_owned() } else { format!("{base}-{n}") }))
        .find(|p| fs::symlink_metadata(p).is_err())
        .unwrap()
}

fn stamp() -> String {
    chrono::Local::now().format(STAMP).to_string()
}

/// Unix time of the last `YYYYmmdd-HHMMSS` in a name (local time).
fn parse_stamp(name: &str) -> Option<i64> {
    let b = name.as_bytes();
    (0..b.len().checked_sub(14)?).rev().find_map(|i| {
        let w = &b[i..i + 15];
        let shape = w.iter().enumerate().all(|(j, c)| if j == 8 { *c == b'-' } else { c.is_ascii_digit() });
        if !shape {
            return None;
        }
        let dt = chrono::NaiveDateTime::parse_from_str(&name[i..i + 15], STAMP).ok()?;
        chrono::Local.from_local_datetime(&dt).earliest().map(|d| d.timestamp())
    })
}

/// Label for a folder name: letters, digits, `-`, `_` (spaces become `-`), max 40.
fn clean_label(label: &str) -> Option<String> {
    let s: String = label
        .trim()
        .chars()
        .map(|c| if c.is_whitespace() { '-' } else { c })
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(40)
        .collect();
    let s = s.trim_matches('-').to_owned();
    (!s.is_empty()).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    const XUID: &str = "2535000000000001";
    const SCID: &str = "29070100-b936-489a-bce8-b9af3980429c";

    struct Tree {
        base: PathBuf,
        saves: Saves,
        install: String,
        group: PathBuf,
    }

    /// Fake ~/Games/xodus with an installed game, a prefix reached through a symlink
    /// (like prefixes/NoMansSky -> ../prefix-nms) and one save folder.
    fn tree(tag: &str) -> Tree {
        let base = std::env::temp_dir().join(format!("xodus-saves-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("Games/xodus");
        let install = root.join("NoMansSky");
        fs::create_dir_all(&install).unwrap();
        fs::write(install.join(DONE_MARKER), "").unwrap();
        let real = root.join("prefix-nms");
        let group = real.join(format!("drive_c/users/crossover/AppData/Local/XodusGameSave/{XUID}/{SCID}"));
        fs::create_dir_all(&group).unwrap();
        fs::create_dir_all(root.join("prefixes")).unwrap();
        std::os::unix::fs::symlink("../prefix-nms", root.join("prefixes/NoMansSky")).unwrap();
        let saves = Saves { root: root.clone(), wine_tmp: base.join("tmp") };
        Tree { base, saves, install: install.to_string_lossy().into_owned(), group }
    }

    fn write_old(path: &Path, body: &str, age_days: u64) -> SystemTime {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
        let t = SystemTime::now() - Duration::from_secs(age_days * 86400);
        fs::File::options().write(true).open(path).unwrap().set_modified(t).unwrap();
        fs::metadata(path).unwrap().modified().unwrap()
    }

    fn list(t: &Tree) -> Value {
        t.saves.list(&t.install, None).unwrap()
    }

    fn version<'a>(v: &'a Value, kind: &str) -> Vec<&'a Value> {
        v["groups"][0]["versions"].as_array().unwrap().iter().filter(|x| x["kind"] == kind).collect()
    }

    #[test]
    fn lists_snapshots_and_restores_keeping_old_data() {
        let t = tree("restore");
        let live = t.group.join(LIVE);
        let m_old = write_old(&live.join("Slot1Auto/data"), "v1", 3);
        write_old(&live.join("AccountData/data"), "acct", 3);
        // Engine cloud backup and an unrelated container folder.
        write_old(&t.group.join("files.bak-20261013-224849/Slot1Auto/data"), "cloud", 5);
        fs::create_dir_all(t.group.join("ABCDEF0123")).unwrap();
        // Hand-made backup in save-backups.
        write_old(&t.saves.root.join("save-backups/NoMansSky-manual-20261013-224800/Slot1Manual/data"), "manual", 4);

        let v = list(&t);
        assert_eq!(v["game"], "NoMansSky");
        assert_eq!(v["restore_blocked"], Value::Null);
        assert_eq!(v["groups"].as_array().unwrap().len(), 1);
        let cur = version(&v, "current")[0];
        assert_eq!(cur["entries"], json!(["AccountData", "Slot1Auto"]));
        assert_eq!(cur["size"], 6);
        assert_eq!(version(&v, "cloud_backup").len(), 1);
        assert_eq!(version(&v, "manual_backup").len(), 1);
        assert_eq!(v["groups"][0]["other"][0]["name"], "ABCDEF0123");

        // Snapshot v1, then the game writes v2.
        let snap = t
            .saves
            .snapshot(&SnapshotRequest { path: t.install.clone(), label: Some("before boss!".into()), group: None })
            .unwrap();
        assert!(snap.name.ends_with("-before-boss"), "{}", snap.name);
        assert_eq!(fs::metadata(snap.path.join("Slot1Auto/data")).unwrap().modified().unwrap(), m_old);
        assert!(snap.path.parent().unwrap().join(META).is_file());
        write_old(&live.join("Slot1Auto/data"), "v2", 0);

        let id = version(&list(&t), "snapshot")[0]["id"].as_str().unwrap().to_owned();
        assert_eq!(id, snap.id);
        let res = t.saves.restore(&VersionRequest { path: t.install.clone(), version_id: id }, None).unwrap();

        // Live folder is v1 again, with the original mtime.
        assert_eq!(fs::read_to_string(live.join("Slot1Auto/data")).unwrap(), "v1");
        assert_eq!(fs::metadata(live.join("Slot1Auto/data")).unwrap().modified().unwrap(), m_old);
        // v2 kept twice: as files.replaced-* and as the before-restore snapshot.
        let replaced = t.group.join(res["replaced"].as_str().unwrap());
        assert!(replaced.file_name().unwrap().to_string_lossy().starts_with(REPLACED));
        assert_eq!(fs::read_to_string(replaced.join("Slot1Auto/data")).unwrap(), "v2");
        let auto = res["auto_snapshot"].as_str().unwrap();
        assert!(auto.ends_with("-before-restore"));
        let v = list(&t);
        let autos: Vec<_> = version(&v, "snapshot").into_iter().filter(|s| s["name"] == auto).collect();
        assert_eq!(fs::read_to_string(Path::new(autos[0]["path"].as_str().unwrap()).join("Slot1Auto/data")).unwrap(), "v2");
        assert_eq!(version(&v, "replaced").len(), 1);
        assert!(!t.group.join(RESTORE_TMP).exists());

        // Restore the cloud backup too: a second replaced folder, nothing lost.
        let bak = version(&v, "cloud_backup")[0]["id"].as_str().unwrap().to_owned();
        t.saves.restore(&VersionRequest { path: t.install.clone(), version_id: bak }, None).unwrap();
        assert_eq!(fs::read_to_string(live.join("Slot1Auto/data")).unwrap(), "cloud");
        assert!(t.group.join("files.bak-20261013-224849/Slot1Auto/data").is_file());
        assert_eq!(version(&list(&t), "replaced").len(), 2);

        fs::remove_dir_all(&t.base).unwrap();
    }

    #[test]
    fn refuses_restore_while_running() {
        let t = tree("busy");
        write_old(&t.group.join("files/Slot1Auto/data"), "live", 1);
        write_old(&t.group.join("files.bak-20261001-101010/Slot1Auto/data"), "old", 2);
        let v = list(&t);
        let id = version(&v, "cloud_backup")[0]["id"].as_str().unwrap().to_owned();
        let req = VersionRequest { path: t.install.clone(), version_id: id };

        let err = t.saves.restore(&req, Some("No Man's Sky is running".into())).unwrap_err();
        assert!(err.to_string().contains("is running"), "{err}");

        // A wineserver socket for the prefix also blocks (game started outside the app).
        let prefix = fs::canonicalize(t.saves.root.join("prefixes/NoMansSky")).unwrap();
        let m = fs::metadata(&prefix).unwrap();
        let dir = t.saves.wine_tmp.join(format!(".wine-{}/server-{:x}-{:x}", m.uid(), m.dev(), m.ino()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("socket"), "").unwrap();
        assert!(list(&t)["restore_blocked"].as_str().unwrap().contains("Wine is running"));
        assert!(t.saves.restore(&req, None).unwrap_err().to_string().contains("Wine is still running"));

        // Nothing changed, no snapshot taken.
        assert_eq!(fs::read_to_string(t.group.join("files/Slot1Auto/data")).unwrap(), "live");
        assert!(version(&list(&t), "snapshot").is_empty());
        assert!(version(&list(&t), "replaced").is_empty());
        fs::remove_dir_all(&t.base).unwrap();
    }

    #[test]
    fn rejects_paths_outside_the_root_and_unknown_versions() {
        let t = tree("paths");
        write_old(&t.group.join("files/a"), "x", 1);
        for bad in [
            "relative/NoMansSky".to_owned(),
            format!("{}/../NoMansSky", t.install),
            t.saves.root.join("prefix-nms").to_string_lossy().into_owned(),
            "/etc".to_owned(),
        ] {
            assert!(t.saves.list(&bad, None).is_err(), "{bad}");
        }
        let req = VersionRequest {
            path: t.install.clone(),
            version_id: format!("crossover/{XUID}/{SCID}/snapshot/../../../../etc"),
        };
        assert!(t.saves.restore(&req, None).unwrap_err().to_string().contains("not found"));
        let cur = VersionRequest { path: t.install.clone(), version_id: format!("crossover/{XUID}/{SCID}/current/files") };
        assert!(t.saves.restore(&cur, None).is_err());
        fs::remove_dir_all(&t.base).unwrap();
    }

    #[test]
    fn names_and_stamps() {
        assert_eq!(clean_label("  before boss! "), Some("before-boss".into()));
        assert_eq!(clean_label("../.."), None);
        assert_eq!(parse_stamp("files.bak-20261013-224849-2"), parse_stamp("x-20261013-224849"));
        assert!(parse_stamp("files.bak-20261013-224849").is_some());
        assert_eq!(parse_stamp("files"), None);
        assert_eq!(user_of(Path::new("/p/drive_c/users/crossover/AppData/Local/XodusGameSave/1/2")), "crossover");
    }
}
