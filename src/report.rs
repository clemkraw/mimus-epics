//! What MIMUS prints when it starts and stops.
//!
//! The exit report is the main purpose of this tool: it records exactly how
//! many updates went on the wire, the figure a consumer's conservation
//! equation must reconcile. Everything else is supporting information.

use std::path::Path;

use mimus_emit::Counters;
use mimus_profile::Profile;
use mimus_serve::Fleet;
use mimus_serve::scenario::Played;

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
    /// The bytes those updates come to, headers included: what sizes a run
    /// of images, where forty updates a second can be forty megabytes.
    pub offered_bytes_per_s: f64,
    pub name_template: String,
    pub drop_connections_s: Option<f64>,
    pub pipeline: String,
    /// What a PUT does, and what an RPC gets.
    pub put: String,
    pub rpc: String,
    pub coalesce_us: u64,
    pub layout: String,
    pub backpressure: String,
    /// The high-water mark: a connection's queue under the drop policy, the
    /// most a connection may hold under the IOC policy.
    pub high_water: usize,
    /// Under the IOC policy, how long a connection may wait on its client.
    pub conn_timeout_s: Option<f64>,
    pub tick_us: u64,
    /// How channels alternate bursts and silences, if they do.
    pub activity: Option<mimus_emit::Activity>,
    /// Alarm transitions per channel per hour, and the channels that can
    /// carry one.
    pub alarms: Option<(f64, u64)>,
    /// The scenario played, if any.
    pub scenario: Option<ScenarioSummary>,
}

/// A scenario as the banner announces it and the report records it.
#[derive(Debug, Clone)]
pub struct ScenarioSummary {
    pub name: Option<String>,
    pub file: std::path::PathBuf,
    pub repeat: u32,
    /// Every pass, not counting the wait at the start.
    pub length_s: f64,
    /// The rate factor every schedule is built at.
    pub headroom: f64,
    /// One pass, phase by phase.
    pub plan: Vec<PlannedPhase>,
    /// Subscriptions waited for before the first phase, and for how long at
    /// most.
    pub waits_for: Option<(u64, f64)>,
}

/// A phase as the banner announces it.
#[derive(Debug, Clone)]
pub struct PlannedPhase {
    pub start_s: f64,
    pub label: String,
    pub length_s: f64,
    /// The commands it runs as it starts and as it ends.
    pub on_start: Option<String>,
    pub on_end: Option<String>,
}

impl ScenarioSummary {
    fn title(&self) -> String {
        match &self.name {
            Some(n) => format!("{n} ({})", self.file.display()),
            None => self.file.display().to_string(),
        }
    }
}

pub fn print_banner(
    profile: &Profile,
    fleet: &Fleet,
    run: &RunSummary,
    addrs: &[std::net::SocketAddr],
    search_port: u16,
) {
    out!("mimus — {}", profile.manifest.summary());
    out!();
    out!(
        "{} PVs across {} virtual IOCs, {} layout",
        run.pvs,
        run.iocs,
        run.layout
    );
    out!("rates {} Hz, types {}", run.rate_mix, run.mix);
    out!(
        "offered load {:.0} updates/s, about {}, backpressure {}",
        run.offered_eps,
        per_second(run.offered_bytes_per_s),
        run.backpressure
    );
    if let Some(timeout) = run.conn_timeout_s {
        out!(
            "  as a PVXS IOC: what a client does not read is kept, up to {} a connection; \
             a connection silent for {timeout} s is closed",
            size(run.high_water)
        );
    }
    if let Some((per_hour, channels)) = run.alarms {
        out!(
            "alarms {per_hour} per channel per hour, on the {channels} channels whose \
             type carries a transition: about {:.2}/s in all",
            per_hour * channels as f64 / 3600.0
        );
    }
    if let Some(a) = run.activity.filter(|a| !a.is_constant()) {
        out!(
            "activity {a}: each channel active {}% of the time, in bursts of {} updates \
             on average, the offered load counting only those",
            a.fraction * 100.0,
            a.mean_burst
        );
    }
    out!("names {}", run.name_template);
    if let Some(s) = run.drop_connections_s {
        out!("dropping one connection every {s} s");
    }
    out!(
        "pipeline {}, put {}, rpc {}",
        run.pipeline,
        run.put,
        run.rpc
    );
    if let Some(sc) = &run.scenario {
        out!();
        let passes = if sc.repeat > 1 {
            format!(", played {} times", sc.repeat)
        } else {
            String::new()
        };
        out!(
            "scenario {}{passes}, {:.0} s in all:",
            sc.title(),
            sc.length_s
        );
        if let Some((n, timeout)) = sc.waits_for {
            out!("  waits for {n} subscriptions, {timeout:.0} s at most, before its first phase");
        }
        for p in &sc.plan {
            out!(
                "  {:>8.1} s  {:<28} {:>8.1} s",
                p.start_s,
                p.label,
                p.length_s
            );
            if let Some(c) = &p.on_start {
                out!("              runs `{c}` as it starts");
            }
            if let Some(c) = &p.on_end {
                out!("              runs `{c}` as it ends");
            }
        }
        if sc.headroom > 1.0 {
            out!(
                "  schedules built for ×{}: thinned to the rate of each phase",
                sc.headroom
            );
        }
    }
    if run.coalesce_us > 0 {
        out!(
            "coalescing writes for up to {} µs — timestamps stay true but \
             delivery is delayed by that much",
            run.coalesce_us
        );
    }
    out!();
    out!("templates in play:");
    for (name, n) in fleet.template_counts() {
        let fixed = fleet
            .templates
            .iter()
            .find(|t| t.name == name)
            .is_some_and(|t| t.is_static());
        if fixed {
            out!("  {name:<24} {n:>8} PVs  static: opening message only");
        } else {
            out!("  {name:<24} {n:>8} PVs");
        }
    }
    out!();
    out!("listening:");
    for (i, addr) in addrs.iter().enumerate() {
        out!("  ioc {i:<3} {addr}");
    }
    out!();
    let port = if search_port == 5076 {
        String::new()
    } else {
        format!(" EPICS_PVA_BROADCAST_PORT={search_port}")
    };
    if search_port == 0 {
        out!("not answering searches, so clients must be told where to look:");
        let list: Vec<String> = addrs.iter().map(|a| a.to_string()).collect();
        out!(
            "  EPICS_PVA_NAME_SERVERS=\"{}\" pvxmonitor {}",
            list.join(" "),
            example_pv(fleet)
        );
    } else if addrs.iter().all(|a| a.ip().is_loopback()) {
        out!("answering searches over UDP, with the listeners on the loopback: a client");
        out!("on this host that searches 127.0.0.1 finds these channels.");
        out!(
            "  EPICS_PVA_AUTO_ADDR_LIST=NO EPICS_PVA_ADDR_LIST=127.0.0.1{port} pvxmonitor {}",
            example_pv(fleet)
        );
    } else {
        out!("answering searches over UDP: a client on this network finds these");
        out!("channels without being told where to look.");
        if port.is_empty() {
            out!("  pvxmonitor {}", example_pv(fleet));
        } else {
            out!(
                "  EPICS_PVA_BROADCAST_PORT={search_port} pvxmonitor {}",
                example_pv(fleet)
            );
        }
    }
    out!();
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
    /// Connections closed because a scenario took their IOC down.
    pub closed_down: u64,
    /// Monitors still active at the end of the run. This gauge tells a low
    /// offered load from a slow scheduler: both produce too few updates, but
    /// they need different fixes.
    pub subscriptions: u64,
    /// Channels that hold a value a client wrote.
    pub written: u64,
    /// Connections closed, under the IOC policy, because their client sent
    /// nothing, or took nothing, for the timeout.
    pub timed_out: u64,
    /// Connections closed, under the IOC policy, because more than the
    /// high-water mark waited for their client.
    pub overflowed: u64,
    /// The most bytes one connection had waiting for its client.
    pub backlog_peak: u64,
}

pub fn finish(
    profile: &Profile,
    run: &RunSummary,
    counters: Counters,
    sessions: Sessions,
    played: Option<&Played>,
    elapsed_s: f64,
    report_path: Option<&Path>,
) {
    let achieved = if elapsed_s > 0.0 {
        counters.emitted as f64 / elapsed_s
    } else {
        0.0
    };
    let offered = run.offered_eps;
    let throughput = if elapsed_s > 0.0 {
        counters.bytes as f64 / elapsed_s
    } else {
        0.0
    };

    // The file is written before anything is printed: the summary goes to a
    // terminal or a pipe that may already be gone, and the file is the record
    // of the run.
    let written = report_path.map(|path| {
        let json = serde_json::json!({
            "sessions": sessions.total,
            "sessions_dropped": sessions.dropped,
            "sessions_closed_down": sessions.closed_down,
            "sessions_timed_out": sessions.timed_out,
            "sessions_overflowed": sessions.overflowed,
            "subscriptions": sessions.subscriptions,
            "late_ticks": counters.late_ticks,
            "wheel_steps": counters.wheel_steps,
            "pipeline_acks": counters.pipeline_acks,
            "flow_control_holds": counters.flow_control_holds,
            "emitted": counters.emitted,
            "dropped_backpressure": counters.dropped_backpressure,
            "held": counters.held,
            "burst": counters.burst,
            "quiet": counters.quiet,
            "alarms": counters.alarms,
            "squashed": counters.squashed,
            "queued_at_close": counters.queued_at_close,
            "unsent": counters.unsent,
            "suspensions": counters.suspensions,
            "behind_s": counters.behind_us as f64 / 1e6,
            "backlog_peak_bytes": sessions.backlog_peak,
            "puts": counters.puts,
            "put_bytes": counters.put_bytes,
            "rpcs": counters.rpcs,
            "channels_written": sessions.written,
            "bytes": counters.bytes,
            "elapsed_s": elapsed_s,
            "achieved_eps": achieved,
            "offered_eps": offered,
            "bytes_per_s": throughput,
            "offered_bytes_per_s": run.offered_bytes_per_s,
            "run": {
                "iocs": run.iocs,
                "pvs": run.pvs,
                "rate_mix": run.rate_mix,
                "name_template": run.name_template,
                "drop_connections_s": run.drop_connections_s,
                "pipeline": run.pipeline,
                "put": run.put,
                "rpc": run.rpc,
                "coalesce_us": run.coalesce_us,
                "layout": run.layout,
                "backpressure": run.backpressure,
                "high_water": run.high_water,
                "conn_timeout_s": run.conn_timeout_s,
                "tick_us": run.tick_us,
                "mix": run.mix,
                "activity": run.activity.map(|a| a.to_string()),
                "alarms_per_hour": run.alarms.map(|(n, _)| n),
                "alarm_channels": run.alarms.map(|(_, c)| c),
            },
            "scenario": scenario_json(run, played),
            "profile": {
                "synthetic": profile.manifest.synthetic,
                "incomplete": profile.manifest.incomplete,
                "pvxs_version": profile.manifest.pvxs_version,
                "epics_base_version": profile.manifest.epics_base_version,
                "catalogue_checksum": profile.manifest.catalogue_checksum,
                "templates": profile.templates.len(),
            },
        });
        let text = serde_json::to_string_pretty(&json).unwrap_or_default();
        (path, std::fs::write(path, text))
    });

    out!();
    out!("sessions           {:>14}", sessions.total);
    if run.drop_connections_s.is_some() {
        out!("dropped by MIMUS   {:>14}", sessions.dropped);
    }
    out!("subscriptions      {:>14}", sessions.subscriptions);
    out!("emitted            {:>14}", counters.emitted);
    out!("dropped (pressure) {:>14}", counters.dropped_backpressure);
    if counters.held > 0 {
        out!("held (frozen)      {:>14}", counters.held);
    }
    if counters.quiet > 0 {
        out!("quiet (activity)   {:>14}", counters.quiet);
    }
    if counters.alarms > 0 {
        out!("alarm transitions  {:>14}", counters.alarms);
    }
    if sessions.closed_down > 0 {
        out!("closed by restarts {:>14}", sessions.closed_down);
    }
    // What a client that falls behind costs it, and what MIMUS held for it.
    if counters.squashed > 0 {
        out!("written over       {:>14}", counters.squashed);
    }
    if counters.queued_at_close > 0 {
        out!("queued at close    {:>14}", counters.queued_at_close);
    }
    if counters.unsent > 0 {
        out!("unsent at close    {:>14}", counters.unsent);
    }
    if sessions.timed_out > 0 {
        out!("closed, timed out  {:>14}", sessions.timed_out);
    }
    if sessions.overflowed > 0 {
        out!("closed, high water {:>14}", sessions.overflowed);
    }
    if counters.suspensions > 0 {
        out!("suspensions        {:>14}", counters.suspensions);
    }
    if counters.behind_us > 0 {
        out!(
            "behind             {:>14.1} s, holding {} at most",
            counters.behind_us as f64 / 1e6,
            size(sessions.backlog_peak as usize)
        );
    }
    // Requests, not updates: none of the counts above includes them.
    if counters.puts > 0 {
        out!("puts               {:>14}", counters.puts);
        out!("bytes put          {:>14}", counters.put_bytes);
    }
    if sessions.written > 0 {
        out!("channels written   {:>14}", sessions.written);
    }
    if counters.rpcs > 0 {
        out!("rpcs               {:>14}", counters.rpcs);
    }
    out!("bytes              {:>14}", counters.bytes);
    out!("elapsed            {elapsed_s:>14.1} s");
    out!("achieved           {achieved:>14.0} updates/s");
    out!("throughput         {:>16}", per_second(throughput));
    if let Some(bpe) = counters.bytes_per_event() {
        out!("bytes/event        {bpe:>14.1}");
    }
    if let Some(ratio) = counters.delivery_ratio() {
        out!("delivered          {:>13.2}%", ratio * 100.0);
    }
    if counters.late_ticks > 0 {
        out!("late ticks         {:>14}", counters.late_ticks);
    }
    if counters.pipeline_acks > 0 {
        out!("pipeline acks      {:>14}", counters.pipeline_acks);
    }
    if counters.flow_control_holds > 0 {
        out!("flow-control holds {:>14}", counters.flow_control_holds);
    }

    // Check for the cause before assigning blame. If no clients connected,
    // nothing was emitted because there were no subscribers. Reporting this
    // as an emitter problem would send the investigation in the wrong direction.
    if sessions.total == 0 {
        out!();
        out!(
            "note: no client ever connected, so nothing was emitted. MIMUS only \
             sends to subscribers; loading channels costs nothing on its own. Point \
             a consumer at the addresses above."
        );
    } else if let (Some(sc), Some(played)) = (&run.scenario, played) {
        print_scenario(sc, played);
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
        out!();
        out!(
            "note: achieved {achieved:.0}/s against {offered:.0}/s offered across all {} \
             channels. MIMUS only sends to subscribed channels, so the gap measures \
             something only if every channel was subscribed for the whole run. If it \
             was, {cause}.",
            run.pvs
        );
        if counters.late_ticks > 0 {
            out!(
                "      {} ticks ran late, so the shortfall is at least partly the \
                 emitter's own scheduling.",
                counters.late_ticks
            );
        }
    }

    if let Some((path, result)) = written {
        match result {
            Ok(()) => out!("\nreport written to {}", path.display()),
            Err(e) => eprintln!("warning: cannot write {}: {e}", path.display()),
        }
    }
}

/// The phases played, as the report file records them.
fn scenario_json(run: &RunSummary, played: Option<&Played>) -> serde_json::Value {
    let (Some(sc), Some(played)) = (&run.scenario, played) else {
        return serde_json::Value::Null;
    };
    let secs = |d: Option<std::time::Duration>| d.map(|d| d.as_secs_f64());
    serde_json::json!({
        "name": sc.name,
        "file": sc.file.display().to_string(),
        "repeat": sc.repeat,
        "length_s": sc.length_s,
        "headroom": sc.headroom,
        "wait": played.wait.as_ref().map(|w| serde_json::json!({
            "waited_s": w.waited.as_secs_f64(),
            "wanted": w.wanted,
            "held": w.held,
        })),
        "phases": played.phases.iter().map(|p| serde_json::json!({
            "label": p.label,
            "kind": p.kind,
            "cycle": p.cycle,
            "start_s": p.start.as_secs_f64(),
            "length_s": p.length.as_secs_f64(),
            "offered": p.offered,
            "emitted": p.counters.emitted,
            "bytes": p.counters.bytes,
            "dropped_backpressure": p.counters.dropped_backpressure,
            "held": p.counters.held,
            "burst": p.counters.burst,
            "quiet": p.counters.quiet,
            "alarms": p.counters.alarms,
            "late_ticks": p.counters.late_ticks,
            "flow_control_holds": p.counters.flow_control_holds,
            "squashed": p.counters.squashed,
            "queued_at_close": p.counters.queued_at_close,
            "unsent": p.counters.unsent,
            "suspensions": p.counters.suspensions,
            "behind_s": p.counters.behind_us as f64 / 1e6,
            "closed": p.closed,
            "subscriptions": p.subscriptions,
            "interrupted": p.interrupted,
        })).collect::<Vec<_>>(),
        "recoveries": played.recoveries.iter().map(|r| serde_json::json!({
            "ioc": r.ioc,
            "phase": r.phase,
            "before": r.before,
            "reconnected_s": secs(r.reconnected),
            "recovered_s": secs(r.recovered),
            "after": r.after,
        })).collect::<Vec<_>>(),
        "catch_ups": played.catch_ups.iter().map(|c| serde_json::json!({
            "phase": c.phase,
            "cycle": c.cycle,
            "behind": c.behind,
            "caught_up_s": secs(c.caught_up),
        })).collect::<Vec<_>>(),
        "commands": played.commands.iter().map(|c| serde_json::json!({
            "phase": c.phase,
            "cycle": c.cycle,
            "at": c.at.key(),
            "command": c.command,
            "status": c.outcome.as_ref().and_then(|o| o.as_ref().ok()),
            "error": c.outcome.as_ref().and_then(|o| o.as_ref().err()),
            "running": c.outcome.is_none(),
        })).collect::<Vec<_>>(),
    })
}

/// "1 connection", "2 connections".
fn connections(n: u64) -> String {
    if n == 1 {
        "1 connection".into()
    } else {
        format!("{n} connections")
    }
}

/// Prints each phase played, and how the consumer came back from restarts.
///
/// `offered` is what the phase asked for if every channel was subscribed
/// throughout; a gap between it and `emitted` measures the consumer only if
/// it was.
fn print_scenario(sc: &ScenarioSummary, played: &Played) {
    out!();
    out!("scenario {}", sc.title());
    if let Some(w) = &played.wait {
        if w.held >= w.wanted {
            out!(
                "  waited {:.1} s for {} subscriptions before the first phase",
                w.waited.as_secs_f64(),
                w.wanted
            );
        } else {
            out!(
                "  started after waiting {:.1} s with {} of the {} subscriptions waited for: \
                 the first phases ran against a consumer still subscribing",
                w.waited.as_secs_f64(),
                w.held,
                w.wanted
            );
        }
    }
    out!(
        "  {:<26} {:>9} {:>9} {:>11} {:>11} {:>9} {:>9} {:>5} {:>6} {:>9}",
        "phase",
        "start",
        "length",
        "offered",
        "emitted",
        "dropped",
        "held",
        "late",
        "closed",
        "subscr."
    );
    for p in &played.phases {
        let mut label = p.label.clone();
        if sc.repeat > 1 {
            label = format!("{} #{}", label, p.cycle + 1);
        }
        if p.interrupted {
            label.push_str(" (cut)");
        }
        out!(
            "  {:<26} {:>7.1} s {:>7.1} s {:>11.0} {:>11} {:>9} {:>9} {:>5} {:>6} {:>9}",
            label,
            p.start.as_secs_f64(),
            p.length.as_secs_f64(),
            p.offered,
            p.counters.emitted,
            p.counters.dropped_backpressure,
            p.counters.held,
            p.counters.late_ticks,
            p.closed,
            p.subscriptions
        );
    }
    if played.phases.iter().any(|p| p.interrupted) {
        out!("  (cut): the run ended before the phase did; its counts cover the part played.");
    }
    for r in &played.recoveries {
        let reconnected = match r.reconnected {
            Some(d) => format!("reconnected {:.2} s after it came back", d.as_secs_f64()),
            None => "no reconnection".to_string(),
        };
        let back = match r.recovered {
            _ if r.before == 0 => "it held no subscription to recover".to_string(),
            Some(d) => format!(
                "all {} subscriptions back after {:.2} s",
                r.before,
                d.as_secs_f64()
            ),
            None => format!(
                "{} of {} subscriptions back when the run ended",
                r.after, r.before
            ),
        };
        out!("  IOC {} ({}): {reconnected}; {back}", r.ioc, r.phase);
    }
    if !played.recoveries.is_empty() {
        out!("  Recovery times are polled every 10 ms.");
    }
    for c in &played.catch_ups {
        let mut label = c.phase.clone();
        if sc.repeat > 1 {
            label = format!("{} #{}", label, c.cycle + 1);
        }
        match c.caught_up {
            _ if c.behind == 0 => out!("  after {label}: no connection was behind"),
            Some(d) => out!(
                "  after {label}: {} behind, caught up {:.2} s later",
                connections(c.behind),
                d.as_secs_f64()
            ),
            None => out!(
                "  after {label}: {} behind, still behind when the run ended",
                connections(c.behind)
            ),
        }
    }
    for c in &played.commands {
        let what = match &c.outcome {
            Some(Ok(0)) => continue,
            Some(Ok(code)) => format!("exited with status {code}"),
            Some(Err(why)) => why.clone(),
            None => "was still running when the run ended".to_string(),
        };
        out!(
            "  phase {}, {}: `{}` {what}",
            c.phase,
            c.at.key(),
            c.command
        );
    }
}
/// A byte count as people read it: 512 B, 64 KiB, 1.5 MiB. Sizes of
/// messages and frames are binary, as the arrays they carry are.
pub fn size(n: usize) -> String {
    const KIB: f64 = 1024.0;
    let x = n as f64;
    if x < KIB {
        format!("{n} B")
    } else if x < KIB * KIB {
        format!("{} KiB", one_decimal(x / KIB))
    } else if x < KIB * KIB * KIB {
        format!("{} MiB", one_decimal(x / (KIB * KIB)))
    } else {
        format!("{} GiB", one_decimal(x / (KIB * KIB * KIB)))
    }
}

/// A rate of bytes as networks count them: 52 kB/s, 1.6 GB/s.
pub fn per_second(bytes_per_s: f64) -> String {
    let (scale, unit) = [(1e9, "GB/s"), (1e6, "MB/s"), (1e3, "kB/s")]
        .into_iter()
        .find(|(scale, _)| bytes_per_s >= *scale)
        .unwrap_or((1.0, "B/s"));
    format!("{} {unit}", one_decimal(bytes_per_s / scale))
}

/// One decimal, without a trailing `.0`.
fn one_decimal(x: f64) -> String {
    let s = format!("{x:.1}");
    s.strip_suffix(".0").map(str::to_string).unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_and_rates_read_as_people_write_them() {
        assert_eq!(size(512), "512 B");
        assert_eq!(size(1536), "1.5 KiB");
        assert_eq!(size(1 << 20), "1 MiB");
        assert_eq!(size(3 << 30), "3 GiB");
        assert_eq!(per_second(0.0), "0 B/s");
        assert_eq!(per_second(52_100.0), "52.1 kB/s");
        assert_eq!(per_second(419_430_400.0), "419.4 MB/s");
        assert_eq!(per_second(1.6e9), "1.6 GB/s");
    }
}
