//! A hand-written profile that lets you try the server without a reference
//! IOC for calibration.
//!
//! This is the only place in MIMUS where message bytes are written rather
//! than observed. It is permitted only because the profile declares itself:
//! the manifest sets `synthetic: true`, and every command that loads such a
//! profile prints a warning banner.
//!
//! Unlike a learned profile, these bytes are hand-encoded from the PVAccess
//! specification. They are intended to be **valid**: a real client should
//! connect, subscribe, and display changing values. But valid does not mean
//! faithful. A real QSRV2 NTScalar includes `display`, `control`, and
//! `valueAlarm` blocks that this profile omits. It also uses a different
//! changed-set and follows PVXS's field ordering. Only `mimus calibrate`
//! produces bytes that match those emitted by a real IOC.

use std::path::Path;

use mimus_profile::{
    Assignment, Introspection, Manifest, Profile, ScalarType, SessionRules, Slot, SlotRole,
    SlotWidth, Template,
};
use mimus_wire::ByteOrder;
use mimus_wire::codec::{put_size, put_string};

const MONITOR: u8 = 0x0D;
const LE: ByteOrder = ByteOrder::Little;

/// PVA field type codes.
mod code {
    /// Signed 32-bit integer.
    pub const INT: u8 = 0x22;
    /// Signed 64-bit integer.
    pub const LONG: u8 = 0x23;
    /// IEEE 754 double.
    pub const DOUBLE: u8 = 0x43;
    pub const STRING: u8 = 0x60;
    pub const STRUCT: u8 = 0x80;
    /// Full introspection descriptor, followed by the ID used to cache it.
    pub const FULL_WITH_ID: u8 = 0xFD;
    /// Reference to a previously cached descriptor ID.
    pub const ONLY_ID: u8 = 0xFE;
    /// Success status with no message. Nothing follows this byte.
    pub const STATUS_OK: u8 = 0xFF;
}

/// A structure descriptor member: its name and the function that writes its type.
type Member<'a> = (&'a str, &'a dyn Fn(&mut Vec<u8>));

/// Append a structure field descriptor.
fn put_struct(out: &mut Vec<u8>, type_id: &str, fields: &[Member<'_>]) {
    out.push(code::STRUCT);
    put_string(out, type_id, LE);
    put_size(out, fields.len(), LE);
    for (name, write) in fields {
        put_string(out, name, LE);
        write(out);
    }
}

fn scalar_field(kind: u8) -> impl Fn(&mut Vec<u8>) {
    move |out: &mut Vec<u8>| out.push(kind)
}

/// Build the introspection descriptor for a minimal NTScalar of the given
/// value type.
///
/// Field order determines the bit numbering used by the changed-set.
/// Numbering is depth-first from the root:
///
/// ```text
/// 0 structure          5 alarm.message
/// 1 value              6 timeStamp
/// 2 alarm              7 timeStamp.secondsPastEpoch
/// 3 alarm.severity     8 timeStamp.nanoseconds
/// 4 alarm.status       9 timeStamp.userTag
/// ```
fn ntscalar_descriptor(type_id: u16, value_kind: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    // A monitor INIT reply contains the request ID, sub-command, status, and
    // descriptor. Include the status in this blob because `calibrate` captures
    // everything after the sub-command verbatim, without interpreting it.
    out.push(code::STATUS_OK);
    out.push(code::FULL_WITH_ID);
    out.extend_from_slice(&type_id.to_le_bytes());

    let alarm = |o: &mut Vec<u8>| {
        put_struct(
            o,
            "alarm_t",
            &[
                ("severity", &scalar_field(code::INT)),
                ("status", &scalar_field(code::INT)),
                ("message", &scalar_field(code::STRING)),
            ],
        )
    };
    let time_stamp = |o: &mut Vec<u8>| {
        put_struct(
            o,
            "time_t",
            &[
                ("secondsPastEpoch", &scalar_field(code::LONG)),
                ("nanoseconds", &scalar_field(code::INT)),
                ("userTag", &scalar_field(code::INT)),
            ],
        )
    };

    put_struct(
        &mut out,
        "epics:nt/NTScalar:1.0",
        &[
            ("value", &scalar_field(value_kind)),
            ("alarm", &alarm),
            ("timeStamp", &time_stamp),
        ],
    );
    out
}

/// The identifier-only descriptor form used by subsequent channels of a type.
fn ntscalar_cached(type_id: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(4);
    out.push(code::STATUS_OK);
    out.push(code::ONLY_ID);
    out.extend_from_slice(&type_id.to_le_bytes());
    out
}

/// The changed-set for a routine update: value, seconds, and nanoseconds.
///
/// These are bits 1, 7, and 8 in the numbering above. A PVA bitset consists
/// of a size followed by bytes, with the least significant bit first, so
/// these bits are encoded as `0x82 0x01`.
fn changed_set() -> Vec<u8> {
    let mut out = Vec::new();
    put_size(&mut out, 2, LE);
    out.push(0b1000_0010); // bits 1 and 7
    out.push(0b0000_0001); // bit 8
    out
}

/// An empty overrun bitset.
///
/// This terminates every monitor update. Without it, a PVXS client rejects
/// the message because the payload ends before all expected data has arrived.
fn overrun_none() -> Vec<u8> {
    let mut out = Vec::new();
    put_size(&mut out, 0, LE);
    out
}

fn fixed_scalar(name: &str, type_id: u16, kind: u8, scalar: ScalarType, width: usize) -> Template {
    let mut t = Template::new(name, MONITOR);
    t.with_introspection(Introspection {
        full: ntscalar_descriptor(type_id, kind),
        id_only: Some(ntscalar_cached(type_id)),
    })
    .push_fixed(changed_set())
    .push_slot(
        Slot::new(1, SlotWidth::Fixed { bytes: width }).with_role(SlotRole::Value { scalar }),
    )
    .push_slot(Slot::new(2, SlotWidth::Fixed { bytes: 8 }).with_role(SlotRole::TimestampSeconds))
    .push_slot(Slot::new(3, SlotWidth::Fixed { bytes: 4 }).with_role(SlotRole::TimestampNanos))
    .push_fixed(overrun_none());
    t.observations = 1000;
    t
}

/// A string-valued NTScalar that also exercises the variable-length emission
/// path. Without it, a demo run would test only the simpler fixed-width case.
fn string_template(type_id: u16) -> Template {
    // A PVA string is a size prefix followed by its bytes. Each sample
    // therefore carries its own length prefix, and the slot width varies.
    let samples = vec![
        b"\x0812:00:00".to_vec(),
        b"\x0c12:00:00.123".to_vec(),
        b"\x0812:00:01".to_vec(),
    ];
    let mut t = Template::new("string", MONITOR);
    t.with_introspection(Introspection {
        full: ntscalar_descriptor(type_id, code::STRING),
        id_only: Some(ntscalar_cached(type_id)),
    })
    .push_fixed(changed_set())
    .push_slot(Slot::new(1, SlotWidth::Variable { min: 9, max: 13 }).with_samples(samples))
    .push_slot(Slot::new(2, SlotWidth::Fixed { bytes: 8 }).with_role(SlotRole::TimestampSeconds))
    .push_slot(Slot::new(3, SlotWidth::Fixed { bytes: 4 }).with_role(SlotRole::TimestampNanos))
    .push_fixed(overrun_none());
    t.observations = 1000;
    t
}

fn build() -> Profile {
    let manifest = Manifest {
        pvxs_version: "unknown".into(),
        epics_base_version: "unknown".into(),
        catalogue_sha256: "0".repeat(64),
        host: "hand-written".into(),
        captured_at: "1970-01-01T00:00:00Z".into(),
        mimus_version: env!("CARGO_PKG_VERSION").to_string(),
        study_duration_s: 0.0,
        pvs_observed: 0,
        synthetic: true,
    };

    let mut p = Profile::new(manifest, SessionRules::default());
    p.templates
        .push(fixed_scalar("double", 1, code::DOUBLE, ScalarType::F64, 8));
    p.templates
        .push(fixed_scalar("int", 2, code::INT, ScalarType::I32, 4));
    p.templates.push(string_template(3));

    // Assign two PVs to each template. Every template must be used, and two
    // instances per type make the descriptor-cache path observable.
    for t in ["double", "int", "string"] {
        for i in 0..2 {
            p.assignments.push(Assignment {
                pv: format!("DEMO:{t}:{i}"),
                template: t.to_string(),
            });
        }
    }
    p
}

pub fn write(path: &Path) -> i32 {
    let profile = build();
    if let Err(e) = profile.validate(mimus_profile::DEFAULT_MIN_OBSERVATIONS) {
        // This indicates that the demo builder and profile format are out of sync.
        eprintln!("error: the demo profile is not valid: {e}");
        return 1;
    }
    let json = match profile.to_json() {
        Ok(j) => j,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    if let Err(e) = std::fs::write(path, json) {
        eprintln!("error: cannot write {}: {e}", path.display());
        return 1;
    }
    println!("wrote {}", path.display());
    println!();
    println!("This profile is SYNTHETIC. The bytes are hand-encoded against the");
    println!("PVA specification, not observed from an IOC: a client will connect");
    println!("and show moving values, but a real NTScalar from QSRV2 carries more");
    println!("than this one does. Good for exercising the server, useless as a");
    println!("measurement.");
    println!();
    println!(
        "  mimus serve --profile {} --pvs 1000 --rate-hz 1",
        path.display()
    );
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_demo_profile_is_valid() {
        build()
            .validate(mimus_profile::DEFAULT_MIN_OBSERVATIONS)
            .unwrap();
    }

    #[test]
    fn the_demo_profile_is_marked_synthetic() {
        // The startup banner uses this flag to prevent demo runs from being
        // mistaken for measurements.
        assert!(build().manifest.synthetic);
    }

    #[test]
    fn the_demo_covers_both_emission_paths() {
        let p = build();
        assert!(p.template("double").unwrap().is_fixed_width());
        assert!(
            !p.template("string").unwrap().is_fixed_width(),
            "a demo that only covers fixed-width templates proves half the emitter"
        );
    }

    #[test]
    fn every_demo_template_has_two_channels() {
        let p = build();
        for (_, n) in p.coverage() {
            assert_eq!(n, 2, "two channels per type is what exposes the cache rule");
        }
    }

    #[test]
    fn an_init_reply_starts_with_a_status() {
        // A PVA monitor INIT reply contains the request ID, sub-command,
        // status, and descriptor. Omitting the status shifts the remaining
        // fields and causes the client to reject the message.
        for t in build().templates {
            let intro = t.introspection.expect("monitor template");
            assert_eq!(intro.full[0], code::STATUS_OK, "{}: full", t.name);
            assert_eq!(
                intro.id_only.as_ref().unwrap()[0],
                code::STATUS_OK,
                "{}: id-only",
                t.name
            );
        }
    }

    #[test]
    fn the_cached_form_carries_the_same_type_id() {
        // The client caches the full descriptor under its type ID and uses
        // the ID in the cached form to retrieve it. If the IDs differ, the
        // second channel of that type cannot resolve its descriptor.
        for t in build().templates {
            let intro = t.introspection.expect("monitor template");
            let full_id = &intro.full[2..4];
            let cached = intro.id_only.unwrap();
            assert_eq!(cached[1], code::ONLY_ID, "{}", t.name);
            assert_eq!(&cached[2..4], full_id, "{}: type id must match", t.name);
        }
    }

    #[test]
    fn type_ids_are_distinct_across_templates() {
        let mut ids = Vec::new();
        for t in build().templates {
            let intro = t.introspection.expect("monitor template");
            ids.push([intro.full[2], intro.full[3]]);
        }
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "type ids collide");
    }

    #[test]
    fn an_update_ends_with_an_overrun_bitset() {
        // A real PVXS client exposed this bug: without the trailing overrun
        // bitset, the payload ends before the client has received all expected
        // data, so it drops the connection rather than just rejecting the update.
        use mimus_profile::Chunk;
        for t in build().templates {
            let Some(Chunk::Fixed { bytes }) = t.chunks.last() else {
                panic!("{}: last chunk must be the overrun bitset", t.name);
            };
            assert_eq!(bytes, &vec![0u8], "{}: empty overrun bitset", t.name);
        }
    }

    #[test]
    fn the_changed_set_marks_value_and_timestamp() {
        // Bits 1, 7, and 8 in the depth-first field numbering. Incorrect bits
        // cause the client to decode values into the wrong fields.
        assert_eq!(changed_set(), vec![2, 0b1000_0010, 0b0000_0001]);
    }

    #[test]
    fn the_descriptor_names_the_normative_type() {
        let d = ntscalar_descriptor(1, code::DOUBLE);
        let text = String::from_utf8_lossy(&d);
        assert!(text.contains("epics:nt/NTScalar:1.0"), "{text:?}");
        assert!(text.contains("value"));
        assert!(text.contains("secondsPastEpoch"));
    }

    #[test]
    fn the_demo_round_trips_through_json() {
        let json = build().to_json().unwrap();
        let back = Profile::from_json(&json).unwrap();
        assert!(back.manifest.synthetic);
        assert_eq!(back.templates.len(), 3);
    }
}
