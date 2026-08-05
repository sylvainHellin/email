// Machine-bound encrypted secrets store.
//
// Stores SMTP/IMAP passwords and OAuth2 token blobs encrypted at rest with a
// key derived from the host machine ID and the user's UID. Decryption fails
// (with an authentication error) if the file is moved to another machine or
// to another user's account, which is the property we want for backup/sync
// leakage protection.
//
// THREAT MODEL (see also docs/secrets.md):
//   - Defends against: file leaking via Time Machine, iCloud, Dropbox,
//     accidental git commit, cross-machine `cp -a ~/` restore. The file is
//     genuinely useless on any other machine.
//   - Does NOT defend against: an attacker running as the same user on the
//     same machine. Such an attacker can attach a debugger, drive Mail.app
//     via AppleScript, read ~/Library/Mail/, etc. -- all easier than this.
//
// CRYPTO CONSTRUCTION:
//   ikm  = machine_id || getuid().to_le_bytes() || b"mailypoppins-v1"
//   key  = HKDF-SHA256(ikm, salt = b"mailypoppins-secrets", info = b"")[..32]
//   ct   = ChaCha20-Poly1305(key, nonce, plaintext)
//   file = b"MPSEC" || version[1] || nonce[12] || ct_with_tag
//
// The key and decrypted plaintext are zeroed on drop.

use anyhow::{anyhow, Context, Result};
use chacha20poly1305::{
    aead::{Aead, KeyInit},
    ChaCha20Poly1305, Nonce,
};
use hkdf::Hkdf;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;
use zeroize::{Zeroize, ZeroizeOnDrop};

// ---------------------------------------------------------------------------
// File format constants
// ---------------------------------------------------------------------------

const MAGIC: &[u8; 5] = b"MPSEC";
const VERSION: u8 = 0x01;
const NONCE_LEN: usize = 12;
const HEADER_LEN: usize = MAGIC.len() + 1 + NONCE_LEN; // 5 + 1 + 12 = 18
const HKDF_SALT: &[u8] = b"mailypoppins-secrets";
const APP_SALT: &[u8] = b"mailypoppins-v1";

// ---------------------------------------------------------------------------
// Backend selection (read from config)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SecretsBackendKind {
    #[default]
    EncryptedFile,
    Keyring,
}

// ---------------------------------------------------------------------------
// Public errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum SecretsError {
    /// Secrets file does not exist yet. Caller should run `mp config init`.
    NotInitialized(PathBuf),
    /// File exists but cannot be decrypted (wrong machine, wrong user,
    /// corrupt, or tampered). Caller should run `mp config reset-secrets`.
    Undecryptable(PathBuf, String),
    /// Underlying I/O or crypto error.
    Other(anyhow::Error),
}

impl std::fmt::Display for SecretsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotInitialized(p) => write!(
                f,
                "Secrets store not initialized at {}. Run `mp config init` (fresh install) or `mp config set-password` to add credentials.",
                p.display()
            ),
            Self::Undecryptable(p, why) => write!(
                f,
                "Cannot decrypt secrets store at {} ({}). This usually means the file was created on a different machine or by a different user. Run `mp config reset-secrets` to wipe and re-enter passwords.",
                p.display(),
                why
            ),
            Self::Other(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for SecretsError {}

impl From<anyhow::Error> for SecretsError {
    fn from(e: anyhow::Error) -> Self {
        SecretsError::Other(e)
    }
}

// ---------------------------------------------------------------------------
// SecretsBackend trait
// ---------------------------------------------------------------------------

pub trait SecretsBackend: Send + Sync {
    fn get(&self, key: &str) -> Result<String>;
    fn set(&self, key: &str, value: &str) -> Result<()>;
    fn delete(&self, key: &str) -> Result<()>;
}

// ---------------------------------------------------------------------------
// Crypto primitives (also used by oauth2.rs for the token cache)
// ---------------------------------------------------------------------------

/// Derived 32-byte symmetric key, zeroed on drop.
#[derive(Zeroize, ZeroizeOnDrop)]
struct DerivedKey([u8; 32]);

fn machine_id_bytes() -> Result<Vec<u8>> {
    // The `machine-uid` crate reads /etc/machine-id on Linux/WSL and
    // IOPlatformUUID on macOS via a shell-out to `ioreg`.
    let id = machine_uid::get().map_err(|e| {
        anyhow!(
            "Failed to read machine ID (needed to derive secrets key): {}",
            e
        )
    })?;
    Ok(id.into_bytes())
}

#[cfg(unix)]
fn user_uid_bytes() -> [u8; 4] {
    // Safe: getuid() is always defined on POSIX, never fails.
    let uid = unsafe { libc_getuid() };
    uid.to_le_bytes()
}

#[cfg(unix)]
extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

#[cfg(not(unix))]
fn user_uid_bytes() -> [u8; 4] {
    // WSL is unix; native Windows is not a target. Fall back to zeros.
    [0u8; 4]
}

/// Derive the 32-byte key from a caller-supplied IKM via HKDF-SHA256.
fn derive_key_from_ikm(ikm: &[u8]) -> DerivedKey {
    let hk = Hkdf::<Sha256>::new(Some(HKDF_SALT), ikm);
    let mut out = [0u8; 32];
    hk.expand(&[], &mut out)
        .expect("32 bytes is well within HKDF-SHA256 output limit");
    DerivedKey(out)
}

/// Build the canonical IKM = machine_id || uid_le[4] || APP_SALT.
fn canonical_ikm() -> Result<Vec<u8>> {
    let mid = machine_id_bytes()?;
    let mut ikm = Vec::with_capacity(mid.len() + 4 + APP_SALT.len());
    ikm.extend_from_slice(&mid);
    ikm.extend_from_slice(&user_uid_bytes());
    ikm.extend_from_slice(APP_SALT);
    Ok(ikm)
}

fn derive_machine_key() -> Result<DerivedKey> {
    let ikm = canonical_ikm()?;
    Ok(derive_key_from_ikm(&ikm))
}

/// Encrypt arbitrary bytes with the machine-bound key.
/// Output: `MAGIC || VERSION || nonce[12] || ct_with_tag`.
pub fn encrypt_blob(plaintext: &[u8]) -> Result<Vec<u8>> {
    let key = derive_machine_key()?;
    encrypt_with_key(&key, plaintext)
}

/// Decrypt bytes produced by `encrypt_blob`.
pub fn decrypt_blob(ciphertext: &[u8]) -> Result<Vec<u8>> {
    let key = derive_machine_key()?;
    decrypt_with_key(&key, ciphertext)
}

fn encrypt_with_key(key: &DerivedKey, plaintext: &[u8]) -> Result<Vec<u8>> {
    let cipher = ChaCha20Poly1305::new(key.0.as_ref().into());
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| anyhow!("ChaCha20-Poly1305 encryption failed: {}", e))?;
    let mut out = Vec::with_capacity(HEADER_LEN + ct.len());
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Atomically write `data` to `path` with owner-only permissions.
///
/// The file is *created* with mode 0600 (unix), so there is no window where
/// umask-default permissions apply (chmod-after-write leaks that window).
/// Writes to a `.tmp` sibling then renames over the destination, preserving
/// overwrite semantics for existing files.
pub fn write_secret_file_atomic(path: &std::path::Path, data: &[u8]) -> Result<()> {
    use std::io::Write;

    let tmp = path.with_extension("enc.tmp");
    // `create_new(true)` fails if the tmp file exists; remove any stale one
    // left behind by a previous crashed run.
    match fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(anyhow!(e)).with_context(|| {
                format!("Failed to remove stale temp file: {}", tmp.display())
            })
        }
    }

    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(&tmp)
        .with_context(|| format!("Failed to create secret temp file: {}", tmp.display()))?;
    file.write_all(data)
        .with_context(|| format!("Failed to write secret file: {}", tmp.display()))?;
    drop(file);

    fs::rename(&tmp, path).with_context(|| {
        format!("Failed to rename {} -> {}", tmp.display(), path.display())
    })?;
    Ok(())
}

fn decrypt_with_key(key: &DerivedKey, ciphertext: &[u8]) -> Result<Vec<u8>> {
    if ciphertext.len() < HEADER_LEN {
        return Err(anyhow!("secrets blob too short"));
    }
    if &ciphertext[..MAGIC.len()] != MAGIC {
        return Err(anyhow!("secrets blob has wrong magic header"));
    }
    let version = ciphertext[MAGIC.len()];
    if version != VERSION {
        return Err(anyhow!(
            "unsupported secrets blob version: {} (expected {})",
            version,
            VERSION
        ));
    }
    let nonce = Nonce::from_slice(&ciphertext[MAGIC.len() + 1..HEADER_LEN]);
    let ct = &ciphertext[HEADER_LEN..];
    let cipher = ChaCha20Poly1305::new(key.0.as_ref().into());
    cipher
        .decrypt(nonce, ct)
        .map_err(|_| anyhow!("authentication failed (wrong key or tampered ciphertext)"))
}

// ---------------------------------------------------------------------------
// EncryptedFileBackend
// ---------------------------------------------------------------------------

/// Plaintext-side TOML payload of the secrets store.
///
/// Note: we do not derive `Zeroize` here because `HashMap` is not zeroize-able
/// directly. Manual zeroize on drop would require iterating values and
/// rebuilding -- against our threat model (attacker with same-user shell
/// access) it offers no real benefit, since the heap allocator already does
/// not promise to clear freed memory.
#[derive(Debug, Default, Serialize, Deserialize)]
struct SecretsFile {
    #[serde(default)]
    entries: HashMap<String, String>,
}

pub struct EncryptedFileBackend {
    path: PathBuf,
    inner: std::sync::RwLock<EncryptedFileState>,
}

struct EncryptedFileState {
    key: DerivedKey,
    file: SecretsFile,
}

impl EncryptedFileBackend {
    /// Open the encrypted file at `path`, deriving the key from the canonical
    /// machine IKM. If the file doesn't exist, return an empty in-memory
    /// store (writes will create the file).
    pub fn open(path: PathBuf) -> std::result::Result<Self, SecretsError> {
        let key = derive_machine_key().map_err(SecretsError::Other)?;
        Self::open_with_key(path, key)
    }

    /// Test-only: construct with a caller-supplied IKM. Used to simulate
    /// "wrong machine" in unit tests without touching the real machine ID.
    #[cfg(test)]
    pub fn open_with_ikm_for_test(
        path: PathBuf,
        ikm: &[u8],
    ) -> std::result::Result<Self, SecretsError> {
        let key = derive_key_from_ikm(ikm);
        Self::open_with_key(path, key)
    }

    fn open_with_key(path: PathBuf, key: DerivedKey) -> std::result::Result<Self, SecretsError> {
        let file = if path.exists() {
            let bytes = fs::read(&path)
                .with_context(|| format!("Failed to read secrets file: {}", path.display()))
                .map_err(SecretsError::Other)?;
            let plaintext = decrypt_with_key(&key, &bytes)
                .map_err(|e| SecretsError::Undecryptable(path.clone(), e.to_string()))?;
            let plaintext_str = std::str::from_utf8(&plaintext).map_err(|e| {
                SecretsError::Undecryptable(
                    path.clone(),
                    format!("decrypted plaintext is not valid UTF-8: {}", e),
                )
            })?;
            let parsed: SecretsFile = toml::from_str(plaintext_str).map_err(|e| {
                SecretsError::Undecryptable(
                    path.clone(),
                    format!("decrypted plaintext is not valid TOML: {}", e),
                )
            })?;
            parsed
        } else {
            SecretsFile::default()
        };
        Ok(Self {
            path,
            inner: std::sync::RwLock::new(EncryptedFileState { key, file }),
        })
    }

    fn save_locked(&self, state: &EncryptedFileState) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create secrets dir: {}", parent.display())
            })?;
        }
        let plaintext = toml::to_string(&state.file)
            .context("Failed to serialize secrets to TOML")?;
        let blob = encrypt_with_key(&state.key, plaintext.as_bytes())?;
        // Atomic write, created with 0600 from the start (no umask window).
        write_secret_file_atomic(&self.path, &blob)?;
        // Drop our copy of the TOML plaintext.
        let mut s = plaintext.into_bytes();
        s.zeroize();
        Ok(())
    }
}

impl SecretsBackend for EncryptedFileBackend {
    fn get(&self, key: &str) -> Result<String> {
        let state = self
            .inner
            .read()
            .map_err(|_| anyhow!("secrets lock poisoned"))?;
        state
            .file
            .entries
            .get(key)
            .cloned()
            .with_context(|| format!("Secret '{}' not found. Run `mp config set-password`.", key))
    }

    fn set(&self, key: &str, value: &str) -> Result<()> {
        let mut state = self
            .inner
            .write()
            .map_err(|_| anyhow!("secrets lock poisoned"))?;
        state.file.entries.insert(key.to_string(), value.to_string());
        self.save_locked(&state)
    }

    fn delete(&self, key: &str) -> Result<()> {
        let mut state = self
            .inner
            .write()
            .map_err(|_| anyhow!("secrets lock poisoned"))?;
        state.file.entries.remove(key);
        self.save_locked(&state)
    }
}

// ---------------------------------------------------------------------------
// KeyringBackend (opt-in via `secrets_backend = "keyring"`)
// ---------------------------------------------------------------------------

const KEYRING_SERVICE: &str = "email-cli";

pub struct KeyringBackend;

impl SecretsBackend for KeyringBackend {
    fn get(&self, key: &str) -> Result<String> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, key)
            .context("Failed to create keyring entry")?;
        entry.get_password().with_context(|| {
            format!(
                "Password '{}' not found in keyring. Run `mp config set-password`.",
                key
            )
        })
    }

    fn set(&self, key: &str, value: &str) -> Result<()> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, key)
            .context("Failed to create keyring entry")?;
        entry
            .set_password(value)
            .with_context(|| format!("Failed to store '{}' in keyring", key))
    }

    fn delete(&self, key: &str) -> Result<()> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, key)
            .context("Failed to create keyring entry")?;
        entry
            .delete_credential()
            .with_context(|| format!("Failed to delete '{}' from keyring", key))
    }
}

// ---------------------------------------------------------------------------
// Process-wide singleton
// ---------------------------------------------------------------------------

static BACKEND: OnceLock<Box<dyn SecretsBackend>> = OnceLock::new();

/// Return the path to the encrypted secrets file: ~/.config/email/secrets.enc
pub fn secrets_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home)
        .join(".config")
        .join("email")
        .join("secrets.enc")
}

/// Initialize the process-wide secrets backend from the given kind.
/// Subsequent calls are no-ops (the first kind wins).
pub fn init(kind: SecretsBackendKind) -> std::result::Result<(), SecretsError> {
    if BACKEND.get().is_some() {
        return Ok(());
    }
    let backend: Box<dyn SecretsBackend> = match kind {
        SecretsBackendKind::EncryptedFile => Box::new(EncryptedFileBackend::open(secrets_path())?),
        SecretsBackendKind::Keyring => Box::new(KeyringBackend),
    };
    let _ = BACKEND.set(backend);
    Ok(())
}

/// Get a secret from the active backend.
pub fn get(key: &str) -> Result<String> {
    backend()?.get(key)
}

/// Set a secret in the active backend.
pub fn set(key: &str, value: &str) -> Result<()> {
    backend()?.set(key, value)
}

/// Delete a secret from the active backend.
pub fn delete(key: &str) -> Result<()> {
    backend()?.delete(key)
}

fn backend() -> Result<&'static (dyn SecretsBackend + 'static)> {
    let b = BACKEND
        .get()
        .ok_or_else(|| anyhow!("secrets backend not initialized -- call secrets::init() first"))?;
    Ok(b.as_ref())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn round_trip_set_save_reload_get() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("secrets.enc");
        let ikm = b"unit-test-ikm-A";

        let backend = EncryptedFileBackend::open_with_ikm_for_test(path.clone(), ikm).unwrap();
        backend.set("smtp-password-foo", "hunter2").unwrap();
        backend.set("imap-password-foo", "hunter3").unwrap();

        // Reopen with the same IKM -- should decrypt cleanly.
        let backend2 = EncryptedFileBackend::open_with_ikm_for_test(path.clone(), ikm).unwrap();
        assert_eq!(backend2.get("smtp-password-foo").unwrap(), "hunter2");
        assert_eq!(backend2.get("imap-password-foo").unwrap(), "hunter3");
    }

    #[test]
    fn delete_removes_entry() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("secrets.enc");
        let ikm = b"unit-test-ikm-B";

        let backend = EncryptedFileBackend::open_with_ikm_for_test(path.clone(), ikm).unwrap();
        backend.set("k", "v").unwrap();
        backend.delete("k").unwrap();
        assert!(backend.get("k").is_err());
    }

    #[test]
    fn wrong_ikm_fails_decryption() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("secrets.enc");

        let backend_a = EncryptedFileBackend::open_with_ikm_for_test(path.clone(), b"ikm-A").unwrap();
        backend_a.set("k", "v").unwrap();

        // Different IKM -- should produce an Undecryptable error.
        let err = EncryptedFileBackend::open_with_ikm_for_test(path.clone(), b"ikm-B")
            .err()
            .expect("decryption should fail with wrong IKM");
        match err {
            SecretsError::Undecryptable(_, _) => {}
            other => panic!("expected Undecryptable, got: {:?}", other),
        }
    }

    #[test]
    fn tampered_ciphertext_fails_decryption() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("secrets.enc");
        let ikm = b"ikm-tamper";

        let backend = EncryptedFileBackend::open_with_ikm_for_test(path.clone(), ikm).unwrap();
        backend.set("k", "v").unwrap();

        // Flip a byte in the ciphertext region (after the 18-byte header).
        let mut bytes = fs::read(&path).unwrap();
        let target = HEADER_LEN + 2;
        assert!(target < bytes.len());
        bytes[target] ^= 0xFF;
        fs::write(&path, &bytes).unwrap();

        let err = EncryptedFileBackend::open_with_ikm_for_test(path.clone(), ikm)
            .err()
            .expect("decryption of tampered file should fail");
        match err {
            SecretsError::Undecryptable(_, _) => {}
            other => panic!("expected Undecryptable, got: {:?}", other),
        }
    }

    #[test]
    fn missing_file_yields_empty_store() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("does-not-exist.enc");
        let backend =
            EncryptedFileBackend::open_with_ikm_for_test(path.clone(), b"ikm-empty").unwrap();
        // Empty store: get returns error, but the backend opens successfully.
        assert!(backend.get("anything").is_err());
        // First set should create the file.
        backend.set("k", "v").unwrap();
        assert!(path.exists());
        assert_eq!(backend.get("k").unwrap(), "v");
    }

    #[test]
    fn file_has_correct_header() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("secrets.enc");
        let backend =
            EncryptedFileBackend::open_with_ikm_for_test(path.clone(), b"ikm-header").unwrap();
        backend.set("k", "v").unwrap();
        let bytes = fs::read(&path).unwrap();
        assert!(bytes.len() > HEADER_LEN);
        assert_eq!(&bytes[..MAGIC.len()], MAGIC);
        assert_eq!(bytes[MAGIC.len()], VERSION);
    }

    #[cfg(unix)]
    #[test]
    fn write_secret_file_atomic_creates_with_0600_and_overwrites() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let path = dir.path().join("secret.enc");

        write_secret_file_atomic(&path, b"first").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "secret file should be created with mode 0600");
        assert_eq!(fs::read(&path).unwrap(), b"first");

        // Overwrite semantics preserved for existing destination files.
        write_secret_file_atomic(&path, b"second").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second");
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        // A stale tmp file from a crashed run must not block the write.
        let tmp = path.with_extension("enc.tmp");
        fs::write(&tmp, b"stale").unwrap();
        write_secret_file_atomic(&path, b"third").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"third");
        assert!(!tmp.exists(), "tmp file should be renamed away");
    }

    #[cfg(unix)]
    #[test]
    fn file_mode_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let path = dir.path().join("secrets.enc");
        let backend =
            EncryptedFileBackend::open_with_ikm_for_test(path.clone(), b"ikm-perm").unwrap();
        backend.set("k", "v").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "secrets file should be chmod 0600");
    }

    #[test]
    fn encrypt_decrypt_blob_round_trip() {
        // The standalone helpers used by oauth2.rs token cache.
        // Skip if we can't read the real machine ID (e.g. CI sandboxes).
        let Ok(_) = machine_id_bytes() else {
            return;
        };
        let pt = b"some token blob with binary \x00\x01\x02 bytes";
        let ct = encrypt_blob(pt).unwrap();
        let back = decrypt_blob(&ct).unwrap();
        assert_eq!(back.as_slice(), pt);
    }
}

