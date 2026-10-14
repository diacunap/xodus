//! Persistent token storage in a single encrypted file, as an alternative to the
//! OS keychain (which on macOS prompts again after every rebuild of an ad-hoc
//! signed binary).
//!
//! Format: `XDSEC1` | salt (32) | nonce (12) | AES-256-GCM(JSON map key -> base64).
//! Key: HKDF-SHA256(ikm = machine id, salt, info = "xodus secrets v1").
//!
//! Threat model: the key is bound to this machine, so a copied file is useless
//! elsewhere; a process running as the same user on this machine can decrypt it
//! (same as a keychain item whose access was approved). File mode 0600.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use fs2::FileExt;
use hkdf::Hkdf;
use sha2::Sha256;

use crate::tokens::store::{TokenBackend, TokenStoreError};

const MAGIC: &[u8; 6] = b"XDSEC1";
const SALT_LEN: usize = 32;
const NONCE_LEN: usize = 12;
const INFO: &[u8] = b"xodus secrets v1";

pub struct EncryptedFileBackend {
    path: PathBuf,
    lock_path: PathBuf,
    /// Read-through for keys missing from the file (one-time migration).
    fallback: Option<Box<dyn TokenBackend>>,
    in_process: Mutex<()>,
}

struct Contents {
    salt: [u8; SALT_LEN],
    map: BTreeMap<String, String>,
}

fn other(msg: impl Into<String>) -> TokenStoreError {
    TokenStoreError::Io(std::io::Error::other(msg.into()))
}

impl EncryptedFileBackend {
    pub fn new(path: PathBuf, fallback: Option<Box<dyn TokenBackend>>) -> Self {
        let lock_path = path.with_extension("lock");
        Self {
            path,
            lock_path,
            fallback,
            in_process: Mutex::new(()),
        }
    }

    /// `~/Library/Application Support/Xodus/secrets.enc` on macOS,
    /// `$XDG_DATA_HOME/xodus/secrets.enc` (or `~/.local/share`) elsewhere.
    pub fn default_path() -> PathBuf {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        if cfg!(target_os = "macos") {
            home.join("Library/Application Support/Xodus/secrets.enc")
        } else {
            std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".local/share"))
                .join("xodus/secrets.enc")
        }
    }

    fn lock(&self, exclusive: bool) -> Result<File, TokenStoreError> {
        if let Some(dir) = self.path.parent() {
            create_private_dir(dir)?;
        }
        let file = OpenOptions::new().create(true).truncate(false).write(true).open(&self.lock_path)?;
        if exclusive { file.lock_exclusive()? } else { file.lock_shared()? }
        Ok(file)
    }

    fn load(&self) -> Result<Option<Contents>, TokenStoreError> {
        let mut raw = Vec::new();
        match File::open(&self.path) {
            Ok(mut f) => f.read_to_end(&mut raw)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if raw.len() < MAGIC.len() + SALT_LEN + NONCE_LEN || &raw[..MAGIC.len()] != MAGIC {
            return Err(other(format!("{} is not a xodus secrets file", self.path.display())));
        }
        let salt: [u8; SALT_LEN] = raw[MAGIC.len()..MAGIC.len() + SALT_LEN].try_into().unwrap();
        let nonce = Nonce::try_from(&raw[MAGIC.len() + SALT_LEN..MAGIC.len() + SALT_LEN + NONCE_LEN])
            .map_err(|_| other("bad nonce"))?;
        let cipher = cipher_for(&salt)?;
        let plain = cipher
            .decrypt(&nonce, &raw[MAGIC.len() + SALT_LEN + NONCE_LEN..])
            .map_err(|_| other("cannot decrypt secrets file (different machine or corrupted)"))?;
        let map = serde_json::from_slice(&plain).map_err(|e| other(format!("secrets file: {e}")))?;
        Ok(Some(Contents { salt, map }))
    }

    fn save(&self, contents: &Contents) -> Result<(), TokenStoreError> {
        let mut nonce = [0u8; NONCE_LEN];
        getrandom_fill(&mut nonce)?;
        let cipher = cipher_for(&contents.salt)?;
        let plain = serde_json::to_vec(&contents.map).map_err(|e| other(e.to_string()))?;
        let sealed = cipher
            .encrypt(&Nonce::from(nonce), plain.as_slice())
            .map_err(|_| other("encryption failed"))?;

        let tmp = self.path.with_extension("enc.tmp");
        {
            let mut f = private_file(&tmp)?;
            f.write_all(MAGIC)?;
            f.write_all(&contents.salt)?;
            f.write_all(&nonce)?;
            f.write_all(&sealed)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    fn update(&self, f: impl FnOnce(&mut BTreeMap<String, String>)) -> Result<(), TokenStoreError> {
        let _guard = self.in_process.lock().unwrap_or_else(|e| e.into_inner());
        let _lock = self.lock(true)?;
        let mut contents = match self.load()? {
            Some(c) => c,
            None => {
                let mut salt = [0u8; SALT_LEN];
                getrandom_fill(&mut salt)?;
                Contents { salt, map: BTreeMap::new() }
            }
        };
        f(&mut contents.map);
        self.save(&contents)
    }

    fn read(&self, key: &str) -> Result<Option<Vec<u8>>, TokenStoreError> {
        let _guard = self.in_process.lock().unwrap_or_else(|e| e.into_inner());
        let _lock = self.lock(false)?;
        let Some(contents) = self.load()? else { return Ok(None) };
        contents
            .map
            .get(key)
            .map(|v| B64.decode(v).map_err(|e| other(format!("secrets file: {e}"))))
            .transpose()
    }
}

impl TokenBackend for EncryptedFileBackend {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, TokenStoreError> {
        if let Some(value) = self.read(key)? {
            return Ok(Some(value));
        }
        // One-time migration: copy from the previous store, leave the original in place.
        let Some(fallback) = &self.fallback else { return Ok(None) };
        let Some(value) = fallback.get(key)? else { return Ok(None) };
        self.set(key, &value)?;
        tracing::info!(key, "migrated secret from keychain to {}", self.path.display());
        Ok(Some(value))
    }

    fn set(&self, key: &str, value: &[u8]) -> Result<(), TokenStoreError> {
        let encoded = B64.encode(value);
        self.update(|map| {
            map.insert(key.to_owned(), encoded);
        })
    }

    fn remove(&self, key: &str) -> Result<(), TokenStoreError> {
        self.update(|map| {
            map.remove(key);
        })?;
        // Removal is an explicit user action (logout): also clear the old store.
        if let Some(fallback) = &self.fallback {
            let _ = fallback.remove(key);
        }
        Ok(())
    }
}

fn cipher_for(salt: &[u8; SALT_LEN]) -> Result<Aes256Gcm, TokenStoreError> {
    let ikm = machine_id()?;
    let mut key = [0u8; 32];
    Hkdf::<Sha256>::new(Some(salt), ikm.as_bytes())
        .expand(INFO, &mut key)
        .map_err(|_| other("key derivation failed"))?;
    Aes256Gcm::new_from_slice(&key).map_err(|_| other("bad key length"))
}

#[cfg(target_os = "macos")]
fn machine_id() -> Result<String, TokenStoreError> {
    let out = std::process::Command::new("/usr/sbin/ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find(|l| l.contains("\"IOPlatformUUID\""))
        .and_then(|l| l.split('"').nth(3))
        .map(str::to_owned)
        .ok_or_else(|| other("IOPlatformUUID not found"))
}

#[cfg(not(target_os = "macos"))]
fn machine_id() -> Result<String, TokenStoreError> {
    for p in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
        if let Ok(id) = fs::read_to_string(p) {
            let id = id.trim();
            if !id.is_empty() {
                return Ok(id.to_owned());
            }
        }
    }
    Err(other("no machine id found"))
}

fn getrandom_fill(buf: &mut [u8]) -> Result<(), TokenStoreError> {
    File::open("/dev/urandom")?.read_exact(buf)?;
    Ok(())
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> Result<(), TokenStoreError> {
    use std::os::unix::fs::DirBuilderExt;
    if !dir.exists() {
        fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> Result<(), TokenStoreError> {
    fs::create_dir_all(dir)?;
    Ok(())
}

#[cfg(unix)]
fn private_file(path: &Path) -> Result<File, TokenStoreError> {
    use std::os::unix::fs::OpenOptionsExt;
    Ok(OpenOptions::new().create(true).truncate(true).write(true).mode(0o600).open(path)?)
}

#[cfg(not(unix))]
fn private_file(path: &Path) -> Result<File, TokenStoreError> {
    Ok(OpenOptions::new().create(true).truncate(true).write(true).open(path)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::backend::MemoryBackend;

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xodus-secrets-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir.join("secrets.enc")
    }

    #[test]
    fn roundtrip_and_encrypted_at_rest() {
        let path = temp_path("rt");
        let b = EncryptedFileBackend::new(path.clone(), None);
        assert_eq!(b.get("k").unwrap(), None);
        b.set("k", b"super-secret-token").unwrap();
        b.set("other", b"x").unwrap();
        assert_eq!(b.get("k").unwrap().as_deref(), Some(&b"super-secret-token"[..]));
        let raw = fs::read(&path).unwrap();
        assert!(raw.starts_with(MAGIC));
        assert!(!raw.windows(5).any(|w| w == b"super"), "plaintext leaked to disk");
        b.remove("k").unwrap();
        assert_eq!(b.get("k").unwrap(), None);
        assert_eq!(b.get("other").unwrap().as_deref(), Some(&b"x"[..]));
        // A fresh instance (another process) reads the same data.
        let b2 = EncryptedFileBackend::new(path.clone(), None);
        assert_eq!(b2.get("other").unwrap().as_deref(), Some(&b"x"[..]));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn migrates_from_fallback_once() {
        let path = temp_path("mig");
        let old = MemoryBackend::default();
        old.set("user-tokens", b"from-keychain").unwrap();
        let b = EncryptedFileBackend::new(path.clone(), Some(Box::new(old)));
        assert_eq!(b.get("user-tokens").unwrap().as_deref(), Some(&b"from-keychain"[..]));
        // Now served from the file, without the fallback.
        let b2 = EncryptedFileBackend::new(path.clone(), None);
        assert_eq!(b2.get("user-tokens").unwrap().as_deref(), Some(&b"from-keychain"[..]));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn rejects_tampered_file() {
        let path = temp_path("tamper");
        let b = EncryptedFileBackend::new(path.clone(), None);
        b.set("k", b"v").unwrap();
        let mut raw = fs::read(&path).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 0xff;
        fs::write(&path, raw).unwrap();
        assert!(b.get("k").is_err());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
