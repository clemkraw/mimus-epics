//! `mimus` CLI.
//!
//! Intentionally thin: parse arguments, load a profile, build a fleet, bind
//! sockets, and print a report. All substantive decisions live in the library
//! crates, where they can be tested without starting a process.

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use mimus_emit::{Activity, Backpressure, Counters, RateMix, Schedule};
use mimus_profile::{DescriptorPolicy, Profile};
use mimus_serve::fleet::{Fleet, FleetSpec, Layout};
use mimus_serve::session::{Pipeline, SessionConfig};
use mimus_serve::{Dropper, Ioc, NameTemplate};

/// `println!` for what MIMUS tells its operator, which survives a closed
/// standard output.
///
/// `println!` panics when the reader has gone: `mimus serve | tee run.log`
/// interrupted with Ctrl-C stops `tee` first, and the summary MIMUS prints
/// on its way out would then abort it before the report file is written.
/// What goes to standard output is informational, so a reader that left is
/// not a reason to stop.
macro_rules! out {
    () => {
        out!("")
    };
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stdout(), $($arg)*);
    }};
}

mod calibrate;
mod demo;
mod report;
mod scenario;

#[derive(Parser)]
#[command(
    name = "mimus",
    version,
    about = "A self-calibrating PVAccess traffic generator",
    long_about = "MIMUS learns the shape of the messages a reference softIocPVX emits, \
                  then replays them at facility scale without any EPICS code in memory.\n\n\
                  Scope is PVXS (QSRV2)."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve a fleet of virtual IOCs from a profile.
    Serve(Box<ServeArgs>),
    /// Print the contents of a profile without serving it.
    Inspect {
        /// Profile to print.
        profile: PathBuf,
        /// Compare against a second profile.
        ///
        /// Useful across PVXS upgrades: a different set of types or type
        /// descriptors from the same environment is a MIMUS bug, and exits
        /// nonzero; between environments, it is a change in PVXS wire
        /// behavior. Shape differences are reported but expected: which bytes
        /// are fixed depends on what moved during each observation.
        #[arg(long)]
        diff: Option<PathBuf>,
    },
    /// Extract templates by observing a reference softIocPVX.
    ///
    /// The IOC must have the reference catalogue loaded. Calibration monitors
    /// every channel in the list until each has yielded enough data to analyze,
    /// and checks every template against the bytes it was derived from. A type
    /// that fails the check is left out and named; if none passes, no profile
    /// is written.
    ///
    /// Exits 0 with a profile written, 3 when the completeness criterion was
    /// not met and no profile was written, 1 on any other error.
    Calibrate(Box<CalibrateArgs>),
    /// Write a hand-crafted profile for trying the server out.
    ///
    /// Not a substitute for calibration. The profile is marked synthetic, and
    /// every command that uses it makes that explicit.
    DemoProfile {
        #[arg(short, long, default_value = "demo.profile.json")]
        out: PathBuf,
    },
}

#[derive(Parser)]
struct ServeArgs {
    /// Profile produced by `mimus calibrate`.
    #[arg(short, long)]
    profile: PathBuf,

    /// Number of virtual IOCs. Each listens on a separate port and appears to
    /// clients as a separate server. Consumers typically run one task per
    /// connection, so this limits how much work can spread across cores.
    #[arg(long, default_value_t = 1)]
    iocs: u16,

    /// Total number of channels across the fleet.
    #[arg(long, default_value_t = 1000)]
    pvs: u32,

    /// Update rate per channel, in Hz. The offered load is `pvs × rate`.
    #[arg(long = "rate-hz", default_value_t = 1.0)]
    rate_hz: f64,

    /// Distribute channels across several rates: `10:2,1:60,0.1:38` assigns
    /// 2% of channels to 10 Hz, 60% to 1 Hz, and 38% to 0.1 Hz. Shares are
    /// relative and need not add up to 100.
    ///
    /// Real facilities rarely run everything at the same frequency. The
    /// distribution shapes the offered load, not just its total volume.
    /// Overrides `--rate-hz`.
    #[arg(long = "rate-mix")]
    rate_mix: Option<String>,

    /// Let channels post in bursts and fall silent in between, as behind a
    /// monitor deadband: `0.3:20` keeps each channel active 30% of the time,
    /// in bursts of 20 updates on average, so it sends about 30% of what its
    /// rate asks, at irregular times.
    ///
    /// A consumer that sees every channel post like a clock is spared the
    /// gaps and runs a facility puts on it. The bursts and silences come
    /// from each channel's seed, so the same channel goes through the same
    /// ones on every run.
    #[arg(long)]
    activity: Option<String>,

    /// Send alarm transitions: this many per channel and per hour, on the
    /// channels whose type carries one. `2` gives each such channel two an
    /// hour on average: a raise, and later its clear.
    ///
    /// The transitions are those the reference IOC sent, learned by
    /// `mimus calibrate` from the catalogue's channels that cross their alarm
    /// limits, and replayed in the order observed. A consumer that indexes
    /// or alerts on alarms is measured on a path it otherwise never takes.
    #[arg(long)]
    alarms: Option<f64>,

    /// Pattern for channel names. Tokens, each in braces: `i` global index,
    /// `ioc` IOC index, `n` index within the IOC, `t` template name. A width
    /// zero-pads, as in `{i:06}`. The default is `{prefix}:{t}:{i}`, with
    /// the `--prefix` value.
    ///
    /// Real facility names can be thirty or forty characters long. At a
    /// million channels, this increases the memory used by the consumer's
    /// name maps, so shorter names understate its footprint.
    #[arg(long = "name-template")]
    name_template: Option<String>,

    /// Whether to honor pipeline credits (`strict`) or only count them
    /// (`permissive`, the default).
    ///
    /// A client that pipelines (`record[pipeline=true,queueSize=N]`) gives a
    /// queue size of N updates and acknowledges each batch it has consumed.
    /// A PVXS server sends nothing to such a monitor once that credit is
    /// spent, and `strict` does the same. `permissive` keeps every
    /// subscription at the offered rate. Monitors that do not pipeline are
    /// served the same way under both.
    #[arg(long, default_value = "permissive")]
    pipeline: Pipeline,

    /// Drop one live connection every N seconds, without warning.
    ///
    /// The consumer must detect the disconnect, reconnect, and resubscribe
    /// to every channel it had there. At scale, with tens of thousands of
    /// channels per connection, that is a much harder test than steady
    /// state, and one that is hard to stage against real IOCs without
    /// restarting them.
    ///
    /// A PVXS client that finds MIMUS through `EPICS_PVA_NAME_SERVERS` waits
    /// several seconds before reconnecting (up to seven in testing), so a
    /// shorter interval keeps it disconnected; one that finds MIMUS through
    /// UDP search comes back at once.
    #[arg(long = "drop-connections")]
    drop_connections: Option<f64>,

    /// Select templates to serve and their relative proportions.
    ///
    /// `ntscalar_f64` serves a single type; `ntscalar_f64:3,ntscalar_i32:1`
    /// allocates three quarters to `ntscalar_f64` and one quarter to
    /// `ntscalar_i32`. Names are those `mimus inspect` lists. By default, all
    /// templates in the profile are served in equal proportions.
    ///
    /// This matters because a population consisting of a single template
    /// can keep a consumer on its fast path and inflate performance figures.
    /// Comparing throughput across different template mixes is meaningless.
    /// The selected mix belongs in the run manifest.
    #[arg(long)]
    mix: Option<String>,

    /// Prefix for generated PV names. A `--name-template` replaces it.
    #[arg(long, default_value = "MIM", conflicts_with = "name_template")]
    prefix: String,

    /// How channels are distributed across IOCs.
    ///
    /// `blocked` assigns contiguous ranges, as in a real facility.
    /// `round-robin` interleaves channels, which can favor clients that
    /// partition their work in the same way. Each IOC still gets every
    /// template and every rate of the mix, in its proportions.
    #[arg(long, default_value = "blocked")]
    layout: Layout,

    /// How to handle backpressure when the consumer falls behind.
    ///
    /// `block` lets TCP throttle the sender, measuring the consumer's
    /// effective capacity. `drop` maintains the nominal rate and discards,
    /// and counts, the updates that do not fit, as intermediate values are
    /// lost when an IOC's monitor queue overflows.
    /// These modes answer different questions; published results must
    /// identify which one was used.
    #[arg(long, default_value = "block")]
    backpressure: Backpressure,

    /// Scheduler resolution in microseconds, from 1 to 1 000 000. Reduce it
    /// for rates above a few hundred hertz.
    #[arg(long = "tick-us", default_value_t = 1000)]
    tick_us: u64,

    /// Address of the TCP listeners. `0.0.0.0` makes the server reachable
    /// from the network.
    #[arg(long, default_value = "127.0.0.1")]
    bind: String,

    /// Starting port. Each IOC uses the next port; zero lets the OS assign it.
    #[arg(long = "base-port", default_value_t = 5075)]
    base_port: u16,

    /// Bytes allowed to queue on one connection before the drop policy
    /// discards updates: the stand-in for an IOC's monitor queue depth.
    #[arg(long = "high-water", default_value_t = 1 << 20)]
    high_water: usize,

    /// Send the full type descriptor in every monitor's opening reply,
    /// whatever the profile recorded.
    ///
    /// By default MIMUS does what the reference server was observed doing. A
    /// profile calibrated against PVXS already sends full descriptors; this
    /// only matters for a profile that recorded cached identifiers.
    #[arg(long = "always-full-descriptor")]
    always_full: bool,

    /// Delay outgoing updates by up to this many microseconds before writing.
    ///
    /// Zero, the default, writes on every tick, resulting in one syscall per
    /// connection per tick, as in an IOC. In a measured run, delivering the
    /// same 42 MB across eight connections instead of one cost 3.6 times
    /// as much CPU because each write carried only an eighth of the data.
    ///
    /// A nonzero value reduces that overhead at the cost of latency, which
    /// consumers measuring end-to-end time against the timestamp field will
    /// observe. The value is recorded in the run manifest.
    #[arg(long = "coalesce-us", default_value_t = 0)]
    coalesce_us: u64,

    /// Respond to PVAccess searches over UDP so clients can discover MIMUS
    /// as they would discover an IOC.
    ///
    /// Clients broadcast searches on port 5076 by default. Like a PVAccess
    /// server, MIMUS binds the port shared, so it can run beside an IOC on
    /// the same host; only the TCP port must differ (`--base-port`). Zero
    /// disables the responder.
    #[arg(long = "search-port", default_value_t = 5076)]
    search_port: u16,

    /// Address on which the search responder listens.
    ///
    /// Unlike `--bind`, which controls the TCP listeners, this address
    /// receives UDP broadcasts addressed to a subnet rather than a host.
    /// Binding to 127.0.0.1 prevents broadcast searches from reaching
    /// the responder, even if discovery appears enabled, and keeps the
    /// unicast searches sent to 127.0.0.1 from an IOC on the same host.
    #[arg(long = "search-bind", default_value = "0.0.0.0")]
    search_bind: String,

    /// Print the first outgoing message of each shape in hexadecimal.
    ///
    /// Rejected bytes are the evidence needed to diagnose why a client
    /// rejected a message.
    #[arg(long)]
    debug: bool,

    /// Stop after this many seconds. Runs until interrupted otherwise; Ctrl-C
    /// or SIGTERM ends a timed run early too, and the report is still written.
    #[arg(long = "duration-s", conflicts_with = "scenario")]
    duration_s: Option<f64>,

    /// Play a scenario: a TOML file of phases (steady, gain, burst, freeze,
    /// restart), played one after the other. The run ends with the last
    /// phase, and the report accounts for each phase apart.
    ///
    /// Consumers rarely fail in steady state. They fail when thousands of
    /// channels change at once, when an IOC restarts and everything it
    /// served reconnects together, or when the load climbs past what they
    /// absorb. `scenarios/` holds examples.
    #[arg(long)]
    scenario: Option<PathBuf>,

    /// Write the accounting report to this file on exit.
    #[arg(long = "report")]
    report: Option<PathBuf>,
}

#[derive(Parser)]
struct CalibrateArgs {
    /// Address of the reference IOC (host:port).
    #[arg(short, long, default_value = "127.0.0.1:5075")]
    addr: String,

    /// Channels to observe, one per line. Lines starting with `#` are comments.
    ///
    /// This is not the record list: QSRV2 exposes record fields as channels
    /// in their own right, and some PVA type codes are only accessible through those fields.
    #[arg(short, long, default_value = "catalogue/channels.txt")]
    channels: PathBuf,

    /// Profile to write. An existing file is replaced only once the new
    /// profile is complete and verified.
    #[arg(short, long, default_value = "profile.json")]
    out: PathBuf,

    /// Catalogue database to record in the manifest for provenance. Give it
    /// once for each database the reference IOC loaded.
    #[arg(long)]
    catalogue: Vec<PathBuf>,

    /// Number of distinct message bodies a changing channel must exhibit.
    ///
    /// Seven was insufficient in the capture this was designed around:
    /// the high byte of a double remained unchanged across seven updates
    /// because the value stayed within the same decade. Unchanging fields
    /// are frozen into the template, which is correct for the capture
    /// window but may not hold outside it.
    #[arg(long = "min-distinct", default_value_t = 30)]
    min_distinct: usize,

    /// Seconds without new observations before a channel is considered
    /// settled, such as a setpoint or deadbanded channel repeating one body.
    #[arg(long = "settled-after", default_value_t = 30.0)]
    settled_after: f64,

    /// Overall calibration deadline in seconds.
    #[arg(long = "deadline-s", default_value_t = 600.0)]
    deadline_s: f64,

    /// PVXS version of the reference IOC, recorded in the manifest.
    /// A profile is meaningful only in the context of the environment that produced it.
    #[arg(long = "pvxs-version", default_value = "unknown")]
    pvxs_version: String,

    /// EPICS Base version of the reference IOC.
    #[arg(long = "epics-version", default_value = "unknown")]
    epics_version: String,

    /// Log observations at each step, channel by channel.
    #[arg(long)]
    debug: bool,

    /// Write a profile even if calibration criteria are not met.
    ///
    /// Useful during development, but risky elsewhere: an incomplete profile
    /// can be served without errors while silently missing data that downstream consumers
    /// cannot detect. Missing criteria are reported, and the profile is marked
    /// incomplete: every command that loads it says so.
    #[arg(long = "allow-incomplete")]
    allow_incomplete: bool,
}

fn main() {
    let cli = Cli::parse();
    let code = match cli.command {
        Command::Serve(args) => run_serve(*args),
        Command::Calibrate(args) => calibrate::run(calibrate::Args {
            addr: args.addr,
            channels: args.channels,
            out: args.out,
            catalogue: args.catalogue,
            min_distinct: args.min_distinct,
            settled_after: args.settled_after,
            deadline_s: args.deadline_s,
            pvxs_version: args.pvxs_version,
            epics_version: args.epics_version,
            debug: args.debug,
            allow_incomplete: args.allow_incomplete,
        }),
        Command::Inspect { profile, diff } => run_inspect(&profile, diff.as_deref()),
        Command::DemoProfile { out } => demo::write(&out),
    };
    std::process::exit(code);
}

/// Parse a comma-separated list of `name` or `name:share` entries.
fn parse_mix(spec: &str) -> Result<Vec<(String, u32)>, String> {
    let mut out = Vec::new();
    for part in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let (name, share) = match part.split_once(':') {
            None => (part, 1u32),
            Some((n, s)) => (
                n,
                s.parse::<u32>()
                    .map_err(|_| format!("{s:?} is not a share"))?,
            ),
        };
        if name.is_empty() {
            return Err(format!("{part:?} has no template name"));
        }
        if out.iter().any(|(n, _)| n == name) {
            return Err(format!("{name:?} is named twice"));
        }
        out.push((name.to_string(), share));
    }
    if out.is_empty() {
        return Err("no templates given".into());
    }
    Ok(out)
}

/// Checks that a file can be written, before the work whose result it holds.
///
/// Creates the file if it is missing and removes it again, so a run that
/// fails later leaves nothing behind.
pub(crate) fn check_writable(path: &std::path::Path) -> Result<(), String> {
    let existed = path.exists();
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    if !existed {
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

fn load(path: &std::path::Path) -> Result<Profile, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    Profile::from_json(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// A hand-crafted profile cannot support measurement claims.
/// Serving it is fine, but its synthetic status must be explicit.
fn warn_if_synthetic(profile: &Profile) {
    if profile.manifest.synthetic {
        eprintln!("┌───────────────────────────────────────────────────────────┐");
        eprintln!("│ SYNTHETIC PROFILE — not calibrated against a real IOC.    │");
        eprintln!("│ Useful for trying the server out. Nothing produced from   │");
        eprintln!("│ this run is a measurement of anything.                    │");
        eprintln!("└───────────────────────────────────────────────────────────┘");
    }
}

/// A profile written with `--allow-incomplete` before its criterion was met
/// serves, but cannot support measurement claims either.
fn warn_if_incomplete(profile: &Profile) {
    if profile.manifest.incomplete {
        eprintln!("┌───────────────────────────────────────────────────────────┐");
        eprintln!("│ INCOMPLETE PROFILE — written with --allow-incomplete      │");
        eprintln!("│ before every channel was observed long enough. Nothing    │");
        eprintln!("│ produced from this run is a measurement of anything.      │");
        eprintln!("└───────────────────────────────────────────────────────────┘");
    }
}

/// Templates derived from few updates are not invalid, but measurements
/// based on them warrant a caveat.
fn warn_if_weakly_supported(profile: &Profile) {
    let weak = profile.weakly_supported(mimus_profile::DEFAULT_MIN_OBSERVATIONS);
    if weak.is_empty() {
        return;
    }
    eprintln!(
        "note: {} template(s) were distilled from fewer than {} updates. A field \
         that did not move during calibration is frozen into the template, which \
         is right for that window and wrong outside it:",
        weak.len(),
        mimus_profile::DEFAULT_MIN_OBSERVATIONS
    );
    for (name, n) in weak.iter().take(8) {
        eprintln!("  {name:<28} {n} update(s)");
    }
}

fn run_serve(args: ServeArgs) -> i32 {
    let profile = match load(&args.profile) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    warn_if_synthetic(&profile);
    warn_if_incomplete(&profile);

    let rate_mix = match &args.rate_mix {
        None => RateMix::uniform(args.rate_hz),
        Some(spec) => match RateMix::parse(spec) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("error: --rate-mix: {e}");
                return 1;
            }
        },
    };

    // Every session builds this schedule when a client connects. Checking it
    // here turns a rate or a tick it cannot use into an error at start-up,
    // instead of connections that close without a word.
    if let Err(e) = Schedule::new(&rate_mix, args.tick_us) {
        eprintln!("error: --rate-hz / --rate-mix / --tick-us: {e}");
        return 1;
    }
    if let Some(n) = args.alarms
        && !(n.is_finite() && n > 0.0)
    {
        eprintln!("error: --alarms: {n} is not a positive number of transitions per hour");
        return 1;
    }
    let activity = match args.activity.as_deref().map(Activity::parse) {
        None => None,
        Some(Ok(a)) => Some(a),
        Some(Err(e)) => {
            eprintln!("error: --activity: {e}");
            return 1;
        }
    };
    if let Some(s) = args.drop_connections
        && !(s > 0.0 && std::time::Duration::try_from_secs_f64(s).is_ok())
    {
        eprintln!("error: --drop-connections: {s} is not a positive number of seconds");
        return 1;
    }
    if let Some(s) = args.duration_s
        && !(s > 0.0 && std::time::Duration::try_from_secs_f64(s).is_ok())
    {
        eprintln!("error: --duration-s: {s} is not a positive number of seconds");
        return 1;
    }
    let scenario = match &args.scenario {
        None => None,
        Some(path) => match scenario::load(path).and_then(|s| s.check(args.iocs).map(|()| s)) {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!("error: --scenario: {e}");
                return 1;
            }
        },
    };
    // A scenario that raises the rate builds every schedule at its highest
    // factor; that schedule has to be one the wheel can run.
    let headroom = scenario
        .as_ref()
        .map_or(1.0, mimus_serve::Scenario::headroom);
    if headroom > 1.0
        && let Err(e) = Schedule::new(&rate_mix.scaled(headroom), args.tick_us)
    {
        eprintln!(
            "error: --scenario: its highest gain, ×{headroom}, takes the rates {rate_mix} Hz \
             past what --tick-us {} can schedule: {e}",
            args.tick_us
        );
        return 1;
    }
    // The report is written at the end of the run. A path that cannot be
    // written would lose the run's accounting after it has been paid for.
    if let Some(path) = &args.report
        && let Err(e) = check_writable(path)
    {
        eprintln!("error: --report: {e}");
        return 1;
    }
    if args.base_port != 0
        && u32::from(args.base_port) + u32::from(args.iocs).saturating_sub(1) > 65535
    {
        eprintln!(
            "error: --base-port {} with --iocs {} runs past port 65535",
            args.base_port, args.iocs
        );
        return 1;
    }

    if args.prefix.is_empty() {
        eprintln!(
            "error: --prefix is empty, which would start every name with a colon; \
             use --name-template '{{t}}:{{i}}' for names without a prefix"
        );
        return 1;
    }
    let names = match &args.name_template {
        None => NameTemplate::default_pattern(&args.prefix),
        Some(pattern) => match NameTemplate::new(pattern) {
            Ok(n) => n,
            Err(e) => {
                eprintln!("error: --name-template: {e}");
                return 1;
            }
        },
    };

    let mut spec = FleetSpec::uniform(&profile, args.iocs, args.pvs);
    spec.layout = args.layout;
    spec.rate_mix = rate_mix.clone();
    spec.names = names;
    if let Some(mix) = &args.mix {
        spec.mix = match parse_mix(mix) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("error: --mix: {e}");
                return 1;
            }
        };
    }

    let fleet = match Fleet::build(&profile, &spec) {
        Ok(f) => Arc::new(f),
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    // Opening messages retain their calibration timestamps unless their values
    // can be traversed. Warn explicitly so clients do not unknowingly store
    // stale initial samples.
    for (name, why) in fleet.unstamped_openings() {
        eprintln!("warning: {name}: opening message sent with its recorded timestamp: {why}");
    }
    // Channels that can carry an alarm transition: those whose type has one.
    let alarm_channels = fleet
        .instances
        .iter()
        .filter(|i| fleet.templates[usize::from(i.template)].alarm.is_some())
        .count() as u64;
    if args.alarms.is_some() && alarm_channels == 0 {
        eprintln!(
            "error: --alarms: no template served carries an alarm transition. A profile \
             learns them from the catalogue's MIM:ALM channels; recalibrate with this \
             version of MIMUS, and check that --mix keeps a type that has one \
             (`mimus inspect` lists them)"
        );
        return 1;
    }

    let cfg = SessionConfig {
        buffer_size: profile.session_rules.server_buffer_size,
        registry_size: profile.session_rules.server_registry_size,
        descriptor_policy: if args.always_full {
            DescriptorPolicy::AlwaysFull
        } else {
            profile.session_rules.descriptor_policy
        },
        backpressure: args.backpressure,
        rate_mix: rate_mix.clone(),
        tick_us: args.tick_us,
        high_water: args.high_water,
        dropper: args.drop_connections.map(|_| Dropper::new()),
        coalesce_us: args.coalesce_us,
        debug: args.debug,
        pipeline: args.pipeline,
        stages: scenario
            .as_ref()
            .map(|_| mimus_serve::Stages::new(args.iocs)),
        headroom,
        activity,
        alarms: args.alarms,
        ..Default::default()
    };

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: cannot start the runtime: {e}");
            return 1;
        }
    };

    let mix_label = spec
        .mix
        .iter()
        .map(|(n, w)| format!("{n}:{w}"))
        .collect::<Vec<_>>()
        .join(",");

    // The load the fleet really offers: each channel at its own group's rate.
    // The mix's proportions hold only over whole cycles of its shares, so for
    // a small population the ideal figure can be far off: three channels of
    // `10:2,1:60,0.1:38` run at 10, 10 and 1 Hz, 21 updates/s, not 2.5.
    // A channel of a static template sends its opening message only, and
    // under --activity a channel sends while it is active only.
    let active = activity.map_or(1.0, |a| a.fraction);
    let offered_eps: f64 = fleet
        .instances
        .iter()
        .filter(|i| !fleet.templates[usize::from(i.template)].is_static())
        .map(|i| rate_mix.groups[usize::from(i.rate_group)].0)
        .fold(0.0, |sum, rate| sum + rate)
        * active;
    let summary = report::RunSummary {
        mix: mix_label,
        iocs: args.iocs,
        pvs: args.pvs,
        rate_mix: rate_mix.to_string(),
        offered_eps,
        name_template: spec.names.pattern().to_string(),
        drop_connections_s: args.drop_connections,
        pipeline: args.pipeline.to_string(),
        coalesce_us: args.coalesce_us,
        layout: args.layout.to_string(),
        backpressure: args.backpressure.to_string(),
        tick_us: args.tick_us,
        activity,
        alarms: args.alarms.map(|n| (n, alarm_channels)),
        scenario: scenario.as_ref().map(|s| report::ScenarioSummary {
            name: s.name.clone(),
            file: args.scenario.clone().unwrap_or_default(),
            repeat: s.repeat,
            length_s: s.length().as_secs_f64(),
            headroom,
            plan: s
                .phases
                .iter()
                .scan(std::time::Duration::ZERO, |at, p| {
                    let start = *at;
                    *at += p.duration;
                    Some((
                        start.as_secs_f64(),
                        p.label.clone(),
                        p.duration.as_secs_f64(),
                    ))
                })
                .collect(),
            waits_for: match s.start {
                mimus_serve::scenario::Start::Now => None,
                mimus_serve::scenario::Start::Subscribed { count, timeout } => Some((
                    match count {
                        mimus_serve::scenario::Count::All => u64::from(args.pvs),
                        mimus_serve::scenario::Count::Minimum(n) => n,
                    },
                    timeout.as_secs_f64(),
                )),
            },
        }),
    };
    // Each IOC's share of the offered load, for the phases that act on some
    // IOCs only.
    let mut offered_by_ioc = vec![0.0f64; usize::from(args.iocs)];
    for i in fleet
        .instances
        .iter()
        .filter(|i| !fleet.templates[usize::from(i.template)].is_static())
    {
        if let Some(slot) = offered_by_ioc.get_mut(usize::from(i.ioc)) {
            *slot += rate_mix.groups[usize::from(i.rate_group)].0 * active;
        }
    }

    runtime.block_on(async move {
        let mut totals = Vec::new();
        let mut addrs = Vec::new();

        for i in 0..args.iocs {
            let port = if args.base_port == 0 {
                0
            } else {
                args.base_port + i
            };
            let bind = format!("{}:{}", args.bind, port);
            let ioc = match Ioc::bind(i, &bind, cfg.clone(), Arc::clone(&fleet)).await {
                Ok(ioc) => ioc,
                Err(e) => {
                    eprintln!("error: cannot bind {bind}: {e}");
                    return 1;
                }
            };
            addrs.push(ioc.addr);
            totals.push(ioc.totals());
            tokio::spawn(ioc.serve());
        }

        // Start discovery once all IOCs are listening and their ports are known.
        //
        // Without discovery, clients must be told where to connect. A consumer
        // whose configuration had to be changed for the benchmark is not quite
        // the same consumer that would connect to a real IOC.
        let mut discovery = None;
        if args.search_port != 0 {
            // Discovery must advertise addresses that the listeners can reach.
            //
            // A responder bound to every interface advertises the host's
            // reachable address. If the listeners are restricted to loopback,
            // remote clients are directed to an address where nothing is
            // listening and receive a connection refusal. Being undiscoverable
            // is a limitation; advertising an unreachable endpoint is misleading.
            if args.bind == "127.0.0.1" && args.search_bind == "0.0.0.0" {
                eprintln!(
                    "warning: searches are answered on every interface but the \
                     listeners are on 127.0.0.1, so a client that broadcasts, or \
                     one elsewhere, will be told to connect where nothing is \
                     listening. Use --bind 0.0.0.0 for a reachable server, or \
                     --search-port 0 with EPICS_PVA_NAME_SERVERS to stay off UDP."
                );
            }
            let endpoints: Vec<mimus_serve::IocEndpoint> = addrs
                .iter()
                .enumerate()
                .map(|(i, a)| mimus_serve::IocEndpoint {
                    index: i as u16,
                    tcp_port: a.port(),
                    guid: mimus_serve::listener::guid_for(i as u16),
                })
                .collect();
            match mimus_serve::SearchResponder::bind(
                &args.search_bind,
                args.search_port,
                Arc::clone(&fleet),
                endpoints,
            )
            .await
            {
                Ok(r) => {
                    let r = Arc::new(match &cfg.stages {
                        Some(stages) => r.with_stages(Arc::clone(stages)),
                        None => r,
                    });
                    discovery = Some(Arc::clone(&r));
                    tokio::spawn(async move { r.serve().await });
                }
                Err(e) => {
                    // Serving remains available to clients configured with
                    // explicit endpoints. Warn so clients relying on discovery
                    // are not left without an explanation.
                    eprintln!(
                        "warning: cannot answer searches on {}:{} ({e}). Clients will \
                         need EPICS_PVA_NAME_SERVERS. Another PVAccess program may \
                         already hold the port; --search-port 0 turns this off.",
                        args.search_bind, args.search_port
                    );
                }
            }
        }

        // Drop one live connection per interval, whichever session claims the
        // request first. Nothing is requested while no client is connected, so
        // a consumer that connects late is not dropped on arrival.
        if let (Some(dropper), Some(every)) = (cfg.dropper.clone(), args.drop_connections) {
            let totals = totals.clone();
            tokio::spawn(async move {
                let mut timer = tokio::time::interval(std::time::Duration::from_secs_f64(every));
                timer.tick().await;
                loop {
                    timer.tick().await;
                    if totals.iter().any(|t| t.sessions_open() > 0) {
                        dropper.request();
                    }
                }
            });
        }

        let search_port = if discovery.is_some() {
            args.search_port
        } else {
            0
        };
        report::print_banner(&profile, &fleet, &summary, &addrs, search_port);

        let started = std::time::Instant::now();
        // Ctrl-C ends a run early, timed or not, and the report is still
        // written: an interrupted timed run keeps the accounting of what it
        // did send.
        let interrupt = async {
            if tokio::signal::ctrl_c().await.is_err() {
                eprintln!(
                    "warning: cannot listen for interrupts; Ctrl-C will end the run \
                     without a report"
                );
                std::future::pending::<()>().await;
            }
        };
        // SIGTERM, what `kill`, systemd and container runtimes send, ends a
        // run the same way.
        let terminate = async {
            #[cfg(unix)]
            {
                use tokio::signal::unix::{SignalKind, signal};
                match signal(SignalKind::terminate()) {
                    Ok(mut term) => {
                        term.recv().await;
                    }
                    Err(_) => std::future::pending::<()>().await,
                }
            }
            #[cfg(not(unix))]
            std::future::pending::<()>().await;
        };
        let deadline = async {
            match args.duration_s {
                Some(s) => tokio::time::sleep(std::time::Duration::from_secs_f64(s)).await,
                None => std::future::pending().await,
            }
        };
        // The scenario, if any, is played against the IOCs as they are now
        // listening, and the run ends with its last phase.
        let target = cfg.stages.as_ref().map(|stages| mimus_serve::Target {
            stages: Arc::clone(stages),
            totals: totals.clone(),
            offered_eps: offered_by_ioc.clone(),
            channels: u64::from(args.pvs),
        });
        let record = mimus_serve::scenario::Record::new();
        let played = async {
            match (&scenario, &target) {
                (Some(s), Some(t)) => mimus_serve::scenario::play(s, t, &record).await,
                _ => std::future::pending().await,
            }
        };
        tokio::select! {
            () = interrupt => {}
            () = terminate => {}
            () = deadline => {}
            () = played => {}
        }
        let played = target.as_ref().map(|t| record.finish(t));

        // Snapshot counters while sessions are still open, rather than relying
        // on end-of-session totals.
        let mut merged = Counters::default();
        let mut sessions = report::Sessions::default();
        for t in &totals {
            merged.merge(&t.snapshot());
            sessions.total += t.sessions_total();
            sessions.dropped += t.sessions_dropped();
            sessions.closed_down += t.sessions_closed_down();
            sessions.subscriptions += t.subscriptions();
        }

        report::finish(
            &profile,
            &summary,
            merged,
            sessions,
            played.as_ref(),
            started.elapsed().as_secs_f64(),
            args.report.as_deref(),
        );
        0
    })
}

/// Whether two templates have the same shape: the same fixed bytes, and the
/// same slots in the same places with the same widths and roles.
///
/// The bytes a slot replays are left out. They are values the reference IOC
/// happened to send during calibration, so two calibrations of one IOC do not
/// record the same ones, and comparing them would report every template as
/// changed.
fn same_shape(a: &mimus_profile::Template, b: &mimus_profile::Template) -> bool {
    a.chunks == b.chunks
        && a.slots.len() == b.slots.len()
        && a.slots
            .iter()
            .zip(&b.slots)
            .all(|(x, y)| x.id == y.id && x.width == y.width && x.role == y.role)
}

fn run_inspect(path: &std::path::Path, diff: Option<&std::path::Path>) -> i32 {
    let profile = match load(path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    warn_if_synthetic(&profile);
    warn_if_incomplete(&profile);
    warn_if_weakly_supported(&profile);

    out!("{}", profile.manifest.summary());
    out!("{}", profile.coverage_summary());
    out!();
    out!("{:<24} {:>6} {:>6}  shape", "template", "PVs", "obs");
    for (name, pvs) in profile.coverage() {
        let t = profile.template(name).expect("listed");
        let shape = if t.is_fixed_width() {
            format!("{} B fixed", t.max_len())
        } else {
            format!("{}..{} B variable", t.min_len(), t.max_len())
        };
        out!("{name:<24} {pvs:>6} {:>6}  {shape}", t.observations);
        for v in &t.variants {
            let changes = if v.changes.is_empty() {
                String::new()
            } else {
                format!(", changes {}", v.changes.join(", "))
            };
            out!(
                "  + {:<20} {:>6} {:>6}  variant{changes}",
                v.name.strip_prefix(name).unwrap_or(&v.name),
                "",
                v.observations
            );
        }
    }

    if let Some(share) = profile.session_rules.dominant_share() {
        out!();
        out!(
            "dominant changed-set covers {:.1}% of observed updates",
            share * 100.0
        );
        let alarms = profile
            .templates
            .iter()
            .filter(|t| t.alarm_variant().is_some())
            .count();
        if alarms > 0 {
            out!(
                "{alarms} template(s) carry an alarm transition, which `serve --alarms` \
                 sends in place of an ordinary update"
            );
        } else if share < 0.95 {
            out!(
                "  note: a large minority of updates use other patterns; emitting only \
                 the dominant one would leave decode paths untested"
            );
        }
    }

    let Some(other_path) = diff else {
        return 0;
    };
    let other = match load(other_path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };

    out!();
    let same_env = profile.manifest.same_environment(&other.manifest);
    if same_env {
        out!("same reference environment");
    } else if profile.manifest.environment_known() && other.manifest.environment_known() {
        out!("different reference environment: differences are wire-behavior changes");
        out!("  a: {}", profile.manifest.summary());
        out!("  b: {}", other.manifest.summary());
    } else {
        out!("reference environment not fully recorded, so differences are reported, not judged");
        out!("  a: {}", profile.manifest.summary());
        out!("  b: {}", other.manifest.summary());
    }

    // Types are compared strictly: the same IOC and catalogue must yield the
    // same set of types with the same descriptors. Shapes are only reported.
    // Which bytes of a message are fixed depends on what moved while it was
    // observed, so two calibrations of one IOC can draw different lines in a
    // value that held still in one window and not in the other.
    let mut type_differences = 0;
    let mut shape_differences = 0;
    for t in &profile.templates {
        match other.template(&t.name) {
            None => {
                out!("  - {} present in a, absent from b", t.name);
                type_differences += 1;
            }
            Some(o) if t.introspection != o.introspection => {
                out!("  ~ {} differs in its type descriptor", t.name);
                type_differences += 1;
            }
            Some(o) if !same_shape(t, o) => {
                out!("  ~ {} differs in shape", t.name);
                shape_differences += 1;
            }
            Some(_) => {}
        }
    }
    for t in &other.templates {
        if profile.template(&t.name).is_none() {
            out!("  + {} present in b, absent from a", t.name);
            type_differences += 1;
        }
    }
    if type_differences + shape_differences == 0 {
        out!("  no differences");
        return 0;
    }
    if shape_differences > 0 {
        out!(
            "  {shape_differences} shape difference(s): which bytes are fixed depends on \
             what moved while each profile was observed"
        );
    }
    if type_differences == 0 {
        return 0;
    }
    // A different set of types from the same environment is a defect. Exit
    // nonzero so CI can detect it without parsing the output.
    if same_env {
        out!("  the same environment produced different types: this is a MIMUS bug");
    }
    i32::from(same_env)
}

#[cfg(test)]
mod shape_tests {
    use super::same_shape;
    use mimus_profile::{Slot, SlotRole, SlotWidth, Template};

    fn template(samples: Vec<Vec<u8>>, width: usize) -> Template {
        let mut t = Template::new("dbl", 0x0D);
        t.push_fixed(vec![0x03, 0x83, 0x01]).push_slot(
            Slot::new(1, SlotWidth::Fixed { bytes: width })
                .with_role(SlotRole::Unclassified)
                .with_samples(samples),
        );
        t
    }

    #[test]
    fn replayed_values_are_not_part_of_the_shape() {
        // Two calibrations of one IOC record different values; that alone
        // must not read as a change.
        let a = template(vec![vec![1, 2], vec![3, 4]], 2);
        let b = template(vec![vec![5, 6]], 2);
        assert!(same_shape(&a, &b));
    }

    #[test]
    fn a_different_slot_width_is_a_different_shape() {
        let a = template(vec![vec![1, 2]], 2);
        let b = template(vec![vec![1, 2, 3, 4]], 4);
        assert!(!same_shape(&a, &b));
    }
}

#[cfg(test)]
mod mix_tests {
    use super::parse_mix;

    #[test]
    fn a_bare_name_is_an_equal_share() {
        assert_eq!(parse_mix("double").unwrap(), vec![("double".into(), 1)]);
    }

    #[test]
    fn shares_are_parsed_and_kept_in_order() {
        assert_eq!(
            parse_mix("double:3,int:1").unwrap(),
            vec![("double".into(), 3), ("int".into(), 1)]
        );
    }

    #[test]
    fn whitespace_and_trailing_commas_are_tolerated() {
        assert_eq!(
            parse_mix(" double , int , ").unwrap(),
            vec![("double".into(), 1), ("int".into(), 1)]
        );
    }

    #[test]
    fn nonsense_is_refused_rather_than_guessed() {
        // Serving a different mix than requested would silently invalidate the run.
        assert!(parse_mix("").is_err());
        assert!(parse_mix("double:x").is_err());
        assert!(parse_mix(":3").is_err());
    }

    #[test]
    fn a_template_named_twice_is_refused() {
        // Two entries for one template would serve it under two lines of
        // the banner and the manifest, with neither share being the one meant.
        assert!(parse_mix("double:1,int:1,double:2").is_err());
    }
}
