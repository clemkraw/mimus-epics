//! The `mimus` command line, run as a user runs it.
//!
//! The library crates test their parts in isolation. These tests run the
//! binary itself: the option checks, the banner, the report, and a whole
//! calibration, with MIMUS serving the demo profile as the reference server.
//! Calibrating MIMUS against itself exercises the calibration client end to
//! end without an EPICS installation, and must give back the types it served.
#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const MIMUS: &str = env!("CARGO_BIN_EXE_mimus");

/// A directory of its own for each test, removed when the test ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("mimus-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self, file: &str) -> PathBuf {
        self.0.join(file)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn mimus(args: &[&str]) -> Output {
    Command::new(MIMUS).args(args).output().unwrap()
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn demo_profile(dir: &Scratch) -> PathBuf {
    let p = dir.path("demo.json");
    let o = mimus(&["demo-profile", "-o", p.to_str().unwrap()]);
    assert!(o.status.success(), "{}", text(&o));
    p
}

/// A running `mimus serve`, with the address of each of its IOCs as the
/// banner gives them.
struct Server {
    child: Option<Child>,
    addrs: Vec<String>,
}

impl Server {
    fn start(args: &[&str]) -> Self {
        let mut child = Command::new(MIMUS)
            .arg("serve")
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut addrs = Vec::new();
        for line in lines.by_ref() {
            let line = line.unwrap();
            if let Some(rest) = line.trim_start().strip_prefix("ioc ") {
                addrs.push(rest.split_whitespace().nth(1).unwrap().to_string());
            } else if !addrs.is_empty() && line.trim().is_empty() {
                break;
            }
        }
        assert!(!addrs.is_empty(), "the banner listed no listener");
        // Keep reading so that MIMUS never blocks on a full pipe.
        std::thread::spawn(move || for _ in lines {});
        Self {
            child: Some(child),
            addrs,
        }
    }

    /// Hands the process over, for a test that ends it itself.
    fn into_child(mut self) -> Child {
        self.child.take().unwrap()
    }

    fn interrupt(self) -> std::process::ExitStatus {
        let mut child = self.into_child();
        signal(&child, "INT");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "MIMUS did not stop on Ctrl-C");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Sends a signal through the shell's own `kill`, which every image with a
/// shell has, unlike a `kill` program.
fn signal(child: &Child, name: &str) {
    let script = format!("kill -{name} {}", child.id());
    assert!(
        Command::new("sh")
            .args(["-c", &script])
            .status()
            .unwrap()
            .success()
    );
}

fn report(path: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn calibrating_mimus_against_itself_gives_back_the_types_it_served() {
    let dir = Scratch::new("self");
    let demo = demo_profile(&dir);
    let served_report = dir.path("served.json");
    let server = Server::start(&[
        "-p",
        demo.to_str().unwrap(),
        "--pvs",
        "6",
        "--rate-hz",
        "50",
        "--base-port",
        "0",
        "--search-port",
        "0",
        "--report",
        served_report.to_str().unwrap(),
    ]);

    // Six channels, two of each of the demo's three types.
    let channels = dir.path("channels.txt");
    std::fs::write(
        &channels,
        "MIM:double:0\nMIM:int:1\nMIM:string:2\nMIM:double:3\nMIM:int:4\nMIM:string:5\n",
    )
    .unwrap();
    // A criterion the deadline cannot meet: no profile, and a status of its own.
    let partial = dir.path("partial.json");
    let o = mimus(&[
        "calibrate",
        "-a",
        &server.addrs[0],
        "-c",
        channels.to_str().unwrap(),
        "-o",
        partial.to_str().unwrap(),
        "--min-distinct",
        "100000",
        "--settled-after",
        "100",
        "--deadline-s",
        "1",
    ]);
    assert_eq!(o.status.code(), Some(3), "{}", text(&o));
    assert!(!partial.exists());

    // The same with --allow-incomplete: a profile, marked as what it is.
    let o = mimus(&[
        "calibrate",
        "-a",
        &server.addrs[0],
        "-c",
        channels.to_str().unwrap(),
        "-o",
        partial.to_str().unwrap(),
        "--min-distinct",
        "100000",
        "--settled-after",
        "100",
        "--deadline-s",
        "1",
        "--allow-incomplete",
    ]);
    assert!(o.status.success(), "{}", text(&o));
    assert_eq!(report(&partial)["manifest"]["incomplete"], true);
    let o = mimus(&["inspect", partial.to_str().unwrap()]);
    assert!(text(&o).contains("INCOMPLETE PROFILE"), "{}", text(&o));

    let learned = dir.path("learned.json");
    let o = mimus(&[
        "calibrate",
        "-a",
        &server.addrs[0],
        "-c",
        channels.to_str().unwrap(),
        "-o",
        learned.to_str().unwrap(),
        "--min-distinct",
        "8",
        "--settled-after",
        "3",
        "--deadline-s",
        "30",
    ]);
    let out = text(&o);
    assert!(o.status.success(), "{out}");
    assert!(
        out.contains("6 PVs observed -> 3 distinct templates"),
        "{out}"
    );

    let profile = report(&learned);
    let mut names: Vec<&str> = profile["templates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["ntscalar_f64", "ntscalar_i32", "ntscalar_string"]);
    // The demo profile sends a type's descriptor once per connection, then
    // its identifier, and calibration must have seen that.
    assert_eq!(
        profile["session_rules"]["descriptor_policy"],
        "full_then_id"
    );
    assert_eq!(profile["manifest"]["pvs_observed"], 6);

    // The learned profile serves too.
    let o = mimus(&["inspect", learned.to_str().unwrap()]);
    assert!(o.status.success(), "{}", text(&o));

    assert!(server.interrupt().success());
    let r = report(&served_report);
    assert!(r["emitted"].as_u64().unwrap() > 0);
    assert_eq!(
        r["subscriptions"], 0,
        "every monitor ended with calibration"
    );
    assert_eq!(r["profile"]["synthetic"], true);
}

#[test]
fn an_interrupted_run_still_writes_its_report_when_stdout_is_gone() {
    // `mimus serve | tee log` interrupted with Ctrl-C: tee goes first, and
    // MIMUS prints its summary into a closed pipe on its way out.
    let dir = Scratch::new("pipe");
    let demo = demo_profile(&dir);
    let path = dir.path("report.json");
    let mut child = Command::new(MIMUS)
        .args(["serve", "-p", demo.to_str().unwrap(), "--base-port", "0"])
        .args(["--search-port", "0", "--report", path.to_str().unwrap()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut first = String::new();
    BufReader::new(child.stdout.as_mut().unwrap())
        .read_line(&mut first)
        .unwrap();
    drop(child.stdout.take());
    std::thread::sleep(Duration::from_millis(300));
    signal(&child, "INT");
    let out = child.wait_with_output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{err}");
    assert!(!err.contains("panicked"), "{err}");
    assert!(report(&path)["run"]["pvs"].as_u64().is_some());
}

#[test]
fn sigterm_ends_a_run_with_its_report() {
    // What `kill`, systemd and container runtimes send.
    let dir = Scratch::new("term");
    let demo = demo_profile(&dir);
    let path = dir.path("report.json");
    let server = Server::start(&[
        "-p",
        demo.to_str().unwrap(),
        "--base-port",
        "0",
        "--search-port",
        "0",
        "--report",
        path.to_str().unwrap(),
    ]);
    let mut child = server.into_child();
    signal(&child, "TERM");
    assert!(child.wait().unwrap().success());
    assert!(report(&path)["emitted"].as_u64().is_some());
}

#[test]
fn a_timed_run_reports_what_it_offered() {
    let dir = Scratch::new("timed");
    let demo = demo_profile(&dir);
    let path = dir.path("report.json");
    let o = mimus(&[
        "serve",
        "-p",
        demo.to_str().unwrap(),
        "--pvs",
        "3",
        "--rate-mix",
        "10:2,1:60,0.1:38",
        "--base-port",
        "0",
        "--search-port",
        "0",
        "--duration-s",
        "0.5",
        "--report",
        path.to_str().unwrap(),
    ]);
    let out = text(&o);
    assert!(o.status.success(), "{out}");
    assert!(
        out.contains("SYNTHETIC PROFILE"),
        "a demo profile says so: {out}"
    );
    let r = report(&path);
    // Three channels of that mix are given 10, 10 and 1 Hz: the offered load
    // is what they were given, not the mix's long-run average.
    assert_eq!(r["offered_eps"], 21.0);
    assert_eq!(r["emitted"], 0, "nothing subscribed");
    assert_eq!(r["run"]["rate_mix"], "10:2,1:60,0.1:38");
}

#[test]
fn options_that_cannot_work_are_refused_before_serving() {
    let dir = Scratch::new("refuse");
    let demo = demo_profile(&dir);
    let p = demo.to_str().unwrap();
    let missing_dir = dir.path("no/such/dir/report.json");
    let cases: &[(&[&str], &str)] = &[
        (&["--duration-s=-1"], "--duration-s"),
        (&["--duration-s", "nan"], "--duration-s"),
        (&["--tick-us", "0"], "tick resolution"),
        (&["--tick-us", "2000000"], "tick resolution"),
        (&["--rate-hz", "0"], "rate"),
        (&["--rate-hz", "0.0000001"], "coarser tick"),
        (&["--rate-hz", "1000000000"], "finer tick"),
        (&["--mix", "nope"], "nope"),
        (&["--mix", "double:1,double:2"], "named twice"),
        (&["--prefix", ""], "--prefix"),
        (&["--drop-connections", "0"], "--drop-connections"),
        (&["--name-template", "X:{t}"], "distinctly"),
        (&["--report", missing_dir.to_str().unwrap()], "--report"),
        (&["--activity", "0.9:2"], "--activity"),
        (&["--activity", "0"], "--activity"),
        (&["--alarms", "0"], "--alarms"),
        (
            &["--alarms", "2"],
            "no template served carries an alarm transition",
        ),
    ];
    for (extra, expect) in cases {
        let mut args = vec!["serve", "-p", p, "--search-port", "0", "--base-port", "0"];
        args.extend_from_slice(extra);
        let o = mimus(&args);
        let out = text(&o);
        assert_eq!(o.status.code(), Some(1), "{extra:?} was accepted: {out}");
        assert!(
            out.contains(expect),
            "{extra:?}: expected {expect:?} in {out}"
        );
        assert!(
            !out.contains("listening"),
            "{extra:?} got as far as serving"
        );
    }
    let o = mimus(&["serve", "-p", p, "--base-port", "65535", "--iocs", "2"]);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o));
    assert!(text(&o).contains("65535"), "{}", text(&o));
    let o = mimus(&[
        "serve",
        "-p",
        p,
        "--prefix",
        "A",
        "--name-template",
        "X:{i}",
    ]);
    assert_eq!(o.status.code(), Some(2), "{}", text(&o));
}

#[test]
fn calibrate_refuses_what_would_only_fail_at_the_end() {
    let dir = Scratch::new("calibrate");
    let channels = dir.path("channels.txt");
    std::fs::write(&channels, "MIM:F64:0\nMIM:F64:1\n").unwrap();
    let dup = dir.path("dup.txt");
    std::fs::write(&dup, "MIM:F64:0\nMIM:F64:0\n").unwrap();
    let c = channels.to_str().unwrap();
    let cases: &[(&[&str], &str)] = &[
        (
            &["-c", dup.to_str().unwrap()],
            "listed on line 1 and again on line 2",
        ),
        (&["-c", c, "--min-distinct", "1"], "--min-distinct"),
        (&["-c", c, "--settled-after", "0"], "--settled-after"),
        (&["-c", c, "--deadline-s=-1"], "--deadline-s"),
        (&["-c", c, "--catalogue", "/no/such.db"], "--catalogue"),
        (&["-c", c, "-o", "/no/such/dir/p.json"], "--out"),
    ];
    for (extra, expect) in cases {
        // Port 9 on the loopback: nothing listens, and nothing should be
        // tried before the options are checked.
        let mut args = vec!["calibrate", "-a", "127.0.0.1:9"];
        args.extend_from_slice(extra);
        let o = mimus(&args);
        let out = text(&o);
        assert_eq!(o.status.code(), Some(1), "{extra:?}: {out}");
        assert!(
            out.contains(expect),
            "{extra:?}: expected {expect:?} in {out}"
        );
        assert!(!out.contains("calibrating against"), "{extra:?} connected");
    }
}

#[test]
fn inspect_compares_and_refuses_what_is_not_a_profile() {
    let dir = Scratch::new("inspect");
    let demo = demo_profile(&dir);
    let p = demo.to_str().unwrap();
    let o = mimus(&["inspect", p]);
    let out = text(&o);
    assert!(o.status.success(), "{out}");
    assert!(
        out.contains("PVs assigned -> 3 distinct templates"),
        "{out}"
    );
    let o = mimus(&["inspect", p, "--diff", p]);
    assert!(o.status.success(), "{}", text(&o));

    let junk = dir.path("junk.json");
    std::fs::write(&junk, "{\"not\": \"a profile\"}").unwrap();
    for bad in [junk.to_str().unwrap(), "/no/such/profile.json"] {
        let o = mimus(&["inspect", bad]);
        assert_eq!(o.status.code(), Some(1), "{}", text(&o));
    }
}

#[test]
fn a_scenario_ends_the_run_and_is_accounted_phase_by_phase() {
    let dir = Scratch::new("scenario");
    let demo = demo_profile(&dir);
    let scenario = dir.path("s.toml");
    std::fs::write(
        &scenario,
        r#"
name = "short"

[[phase]]
kind = "steady"
duration = "300ms"
label = "warm"

[[phase]]
kind = "burst"
share = 0.5

[[phase]]
kind = "freeze"
duration = "200ms"
iocs = [0]

[[phase]]
kind = "gain"
duration = "300ms"
gain = 2

[[phase]]
kind = "restart"
iocs = [1]
down = "200ms"
duration = "300ms"
"#,
    )
    .unwrap();
    let path = dir.path("report.json");
    let started = Instant::now();
    let o = mimus(&[
        "serve",
        "-p",
        demo.to_str().unwrap(),
        "--iocs",
        "2",
        "--pvs",
        "4",
        "--base-port",
        "0",
        "--search-port",
        "0",
        "--scenario",
        scenario.to_str().unwrap(),
        "--report",
        path.to_str().unwrap(),
    ]);
    let out = text(&o);
    assert!(o.status.success(), "{out}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the run ends with its scenario"
    );
    assert!(
        out.contains("scenario short"),
        "the banner announces it: {out}"
    );
    assert!(out.contains("schedules built for ×2"), "{out}");

    let r = report(&path);
    let s = &r["scenario"];
    assert_eq!(s["name"], "short");
    assert_eq!(s["headroom"], 2.0);
    let phases = s["phases"].as_array().unwrap();
    let labels: Vec<&str> = phases
        .iter()
        .map(|p| p["label"].as_str().unwrap())
        .collect();
    assert_eq!(
        labels,
        [
            "warm",
            "burst 50%",
            "freeze IOC 0",
            "gain ×2",
            "restart IOC 1"
        ]
    );
    for p in phases {
        assert_eq!(p["interrupted"], false);
        assert_eq!(p["emitted"], 0, "no client: nothing is sent");
    }
    let length = |i: usize| phases[i]["length_s"].as_f64().unwrap();
    assert!((0.25..0.6).contains(&length(0)), "{}", length(0));
    assert!(length(1) < 0.5, "a burst is an instant: {}", length(1));
    // Offered as if every channel were subscribed: 4 channels at 1 Hz, two
    // per IOC, at twice the rate during the gain.
    let offered = |i: usize| phases[i]["offered"].as_f64().unwrap();
    assert!(
        (offered(3) - 4.0 * 2.0 * length(3)).abs() < 0.5,
        "{}",
        offered(3)
    );
    let recoveries = s["recoveries"].as_array().unwrap();
    assert_eq!(recoveries.len(), 1);
    assert_eq!(recoveries[0]["ioc"], 1);
    assert_eq!(recoveries[0]["before"], 0, "nothing to recover");
}

#[test]
fn a_scenario_that_cannot_be_played_is_refused_before_serving() {
    let dir = Scratch::new("scenario-refused");
    let demo = demo_profile(&dir);
    let p = demo.to_str().unwrap();
    let write = |name: &str, body: &str| {
        let f = dir.path(name);
        std::fs::write(&f, body).unwrap();
        f.to_str().unwrap().to_string()
    };
    let far = write(
        "far.toml",
        "[[phase]]\nkind = \"restart\"\niocs = [5]\ndown = \"1s\"\n",
    );
    let typo = write(
        "typo.toml",
        "[[phase]]\nkind = \"steady\"\nduration = \"1s\"\n\n[[phase]]\nkind = \"gain\"\nduration = 60\ngain = 2\n",
    );
    let missing = dir.path("missing.toml");
    let cases: Vec<(String, &str)> = vec![
        (far, "IOC 5 does not exist"),
        (typo, "line 7"),
        (missing.to_str().unwrap().to_string(), "cannot read"),
    ];
    for (file, expect) in &cases {
        let o = mimus(&[
            "serve",
            "-p",
            p,
            "--iocs",
            "2",
            "--search-port",
            "0",
            "--base-port",
            "0",
            "--scenario",
            file,
        ]);
        let out = text(&o);
        assert_eq!(o.status.code(), Some(1), "{file} was accepted: {out}");
        assert!(out.contains(expect), "{file}: expected {expect:?} in {out}");
        assert!(!out.contains("listening"), "{file} got as far as serving");
    }
    let o = mimus(&[
        "serve",
        "-p",
        p,
        "--scenario",
        &cases[0].0,
        "--duration-s",
        "5",
    ]);
    assert_eq!(o.status.code(), Some(2), "a scenario sets its own length");
}

#[test]
fn an_activity_model_lowers_the_offered_load_by_its_fraction() {
    let dir = Scratch::new("activity");
    let demo = demo_profile(&dir);
    let path = dir.path("report.json");
    let o = mimus(&[
        "serve",
        "-p",
        demo.to_str().unwrap(),
        "--pvs",
        "10",
        "--rate-hz",
        "10",
        "--activity",
        "0.5:4",
        "--base-port",
        "0",
        "--search-port",
        "0",
        "--duration-s",
        "0.3",
        "--report",
        path.to_str().unwrap(),
    ]);
    let out = text(&o);
    assert!(o.status.success(), "{out}");
    assert!(out.contains("activity 0.5:4"), "the banner says so: {out}");
    let r = report(&path);
    assert_eq!(r["offered_eps"], 50.0, "half of ten channels at 10 Hz");
    assert_eq!(r["run"]["activity"], "0.5:4");
    assert_eq!(r["quiet"], 0, "nothing subscribed, nothing due");
}
