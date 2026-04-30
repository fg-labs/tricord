//! The aggregate benchmark record.
//!
//! Mirrors `snakemake.benchmark.BenchmarkRecord` so that downstream tools that
//! parse Snakemake's `benchmark:` directive output can ingest our TSV unchanged.

use std::fmt::Write as _;

use serde::Serialize;

/// Tab-separated header row, identical to Snakemake's.
pub const TSV_HEADER: &str =
    "s\th:m:s\tmax_rss\tmax_vms\tmax_uss\tmax_pss\tio_in\tio_out\tmean_load\tcpu_time";

/// Tab-separated header for the per-tick trace TSV (`--trace`). Distinct from
/// [`TSV_HEADER`] because the trace records *instantaneous* memory and
/// *cumulative-so-far* I/O and CPU values; there is no `mean_load` or `h:m:s`
/// column because those are only meaningful for the whole-run aggregate.
pub const TRACE_TSV_HEADER: &str = "s\trss\tvms\tuss\tpss\tio_in\tio_out\tcpu_time\tn_procs";

/// One per-tick row of the trace TSV.
///
/// Memory fields (`rss`, `vms`, `uss`, `pss`) are summed across the *currently-
/// live* process tree at that tick, in MiB. I/O and CPU fields are the running
/// per-PID accumulators summed across *every* PID observed so far (including
/// children that have already exited), so the values are monotonically
/// non-decreasing across rows.
#[derive(Debug, Clone, Default)]
pub struct TickRecord {
    /// Seconds since the sampler started.
    pub elapsed: f64,
    /// Summed RSS across live processes, MiB.
    pub rss: f64,
    /// Summed VMS across live processes, MiB.
    pub vms: f64,
    /// Summed USS across live processes, MiB. `None` if no process exposed it.
    pub uss: Option<f64>,
    /// Summed PSS across live processes, MiB. `None` if no process exposed it.
    pub pss: Option<f64>,
    /// Cumulative bytes read across observed PIDs, MiB. `None` if never seen.
    pub io_in: Option<f64>,
    /// Cumulative bytes written across observed PIDs, MiB. `None` if never seen.
    pub io_out: Option<f64>,
    /// Cumulative user + system CPU time across observed PIDs, seconds.
    pub cpu_time: f64,
    /// Number of live processes in this tick.
    pub n_procs: usize,
}

impl TickRecord {
    /// Render this tick as a single TSV row using `%.4f` for elapsed time,
    /// `%.2f` for floats, `-` for missing optional values, and a bare integer
    /// for `n_procs`.
    #[must_use]
    pub fn to_tsv_row(&self) -> String {
        let mut out = String::with_capacity(96);
        write!(out, "{:.4}\t{:.2}\t{:.2}", self.elapsed, self.rss, self.vms).unwrap();
        for value in [self.uss, self.pss, self.io_in, self.io_out] {
            out.push('\t');
            out.push_str(&format_optional_float(value));
        }
        write!(out, "\t{:.2}\t{}", self.cpu_time, self.n_procs).unwrap();
        out
    }
}

/// One row of benchmark output: the aggregate of all samples taken across the run.
///
/// Memory values are in MiB. `io_in` and `io_out` are in MiB. `running_time` and
/// `cpu_time` are in seconds. `mean_load` is "percent of one CPU core" averaged
/// over the wall-clock run (i.e. 100 = one core fully utilized; 200 = two).
///
/// `Option`-valued fields are `None` when the underlying OS does not expose the
/// metric for the platform (e.g. `io_in` on macOS prior to introspection, or
/// `max_pss` on macOS where the kernel does not compute proportional set size).
#[derive(Debug, Clone, Default, Serialize)]
pub struct BenchmarkRecord {
    /// Wall-clock running time in seconds.
    pub running_time: f64,
    /// Peak resident set size, summed across the process tree, in MiB.
    pub max_rss: Option<f64>,
    /// Peak virtual memory size, summed across the process tree, in MiB.
    pub max_vms: Option<f64>,
    /// Peak unique set size, summed across the process tree, in MiB.
    pub max_uss: Option<f64>,
    /// Peak proportional set size, summed across the process tree, in MiB.
    pub max_pss: Option<f64>,
    /// Cumulative bytes read from disk by the process tree, in MiB.
    pub io_in: Option<f64>,
    /// Cumulative bytes written to disk by the process tree, in MiB.
    pub io_out: Option<f64>,
    /// Average CPU load over the run, as percent of one core.
    pub mean_load: f64,
    /// Cumulative user + system CPU time across the process tree, in seconds.
    pub cpu_time: f64,
    /// Whether at least one sample successfully read OS resource counters.
    ///
    /// When `false` the TSV row is rendered with `NA` placeholders for every
    /// resource column, matching Snakemake's behavior for processes that exited
    /// before the first poll.
    pub data_collected: bool,
}

impl BenchmarkRecord {
    /// Render this record as a single TSV row using Snakemake's column order
    /// and value formatting (`%.4f` for `s`, `%.2f` for floats, `-` for `None`,
    /// `NA` across all resource columns when `data_collected == false`).
    #[must_use]
    pub fn to_tsv_row(&self) -> String {
        let mut out = String::with_capacity(96);
        write!(out, "{:.4}\t{}", self.running_time, format_hms(self.running_time)).unwrap();

        if !self.data_collected {
            for _ in 0..8 {
                out.push_str("\tNA");
            }
            return out;
        }

        for value in
            [self.max_rss, self.max_vms, self.max_uss, self.max_pss, self.io_in, self.io_out]
        {
            out.push('\t');
            out.push_str(&format_optional_float(value));
        }
        write!(out, "\t{:.2}\t{:.2}", self.mean_load, self.cpu_time).unwrap();
        out
    }

    /// Render this record as a complete TSV document (header + single data row,
    /// trailing newline).
    #[must_use]
    pub fn to_tsv_document(&self) -> String {
        let mut out = String::with_capacity(192);
        out.push_str(TSV_HEADER);
        out.push('\n');
        out.push_str(&self.to_tsv_row());
        out.push('\n');
        out
    }

    /// Serialize this record as a JSON object string.
    ///
    /// # Errors
    /// Returns an error only if `serde_json` itself fails (which should not
    /// happen for this struct).
    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string(self)
    }

    /// Pretty one-line summary suitable for printing to stderr after a run.
    #[must_use]
    pub fn summary_line(&self) -> String {
        let mb = |x: Option<f64>| match x {
            Some(v) => format!("{v:.1}MiB"),
            None => "-".to_string(),
        };
        format!(
            "wall={:.2}s cpu={:.2}s mean_load={:.0}% max_rss={} max_uss={} io_in={} io_out={}",
            self.running_time,
            self.cpu_time,
            self.mean_load,
            mb(self.max_rss),
            mb(self.max_uss),
            mb(self.io_in),
            mb(self.io_out),
        )
    }
}

fn format_optional_float(value: Option<f64>) -> String {
    match value {
        Some(v) => format!("{v:.2}"),
        None => "-".to_string(),
    }
}

/// Format `seconds` as `H:MM:SS` (or `N day(s), H:MM:SS` past 24 hours),
/// matching Python's `str(datetime.timedelta(seconds=...))` truncated to
/// integer seconds.
#[must_use]
pub fn format_hms(seconds: f64) -> String {
    let total = seconds.max(0.0) as u64;
    let days = total / 86_400;
    let rem = total % 86_400;
    let hh = rem / 3600;
    let mm = (rem % 3600) / 60;
    let ss = rem % 60;
    let body = format!("{hh}:{mm:02}:{ss:02}");
    if days == 0 {
        body
    } else if days == 1 {
        format!("1 day, {body}")
    } else {
        format!("{days} days, {body}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_matches_snakemake() {
        assert_eq!(
            TSV_HEADER,
            "s\th:m:s\tmax_rss\tmax_vms\tmax_uss\tmax_pss\tio_in\tio_out\tmean_load\tcpu_time"
        );
    }

    #[test]
    fn hms_zero() {
        assert_eq!(format_hms(0.0), "0:00:00");
    }

    #[test]
    fn hms_seconds() {
        assert_eq!(format_hms(7.9), "0:00:07");
        assert_eq!(format_hms(59.0), "0:00:59");
    }

    #[test]
    fn hms_minutes_hours() {
        assert_eq!(format_hms(60.0), "0:01:00");
        assert_eq!(format_hms(3661.0), "1:01:01");
    }

    #[test]
    fn hms_days_singular_and_plural() {
        assert_eq!(format_hms(86_400.0), "1 day, 0:00:00");
        assert_eq!(format_hms(86_400.0 * 2.0 + 3661.0), "2 days, 1:01:01");
    }

    #[test]
    fn tsv_row_no_data_uses_na_placeholders() {
        let record =
            BenchmarkRecord { running_time: 0.1234, data_collected: false, ..Default::default() };
        assert_eq!(record.to_tsv_row(), "0.1234\t0:00:00\tNA\tNA\tNA\tNA\tNA\tNA\tNA\tNA");
    }

    #[test]
    fn tsv_row_full_data() {
        let record = BenchmarkRecord {
            running_time: 12.3456,
            max_rss: Some(101.5),
            max_vms: Some(2048.0),
            max_uss: Some(95.2),
            max_pss: Some(96.0),
            io_in: Some(1.25),
            io_out: Some(0.5),
            mean_load: 175.0,
            cpu_time: 21.6,
            data_collected: true,
        };
        assert_eq!(
            record.to_tsv_row(),
            "12.3456\t0:00:12\t101.50\t2048.00\t95.20\t96.00\t1.25\t0.50\t175.00\t21.60"
        );
    }

    #[test]
    fn tsv_row_missing_io_renders_dash() {
        let record = BenchmarkRecord {
            running_time: 1.0,
            max_rss: Some(10.0),
            max_vms: Some(20.0),
            max_uss: Some(8.0),
            max_pss: None,
            io_in: None,
            io_out: None,
            mean_load: 0.0,
            cpu_time: 0.0,
            data_collected: true,
        };
        assert_eq!(record.to_tsv_row(), "1.0000\t0:00:01\t10.00\t20.00\t8.00\t-\t-\t-\t0.00\t0.00");
    }

    #[test]
    fn tsv_document_has_header_and_row() {
        let record =
            BenchmarkRecord { running_time: 0.5, data_collected: false, ..Default::default() };
        let doc = record.to_tsv_document();
        let mut lines = doc.lines();
        assert_eq!(lines.next(), Some(TSV_HEADER));
        assert!(lines.next().is_some_and(|line| line.starts_with("0.5000\t")));
        assert_eq!(lines.next(), None);
        assert!(doc.ends_with('\n'));
    }

    #[test]
    fn trace_header_lists_per_tick_columns() {
        assert_eq!(TRACE_TSV_HEADER, "s\trss\tvms\tuss\tpss\tio_in\tio_out\tcpu_time\tn_procs");
    }

    #[test]
    fn tick_row_full_data() {
        let tick = TickRecord {
            elapsed: 0.5012,
            rss: 102.30,
            vms: 2048.0,
            uss: Some(95.2),
            pss: Some(96.0),
            io_in: Some(1.25),
            io_out: Some(0.5),
            cpu_time: 0.75,
            n_procs: 3,
        };
        assert_eq!(tick.to_tsv_row(), "0.5012\t102.30\t2048.00\t95.20\t96.00\t1.25\t0.50\t0.75\t3");
    }

    #[test]
    fn tick_row_missing_optionals_render_as_dash() {
        let tick = TickRecord {
            elapsed: 1.0,
            rss: 10.0,
            vms: 20.0,
            uss: None,
            pss: None,
            io_in: None,
            io_out: None,
            cpu_time: 0.0,
            n_procs: 1,
        };
        assert_eq!(tick.to_tsv_row(), "1.0000\t10.00\t20.00\t-\t-\t-\t-\t0.00\t1");
    }

    #[test]
    fn json_round_trip_preserves_fields() {
        let record = BenchmarkRecord {
            running_time: 1.5,
            max_rss: Some(42.0),
            max_pss: None,
            data_collected: true,
            ..Default::default()
        };
        let json = record.to_json().unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["running_time"], 1.5);
        assert_eq!(v["max_rss"], 42.0);
        assert!(v["max_pss"].is_null());
        assert_eq!(v["data_collected"], true);
    }
}
