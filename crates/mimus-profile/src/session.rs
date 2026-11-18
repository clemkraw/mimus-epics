//! Session rules: connection-level behavior a profile must reproduce.
//!
//! A template describes the structure of a message, but not when PVXS
//! sends a full introspection descriptor versus a cached type ID, or
//! which fields it marks as changed in routine updates. These behaviors
//! belong to the connection and must be reproduced accurately, especially
//! during subscription setup, which dominates startup at large channel counts.

use serde::{Deserialize, Serialize};

/// How the server sends type descriptors on a new connection.
///
/// PVA caches introspection data per connection. The first channel
/// of each type carries the full descriptor; subsequent channels carry
/// only its identifier. For 100,000 channels of one type, that means
/// one full descriptor and 99,999 identifiers.
///
/// Replaying the full descriptor for every channel would overstate
/// connection setup costs and distort benchmark results.
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
    /// SHA-256 of the catalogue database, in lowercase hex.
    pub catalogue_sha256: String,
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
}

impl Manifest {
    /// One line for logs and reports.
    pub fn summary(&self) -> String {
        format!(
            "PVXS {} / base {} / catalogue {} / {} PVs / {}",
            self.pvxs_version,
            self.epics_base_version,
            &self.catalogue_sha256[..self.catalogue_sha256.len().min(12)],
            self.pvs_observed,
            self.captured_at
        )
    }

    /// Returns whether both profiles were captured from the same environment.
    ///
    /// Used by `mimus inspect --diff` to distinguish MIMUS regressions
    /// from changes in PVXS wire behavior.
    pub fn same_environment(&self, other: &Self) -> bool {
        self.pvxs_version == other.pvxs_version
            && self.epics_base_version == other.epics_base_version
            && self.catalogue_sha256 == other.catalogue_sha256
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Manifest {
        Manifest {
            pvxs_version: "1.4.2".into(),
            epics_base_version: "7.0.8".into(),
            catalogue_sha256: "a".repeat(64),
            host: "bench01".into(),
            captured_at: "2026-08-15T12:00:00Z".into(),
            mimus_version: "0.1.0".into(),
            study_duration_s: 42.0,
            pvs_observed: 20,
            synthetic: false,
        }
    }

    #[test]
    fn descriptor_policy_defaults_to_the_pvxs_behaviour() {
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
