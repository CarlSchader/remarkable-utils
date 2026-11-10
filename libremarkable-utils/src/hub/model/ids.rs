//! Validated ids and names: document, page, repo and device ids, device
//! names, and safe xochitl-relative paths.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::layout::checked_components;
use super::{ModelError, Result};

/// Longest accepted page id. Real page ids are 36-character UUIDs.
const MAX_PAGE_ID_LEN: usize = 128;

/// Longest accepted device name, in characters.
const MAX_DEVICE_NAME_CHARS: usize = 64;

fn is_canonical_uuid(s: &str) -> bool {
    uuid::Uuid::try_parse(s).is_ok_and(|u| u.hyphenated().to_string() == s)
}

fn is_page_id(s: &str) -> bool {
    (1..=MAX_PAGE_ID_LEN).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn is_device_name(s: &str) -> bool {
    (1..=MAX_DEVICE_NAME_CHARS).contains(&s.chars().count())
        && !s.chars().any(|c| c.is_control() || c == '<' || c == '>')
}

fn is_safe_relative_path(s: &str) -> bool {
    checked_components(s).is_ok()
}

/// A string newtype validated on construction and on deserialisation.
macro_rules! validated_id {
    ($(#[$meta:meta])* $name:ident, $valid:path, $err:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// Validate and wrap an id.
            pub fn new(id: impl Into<String>) -> Result<Self> {
                let id = id.into();
                if $valid(&id) {
                    Ok(Self(id))
                } else {
                    Err(ModelError::$err(id))
                }
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = ModelError;

            fn try_from(id: String) -> Result<Self> {
                Self::new(id)
            }
        }

        impl From<$name> for String {
            fn from(id: $name) -> Self {
                id.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

validated_id!(
    /// A document or folder UUID, in canonical lowercase hyphenated form
    /// (the form xochitl writes). Anything else on the tablet is an
    /// unknown file, never mirrored.
    DocId,
    is_canonical_uuid,
    InvalidDocId
);

validated_id!(
    /// A page id inside a document: `[A-Za-z0-9_-]`, 1 to 128 bytes, so
    /// it can never carry a separator or a `.`/`..` component. Real page
    /// ids are UUIDs.
    PageId,
    is_page_id,
    InvalidPageId
);

validated_id!(
    /// The origin's identity, created by `init` (a random UUID).
    RepoId,
    is_canonical_uuid,
    InvalidId
);

validated_id!(
    /// A tablet's identity, created at clone (a random UUID; never the
    /// serial or hostname). Used in the device ref name.
    DeviceId,
    is_canonical_uuid,
    InvalidId
);

validated_id!(
    /// A tablet's user-chosen name: 1 to 64 characters, no control
    /// characters, no `<` or `>`. It ends up in git identities (which
    /// cannot contain angle brackets), fork names and commit trailers, so
    /// a newline must never get through.
    DeviceName,
    is_device_name,
    InvalidDeviceName
);

validated_id!(
    /// A safe path relative to the xochitl data dir (no absolute path,
    /// no empty, `.` or `..` components): manifest keys can never point
    /// the device layer outside the data dir.
    XochitlPath,
    is_safe_relative_path,
    UnsafePath
);

impl RepoId {
    /// A fresh random repo id.
    pub fn random() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}

impl DeviceId {
    /// A fresh random device id.
    pub fn random() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::Oid;
    use crate::hub::model::fixtures::*;

    #[test]
    fn doc_ids_must_be_canonical_lowercase_uuids() {
        assert!(DocId::new(DOC).is_ok());
        [
            "",
            "not-a-uuid",
            "8C1F0B9E-0000-4000-8000-000000000001",
            "8c1f0b9e000040008000000000000001",
            "{8c1f0b9e-0000-4000-8000-000000000001}",
            "urn:uuid:8c1f0b9e-0000-4000-8000-000000000001",
            "8c1f0b9e-0000-4000-8000-000000000001/..",
            "../8c1f0b9e-0000-4000-8000-000000000001",
        ]
        .iter()
        .for_each(|bad| {
            assert!(
                matches!(DocId::new(*bad), Err(ModelError::InvalidDocId(_))),
                "{bad}"
            )
        });
    }

    #[test]
    fn page_ids_reject_separators_and_dots() {
        assert!(PageId::new(PAGE).is_ok());
        assert!(PageId::new("page_1-x").is_ok());
        [
            "",
            ".",
            "..",
            "a/b",
            "a.b",
            "a\\b",
            "a b",
            "a\0b",
            &"x".repeat(129),
        ]
        .iter()
        .for_each(|bad| {
            assert!(
                matches!(PageId::new(*bad), Err(ModelError::InvalidPageId(_))),
                "{bad}"
            )
        });
    }

    #[test]
    fn object_ids_are_lowercase_sha1_or_sha256_hex() {
        assert!("a".repeat(40).parse::<Oid>().is_ok());
        assert!("0123456789abcdef".repeat(4).parse::<Oid>().is_ok());
        [
            "",
            &"a".repeat(39),
            &"a".repeat(41),
            &"A".repeat(40),
            &"g".repeat(40),
        ]
        .iter()
        .for_each(|bad| {
            assert!(
                matches!(
                    bad.parse::<Oid>(),
                    Err(crate::error::Error::InvalidObjectId(_))
                ),
                "{bad}"
            )
        });
    }

    #[test]
    fn repo_and_device_ids() {
        assert!(RepoId::new(REPO).is_ok());
        assert!(DeviceId::new(DEVICE).is_ok());
        assert!(matches!(
            DeviceId::new("my-tablet"),
            Err(ModelError::InvalidId(_))
        ));
        let random = DeviceId::random();
        assert!(DeviceId::new(random.as_str()).is_ok());
        assert_ne!(RepoId::random(), RepoId::random());
    }

    #[test]
    fn device_names() {
        [
            "Paper Pro c001",
            "Ünïcödé tablet",
            &"x".repeat(64),
            &"é".repeat(64),
        ]
        .iter()
        .for_each(|name| assert!(DeviceName::new(*name).is_ok(), "{name}"));
        [
            "",
            "a\nb",
            "a\rb",
            "tab\there",
            "nul\0",
            "esc\u{1b}[0m",
            &"x".repeat(65),
            // Git identities cannot contain angle brackets.
            "Evil <evil@example.invalid>",
            "a<b",
            "a>b",
        ]
        .iter()
        .for_each(|name| {
            assert!(
                matches!(
                    DeviceName::new(*name),
                    Err(ModelError::InvalidDeviceName(_))
                ),
                "{name:?}"
            )
        });
    }

    #[test]
    fn xochitl_paths_must_be_safe() {
        [
            format!("{DOC}.metadata"),
            format!("{DOC}/{PAGE}.rm"),
            ".tree".to_string(),
        ]
        .iter()
        .for_each(|path| assert!(XochitlPath::new(path.as_str()).is_ok(), "{path}"));
        ["", "/abs", "../x", "a/../b", "a//b", "a/", "./a"]
            .iter()
            .for_each(|path| {
                assert!(
                    matches!(XochitlPath::new(*path), Err(ModelError::UnsafePath(_))),
                    "{path}"
                )
            });
    }
}
