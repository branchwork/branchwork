//! At-rest encryption for Branchwork-managed credentials.
//!
//! Phase 3.1 of the runner-daemon-workspace plan introduces a
//! `credentials` table whose `encrypted_secret` column carries
//! AES-256-GCM ciphertext. Plaintext (raw SSH private keys, GH PATs,
//! GitLab PATs) never reaches the DB — encryption happens at write time,
//! decryption at the moment of dispatch.
//!
//! The master key lives at `$BRANCHWORK_DATA_DIR/.master.key` (or
//! `<claude_dir>/.master.key` when the env var is unset — see
//! [`resolve_master_key_path`]). It is generated on first boot via
//! `rand::rng()` and written with mode `0600` on Unix. Rotating the file
//! invalidates every existing `encrypted_secret` row — verified by the
//! round-trip + key-rotation unit tests at the bottom of this module.
//!
//! Wire format of [`encrypt`] output:
//! `nonce (12 B) || ciphertext || GCM tag (16 B)`
//! [`decrypt`] re-splits the nonce, runs `Aes256Gcm::decrypt` to validate
//! the tag, and returns the original plaintext bytes.
//!
//! ## Threat model
//!
//! - Disk read of `branchwork.db` alone does NOT leak secrets: an
//!   attacker without the master key sees only ciphertext.
//! - Disk read of `branchwork.db` AND `.master.key` leaks every active
//!   credential; the file mode (`0600`) is the only access control.
//!   Hosts that need stronger guarantees should mount `.master.key` from
//!   a secrets-management volume (Vault, AWS Secrets Manager, KMS).
//! - In-memory plaintext is unavoidable at dispatch time. Callers should
//!   not log decrypted bytes.

use std::io;
use std::path::{Path, PathBuf};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use rand::RngCore;

/// AES-256-GCM nonce length in bytes (96 bits, the recommended GCM size).
pub const NONCE_LEN: usize = 12;

/// AES-256 key length in bytes (256 bits).
pub const KEY_LEN: usize = 32;

/// Filename of the master key under the resolved data directory.
pub const MASTER_KEY_FILENAME: &str = ".master.key";

/// Wraps the master AES key. Manual `Debug` impl below redacts the bytes
/// — derive(Debug) would silently spill key material into logs / panic
/// messages. The inner bytes never leave this module except through
/// [`encrypt`] / [`decrypt`].
#[derive(Clone)]
pub struct MasterKey([u8; KEY_LEN]);

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Don't leak the bytes into logs / panic messages. The length
        // signals "this is the master key, not some empty placeholder".
        write!(f, "MasterKey([REDACTED; {KEY_LEN}])")
    }
}

impl MasterKey {
    /// Construct a [`MasterKey`] from raw bytes. Returns an error if the
    /// slice is the wrong length — short reads of `.master.key` would
    /// silently weaken the cipher otherwise.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CryptoError> {
        if bytes.len() != KEY_LEN {
            return Err(CryptoError::InvalidKeyLength {
                expected: KEY_LEN,
                got: bytes.len(),
            });
        }
        let mut buf = [0u8; KEY_LEN];
        buf.copy_from_slice(bytes);
        Ok(MasterKey(buf))
    }

    /// Generate a fresh random key via [`rand::rng`].
    pub fn generate() -> Self {
        let mut buf = [0u8; KEY_LEN];
        rand::rng().fill_bytes(&mut buf);
        MasterKey(buf)
    }

    /// Expose the inner bytes. Intentionally crate-private — callers
    /// outside `crypto.rs` only ever need `encrypt` / `decrypt`.
    #[cfg(test)]
    pub(crate) fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }
}

/// Crypto-layer errors that bubble up to DB helpers. Manual `Display` +
/// `Error` impls below — the rest of the crate hand-rolls error types,
/// no `thiserror` dep.
#[derive(Debug)]
pub enum CryptoError {
    InvalidKeyLength { expected: usize, got: usize },
    CiphertextTooShort(usize),
    DecryptionFailed,
    EncryptionFailed(String),
    Io(io::Error),
}

impl std::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CryptoError::InvalidKeyLength { expected, got } => {
                write!(
                    f,
                    "invalid key length: expected {expected} bytes, got {got}"
                )
            }
            CryptoError::CiphertextTooShort(got) => write!(
                f,
                "ciphertext too short: expected at least {NONCE_LEN} bytes for the nonce, got {got}"
            ),
            CryptoError::DecryptionFailed => {
                f.write_str("AES-GCM decryption failed (wrong key or tampered ciphertext)")
            }
            CryptoError::EncryptionFailed(s) => write!(f, "AES-GCM encryption failed: {s}"),
            CryptoError::Io(e) => write!(f, "master-key I/O error: {e}"),
        }
    }
}

impl std::error::Error for CryptoError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CryptoError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for CryptoError {
    fn from(e: io::Error) -> Self {
        CryptoError::Io(e)
    }
}

/// Resolve the on-disk path of `.master.key`. Honors `$BRANCHWORK_DATA_DIR`
/// when set (matches the Docker overlay's `ENV BRANCHWORK_DATA_DIR=/data`);
/// otherwise falls back to `fallback_dir` (typically the `--claude-dir`
/// from CLI config).
///
/// Pure helper so unit tests can pin both branches without mutating the
/// process env.
pub fn resolve_master_key_path_from(env_data_dir: Option<&str>, fallback_dir: &Path) -> PathBuf {
    match env_data_dir {
        Some(p) if !p.trim().is_empty() => PathBuf::from(p).join(MASTER_KEY_FILENAME),
        _ => fallback_dir.join(MASTER_KEY_FILENAME),
    }
}

/// Resolve the on-disk path of `.master.key` using the live process env.
/// Thin wrapper around [`resolve_master_key_path_from`] — kept separate so
/// callers that want a fixed path (tests, offline tooling) can skip the
/// env lookup.
#[allow(dead_code)] // wired in by the credentials API in later phases
pub fn resolve_master_key_path(fallback_dir: &Path) -> PathBuf {
    let env = std::env::var("BRANCHWORK_DATA_DIR").ok();
    resolve_master_key_path_from(env.as_deref(), fallback_dir)
}

/// Load the master key from `path`, or generate + persist a fresh one if
/// the file is absent. The file is created with mode `0600` on Unix; the
/// parent directory is created if missing. On Windows the file is written
/// with the default ACL — operators relying on Windows hosts should
/// restrict access via NTFS permissions out of band.
///
/// Idempotent: a second call on the same path reads the same bytes.
/// Rotating the file (replace contents OR delete + let the next call
/// regenerate) breaks every existing ciphertext — that is the canonical
/// "invalidate all credentials" knob.
#[allow(dead_code)] // wired in by the credentials API in later phases
pub fn load_or_generate_master_key(path: &Path) -> Result<MasterKey, CryptoError> {
    if path.exists() {
        let bytes = std::fs::read(path)?;
        return MasterKey::from_bytes(&bytes);
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let key = MasterKey::generate();
    write_master_key_file(path, key.as_bytes_for_persist())?;
    Ok(key)
}

impl MasterKey {
    /// Crate-private helper so [`load_or_generate_master_key`] can persist
    /// freshly minted bytes without exposing the inner `[u8; 32]` to
    /// general callers.
    #[allow(dead_code)] // co-consumed with `load_or_generate_master_key`
    fn as_bytes_for_persist(&self) -> &[u8; KEY_LEN] {
        &self.0
    }
}

/// Atomically-ish write the master key with `0600` on Unix. The file is
/// created via `OpenOptions::create_new` so a concurrent caller can't
/// silently overwrite an existing key — concurrent first-boot races land
/// on the same physical file and one wins, the other reads it on retry
/// (handled by the caller via [`load_or_generate_master_key`]'s exists()
/// pre-check).
#[cfg(unix)]
#[allow(dead_code)] // co-consumed with `load_or_generate_master_key`
fn write_master_key_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
#[allow(dead_code)] // co-consumed with `load_or_generate_master_key`
fn write_master_key_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

/// Encrypt `plaintext` under `key` using AES-256-GCM with a fresh random
/// 96-bit nonce. The returned `Vec<u8>` carries `nonce || ciphertext ||
/// tag`; [`decrypt`] expects that layout.
pub fn encrypt(key: &MasterKey, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let aes_key = Key::<Aes256Gcm>::from_slice(&key.0);
    let cipher = Aes256Gcm::new(aes_key);

    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let mut ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| CryptoError::EncryptionFailed(e.to_string()))?;

    let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    out.extend_from_slice(&nonce_bytes);
    out.append(&mut ciphertext);
    Ok(out)
}

/// Decrypt the output of [`encrypt`]. Returns
/// [`CryptoError::DecryptionFailed`] for both "wrong key" and "tampered
/// ciphertext" — AES-GCM treats them identically, and the caller doesn't
/// need to distinguish.
pub fn decrypt(key: &MasterKey, encrypted: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if encrypted.len() < NONCE_LEN {
        return Err(CryptoError::CiphertextTooShort(encrypted.len()));
    }
    let (nonce_bytes, ciphertext) = encrypted.split_at(NONCE_LEN);

    let aes_key = Key::<Aes256Gcm>::from_slice(&key.0);
    let cipher = Aes256Gcm::new(aes_key);
    let nonce = Nonce::from_slice(nonce_bytes);

    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| CryptoError::DecryptionFailed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn encrypt_decrypt_round_trip_returns_original_bytes() {
        let key = MasterKey::generate();
        let plaintext = b"-----BEGIN OPENSSH PRIVATE KEY-----\nabc...\n";
        let ciphertext = encrypt(&key, plaintext).unwrap();
        // Sanity: ciphertext differs from plaintext, and is at least
        // nonce (12) + tag (16) bytes longer than plaintext.
        assert_ne!(ciphertext.as_slice(), plaintext);
        assert!(ciphertext.len() >= plaintext.len() + NONCE_LEN + 16);
        let decrypted = decrypt(&key, &ciphertext).unwrap();
        assert_eq!(decrypted.as_slice(), plaintext);
    }

    #[test]
    fn encrypt_emits_fresh_nonce_per_call() {
        // Two encrypt calls on the same key + plaintext must produce
        // different ciphertexts (different nonces). Otherwise an attacker
        // observing repeated writes could correlate them.
        let key = MasterKey::generate();
        let p = b"hello";
        let a = encrypt(&key, p).unwrap();
        let b = encrypt(&key, p).unwrap();
        assert_ne!(a, b);
        // Both still decrypt to the same plaintext.
        assert_eq!(decrypt(&key, &a).unwrap(), p);
        assert_eq!(decrypt(&key, &b).unwrap(), p);
    }

    #[test]
    fn rotating_master_key_invalidates_existing_ciphertext() {
        // The acceptance test for Phase 3.1: ciphertext encrypted under
        // master key A must NOT decrypt under master key B. This pins the
        // "rotate `.master.key` to invalidate every credential" knob.
        let key_a = MasterKey::generate();
        let key_b = MasterKey::generate();
        // Guard against the absurdly improbable equal-generation case.
        assert_ne!(key_a.as_bytes(), key_b.as_bytes());

        let plaintext = b"ghp_supersecrettoken";
        let ciphertext = encrypt(&key_a, plaintext).unwrap();
        let err = decrypt(&key_b, &ciphertext).unwrap_err();
        assert!(matches!(err, CryptoError::DecryptionFailed));
    }

    #[test]
    fn decrypt_rejects_tampered_ciphertext() {
        let key = MasterKey::generate();
        let mut ciphertext = encrypt(&key, b"abc").unwrap();
        // Flip the last byte (inside the GCM tag) — must fail tag check.
        let last = ciphertext.len() - 1;
        ciphertext[last] ^= 0x01;
        let err = decrypt(&key, &ciphertext).unwrap_err();
        assert!(matches!(err, CryptoError::DecryptionFailed));
    }

    #[test]
    fn decrypt_rejects_short_input() {
        let key = MasterKey::generate();
        let err = decrypt(&key, &[1, 2, 3]).unwrap_err();
        assert!(matches!(err, CryptoError::CiphertextTooShort(3)));
    }

    #[test]
    fn from_bytes_rejects_wrong_length() {
        let err = MasterKey::from_bytes(&[0u8; 16]).unwrap_err();
        assert!(matches!(
            err,
            CryptoError::InvalidKeyLength {
                expected: 32,
                got: 16
            }
        ));
    }

    #[test]
    fn load_or_generate_creates_file_on_first_call() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".master.key");
        assert!(!path.exists());
        let key = load_or_generate_master_key(&path).unwrap();
        assert!(path.exists());
        // File length must be exactly KEY_LEN bytes (no trailing newline,
        // no JSON wrapping — the file IS the raw key material).
        let on_disk = std::fs::read(&path).unwrap();
        assert_eq!(on_disk.len(), KEY_LEN);
        assert_eq!(&on_disk, key.as_bytes());
    }

    #[test]
    fn load_or_generate_is_idempotent_on_existing_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".master.key");
        let key_a = load_or_generate_master_key(&path).unwrap();
        let key_b = load_or_generate_master_key(&path).unwrap();
        assert_eq!(key_a.as_bytes(), key_b.as_bytes());
    }

    #[test]
    fn load_or_generate_creates_parent_dir_if_missing() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("a").join("b").join(".master.key");
        assert!(!nested.parent().unwrap().exists());
        let _ = load_or_generate_master_key(&nested).unwrap();
        assert!(nested.exists());
    }

    #[cfg(unix)]
    #[test]
    fn fresh_master_key_file_has_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".master.key");
        let _ = load_or_generate_master_key(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "master key must be 0600, got {mode:o}");
    }

    #[test]
    fn resolve_master_key_path_prefers_env_when_set() {
        let p = resolve_master_key_path_from(Some("/data"), Path::new("/home/u/.claude"));
        assert_eq!(p, PathBuf::from("/data/.master.key"));
    }

    #[test]
    fn resolve_master_key_path_falls_back_when_env_unset() {
        let p = resolve_master_key_path_from(None, Path::new("/home/u/.claude"));
        assert_eq!(p, PathBuf::from("/home/u/.claude/.master.key"));
    }

    #[test]
    fn resolve_master_key_path_falls_back_on_empty_env() {
        // Whitespace-only env value is treated as unset (matches the
        // `webhook_url` filter pattern in config.rs).
        let p = resolve_master_key_path_from(Some(""), Path::new("/c"));
        assert_eq!(p, PathBuf::from("/c/.master.key"));
        let p2 = resolve_master_key_path_from(Some("   "), Path::new("/c"));
        assert_eq!(p2, PathBuf::from("/c/.master.key"));
    }

    #[test]
    fn end_to_end_rotation_invalidates_on_disk_round_trip() {
        // Simulate the rotation scenario the task acceptance pins:
        //   1. Boot once → generate key A → encrypt some plaintext.
        //   2. Rotate the file (delete + regenerate) → get key B.
        //   3. The ciphertext encrypted under A no longer decrypts.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".master.key");

        let key_a = load_or_generate_master_key(&path).unwrap();
        let plaintext = b"github_pat_AAA";
        let ciphertext = encrypt(&key_a, plaintext).unwrap();
        // Sanity: round-trips under the same key.
        assert_eq!(decrypt(&key_a, &ciphertext).unwrap(), plaintext);

        // Rotate: drop the file, regenerate.
        std::fs::remove_file(&path).unwrap();
        let key_b = load_or_generate_master_key(&path).unwrap();
        assert_ne!(key_a.as_bytes(), key_b.as_bytes());

        let err = decrypt(&key_b, &ciphertext).unwrap_err();
        assert!(matches!(err, CryptoError::DecryptionFailed));
    }
}
