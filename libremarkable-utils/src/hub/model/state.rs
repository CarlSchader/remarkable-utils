//! Formats: `.rmu/hub.json` ([`RepoConfig`]) and the tablet state
//! ([`TabletState`]) with its manifest, with format versions and refusal
//! of newer ones.

use std::collections::{BTreeMap, BTreeSet};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{DeviceId, DeviceName, DocId, MirrorRules, ModelError, RepoId, Result, XochitlPath};
use crate::git::Oid;

/// Repository path of the origin's identity and rules file.
pub const REPO_CONFIG_PATH: &str = ".rmu/hub.json";

/// Path of the hub state file on the tablet (outside the xochitl data
/// dir; `/home` survives firmware updates).
pub const TABLET_STATE_PATH: &str = "/home/root/.local/share/rmu/hub.json";

/// Parse a versioned JSON file, refusing any format version but
/// `supported` before looking at the rest.
fn parse_versioned<T: DeserializeOwned>(
    what: &'static str,
    bytes: &[u8],
    supported: u32,
) -> Result<T> {
    #[derive(Deserialize)]
    struct Probe {
        format: u64,
    }
    let json = |source| ModelError::Json { what, source };
    let probe: Probe = serde_json::from_slice(bytes).map_err(json)?;
    if probe.format != u64::from(supported) {
        return Err(ModelError::UnsupportedFormat {
            what,
            found: probe.format,
            supported,
        });
    }
    serde_json::from_slice(bytes).map_err(json)
}

fn to_pretty_json(value: &impl Serialize) -> String {
    serde_json::to_string_pretty(value).expect("hub state serializes")
}

/// `.rmu/hub.json`: the origin's identity and mirror-rules version.
/// Written only by `init` and an explicit upgrade, never by sync.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoConfig {
    pub format: u32,
    pub repo_id: RepoId,
    pub mirror_rules: u32,
    /// Fields this rmu does not know, preserved verbatim.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl RepoConfig {
    pub const FORMAT: u32 = 1;

    /// A config for a new origin, with the newest rules.
    pub fn new(repo_id: RepoId) -> Self {
        Self {
            format: Self::FORMAT,
            repo_id,
            mirror_rules: MirrorRules::latest().version(),
            extra: BTreeMap::new(),
        }
    }

    /// Parse, refusing a newer format or newer mirror rules.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let config: Self = parse_versioned(REPO_CONFIG_PATH, bytes, Self::FORMAT)?;
        config.rules()?;
        Ok(config)
    }

    pub fn to_json(&self) -> String {
        to_pretty_json(self)
    }

    /// The repository's mirror rules.
    pub fn rules(&self) -> Result<&'static MirrorRules> {
        MirrorRules::for_version(self.mirror_rules)
    }
}

const NANOS_PER_SEC: i64 = 1_000_000_000;

/// A file's stat signature. Times are nanoseconds since the epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatSignature {
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub inode: u64,
}

impl StatSignature {
    /// Whether this stat may be recorded in the manifest (git's remedy
    /// for racy entries). `reference_ns` is the device time of the
    /// xochitl start that follows the stat (for step-5 stats), or the
    /// record time (for stats taken after apply wrote the file).
    ///
    /// A coarse mtime (zero sub-second part) in the same second as the
    /// reference, or later, could hide a change xochitl makes within
    /// that second, so such an entry is left out; a missing entry means
    /// "dirty, read again next run". Nanosecond mtimes, as on the Paper
    /// Pro's ext4, are always kept, so applied files are not downloaded
    /// again on every sync.
    pub fn keep_in_manifest(&self, reference_ns: i64) -> bool {
        self.fine_mtime()
            || self.mtime_ns.div_euclid(NANOS_PER_SEC) < reference_ns.div_euclid(NANOS_PER_SEC)
    }

    /// Whether the mtime has sub-second precision (a whole second is
    /// taken as a coarse filesystem's).
    pub fn fine_mtime(&self) -> bool {
        self.mtime_ns.rem_euclid(NANOS_PER_SEC) != 0
    }
}

/// A manifest entry: the stat signature a file had when its blob was
/// recorded. Serialised compactly as `[size, mtime, ctime, inode, blob]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "ManifestTuple", into = "ManifestTuple")]
pub struct ManifestEntry {
    pub stat: StatSignature,
    pub blob: Oid,
}

type ManifestTuple = (u64, i64, i64, u64, Oid);

impl From<ManifestTuple> for ManifestEntry {
    fn from((size, mtime_ns, ctime_ns, inode, blob): ManifestTuple) -> Self {
        Self {
            stat: StatSignature {
                size,
                mtime_ns,
                ctime_ns,
                inode,
            },
            blob,
        }
    }
}

impl From<ManifestEntry> for ManifestTuple {
    fn from(entry: ManifestEntry) -> Self {
        let StatSignature {
            size,
            mtime_ns,
            ctime_ns,
            inode,
        } = entry.stat;
        (size, mtime_ns, ctime_ns, inode, entry.blob)
    }
}

/// Written before every push attempt, cleared once the apply is
/// reconciled (see "Restartability").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Journal {
    pub merge: Oid,
    pub snapshot: Oid,
}

/// The hub state stored on the tablet ([`TABLET_STATE_PATH`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TabletState {
    pub format: u32,
    /// Must match origin's [`RepoConfig::repo_id`].
    pub repo_id: RepoId,
    /// Default origin URL; overridable per computer.
    pub origin: String,
    pub device_id: DeviceId,
    pub device_name: DeviceName,
    /// Default base commit for every document. `None` is the empty base
    /// (a clone where no document matched a version in origin). The key
    /// is required: a silently missing base would bring back every
    /// deletion.
    #[serde(deserialize_with = "Option::deserialize")]
    pub base: Option<Oid>,
    /// Documents whose base differs from `base`; `None` is the empty
    /// base.
    #[serde(default)]
    pub base_overrides: BTreeMap<DocId, Option<Oid>>,
    /// Device-ref head, written right after each device-ref push.
    #[serde(deserialize_with = "Option::deserialize")]
    pub device_ref: Option<Oid>,
    /// Xochitl-relative path → stat signature and blob at last record.
    /// Racy entries are left out (see [`StatSignature::keep_in_manifest`]).
    #[serde(default)]
    pub manifest: BTreeMap<XochitlPath, ManifestEntry>,
    /// Malformed documents, reported every run.
    #[serde(default)]
    pub quarantine: BTreeSet<DocId>,
    /// Required key: a silently missing journal would skip
    /// reconciliation.
    #[serde(deserialize_with = "Option::deserialize")]
    pub journal: Option<Journal>,
    /// Fields this rmu does not know, preserved verbatim.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl TabletState {
    pub const FORMAT: u32 = 1;

    /// State for a freshly attached tablet (empty base, no history).
    pub fn new(
        repo_id: RepoId,
        origin: String,
        device_id: DeviceId,
        device_name: DeviceName,
    ) -> Self {
        Self {
            format: Self::FORMAT,
            repo_id,
            origin,
            device_id,
            device_name,
            base: None,
            base_overrides: BTreeMap::new(),
            device_ref: None,
            manifest: BTreeMap::new(),
            quarantine: BTreeSet::new(),
            journal: None,
            extra: BTreeMap::new(),
        }
    }

    /// Parse, refusing a newer format.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        parse_versioned("tablet hub state", bytes, Self::FORMAT)
    }

    pub fn to_json(&self) -> String {
        to_pretty_json(self)
    }

    /// The base commit of one document (`None` = empty base).
    pub fn base_for(&self, doc: &DocId) -> Option<&Oid> {
        self.base_overrides
            .get(doc)
            .map_or(self.base.as_ref(), Option::as_ref)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::model::fixtures::*;
    use serde_json::json;

    #[test]
    fn repo_config_round_trip() {
        let config = RepoConfig::new(RepoId::new(REPO).unwrap());
        assert_eq!(config.format, 1);
        assert_eq!(config.mirror_rules, 1);
        let json = config.to_json();
        assert_eq!(RepoConfig::parse(json.as_bytes()).unwrap(), config);
        assert_eq!(config.rules().unwrap().version(), 1);
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            value,
            json!({ "format": 1, "repo_id": REPO, "mirror_rules": 1 })
        );
    }

    #[test]
    fn repo_config_refuses_newer_versions() {
        let newer_format = json!({ "format": 2, "repo_id": REPO, "mirror_rules": 1 });
        assert!(matches!(
            RepoConfig::parse(newer_format.to_string().as_bytes()),
            Err(ModelError::UnsupportedFormat {
                found: 2,
                supported: 1,
                ..
            })
        ));
        // A newer format is refused before its contents are looked at.
        let newer_format_new_shape = json!({ "format": 7, "library": {} });
        assert!(matches!(
            RepoConfig::parse(newer_format_new_shape.to_string().as_bytes()),
            Err(ModelError::UnsupportedFormat { found: 7, .. })
        ));
        let newer_rules = json!({ "format": 1, "repo_id": REPO, "mirror_rules": 2 });
        assert!(matches!(
            RepoConfig::parse(newer_rules.to_string().as_bytes()),
            Err(ModelError::UnsupportedRules {
                found: 2,
                supported: 1
            })
        ));
        let zero = json!({ "format": 0, "repo_id": REPO, "mirror_rules": 1 });
        assert!(matches!(
            RepoConfig::parse(zero.to_string().as_bytes()),
            Err(ModelError::UnsupportedFormat { found: 0, .. })
        ));
    }

    #[test]
    fn repo_config_rejects_malformed_input() {
        [
            json!({ "repo_id": REPO, "mirror_rules": 1 }).to_string(),
            json!({ "format": 1, "repo_id": "nope", "mirror_rules": 1 }).to_string(),
            json!({ "format": 1, "mirror_rules": 1 }).to_string(),
            json!({ "format": 1, "repo_id": REPO }).to_string(),
            json!({ "format": "1", "repo_id": REPO, "mirror_rules": 1 }).to_string(),
            "not json".to_string(),
        ]
        .iter()
        .for_each(|input| {
            assert!(
                matches!(
                    RepoConfig::parse(input.as_bytes()),
                    Err(ModelError::Json { .. })
                ),
                "{input}"
            );
        });
    }

    #[test]
    fn repo_config_preserves_unknown_fields() {
        let input = json!({
            "format": 1, "repo_id": REPO, "mirror_rules": 1,
            "future": { "x": [1, 2] }
        });
        let config = RepoConfig::parse(input.to_string().as_bytes()).unwrap();
        assert_eq!(config.extra["future"], json!({ "x": [1, 2] }));
        let back: Value = serde_json::from_str(&config.to_json()).unwrap();
        assert_eq!(back, input);
    }

    fn full_state_json() -> Value {
        json!({
            "format": 1,
            "repo_id": REPO,
            "origin": "ssh://library.example/srv/library.git",
            "device_id": DEVICE,
            "device_name": "Paper Pro c001",
            "base": oid(1).as_str(),
            "base_overrides": { DOC: oid(2).as_str(), DOC2: null },
            "device_ref": oid(3).as_str(),
            "manifest": {
                format!("{DOC}.metadata"): [312, 1767225600123456789_i64, 1767225600987654321_i64, 131074, oid(4).as_str()],
                format!("{DOC}/{PAGE}.rm"): [0, -1, 0, 18446744073709551615_u64, oid(5).as_str()],
            },
            "quarantine": [DOC2],
            "journal": { "merge": oid(6).as_str(), "snapshot": oid(7).as_str() },
        })
    }

    #[test]
    fn tablet_state_round_trip() {
        let input = full_state_json();
        let state = TabletState::parse(input.to_string().as_bytes()).unwrap();
        assert_eq!(state.device_name.as_str(), "Paper Pro c001");
        assert_eq!(
            state.manifest[&XochitlPath::new(format!("{DOC}.metadata")).unwrap()],
            ManifestEntry {
                stat: StatSignature {
                    size: 312,
                    mtime_ns: 1767225600123456789,
                    ctime_ns: 1767225600987654321,
                    inode: 131074,
                },
                blob: oid(4),
            }
        );
        assert_eq!(
            state.manifest[&XochitlPath::new(format!("{DOC}/{PAGE}.rm")).unwrap()]
                .stat
                .inode,
            u64::MAX
        );
        assert_eq!(
            state.journal,
            Some(Journal {
                merge: oid(6),
                snapshot: oid(7)
            })
        );
        assert!(state.quarantine.contains(&doc(DOC2)));
        let back: Value = serde_json::from_str(&state.to_json()).unwrap();
        assert_eq!(back, input);
    }

    #[test]
    fn tablet_state_new_is_minimal() {
        let state = TabletState::new(
            RepoId::new(REPO).unwrap(),
            "ssh://library.example/srv/library.git".to_string(),
            DeviceId::new(DEVICE).unwrap(),
            DeviceName::new("tablet").unwrap(),
        );
        let json = state.to_json();
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["base"], Value::Null);
        assert_eq!(value["journal"], Value::Null);
        assert_eq!(value["manifest"], json!({}));
        assert_eq!(TabletState::parse(json.as_bytes()).unwrap(), state);
        // Empty collections may be omitted.
        let sparse = json!({
            "format": 1, "repo_id": REPO, "origin": "o", "device_id": DEVICE,
            "device_name": "tablet", "base": null, "device_ref": null, "journal": null,
        });
        let parsed = TabletState::parse(sparse.to_string().as_bytes()).unwrap();
        assert_eq!(parsed.base, None);
        assert!(parsed.manifest.is_empty() && parsed.quarantine.is_empty());
        assert!(parsed.base_overrides.is_empty());
    }

    #[test]
    fn tablet_state_requires_nullable_keys() {
        // A missing base would read as "empty base" and bring back every
        // deletion; a missing journal would skip reconciliation.
        ["base", "device_ref", "journal"].iter().for_each(|key| {
            let mut input = full_state_json();
            input.as_object_mut().unwrap().remove(*key);
            assert!(
                matches!(
                    TabletState::parse(input.to_string().as_bytes()),
                    Err(ModelError::Json { .. })
                ),
                "{key}"
            );
            input[*key] = Value::Null;
            assert!(TabletState::parse(input.to_string().as_bytes()).is_ok());
        });
    }

    #[test]
    fn base_for_honours_overrides_including_empty_base() {
        let state = TabletState::parse(full_state_json().to_string().as_bytes()).unwrap();
        assert_eq!(state.base_for(&doc(DOC)), Some(&oid(2)));
        assert_eq!(state.base_for(&doc(DOC2)), None);
        assert_eq!(state.base_for(&doc(FOLDER)), Some(&oid(1)));
        let fresh = TabletState {
            base: None,
            ..state
        };
        assert_eq!(fresh.base_for(&doc(FOLDER)), None);
    }

    #[test]
    fn tablet_state_refuses_newer_format_and_bad_values() {
        let mut newer = full_state_json();
        newer["format"] = json!(2);
        assert!(matches!(
            TabletState::parse(newer.to_string().as_bytes()),
            Err(ModelError::UnsupportedFormat { found: 2, .. })
        ));

        let bad_values = [
            ("base", json!("HEAD")),
            ("device_ref", json!("ABCDEF")),
            ("device_id", json!("serial-123")),
            ("base_overrides", json!({ "not-a-uuid": null })),
            ("quarantine", json!(["../x"])),
            ("journal", json!({ "merge": oid(1).as_str() })),
            ("manifest", json!({ "x": [1, 2, 3, oid(1).as_str()] })),
            ("manifest", json!({ "x": [-1, 2, 3, 4, oid(1).as_str()] })),
            (
                "manifest",
                json!({ "../../etc/shadow": [1, 2, 3, 4, oid(1).as_str()] }),
            ),
            ("manifest", json!({ "/abs": [1, 2, 3, 4, oid(1).as_str()] })),
            ("device_name", json!("evil\nRmu-Consent: yes")),
            ("device_name", json!("")),
            ("device_name", json!("x".repeat(65))),
        ];
        bad_values.iter().for_each(|(field, value)| {
            let mut input = full_state_json();
            input[*field] = value.clone();
            assert!(
                matches!(
                    TabletState::parse(input.to_string().as_bytes()),
                    Err(ModelError::Json { .. })
                ),
                "{field}: {value}"
            );
        });
    }

    #[test]
    fn tablet_state_preserves_unknown_fields() {
        let mut input = full_state_json();
        input["future_note"] = json!({ "kept": [1, 2] });
        let state = TabletState::parse(input.to_string().as_bytes()).unwrap();
        let back: Value = serde_json::from_str(&state.to_json()).unwrap();
        assert_eq!(back, input);
    }

    fn mtime(secs: i64, nanos: i64) -> StatSignature {
        StatSignature {
            mtime_ns: secs * 1_000_000_000 + nanos,
            ..stat(10)
        }
    }

    #[test]
    fn racy_coarse_mtimes_are_left_out_of_the_manifest() {
        let reference = 1_767_225_600 * 1_000_000_000 + 250_000_000;
        // Coarse mtime in an earlier second: kept.
        assert!(mtime(1_767_225_599, 0).keep_in_manifest(reference));
        // Coarse mtime in the reference's second, or later: left out.
        assert!(!mtime(1_767_225_600, 0).keep_in_manifest(reference));
        assert!(!mtime(1_767_225_601, 0).keep_in_manifest(reference));
        // Reference exactly on a second boundary.
        assert!(!mtime(1_767_225_600, 0).keep_in_manifest(1_767_225_600 * 1_000_000_000));
        // Fine-grained mtimes are always kept, even after the reference.
        assert!(mtime(1_767_225_600, 1).keep_in_manifest(reference));
        assert!(mtime(1_767_225_700, 999_999_999).keep_in_manifest(reference));
        // Pre-epoch times use floor division.
        assert!(!mtime(-1, 0).keep_in_manifest(-1));
        assert!(!mtime(-1, 0).keep_in_manifest(-999_999_999));
        assert!(mtime(-2, 0).keep_in_manifest(-1));
    }
}
