//! Output writers: TSV (Snakemake-compatible), JSON, and a stderr summary.

use std::{
    fs::File,
    io::{self, BufWriter, Write},
    path::Path,
};

use crate::record::BenchmarkRecord;

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

/// Serialize `record` to `path` in the requested format. Creates parent
/// directories if needed; overwrites existing files.
///
/// # Errors
/// Returns any I/O error from the file system or serialization layer.
pub fn write_to_path(
    record: &BenchmarkRecord,
    path: &Path,
    format: OutputFormat,
) -> io::Result<()> {
    ensure_parent_dir(path)?;
    let file = File::create(path)?;
    let mut writer = BufWriter::new(file);
    write_to(record, &mut writer, format)?;
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
) -> io::Result<()> {
    match format {
        OutputFormat::Tsv => writer.write_all(record.to_tsv_document().as_bytes()),
        OutputFormat::Json => {
            let json = record.to_json().map_err(io::Error::other)?;
            writer.write_all(json.as_bytes())?;
            writer.write_all(b"\n")
        }
    }
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
            data_collected: true,
        }
    }

    #[test]
    fn tsv_writer_emits_header_and_data_row() {
        let mut buf = Vec::new();
        write_to(&sample_record(), &mut buf, OutputFormat::Tsv).unwrap();
        let text = std::str::from_utf8(&buf).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("s\th:m:s\t"));
        assert!(lines[1].starts_with("0.5000\t"));
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn json_writer_emits_one_object_per_line() {
        let mut buf = Vec::new();
        write_to(&sample_record(), &mut buf, OutputFormat::Json).unwrap();
        let text = std::str::from_utf8(&buf).unwrap().trim_end();
        let value: serde_json::Value = serde_json::from_str(text).unwrap();
        assert!(value.is_object());
        assert_eq!(value["data_collected"], true);
    }

    #[test]
    fn write_to_path_creates_parent_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested/deeper/timing.tsv");
        write_to_path(&sample_record(), &path, OutputFormat::Tsv).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("s\th:m:s"));
    }
}
