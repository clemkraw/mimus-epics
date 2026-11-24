//! The profile: everything `mimus serve` needs, and nothing it does not.
//!
//! A profile is the sole output of the study phase and the sole input
//! to the generation phase. The two phases communicate only through
//! this file.
//!
//! This makes profiles easy to inspect, compare across PVXS versions,
//! archive alongside benchmark results, and read without running MIMUS.
use serde::{Deserialize, Serialize};

use crate::session::{Manifest, SessionRules};
use crate::template::{Template, TemplateError};

/// Schema version of the profile format.
///
/// Bump this whenever a change alters the meaning of an existing field.
/// Profiles with a newer schema are rejected rather than parsed
/// optimistically, which could silently produce invalid output.
///
/// Version 2 added the complete initial update sent when a monitor
/// starts. Version 1 profiles contain only deltas, leaving metadata
/// fields undefined and producing traffic unlike a real IOC.
pub const SCHEMA_VERSION: u32 = 2;

/// Minimum number of updates required before trusting a template.
///
/// Two observations can reveal changing bytes, but cannot establish
/// that other bytes are constant. Fields that change only during alarm
/// transitions may appear fixed over short observation periods.
pub const DEFAULT_MIN_OBSERVATIONS: u32 = 30;

/// A learned model of one reference environment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub schema_version: u32,
    pub manifest: Manifest,
    pub session_rules: SessionRules,
    pub templates: Vec<Template>,
    /// Maps each observed PV to its template.
    /// Used to report coverage and show that message structure
    /// depends on record type rather than PV name.
    pub assignments: Vec<Assignment>,
}

/// Maps an observed PV to the template learned from it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assignment {
    pub pv: String,
    pub template: String,
}

/// Errors indicating that a profile is invalid or unsupported.
#[derive(Debug, Clone, PartialEq)]
pub enum ProfileError {
    /// Written with a newer schema version than this reader supports.
    SchemaTooNew {
        found: u32,
        supported: u32,
    },
    /// Written with an older schema version that cannot be migrated.
    SchemaTooOld {
        found: u32,
        supported: u32,
    },
    Template(TemplateError),
    /// Two templates have the same name.
    DuplicateTemplate {
        name: String,
    },
    /// A PV references a template that does not exist.
    UnknownTemplate {
        pv: String,
        template: String,
    },
    /// No PV is assigned to this template.
    UnusedTemplate {
        name: String,
    },
    /// The profile contains no templates.
    Empty,
    Json(String),
}

impl From<TemplateError> for ProfileError {
    fn from(e: TemplateError) -> Self {
        Self::Template(e)
    }
}

impl std::fmt::Display for ProfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SchemaTooNew { found, supported } => write!(
                f,
                "profile schema v{found} is newer than the supported v{supported}: upgrade MIMUS"
            ),
            Self::SchemaTooOld { found, supported } => write!(
                f,
                "profile schema v{found} predates the supported v{supported}: re-run `mimus calibrate`"
            ),
            Self::Template(e) => write!(f, "{e}"),
            Self::DuplicateTemplate { name } => write!(f, "template {name} is defined twice"),
            Self::UnknownTemplate { pv, template } => {
                write!(f, "{pv} is assigned to unknown template {template}")
            }
            Self::UnusedTemplate { name } => write!(f, "template {name} is used by no PV"),
            Self::Empty => f.write_str("profile contains no templates"),
            Self::Json(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ProfileError {}

impl Profile {
    pub fn new(manifest: Manifest, session_rules: SessionRules) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            manifest,
            session_rules,
            templates: Vec::new(),
            assignments: Vec::new(),
        }
    }

    pub fn template(&self, name: &str) -> Option<&Template> {
        self.templates.iter().find(|t| t.name == name)
    }

    /// Returns the number of PVs assigned to each template,
    /// ordered by usage count, then by template name.
    pub fn coverage(&self) -> Vec<(&str, usize)> {
        let mut out: Vec<(&str, usize)> = self
            .templates
            .iter()
            .map(|t| {
                let n = self
                    .assignments
                    .iter()
                    .filter(|a| a.template == t.name)
                    .count();
                (t.name.as_str(), n)
            })
            .collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        out
    }

    /// Returns the summary printed after calibration and used in reports.
    ///
    /// A synthetic profile observed nothing, so its channels are said to be
    /// assigned rather than observed.
    pub fn coverage_summary(&self) -> String {
        let how = if self.manifest.synthetic {
            "assigned"
        } else {
            "observed"
        };
        format!(
            "{} PVs {how} -> {} distinct templates",
            self.assignments.len(),
            self.templates.len()
        )
    }

    /// Validates the entire profile before use.
    ///
    /// Run this when loading the profile so invalid data is rejected
    /// before a benchmark starts, not during execution.
    pub fn validate(&self, min_observations: u32) -> Result<(), ProfileError> {
        if self.schema_version > SCHEMA_VERSION {
            return Err(ProfileError::SchemaTooNew {
                found: self.schema_version,
                supported: SCHEMA_VERSION,
            });
        }
        if self.schema_version < SCHEMA_VERSION {
            return Err(ProfileError::SchemaTooOld {
                found: self.schema_version,
                supported: SCHEMA_VERSION,
            });
        }
        if self.templates.is_empty() {
            return Err(ProfileError::Empty);
        }

        let mut names: Vec<&str> = Vec::with_capacity(self.templates.len());
        for t in &self.templates {
            if names.contains(&t.name.as_str()) {
                return Err(ProfileError::DuplicateTemplate {
                    name: t.name.clone(),
                });
            }
            names.push(&t.name);
            t.validate(min_observations)?;
        }

        for a in &self.assignments {
            if !names.contains(&a.template.as_str()) {
                return Err(ProfileError::UnknownTemplate {
                    pv: a.pv.clone(),
                    template: a.template.clone(),
                });
            }
        }

        for name in &names {
            if !self.assignments.iter().any(|a| a.template == *name) {
                return Err(ProfileError::UnusedTemplate {
                    name: (*name).to_string(),
                });
            }
        }

        Ok(())
    }

    /// Profiles are written once per calibration and are intended
    /// to be read and diffed, not processed on a hot path.
    pub fn to_json(&self) -> Result<String, ProfileError> {
        serde_json::to_string_pretty(self).map_err(|e| ProfileError::Json(e.to_string()))
    }

    /// Parses and validates a profile in one step.
    ///
    /// Only structural rules are checked here. The minimum number of
    /// observations is a calibration requirement, not a format constraint.
    /// Static templates are complete after one observation, so applying
    /// a fixed threshold at load time would reject valid profiles.
    ///
    /// [`DEFAULT_MIN_OBSERVATIONS`] remains the recommended threshold
    /// during calibration, where this decision belongs.
    pub fn from_json(s: &str) -> Result<Self, ProfileError> {
        let profile: Self =
            serde_json::from_str(s).map_err(|e| ProfileError::Json(e.to_string()))?;
        profile.validate(0)?;
        Ok(profile)
    }

    /// Returns templates supported by fewer than `floor` observations,
    /// along with their observation counts.
    ///
    /// Helps assess the reliability of benchmark results by identifying
    /// templates with limited evidence. Slotless templates are excluded:
    /// they describe static messages, fully captured by a single observation.
    pub fn weakly_supported(&self, floor: u32) -> Vec<(&str, u32)> {
        self.templates
            .iter()
            .filter(|t| !t.slots.is_empty() && t.observations < floor)
            .map(|t| (t.name.as_str(), t.observations))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slot::{ScalarType, Slot, SlotRole, SlotWidth};
    use crate::template::{Introspection, Template};

    const MONITOR: u8 = 0x0D;

    fn intro() -> Introspection {
        Introspection {
            full: vec![0xFD, 0x01, 0x00, 0x80],
            id_only: Some(vec![0xFE, 0x01, 0x00]),
        }
    }

    fn manifest() -> Manifest {
        Manifest {
            pvxs_version: "1.4.2".into(),
            epics_base_version: "7.0.8".into(),
            catalogue_sha256: "b".repeat(64),
            host: "bench01".into(),
            captured_at: "2026-08-15T12:00:00Z".into(),
            mimus_version: "0.1.0".into(),
            study_duration_s: 60.0,
            pvs_observed: 4,
            synthetic: false,
        }
    }

    fn template(name: &str) -> Template {
        let mut t = Template::new(name, MONITOR);
        t.with_introspection(intro())
            .push_fixed(vec![0x01, 0x02])
            .push_slot(
                Slot::new(1, SlotWidth::Fixed { bytes: 8 }).with_role(SlotRole::Value {
                    scalar: ScalarType::F64,
                }),
            )
            .push_slot(
                Slot::new(2, SlotWidth::Fixed { bytes: 8 }).with_role(SlotRole::TimestampSeconds),
            );
        t.observations = 100;
        t
    }

    fn profile() -> Profile {
        let mut p = Profile::new(manifest(), SessionRules::default());
        p.templates.push(template("ntscalar_double"));
        p.templates.push(template("ntscalar_int"));
        p.assignments = vec![
            Assignment {
                pv: "MIM:DBL:0".into(),
                template: "ntscalar_double".into(),
            },
            Assignment {
                pv: "MIM:DBL:1".into(),
                template: "ntscalar_double".into(),
            },
            Assignment {
                pv: "MIM:INT:0".into(),
                template: "ntscalar_int".into(),
            },
        ];
        p
    }

    #[test]
    fn a_well_formed_profile_validates() {
        profile().validate(DEFAULT_MIN_OBSERVATIONS).unwrap();
    }

    #[test]
    fn coverage_counts_pvs_per_template() {
        let p = profile();
        assert_eq!(
            p.coverage(),
            vec![("ntscalar_double", 2), ("ntscalar_int", 1)]
        );
        assert_eq!(
            p.coverage_summary(),
            "3 PVs observed -> 2 distinct templates"
        );
        let mut synthetic = p.clone();
        synthetic.manifest.synthetic = true;
        assert_eq!(
            synthetic.coverage_summary(),
            "3 PVs assigned -> 2 distinct templates"
        );
    }

    #[test]
    fn a_future_schema_is_refused_not_guessed() {
        let mut p = profile();
        p.schema_version = SCHEMA_VERSION + 1;
        assert_eq!(
            p.validate(0),
            Err(ProfileError::SchemaTooNew {
                found: SCHEMA_VERSION + 1,
                supported: SCHEMA_VERSION
            })
        );
    }

    #[test]
    fn an_older_schema_is_refused_too() {
        let mut p = profile();
        p.schema_version = 0;
        assert!(matches!(
            p.validate(0),
            Err(ProfileError::SchemaTooOld { found: 0, .. })
        ));
    }

    #[test]
    fn rejects_a_pv_pointing_at_a_missing_template() {
        let mut p = profile();
        p.assignments.push(Assignment {
            pv: "MIM:STR:0".into(),
            template: "ntscalar_string".into(),
        });
        assert_eq!(
            p.validate(0),
            Err(ProfileError::UnknownTemplate {
                pv: "MIM:STR:0".into(),
                template: "ntscalar_string".into()
            })
        );
    }

    #[test]
    fn rejects_a_template_no_pv_uses() {
        // An unused template means the study learned a structure
        // it could not associate with any PV.
        let mut p = profile();
        p.templates.push(template("ntenum"));
        assert_eq!(
            p.validate(0),
            Err(ProfileError::UnusedTemplate {
                name: "ntenum".into()
            })
        );
    }

    #[test]
    fn rejects_an_empty_profile() {
        let p = Profile::new(manifest(), SessionRules::default());
        assert_eq!(p.validate(0), Err(ProfileError::Empty));
    }

    #[test]
    fn a_slotted_template_still_needs_two_observations() {
        // One observation cannot reveal which bytes change.
        // This structural minimum applies regardless of the caller's threshold.
        let mut p = profile();
        p.templates[0].observations = 1;
        let json = serde_json::to_string(&p).unwrap();
        assert!(matches!(
            Profile::from_json(&json),
            Err(ProfileError::Template(TemplateError::TooFewObservations {
                observations: 1,
                required: 2,
                ..
            }))
        ));
    }

    #[test]
    fn a_static_template_loads_with_a_single_observation() {
        // A static value is fully described by one observation.
        // The writer must not produce a profile that the reader rejects.
        let mut p = profile();
        p.templates[0].chunks = vec![crate::Chunk::Fixed {
            bytes: vec![1, 2, 3],
        }];
        p.templates[0].slots.clear();
        p.templates[0].observations = 1;
        let json = serde_json::to_string(&p).unwrap();
        Profile::from_json(&json).expect("a static template must load");
    }

    #[test]
    fn round_trips_through_json_with_validation() {
        let p = profile();
        let json = p.to_json().unwrap();
        assert_eq!(Profile::from_json(&json).unwrap(), p);
    }

    #[test]
    fn malformed_json_is_an_error_not_a_panic() {
        assert!(matches!(
            Profile::from_json("{ this is not json"),
            Err(ProfileError::Json(_))
        ));
    }
}

#[cfg(test)]
mod support_tests {
    use super::*;
    use crate::slot::{ScalarType, Slot, SlotRole, SlotWidth};
    use crate::template::{Introspection, Template};

    const MONITOR: u8 = 0x0D;

    fn intro() -> Introspection {
        Introspection {
            full: vec![0xFF, 0x80, 0x01, b'x'],
            id_only: None,
        }
    }

    // A static setpoint: one complete observation, with no changing fields.
    fn static_template(name: &str) -> Template {
        let mut t = Template::new(name, MONITOR);
        t.with_introspection(intro()).push_fixed(vec![1, 2, 3, 4]);
        t.observations = 1;
        t
    }

    fn moving_template(name: &str, observations: u32) -> Template {
        let mut t = Template::new(name, MONITOR);
        t.with_introspection(intro())
            .push_fixed(vec![0x02])
            .push_slot(
                Slot::new(1, SlotWidth::Fixed { bytes: 8 }).with_role(SlotRole::Value {
                    scalar: ScalarType::F64,
                }),
            );
        t.observations = observations;
        t
    }

    fn with(templates: Vec<Template>) -> Profile {
        let mut p = Profile::new(
            Manifest {
                pvxs_version: "1.4.2".into(),
                epics_base_version: "7.0.8.1".into(),
                catalogue_sha256: "x".repeat(64),
                host: "test".into(),
                captured_at: "posix:0".into(),
                mimus_version: "0.1.0".into(),
                study_duration_s: 30.0,
                pvs_observed: 2,
                synthetic: false,
            },
            SessionRules::default(),
        );
        for t in templates {
            p.assignments.push(Assignment {
                pv: format!("PV:{}", t.name),
                template: t.name.clone(),
            });
            p.templates.push(t);
        }
        p
    }

    #[test]
    fn a_slotless_template_needs_only_one_observation() {
        // No fields change, so one observation fully describes the template.
        // Requiring thirty would make static setpoints impossible to represent.
        static_template("setpoint").validate(30).unwrap();
    }

    #[test]
    fn a_template_with_slots_still_faces_the_floor() {
        assert!(moving_template("value", 4).validate(30).is_err());
        moving_template("value", 30).validate(30).unwrap();
    }

    #[test]
    fn a_profile_survives_the_round_trip_whatever_floor_produced_it() {
        // A profile written with a floor of one must not be rejected at load
        // because the reader applies a floor of thirty. Loading must not impose
        // a calibration threshold the profile never claimed to meet.
        let p = with(vec![
            static_template("setpoint"),
            moving_template("value", 5),
        ]);
        let json = p.to_json().unwrap();
        let back = Profile::from_json(&json).expect("a written profile must load");
        assert_eq!(back.templates.len(), 2);
    }

    #[test]
    fn weak_support_is_reported_rather_than_refused() {
        // Observation counts indicate how well supported a template is,
        // not whether it is valid. Report weak evidence so users can decide
        // whether to trust the results.
        let p = with(vec![
            static_template("setpoint"),
            moving_template("value", 5),
        ]);
        assert_eq!(p.weakly_supported(30), vec![("value", 5)]);
        assert!(
            p.weakly_supported(5).is_empty(),
            "a setpoint is never weakly supported"
        );
    }

    #[test]
    fn structural_faults_are_still_refused_at_load() {
        // Removing the observation threshold must not weaken structural validation.
        let mut p = with(vec![moving_template("value", 30)]);
        p.templates[0].chunks.push(crate::Chunk::Slot { id: 99 });
        let json = serde_json::to_string(&p).unwrap();
        assert!(Profile::from_json(&json).is_err());
    }
}
