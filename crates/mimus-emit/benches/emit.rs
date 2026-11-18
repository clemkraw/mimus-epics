//! Cost of producing one update, without socket I/O.
//!
//! This benchmark uses no external harness so the measured work remains
//! explicit and easy to inspect. The results may be published, so readers
//! should be able to understand exactly what is being timed.
//!
//! Run with `cargo bench -p mimus-emit`. Results are meaningful only in
//! a release build on an otherwise idle machine.

use std::time::Instant;

use mimus_emit::{Compiled, EpicsTime};
use mimus_profile::{ScalarType, Slot, SlotRole, SlotWidth, Template};

const MONITOR: u8 = 0x0D;

/// Approximate QSRV2 wire layout for an NTScalar double update:
/// request ID, sub-command, changed-set, value, seconds and nanoseconds.
fn ntscalar_double() -> Template {
    let mut t = Template::new("ntscalar_double", MONITOR);
    t.push_fixed(vec![0x01, 0x00, 0x00, 0x00, 0x00, 0x03, 0x83, 0x01])
        .push_slot(
            Slot::new(1, SlotWidth::Fixed { bytes: 8 }).with_role(SlotRole::Value {
                scalar: ScalarType::F64,
            }),
        )
        .push_slot(
            Slot::new(2, SlotWidth::Fixed { bytes: 8 }).with_role(SlotRole::TimestampSeconds),
        )
        .push_slot(Slot::new(3, SlotWidth::Fixed { bytes: 4 }).with_role(SlotRole::TimestampNanos));
    t.observations = 1000;
    t
}

fn ntscalar_string() -> Template {
    let mut t = Template::new("ntscalar_string", MONITOR);
    t.push_fixed(vec![0x01, 0x00, 0x00, 0x00, 0x00, 0x03, 0x83, 0x01])
        .push_slot(
            Slot::new(1, SlotWidth::Variable { min: 9, max: 27 })
                .with_samples(vec![b"\x0812:00:00".to_vec(), b"\x0c12:00:00.123".to_vec()]),
        )
        .push_slot(
            Slot::new(2, SlotWidth::Fixed { bytes: 8 }).with_role(SlotRole::TimestampSeconds),
        );
    t.observations = 1000;
    t
}

fn bench(label: &str, template: &Template, iters: u64) {
    let compiled = Compiled::new(template).expect("compile");
    let now = EpicsTime::now();
    let mut out: Vec<u8> = Vec::with_capacity(compiled.max_len() * 4096);

    // Warm up the output buffer and branch predictors.
    for step in 0..10_000u64 {
        compiled.render(step, 1, now, &mut out);
        if out.len() > compiled.max_len() * 2048 {
            out.clear();
        }
    }

    out.clear();
    let start = Instant::now();
    for step in 0..iters {
        compiled.render(step, 1, now, &mut out);
        // Periodically clear the buffer to avoid measuring the cost of
        // growing a multi-gigabyte allocation. A real emitter also clears
        // its buffer after sending.
        if out.len() >= compiled.max_len() * 2048 {
            out.clear();
        }
    }
    let elapsed = start.elapsed();

    let ns = elapsed.as_nanos() as f64 / iters as f64;
    let bytes = compiled.max_len();
    println!(
        "{label:<26} {ns:>7.1} ns/event  {:>9.2} M ev/s  {bytes:>3} B/event  path={}",
        1000.0 / ns,
        if compiled.is_flat() {
            "flat"
        } else {
            "chunked"
        }
    );
    // Keep the output observable to prevent the compiler from eliminating
    // the rendering loop.
    std::hint::black_box(&out);
}

fn main() {
    println!("mimus-emit — cost of one update, no socket\n");
    bench("ntscalar_double (flat)", &ntscalar_double(), 20_000_000);
    bench("ntscalar_string (chunked)", &ntscalar_string(), 20_000_000);
    println!(
        "\nThese are emission costs only. A published throughput figure must \
         come from a full run with its manifest."
    );
}
