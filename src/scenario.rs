//! Reading scenario files.
//!
//! A scenario is written in TOML, one `[[phase]]` table per phase, in the
//! order they are played. This module turns the file into a
//! [`mimus_serve::Scenario`] and says, with the line, what it cannot read;
//! the scenario then checks itself against the fleet it will be played on.
//!
//! ```toml
//! name = "beam dump"
//!
//! [start]
//! subscriptions = "all"    # wait until the consumer has subscribed to every channel
//! timeout = "10m"
//!
//! [[phase]]
//! kind = "steady"
//! duration = "60s"
//!
//! [[phase]]
//! kind = "burst"           # every subscribed channel sends one update at once
//!
//! [[phase]]
//! kind = "restart"
//! iocs = [1]
//! down = "10s"
//! duration = "60s"
//! ```

use std::time::Duration;

use mimus_serve::scenario::{Action, Count, Iocs, Phase, Resume, Scenario, Start};
use serde::Deserialize;
use serde::de::{self, Deserializer, Visitor};

/// Reads the scenario in `path`.
pub fn load(path: &std::path::Path) -> Result<Scenario, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    parse(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Reads a scenario from its text.
pub fn parse(text: &str) -> Result<Scenario, String> {
    let raw: RawScenario = toml::from_str(text).map_err(|e| e.to_string())?;
    raw.into_scenario(text)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawScenario {
    name: Option<String>,
    /// For whoever reads the file; MIMUS does not use it.
    #[allow(dead_code)]
    description: Option<String>,
    repeat: Option<u32>,
    start: Option<RawStart>,
    /// Spanned, so that a mistake inside a table is reported at its own
    /// line rather than at the first table of the array.
    #[serde(rename = "phase", default)]
    phases: Vec<toml::Spanned<RawPhase>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStart {
    subscriptions: RawCount,
    timeout: Option<Span>,
}

/// One `[[phase]]` table. Its fields are read flat, so that a mistake is
/// reported at its own line, then checked against what the kind takes.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPhase {
    kind: RawKind,
    label: Option<String>,
    duration: Option<Span>,
    gain: Option<RawGain>,
    iocs: Option<RawIocs>,
    share: Option<f64>,
    resume: Option<RawResume>,
    down: Option<Span>,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum RawKind {
    Steady,
    Gain,
    Burst,
    Freeze,
    Restart,
}

impl RawKind {
    fn name(self) -> &'static str {
        match self {
            Self::Steady => "steady",
            Self::Gain => "gain",
            Self::Burst => "burst",
            Self::Freeze => "freeze",
            Self::Restart => "restart",
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum RawResume {
    Burst,
    Silent,
}

/// How long the scenario waits at its start when no timeout is given.
const DEFAULT_START_TIMEOUT: Duration = Duration::from_secs(600);

impl RawScenario {
    fn into_scenario(self, text: &str) -> Result<Scenario, String> {
        let start = match self.start {
            None => Start::Now,
            Some(s) => Start::Subscribed {
                count: s.subscriptions.0,
                timeout: s.timeout.map_or(DEFAULT_START_TIMEOUT, |t| t.0),
            },
        };
        let mut phases = Vec::new();
        for (n, raw) in self.phases.into_iter().enumerate() {
            let line = text[..raw.span().start.min(text.len())]
                .matches('\n')
                .count()
                + 1;
            phases.extend(raw.into_inner().into_phases(n + 1, line)?);
        }
        if phases.is_empty() {
            return Err("the scenario has no [[phase]]".into());
        }
        Ok(Scenario {
            name: self.name,
            start,
            repeat: self.repeat.unwrap_or(1),
            phases,
        })
    }
}

impl RawPhase {
    /// Returns the phases this table stands for: one, or several for a gain
    /// climbing in steps. `n` is the table's place in the file, from 1, and
    /// `line` the line it starts on.
    fn into_phases(self, n: usize, line: usize) -> Result<Vec<Phase>, String> {
        let kind = self.kind;
        let at = |why: String| format!("phase {n} ({}, line {line}): {why}", kind.name());
        // What each kind takes; anything else in the table is a mistake.
        let present = [
            ("duration", self.duration.is_some()),
            ("gain", self.gain.is_some()),
            ("iocs", self.iocs.is_some()),
            ("share", self.share.is_some()),
            ("resume", self.resume.is_some()),
            ("down", self.down.is_some()),
        ];
        let (required, optional): (&[&str], &[&str]) = match kind {
            RawKind::Steady => (&["duration"], &[]),
            RawKind::Gain => (&["duration", "gain"], &["iocs"]),
            RawKind::Burst => (&[], &["share", "iocs", "duration"]),
            RawKind::Freeze => (&["duration"], &["iocs", "resume"]),
            RawKind::Restart => (&["down"], &["duration", "iocs"]),
        };
        for (field, given) in present {
            if given && !required.contains(&field) && !optional.contains(&field) {
                return Err(at(format!(
                    "`{field}` does not apply to a {} phase",
                    kind.name()
                )));
            }
            if !given && required.contains(&field) {
                return Err(at(format!("a {} phase needs `{field}`", kind.name())));
            }
        }

        let targets = self.iocs.map_or(Iocs::All, |i| i.0);
        let duration = self.duration.map(|d| d.0);
        let label = self.label;
        let one = |default: String, duration: Duration, action| {
            Ok(vec![Phase {
                label: label.clone().unwrap_or(default),
                duration,
                action,
            }])
        };
        match kind {
            RawKind::Steady => one(
                "steady".into(),
                duration.unwrap_or_default(),
                Action::Steady,
            ),
            RawKind::Burst => {
                let share = self.share.unwrap_or(1.0);
                one(
                    format!("burst {}{}", percent(share), on(&targets)),
                    duration.unwrap_or_default(),
                    Action::Burst {
                        share,
                        iocs: targets,
                    },
                )
            }
            RawKind::Freeze => {
                let resume = match self.resume {
                    None | Some(RawResume::Burst) => Resume::Burst,
                    Some(RawResume::Silent) => Resume::Silent,
                };
                one(
                    format!("freeze{}", on(&targets)),
                    duration.unwrap_or_default(),
                    Action::Freeze {
                        iocs: targets,
                        resume,
                    },
                )
            }
            RawKind::Restart => {
                let down = self.down.map(|d| d.0).unwrap_or_default();
                one(
                    format!("restart{}", on(&targets)),
                    duration.unwrap_or(down),
                    Action::Restart {
                        iocs: targets,
                        down,
                    },
                )
            }
            RawKind::Gain => {
                let duration = duration.unwrap_or_default();
                let Some(RawGain { from, to, steps }) = self.gain else {
                    unreachable!("checked above");
                };
                let Some(steps) = steps else {
                    let factor = if from == to {
                        format!("×{}", factor(from))
                    } else {
                        format!("×{}→×{}", factor(from), factor(to))
                    };
                    return one(
                        format!("gain {factor}{}", on(&targets)),
                        duration,
                        Action::Gain {
                            from,
                            to,
                            iocs: targets,
                        },
                    );
                };
                if steps < 2 {
                    return Err(at(format!(
                        "steps = {steps}, but a staircase has at least two steps"
                    )));
                }
                // Equal steps from `from` to `to`, both included, each for an
                // equal share of the duration and each reported apart: the
                // step at which the consumer falls behind is the finding.
                let each = duration / steps;
                Ok((0..steps)
                    .map(|k| {
                        let g = from + (to - from) * f64::from(k) / f64::from(steps - 1);
                        Phase {
                            label: match &label {
                                Some(l) => format!("{l} {}/{steps}", k + 1),
                                None => format!("gain ×{}{}", factor(g), on(&targets)),
                            },
                            duration: each,
                            action: Action::Gain {
                                from: g,
                                to: g,
                                iocs: targets.clone(),
                            },
                        }
                    })
                    .collect())
            }
        }
    }
}

/// The IOCs a phase targets, as a label suffix: nothing for all of them.
fn on(iocs: &Iocs) -> String {
    match iocs {
        Iocs::All => String::new(),
        some => format!(" {some}"),
    }
}

/// A factor with no more decimals than it needs.
fn factor(f: f64) -> String {
    let s = format!("{f:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn percent(share: f64) -> String {
    format!("{}%", factor(share * 100.0))
}

/// A duration written with its unit: `500ms`, `60s`, `5m`, `1h`, or a sum
/// such as `1m30s`.
struct Span(Duration);

impl<'de> Deserialize<'de> for Span {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = Span;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str(
                    "a duration with its unit, such as \"500ms\", \"60s\", \"5m\" or \"1h\"",
                )
            }
            fn visit_str<E: de::Error>(self, s: &str) -> Result<Span, E> {
                parse_duration(s).map(Span).map_err(E::custom)
            }
        }
        d.deserialize_str(V)
    }
}

/// Parses a duration such as `1.5s` or `1m30s`.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let bad = || {
        format!(
            "{s:?} is not a duration: write a number and its unit, ms, s, m or h, \
             such as \"500ms\", \"60s\" or \"1m30s\""
        )
    };
    let mut total = 0.0f64;
    let mut rest = s.trim();
    if rest.is_empty() {
        return Err(bad());
    }
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .ok_or_else(bad)?;
        if digits == 0 {
            return Err(bad());
        }
        let value: f64 = rest[..digits].parse().map_err(|_| bad())?;
        rest = &rest[digits..];
        let unit_len = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let seconds = match &rest[..unit_len] {
            "ms" => 0.001,
            "s" => 1.0,
            "m" | "min" => 60.0,
            "h" => 3600.0,
            _ => return Err(bad()),
        };
        rest = &rest[unit_len..];
        total += value * seconds;
    }
    Duration::try_from_secs_f64(total).map_err(|_| bad())
}

/// The IOCs a phase acts on: `"all"`, or a list of indices.
struct RawIocs(Iocs);

impl<'de> Deserialize<'de> for RawIocs {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = RawIocs;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("\"all\" or a list of IOC indices such as [0, 2]")
            }
            fn visit_str<E: de::Error>(self, s: &str) -> Result<RawIocs, E> {
                match s {
                    "all" => Ok(RawIocs(Iocs::All)),
                    other => Err(E::custom(format!(
                        "{other:?}: write \"all\", or a list of IOC indices such as [0, 2]"
                    ))),
                }
            }
            fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<RawIocs, A::Error> {
                let mut list = Vec::new();
                while let Some(i) = seq.next_element::<u16>()? {
                    list.push(i);
                }
                Ok(RawIocs(Iocs::These(list)))
            }
        }
        d.deserialize_any(V)
    }
}

/// A number of subscriptions to wait for: `"all"`, or a count.
struct RawCount(Count);

impl<'de> Deserialize<'de> for RawCount {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = RawCount;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("\"all\" or a number of subscriptions")
            }
            fn visit_str<E: de::Error>(self, s: &str) -> Result<RawCount, E> {
                match s {
                    "all" => Ok(RawCount(Count::All)),
                    other => Err(E::custom(format!(
                        "{other:?}: write \"all\", or a number of subscriptions"
                    ))),
                }
            }
            fn visit_i64<E: de::Error>(self, n: i64) -> Result<RawCount, E> {
                u64::try_from(n)
                    .map(|n| RawCount(Count::Minimum(n)))
                    .map_err(|_| E::custom(format!("{n} subscriptions cannot be waited for")))
            }
        }
        d.deserialize_any(V)
    }
}

/// A rate factor: a number, or `{ from = 1, to = 20 }` for a ramp, with
/// `steps = 5` for a staircase.
struct RawGain {
    from: f64,
    to: f64,
    steps: Option<u32>,
}

impl<'de> Deserialize<'de> for RawGain {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Ramp {
            from: f64,
            to: f64,
            steps: Option<u32>,
        }
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = RawGain;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a factor such as 10, or a ramp such as { from = 1, to = 20 }")
            }
            fn visit_f64<E: de::Error>(self, g: f64) -> Result<RawGain, E> {
                Ok(RawGain {
                    from: g,
                    to: g,
                    steps: None,
                })
            }
            fn visit_i64<E: de::Error>(self, g: i64) -> Result<RawGain, E> {
                self.visit_f64(g as f64)
            }
            fn visit_map<A: de::MapAccess<'de>>(self, map: A) -> Result<RawGain, A::Error> {
                let r = Ramp::deserialize(de::value::MapAccessDeserializer::new(map))?;
                Ok(RawGain {
                    from: r.from,
                    to: r.to,
                    steps: r.steps,
                })
            }
        }
        d.deserialize_any(V)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_carry_their_unit() {
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("60s").unwrap(), Duration::from_secs(60));
        assert_eq!(parse_duration("1.5s").unwrap(), Duration::from_millis(1500));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_duration("1m30s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_duration("0s").unwrap(), Duration::ZERO);
        for bad in ["", "60", "s", "10 s", "-1s", "1d", "1.2.3s", "ms5"] {
            assert!(parse_duration(bad).is_err(), "{bad:?} was read");
        }
    }

    #[test]
    fn every_kind_of_phase_is_read() {
        let s = parse(
            r#"
            name = "all of it"
            repeat = 2

            [start]
            subscriptions = "all"
            timeout = "1m"

            [[phase]]
            kind = "steady"
            duration = "10s"

            [[phase]]
            kind = "gain"
            duration = "20s"
            gain = { from = 1, to = 4 }
            iocs = [1]

            [[phase]]
            kind = "burst"
            share = 0.5

            [[phase]]
            kind = "freeze"
            duration = "3s"
            resume = "silent"
            iocs = "all"

            [[phase]]
            kind = "restart"
            iocs = [0, 1]
            down = "5s"
            duration = "30s"
            label = "both restart"
            "#,
        )
        .unwrap();
        assert_eq!(s.name.as_deref(), Some("all of it"));
        assert_eq!(s.repeat, 2);
        assert_eq!(
            s.start,
            Start::Subscribed {
                count: Count::All,
                timeout: Duration::from_secs(60)
            }
        );
        let labels: Vec<&str> = s.phases.iter().map(|p| p.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "steady",
                "gain ×1→×4 IOC 1",
                "burst 50%",
                "freeze",
                "both restart"
            ]
        );
        assert_eq!(
            s.phases[2].duration,
            Duration::ZERO,
            "a burst is an instant"
        );
        assert_eq!(
            s.phases[3].action,
            Action::Freeze {
                iocs: Iocs::All,
                resume: Resume::Silent
            }
        );
        assert_eq!(s.length(), Duration::from_secs(2 * 63));
        assert!(s.check(2).is_ok());
    }

    #[test]
    fn a_staircase_is_played_and_reported_step_by_step() {
        let s = parse(
            r#"
            [[phase]]
            kind = "gain"
            duration = "40s"
            gain = { from = 1, to = 7, steps = 4 }
            "#,
        )
        .unwrap();
        let steps: Vec<(String, f64, Duration)> = s
            .phases
            .iter()
            .map(|p| match p.action {
                Action::Gain { from, to, .. } => {
                    assert_eq!(from, to, "each step is flat");
                    (p.label.clone(), from, p.duration)
                }
                _ => panic!("a gain"),
            })
            .collect();
        let ten = Duration::from_secs(10);
        assert_eq!(
            steps,
            [
                ("gain ×1".to_string(), 1.0, ten),
                ("gain ×3".to_string(), 3.0, ten),
                ("gain ×5".to_string(), 5.0, ten),
                ("gain ×7".to_string(), 7.0, ten),
            ]
        );
        assert_eq!(s.headroom(), 7.0);
    }

    #[test]
    fn a_mistake_is_named_with_its_line() {
        let cases = [
            ("[[phase]]\nkind = \"steady\"\nduration = 60\n", "line 3"),
            (
                "[[phase]]\nkind = \"steady\"\nduraton = \"60s\"\n",
                "line 3",
            ),
            (
                "[[phase]]\nkind = \"stedy\"\nduration = \"60s\"\n",
                "line 2",
            ),
            (
                "[[phase]]\nkind = \"burst\"\niocs = \"some\"\n",
                "write \"all\"",
            ),
            (
                "[[phase]]\nkind = \"gain\"\nduration = \"1s\"\ngain = { from = 1, too = 2 }\n",
                "line 4",
            ),
        ];
        for (text, want) in cases {
            let e = parse(text).unwrap_err();
            assert!(e.contains(want), "{text:?} gave {e}, without {want:?}");
        }
        assert!(
            parse("name = \"no phase\"\n")
                .unwrap_err()
                .contains("no [[phase]]")
        );
        let e = parse(
            "[[phase]]\nkind = \"steady\"\nduration = \"1s\"\n\n\
             [[phase]]\nkind = \"steady\"\nshare = 0.5\nduration = \"1s\"\n",
        )
        .unwrap_err();
        assert!(
            e.contains("phase 2 (steady, line 5): `share` does not apply"),
            "{e}"
        );
        let e = parse("[[phase]]\nkind = \"restart\"\niocs = [0]\n").unwrap_err();
        assert!(
            e.contains("phase 1 (restart, line 1): a restart phase needs `down`"),
            "{e}"
        );
        // A mistake in a later table is named at its own line, not the first's.
        let e = parse(
            "[[phase]]\nkind = \"steady\"\nduration = \"1s\"\n\n\
             [[phase]]\nkind = \"gain\"\nduration = 60\ngain = 2\n",
        )
        .unwrap_err();
        assert!(e.contains("line 7"), "{e}");
        assert!(
            parse("[[phase]]\nkind = \"gain\"\nduration = \"1s\"\ngain = { from = 1, to = 2, steps = 1 }\n")
                .unwrap_err()
                .contains("at least two steps")
        );
    }

    #[test]
    fn the_scenarios_shipped_with_mimus_are_valid() {
        for (name, text) in [
            ("beam-dump", include_str!("../scenarios/beam-dump.toml")),
            ("ioc-restart", include_str!("../scenarios/ioc-restart.toml")),
            ("freeze", include_str!("../scenarios/freeze.toml")),
            ("staircase", include_str!("../scenarios/staircase.toml")),
            ("bad-day", include_str!("../scenarios/bad-day.toml")),
        ] {
            let s = parse(text).unwrap_or_else(|e| panic!("{name}: {e}"));
            s.check(4).unwrap_or_else(|e| panic!("{name}: {e}"));
        }
    }
}
