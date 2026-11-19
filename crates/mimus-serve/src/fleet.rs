//! The fleet: the PVs in the system, their templates, and the virtual
//! IOCs that serve them.
//!
//! Constructed once at startup from a profile and a small set of
//! configuration values. At a million channels, this is the largest
//! data structure in the process, so it uses parallel arrays indexed
//! by channel instance rather than a map of per-channel objects.

use std::collections::HashMap;

use mimus_emit::{CompileError, Compiled, EpicsTime, Instance, RateMix};
use mimus_profile::Profile;
use mimus_wire::header::ByteOrder;
use mimus_wire::pvdata::Descriptor;

use crate::naming::{NameContext, NameTemplate};

/// Number of opening-message renderings checked by [`Fleet::unstamped_openings`].
pub const OPENING_CHECK_STEPS: u64 = 1024;

/// How PVs are distributed across virtual IOCs.
///
/// Distribution affects how efficiently clients can discover PVs. A client
/// that partitions its PV list using round-robin assignment will find every
/// PV on the first pass against a round-robin fleet. With blocked assignment,
/// it may find very few and need to broadcast the remaining searches to
/// every IOC.
///
/// Real facilities often group PVs by subsystem, making blocked distribution
/// a more representative default. Offering only round-robin would give
/// clients an artificially favorable discovery workload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layout {
    /// Assigns PV `i` to IOC `i % iocs`.
    ///
    /// A favorable case for clients that partition searches round-robin.
    RoundRobin,
    /// Assigns contiguous ranges of PVs to each IOC.
    ///
    /// Models facilities where PVs are grouped by subsystem.
    #[default]
    Blocked,
}

impl std::str::FromStr for Layout {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "round-robin" | "roundrobin" => Ok(Self::RoundRobin),
            "blocked" => Ok(Self::Blocked),
            other => Err(format!("unknown layout {other:?}")),
        }
    }
}

impl std::fmt::Display for Layout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::RoundRobin => "round-robin",
            Self::Blocked => "blocked",
        })
    }
}

/// Configuration for generating a fleet of PVs.
#[derive(Debug, Clone)]
pub struct FleetSpec {
    pub iocs: u16,
    pub pvs: u32,
    pub layout: Layout,
    /// Template names paired with their relative population weights.
    ///
    /// Weights are normalized when building the fleet. For example, `[(a, 1), (b, 3)]` assigns
    /// one quarter of the PVs to `a` and three quarters to `b`.
    ///
    /// A fleet using only one template may leave client fast paths
    /// untested. The template mix should therefore be recorded in the run manifest.
    pub mix: Vec<(String, u32)>,
    /// Distribution of channels across update rates.
    pub rate_mix: RateMix,
    /// Pattern used to generate channel names.
    pub names: NameTemplate,
}

impl FleetSpec {
    /// Creates a fleet with equal shares for every template in the profile.
    pub fn uniform(profile: &Profile, iocs: u16, pvs: u32) -> Self {
        Self {
            iocs,
            pvs,
            layout: Layout::default(),
            mix: profile
                .templates
                .iter()
                .map(|t| (t.name.clone(), 1))
                .collect(),
            rate_mix: RateMix::uniform(1.0),
            names: NameTemplate::default_pattern("MIM"),
        }
    }
}

/// Errors that prevent a fleet from being built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FleetError {
    NoIocs,
    NoPvs,
    /// The mix references a template not present in the profile.
    ///
    /// The build fails rather than silently skipping the entry, which
    /// would change the requested population and invalidate the run.
    UnknownTemplate(String),
    EmptyMix,
    Compile(CompileError),
}

impl From<CompileError> for FleetError {
    fn from(e: CompileError) -> Self {
        Self::Compile(e)
    }
}

impl std::fmt::Display for FleetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoIocs => f.write_str("a fleet needs at least one IOC"),
            Self::NoPvs => f.write_str("a fleet needs at least one PV"),
            Self::UnknownTemplate(n) => {
                write!(
                    f,
                    "the mix names template {n:?}, which the profile does not contain"
                )
            }
            Self::EmptyMix => f.write_str("the mix has no templates with a nonzero share"),
            Self::Compile(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for FleetError {}

/// The complete synthetic population.
pub struct Fleet {
    /// Compiled templates, indexed by `Instance::template`.
    pub templates: Vec<Compiled>,
    /// Opening-message descriptor for each template, parallel to `templates`.
    ///
    /// `None` means the template has no opening message or its descriptor
    /// could not be read. In that case, the message is sent unchanged.
    pub openings: Vec<Option<Descriptor>>,
    /// One entry per PV, parallel to `names`.
    pub instances: Vec<Instance>,
    pub names: Vec<String>,
    /// Maps each PV name to its instance index for search requests.
    index: HashMap<String, u32>,
    iocs: u16,
}

impl Fleet {
    /// Builds the synthetic population from a profile and fleet specification.
    pub fn build(profile: &Profile, spec: &FleetSpec) -> Result<Self, FleetError> {
        if spec.iocs == 0 {
            return Err(FleetError::NoIocs);
        }
        if spec.pvs == 0 {
            return Err(FleetError::NoPvs);
        }

        let mut templates = Vec::with_capacity(spec.mix.len());
        let mut weights = Vec::with_capacity(spec.mix.len());
        for (name, share) in &spec.mix {
            if *share == 0 {
                continue;
            }
            let t = profile
                .template(name)
                .ok_or_else(|| FleetError::UnknownTemplate(name.clone()))?;
            templates.push(Compiled::new(t)?);
            weights.push(*share);
        }
        if templates.is_empty() {
            return Err(FleetError::EmptyMix);
        }

        // Build a repeating pattern that preserves the requested proportions
        // across population prefixes. Taking the first N entries from a list
        // grouped by template could heavily skew the mix when N is not evenly
        // divisible by the total weight.
        let total: u32 = weights.iter().sum();
        let mut pattern = Vec::with_capacity(total as usize);
        for (idx, w) in weights.iter().enumerate() {
            for _ in 0..*w {
                pattern.push(idx as u16);
            }
        }

        let mut instances = Vec::with_capacity(spec.pvs as usize);
        let mut names = Vec::with_capacity(spec.pvs as usize);
        let mut index = HashMap::with_capacity(spec.pvs as usize);

        let per_ioc = spec.pvs.div_ceil(spec.iocs as u32);
        let mut per_ioc_seen = vec![0u32; spec.iocs as usize];
        for i in 0..spec.pvs {
            let template = pattern[(i as usize) % pattern.len()];
            let ioc = match spec.layout {
                Layout::RoundRobin => (i % spec.iocs as u32) as u16,
                Layout::Blocked => ((i / per_ioc) as u16).min(spec.iocs - 1),
            };
            let index_in_ioc = per_ioc_seen[ioc as usize];
            per_ioc_seen[ioc as usize] += 1;

            let name = spec.names.expand(&NameContext {
                index: i,
                ioc,
                index_in_ioc,
                template: &templates[template as usize].name,
            });
            index.insert(name.clone(), i);
            names.push(name);
            // Use the PV index as the seed so rebuilding the fleet reproduces
            // the same stream of opening-message samples.
            instances.push(
                Instance::new(template, ioc, i as u64 + 1)
                    .with_rate_group(spec.rate_mix.group_of(i)),
            );
        }

        // Parse opening descriptors in the order templates are declared.
        // The session uses little-endian byte order.
        let openings = templates
            .iter()
            .map(|t| {
                t.initial.as_ref()?;
                Descriptor::from_init_reply(&t.intro_full, ByteOrder::Little).ok()
            })
            .collect();

        Ok(Self {
            templates,
            openings,
            instances,
            names,
            index,
            iocs: spec.iocs,
        })
    }

    /// Returns templates whose opening messages retain their recorded
    /// timestamps, together with the reason.
    ///
    /// Each instance's opening message is rendered using its seed, which
    /// selects a recorded sample. Each slot contains one sample per channel
    /// observed during calibration. Checking the first
    /// [`OPENING_CHECK_STEPS`] renderings therefore covers every sample that
    /// calibration can produce for up to that many channels per type.
    pub fn unstamped_openings(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let now = EpicsTime::now();
        for (t, desc) in self.templates.iter().zip(&self.openings) {
            let Some(initial) = t.initial.as_deref() else {
                continue;
            };
            let Some(desc) = desc else {
                out.push((t.name.clone(), "its type descriptor cannot be read".into()));
                continue;
            };
            if !desc.has_stamp() {
                out.push((t.name.clone(), "its type has no top-level timeStamp".into()));
                continue;
            }
            let mut body = Vec::with_capacity(initial.max_len());
            for step in 0..OPENING_CHECK_STEPS {
                body.clear();
                initial.render(step, step, now, &mut body);
                let why = match desc.locate(&body) {
                    Ok(at) if at.stamp.is_some() => continue,
                    Ok(_) => "its opening message does not include the timestamp".to_string(),
                    Err(e) => format!("its opening message cannot be walked: {e}"),
                };
                out.push((t.name.clone(), why));
                break;
            }
        }
        out
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.instances.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.instances.is_empty()
    }

    #[inline]
    pub fn iocs(&self) -> u16 {
        self.iocs
    }

    /// Returns the instance index for a PV name, if present in this fleet.
    #[inline]
    pub fn lookup(&self, name: &str) -> Option<u32> {
        self.index.get(name).copied()
    }

    /// Returns the instance index for a PV name only if it belongs to `ioc`.
    ///
    /// A virtual IOC must not claim channels it does not serve. Returning a
    /// match for every name would make every search succeed in the first round
    /// and conceal the multi-round search behavior.
    pub fn lookup_on(&self, name: &str, ioc: u16) -> Option<u32> {
        let idx = self.lookup(name)?;
        (self.instances[idx as usize].ioc == ioc).then_some(idx)
    }

    /// Returns the number of PVs served by each IOC, indexed by IOC number.
    pub fn per_ioc_counts(&self) -> Vec<usize> {
        let mut counts = vec![0usize; self.iocs as usize];
        for inst in &self.instances {
            counts[inst.ioc as usize] += 1;
        }
        counts
    }

    /// Returns the number of PVs assigned to each rate group.
    pub fn rate_group_counts(&self, groups: usize) -> Vec<usize> {
        let mut counts = vec![0usize; groups];
        for inst in &self.instances {
            if let Some(c) = counts.get_mut(inst.rate_group as usize) {
                *c += 1;
            }
        }
        counts
    }

    /// Returns the number of PVs using each template, in template order.
    pub fn template_counts(&self) -> Vec<(&str, usize)> {
        let mut counts = vec![0usize; self.templates.len()];
        for inst in &self.instances {
            counts[inst.template as usize] += 1;
        }
        self.templates
            .iter()
            .zip(counts)
            .map(|(t, n)| (t.name.as_str(), n))
            .collect()
    }
}

impl std::fmt::Debug for Fleet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fleet")
            .field("pvs", &self.instances.len())
            .field("iocs", &self.iocs)
            .field("templates", &self.templates.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mimus_profile::{
        Assignment, Introspection, Manifest, ScalarType, SessionRules, Slot, SlotRole, SlotWidth,
        Template,
    };

    const MONITOR: u8 = 0x0D;

    fn template(name: &str) -> Template {
        let mut t = Template::new(name, MONITOR);
        t.with_introspection(Introspection {
            full: vec![0xFF, 0xFD, 0x01],
            id_only: Some(vec![0xFF, 0xFE, 0x01]),
        })
        .push_fixed(vec![0x01])
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
        let mut p = Profile::new(
            Manifest {
                pvxs_version: "1.4.2".into(),
                epics_base_version: "7.0.8".into(),
                catalogue_sha256: "c".repeat(64),
                host: "test".into(),
                captured_at: "2026-08-15T00:00:00Z".into(),
                mimus_version: "0.1.0".into(),
                study_duration_s: 1.0,
                pvs_observed: 2,
                synthetic: false,
            },
            SessionRules::default(),
        );
        p.templates.push(template("dbl"));
        p.templates.push(template("int"));
        p.assignments = vec![
            Assignment {
                pv: "A".into(),
                template: "dbl".into(),
            },
            Assignment {
                pv: "B".into(),
                template: "int".into(),
            },
        ];
        p
    }

    fn spec(iocs: u16, pvs: u32, layout: Layout) -> FleetSpec {
        FleetSpec {
            iocs,
            pvs,
            layout,
            mix: vec![("dbl".into(), 3), ("int".into(), 1)],
            rate_mix: RateMix::uniform(1.0),
            names: NameTemplate::default_pattern("MIM"),
        }
    }

    #[test]
    fn builds_the_requested_population() {
        let f = Fleet::build(&profile(), &spec(4, 1000, Layout::Blocked)).unwrap();
        assert_eq!(f.len(), 1000);
        assert_eq!(f.iocs(), 4);
    }

    #[test]
    fn the_mix_is_respected() {
        let f = Fleet::build(&profile(), &spec(1, 1000, Layout::Blocked)).unwrap();
        let counts = f.template_counts();
        assert_eq!(counts, vec![("dbl", 750), ("int", 250)]);
    }

    #[test]
    fn the_mix_holds_even_for_an_awkward_population() {
        // The repeating pattern preserves both templates in a population of
        // seven. Taking the first N entries from a template-grouped list would
        // assign all seven PVs to the majority template.
        let f = Fleet::build(&profile(), &spec(1, 7, Layout::Blocked)).unwrap();
        let counts = f.template_counts();
        assert_eq!(counts[0].1 + counts[1].1, 7);
        assert!(counts[1].1 > 0, "the minority template must still appear");
    }

    #[test]
    fn blocked_layout_gives_contiguous_ranges() {
        let f = Fleet::build(&profile(), &spec(4, 1000, Layout::Blocked)).unwrap();
        assert_eq!(f.per_ioc_counts(), vec![250, 250, 250, 250]);
        assert_eq!(f.instances[0].ioc, 0);
        assert_eq!(f.instances[249].ioc, 0);
        assert_eq!(f.instances[250].ioc, 1);
    }

    #[test]
    fn round_robin_layout_interleaves() {
        let f = Fleet::build(&profile(), &spec(4, 1000, Layout::RoundRobin)).unwrap();
        assert_eq!(f.per_ioc_counts(), vec![250, 250, 250, 250]);
        assert_eq!(f.instances[0].ioc, 0);
        assert_eq!(f.instances[1].ioc, 1);
        assert_eq!(f.instances[4].ioc, 0);
    }

    #[test]
    fn an_uneven_population_still_fits_in_the_declared_iocs() {
        let f = Fleet::build(&profile(), &spec(3, 10, Layout::Blocked)).unwrap();
        assert_eq!(f.per_ioc_counts().iter().sum::<usize>(), 10);
        assert!(f.instances.iter().all(|i| i.ioc < 3));
    }

    #[test]
    fn an_ioc_only_claims_its_own_channels() {
        let f = Fleet::build(&profile(), &spec(4, 1000, Layout::Blocked)).unwrap();
        let name = f.names[300].clone();
        assert_eq!(f.lookup_on(&name, 1), Some(300));
        assert_eq!(f.lookup_on(&name, 0), None, "IOC 0 must not claim it");
        assert_eq!(f.lookup_on("nonexistent", 0), None);
    }

    #[test]
    fn names_are_unique() {
        let f = Fleet::build(&profile(), &spec(2, 500, Layout::Blocked)).unwrap();
        let mut sorted = f.names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 500);
    }

    #[test]
    fn instances_are_seeded_distinctly() {
        // Distinct seeds prevent channels of the same template from emitting
        // identical values at the same instant.
        let f = Fleet::build(&profile(), &spec(1, 100, Layout::Blocked)).unwrap();
        let mut seeds: Vec<u64> = f.instances.iter().map(|i| i.seed).collect();
        seeds.sort_unstable();
        seeds.dedup();
        assert_eq!(seeds.len(), 100);
    }

    #[test]
    fn rejects_a_mix_naming_an_absent_template() {
        let mut s = spec(1, 10, Layout::Blocked);
        s.mix = vec![("ntenum".into(), 1)];
        assert_eq!(
            Fleet::build(&profile(), &s).unwrap_err(),
            FleetError::UnknownTemplate("ntenum".into())
        );
    }

    #[test]
    fn rejects_degenerate_specs() {
        assert_eq!(
            Fleet::build(&profile(), &spec(0, 10, Layout::Blocked)).unwrap_err(),
            FleetError::NoIocs
        );
        assert_eq!(
            Fleet::build(&profile(), &spec(1, 0, Layout::Blocked)).unwrap_err(),
            FleetError::NoPvs
        );
        let mut s = spec(1, 10, Layout::Blocked);
        s.mix = vec![("dbl".into(), 0)];
        assert_eq!(
            Fleet::build(&profile(), &s).unwrap_err(),
            FleetError::EmptyMix
        );
    }

    #[test]
    fn layout_parses_both_ways() {
        assert_eq!("blocked".parse(), Ok(Layout::Blocked));
        assert_eq!("round-robin".parse(), Ok(Layout::RoundRobin));
        assert!("random".parse::<Layout>().is_err());
        assert_eq!(Layout::Blocked.to_string(), "blocked");
    }
}
