<div align="center">
  <h1 align="center">MIMUS-EPICS</h1>
  <p align="center">
    <strong>Mirrored IOC Model for Ultra-Scale Simulation of EPICS Control Systems</strong>
  </p>
  <p align="center">
    <em>A self-calibrating PVAccess traffic generator for testing EPICS 7 clients at facility scale.</em>
  </p>
  <p align="center">
    <a href="#why">Why</a> •
    <a href="#how-it-works">How it works</a> •
    <a href="#quick-start">Quick start</a> •
    <a href="#usage">Usage</a> •
    <a href="#performance">Performance</a> •
    <a href="#fidelity">Fidelity</a> •
    <a href="#scope-and-limitations">Scope</a>
  </p>
  <p align="center">
    <img src="https://img.shields.io/badge/language-Rust_2024-orange?style=flat-square&logo=rust&logoColor=white" alt="Rust">
    <img src="https://img.shields.io/badge/runtime-Tokio-blue?style=flat-square&logo=data:image/svg+xml;base64,PHN2ZyB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciIHdpZHRoPSIxMjAiIGhlaWdodD0iMTA4IiB2aWV3Qm94PSIwIDAgMTIwIDEwOCI%2BPHRpdGxlPk1hcmsgQ29weTwvdGl0bGU%2BPGcgZmlsbD0iI2ZmZiIgZmlsbC1ydWxlPSJldmVub2RkIj48cGF0aCBkPSJtMjkuODkzIDY4Ljc5NC0xLjY4Ny45NzQgMiAzLjQ2NCAxLjY4Ny0uOTc0IDEyLjIxNC03LjA1MiAxLjY4Ny0uOTc0LTItMy40NjQtMS42ODcuOTc0em01OC4yMTQgMy40NjQgMS42ODcuOTc0IDItMy40NjQtMS42ODctLjk3NC0xMi4yMTQtNy4wNTItMS42ODctLjk3NC0yIDMuNDY0IDEuNjg3Ljk3NHoiLz48cGF0aCBkPSJNODAgNTRjMC0xMS4wNDYtOC45NTQtMjAtMjAtMjBzLTIwIDguOTU0LTIwIDIwIDguOTU0IDIwIDIwIDIwIDIwLTguOTU0IDIwLTIwbS0zNS43NjcgMGMwLTguNzA4IDcuMDU5LTE1Ljc2NyAxNS43NjctMTUuNzY3Uzc1Ljc2NyA0NS4yOTIgNzUuNzY3IDU0IDY4LjcwOCA2OS43NjcgNjAgNjkuNzY3IDQ0LjIzMyA2Mi43MDggNDQuMjMzIDU0Ii8%2BPGNpcmNsZSBjeD0iMjQiIGN5PSI3NSIgcj0iMyIvPjxjaXJjbGUgY3g9IjYwIiBjeT0iOTYiIHI9IjMiLz48Y2lyY2xlIGN4PSI2MCIgY3k9IjEyIiByPSIzIi8%2BPGNpcmNsZSBjeD0iOTYiIGN5PSIzMyIgcj0iMyIvPjxjaXJjbGUgY3g9IjI0IiBjeT0iMzMiIHI9IjMiLz48Y2lyY2xlIGN4PSI5NiIgY3k9Ijc1IiByPSIzIi8%2BPGNpcmNsZSBjeD0iNjAiIGN5PSI1NCIgcj0iMyIvPjxwYXRoIGQ9Ik0yIDUySDB2NGg0MnYtNGgtMnptNzggMGgtMnY0aDQydi00aC0yem0tNy4xNyAxOC4zODktMS4wMDctMS43MjktMy40NTYgMi4wMTQgMS4wMDcgMS43MjggMTkuMTI4IDMyLjgzNSAxLjAwNyAxLjcyOCAzLjQ1Ni0yLjAxNC0xLjAwNy0xLjcyOHpNMzIuNTgxIDIuMTQgMzEuNTc1LjQxMmwtMy40NTcgMi4wMTMgMS4wMDcgMS43MjkgMTkuMTI4IDMyLjgzNCAxLjAwNyAxLjcyOCAzLjQ1Ni0yLjAxMy0xLjAwNi0xLjcyOHpNOTAuNzEgNC4xNTRsMS4wMDYtMS43MjlMODguMjYuNDEyIDg3LjI1MyAyLjE0IDY4LjEyNSAzNC45NzVsLTEuMDA3IDEuNzI4IDMuNDU3IDIuMDEzIDEuMDA2LTEuNzI4ek01MC44MjcgNzIuNTk3bDEuMDA2LTEuNzI4LTMuNDU2LTIuMDE0LTEuMDA3IDEuNzI4LTE5LjEyOCAzMi44MzUtMS4wMDcgMS43MjggMy40NTcgMi4wMTQgMS4wMDYtMS43Mjh6TTU4IDg3LjAxN1Y4OWg0VjcxaC00djEuOTgzem0wLTUxLjk2NVYzN2g0VjE5aC00djEuOTQ4ek00Mi4xMDcgNDYuMjU4bDEuNjg3Ljk3NCAyLTMuNDY0LTEuNjg3LS45NzQtMTIuMjE0LTcuMDUyLTEuNjg3LS45NzQtMiAzLjQ2NCAxLjY4Ny45NzR6bTMzLjc4Ni0zLjQ2NC0xLjY4Ny45NzQgMiAzLjQ2NCAxLjY4Ny0uOTc0IDEyLjIxNC03LjA1MiAxLjY4Ny0uOTc0LTItMy40NjQtMS42ODcuOTc0eiIvPjwvZz48L3N2Zz4%3D" alt="Tokio">
    <img src="https://img.shields.io/badge/protocol-PVAccess_(pvxs)-blueviolet?style=flat-square&logo=data:image/svg+xml;base64,PHN2ZyB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciIHZpZXdCb3g9IjAgMCAyNCAyNCIgZmlsbD0ibm9uZSIgc3Ryb2tlPSIjZmZmIiBzdHJva2Utd2lkdGg9IjIuMyIgc3Ryb2tlLWxpbmVjYXA9InJvdW5kIiBzdHJva2UtbGluZWpvaW49InJvdW5kIj48cGF0aCBkPSJNMS41IDEyaDRsMy03LjUgNCAxNSAzLTcuNWg0Ii8%2BPC9zdmc%2BCg%3D%3D" alt="PVAccess">
    <img src="https://img.shields.io/badge/license-MIT-lightgrey?style=flat-square&logo=opensourceinitiative&logoColor=white" alt="License">
  </p>
</div>

> **A standalone PVAccess traffic generator. EPICS base is not required to build or run MIMUS** - it is a single Rust binary, not an EPICS module. A reference `softIocPVX` is needed once, during the calibration phase, to teach MIMUS what to emit.

---

## Why

Testing an EPICS 7 client (an archiver, a gateway, a forwarder, a control-room display) at facility scale means generating a facility-scale PVAccess stream. Today the only way to do that is to run the real thing: a fleet of `softIocPVX` processes.

That approach breaks down well below the scale that matters. A soft IOC carries records, scan threads and a server-side pvData structure for every PV, and processes every record on every scan. A facility-scale population needs many such processes, and on a single measurement host they compete with the system under test for the same cores: the number you measure is no longer the number you wanted.

But a client does not observe an IOC. It observes **bytes on a socket**. For a scalar monitor update in steady state, the PVAccess message is a fixed-shape frame in which only the value and the timestamp change from one update to the next.

MIMUS exploits exactly that. It watches a real `softIocPVX`, learns the byte-level shape of the messages it emits, verifies that it can reproduce them exactly, then replays them at arbitrary scale without any EPICS code in memory.

## How it works

MIMUS runs in three phases. The reference IOC and the client under test need never run together: the IOC is stopped once the profile is written, before the client connects.

```
  ┌─────────────────────────────┐
  │  softIocPVX (reference)     │   1 · calibrate
  │  catalogue/, every type     │
  └──────────────┬──────────────┘
                 │  real PVAccess traffic
                 ▼
  ┌─────────────────────────────┐
  │  mimus calibrate            │   observe · diff · verify
  │  learns message templates   │
  └──────────────┬──────────────┘
                 ▼
  ┌─────────────────────────────┐
  │  profile.json               │   templates + session rules
  └──────────────┬──────────────┘
                 ▼
  ┌─────────────────────────────┐
  │  reference IOC stopped      │   2 · teardown
  └──────────────┬──────────────┘
                 ▼
  ┌─────────────────────────────┐
  │  mimus serve                │   3 · generate
  │  N ports = N virtual IOCs   │
  └──────────────┬──────────────┘
                 ▼
  ┌─────────────────────────────┐
  │  client under test          │
  │  archiver · gateway · UI    │
  └─────────────────────────────┘
```

### 1 · Calibrate

`mimus calibrate` connects to a reference `softIocPVX` as an ordinary PVAccess client and subscribes to every channel in [`catalogue/channels.txt`](catalogue/channels.txt). It parses the PVAccess **framing** (message boundaries, request ids, monitor sub-commands) but treats each monitor payload as opaque bytes.

Variable regions are found by **differential analysis**: successive updates of the same channel are compared byte by byte. Bytes that never change become the fixed part of a template; bytes that change become slots. When serving, the timestamp slots get the current time, and every other slot, the value included, replays the bytes it took during calibration, in order and in a loop.

Observation ends on a **completeness criterion**, not a timer: every channel must show at least `--min-distinct` distinct bodies (30 by default) or settle, repeating one body for `--settled-after` seconds. A channel the IOC never answers is reported as a gap rather than holding up the run. Calibration then builds one template per type descriptor; [the catalogue README](catalogue/README.md#from-catalogue-to-profile) describes exactly which channel each template comes from.

### 2 · Verify, then teardown

Before anything is written, every template must reproduce exactly the bytes it was built from, and one of its messages is rendered through the emitter that will serve it. A type that falls short of an exact match is left out of the profile and reported by name; if nothing verifies, no profile is written. Once the profile exists, the reference IOC is no longer needed.

This is what makes fidelity a verified property rather than a claim - see [Fidelity](#fidelity).

### 3 · Generate

`mimus serve` loads the profile, replicates its templates across the requested number of channels and opens one listening socket per virtual IOC. Each socket behaves as an independent IOC: it validates the connection, answers searches, creates channels, serves `GET`, `GET_FIELD` and `MONITOR` (with pipelining), answers echoes, and then emits. `PUT` and `RPC` are refused with an error status, as PVXS refuses an operation a channel does not implement.

Emission runs on a timing wheel. At each tick, the channels due in that bucket have their compiled template copied into the connection's write buffer with the value and timestamp slots written in place.

## Quick start

```bash
# 0 · build (no EPICS needed)
cargo build --release

# 1 · start the reference IOC with the catalogue, and let its arrays fill
softIocPVX -S -d catalogue/catalogue.db -d catalogue/catalogue-groups.db &
sleep 70

# 2 · calibrate; the versions go into the profile's manifest (pvxinfo -V prints them)
./target/release/mimus calibrate --addr 127.0.0.1:5075 \
    --channels catalogue/channels.txt \
    --catalogue catalogue/catalogue.db --catalogue catalogue/catalogue-groups.db \
    --pvxs-version <PVXS version> --epics-version <EPICS base version> \
    --out profile.json
kill %1                     # the reference IOC is no longer needed

# 3 · see what was learned
./target/release/mimus inspect profile.json

# 4 · serve 100 000 double channels at 10 Hz from 4 virtual IOCs
#     (without --mix, every template in the profile is served in equal shares)
./target/release/mimus serve --profile profile.json \
    --iocs 4 --pvs 100000 --rate-hz 10 --mix ntscalar_f64 --search-port 0

# 5 · in another terminal, read them with the PVXS tools
export EPICS_PVA_NAME_SERVERS="127.0.0.1:5075 127.0.0.1:5076 127.0.0.1:5077 127.0.0.1:5078"
pvxget MIM:ntscalar_f64:0
pvxmonitor MIM:ntscalar_f64:0
```

Channel names follow `MIM:{template}:{index}` by default, with one index across the whole fleet, and `mimus serve` prints the address list to use with the name of a channel it serves. To try the server without a reference IOC, `mimus demo-profile` writes a hand-made profile; it is marked synthetic, and every command that loads it says so.

## Usage

```
mimus calibrate     extract templates by observing a reference softIocPVX
mimus inspect       print what a profile contains, without serving it
mimus serve         serve a fleet of virtual IOCs from a profile
mimus demo-profile  write a hand-made profile for trying the server out
```

`mimus <command> --help` gives the full description of every option.

### `mimus calibrate`

| Option | Default | What it does |
|---|---|---|
| `-a, --addr` | `127.0.0.1:5075` | Reference IOC, `host:port` |
| `-c, --channels` | `catalogue/channels.txt` | Channels to observe, one per line |
| `-o, --out` | `profile.json` | Profile to write |
| `--catalogue` | - | Database the reference IOC loaded, recorded in the manifest so a profile can be traced to what produced it. Give it once per database |
| `--pvxs-version`, `--epics-version` | `unknown` | Versions of the reference IOC, recorded in the manifest |
| `--min-distinct` | `30` | Distinct bodies a moving channel must show |
| `--settled-after` | `30` | Seconds without anything new before a channel counts as settled |
| `--deadline-s` | `600` | Overall limit |
| `--allow-incomplete` | off | Write a profile even if the criterion was not met (development only). The profile is marked incomplete, and every command that loads it says so |
| `--debug` | off | Print what was seen, channel by channel |

`mimus calibrate` exits 0 with a profile written, 3 when the completeness criterion was not met (no profile is written), and 1 on any other error.

### `mimus inspect`

`mimus inspect profile.json` lists the templates with the number of channels assigned to each, their body size and whether it varies, and how much of the observed traffic the dominant changed-set covers. `mimus inspect a.json --diff b.json` compares two profiles. It exits nonzero when they come from the same environment (the same PVXS and EPICS base versions and catalogue checksums, all recorded at calibration) and still differ in their types or type descriptors; differences in shape are reported but expected, since which bytes are fixed depends on what moved while each profile was observed.

### `mimus serve`

**Population and load**

| Option | Default | What it does |
|---|---|---|
| `-p, --profile` | - | Profile produced by `mimus calibrate` |
| `--pvs` | `1000` | Total channels across the fleet |
| `--iocs` | `1` | Virtual IOCs, one listening port each. A consumer typically runs one task per connection, so this caps how far its work can spread across cores |
| `--rate-hz` | `1` | Updates per second per channel. Offered load is `pvs × rate` |
| `--rate-mix` | - | Several rates at once: `10:2,1:60,0.1:38` puts 2 % of channels at 10 Hz, 60 % at 1 Hz, 38 % at 0.1 Hz. Shares are relative. Overrides `--rate-hz` |
| `--activity` | every update sent | Channels post in bursts and fall silent in between, as behind a monitor deadband: `0.3:20` keeps each channel active 30 % of the time, in bursts of 20 updates on average (see [Irregular updates](#irregular-updates)) |
| `--mix` | every template, equal shares | Which templates to serve and in what proportion: `ntscalar_f64:3,ntscalar_i32:1` |
| `--layout` | `blocked` | `blocked` gives each IOC a contiguous range of channels, as a facility does; `round-robin` interleaves them, each IOC still getting every template and rate of the mix |
| `--prefix` | `MIM` | Prefix of the default names; a `--name-template` replaces it |
| `--name-template` | `{prefix}:{t}:{i}` | Name pattern. Tokens: `{i}` global index, `{ioc}` IOC index, `{n}` index within the IOC, `{t}` template; `{i:06}` pads. Real names run to 30 or 40 characters, which drives a consumer's memory for name maps |

**Network and discovery**

| Option | Default | What it does |
|---|---|---|
| `--bind` | `127.0.0.1` | Address of the TCP listeners |
| `--base-port` | `5075` | First TCP port; each IOC takes the next one. `0` lets the OS pick |
| `--search-port` | `5076` | UDP port on which searches are answered. `0` turns the responder off |
| `--search-bind` | `0.0.0.0` | Address the UDP responder listens on |

**Protocol behavior**

| Option | Default | What it does |
|---|---|---|
| `--backpressure` | `block` | `block` lets TCP throttle MIMUS; `drop` holds the nominal rate and counts what it discards (see below) |
| `--high-water` | `1048576` | Bytes allowed to queue per connection before `drop` discards: the stand-in for an IOC's monitor queue |
| `--pipeline` | `permissive` | `strict` withholds updates from a pipelined monitor with no credit left, as PVXS does: the credit starts at the client's queue size and grows with each acknowledgement. `permissive` only counts acknowledgements |
| `--always-full-descriptor` | off | Send the full type descriptor on every monitor, whatever the profile recorded |
| `--tick-us` | `1000` | Scheduler resolution, from 1 µs to 1 s; lower it for rates above a few hundred hertz |
| `--coalesce-us` | `0` | Hold updates back up to this long to write them in fewer, larger writes. Zero writes on every tick; a nonzero value adds latency a consumer will see in the timestamps |

**Faults and run control**

| Option | Default | What it does |
|---|---|---|
| `--drop-connections` | - | Close one live connection every N seconds without warning: the consumer must reconnect and re-subscribe every channel it had there. A PVXS client that finds MIMUS through `EPICS_PVA_NAME_SERVERS` can take several seconds to come back, so keep N well above that |
| `--scenario` | - | Play a scenario: phases of steady load, raised rates, bursts, freezes and IOC restarts, one after the other, each accounted apart. The run ends with the last phase (see [Scenarios](#scenarios)) |
| `--duration-s` | until interrupted | Stop after this many seconds. Ctrl-C or SIGTERM ends a timed run early too, and the report is still written |
| `--report` | - | Write the accounting report (JSON) here on exit |
| `--debug` | off | Print the first message of each shape, in hex, as it goes out |

### Examples

```bash
# one million channels at 1 Hz across 20 virtual IOCs, with facility-like names
mimus serve -p profile.json --iocs 20 --pvs 1000000 --rate-hz 1 \
    --name-template 'SR:C{ioc:02}:DEV{n:05}:{t}'

# a realistic spread of scan rates, a single type, one hour, with a report
mimus serve -p profile.json --iocs 8 --pvs 200000 \
    --rate-mix 10:2,1:60,0.1:38 --mix ntscalar_f64 \
    --duration-s 3600 --report run.json

# loss under offered load instead of saturation
mimus serve -p profile.json --iocs 16 --pvs 500000 --rate-hz 7 --backpressure drop

# one connection dropped without warning every 30 s
mimus serve -p profile.json --iocs 10 --pvs 100000 --drop-connections 30

# an IOC restarts once the consumer has subscribed to everything
mimus serve -p profile.json --iocs 8 --pvs 200000 \
    --scenario scenarios/ioc-restart.toml --report restart.json
```

### Discovery

With the defaults, MIMUS answers UDP searches on port 5076 while its TCP listeners stay on the loopback. A client on the same host that searches by unicast (`EPICS_PVA_ADDR_LIST=127.0.0.1`) finds it. A client that broadcasts, or one on another host, is told to connect to an address where nothing listens; MIMUS warns about this at startup. Choose one of:

- **`--bind 0.0.0.0`**: the server is reachable from the network, and UDP discovery works for every client.
- **`--search-port 0`** and `EPICS_PVA_NAME_SERVERS` set to the addresses MIMUS prints: no UDP at all.

MIMUS shares UDP port 5076 with an IOC on the same host, as PVAccess servers do, and both stay discoverable; only the TCP port must differ from the IOC's 5075 (`--base-port`).

### Backpressure: two different questions

When the client under test cannot keep up, TCP would normally throttle the sender, which silently turns a throughput measurement into something else. A real IOC does not behave that way: its monitor queue overflows, it drops, and it keeps running at its scan rate. MIMUS makes the choice explicit:

- **`--backpressure block`**: MIMUS lets TCP throttle it. What it managed to send over the window *is* the client's real absorption capacity. This is the saturation measurement.
- **`--backpressure drop`**: MIMUS holds its nominal rate and discards what does not fit within `--high-water`, counting every discard, as intermediate values are lost when an IOC's monitor queue overflows. This is the loss-under-offered-load measurement.

These answer different questions. Any published figure must state which mode produced it.

### Irregular updates

A real IOC does not post every channel like a clock. A monitor with a deadband sends only when the value has moved far enough: a channel posts at its scan rate while its process moves, and not at all while it is still. A consumer fed only regular updates is spared the gaps, runs and uneven batches a facility puts on it, and its figures flatter it.

`--activity FRACTION:BURST` gives every channel that behavior. A channel is either active, and sends each update it is due, or quiet, and sends none. After each update it is due, an active channel falls quiet with probability `1/BURST`, and a quiet one wakes at the rate that keeps it active `FRACTION` of the time. Its bursts then last `BURST` updates on average, its silences `BURST × (1 − FRACTION) / FRACTION`, and it sends `FRACTION` of what its rate asks, which is the offered load MIMUS reports. The draws come from the channel's seed and the count of updates it was due, so the same channel goes through the same bursts and silences on every run.

What a quiet channel does not send is counted as `quiet`, not as a loss: an IOC would not have sent it either. Bursts asked for by a scenario send from every channel they select, quiet or not.

### Scenarios

Consumers seldom fail in steady state. They fail when thousands of channels change at the same instant, when an IOC restarts and every channel it served reconnects together, when a stalled IOC resumes, or when the load climbs past what they absorb. `--scenario` stages those moments against the same fleet, from a TOML file of phases played in order:

```toml
name = "IOC restart"

[start]
subscriptions = "all"    # play nothing until the consumer has subscribed to every channel
timeout = "10m"

[[phase]]
kind = "steady"
duration = "30s"

[[phase]]
kind = "restart"
iocs = [0]
down = "10s"             # gone for ten seconds...
duration = "90s"         # ...then eighty to come back

[[phase]]
kind = "steady"
duration = "30s"
```

| `kind` | Fields | What happens |
|---|---|---|
| `steady` | `duration` | The configured rates |
| `gain` | `duration`, `gain`, `iocs` | Every channel of the IOCs targeted at `gain` times its rate. `gain = { from = 1, to = 20 }` ramps over the phase; with `steps = 10` as well, it climbs in ten equal steps, each reported apart |
| `burst` | `share`, `iocs`, `duration` | When the phase starts, `share` of the subscribed channels (all by default) send one update each, all with the same timestamp, as in a beam dump. The same channels are picked on every run |
| `freeze` | `duration`, `iocs`, `resume` | The IOCs targeted send nothing; what falls due is counted as `held` and never sent. At the end, `resume = "burst"` (the default) sends every subscription its latest value at once, as a monitor queue does; `"silent"` carries on at the rates |
| `restart` | `down`, `duration`, `iocs` | The IOCs targeted close every connection, refuse new ones and answer no search for `down`, then come back on the same ports. The rest of `duration` (`down` by default) is left for the consumer to reconnect |

`iocs` is `"all"`, the default, or a list of indices. `label` names a phase in the report. Durations carry their unit: `500ms`, `60s`, `5m`, `1h`, `1m30s`. `repeat = 3` plays the phases three times. [`scenarios/`](scenarios) holds a beam dump, an IOC restart, a freeze, a staircase to find a consumer's limit, and a bad day that combines them.

Each phase starts from the configured rates, and what it changes lasts as long as it does. A raised rate is not a rebuilt wheel: every schedule is built at the highest gain of the scenario and thinned back to the gain of the moment, so a channel at three times its rate sends three evenly spread updates, not three at once. A file that cannot be played is refused before MIMUS listens, with the line and the phase at fault.

The report accounts for each phase apart, and for how the consumer came back from each restart. From a run of 1 000 channels at 10 Hz on two virtual IOCs, with `pvxmonitor` subscribed to all of them:

```
scenario readme (readme.toml)
  waited 1.4 s for 1000 subscriptions before the first phase
  phase                          start    length     offered     emitted   dropped      held  late closed   subscr.
  before                         0.0 s    10.0 s      100008      100015         0         0     0      0      1000
  burst 100%                    10.0 s     0.0 s        1308        1295         0         0     0      0      1000
  restart IOC 0                 10.0 s    12.0 s      109689      101580         0         0     0      1      1000
  freeze                        22.0 s     2.0 s        1307        1295         0     20005     0      0      1000
  after                         24.0 s     5.0 s       49696       49705         0         0     0      0      1000
  IOC 0 (restart IOC 0): reconnected 0.95 s after it came back; all 500 subscriptions back after 2.96 s
```

`offered` is what the phase asked for if every channel was subscribed throughout: the rates times the phase's gain, nothing from a frozen or a down IOC, plus the updates of its bursts. The restart's shortfall is the time `pvxmonitor` took to subscribe again to the 500 channels of IOC 0, and the line under the table measures it: the first connection came back 0.95 s after the IOC did, and the last subscription 2.96 s after. The JSON report holds the same, under `scenario`.

## Accounting

MIMUS knows exactly what it put on the wire. On exit it prints a summary and, with `--report`, writes it as JSON. Abridged, from a 12-second run of 1 000 channels at 10 Hz on two virtual IOCs, with `pvxmonitor` subscribed to all of them:

```json
{
  "emitted": 115564,
  "dropped_backpressure": 0,
  "bytes": 4886624,
  "elapsed_s": 12.001793013,
  "offered_eps": 10000.0,
  "achieved_eps": 9628.894605566382,
  "late_ticks": 0,
  "flow_control_holds": 0,
  "pipeline_acks": 0,
  "sessions": 2,
  "sessions_dropped": 0,
  "subscriptions": 1000,
  "wheel_steps": 22914,
  "profile": { "pvxs_version": "1.4.1", "epics_base_version": "7.0.8.1", "templates": 26, "synthetic": false, "...": "..." },
  "run": { "iocs": 2, "pvs": 1000, "rate_mix": "10:1", "mix": "ntscalar_f64:1", "backpressure": "block", "...": "..." }
}
```

| Field | Meaning |
|---|---|
| `emitted` | Updates written to the wire, opening messages included |
| `sessions`, `sessions_dropped`, `sessions_closed_down` | Connections accepted over the run, those MIMUS closed under `--drop-connections`, and those closed because a scenario took their IOC down |
| `dropped_backpressure` | Updates discarded under `--backpressure drop`. Counted, never silent |
| `bytes` | Bytes written, headers included |
| `elapsed_s` | Seconds from the moment MIMUS is listening to the end of the run |
| `offered_eps` | The load the fleet offers: every channel's rate, summed, whether a client subscribed to it or not |
| `achieved_eps` | `emitted / elapsed_s`, over the whole run, including any time before the consumer subscribed |
| `subscriptions` | Monitors still open when the run ended |
| `late_ticks` | Times a connection fell more than 16 ticks behind. Up to 16 missed ticks are made up; beyond that the rest is skipped, never sent, so nonzero means the offered rate was not delivered in full, and any figure from the run must say so |
| `pipeline_acks`, `flow_control_holds` | Credit acknowledgements received from pipelined monitors, and updates withheld from one with no credit left (`--pipeline strict` only) |
| `wheel_steps` | Turns of the timing wheel, to tell a wheel turning at the wrong speed from a wrong population |
| `held`, `burst` | Under a scenario: updates that fell due while their IOC was frozen and were never sent, and updates sent by bursts and resumes, a part of `emitted` |
| `quiet` | Under `--activity`: updates due while the channel was quiet, and so not sent |
| `scenario` | The scenario played, phase by phase, with its restarts' recovery times; `null` without one |
| `profile`, `run` | The manifest: what was served, and with which options |

`emitted` is what makes an end-to-end conservation check possible on the consumer side. Updates dropped under `--backpressure drop` never reached the wire, so they are not part of it. For an archiver:

```
mimus.emitted == rows_stored + <every loss counter the client exposes>
```

Any residual is either a bug in the client or a missing counter in it, and both are worth knowing. A consumer that cannot close this equation is a consumer that loses data without noticing. On MIMUS's side, an update due on a subscribed channel is either emitted, dropped (`dropped_backpressure`), withheld from a pipelined monitor with no credit (`flow_control_holds`), held while a scenario froze its IOC (`held`) or left unsent while its channel was quiet under `--activity` (`quiet`); only the time skipped after a late tick is in none of the five.

## Performance

### Cost model

The work MIMUS does per update is small and constant:

- **No encoding.** Each template is compiled once. Serving an update copies its fixed bytes and writes the slots in place. A steady-state `double` update is a 41-byte frame, of which 20 bytes are rewritten: the value (8), `secondsPastEpoch` (8) and `nanoseconds` (4).
- **No records.** There is no record processing, no scan thread and no pvData structure. Each channel is a small fixed-size record (its template, IOC, rate group, seed and update counter) plus its name and an entry in the name index that answers searches. The message itself exists once per template, not once per channel.
- **Few system calls.** A timing wheel finds the channels due on each tick, and each connection's updates for the tick go out in one write. `--coalesce-us` batches further, at the cost of latency.
- **A light startup.** Starting builds the channel table and opens the listeners; there is no record to initialize, process or scan.

### Measuring it

Three scripts at the root of the repository measure MIMUS against `softIocPVX` on the same machine. They need Linux, `python3`, a PVXS build (`PVXS_BIN`, default `/opt/epics/pvxs/bin/linux-x86_64`), a release build of MIMUS and a calibrated profile.

| Script | What it does |
|---|---|
| `bench.sh` | Every measurement behind the published figures, in one run: memory; capacity of MIMUS and of `softIocPVX`; every template; the whole profile on both sides; head to head; subscription; a soak; where `softIocPVX` stops serving (population against rate); records per IOC process; one channel up to ten million updates a second; the reference client (`pvxmonitor`) on both; timing accuracy; interoperability of every template with `pvxinfo`, `pvxget` and `pvxmonitor`; and a drift control run last |
| `bench-selftest.sh` | Checks the benchmark's own consumer against a fake server whose true frame count is known, in ten shapes from a steady stream to a silent server. `bench.sh` runs it first and refuses to start if it fails |
| `repro-pvxs-stall.sh` | A standalone reproduction, with nothing from MIMUS in it, of a `softIocPVX` monitor stall observed during the benchmark |

```bash
cargo build --release
sudo cpupower frequency-set -g performance    # optional; the governor is recorded either way
./bench.sh --profile profile.json --reps 5     # about eleven hours; --quick for a short check
./bench.sh --report bench-YYYYMMDD-HHMMSS      # print the report of a finished run again
```

Every measurement is validated, retried on failure, and excluded by name if it never validates. A run writes `bench-YYYYMMDD-HHMMSS/` with `run.json` (every parameter, the profile and its hash, the hash of the script), the raw per-point data, the machine's clock and load through the run, and the soak trend, so any figure can be checked without rerunning it.

### Results

Measured figures are published with the accompanying paper. Each comes from a `bench.sh` run and is quoted with its `run.json`; a figure without one is not a result of this project.

## Fidelity

The obvious objection to any synthetic load generator is: *how do you know it is not gentler than the real thing?* MIMUS answers mechanically rather than rhetorically.

- **The ground truth is the reference implementation, not the specification.** Templates are extracted from bytes actually emitted by `softIocPVX`, so implementation details the specification does not mandate - field ordering, structure composition, changed-set conventions - are reproduced rather than guessed.
- **No knowledge of EPICS records is compiled in.** MIMUS knows the PVAccess protocol and the PVData encoding, but no record semantics, and no normative-type definition outside the hand-written demo profile, which is marked synthetic. If PVXS changes what it puts on the wire, the observation changes and the templates follow.
- **Every calibration re-verifies.** The verification step is not an optional test; it is the gate every template passes to enter a profile.
- **Coverage is a number, not an adjective.** Calibration reports how many templates it built and which channels map to which: `82 PVs observed -> 26 distinct templates` is a statement a reviewer can check.
- **MIMUS refuses to synthesize what it has not learned.** A calibrated profile holds only templates built from what was observed: no built-in template is mixed in and there is no "close enough" fallback. A type absent from the catalogue is a type MIMUS cannot serve.

### What a profile reproduces

For every type it observed, a profile reproduces:

- the exact type descriptor;
- the **opening message** of a subscription or a `GET`, taken from the opening messages the reference IOC sent for that type, with its timestamp set to the channel's last processing time. Opening messages rotate across the served channels, so limits, units, enum choices and alarm states vary as they did on the IOC;
- a stream of **steady-state updates** in the shape the IOC used most for that type, typically value and timestamp, at the rate chosen when serving. Length changes are reproduced: a string whose length moves between updates stays variable.

What it does not reproduce yet, by design of the current calibration: channels that stay static, alarm transitions, and the less frequent update shapes of a type. [The catalogue README](catalogue/README.md#from-catalogue-to-profile) gives the detail.

### Session rules

A profile is more than a bag of bytes. It also records what the reference server did at the session level: the buffer and introspection-registry sizes it proposed, the changed-sets it used and how often, and whether it ever sent a type descriptor as a reference to one it had sent before. PVAccess allows a server to send a full descriptor once per connection and only its identifier afterwards; PVXS sends the full descriptor every time. MIMUS does what the profile observed. The per-connection registry is implemented for servers that do use it, and `--always-full-descriptor` forces full descriptors whatever the profile says.

## Scope and limitations

- **The monitor path, plus `GET` and `GET_FIELD`.** The monitor path dominates a long-running client's workload; the two read commands make `pvxget` and `pvxinfo` work against MIMUS, which turns the standard PVXS tools into an independent conformance check. `PUT` and `RPC` are out of scope and answered with an error, and MIMUS sends no beacons.
- **PVXS (QSRV2) only.** QSRV1 puts different bytes on the wire for the same record; a profile calibrated against one is wrong against the other.
- **One update shape per type.** Every channel of a type replays the same recorded values in a loop, in the type's most frequent shape. Static channels of a type that also has a moving channel update at the serving rate (`--rate-hz` must be positive); a type whose every channel was static gets a static template, whose channels send their opening message and nothing after it, as on the IOC. A channel keeps the alarm state of its opening message.
- **Structure is replicated; IOC dynamics are not.** Scan jitter under host load and monitor-queue behavior are properties *of an IOC*, and MIMUS deliberately does not reproduce them: a measurement rig wants a deterministic, reproducible offered load. Where IOC behavior does affect the client (overflow policy), it is exposed as `--backpressure`.
- **A profile is bound to a version.** Its manifest records the PVXS and EPICS base versions, a checksum of each catalogue database, the host, the date and the MIMUS version. Comparing profiles across PVXS releases is, incidentally, a wire-behavior change detector.
- **Calibrate at small scale, generate at large scale.** The shipped catalogue has fewer than a hundred channels, enough to cover every type. Replication to millions happens afterwards, without the reference IOC.

## Repository layout

| Path | Contents |
|---|---|
| [`crates/mimus-wire`](crates/mimus-wire) | PVAccess framing, headers, segmentation reassembly, control messages, and a generic PVData walker the tests decode MIMUS's messages with |
| [`crates/mimus-profile`](crates/mimus-profile) | Profile format: templates, slots, session rules, manifest, schema versioning |
| [`crates/mimus-emit`](crates/mimus-emit) | Template compilation and rendering, signals, timing wheel, counters |
| [`crates/mimus-calibrate`](crates/mimus-calibrate) | Client mode: observation, differential analysis, template building, shadow verification |
| [`crates/mimus-serve`](crates/mimus-serve) | Server mode: fleet and naming, listeners, sessions, the per-connection type registry, UDP search responder |
| [`src`](src) | The `mimus` command line |
| [`catalogue`](catalogue) | The reference database and channel list calibration runs against |
| [`scenarios`](scenarios) | Example scenarios for `--scenario` |
| `bench.sh`, `bench-selftest.sh`, `repro-pvxs-stall.sh` | The benchmark and its companions |

## Building and testing

```bash
cargo build --release
cargo test --workspace
```

No EPICS base, no PVXS, no C++ toolchain. The Rust version is pinned in [`rust-toolchain.toml`](rust-toolchain.toml). Continuous integration checks formatting, Clippy and the documentation with warnings denied, and runs the whole test suite, in a Debian 13 container. A `softIocPVX` is needed only for `mimus calibrate` and the benchmark; see the [PVXS documentation](https://epics-base.github.io/pvxs/). Contributions are welcome: see [CONTRIBUTING.md](CONTRIBUTING.md).

## Related work

Synthetic PVAccess servers exist ([`p4p`](https://github.com/mdavidsaver/p4p) and libraries built on it can serve arbitrary PVs) but they are full protocol implementations, with the per-event cost that implies. Benchmarks of EPICS infrastructure generally drive load from real IOCs; see, for example, the ESS study [*An Archiver Appliance Performance and Resources Consumption Study*](https://doi.org/10.18429/JACoW-ICALEPCS2021-WEPV048) (ICALEPCS'21, WEPV048).

The underlying technique (learning message templates from observed traffic and replaying them at scale) is long established in network testing (tcpreplay, Ostinato, Cisco TRex). MIMUS applies it to PVAccess.

## License

MIT. See [LICENSE](LICENSE).

## Author

Clément Krawiec.

MIMUS was developed with the help of Claude, Anthropic's AI assistant, used through the Claude app ([claude.ai](https://claude.ai)) rather than Claude Code.

MIMUS was written to benchmark [AURA-EPICS](https://github.com/clemkraw/aura-epics), a high-throughput PVAccess archiver, but has no dependency on it: any PVAccess client can be driven by it.
