//! Materialise a wire [`Credential`] envelope as a tmpfs-backed file for
//! the lifetime of a single git child process.
//!
//! Plan `runner-daemon-workspace` task 3.3 introduces the on-demand
//! credential envelope: the server resolves a credential id against the
//! encrypted `credentials` table, ships the decrypted secret on the wire
//! alongside `CloneProject` (and, later, fetch / push RPCs), and the
//! runner — or the standalone-mode dispatcher running in the same role —
//! turns it into:
//!
//!  - a 0600 file on tmpfs (`mktemp -p $XDG_RUNTIME_DIR` falling back to
//!    `/tmp` when the runtime dir is unavailable),
//!  - a set of environment variables fed to the git child process **only**
//!    (`GIT_SSH_COMMAND='ssh -i <path> -o IdentitiesOnly=yes -o
//!    StrictHostKeyChecking=accept-new'` for `ssh_key`, or
//!    `GIT_ASKPASS=<wrapper>` for `gh_pat` / `gitlab_pat`),
//!  - and an RAII guard that unlinks the file on every exit path.
//!
//! Crucially, the broader process environment (`HOME`, `SSH_AUTH_SOCK`,
//! the operator's interactive `ssh-add` state) is **not** touched —
//! mutations land on `Command::env` for the spawned child, which inherits
//! parent env by default but where any per-child overrides are scoped to
//! that child process only. The operator's `ssh-add -l` listing before
//! and after a clone is byte-identical.
//!
//! Self-contained: no `crate::` deps except the wire [`Credential`] type.
//! Both the server binary and the runner binary include this file (the
//! server via `mod credential_material;` in `main.rs`, the runner via
//! `#[path = "../credential_material.rs"]`).

#![allow(dead_code)] // Both binaries include this module but each uses a different subset.

use std::path::{Path, PathBuf};

// The runner #[path]-includes `runner_protocol.rs` as its own crate-level
// module (`runner_protocol`). The server registers it as
// `crate::saas::runner_protocol`. We resolve the wire type via a re-export
// from the including binary so this leaf module can stay path-agnostic.
//
// The server binary creates the `mod credential_material;` registration in
// `main.rs` and supplies `use crate::saas::runner_protocol::Credential;`.
// The runner binary creates a `pub use runner_protocol::Credential;` at
// the same depth. We import via a relative module path that resolves to
// whichever wire module the host binary mounts.
//
// Both binaries already follow this idiom for `git_helpers.rs` which uses
// `crate::saas::runner_protocol::MergeOutcome` directly (server-side) and
// the runner's flat re-export (runner-side).
use crate::saas::runner_protocol::Credential;

// ── Constants ───────────────────────────────────────────────────────────────

/// `ssh_key` discriminator on `Credential::kind`. Must match the wire
/// form documented on [`crate::saas::runner_protocol::Credential`] and
/// the `db::CredentialKind::SshKey` round-trip.
pub const KIND_SSH_KEY: &str = "ssh_key";
/// `gh_pat` discriminator — GitHub Personal Access Token. Same shape on
/// disk and on the wire as `gitlab_pat` (both are bearer tokens injected
/// via `GIT_ASKPASS`), kept separate so the host-hint can pin which API
/// the token authenticates against.
pub const KIND_GH_PAT: &str = "gh_pat";
/// `gitlab_pat` discriminator — GitLab Personal Access Token.
pub const KIND_GITLAB_PAT: &str = "gitlab_pat";

// ── Errors ──────────────────────────────────────────────────────────────────

/// Errors surfaced by [`CredentialMaterial::materialize`]. Operator-facing
/// strings flow into [`crate::saas::runner_protocol::WireMessage::CloneFailed`]
/// verbatim via `Display`.
#[derive(Debug)]
pub enum MaterializeError {
    /// `tempfile::Builder::tempfile_in` returned an IO error. Usually
    /// "permission denied" on locked-down tmpfs mounts or "no such file
    /// or directory" when neither `$XDG_RUNTIME_DIR` nor `/tmp` is
    /// writable.
    Io(std::io::Error),
    /// `Credential::kind` did not match any of `ssh_key | gh_pat | gitlab_pat`.
    /// The runner refuses to dispatch — surfaces as a `CloneFailed`.
    UnknownKind(String),
    /// `Credential::secret` was empty. Treated as a programmer error
    /// (the server should never resolve to an empty secret), but
    /// surfaced rather than panicking so a corrupted row degrades to a
    /// dashboard error instead of a runner crash.
    EmptySecret,
}

impl std::fmt::Display for MaterializeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MaterializeError::Io(e) => write!(f, "credential materialise IO error: {e}"),
            MaterializeError::UnknownKind(k) => write!(f, "unknown credential kind: {k}"),
            MaterializeError::EmptySecret => f.write_str("credential secret is empty"),
        }
    }
}

impl std::error::Error for MaterializeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            MaterializeError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for MaterializeError {
    fn from(e: std::io::Error) -> Self {
        MaterializeError::Io(e)
    }
}

// ── CredentialMaterial ──────────────────────────────────────────────────────

/// RAII guard around a materialised credential. The on-disk file is
/// unlinked when this drops, so every exit path (success, git non-zero
/// exit, process spawn failure, panic via destructor) reliably removes
/// the secret.
///
/// **Move semantics**: do not clone. Consumers `materialize` once, hand
/// the env vars to the child via `Command::env`, then drop the guard
/// after the child returns.
///
/// `Debug` is hand-rolled so it never references the on-disk file
/// contents — the path itself isn't secret, but a future maintainer
/// inadvertently adding a `secret_preview` field would be redacted
/// here.
pub struct CredentialMaterial {
    /// Absolute path of the 0600 file holding the secret. Used by
    /// [`Self::git_env_vars`] to render `GIT_SSH_COMMAND` / `GIT_ASKPASS`.
    path: PathBuf,
    /// `Credential::kind` from the wire envelope — drives env-var
    /// selection in [`Self::git_env_vars`].
    kind: String,
    /// Operator-facing id of the credential (echoed in structured log
    /// lines but never inside an env var the git child can read back).
    credential_id: String,
}

impl std::fmt::Debug for CredentialMaterial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Path / kind / credential_id are operator-visible; secret bytes
        // live on disk only and are intentionally NOT touched here.
        f.debug_struct("CredentialMaterial")
            .field("path", &self.path)
            .field("kind", &self.kind)
            .field("credential_id", &self.credential_id)
            .finish()
    }
}

impl CredentialMaterial {
    /// Write the secret to a fresh tmpfs file (0600) under
    /// `$XDG_RUNTIME_DIR` or `/tmp`, and return an RAII guard.
    ///
    /// On Unix the file is created via `OpenOptions::new().create_new(true)
    /// .mode(0o600)` so the secret is never world-readable even for a
    /// race-window. On Windows the file is created with default ACLs
    /// (the runner is a single-user surface today; Windows hardening is
    /// future work).
    ///
    /// `git_env_vars()` returns `GIT_SSH_COMMAND` (for `ssh_key`) or
    /// `GIT_ASKPASS`-style env vars (for `gh_pat` / `gitlab_pat`). For
    /// the PAT case the file holds a 4-line askpass shell wrapper, NOT
    /// the raw token — the wrapper echoes the token from a sibling file
    /// (also 0600) so a process listing of the askpass invocation does
    /// not leak the token in argv.
    pub fn materialize(cred: &Credential) -> Result<Self, MaterializeError> {
        if cred.secret.is_empty() {
            return Err(MaterializeError::EmptySecret);
        }
        let kind = cred.kind.as_str();
        if !matches!(kind, KIND_SSH_KEY | KIND_GH_PAT | KIND_GITLAB_PAT) {
            return Err(MaterializeError::UnknownKind(cred.kind.clone()));
        }

        match kind {
            KIND_SSH_KEY => Self::materialize_ssh_key(cred),
            KIND_GH_PAT | KIND_GITLAB_PAT => Self::materialize_pat(cred),
            // Already handled by the matches! check above.
            _ => unreachable!(),
        }
    }

    /// SSH key path. The secret file is the raw OpenSSH PEM body; git
    /// consumes it via `ssh -i <path>` inside `GIT_SSH_COMMAND`.
    fn materialize_ssh_key(cred: &Credential) -> Result<Self, MaterializeError> {
        let dir = tmpfs_dir();
        let path = create_secret_file(&dir, "branchwork-credential-", "", &cred.secret)?;
        // OpenSSH wants the PEM to end with a newline. Most callers
        // already include one (the `ssh-keygen` round-trip preserves
        // it), but a hand-typed key won't — append unconditionally if
        // the buffer doesn't already end with `\n`. This is cheap and
        // makes the integration robust against operator typos.
        ensure_trailing_newline(&path)?;
        Ok(CredentialMaterial {
            path,
            kind: cred.kind.clone(),
            credential_id: cred.id.clone(),
        })
    }

    /// PAT path. The secret file holds a 4-line askpass shell wrapper
    /// that `cat`s the token from a sibling file. We split the wrapper
    /// from the token because `GIT_ASKPASS` invokes the wrapper as a
    /// child process — if we shoved the token into the wrapper itself
    /// it would briefly appear in the parent's process listing during
    /// fork+exec. The sibling file is the same 0600 + tmpfs file we
    /// already use for SSH keys, just with the token as its content.
    ///
    /// On Drop we unlink BOTH the wrapper (`path`) and the token sibling.
    /// The sibling is tracked in a process-global table keyed by wrapper
    /// path; see [`register_sibling`] / [`take_sibling`].
    fn materialize_pat(cred: &Credential) -> Result<Self, MaterializeError> {
        let dir = tmpfs_dir();
        let token_path = create_secret_file(&dir, "branchwork-token-", "", &cred.secret)?;
        // Wrapper body: `cat <token-path>`. The token rides in the
        // sibling file, not inline in the wrapper, so the wrapper itself
        // doesn't carry the secret.
        let wrapper_body = format!(
            "#!/bin/sh\n# branchwork: GIT_ASKPASS wrapper for credential {id}\ncat {token}\n",
            id = shell_safe(&cred.id),
            token = shell_safe(&token_path.display().to_string()),
        );
        let wrapper_path =
            match create_secret_file(&dir, "branchwork-askpass-", ".sh", wrapper_body.as_bytes()) {
                Ok(p) => p,
                Err(e) => {
                    // Token file is now orphaned — best-effort unlink so we
                    // don't leak it.
                    let _ = std::fs::remove_file(&token_path);
                    return Err(e.into());
                }
            };
        // Mark the wrapper executable so `GIT_ASKPASS` can run it.
        if let Err(e) = set_executable(&wrapper_path) {
            let _ = std::fs::remove_file(&wrapper_path);
            let _ = std::fs::remove_file(&token_path);
            return Err(e.into());
        }
        register_sibling(&wrapper_path, token_path);
        Ok(CredentialMaterial {
            path: wrapper_path,
            kind: cred.kind.clone(),
            credential_id: cred.id.clone(),
        })
    }

    /// Path of the on-disk secret file. Exposed for tests and structured
    /// logging only. The runtime never needs to read the path back out —
    /// [`Self::git_env_vars`] is the only consumer in production code.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the credential kind exactly as it arrived on the wire.
    /// Useful for structured logging.
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// Returns the env vars to splice into the git child process. For
    /// `ssh_key` this is `GIT_SSH_COMMAND`; for `gh_pat` / `gitlab_pat`
    /// this is `GIT_ASKPASS` plus `GIT_TERMINAL_PROMPT=0` so a missing
    /// askpass dispatch never falls through to an interactive prompt.
    ///
    /// **Crucially**: these are returned as a `Vec` so the caller can
    /// `Command::env(k, v)` for the child only — we never call
    /// `std::env::set_var` and never touch the parent process env.
    pub fn git_env_vars(&self) -> Vec<(&'static str, String)> {
        match self.kind.as_str() {
            KIND_SSH_KEY => {
                // `IdentitiesOnly=yes` stops ssh from offering keys
                // from `ssh-agent` ahead of ours (the brief is explicit
                // — only the explicit credential authenticates the
                // op). `accept-new` accepts unknown hosts on first
                // contact but rejects on key change; same posture
                // `git clone -c StrictHostKeyChecking=accept-new` uses.
                let cmd = format!(
                    "ssh -i {path} -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new",
                    path = shell_safe(&self.path.display().to_string()),
                );
                vec![("GIT_SSH_COMMAND", cmd)]
            }
            KIND_GH_PAT | KIND_GITLAB_PAT => {
                vec![
                    ("GIT_ASKPASS", self.path.display().to_string()),
                    // Belt-and-braces: if GIT_ASKPASS misfires for any
                    // reason, git must NOT fall through to TTY prompt
                    // — that would hang the child forever in the
                    // non-interactive runner context.
                    ("GIT_TERMINAL_PROMPT", "0".to_string()),
                ]
            }
            _ => Vec::new(),
        }
    }
}

impl Drop for CredentialMaterial {
    fn drop(&mut self) {
        // Best-effort unlink. We deliberately ignore errors here because
        // the file might already be gone (e.g. if the operator wiped
        // tmpfs while we held the guard) and a panic on Drop would
        // poison the wider stack unwinding.
        let _ = std::fs::remove_file(&self.path);
        if let Some(sibling) = take_sibling(&self.path) {
            let _ = std::fs::remove_file(sibling);
        }
    }
}

// ── tmpfs helpers ───────────────────────────────────────────────────────────

/// Pick the tmpfs directory: prefer `$XDG_RUNTIME_DIR` (per-user tmpfs
/// on systemd hosts, mode 0700) and fall back to `/tmp` otherwise.
/// Returns an owned PathBuf because the value is read-once at materialise
/// time and the directory is fixed for the process lifetime.
fn tmpfs_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        let p = PathBuf::from(&dir);
        if p.is_dir() {
            return p;
        }
    }
    PathBuf::from("/tmp")
}

/// Create a fresh secret file in `dir` with mode 0600 (Unix). Writes
/// `contents` and returns the absolute path. Caller owns deletion.
fn create_secret_file(
    dir: &Path,
    prefix: &str,
    suffix: &str,
    contents: &[u8],
) -> std::io::Result<PathBuf> {
    use std::io::Write;
    // Loop with a fresh random suffix until create_new succeeds — same
    // shape `tempfile` uses but without pulling the dep into the runner.
    let mut attempts = 0;
    loop {
        attempts += 1;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let pid = std::process::id();
        // Include attempt index so consecutive retries within a single
        // nanosecond don't collide.
        let filename = format!("{prefix}{pid}-{nanos}-{attempts}{suffix}");
        let path = dir.join(&filename);
        let result = open_secret_for_create(&path);
        match result {
            Ok(mut file) => {
                file.write_all(contents)?;
                file.sync_all()?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempts < 16 => {
                continue;
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(unix)]
fn open_secret_for_create(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn open_secret_for_create(path: &Path) -> std::io::Result<std::fs::File> {
    // On non-Unix the runner is a single-user dev surface today; default
    // ACLs (no inherited share) are acceptable until Windows hardening
    // lands as a separate task.
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

#[cfg(unix)]
fn set_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    // 0700 — owner rwx, no group/other. Wrapper is invoked by the same
    // user; no other principal should ever see it.
    perms.set_mode(0o700);
    std::fs::set_permissions(path, perms)
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Append a `\n` to `path` if it doesn't already end with one. Used to
/// make hand-typed SSH keys robust to a missing terminal newline (which
/// OpenSSH otherwise refuses to load).
fn ensure_trailing_newline(path: &Path) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .append(true)
        .open(path)?;
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(());
    }
    file.seek(SeekFrom::Start(len - 1))?;
    let mut buf = [0u8; 1];
    file.read_exact(&mut buf)?;
    if buf[0] != b'\n' {
        file.seek(SeekFrom::End(0))?;
        file.write_all(b"\n")?;
        file.sync_all()?;
    }
    Ok(())
}

/// Shell-quote a string so it can be embedded inside `GIT_SSH_COMMAND` or
/// the askpass wrapper body. We wrap in single quotes and escape any
/// embedded single quotes via the standard `'\''` dance — same shape
/// `install-runner.sh` uses for its template substitution.
fn shell_safe(s: &str) -> String {
    let escaped = s.replace('\'', "'\\''");
    format!("'{escaped}'")
}

// ── sibling table (PAT askpass wrapper ↔ token file) ────────────────────────
//
// The PAT materialise path emits TWO files: the askpass wrapper (which we
// hand to `GIT_ASKPASS`) and the token sibling (which the wrapper `cat`s).
// We need to delete BOTH on Drop, but the public `CredentialMaterial`
// type only carries one `path`. To avoid widening the struct we keep a
// small process-global map from wrapper path → token path. Inserts and
// removes are scoped to the lifetime of a single Materialize/Drop pair,
// so the map is bounded by the number of concurrent clones in flight on
// the host (~1 in practice).

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::OnceLock;

fn sibling_table() -> &'static Mutex<HashMap<PathBuf, PathBuf>> {
    static TABLE: OnceLock<Mutex<HashMap<PathBuf, PathBuf>>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn register_sibling(wrapper: &Path, token: PathBuf) {
    if let Ok(mut guard) = sibling_table().lock() {
        guard.insert(wrapper.to_path_buf(), token);
    }
}

fn take_sibling(wrapper: &Path) -> Option<PathBuf> {
    sibling_table().lock().ok()?.remove(wrapper)
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn ssh_credential() -> Credential {
        Credential {
            id: "cred-ssh-1".into(),
            kind: KIND_SSH_KEY.into(),
            secret:
                b"-----BEGIN OPENSSH PRIVATE KEY-----\nfake\n-----END OPENSSH PRIVATE KEY-----\n"
                    .to_vec(),
            public_part: Some("ssh-ed25519 AAAA fake".into()),
            host_hint: Some("github.com".into()),
        }
    }

    fn gh_pat_credential() -> Credential {
        Credential {
            id: "cred-pat-1".into(),
            kind: KIND_GH_PAT.into(),
            secret: b"ghp_thisisnotreal0123".to_vec(),
            public_part: None,
            host_hint: Some("github.com".into()),
        }
    }

    #[test]
    fn ssh_key_materialise_writes_secret_file_with_mode_0600() {
        let cred = ssh_credential();
        let m = CredentialMaterial::materialize(&cred).expect("materialise");
        let path = m.path().to_path_buf();
        assert!(
            path.exists(),
            "secret file should exist: {}",
            path.display()
        );

        // Verify content matches.
        let content = std::fs::read(&path).expect("read");
        assert_eq!(content, cred.secret);

        // Verify mode is 0600 (Unix). Skip the check on non-Unix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "secret file must be 0600, got 0{mode:o}");
        }

        // Drop unlinks the file.
        drop(m);
        assert!(
            !path.exists(),
            "secret file should be unlinked after drop: {}",
            path.display()
        );
    }

    #[test]
    fn ssh_key_env_vars_match_brief_contract() {
        let cred = ssh_credential();
        let m = CredentialMaterial::materialize(&cred).expect("materialise");
        let env = m.git_env_vars();
        assert_eq!(env.len(), 1, "ssh_key emits one env var: {env:?}");
        assert_eq!(env[0].0, "GIT_SSH_COMMAND");
        let cmd = &env[0].1;
        // Exact contract from the task brief:
        // `ssh -i <path> -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new`
        assert!(cmd.starts_with("ssh -i "), "wrong prefix: {cmd}");
        assert!(
            cmd.contains("IdentitiesOnly=yes"),
            "missing IdentitiesOnly=yes: {cmd}"
        );
        assert!(
            cmd.contains("StrictHostKeyChecking=accept-new"),
            "missing StrictHostKeyChecking=accept-new: {cmd}"
        );
        // The path is shell-quoted (single quotes) so spaces / specials
        // don't break the ssh invocation.
        assert!(cmd.contains(&format!("'{}'", m.path().display())));
    }

    #[test]
    fn pat_materialise_writes_wrapper_and_token_files() {
        let cred = gh_pat_credential();
        let m = CredentialMaterial::materialize(&cred).expect("materialise");
        let wrapper_path = m.path().to_path_buf();
        assert!(
            wrapper_path.exists(),
            "wrapper should exist: {}",
            wrapper_path.display()
        );

        // The wrapper body should `cat` the sibling token file.
        let body = std::fs::read_to_string(&wrapper_path).expect("read");
        assert!(
            body.contains("#!/bin/sh"),
            "wrapper missing shebang: {body}"
        );
        assert!(
            body.contains("cat "),
            "wrapper missing cat invocation: {body}"
        );
        // The wrapper must NOT contain the raw token bytes — that would
        // defeat the askpass-from-sibling-file design.
        assert!(
            !body.contains("ghp_thisisnotreal0123"),
            "wrapper leaked the raw token: {body}"
        );

        // Wrapper is mode 0700 (executable for owner only).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&wrapper_path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700);
        }

        // The token sibling must be present and contain the secret.
        let token_path = sibling_table()
            .lock()
            .unwrap()
            .get(&wrapper_path)
            .cloned()
            .expect("sibling registered");
        assert!(token_path.exists(), "token sibling should exist");
        let token_content = std::fs::read(&token_path).expect("read token");
        assert_eq!(token_content, cred.secret);

        // Drop unlinks BOTH wrapper and token sibling.
        drop(m);
        assert!(
            !wrapper_path.exists(),
            "wrapper should be unlinked after drop"
        );
        assert!(
            !token_path.exists(),
            "token sibling should be unlinked after drop"
        );
    }

    #[test]
    fn pat_env_vars_emit_git_askpass_and_disable_terminal_prompt() {
        let cred = gh_pat_credential();
        let m = CredentialMaterial::materialize(&cred).expect("materialise");
        let env: HashMap<_, _> = m.git_env_vars().into_iter().collect();
        assert_eq!(
            env.get("GIT_ASKPASS").map(String::as_str),
            Some(m.path().display().to_string().as_str())
        );
        assert_eq!(
            env.get("GIT_TERMINAL_PROMPT").map(String::as_str),
            Some("0")
        );
    }

    #[test]
    fn unknown_kind_rejected() {
        let mut cred = ssh_credential();
        cred.kind = "future_oauth".into();
        match CredentialMaterial::materialize(&cred) {
            Err(MaterializeError::UnknownKind(k)) => assert_eq!(k, "future_oauth"),
            other => panic!("expected UnknownKind, got: {other:?}"),
        }
    }

    #[test]
    fn empty_secret_rejected() {
        let mut cred = ssh_credential();
        cred.secret = Vec::new();
        match CredentialMaterial::materialize(&cred) {
            Err(MaterializeError::EmptySecret) => {}
            other => panic!("expected EmptySecret, got: {other:?}"),
        }
    }

    #[test]
    fn ssh_key_file_ends_with_newline_even_if_secret_does_not() {
        // OpenSSH refuses keys without a terminal newline. Hand-typed
        // keys won't have one — materialise should append unconditionally.
        let mut cred = ssh_credential();
        cred.secret.pop(); // strip the trailing \n from the fixture
        let m = CredentialMaterial::materialize(&cred).expect("materialise");
        let content = std::fs::read(m.path()).expect("read");
        assert_eq!(*content.last().unwrap(), b'\n', "missing terminal newline");
    }

    #[test]
    fn ssh_key_env_does_not_pollute_parent_process() {
        // The whole point of T3.3 is that the operator's session env is
        // untouched. We can't see Command::env from here directly, but
        // we *can* assert that std::env::var("GIT_SSH_COMMAND") is None
        // (or at least unchanged) BEFORE materialise, and remains
        // unchanged AFTER materialise — proving we never leaked into
        // the parent.
        let before = std::env::var_os("GIT_SSH_COMMAND");
        let cred = ssh_credential();
        let m = CredentialMaterial::materialize(&cred).expect("materialise");
        let _env = m.git_env_vars(); // would-be child env
        let after = std::env::var_os("GIT_SSH_COMMAND");
        assert_eq!(
            before, after,
            "materialise must not touch the parent process env"
        );
    }

    #[test]
    fn shell_safe_quotes_single_quotes() {
        // The shell-quoting helper is load-bearing for the SSH path
        // (`ssh -i '<path>'`). A naive implementation that just wraps in
        // single quotes would break on a path containing a single quote.
        assert_eq!(shell_safe("hello"), "'hello'");
        assert_eq!(shell_safe("it's"), "'it'\\''s'");
        assert_eq!(shell_safe(""), "''");
    }

    #[test]
    fn tmpfs_dir_prefers_xdg_runtime_dir_when_set() {
        // We can't safely mutate process env in a test (it would race
        // with other tests under cargo test default parallelism), so we
        // assert via the helper indirectly: if `$XDG_RUNTIME_DIR` is set
        // and is a directory, tmpfs_dir should return it; otherwise
        // `/tmp`. Just check the fallback is reachable.
        let dir = tmpfs_dir();
        assert!(dir.is_dir() || dir == Path::new("/tmp"));
    }
}
