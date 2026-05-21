//! GPG signing/verifying — NON_GOALS.md Batch F.
//!
//! Approach: shell out. Git itself shells out to `gpg`; doing the same here
//! keeps key management, smartcard support, key servers, expiration, etc.
//! out of rustygit's scope. We just pipe the payload to `gpg` over stdin and
//! pipe the signature out over stdout.
//!
//! Configuration knobs honored (matching upstream git semantics):
//!
//! - `gpg.program` — the gpg binary to invoke. Default `gpg`.
//! - `user.signingkey` — the key id passed via `--local-user`. Optional.
//! - `commit.gpgsign` / `tag.gpgsign` — boolean enabling sign-by-default.
//!   Honored in `cli::commit::run` (this module is the primitive; the
//!   commit porcelain decides whether to call it).
//!
//! Out of scope:
//! - SSH and X.509 signing formats (git supports `gpg.format = ssh` /
//!   `x509`). Today we always invoke `gpg`. A future enum
//!   `SigningFormat::{OpenPgp, Ssh, X509}` can extend this module without
//!   changing the commit-porcelain integration point.
//!
//! ## Testing approach
//!
//! `Signer` is a trait. `GpgSigner` is the production impl that spawns the
//! configured gpg binary. Unit tests use `MockSigner` to verify the
//! commit/tag integration without a real gpg installation. Integration
//! tests in `tests/non_goals_signing.rs` gate on `has_gpg()` and set up a
//! disposable GNUPGHOME with a generated key.

use std::io::Write;
use std::process::{Command, Stdio};

use thiserror::Error;

/// Result of a successful signing operation: ASCII-armored OpenPGP signature
/// bytes, as produced by `gpg --bsa`. Includes the `-----BEGIN PGP
/// SIGNATURE-----` / `-----END PGP SIGNATURE-----` framing lines — the
/// caller folds this into the commit's `gpgsig` header verbatim.
pub type Signature = Vec<u8>;

/// What the verifier found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyOutcome {
    /// Signature valid and verifiable with the keyring we have.
    Good {
        /// The signing key's fingerprint (uppercase hex) as reported by gpg.
        fingerprint: Option<String>,
        /// The signer's identity string from gpg (e.g. `"Daisy <d@e>"`).
        signer: Option<String>,
    },
    /// Signature was syntactically valid but the key isn't in our keyring.
    UnknownKey,
    /// Signature failed verification (tampered, malformed, etc.).
    Bad { reason: String },
}

#[derive(Debug, Error)]
pub enum SignError {
    #[error("gpg program '{program}' not found or not runnable: {source}")]
    GpgUnavailable {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error("gpg failed (exit {code:?}): {stderr}")]
    GpgFailed { code: Option<i32>, stderr: String },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("gpg returned an empty signature")]
    EmptySignature,
}

/// Abstract over the signing call so commit/tag porcelain can inject a
/// mock signer in tests.
pub trait Signer: Send + Sync {
    /// Sign `payload` (the unsigned commit/tag body, raw bytes). Returns an
    /// ASCII-armored signature ready to fold into a `gpgsig` header.
    fn sign(&self, payload: &[u8]) -> Result<Signature, SignError>;

    /// Verify `signature` over `payload`. Returns the parsed outcome.
    fn verify(&self, payload: &[u8], signature: &[u8]) -> Result<VerifyOutcome, SignError>;
}

/// Production signer: spawns the configured gpg binary.
///
/// `program` defaults to `"gpg"`. `key_id` is optional — when set we pass
/// `--local-user <id>` so gpg picks the right key on a multi-key keyring.
pub struct GpgSigner {
    pub program: String,
    pub key_id: Option<String>,
}

impl GpgSigner {
    pub fn new(program: impl Into<String>, key_id: Option<String>) -> Self {
        Self {
            program: program.into(),
            key_id,
        }
    }

    /// Build a `GpgSigner` from `[gpg]program` / `user.signingkey` config
    /// values, with reasonable defaults applied.
    pub fn from_config(config: &crate::config::Config) -> Self {
        let program = config
            .get_string("gpg", "program")
            .map(str::to_string)
            .unwrap_or_else(|| "gpg".to_string());
        let key_id = config.get_string("user", "signingkey").map(str::to_string);
        Self { program, key_id }
    }
}

impl Signer for GpgSigner {
    fn sign(&self, payload: &[u8]) -> Result<Signature, SignError> {
        // `gpg --bsa` = detached, binary-compatible, armored. Combined with
        // `--detach-sign --armor --batch --pinentry-mode loopback` matches
        // what upstream git does. `--batch` avoids pinentry prompts when
        // possible; with a passphraseless key (the typical test setup),
        // this Just Works.
        let mut cmd = Command::new(&self.program);
        cmd.args([
            "--detach-sign",
            "--armor",
            "--batch",
            // gpg2 changed prompt behavior; force the loopback mode so a
            // pinentry program isn't required.
            "--pinentry-mode",
            "loopback",
        ]);
        if let Some(key) = &self.key_id {
            cmd.args(["--local-user", key]);
        }
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(|e| SignError::GpgUnavailable {
            program: self.program.clone(),
            source: e,
        })?;
        {
            let stdin = child.stdin.as_mut().expect("stdin should be piped");
            stdin.write_all(payload)?;
        }
        let out = child.wait_with_output()?;
        if !out.status.success() {
            return Err(SignError::GpgFailed {
                code: out.status.code(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
        }
        if out.stdout.is_empty() {
            return Err(SignError::EmptySignature);
        }
        Ok(out.stdout)
    }

    fn verify(&self, payload: &[u8], signature: &[u8]) -> Result<VerifyOutcome, SignError> {
        // Two-file detached verify: write the sig to a tempfile, pipe payload
        // on stdin via `--`. The simplest cross-platform approach is to write
        // both to tempfiles to avoid pty issues with `--verify -- - <sigfile>`.
        let tmp = tempfile::TempDir::new()?;
        let sig_path = tmp.path().join("sig.asc");
        let pay_path = tmp.path().join("payload");
        std::fs::write(&sig_path, signature)?;
        std::fs::write(&pay_path, payload)?;

        let mut cmd = Command::new(&self.program);
        cmd.args([
            "--verify",
            "--status-fd",
            "2", // status-fd to stderr (we'll parse it)
            sig_path.to_str().expect("ascii tmpdir"),
            pay_path.to_str().expect("ascii tmpdir"),
        ]);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        let out = cmd.output().map_err(|e| SignError::GpgUnavailable {
            program: self.program.clone(),
            source: e,
        })?;
        let stderr = String::from_utf8_lossy(&out.stderr);

        // gpg's machine-readable status lines start with `[GNUPG:]`. The
        // ones we care about:
        //   `[GNUPG:] GOODSIG <keyid> <name>`
        //   `[GNUPG:] BADSIG <keyid> <name>`
        //   `[GNUPG:] ERRSIG <keyid> ...`  ← typically NO_PUBKEY follows
        //   `[GNUPG:] NO_PUBKEY <keyid>`
        //   `[GNUPG:] VALIDSIG <fpr> ...`
        let mut fingerprint: Option<String> = None;
        let mut signer: Option<String> = None;
        let mut saw_good = false;
        let mut saw_bad = false;
        let mut no_pubkey = false;
        for line in stderr.lines() {
            if let Some(rest) = line.strip_prefix("[GNUPG:] GOODSIG ") {
                saw_good = true;
                // rest = "<keyid> <signer-name-with-spaces>"
                let mut parts = rest.splitn(2, ' ');
                let _keyid = parts.next();
                signer = parts.next().map(str::to_string);
            } else if line.starts_with("[GNUPG:] BADSIG ") {
                saw_bad = true;
            } else if line.starts_with("[GNUPG:] NO_PUBKEY") {
                no_pubkey = true;
            } else if let Some(rest) = line.strip_prefix("[GNUPG:] VALIDSIG ") {
                fingerprint = rest.split_whitespace().next().map(str::to_string);
            }
        }

        if saw_bad {
            return Ok(VerifyOutcome::Bad {
                reason: "BADSIG reported by gpg".to_string(),
            });
        }
        if no_pubkey {
            return Ok(VerifyOutcome::UnknownKey);
        }
        if saw_good && out.status.success() {
            return Ok(VerifyOutcome::Good {
                fingerprint,
                signer,
            });
        }
        // Catch-all: gpg returned non-success without an explicit BADSIG —
        // treat as bad with the stderr as the reason.
        Ok(VerifyOutcome::Bad {
            reason: stderr.into_owned(),
        })
    }
}

// ---------------------------------------------------------------------------
// SSH signing (`gpg.format = ssh`)
// ---------------------------------------------------------------------------

/// SSH-format signer: wraps `ssh-keygen -Y sign/verify`.
///
/// The signature format is "SSHSIG" — an ASCII-armored block beginning
/// with `-----BEGIN SSH SIGNATURE-----` and ending with
/// `-----END SSH SIGNATURE-----`. Git stores it in the same `gpgsig`
/// commit header / signed-tag message trailer as GPG signatures; the
/// recipient detects the format by the BEGIN line.
pub struct SshSigner {
    /// Path to the private key (or pub key for verify).
    pub key_path: String,
    /// Path to the allowed-signers file (verify only). When `None`,
    /// verification will fail with `UnknownKey`.
    pub allowed_signers: Option<String>,
}

impl SshSigner {
    pub fn new(key_path: impl Into<String>, allowed_signers: Option<String>) -> Self {
        Self {
            key_path: key_path.into(),
            allowed_signers,
        }
    }

    pub fn from_config(config: &crate::config::Config) -> Self {
        let key_path = config
            .get_string("user", "signingkey")
            .map(str::to_string)
            .unwrap_or_default();
        let allowed = config
            .get_string("gpg", "ssh.allowedSignersFile")
            .map(str::to_string);
        Self {
            key_path,
            allowed_signers: allowed,
        }
    }
}

impl Signer for SshSigner {
    fn sign(&self, payload: &[u8]) -> Result<Signature, SignError> {
        // ssh-keygen reads input from stdin when -O sign-with-stdin is used,
        // but the portable invocation feeds a temp file.
        let tmp = tempfile::tempdir()?;
        let payload_path = tmp.path().join("payload");
        std::fs::write(&payload_path, payload)?;
        let mut cmd = Command::new("ssh-keygen");
        cmd.args(["-Y", "sign", "-f", &self.key_path, "-n", "git"]);
        cmd.arg(&payload_path);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        let out = cmd.output().map_err(|e| SignError::GpgUnavailable {
            program: "ssh-keygen".to_string(),
            source: e,
        })?;
        if !out.status.success() {
            return Err(SignError::GpgFailed {
                code: out.status.code(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
        }
        // ssh-keygen writes the .sig next to the input, not to stdout.
        let sig_path = format!("{}.sig", payload_path.display());
        let sig = std::fs::read(&sig_path)?;
        if sig.is_empty() {
            return Err(SignError::EmptySignature);
        }
        Ok(sig)
    }

    fn verify(&self, payload: &[u8], signature: &[u8]) -> Result<VerifyOutcome, SignError> {
        let Some(allowed) = &self.allowed_signers else {
            return Ok(VerifyOutcome::UnknownKey);
        };
        let tmp = tempfile::tempdir()?;
        let payload_path = tmp.path().join("payload");
        std::fs::write(&payload_path, payload)?;
        let sig_path = tmp.path().join("payload.sig");
        std::fs::write(&sig_path, signature)?;
        let out = Command::new("ssh-keygen")
            .args([
                "-Y",
                "verify",
                "-f",
                allowed,
                "-n",
                "git",
                "-s",
                sig_path.to_str().unwrap_or(""),
                "-I",
                "*",
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| SignError::GpgUnavailable {
                program: "ssh-keygen".to_string(),
                source: e,
            })?;
        if out.status.success() {
            return Ok(VerifyOutcome::Good {
                fingerprint: None,
                signer: None,
            });
        }
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        if stderr.contains("not found") {
            return Ok(VerifyOutcome::UnknownKey);
        }
        Ok(VerifyOutcome::Bad { reason: stderr })
    }
}

/// Pick the right signer based on `gpg.format` config. Default is GPG.
pub fn signer_from_config(config: &crate::config::Config) -> Box<dyn Signer> {
    match config.get_string("gpg", "format") {
        Some("ssh") => Box::new(SshSigner::from_config(config)),
        _ => Box::new(GpgSigner::from_config(config)),
    }
}

pub mod testing {
    //! Test doubles for porcelain integration tests that don't have a real
    //! gpg installation.
    //!
    //! Lives outside `#[cfg(test)]` so the `tests/` directory (which links
    //! against the release library) can use it. The cost is a small amount
    //! of unused code in the binary; the benefit is shareable mocks.

    use super::*;
    use std::sync::Mutex;

    /// A signer that returns a fixed ASCII signature and a fixed verify
    /// outcome. Records the payloads it saw for assertion.
    pub struct MockSigner {
        pub signature: Vec<u8>,
        pub verify: VerifyOutcome,
        pub signed_payloads: Mutex<Vec<Vec<u8>>>,
    }

    impl MockSigner {
        pub fn good() -> Self {
            Self {
                signature: b"-----BEGIN PGP SIGNATURE-----\n\
                             \n\
                             iQABATFAKE\n\
                             -----END PGP SIGNATURE-----\n"
                    .to_vec(),
                verify: VerifyOutcome::Good {
                    fingerprint: Some("DEADBEEFCAFEF00DDEADBEEFCAFEF00DDEADBEEF".to_string()),
                    signer: Some("Daisy <d@e>".to_string()),
                },
                signed_payloads: Mutex::new(Vec::new()),
            }
        }
    }

    impl Signer for MockSigner {
        fn sign(&self, payload: &[u8]) -> Result<Signature, SignError> {
            self.signed_payloads.lock().unwrap().push(payload.to_vec());
            Ok(self.signature.clone())
        }

        fn verify(&self, _payload: &[u8], _signature: &[u8]) -> Result<VerifyOutcome, SignError> {
            Ok(self.verify.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `gpgsig` headers fold continuation lines with a leading single space.
    /// This module emits the raw signature; the commit serializer folds it.
    /// We don't test folding here — that lives in `commit.rs`. But we DO
    /// test that the signer trait returns the raw bytes verbatim.
    #[test]
    fn mock_signer_records_payload() {
        use super::testing::MockSigner;
        let m = MockSigner::good();
        let sig = m.sign(b"tree abc\nauthor x\n\nmsg\n").unwrap();
        assert!(sig.starts_with(b"-----BEGIN PGP SIGNATURE-----"));
        assert_eq!(m.signed_payloads.lock().unwrap().len(), 1);
        assert_eq!(
            m.signed_payloads.lock().unwrap()[0],
            b"tree abc\nauthor x\n\nmsg\n"
        );
    }

    #[test]
    fn mock_signer_verify_returns_configured_outcome() {
        use super::testing::MockSigner;
        let m = MockSigner::good();
        let v = m.verify(b"payload", b"sig").unwrap();
        match v {
            VerifyOutcome::Good {
                fingerprint,
                signer,
            } => {
                assert!(fingerprint.is_some());
                assert_eq!(signer.as_deref(), Some("Daisy <d@e>"));
            }
            other => panic!("expected Good, got {other:?}"),
        }
    }

    /// `GpgSigner::from_config` falls back to `"gpg"` and no key when the
    /// config doesn't specify either. (We can't easily test "gpg.program is
    /// honored" without mocking Config; this is the simpler half.)
    #[test]
    fn gpg_signer_defaults_when_config_silent() {
        let tmp = tempfile::tempdir().unwrap();
        let fake_gitdir = tmp.path().join(".git");
        std::fs::create_dir_all(&fake_gitdir).unwrap();
        // Empty config file is the most realistic "silent" state.
        std::fs::write(fake_gitdir.join("config"), b"").unwrap();
        let cfg = crate::config::Config::from_repo_dir(&fake_gitdir).unwrap();
        let s = GpgSigner::from_config(&cfg);
        assert_eq!(s.program, "gpg");
        assert!(s.key_id.is_none());
    }

    #[test]
    fn gpg_signer_honors_config_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let fake_gitdir = tmp.path().join(".git");
        std::fs::create_dir_all(&fake_gitdir).unwrap();
        std::fs::write(
            fake_gitdir.join("config"),
            b"[gpg]\n\tprogram = /usr/local/bin/gpg2\n[user]\n\tsigningkey = ABCDEF12\n",
        )
        .unwrap();
        let cfg = crate::config::Config::from_repo_dir(&fake_gitdir).unwrap();
        let s = GpgSigner::from_config(&cfg);
        assert_eq!(s.program, "/usr/local/bin/gpg2");
        assert_eq!(s.key_id.as_deref(), Some("ABCDEF12"));
    }
}
