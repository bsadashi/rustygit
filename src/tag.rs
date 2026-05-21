//! Tag object format.
//!
//! After the standard `tag <size>\0` framing applied by `RawObject`, the
//! tag body is a fixed-shape header block followed by a blank line and
//! then a free-form message:
//!
//! ```text
//! object <hex-oid>\n
//! type <commit|tree|blob|tag>\n
//! tag <name>\n
//! tagger <Name> <<email>> <unix-secs> <±HHMM>\n
//! \n
//! <message bytes — opaque>
//! ```
//!
//! Signed tags append `-----BEGIN PGP SIGNATURE-----\n…\n-----END PGP
//! SIGNATURE-----\n` to the message body itself (NOT as a header, the way
//! `gpgsig` works in commits). For now we preserve the message bytes
//! verbatim, so a signed tag round-trips correctly even though we don't
//! yet have a porcelain path to *create* one.

use thiserror::Error;

use crate::hash::{HashError, HashKind, ObjectId};
use crate::identity::{IdentityError, Signature};
use crate::object::{ObjectKind, RawObject};

#[derive(Debug, Clone)]
pub struct Tag {
    pub object: ObjectId,
    pub kind: ObjectKind,
    /// The short tag name (e.g. `v1.0`, NOT `refs/tags/v1.0`).
    pub name: Vec<u8>,
    pub tagger: Option<Signature>,
    /// Opaque message bytes — preserved verbatim including any embedded
    /// PGP signature trailer.
    pub message: Vec<u8>,
}

impl Tag {
    pub fn new(
        object: ObjectId,
        kind: ObjectKind,
        name: Vec<u8>,
        tagger: Signature,
        message: Vec<u8>,
    ) -> Self {
        Self {
            object,
            kind,
            name,
            tagger: Some(tagger),
            message,
        }
    }

    /// Parse a tag-object body (post-framing).
    pub fn parse(body: &[u8], hash_kind: HashKind) -> Result<Self, TagError> {
        let split = body
            .windows(2)
            .position(|w| w == b"\n\n")
            .ok_or(TagError::MissingMessageSeparator)?;
        let header_bytes = &body[..split];
        let message = body[split + 2..].to_vec();

        let mut object: Option<ObjectId> = None;
        let mut kind: Option<ObjectKind> = None;
        let mut name: Option<Vec<u8>> = None;
        let mut tagger: Option<Signature> = None;

        for line in header_bytes.split(|&b| b == b'\n') {
            if line.is_empty() {
                continue;
            }
            let space = line
                .iter()
                .position(|&b| b == b' ')
                .ok_or_else(|| TagError::Malformed("header without value".into()))?;
            let key = &line[..space];
            let value = &line[space + 1..];

            if key == b"object" {
                if object.is_some() {
                    return Err(TagError::DuplicateHeader("object"));
                }
                let hex = std::str::from_utf8(value)
                    .map_err(|_| TagError::Malformed("object oid not valid UTF-8".into()))?;
                object = Some(ObjectId::parse_hex(hash_kind, hex.trim())?);
            } else if key == b"type" {
                let s = std::str::from_utf8(value)
                    .map_err(|_| TagError::Malformed("type not valid UTF-8".into()))?
                    .trim();
                kind = Some(match s {
                    "blob" => ObjectKind::Blob,
                    "tree" => ObjectKind::Tree,
                    "commit" => ObjectKind::Commit,
                    "tag" => ObjectKind::Tag,
                    other => return Err(TagError::Malformed(format!("unknown type {other:?}"))),
                });
            } else if key == b"tag" {
                name = Some(value.to_vec());
            } else if key == b"tagger" {
                tagger = Some(Signature::parse(value)?);
            } else {
                // Permissive on unknown headers — git doesn't define any
                // others today but a future revision might.
            }
        }

        let object = object.ok_or(TagError::MissingHeader("object"))?;
        let kind = kind.ok_or(TagError::MissingHeader("type"))?;
        let name = name.ok_or(TagError::MissingHeader("tag"))?;
        Ok(Self {
            object,
            kind,
            name,
            tagger,
            message,
        })
    }

    /// Serialize the tag body (un-framed). Caller wraps with
    /// [`Self::to_object`] for the final `tag <len>\0…` representation.
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"object ");
        out.extend_from_slice(self.object.to_string().as_bytes());
        out.push(b'\n');
        out.extend_from_slice(b"type ");
        out.extend_from_slice(self.kind.as_str().as_bytes());
        out.push(b'\n');
        out.extend_from_slice(b"tag ");
        out.extend_from_slice(&self.name);
        out.push(b'\n');
        if let Some(tagger) = &self.tagger {
            out.extend_from_slice(b"tagger ");
            out.extend_from_slice(tagger.serialize().as_bytes());
            out.push(b'\n');
        }
        out.push(b'\n');
        out.extend_from_slice(&self.message);
        out
    }

    pub fn to_object(&self) -> RawObject {
        RawObject::new(ObjectKind::Tag, self.serialize())
    }
}

#[derive(Error, Debug)]
pub enum TagError {
    #[error("malformed tag: {0}")]
    Malformed(String),
    #[error("duplicate header: {0}")]
    DuplicateHeader(&'static str),
    #[error("missing header: {0}")]
    MissingHeader(&'static str),
    #[error("missing blank line between headers and message")]
    MissingMessageSeparator,
    #[error(transparent)]
    Hash(#[from] HashError),
    #[error(transparent)]
    Identity(#[from] IdentityError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Time;

    fn fixture_sig() -> Signature {
        Signature {
            name: "t".into(),
            email: "t@t".into(),
            when: Time {
                seconds: 1700000000,
                offset_minutes: 0,
            },
        }
    }

    fn fixture_oid() -> ObjectId {
        ObjectId::parse_hex(HashKind::Sha1, "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef").unwrap()
    }

    #[test]
    fn round_trip_serialize_parse() {
        let tag = Tag::new(
            fixture_oid(),
            ObjectKind::Commit,
            b"v1.0".to_vec(),
            fixture_sig(),
            b"the message\n".to_vec(),
        );
        let bytes = tag.serialize();
        let parsed = Tag::parse(&bytes, HashKind::Sha1).unwrap();
        assert_eq!(parsed.object, tag.object);
        assert_eq!(parsed.kind, ObjectKind::Commit);
        assert_eq!(parsed.name, b"v1.0");
        assert_eq!(parsed.message, b"the message\n");
        assert!(parsed.tagger.is_some());
    }

    #[test]
    fn parse_missing_object_errors() {
        let body = b"type commit\ntag v1\n\nmsg\n";
        let err = Tag::parse(body, HashKind::Sha1).unwrap_err();
        assert!(matches!(err, TagError::MissingHeader("object")));
    }

    #[test]
    fn parse_unknown_type_errors() {
        let body = b"object deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\ntype banana\ntag v\n\nmsg\n";
        let err = Tag::parse(body, HashKind::Sha1).unwrap_err();
        assert!(matches!(err, TagError::Malformed(_)));
    }

    #[test]
    fn parses_each_object_type() {
        for kind in [
            ObjectKind::Blob,
            ObjectKind::Tree,
            ObjectKind::Commit,
            ObjectKind::Tag,
        ] {
            let body = format!(
                "object deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\ntype {}\ntag v\ntagger t <t@t> 1700000000 +0000\n\nm\n",
                kind.as_str()
            );
            let parsed = Tag::parse(body.as_bytes(), HashKind::Sha1).unwrap();
            assert_eq!(parsed.kind, kind);
        }
    }

    #[test]
    fn signed_tag_preserves_signature_in_message() {
        let signed_body = "object deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\n\
                           type commit\n\
                           tag v1\n\
                           tagger t <t@t> 1700000000 +0000\n\
                           \n\
                           My tag\n\
                           -----BEGIN PGP SIGNATURE-----\n\
                           sig-bytes\n\
                           -----END PGP SIGNATURE-----\n";
        let parsed = Tag::parse(signed_body.as_bytes(), HashKind::Sha1).unwrap();
        assert!(parsed.message.starts_with(b"My tag\n"));
        assert!(parsed
            .message
            .windows(b"BEGIN PGP SIGNATURE".len())
            .any(|w| w == b"BEGIN PGP SIGNATURE"));
        // Round-trip preserves the signature bytes.
        let re = String::from_utf8(parsed.serialize()).unwrap();
        assert_eq!(re, signed_body);
    }
}
