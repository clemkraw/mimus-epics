//! What MIMUS prints when it starts and stops.
//!
//! The exit report is the main purpose of this tool: it records exactly how
//! many updates went on the wire, the figure a consumer's conservation
//! equation must reconcile. Everything else is supporting information.

use std::path::Path;

use mimus_emit::Counters;
use mimus_profile::Profile;
use mimus_serve::Fleet;

/// The configuration used for the run. Included in the report because a
/// throughput figure is meaningless without its offered load, rate mix,
/// and backpressure policy.
#[derive(Debug, Clone)]
pub struct RunSummary {
    pub mix: String,
    pub iocs: u16,
    pub pvs: u32,
    pub rate_mix: String,
    /// The total offered update rate implied by the population. This
    /// describes the run as a whole; it cannot be inferred from a single
    /// rate when the population spans multiple rates.
    pub offered_eps: f64,
    pub name_template: String,
    pub drop_connections_s: Option<f64>,
    pub pipeline: String,
    pub coalesce_us: u64,
    pub layout: String,
    pub backpressure: String,
    pub tick_us: u64,
}

pub fn print_banner(
    profile: &Profile,
    fleet: &Fleet,
    run: &RunSummary,
    addrs: &[std::net::SocketAddr],
    discovery: bool,
) {
    println!("mimus — {}", profile.manifest.summary());
    println!();
    println!(
        "{} PVs across {} virtual IOCs, {} layout",
        run.pvs, run.iocs, run.layout
    );
    println!("rates {} Hz, types {}", run.rate_mix, run.mix);
    println!(
        "offered load {:.0} updates/s, backpressure {}",
        run.offered_eps, run.backpressure
    );
    println!("names {}", run.name_template);
    if let Some(s) = run.drop_connections_s {
        println!("dropping one connection every {s} s");
    }
    println!("pipeline {}", run.pipeline);
    if run.coalesce_us > 0 {
        println!(
            "coalescing writes for up to {} µs — timestamps stay true but \
             delivery is delayed by that much",
            run.coalesce_us
        );
    }
    println!();
    println!("templates in play:");
    for (name, n) in fleet.template_counts() {
        println!("  {name:<24} {n:>8} PVs");
    }
    println!();
    println!("listening:");
    for (i, addr) in addrs.iter().enumerate() {
        println!("  ioc {i:<3} {addr}");
    }
    println!();
    if discovery {
        println!("answering searches over UDP: a client on this network finds these");
        println!("channels without being told where to look.");
        println!("  pvmonitor {}", example_pv(fleet));
    } else {
        println!("not answering searches, so clients must be told where to look:");
        let list: Vec<String> = addrs.iter().map(|a| a.to_string()).collect();
        println!(
            "  EPICS_PVA_NAME_SERVERS=\"{}\" pvmonitor {}",
            list.join(" "),
            example_pv(fleet)
        );
    }
    println!();
}

fn example_pv(fleet: &Fleet) -> &str {
    fleet.names.first().map(String::as_str).unwrap_or("PV")
}

/// What the run saw of its clients.
#[derive(Debug, Default, Clone, Copy)]
pub struct Sessions {
    /// Connections accepted over the whole run.
    pub total: u64,
    /// Connections MIMUS closed under `--drop-connections`.
    pub dropped: u64,
    /// Monitors still active at the end of the run. This gauge tells a low
    /// offered load from a slow scheduler: both produce too few updates, but
    /// they need different fixes.
    pub subscriptions: u64,
}

pub fn finish(
    profile: &Profile,
    run: &RunSummary,
    counters: Counters,
    sessions: Sessions,
    elapsed_s: f64,
    report_path: Option<&Path>,
) {
    let achieved = if elapsed_s > 0.0 {
        counters.emitted as f64 / elapsed_s
    } else {
        0.0
    };
    let offered = run.offered_eps;

    println!();
    println!("sessions           {:>14}", sessions.total);
    if run.drop_connections_s.is_some() {
        println!("dropped by MIMUS   {:>14}", sessions.dropped);
    }
    println!("subscriptions      {:>14}", sessions.subscriptions);
    println!("emitted            {:>14}", counters.emitted);
    println!("dropped (pressure) {:>14}", counters.dropped_backpressure);
    println!("bytes              {:>14}", counters.bytes);
    println!("elapsed            {elapsed_s:>14.1} s");
    println!("achieved           {achieved:>14.0} updates/s");
    if let Some(bpe) = counters.bytes_per_event() {
        println!("bytes/event        {bpe:>14.1}");
    }
    if let Some(ratio) = counters.delivery_ratio() {
        println!("delivered          {:>13.2}%", ratio * 100.0);
    }
    if counters.late_ticks > 0 {
        println!("late ticks         {:>14}", counters.late_ticks);
    }
    if counters.pipeline_acks > 0 {
        println!("pipeline acks      {:>14}", counters.pipeline_acks);
    }
    if counters.flow_control_holds > 0 {
        println!("flow-control holds {:>14}", counters.flow_control_holds);
    }

    // Check for the cause before assigning blame. If no clients connected,
    // nothing was emitted because there were no subscribers. Reporting this
    // as an emitter problem would send the investigation in the wrong direction.
    if sessions.total == 0 {
        println!();
        println!(
            "note: no client ever connected, so nothing was emitted. MIMUS only \
             sends to subscribers; loading channels costs nothing on its own. Point \
             a consumer at the addresses above."
        );
    } else if offered > 0.0 && achieved < offered * 0.95 {
        // Under drop, the updates discarded for a slow consumer are part of
        // the gap; only what remains beyond them is the emitter's.
        let cause = if run.backpressure == "drop" {
            let dropped = if elapsed_s > 0.0 {
                counters.dropped_backpressure as f64 / elapsed_s
            } else {
                0.0
            };
            format!(
                "{dropped:.0}/s of it were updates dropped because the consumer fell \
                 behind; any shortfall beyond those means MIMUS itself could not keep up \
                 and the run is not a valid measurement"
            )
        } else {
            "the gap is the consumer's absorption limit".to_string()
        };
        println!();
        println!(
            "note: achieved {achieved:.0}/s against {offered:.0}/s offered across all {} \
             channels. MIMUS only sends to subscribed channels, so the gap measures \
             something only if every channel was subscribed for the whole run. If it \
             was, {cause}.",
            run.pvs
        );
        if counters.late_ticks > 0 {
            println!(
                "      {} ticks ran late, so the shortfall is at least partly the \
                 emitter's own scheduling.",
                counters.late_ticks
            );
        }
    }

    let Some(path) = report_path else {
        return;
    };
    let json = serde_json::json!({
        "sessions": sessions.total,
        "sessions_dropped": sessions.dropped,
        "subscriptions": sessions.subscriptions,
        "late_ticks": counters.late_ticks,
        "wheel_steps": counters.wheel_steps,
        "pipeline_acks": counters.pipeline_acks,
        "flow_control_holds": counters.flow_control_holds,
        "emitted": counters.emitted,
        "dropped_backpressure": counters.dropped_backpressure,
        "bytes": counters.bytes,
        "elapsed_s": elapsed_s,
        "achieved_eps": achieved,
        "offered_eps": offered,
        "run": {
            "iocs": run.iocs,
            "pvs": run.pvs,
            "rate_mix": run.rate_mix,
            "name_template": run.name_template,
            "drop_connections_s": run.drop_connections_s,
            "pipeline": run.pipeline,
            "coalesce_us": run.coalesce_us,
            "layout": run.layout,
            "backpressure": run.backpressure,
            "tick_us": run.tick_us,
            "mix": run.mix,
        },
        "profile": {
            "synthetic": profile.manifest.synthetic,
            "pvxs_version": profile.manifest.pvxs_version,
            "epics_base_version": profile.manifest.epics_base_version,
            "catalogue_sha256": profile.manifest.catalogue_sha256,
            "templates": profile.templates.len(),
        },
    });
    match std::fs::write(
        path,
        serde_json::to_string_pretty(&json).unwrap_or_default(),
    ) {
        Ok(()) => println!("\nreport written to {}", path.display()),
        Err(e) => eprintln!("warning: cannot write {}: {e}", path.display()),
    }
}
