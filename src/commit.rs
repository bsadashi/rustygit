//! Commit object format.
//!
//! After the standard `commit <size>\0` framing applied by `RawObject`, the
//! commit body is a sequence of headers followed by a blank line and then a
//! free-form message:
//!
//! ```text
//! tree <hex-oid>\n
//! parent <hex-oid>\n            (zero or more)
//! author <Name> <<email>> <unix-secs> <±HHMM>\n
//! committer <Name> <<email>> <unix-secs> <±HHMM>\n
//! [encoding|gpgsig|mergetag headers, optional]\n
//! \n
//! <message bytes — opaque, may contain anything>
//! ```
//!
//! `gpgsig` headers span multiple lines: every continuation line begins with
//! a single space, which we strip on read and re-prepend on write. We
//! preserve `gpgsig` and `encoding` byte-for-byte for round-tripping objects
//! we *read*; but we don't *produce* either header from `commit`. The
//! `mergetag` header (older, also a folded multi-line header) is parsed and
//! discarded — re-serializing a commit that originally had a mergetag will
//! drop it, which is acceptable for M3 since `rustygit commit` itself never
//! produces them.

use thiserror::Error;

use crate::hash::{HashError, HashKind, ObjectId};
use crate::identity::{IdentityError, Signature};
use crate::object::{ObjectKind, RawObject};

#[derive(Debug, Clone)]
pub struct Commit {
    pub tree: ObjectId,
    pub parents: Vec<ObjectId>,
    pub author: Signature,
    pub committer: Signature,
    /// Opaque message bytes, including any trailing newline(s) that the
    /// caller wants preserved. We do *not* mutate or normalize them.
    pub message: Vec<u8>,
    /// `encoding` header, preserved on round-trip if present.
    pub encoding: Option<Vec<u8>>,
    /// `gpgsig` header, preserved on round-trip if present. Stored
    /// "unfolded" — the leading-space continuation prefix is removed during
    /// parsing and re-added during serialization.
    pub gpgsig: Option<Vec<u8>>,
}

impl Commit {
    /// Build a commit with no parents (typical for the very first commit).
    pub fn root(tree: ObjectId, author: Signature, committer: Signature, message: Vec<u8>) -> Self {
        Self {
            tree,
            parents: Vec::new(),
            author,
            committer,
            message,
            encoding: None,
            gpgsig: None,
        }
    }

    /// Parse the body of a commit object (post-framing).
    pub fn parse(body: &[u8], hash_kind: HashKind) -> Result<Self, CommitError> {
        // Find the blank-line separator: the first `\n\n`. Everything before
        // it is headers; everything after is the message.
        let split = find_double_newline(body).ok_or(CommitError::MissingMessageSeparator)?;
        let header_bytes = &body[..split];
        let message = body[split + 2..].to_vec();

        let mut tree: Option<ObjectId> = None;
        let mut parents: Vec<ObjectId> = Vec::new();
        let mut author: Option<Signature> = None;
        let mut committer: Option<Signature> = None;
        let mut encoding: Option<Vec<u8>> = None;
        let mut gpgsig: Option<Vec<u8>> = None;

        for header in HeaderIter::new(header_bytes) {
            let (key, value) = header?;
            if key == b"tree" {
                if tree.is_some() {
                    return Err(CommitError::DuplicateHeader("tree".into()));
                }
                let hex = std::str::from_utf8(&value)
                    .map_err(|_| CommitError::Malformed("tree oid not valid UTF-8".into()))?;
                tree = Some(ObjectId::parse_hex(hash_kind, hex.trim())?);
            } else if key == b"parent" {
                let hex = std::str::from_utf8(&value)
                    .map_err(|_| CommitError::Malformed("parent oid not valid UTF-8".into()))?;
                parents.push(ObjectId::parse_hex(hash_kind, hex.trim())?);
            } else if key == b"author" {
                if author.is_some() {
                    return Err(CommitError::DuplicateHeader("author".into()));
                }
                author = Some(Signature::parse(&value)?);
            } else if key == b"committer" {
                if committer.is_some() {
                    return Err(CommitError::DuplicateHeader("committer".into()));
                }
                committer = Some(Signature::parse(&value)?);
            } else if key == b"encoding" {
                encoding = Some(value);
            } else if key == b"gpgsig" {
                gpgsig = Some(value);
            } else {
                // Be permissive: ignore unknown headers (mergetag, HG:rename,
                // etc.) rather than erroring. Real-world commits in older
                // repos can have surprises and we don't want to refuse to
                // read them.
            }
        }

        let tree = tree.ok_or(CommitError::MissingHeader("tree"))?;
        let author = author.ok_or(CommitError::MissingHeader("author"))?;
        let committer = committer.ok_or(CommitError::MissingHeader("committer"))?;

        Ok(Self {
            tree,
            parents,
            author,
            committer,
            message,
            encoding,
            gpgsig,
        })
    }

    /// Serialize back to wire form.
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"tree ");
        out.extend_from_slice(self.tree.to_string().as_bytes());
        out.push(b'\n');
        for p in &self.parents {
            out.extend_from_slice(b"parent ");
            out.extend_from_slice(p.to_string().as_bytes());
            out.push(b'\n');
        }
        out.extend_from_slice(b"author ");
        out.extend_from_slice(self.author.serialize().as_bytes());
        out.push(b'\n');
        out.extend_from_slice(b"committer ");
        out.extend_from_slice(self.committer.serialize().as_bytes());
        out.push(b'\n');
        if let Some(enc) = &self.encoding {
            out.extend_from_slice(b"encoding ");
            out.extend_from_slice(enc);
            out.push(b'\n');
        }
        if let Some(sig) = &self.gpgsig {
            out.extend_from_slice(b"gpgsig ");
            // Re-fold continuation lines: every `\n` inside the sig becomes
            // `\n ` so the whole header is one logical block.
            let mut first = true;
            for line in sig.split(|&b| b == b'\n') {
                if !first {
                    out.push(b'\n');
                    out.push(b' ');
                }
                out.extend_from_slice(line);
                first = false;
            }
            out.push(b'\n');
        }
        out.push(b'\n');
        out.extend_from_slice(&self.message);
        out
    }

    /// Wrap `serialize()` with the `commit` object framing.
    pub fn to_object(&self) -> RawObject {
        RawObject::new(ObjectKind::Commit, self.serialize())
    }
}

/// Iterate logical commit headers, joining folded continuation lines.
///
/// Each yielded item is `(key_bytes, value_bytes)` where `key_bytes` is the
/// run of bytes before the first space on the first line, and `value_bytes`
/// is the rest (with subsequent ` `-prefixed continuation lines joined by
/// `\n` and the leading space stripped).
struct HeaderIter<'a> {
    rem: &'a [u8],
}

impl<'a> HeaderIter<'a> {
    fn new(rem: &'a [u8]) -> Self {
        Self { rem }
    }
}

impl<'a> Iterator for HeaderIter<'a> {
    type Item = Result<(&'a [u8], Vec<u8>), CommitError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.rem.is_empty() {
            return None;
        }
        // Each header starts with a non-space byte.
        let line_end = self
            .rem
            .iter()
            .position(|&b| b == b'\n')
            .unwrap_or(self.rem.len());
        let first_line = &self.rem[..line_end];
        // Advance past `\n` if present.
        let advance = if line_end < self.rem.len() {
            line_end + 1
        } else {
            line_end
        };
        self.rem = &self.rem[advance..];

        if first_line.is_empty() {
            return Some(Err(CommitError::Malformed(
                "unexpected blank line in headers".into(),
            )));
        }
        if first_line[0] == b' ' {
            return Some(Err(CommitError::Malformed(
                "header continuation without preceding header".into(),
            )));
        }

        let space = match first_line.iter().position(|&b| b == b' ') {
            Some(i) => i,
            None => {
                // A header with no value (no space). Treat the whole line as the key.
                return Some(Ok((first_line, Vec::new())));
            }
        };
        let key = &first_line[..space];
        let mut value = first_line[space + 1..].to_vec();

        // Join folded continuations: every line that starts with a single
        // space belongs to this header. We strip exactly one leading space.
        while !self.rem.is_empty() && self.rem[0] == b' ' {
            let cont_end = self
                .rem
                .iter()
                .position(|&b| b == b'\n')
                .unwrap_or(self.rem.len());
            let cont = &self.rem[1..cont_end]; // strip the leading space
            let advance = if cont_end < self.rem.len() {
                cont_end + 1
            } else {
                cont_end
            };
            value.push(b'\n');
            value.extend_from_slice(cont);
            self.rem = &self.rem[advance..];
        }

        Some(Ok((key, value)))
    }
}

/// Find the first occurrence of `\n\n`, returning the index of the first `\n`.
fn find_double_newline(data: &[u8]) -> Option<usize> {
    (0..data.len().saturating_sub(1)).find(|&i| data[i] == b'\n' && data[i + 1] == b'\n')
}

#[derive(Error, Debug)]
pub enum CommitError {
    #[error("commit body has no header/message separator (missing blank line)")]
    MissingMessageSeparator,
    #[error("commit missing required header: {0}")]
    MissingHeader(&'static str),
    #[error("commit has duplicate {0} header")]
    DuplicateHeader(String),
    #[error("malformed commit: {0}")]
    Malformed(String),
    #[error(transparent)]
    Hash(#[from] HashError),
    #[error(transparent)]
    Identity(#[from] IdentityError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Time;

    fn fake_oid_sha1(byte: u8) -> ObjectId {
        ObjectId::from_bytes(HashKind::Sha1, &[byte; 20]).unwrap()
    }

    #[test]
    fn parse_and_serialize_minimal() {
        let body = "\
tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904
author Alice <a@example.com> 1700000000 +0000
committer Alice <a@example.com> 1700000000 +0000

initial commit
";
        let commit = Commit::parse(body.as_bytes(), HashKind::Sha1).unwrap();
        assert_eq!(
            commit.tree.to_string(),
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
        );
        assert!(commit.parents.is_empty());
        assert_eq!(commit.author.name, "Alice");
        assert_eq!(commit.author.email, "a@example.com");
        assert_eq!(commit.author.when.seconds, 1700000000);
        assert_eq!(commit.message, b"initial commit\n");

        let bytes = commit.serialize();
        assert_eq!(bytes, body.as_bytes());
    }

    #[test]
    fn parse_with_two_parents() {
        let body = "\
tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904
parent aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
parent bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
author Bob <b@example.com> 100 -0500
committer Bob <b@example.com> 100 -0500

merge it
";
        let commit = Commit::parse(body.as_bytes(), HashKind::Sha1).unwrap();
        assert_eq!(commit.parents.len(), 2);
        assert_eq!(
            commit.parents[0].to_string(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert_eq!(commit.author.when.offset_minutes, -300);
        let bytes = commit.serialize();
        assert_eq!(bytes, body.as_bytes());
    }

    #[test]
    fn build_from_scratch_round_trips() {
        let tree = fake_oid_sha1(0xab);
        let author = Signature::new("X", "x@y.z", Time::new(123, 0));
        let committer = author.clone();
        let commit = Commit::root(tree, author, committer, b"hello\n".to_vec());
        let bytes = commit.serialize();
        let parsed = Commit::parse(&bytes, HashKind::Sha1).unwrap();
        assert_eq!(parsed.tree, tree);
        assert_eq!(parsed.author.name, "X");
        assert_eq!(parsed.message, b"hello\n");
    }

    #[test]
    fn message_with_internal_blank_lines() {
        // The first `\n\n` after the headers separates them from the
        // message. Subsequent blank lines inside the message must be kept
        // verbatim.
        let body = "\
tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904
author X <x@y.z> 1 +0000
committer X <x@y.z> 1 +0000

subject line

body para 1

body para 2
";
        let commit = Commit::parse(body.as_bytes(), HashKind::Sha1).unwrap();
        assert_eq!(
            commit.message,
            b"subject line\n\nbody para 1\n\nbody para 2\n"
        );
        assert_eq!(commit.serialize(), body.as_bytes());
    }

    #[test]
    fn empty_message_is_ok() {
        let body = "\
tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904
author X <x@y.z> 1 +0000
committer X <x@y.z> 1 +0000

";
        let commit = Commit::parse(body.as_bytes(), HashKind::Sha1).unwrap();
        assert!(commit.message.is_empty());
        assert_eq!(commit.serialize(), body.as_bytes());
    }

    #[test]
    fn parse_preserves_encoding_header() {
        let body = "\
tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904
author X <x@y.z> 1 +0000
committer X <x@y.z> 1 +0000
encoding ISO-8859-1

cafe
";
        let commit = Commit::parse(body.as_bytes(), HashKind::Sha1).unwrap();
        assert_eq!(commit.encoding.as_deref(), Some(b"ISO-8859-1".as_ref()));
        assert_eq!(commit.serialize(), body.as_bytes());
    }

    #[test]
    fn parse_round_trip_with_gpgsig() {
        // gpgsig is folded over multiple continuation lines (each starting
        // with a single space). The "blank" line inside the PGP armor is
        // itself a continuation: ` \n` — a space + newline — which strips to
        // an empty byte run. We unfold on read and re-fold on write. We
        // construct the bytes explicitly rather than as a string literal so
        // the trailing-space-only line is unambiguous.
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n");
        body.extend_from_slice(b"author X <x@y.z> 1 +0000\n");
        body.extend_from_slice(b"committer X <x@y.z> 1 +0000\n");
        body.extend_from_slice(b"gpgsig -----BEGIN PGP SIGNATURE-----\n");
        body.extend_from_slice(b" \n");
        body.extend_from_slice(b" iQEcBAABAgAGBQJVw...\n");
        body.extend_from_slice(b" ...moredata...\n");
        body.extend_from_slice(b" -----END PGP SIGNATURE-----\n");
        body.extend_from_slice(b"\n");
        body.extend_from_slice(b"signed commit\n");

        let commit = Commit::parse(&body, HashKind::Sha1).unwrap();
        assert!(commit.gpgsig.is_some());
        // The unfolded sig should contain the empty line and BEGIN/END markers.
        let sig = commit.gpgsig.as_ref().unwrap();
        assert!(sig.starts_with(b"-----BEGIN PGP SIGNATURE-----"));
        assert!(sig.ends_with(b"-----END PGP SIGNATURE-----"));
        assert_eq!(commit.serialize(), body);
    }

    #[test]
    fn missing_tree_errors() {
        let body = "\
author X <x@y.z> 1 +0000
committer X <x@y.z> 1 +0000

oops
";
        let err = Commit::parse(body.as_bytes(), HashKind::Sha1).unwrap_err();
        assert!(matches!(err, CommitError::MissingHeader("tree")));
    }

    #[test]
    fn missing_message_separator_errors() {
        let body = "\
tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904
author X <x@y.z> 1 +0000
committer X <x@y.z> 1 +0000
";
        // No trailing blank line → no separator.
        let err = Commit::parse(body.as_bytes(), HashKind::Sha1).unwrap_err();
        assert!(matches!(err, CommitError::MissingMessageSeparator));
    }

    /// Build a commit programmatically and verify that running it through
    /// real `git cat-file -p` (in a tempdir) yields the expected output.
    /// Skipped silently if `git` isn't on PATH.
    #[test]
    fn cat_file_p_matches_real_git() {
        if !has_git() {
            return;
        }
        use std::process::Command;
        let dir = tempfile::tempdir().unwrap();
        // `git init` to set up the object store.
        let status = Command::new("git")
            .arg("init")
            .arg("-q")
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success());

        // Write the empty tree first.
        let empty_tree_status = Command::new("git")
            .args(["hash-object", "-t", "tree", "-w", "--stdin"])
            .current_dir(dir.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                if let Some(mut stdin) = child.stdin.take() {
                    stdin.write_all(b"").unwrap();
                }
                child.wait_with_output()
            });
        let tree_oid_str = match empty_tree_status {
            Ok(out) if out.status.success() => {
                String::from_utf8(out.stdout).unwrap().trim().to_string()
            }
            _ => return, // best-effort
        };
        let tree_oid = ObjectId::parse_hex(HashKind::Sha1, &tree_oid_str).unwrap();

        // Build our Commit and write its raw bytes via `git hash-object -t commit`.
        let when = Time::new(1700000000, 0);
        let author = Signature::new("Alice", "a@example.com", when);
        let committer = author.clone();
        let commit = Commit::root(tree_oid, author, committer, b"a message\n".to_vec());
        let bytes = commit.serialize();

        let hash = Command::new("git")
            .args(["hash-object", "-t", "commit", "-w", "--stdin"])
            .current_dir(dir.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                if let Some(mut stdin) = child.stdin.take() {
                    stdin.write_all(&bytes).unwrap();
                }
                child.wait_with_output()
            })
            .unwrap();
        assert!(hash.status.success(), "hash-object failed: {:?}", hash);
        let oid_str = String::from_utf8(hash.stdout).unwrap().trim().to_string();

        // Now `git cat-file -p` it back and compare.
        let pretty = Command::new("git")
            .args(["cat-file", "-p", &oid_str])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(pretty.status.success());
        let pretty_str = std::str::from_utf8(&pretty.stdout).unwrap();
        assert!(pretty_str.contains(&format!("tree {}", tree_oid_str)));
        assert!(pretty_str.contains("author Alice <a@example.com> 1700000000 +0000"));
        assert!(pretty_str.contains("committer Alice <a@example.com> 1700000000 +0000"));
        assert!(pretty_str.ends_with("a message\n"));
    }

    /// Have git produce a commit with `commit-tree` and parse it back.
    #[test]
    fn parses_real_git_commit_tree_output() {
        if !has_git() {
            return;
        }
        use std::process::Command;
        let dir = tempfile::tempdir().unwrap();
        let status = Command::new("git")
            .arg("init")
            .arg("-q")
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success());

        // Create empty tree.
        let mt = Command::new("git")
            .args(["hash-object", "-t", "tree", "-w", "--stdin"])
            .current_dir(dir.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                if let Some(mut stdin) = child.stdin.take() {
                    stdin.write_all(b"").unwrap();
                }
                child.wait_with_output()
            })
            .unwrap();
        let tree_oid_str = String::from_utf8(mt.stdout).unwrap().trim().to_string();

        // Use `commit-tree` with explicit env to make the output deterministic.
        let mut cmd = Command::new("git");
        cmd.args(["commit-tree", &tree_oid_str, "-m", "deterministic"]);
        cmd.env("GIT_AUTHOR_NAME", "Tester");
        cmd.env("GIT_AUTHOR_EMAIL", "t@e.x");
        cmd.env("GIT_AUTHOR_DATE", "1700000000 +0000");
        cmd.env("GIT_COMMITTER_NAME", "Tester");
        cmd.env("GIT_COMMITTER_EMAIL", "t@e.x");
        cmd.env("GIT_COMMITTER_DATE", "1700000000 +0000");
        cmd.current_dir(dir.path());
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{:?}", out);
        let oid_str = String::from_utf8(out.stdout).unwrap().trim().to_string();

        // Read the commit body via `git cat-file commit <oid>`.
        let body_out = Command::new("git")
            .args(["cat-file", "commit", &oid_str])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(body_out.status.success());
        let body = body_out.stdout;

        // Parse with our code.
        let commit = Commit::parse(&body, HashKind::Sha1).unwrap();
        assert_eq!(commit.tree.to_string(), tree_oid_str);
        assert_eq!(commit.author.name, "Tester");
        assert_eq!(commit.author.email, "t@e.x");
        assert_eq!(commit.author.when.seconds, 1700000000);
        assert_eq!(commit.author.when.offset_minutes, 0);
        assert_eq!(commit.committer.name, "Tester");
        assert_eq!(commit.message, b"deterministic\n");

        // Round-trip: our serialize should reproduce git's bytes exactly.
        assert_eq!(commit.serialize(), body);
    }

    fn has_git() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}
