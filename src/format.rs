//! Output writers: TSV (Snakemake-compatible), JSON, and a stderr summary.

use std::{
    fs::File,
    io::{self, BufWriter, Write},
    path::Path,
};

use crate::record::{BenchmarkRecord, SchemaMode};

/// One of the supported on-disk output formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// Snakemake-format TSV: a header row plus one data row.
    Tsv,
    /// Single JSON object, one line, no trailing newline.
    Json,
}

impl OutputFormat {
    /// File-extension hint for help text and downstream tooling.
    #[must_use]
    pub fn extension(self) -> &'static str {
        match self {
            Self::Tsv => "tsv",
            Self::Json => "json",
        }
    }
}

/// Serialize `record` to `path` in the requested format and schema mode.
/// Creates parent directories if needed; overwrites existing files.
///
/// # Errors
/// Returns any I/O error from the file system or serialization layer.
pub fn write_to_path(
    record: &BenchmarkRecord,
    path: &Path,
    format: OutputFormat,
    mode: SchemaMode,
) -> io::Result<()> {
    ensure_parent_dir(path)?;
    let file = File::create(path)?;
    let mut writer = BufWriter::new(file);
    write_to(record, &mut writer, format, mode)?;
    // Surface flush errors instead of letting BufWriter::drop swallow them.
    writer.flush()
}

/// Create the parent directory of `path` if it has one and isn't empty.
/// Used by both the aggregate output writer and the trace writer.
///
/// # Errors
/// Returns any I/O error from `create_dir_all`.
pub(crate) fn ensure_parent_dir(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

/// Serialize `record` into the given writer.
///
/// # Errors
/// Returns any underlying I/O or JSON error.
pub fn write_to<W: Write>(
    record: &BenchmarkRecord,
    writer: &mut W,
    format: OutputFormat,
    mode: SchemaMode,
) -> io::Result<()> {
    match format {
        OutputFormat::Tsv => writer.write_all(record.to_tsv_document(mode).as_bytes()),
        OutputFormat::Json => {
            let json = record.to_json(mode).map_err(io::Error::other)?;
            writer.write_all(json.as_bytes())?;
            writer.write_all(b"\n")
        }
    }
}

/// Serialize `record` as a Markdown table to `path`. Creates parent
/// directories if needed; overwrites existing files.
///
/// Lives alongside [`write_to_path`] but takes no `OutputFormat`: Markdown is
/// a sidecar output (`--export-markdown`) that can be requested alongside the
/// primary `--out`/`--format` file, not a third value of `--format`.
///
/// # Errors
/// Returns any I/O error from the file system.
pub fn write_markdown_to_path(
    record: &BenchmarkRecord,
    path: &Path,
    mode: SchemaMode,
) -> io::Result<()> {
    ensure_parent_dir(path)?;
    let file = File::create(path)?;
    let mut writer = BufWriter::new(file);
    writer.write_all(record.to_markdown_document(mode).as_bytes())?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_record() -> BenchmarkRecord {
        BenchmarkRecord {
            running_time: 0.5,
            max_rss: Some(8.0),
            max_vms: Some(64.0),
            max_uss: Some(7.0),
            max_pss: Some(7.5),
            io_in: Some(1.0),
            io_out: Some(0.25),
            mean_load: 100.0,
            cpu_time: 0.5,
            major_page_faults: Some(3),
            minor_page_faults: Some(120),
            voluntary_ctx_switches: Some(40),
            involuntary_ctx_switches: Some(5),
            peak_n_threads: Some(6),
            peak_n_procs: 2,
            loadavg_1m_start: Some(0.75),
            loadavg_1m_end: Some(1.10),
            max_swap: Some(0.25),
            page_cache_start: Some(256.0),
            page_cache_end: Some(240.0),
            data_collected: true,
        }
    }

    #[test]
    fn tsv_writer_full_mode_emits_full_header_and_data_row() {
        let mut buf = Vec::new();
        write_to(&sample_record(), &mut buf, OutputFormat::Tsv, SchemaMode::Full).unwrap();
        let text = std::str::from_utf8(&buf).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\tmajor_page_faults\tminor_page_faults\t"));
        assert!(lines[0].contains("\tvoluntary_ctx_switches\tinvoluntary_ctx_switches\t"));
        assert!(lines[0].contains("\tpeak_n_threads\tpeak_n_procs\t"));
        assert!(lines[0].contains("\tloadavg_1m_start\tloadavg_1m_end\t"));
        assert!(lines[0].contains("\tmax_swap\t"));
        assert!(lines[0].ends_with("\tpage_cache_start\tpage_cache_end"));
        assert!(lines[1].starts_with("0.5000\t"));
        // sample_record(): major=3 minor=120 voluntary=40 involuntary=5
        //                  peak_n_threads=6 peak_n_procs=2 loadavg 0.75 → 1.10
        //                  max_swap=0.25 page_cache 256.0 → 240.0
        assert!(
            lines[1].ends_with("\t3\t120\t40\t5\t6\t2\t0.75\t1.10\t0.25\t256.00\t240.00"),
            "data row: {}",
            lines[1]
        );
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn tsv_writer_strict_mode_emits_snakemake_header() {
        let mut buf = Vec::new();
        write_to(&sample_record(), &mut buf, OutputFormat::Tsv, SchemaMode::SnakemakeStrict)
            .unwrap();
        let text = std::str::from_utf8(&buf).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], crate::record::TSV_HEADER);
        // Data row has exactly 10 tab-separated columns.
        assert_eq!(lines[1].split('\t').count(), 10);
    }

    #[test]
    fn json_writer_full_mode_includes_tricord_fields() {
        let mut buf = Vec::new();
        write_to(&sample_record(), &mut buf, OutputFormat::Json, SchemaMode::Full).unwrap();
        let text = std::str::from_utf8(&buf).unwrap().trim_end();
        let value: serde_json::Value = serde_json::from_str(text).unwrap();
        assert!(value.is_object());
        assert_eq!(value["data_collected"], true);
        assert_eq!(value["major_page_faults"], 3);
        assert_eq!(value["minor_page_faults"], 120);
    }

    #[test]
    fn json_writer_strict_mode_omits_tricord_fields() {
        let mut buf = Vec::new();
        write_to(&sample_record(), &mut buf, OutputFormat::Json, SchemaMode::SnakemakeStrict)
            .unwrap();
        let text = std::str::from_utf8(&buf).unwrap().trim_end();
        let value: serde_json::Value = serde_json::from_str(text).unwrap();
        let obj = value.as_object().unwrap();
        assert!(!obj.contains_key("major_page_faults"));
        assert!(!obj.contains_key("minor_page_faults"));
    }

    #[test]
    fn write_to_path_creates_parent_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested/deeper/timing.tsv");
        write_to_path(&sample_record(), &path, OutputFormat::Tsv, SchemaMode::Full).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("s\th:m:s"));
    }

    #[test]
    fn write_markdown_to_path_emits_table_and_creates_parents() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested/deeper/timing.md");
        write_markdown_to_path(&sample_record(), &path, SchemaMode::Full).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let mut lines = text.lines();
        assert!(lines.next().is_some_and(|line| line.starts_with("| metric")));
        assert!(lines.next().is_some_and(|line| line.starts_with("|:")));
        // Spot-check the page-fault row added in full mode.
        assert!(text.contains("major_page_faults"), "missing page-fault row in: {text}");
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn write_markdown_to_path_strict_omits_page_faults() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("timing.md");
        write_markdown_to_path(&sample_record(), &path, SchemaMode::SnakemakeStrict).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("major_page_faults"));
    }
}
