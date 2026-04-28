[![Build](https://github.com/fg-labs/tricord/actions/workflows/check.yml/badge.svg)](https://github.com/fg-labs/tricord/actions/workflows/check.yml)
[![Version at crates.io](https://img.shields.io/crates/v/tricord)](https://crates.io/crates/tricord)
[![Documentation at docs.rs](https://img.shields.io/docsrs/tricord)](https://docs.rs/tricord)
[![codecov](https://codecov.io/gh/fg-labs/tricord/graph/badge.svg)](https://codecov.io/gh/fg-labs/tricord)
[![License](http://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/fg-labs/tricord/blob/main/LICENSE)

# tricord

Run a command, watch its entire process tree, and report how much CPU,
memory, and disk I/O it used. The companion binary is named `tricorder`.

Think of it as a more thorough `/usr/bin/time -v`: it polls the process tree
on an interval, follows children and grandchildren, and writes a single
record summarising peak memory, total bytes read/written, average CPU load,
and total CPU time. Output is either a tidy TSV or one-line JSON, suited to
both spreadsheets and pipelines.

<p>
<a href="https://fulcrumgenomics.com">
<picture>
  <source media="(prefers-color-scheme: dark)" srcset=".github/logos/fulcrumgenomics-dark.svg">
  <source media="(prefers-color-scheme: light)" srcset=".github/logos/fulcrumgenomics-light.svg">
  <img alt="Fulcrum Genomics" src=".github/logos/fulcrumgenomics-light.svg" height="100">
</picture>
</a>
</p>

[Visit us at Fulcrum Genomics](https://www.fulcrumgenomics.com) to learn more about how we can power your bioinformatics with tricord and beyond.

## Highlights

- **Whole-tree accounting** — follows forked children and aggregates their
  resource usage; an exited child's I/O is still counted.
- **Two output formats** — Snakemake-compatible TSV by default, one-line
  JSON (`--format json`) for programmatic consumers.
- **Optional one-line summary** to stderr (`--summary`).
- **Clean shutdown** — forwards `SIGINT` / `SIGTERM` / `SIGHUP` to the
  child's process group so orchestrators can tear runs down without
  leaking children.
- **No runtime dependencies** — single self-contained Rust binary.
  No Python, no `psutil`.
- **Cross-platform** — Linux (full column set) and macOS (graceful
  degradation; see [Platform notes](#platform-notes)).

## Installation

Requires Rust 1.94.0 or later.

```bash
cargo install tricord
# This installs the `tricorder` binary (the crate name is `tricord`,
# the binary name is `tricorder`).

# Or, from source:
git clone https://github.com/fg-labs/tricord.git
cd tricord
cargo build --release
# Binary is at target/release/tricorder
```

## Usage

```bash
tricorder --out timing.tsv -- bash -c 'samtools sort -@ 8 big.bam -o sorted.bam'
```

```text
Usage: tricorder [OPTIONS] --out <PATH> -- <CMD>...

Options:
      --out <PATH>           Output file path
      --format <FORMAT>      tsv | json [default: tsv]
      --interval <SECONDS>   Sampling interval [default: 0.5]
      --summary              Print one-line summary to stderr after the run
  -v, --verbose...           Increase log level (-v, -vv, -vvv)
  -h, --help                 Print help
  -V, --version              Print version
```

The command to benchmark is everything after the `--` separator. No shell
interpretation is done by `tricorder` itself; if you need shell features (pipes,
quoting), invoke `bash -c '...'` explicitly.

## Output

### TSV (default)

```text
s	h:m:s	max_rss	max_vms	max_uss	max_pss	io_in	io_out	mean_load	cpu_time
12.3456	0:00:12	101.50	2048.00	95.20	96.00	1.25	0.50	175.00	21.60
```

| Column | Units | Meaning |
|---|---|---|
| `s` | seconds (`%.4f`) | Wall-clock running time |
| `h:m:s` | `H:MM:SS` | Same value, human-readable |
| `max_rss` | MiB (`%.2f`) | Peak summed RSS across the process tree |
| `max_vms` | MiB | Peak summed virtual memory size |
| `max_uss` | MiB | Peak summed unique set size |
| `max_pss` | MiB | Peak summed proportional set size (Linux only — see below) |
| `io_in` | MiB | Total bytes read from disk by the process tree |
| `io_out` | MiB | Total bytes written to disk by the process tree |
| `mean_load` | percent of one core | Average CPU load over the run (e.g. 175 = 1.75 cores) |
| `cpu_time` | seconds | Total user + system CPU time across the process tree |

Missing values render as `-`; if the run was too short for any sample to
succeed, every resource column is `NA`.

### JSON (`--format json`)

```json
{"running_time":12.3456,"max_rss":101.5,"max_vms":2048.0,"max_uss":95.2,"max_pss":96.0,"io_in":1.25,"io_out":0.5,"mean_load":175.0,"cpu_time":21.6,"data_collected":true}
```

Same fields, raw numeric types, `null` for missing.

## Platform notes

`tricord` runs on **Linux** and **macOS**. The Linux implementation reads
`/proc/<pid>/{stat,status,smaps_rollup,io}` via the [`procfs`] crate; the
macOS implementation uses [`libproc`]'s `proc_pidinfo` and `proc_pid_rusage`
(`RUSAGE_INFO_V4`).

[`procfs`]: https://crates.io/crates/procfs
[`libproc`]: https://crates.io/crates/libproc

| Metric | Linux | macOS |
|---|---|---|
| `max_rss`, `max_vms` | `/proc/<pid>/status` | `proc_taskinfo` |
| `max_uss` | `/proc/<pid>/smaps_rollup` (Private_Clean + Private_Dirty) | `proc_pid_rusage::ri_phys_footprint` |
| `max_pss` | `/proc/<pid>/smaps_rollup` (Pss) | mirrors `max_uss` (kernel does not compute PSS — see below) |
| `io_in`, `io_out` | `/proc/<pid>/io` | `proc_pid_rusage::ri_diskio_*` |
| `cpu_time` | `/proc/<pid>/stat` (utime + stime) | `proc_taskinfo::pti_total_user + pti_total_system` |

### macOS PSS approximation

The macOS kernel does not compute proportional set size — there is no
equivalent of Linux's `/proc/<pid>/smaps[_rollup]`'s `Pss:` line.
We populate `max_pss` with the same `phys_footprint` value used for
`max_uss`. For benchmarking workloads (a single dominant compute child plus
shared system libraries) the two are typically within a few percent.

If you need real PSS numbers, run on Linux.

## Signals

`tricorder` spawns the child in its own process group and installs handlers
for `SIGINT`, `SIGTERM`, and `SIGHUP`. When any of these arrive at `tricorder`,
they are forwarded to the child's process group. `tricorder` then waits for
the child to exit, writes the (partial) output, and returns:

- the child's exit code if it exited normally
- `128 + signum` if the child was killed by a signal

Hitting Ctrl-C during a run thus tears the child down cleanly and still
produces a record you can inspect for "what was happening when I killed it".

## Use as a library

```rust
use std::time::Duration;
use tricord::{
    run::{run_command, RunOptions},
    format::OutputFormat,
};

let options = RunOptions {
    interval: Duration::from_millis(500),
    output_path: std::path::Path::new("/tmp/timing.tsv").into(),
    format: OutputFormat::Tsv,
    force_summary: false,
    trace_path: None,
};
let outcome = run_command("samtools", &["sort".into(), "in.bam".into()], &options).unwrap();
println!("exit={} cpu_time={:.2}s", outcome.exit_code(), outcome.record.cpu_time);
```

## Motivation, and a note on Snakemake

`tricord` started life as a Rust port of an in-house Python helper
(`bench-cmd.py`) that wrapped `snakemake.benchmark.benchmarked()` so we
could time *just* the expensive part of a rule — `benchmark:` measures the
entire `shell:` block, including any prewarming or staging the rule does
before the work you care about.

The default TSV output is therefore bit-format-compatible with
`snakemake.benchmark.write_benchmark_records(extended_fmt=False)`: identical
columns, identical formatting, drop-in replacement for use inside a rule's
`shell:` block. But there's nothing Snakemake-specific about `tricorder`
itself — it's a general-purpose process-tree resource sampler that's just as
happy being invoked by `make`, a CI script, or by hand at a terminal.

A few values are computed slightly more accurately than Snakemake's sampler:

- **`io_in` / `io_out`**: Snakemake reports the latest snapshot's per-process
  values, summed across alive processes. If a child exits between snapshots
  its I/O is dropped. `tricord` keeps the last-observed cumulative value
  per PID and sums those at the end, so an exited child's I/O is still
  counted.
- **`cpu_time`**: same correction — Snakemake takes the latest snapshot's
  alive-process sum; `tricord` accumulates per-PID maxima.
- **`mean_load`**: derived from the corrected `cpu_time`, so it doesn't
  inherit the first-poll-zero quirk from `psutil.cpu_percent()`.

For long, mostly-monolithic runs the differences are well under a percent.

## License

MIT — see [LICENSE](LICENSE).
