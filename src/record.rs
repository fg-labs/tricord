//! The aggregate benchmark record.
//!
//! Mirrors `snakemake.benchmark.BenchmarkRecord` so that downstream tools that
//! parse Snakemake's `benchmark:` directive output can ingest our TSV unchanged.

use std::fmt::Write as _;

use serde::Serialize;

/// Tab-separated aggregate header, **identical to Snakemake's**. Selected by
/// [`SchemaMode::SnakemakeStrict`] — emitted when the user passes
/// `--snakemake` to opt out of `tricord`-specific column additions.
pub const TSV_HEADER: &str =
    "s\th:m:s\tmax_rss\tmax_vms\tmax_uss\tmax_pss\tio_in\tio_out\tmean_load\tcpu_time";

/// Tab-separated aggregate header in full mode: [`TSV_HEADER`] plus every
/// column `tricord` has added on top of the Snakemake schema. Selected by
/// [`SchemaMode::Full`] (the default).
pub const TSV_HEADER_FULL: &str = "s\th:m:s\tmax_rss\tmax_vms\tmax_uss\tmax_pss\t\
                                   io_in\tio_out\tmean_load\tcpu_time\t\
                                   major_page_faults\tminor_page_faults\t\
                                   voluntary_ctx_switches\tinvoluntary_ctx_switches";

/// Return the appropriate aggregate header for the requested schema mode.
#[must_use]
pub fn tsv_header(mode: SchemaMode) -> &'static str {
    match mode {
        SchemaMode::Full => TSV_HEADER_FULL,
        SchemaMode::SnakemakeStrict => TSV_HEADER,
    }
}

/// Tab-separated header for the per-tick trace TSV (`--trace`). The trace is
/// `tricord`-native (not Snakemake-derived) and so is not affected by
/// [`SchemaMode`] — it always includes every column.
pub const TRACE_TSV_HEADER: &str = "s\trss\tvms\tuss\tpss\tio_in\tio_out\tcpu_time\tn_procs\t\
                                    major_page_faults\tminor_page_faults\t\
                                    voluntary_ctx_switches\tinvoluntary_ctx_switches";

/// Aggregate-output schema selector.
///
/// All `BenchmarkRecord` formatters (`to_tsv_*`, `to_json`, `to_markdown_*`)
/// take a `SchemaMode`. Snakemake-strict mode emits only the original 10-column
/// schema; full mode emits everything `tricord` collects. The per-tick trace
/// is unaffected — `SchemaMode` is purely about the aggregate record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SchemaMode {
    /// Full schema — `tricord`'s superset of the Snakemake columns. The
    /// default for CLI users and library consumers.
    #[default]
    Full,
    /// Snakemake-strict — only the original 10 columns of
    /// `snakemake.benchmark.write_benchmark_records(extended_fmt=False)`.
    /// Selected by `--snakemake` on the CLI; useful when downstream tooling
    /// pins to the Snakemake schema.
    SnakemakeStrict,
}

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
    /// Major page faults that occurred *during this tick*, summed across the
    /// observed PIDs (delta from the previous observation, not cumulative).
    /// `None` if no process exposed major-fault counters this tick.
    pub major_page_faults: Option<u64>,
    /// Minor page faults that occurred *during this tick*, summed across the
    /// observed PIDs (delta from the previous observation). `None` if no
    /// process exposed minor-fault counters this tick — always `None` on
    /// macOS where the kernel does not expose minor faults via
    /// `proc_pid_rusage`.
    pub minor_page_faults: Option<u64>,
    /// Voluntary context switches that occurred *during this tick*, summed
    /// across the observed PIDs (delta from the previous observation).
    /// `None` if no process exposed counters this tick — always `None` on
    /// macOS (`proc_pid_rusage` does not split context switches).
    pub voluntary_ctx_switches: Option<u64>,
    /// Involuntary context switches that occurred *during this tick*, summed
    /// across the observed PIDs (delta from the previous observation).
    /// `None` if no process exposed counters this tick — always `None` on
    /// macOS.
    pub involuntary_ctx_switches: Option<u64>,
}

impl TickRecord {
    /// Render this tick as a single TSV row using `%.4f` for elapsed time,
    /// `%.2f` for floats, `-` for missing optional values, and a bare integer
    /// for `n_procs` and page-fault counts.
    #[must_use]
    pub fn to_tsv_row(&self) -> String {
        let mut out = String::with_capacity(128);
        write!(out, "{:.4}\t{:.2}\t{:.2}", self.elapsed, self.rss, self.vms).unwrap();
        for value in [self.uss, self.pss, self.io_in, self.io_out] {
            out.push('\t');
            out.push_str(&format_optional_float(value));
        }
        write!(out, "\t{:.2}\t{}", self.cpu_time, self.n_procs).unwrap();
        for value in [
            self.major_page_faults,
            self.minor_page_faults,
            self.voluntary_ctx_switches,
            self.involuntary_ctx_switches,
        ] {
            out.push('\t');
            out.push_str(&format_optional_u64(value));
        }
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
    /// Total major page faults observed across the process tree (summed
    /// per-PID maximum, mirroring CPU/I/O aggregation). `None` if no process
    /// ever exposed major-fault counters.
    pub major_page_faults: Option<u64>,
    /// Total minor page faults observed across the process tree (summed
    /// per-PID maximum). `None` if no process exposed minor-fault counters —
    /// always `None` on macOS where the kernel does not expose minor faults
    /// via `proc_pid_rusage`.
    pub minor_page_faults: Option<u64>,
    /// Total voluntary context switches observed across the process tree
    /// (summed per-PID maximum). `None` on macOS — `proc_pid_rusage` does
    /// not split context switches voluntary vs involuntary.
    pub voluntary_ctx_switches: Option<u64>,
    /// Total involuntary context switches observed across the process tree
    /// (summed per-PID maximum). `None` on macOS.
    pub involuntary_ctx_switches: Option<u64>,
    /// Whether at least one sample successfully read OS resource counters.
    ///
    /// When `false` the TSV row is rendered with `NA` placeholders for every
    /// resource column, matching Snakemake's behavior for processes that exited
    /// before the first poll.
    pub data_collected: bool,
}

impl BenchmarkRecord {
    /// Render this record as a single TSV row.
    ///
    /// Snakemake's column order and value formatting (`%.4f` for `s`, `%.2f`
    /// for floats, `-` for `None`, `NA` across all resource columns when
    /// `data_collected == false`) applies in both modes. In `Full` mode the
    /// `tricord`-added columns are appended on the right; page-fault counts
    /// render as bare integers (or `-` when missing, `NA` when no data).
    #[must_use]
    pub fn to_tsv_row(&self, mode: SchemaMode) -> String {
        let mut out = String::with_capacity(128);
        write!(out, "{:.4}\t{}", self.running_time, format_hms(self.running_time)).unwrap();

        let extra_cols = match mode {
            // page faults (2) + ctx switches (2)
            SchemaMode::Full => 4,
            SchemaMode::SnakemakeStrict => 0,
        };

        if !self.data_collected {
            for _ in 0..(8 + extra_cols) {
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
        if mode == SchemaMode::Full {
            for value in [
                self.major_page_faults,
                self.minor_page_faults,
                self.voluntary_ctx_switches,
                self.involuntary_ctx_switches,
            ] {
                out.push('\t');
                out.push_str(&format_optional_u64(value));
            }
        }
        out
    }

    /// Render this record as a complete TSV document (header + single data row,
    /// trailing newline). The header matches `mode` — strict mode emits
    /// [`TSV_HEADER`]; full mode emits [`TSV_HEADER_FULL`].
    #[must_use]
    pub fn to_tsv_document(&self, mode: SchemaMode) -> String {
        let mut out = String::with_capacity(256);
        out.push_str(tsv_header(mode));
        out.push('\n');
        out.push_str(&self.to_tsv_row(mode));
        out.push('\n');
        out
    }

    /// Serialize this record as a JSON object string.
    ///
    /// In `Full` mode every field of the struct is serialized. In
    /// `SnakemakeStrict` mode only the original 10 Snakemake fields plus
    /// `data_collected` are emitted; `tricord`-added fields are omitted from
    /// the object entirely (not set to `null`).
    ///
    /// # Errors
    /// Returns an error only if `serde_json` itself fails (which should not
    /// happen for this struct).
    pub fn to_json(&self, mode: SchemaMode) -> serde_json::Result<String> {
        match mode {
            SchemaMode::Full => serde_json::to_string(self),
            SchemaMode::SnakemakeStrict => serde_json::to_string(&SnakemakeView::from(self)),
        }
    }

    /// Render this record as a Markdown table (two columns: `metric | value`).
    ///
    /// Uses the same column order and value formatting as `to_tsv_row` and
    /// `TSV_HEADER`: `%.4f` for `s`, `%.2f` for floats, `-` for missing
    /// optional values, and `NA` across all resource cells when
    /// `data_collected == false`. Column widths auto-fit the longest cell so
    /// the table stays compact in pasted PR/issue output.
    ///
    /// In `SnakemakeStrict` mode the `tricord`-added rows are omitted —
    /// the table holds only the original 10 Snakemake metric rows.
    #[must_use]
    pub fn to_markdown_document(&self, mode: SchemaMode) -> String {
        let rows = self.markdown_rows(mode);
        let metric_w = rows.iter().map(|(m, _)| m.len()).max().unwrap_or(0).max("metric".len());
        let value_w = rows.iter().map(|(_, v)| v.len()).max().unwrap_or(0).max("value".len());

        let mut out = String::with_capacity(256);
        writeln!(out, "| {:<metric_w$} | {:>value_w$} |", "metric", "value").unwrap();
        writeln!(out, "|:{}|{}:|", "-".repeat(metric_w + 1), "-".repeat(value_w + 1)).unwrap();
        for (metric, value) in &rows {
            writeln!(out, "| {metric:<metric_w$} | {value:>value_w$} |").unwrap();
        }
        out
    }

    /// Build the ordered `(metric_label, value_string)` pairs that drive the
    /// Markdown table. The list lives here as a single authoritative column
    /// order for the Markdown formatter; `to_tsv_row` keeps its own
    /// independent implementation, so both must be updated together when new
    /// metrics land.
    fn markdown_rows(&self, mode: SchemaMode) -> Vec<(&'static str, String)> {
        let cell = |value: Option<f64>| {
            if self.data_collected { format_optional_float(value) } else { "NA".to_string() }
        };
        let scalar = |value: f64| {
            if self.data_collected { format!("{value:.2}") } else { "NA".to_string() }
        };
        let int_cell = |value: Option<u64>| {
            if self.data_collected { format_optional_u64(value) } else { "NA".to_string() }
        };
        let mut rows = vec![
            ("s", format!("{:.4}", self.running_time)),
            ("h:m:s", format_hms(self.running_time)),
            ("max_rss", cell(self.max_rss)),
            ("max_vms", cell(self.max_vms)),
            ("max_uss", cell(self.max_uss)),
            ("max_pss", cell(self.max_pss)),
            ("io_in", cell(self.io_in)),
            ("io_out", cell(self.io_out)),
            ("mean_load", scalar(self.mean_load)),
            ("cpu_time", scalar(self.cpu_time)),
        ];
        if mode == SchemaMode::Full {
            rows.push(("major_page_faults", int_cell(self.major_page_faults)));
            rows.push(("minor_page_faults", int_cell(self.minor_page_faults)));
            rows.push(("voluntary_ctx_switches", int_cell(self.voluntary_ctx_switches)));
            rows.push(("involuntary_ctx_switches", int_cell(self.involuntary_ctx_switches)));
        }
        rows
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

/// Render an `Option<u64>` cell — bare integer for `Some`, `-` for `None`.
/// Used for page-fault columns where decimals would be misleading.
fn format_optional_u64(value: Option<u64>) -> String {
    match value {
        Some(v) => v.to_string(),
        None => "-".to_string(),
    }
}

/// Strict-mode JSON projection — only the fields in the original Snakemake
/// schema, by borrowed reference so we don't have to copy the record.
///
/// Kept in sync by review pressure: every `tricord`-added field on
/// [`BenchmarkRecord`] must be deliberately *absent* from this struct.
#[derive(Serialize)]
struct SnakemakeView<'a> {
    running_time: f64,
    max_rss: &'a Option<f64>,
    max_vms: &'a Option<f64>,
    max_uss: &'a Option<f64>,
    max_pss: &'a Option<f64>,
    io_in: &'a Option<f64>,
    io_out: &'a Option<f64>,
    mean_load: f64,
    cpu_time: f64,
    data_collected: bool,
}

impl<'a> From<&'a BenchmarkRecord> for SnakemakeView<'a> {
    fn from(r: &'a BenchmarkRecord) -> Self {
        Self {
            running_time: r.running_time,
            max_rss: &r.max_rss,
            max_vms: &r.max_vms,
            max_uss: &r.max_uss,
            max_pss: &r.max_pss,
            io_in: &r.io_in,
            io_out: &r.io_out,
            mean_load: r.mean_load,
            cpu_time: r.cpu_time,
            data_collected: r.data_collected,
        }
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

    fn full_record() -> BenchmarkRecord {
        BenchmarkRecord {
            running_time: 12.3456,
            max_rss: Some(101.5),
            max_vms: Some(2048.0),
            max_uss: Some(95.2),
            max_pss: Some(96.0),
            io_in: Some(1.25),
            io_out: Some(0.5),
            mean_load: 175.0,
            cpu_time: 21.6,
            major_page_faults: Some(42),
            minor_page_faults: Some(1234),
            voluntary_ctx_switches: Some(80),
            involuntary_ctx_switches: Some(7),
            data_collected: true,
        }
    }

    #[test]
    fn header_matches_snakemake() {
        assert_eq!(
            TSV_HEADER,
            "s\th:m:s\tmax_rss\tmax_vms\tmax_uss\tmax_pss\tio_in\tio_out\tmean_load\tcpu_time"
        );
    }

    #[test]
    fn header_full_appends_tricord_columns() {
        assert!(TSV_HEADER_FULL.starts_with(TSV_HEADER));
        assert!(TSV_HEADER_FULL.ends_with("\tinvoluntary_ctx_switches"));
        // No reordering of the snakemake prefix.
        let strict_cols: Vec<&str> = TSV_HEADER.split('\t').collect();
        let full_cols: Vec<&str> = TSV_HEADER_FULL.split('\t').collect();
        assert_eq!(&full_cols[..strict_cols.len()], &strict_cols[..]);
        // All four tricord-added columns are present.
        for col in [
            "major_page_faults",
            "minor_page_faults",
            "voluntary_ctx_switches",
            "involuntary_ctx_switches",
        ] {
            assert!(full_cols.contains(&col), "missing {col} in {TSV_HEADER_FULL}");
        }
    }

    #[test]
    fn tsv_header_dispatches_on_mode() {
        assert_eq!(tsv_header(SchemaMode::SnakemakeStrict), TSV_HEADER);
        assert_eq!(tsv_header(SchemaMode::Full), TSV_HEADER_FULL);
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
    fn tsv_row_no_data_strict_has_10_nas() {
        let record =
            BenchmarkRecord { running_time: 0.1234, data_collected: false, ..Default::default() };
        assert_eq!(
            record.to_tsv_row(SchemaMode::SnakemakeStrict),
            "0.1234\t0:00:00\tNA\tNA\tNA\tNA\tNA\tNA\tNA\tNA"
        );
    }

    #[test]
    fn tsv_row_no_data_full_has_one_na_per_full_column() {
        // Full mode emits one NA per resource column (everything but `s` and
        // `h:m:s`). Auto-derived from TSV_HEADER_FULL so future appended
        // columns extend it automatically.
        let record =
            BenchmarkRecord { running_time: 0.1234, data_collected: false, ..Default::default() };
        let total_cols = TSV_HEADER_FULL.split('\t').count();
        let row = record.to_tsv_row(SchemaMode::Full);
        assert!(row.starts_with("0.1234\t0:00:00\t"));
        assert_eq!(row.split('\t').count(), total_cols);
        assert_eq!(row.split('\t').filter(|c| *c == "NA").count(), total_cols - 2);
    }

    #[test]
    fn tsv_row_full_mode_appends_all_tricord_columns() {
        assert_eq!(
            full_record().to_tsv_row(SchemaMode::Full),
            "12.3456\t0:00:12\t101.50\t2048.00\t95.20\t96.00\t1.25\t0.50\t175.00\t21.60\t\
             42\t1234\t80\t7",
        );
    }

    #[test]
    fn tsv_row_strict_mode_omits_tricord_columns() {
        // Same record, snakemake-strict mode: drop every tricord-added column.
        assert_eq!(
            full_record().to_tsv_row(SchemaMode::SnakemakeStrict),
            "12.3456\t0:00:12\t101.50\t2048.00\t95.20\t96.00\t1.25\t0.50\t175.00\t21.60"
        );
    }

    #[test]
    fn tsv_row_full_mode_renders_missing_tricord_metrics_as_dash() {
        let record = BenchmarkRecord {
            major_page_faults: None,
            minor_page_faults: None,
            voluntary_ctx_switches: None,
            involuntary_ctx_switches: None,
            ..full_record()
        };
        assert!(
            record.to_tsv_row(SchemaMode::Full).ends_with("\t-\t-\t-\t-"),
            "row was: {}",
            record.to_tsv_row(SchemaMode::Full),
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
            ..Default::default()
        };
        assert_eq!(
            record.to_tsv_row(SchemaMode::SnakemakeStrict),
            "1.0000\t0:00:01\t10.00\t20.00\t8.00\t-\t-\t-\t0.00\t0.00"
        );
    }

    #[test]
    fn tsv_document_strict_uses_snakemake_header() {
        let record =
            BenchmarkRecord { running_time: 0.5, data_collected: false, ..Default::default() };
        let doc = record.to_tsv_document(SchemaMode::SnakemakeStrict);
        let mut lines = doc.lines();
        assert_eq!(lines.next(), Some(TSV_HEADER));
        assert!(lines.next().is_some_and(|line| line.starts_with("0.5000\t")));
        assert_eq!(lines.next(), None);
        assert!(doc.ends_with('\n'));
    }

    #[test]
    fn tsv_document_full_uses_full_header() {
        let record =
            BenchmarkRecord { running_time: 0.5, data_collected: false, ..Default::default() };
        let doc = record.to_tsv_document(SchemaMode::Full);
        let mut lines = doc.lines();
        assert_eq!(lines.next(), Some(TSV_HEADER_FULL));
        assert!(lines.next().is_some_and(|line| line.starts_with("0.5000\t")));
    }

    #[test]
    fn trace_header_lists_per_tick_columns_including_page_faults() {
        assert_eq!(
            TRACE_TSV_HEADER,
            "s\trss\tvms\tuss\tpss\tio_in\tio_out\tcpu_time\tn_procs\t\
             major_page_faults\tminor_page_faults\t\
             voluntary_ctx_switches\tinvoluntary_ctx_switches"
        );
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
            major_page_faults: Some(2),
            minor_page_faults: Some(150),
            voluntary_ctx_switches: Some(11),
            involuntary_ctx_switches: Some(3),
        };
        assert_eq!(
            tick.to_tsv_row(),
            "0.5012\t102.30\t2048.00\t95.20\t96.00\t1.25\t0.50\t0.75\t3\t2\t150\t11\t3"
        );
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
            major_page_faults: None,
            minor_page_faults: None,
            voluntary_ctx_switches: None,
            involuntary_ctx_switches: None,
        };
        assert_eq!(tick.to_tsv_row(), "1.0000\t10.00\t20.00\t-\t-\t-\t-\t0.00\t1\t-\t-\t-\t-");
    }

    #[test]
    fn markdown_full_data_exact_layout() {
        // Widest label is now "involuntary_ctx_switches" (24 chars), driving
        // the metric column width. Value widths are still 7 ("12.3456" /
        // "2048.00" / "0:00:12") — the new int values are shorter.
        //
        // When new metrics land (tracking issue #11 on fg-labs/tricord), this
        // golden block needs re-recording — both for the new rows and for any
        // width shift if a new label is longer.
        let expected = "\
| metric                   |   value |
|:-------------------------|--------:|
| s                        | 12.3456 |
| h:m:s                    | 0:00:12 |
| max_rss                  |  101.50 |
| max_vms                  | 2048.00 |
| max_uss                  |   95.20 |
| max_pss                  |   96.00 |
| io_in                    |    1.25 |
| io_out                   |    0.50 |
| mean_load                |  175.00 |
| cpu_time                 |   21.60 |
| major_page_faults        |      42 |
| minor_page_faults        |    1234 |
| voluntary_ctx_switches   |      80 |
| involuntary_ctx_switches |       7 |
";
        assert_eq!(full_record().to_markdown_document(SchemaMode::Full), expected);
    }

    #[test]
    fn markdown_strict_mode_drops_tricord_added_rows() {
        let doc = full_record().to_markdown_document(SchemaMode::SnakemakeStrict);
        assert!(!doc.contains("major_page_faults"), "strict doc must not list page faults: {doc}");
        assert!(!doc.contains("minor_page_faults"));
        // Header + alignment row + 10 metric rows.
        assert_eq!(doc.lines().count(), 12);
    }

    #[test]
    fn markdown_no_data_uses_na_in_resource_cells() {
        let record =
            BenchmarkRecord { running_time: 0.1234, data_collected: false, ..Default::default() };
        let doc = record.to_markdown_document(SchemaMode::Full);
        // Strict-prefix metrics: every resource row ends in NA. Page-fault
        // rows do too, since data_collected gates them.
        for metric in [
            "max_rss",
            "max_vms",
            "max_uss",
            "max_pss",
            "io_in",
            "io_out",
            "mean_load",
            "cpu_time",
            "major_page_faults",
            "minor_page_faults",
        ] {
            let row = doc
                .lines()
                .find(|l| l.contains(metric))
                .unwrap_or_else(|| panic!("no row for {metric}"));
            assert!(row.ends_with("NA |"), "row for {metric}: {row}");
        }
    }

    #[test]
    fn markdown_missing_optionals_render_as_dash() {
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
            major_page_faults: None,
            minor_page_faults: None,
            voluntary_ctx_switches: None,
            involuntary_ctx_switches: None,
            data_collected: true,
        };
        let doc = record.to_markdown_document(SchemaMode::Full);
        for metric in [
            "max_pss",
            "io_in",
            "io_out",
            "major_page_faults",
            "minor_page_faults",
            "voluntary_ctx_switches",
            "involuntary_ctx_switches",
        ] {
            let row = doc.lines().find(|l| l.contains(metric)).expect("metric row");
            assert!(row.contains(" - "), "expected dash in {metric} row: {row}");
        }
    }

    #[test]
    fn markdown_full_row_count_tracks_tsv_header_full() {
        let record =
            BenchmarkRecord { running_time: 0.5, data_collected: false, ..Default::default() };
        let doc = record.to_markdown_document(SchemaMode::Full);
        // header + alignment + one data row per column in TSV_HEADER_FULL.
        let expected_rows = 2 + TSV_HEADER_FULL.split('\t').count();
        assert_eq!(doc.lines().count(), expected_rows);
    }

    #[test]
    fn json_full_mode_preserves_fields() {
        let record = BenchmarkRecord {
            running_time: 1.5,
            max_rss: Some(42.0),
            max_pss: None,
            major_page_faults: Some(7),
            data_collected: true,
            ..Default::default()
        };
        let json = record.to_json(SchemaMode::Full).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["running_time"], 1.5);
        assert_eq!(v["max_rss"], 42.0);
        assert!(v["max_pss"].is_null());
        assert_eq!(v["data_collected"], true);
        assert_eq!(v["major_page_faults"], 7);
        assert!(v["minor_page_faults"].is_null());
    }

    #[test]
    fn json_strict_mode_omits_tricord_added_fields() {
        let json = full_record().to_json(SchemaMode::SnakemakeStrict).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = v.as_object().expect("object");
        assert!(!obj.contains_key("major_page_faults"), "strict json: {json}");
        assert!(!obj.contains_key("minor_page_faults"));
        // 9 snakemake-numeric fields (running_time + 8 metrics) +
        // data_collected = 10 keys total.
        assert_eq!(obj.len(), 10, "unexpected key set: {obj:?}");
    }
}
