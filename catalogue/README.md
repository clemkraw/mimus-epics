# Reference catalogue

This directory defines **what MIMUS is calibrated against**, and so the
limit of what it can reproduce.

MIMUS replays byte templates recorded from a reference IOC. A calibrated
profile holds only templates built from what `mimus calibrate` observed;
there is no built-in template to fall back on. The catalogue is therefore
the boundary of what any profile calibrated against it covers, and this
file is the answer to "what was tested".

Adding coverage means adding records here, listing their channels in
`channels.txt`, and running `mimus calibrate` again. Nothing else.

## Files

| File | Needs | Adds |
|---|---|---|
| `catalogue.db` | EPICS base and PVXS | scalar and array type codes, enums, alarm transitions, static channels |
| `catalogue-groups.db` | EPICS base and PVXS | nested structures, the variant union, a top-level bool |
| `channels.txt` | - | the channels `mimus calibrate` observes |

`channels.txt` is not the record list. QSRV2 serves every record *field* as
a channel of its own, and some type codes are only reachable that way: no
base record has an unsigned VAL, so `uint32` comes from `MIM:MBBI:0.RVAL`.
Its order matters too; see [From catalogue to profile](#from-catalogue-to-profile).

```bash
softIocPVX -S -d catalogue/catalogue.db -d catalogue/catalogue-groups.db
```

## Scope: PVXS / QSRV2 only

The reference IOC is `softIocPVX`, never `softIocPVA`.

This is a correctness constraint, not a preference. QSRV1 (pvAccessCPP) and
QSRV2 (PVXS) do not put the same bytes on the wire for the same record:
field composition and changed-set conventions differ. A template extracted
against one is wrong against the other, and the bytes decode into something
plausible either way, so nothing downstream notices.

## What "coverage" means here

MIMUS reproduces byte templates, so the unit of coverage is the PVAccess
**type code**, not the normative type. A decoder is recursive over type
codes: a consumer that handles a double, a string, an array, an enum and a
nested structure handles an NTTable too, because an NTTable is those things
arranged differently. Counting record types would mean nothing: an `ai` and
a `calc` are indistinguishable once encoded.

### Scalar type codes

QSRV2 serves an array field of one element (`NELM 1`) as a scalar, so a
one-element `aai` reaches the codes no base record carries in VAL.

| Code | Moving | Static | Source |
|---|---|---|---|
| `double` | `MIM:F64:*` | `MIM:F64:S:*` | `calc`, `ao` |
| `float` | `MIM:F32:*` | - | one-element `aai` |
| `int8` | `MIM:I8:*` | - | one-element `aai` |
| `uint8` | `MIM:U8:*` | `MIM:F64:*.UDF` | one-element `aai`; field channel |
| `int16` | `MIM:I16:*` | `MIM:F64:*.PREC` | one-element `aai`; field channel |
| `uint16` | `MIM:U16:*` | - | one-element `aai` |
| `int32` | `MIM:I32:*` | `MIM:I32:S:*` | `longin`, `longout` |
| `uint32` | `MIM:MBBI:*.RVAL` | `MIM:BI:*.RVAL` | field channels |
| `int64` | `MIM:I64:*` | - | `int64in` |
| `uint64` | `MIM:U64:*` | - | one-element `aai` |
| `string` | `MIM:STR:*`, `MIM:STRV:*`, `MIM:LSTR:*` | `MIM:STR:S:*` | `MIM:STRV:*` varies in length |
| `bool` | - | `MIM:GRP:BOOL*` `enabled` | group `const`; also `valueAlarm.active` in every NTScalar |

`MIM:MBBI:*` use `Raw Soft Channel`, which writes RVAL and derives VAL from
it, so their RVAL moves; `MIM:BI:*` use `Soft Channel`, which leaves RVAL
alone.

### Array type codes

| Code | Moving | Static |
|---|---|---|
| `double[]` | `MIM:AF64:*` | `MIM:AF64:S:*`, `MIM:AEMPTY:*` (empty) |
| `float[]` | `MIM:AF32:*` | - |
| `int8[]`, `uint8[]` | `MIM:AI8:*`, `MIM:AU8:*` | - |
| `int16[]`, `uint16[]` | `MIM:AI16:*`, `MIM:AU16:*` | - |
| `int32[]`, `uint32[]` | `MIM:AI32:*`, `MIM:AU32:*` | - |
| `int64[]`, `uint64[]` | `MIM:AI64:*`, `MIM:AU64:*` | - |
| `string[]` | `MIM:ASTRM:*` (one element) | `MIM:ASTR:*`, `display.form.choices`, NTEnum `choices` |

The numeric arrays copy a `compress` circular buffer, which grows one
element per scan until `NSAM`: their length changes over the first 64 scans
and stays fixed after. `MIM:ASTRM:*` are waveforms reading a scalar string,
so they hold one element that changes every scan.

### Structural type codes

| Code | Covered by |
|---|---|
| structure | every NTScalar, NTScalarArray and NTEnum; nested in `MIM:GRP:*` |
| variant union (`any`) | `MIM:GRP:0`, `MIM:GRP:1`, member `y_any` |

### Behaviors

Type codes are not the whole story. The catalogue also makes the IOC emit
these, because each exercises a decode path nothing else reaches:

- **Variable-length values**: `MIM:STRV:*` are `printf` records formatting
  numbers with `%g`, so their length changes between updates and every
  byte after them shifts. This is the case that breaks a fixed-offset
  template.
- **Variable-length arrays**: the numeric arrays while their circular
  buffers fill, over the first 64 s after the IOC starts.
- **Static channels**: `MIM:*:S:*`, `MIM:ASTR:*`, the zero-length
  `MIM:AEMPTY:*` and the static field channels. A facility has thousands of setpoints, limits and
  descriptions that never move. An archiver subscribes to them, receives
  one initial update and then nothing.
- **Alarm transitions**: `MIM:ALM:*` sweep across their limits, so
  `alarm.severity`, `alarm.status` and `alarm.message` change and the
  update marks a different set of bits: MINOR on HIGH and LOW, MAJOR on
  HIHI and LOLO. `MIM:INVALID:*` read a record that does not exist, which
  gives INVALID with a link alarm.

A profile reproduces the first and the alarm transitions; the next section
says when it reproduces the second, and why not the static channels.

## From catalogue to profile

`mimus calibrate` groups the channels by type descriptor and builds **one
template per type**, from the first channel of that type in `channels.txt`
and its most frequent steady-state changed-set. The opening messages of
all the channels of the type go into the template's opening message. Their
later updates are searched for other changed-sets: each one seen in at
least two distinct bodies is templated and checked the same way, as a
**variant** of the type, named by the fields it changes beyond the ordinary
update. Every channel of the type is assigned to the template.

`channels.txt` is ordered for that rule: each type lists a moving channel
before its static ones, and the strings list the variable-length
`MIM:STRV:*` first. Calibrated against this catalogue, a profile therefore
holds 26 templates, all moving, and reproduces every type code in the
tables above, variable-length strings included.

**When to calibrate.** Started within 64 s of the IOC, calibration sees the
circular buffers grow and templates the arrays as variable length, between
whatever lengths it happened to see. Started later, it sees them full and
templates them at 64 elements. Only the second gives the same profile
twice: start the IOC, wait at least 70 s, then calibrate.

**Alarm transitions.** The `MIM:ALM:*` channels are of the same type as
`MIM:F64:*`, so their limit crossings become the `alarm` variant of
`ntscalar_f64`: in one calibration, ten distinct transitions through
`HIGH`, `HIHI`, `LOW` and back, each reproduced exactly. `mimus inspect`
lists it under its template, and `mimus serve --alarms N` sends it, N times
per channel and per hour, in place of an ordinary update. The transitions
follow one another in the order the IOC sent them, each with the value, the
severity, the status and the message it had; between them, the value
follows the ordinary update's replay, so a channel can read a value inside
its limits while its last transition left it in `MAJOR`.

A group structure such as `nttable_f64` changes `alarm` in every update. It
gets a variant for the updates that move other members, named by its
changed-set since the fields it touches are the same, and no alarm
transition.

What a profile does not reproduce:

- **Static channels.** Every type here has a moving channel listed first,
  so its static channels share the moving template. A type whose channels
  were all static would get a static template instead, whose channels send
  their opening message and nothing after it.
- **The changed-sets nothing was learned from.** A changed-set seen in a
  single body is left out: there is nothing to compare it with, and its
  timestamp cannot be told apart. `session_rules.changed_sets` records
  every one seen, and `mimus inspect` says how much of the traffic the
  dominant one covers.

## Not covered

- **Union, and arrays of structures, unions or variant unions.** No base
  record produces them. A QSRV2 group can serve an array of structures
  (field names such as `"elem[0].v"`), which this catalogue does not use
  yet. NTNDArray, which has a union and arrays of structures, needs
  areaDetector.
- **Bounded strings, bounded and fixed-size arrays.** QSRV2 serves every
  string and array in its variable form, so an IOC never sends these codes.
- **`boolean[]`.** A group `const` is a scalar only: given a JSON array,
  QSRV2 warns `Unknown group field option` and serves one element.
- **Segmented messages.** PVXS reassembles segmented messages it receives
  but does not segment the ones it sends, so no IOC built on it produces
  one. mimus-wire's reassembly is covered by its unit tests.

The claim is therefore **every scalar and array type code QSRV2 serves from
EPICS base records, plus the group structures above**, not "all of
PVAccess".

## Rules for entries

**Two instances per type.** Calibration templates the opening message of a
type by comparing the opening messages of its channels; a type carried by
one channel has its single opening message replayed as recorded, so every
channel of that type opens with the same value. A second channel also
shows whether the server sends the type descriptor
again or only a reference to it (PVXS sends it in full every time; the
profile records what was observed under `session_rules.descriptor_policy`),
and that the shape of a message follows the record type rather than the
channel name.

**A moving and a static variant where possible.** Both are real, and an
archiver has to handle both.

**A moving channel first in `channels.txt`.** Calibration templates each
type from its first channel, as described above.

**Scan rates stay at 1 second.** Calibration is about coverage, not
throughput.

## The structure PVXS emits

An NTScalar double from QSRV2, depth first, with the bit number the
changed-set refers to:

```
 0  structure               17    display.form.index
 1  value                   18    display.form.choices
 2  alarm                   19  control
 3    severity              20    limitLow
 4    status                21    limitHigh
 5    message               22    minStep
 6  timeStamp               23  valueAlarm
 7    secondsPastEpoch      24    active
 8    nanoseconds           25    lowAlarmLimit
 9    userTag               26    lowWarningLimit
10  display                 27    highWarningLimit
11    limitLow              28    highAlarmLimit
12    limitHigh             29    lowAlarmSeverity
13    description           30    lowWarningSeverity
14    units                 31    highWarningSeverity
15    precision             32    highAlarmSeverity
16    form                  33    hysteresis
```

Thirty-four fields. The first update carries all of them; a steady-state
update carries only the ones that changed.

## Version pinning

A profile is only meaningful alongside the environment that produced it.
The manifest records the PVXS and EPICS base versions, a checksum of each
database passed with `--catalogue` (FNV-1a 64-bit and size, enough to
notice a changed catalogue, not a cryptographic digest), the host, the date
and the MIMUS version:

```bash
mimus calibrate --addr 127.0.0.1:5075 \
    --channels catalogue/channels.txt \
    --catalogue catalogue/catalogue.db --catalogue catalogue/catalogue-groups.db \
    --pvxs-version <PVXS version> --epics-version <EPICS base version> \
    --out profile.json
```

`pvxinfo -V` prints both versions for the installation it was built from.

Comparing two profiles across a PVXS upgrade is also a wire-behavior
change detector for PVXS itself. `mimus inspect a.json --diff b.json` lists
the differences, and exits nonzero when the two profiles come from the same
environment (versions and catalogue checksums recorded and equal) and still
differ in their types or type descriptors. Differences
in shape are expected between any two calibrations: which bytes are fixed
depends on what moved while each profile was observed.

## Checking the catalogue on a new build

Run these before calibrating against a PVXS or EPICS base version the
catalogue has not been checked on.

```bash
softIocPVX -S -d catalogue/catalogue.db -d catalogue/catalogue-groups.db &
sleep 5
export EPICS_PVA_AUTO_ADDR_LIST=NO EPICS_PVA_NAME_SERVERS=127.0.0.1:5075
CHANNELS=$(grep -v -E '^\s*(#|$)' catalogue/channels.txt)
```

**1. The IOC loads cleanly.** Its startup output must reach
`iocRun: All initialization complete` with no database error and no
`Q:group` warning.

**2. Every channel resolves, with the type in the tables above.**

```bash
pvxinfo -w 5 $CHANNELS
```

**3. The moving ones move, the static ones do not.**

```bash
timeout -s INT 10 pvxmonitor $CHANNELS 2>/dev/null | grep -v '^ ' | sort | uniq -c
```

Static channels, and `MIM:INVALID:*`, appear once. Moving ones appear about
once per second, `MIM:GRP:0` and `MIM:GRP:1` twice as often since two of
their members trigger an update.

**4. Alarms transition.**

```bash
timeout -s INT 70 pvxmonitor MIM:ALM:0 MIM:INVALID:0 | grep -E '^MIM|alarm'
```

`MIM:ALM:0` must go through `HIGH`, `HIHI`, `LOW` and `LOLO` within one
sweep, and `MIM:INVALID:0` must show severity 3 (INVALID) with the message
`field INP`.

**5. Two instances of a type have the same shape.**

```bash
for pair in "MIM:F64:0 MIM:F64:1" "MIM:MBBI:0 MIM:MBBI:1" \
            "MIM:AF64:0 MIM:AF64:1" "MIM:GRP:0 MIM:GRP:1"; do
    set -- $pair
    diff <(pvxinfo $1 | tail -n +2) <(pvxinfo $2 | tail -n +2) > /dev/null \
        && echo "same shape  $1  $2" || echo "DIFFERENT   $1  $2"
done
```

**6. Keep the reference.**

```bash
for pv in $CHANNELS; do pvxinfo $pv; done > catalogue-pvxinfo.txt
```

It records the types the reference IOC served. Keep it beside the profile
calibrated from it.
