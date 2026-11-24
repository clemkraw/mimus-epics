//! Session rules: connection-level behavior a profile must reproduce.
//!
//! A template describes the structure of a message, but not when PVXS
//! sends a full introspection descriptor versus a cached type ID, or
//! which fields it marks as changed in routine updates. These behaviors
//! belong to the connection and must be reproduced accurately, especially
//! during subscription setup, which dominates startup at large channel counts.

use serde::{Deserialize, Serialize};

/// How the server sends type descriptors when a monitor or a `GET` opens.
///
/// PVAccess lets a server cache introspection data per connection: the
/// first channel of a type carries the full descriptor, later ones only its
/// identifier, so 100,000 channels of one type cost one full descriptor and
/// 99,999 identifiers. PVXS does not do this and sends the full descriptor
/// every time. Calibration records which of the two the reference server
/// did, and MIMUS does the same: either choice made for the other server
/// would misstate the cost of setting up subscriptions.
///
/// The default, used only by profiles built in code, is the caching form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DescriptorPolicy {
    /// Send the full descriptor once per type, then its identifier.
    #[default]
    FullThenId,
    /// Send the full descriptor for every channel.
    /// Use only when this behavior was observed during calibration.
    AlwaysFull,
}

/// A changed-set pattern observed during learning and its frequency.
///
/// PVXS marks different fields depending on the update. A plain value
/// change and an alarm transition, for example, produce different bitsets.
/// Replaying only the simplest pattern would leave other client decode
/// paths untested.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedSetPattern {
    /// Raw bitset as encoded on the wire.
    #[serde(with = "crate::hex::bytes")]
    pub bitset: Vec<u8>,
    /// Number of times this pattern was observed.
    pub count: u32,
}

/// Connection-level behavior learned alongside the templates.
///
/// `PartialEq` is sufficient because `marker_interval_s` is a float.
/// It supports tests and `mimus inspect --diff` without requiring `Eq`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRules {
    pub descriptor_policy: DescriptorPolicy,
    /// Receive buffer size proposed by the reference server during validation.
    pub server_buffer_size: i32,
    /// Introspection registry size proposed by the reference server.
    pub server_registry_size: i16,
    /// Interval between flow-control markers, if observed.
    /// `None` means no markers were observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker_interval_s: Option<f64>,
    /// Observed changed-set patterns, ordered by frequency.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_sets: Vec<ChangedSetPattern>,
}

impl Default for SessionRules {
    fn default() -> Self {
        Self {
            descriptor_policy: DescriptorPolicy::default(),
            // PVXS defaults; overwritten by whatever is observed.
            server_buffer_size: 16384,
            server_registry_size: 0x7FFF,
            marker_interval_s: None,
            changed_sets: Vec::new(),
        }
    }
}

impl SessionRules {
    /// Returns the most frequent changed-set pattern.
    /// Used as the default for steady-state emission.
    pub fn dominant_changed_set(&self) -> Option<&ChangedSetPattern> {
        self.changed_sets.iter().max_by_key(|p| p.count)
    }

    /// Returns the share of updates using the dominant pattern, in `0.0..=1.0`.
    ///
    /// A value near 1.0 indicates uniform steady-state behavior.
    /// A lower value means other patterns also occur and should be
    /// represented to exercise the corresponding client decode paths.
    pub fn dominant_share(&self) -> Option<f64> {
        let total: u32 = self.changed_sets.iter().map(|p| p.count).sum();
        if total == 0 {
            return None;
        }
        self.dominant_changed_set()
            .map(|p| p.count as f64 / total as f64)
    }
}

/// Provenance of a profile: how, when and where it was produced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// PVXS version reported by the reference IOC.
    pub pvxs_version: String,
    /// EPICS Base version of the reference IOC.
    pub epics_base_version: String,
    /// Checksum of the catalogue databases passed to `mimus calibrate`
    /// with `--catalogue`, one `fnv1a64:<hash>:<size>` per database, joined
    /// by `+` in the order given. Enough to notice a changed catalogue, not
    /// a cryptographic digest. `unspecified` when no database was given,
    /// `unavailable` when one could not be read.
    ///
    /// Profiles written before this field had its name call it
    /// `catalogue_sha256`; they are still read.
    #[serde(alias = "catalogue_sha256")]
    pub catalogue_checksum: String,
    /// Host on which calibration ran.
    pub host: String,
    /// Calibration timestamp in RFC 3339 format.
    pub captured_at: String,
    /// MIMUS version used for calibration.
    pub mimus_version: String,
    /// Duration of the study phase, in seconds.
    pub study_duration_s: f64,
    /// Number of PVs observed during the study.
    pub pvs_observed: u32,
    /// Whether the profile was written by hand rather than learned
    /// from a reference IOC.
    ///
    /// Synthetic profiles can still be served, but they are not
    /// calibration results and must not be presented as measurements.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub synthetic: bool,
    /// Whether `mimus calibrate --allow-incomplete` wrote the profile
    /// without its completeness criterion met.
    ///
    /// Such a profile serves, but some channels were not observed long
    /// enough to trust their templates, so it is not a measurement basis.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub incomplete: bool,
}

impl Manifest {
    /// One line for logs and reports.
    pub fn summary(&self) -> String {
        format!(
            "PVXS {} / base {} / catalogue {} / {} PVs / {}",
            self.pvxs_version,
            self.epics_base_version,
            &self.catalogue_checksum[..self.catalogue_checksum.len().min(12)],
            self.pvs_observed,
            self.captured_at
        )
    }

    /// Whether the manifest says which environment produced the profile:
    /// both versions given, the catalogue recorded, and not hand-written.
    ///
    /// `mimus calibrate` writes `unknown` for a version it was not given and
    /// `unspecified` or `unavailable` for a catalogue it could not record.
    pub fn environment_known(&self) -> bool {
        let given = |v: &str| !matches!(v, "" | "unknown" | "unspecified" | "unavailable");
        !self.synthetic
            && given(&self.pvxs_version)
            && given(&self.epics_base_version)
            && given(&self.catalogue_checksum)
    }

    /// Returns whether both profiles were captured from the same, known
    /// environment.
    ///
    /// Used by `mimus inspect --diff` to distinguish MIMUS regressions
    /// from changes in PVXS wire behavior. Two profiles whose environment is
    /// not recorded do not share one: `unknown` matching `unknown` says
    /// nothing about the IOC behind either.
    pub fn same_environment(&self, other: &Self) -> bool {
        self.environment_known()
            && other.environment_known()
            && self.pvxs_version == other.pvxs_version
            && self.epics_base_version == other.epics_base_version
            && self.catalogue_checksum == other.catalogue_checksum
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Manifest {
        Manifest {
            pvxs_version: "1.4.2".into(),
            epics_base_version: "7.0.8".into(),
            catalogue_checksum: "a".repeat(64),
            host: "bench01".into(),
            captured_at: "2026-08-15T12:00:00Z".into(),
            mimus_version: "0.1.0".into(),
            study_duration_s: 42.0,
            pvs_observed: 20,
            synthetic: false,
            incomplete: false,
        }
    }

    #[test]
    fn a_manifest_written_under_the_old_checksum_name_still_reads() {
        let m = manifest();
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"catalogue_checksum\""));
        let old = json.replace("\"catalogue_checksum\"", "\"catalogue_sha256\"");
        assert_eq!(serde_json::from_str::<Manifest>(&old).unwrap(), m);
    }

    #[test]
    fn descriptor_policy_defaults_to_the_caching_form() {
        assert_eq!(
            SessionRules::default().descriptor_policy,
            DescriptorPolicy::FullThenId
        );
    }

    #[test]
    fn dominant_changed_set_is_the_most_frequent() {
        let rules = SessionRules {
            changed_sets: vec![
                ChangedSetPattern {
                    bitset: vec![0x01],
                    count: 10,
                },
                ChangedSetPattern {
                    bitset: vec![0x83],
                    count: 990,
                },
            ],
            ..Default::default()
        };
        assert_eq!(rules.dominant_changed_set().unwrap().bitset, vec![0x83]);
        assert!((rules.dominant_share().unwrap() - 0.99).abs() < 1e-9);
    }

    #[test]
    fn dominant_share_is_absent_when_nothing_was_observed() {
        assert!(SessionRules::default().dominant_share().is_none());
    }

    #[test]
    fn environments_compare_on_provenance_not_on_run_details() {
        let a = manifest();
        let mut b = manifest();
        b.host = "bench02".into();
        b.captured_at = "2026-09-01T00:00:00Z".into();
        b.study_duration_s = 99.0;
        assert!(a.same_environment(&b));

        b.pvxs_version = "1.5.0".into();
        assert!(!a.same_environment(&b));
    }

    #[test]
    fn an_unrecorded_environment_is_never_the_same_as_another() {
        for unrecorded in [
            |m: &mut Manifest| m.pvxs_version = "unknown".into(),
            |m: &mut Manifest| m.epics_base_version = "unknown".into(),
            |m: &mut Manifest| m.catalogue_checksum = "unspecified".into(),
            |m: &mut Manifest| m.catalogue_checksum = "unavailable".into(),
            |m: &mut Manifest| m.synthetic = true,
        ] {
            let mut a = manifest();
            unrecorded(&mut a);
            let b = a.clone();
            assert!(!a.environment_known());
            assert!(!a.same_environment(&b), "{a:?}");
        }
        assert!(manifest().environment_known());
    }

    #[test]
    fn round_trips_through_json() {
        let rules = SessionRules {
            marker_interval_s: Some(1.5),
            changed_sets: vec![ChangedSetPattern {
                bitset: vec![0x83, 0x01],
                count: 7,
            }],
            ..Default::default()
        };
        let json = serde_json::to_string(&rules).unwrap();
        assert_eq!(serde_json::from_str::<SessionRules>(&json).unwrap(), rules);
        assert!(json.contains("8301"));

        let m = manifest();
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(serde_json::from_str::<Manifest>(&json).unwrap(), m);
    }
}
