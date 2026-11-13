//! Release parent-side pipeline descriptors without changing the child's streams.

use std::{
    fs::OpenOptions,
    io::{self, Write},
    mem::MaybeUninit,
    os::{
        fd::{AsRawFd, RawFd},
        unix::fs::MetadataExt,
    },
    path::Path,
};

/// Which inherited standard streams must be released after spawning.
///
/// Captured before tricord allocates any descriptor. A standard descriptor that
/// is closed on entry is vacant, and an internal descriptor (for example the
/// signal-forwarding socket) may later reuse its number. Re-probing after that
/// point would mistake tricord's own descriptor for an inherited stream.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ReleasePlan {
    stdin: bool,
    stdout: bool,
}

impl ReleasePlan {
    /// Classify stdin and stdout as they were inherited by the caller.
    pub(crate) fn capture() -> io::Result<Self> {
        Ok(Self {
            stdin: is_pipe_or_socket(libc::STDIN_FILENO)?,
            stdout: is_pipe_or_socket(libc::STDOUT_FILENO)?,
        })
    }

    fn targets(self) -> impl Iterator<Item = RawFd> {
        [(libc::STDIN_FILENO, self.stdin), (libc::STDOUT_FILENO, self.stdout)]
            .into_iter()
            .filter_map(|(fd, release)| release.then_some(fd))
    }
}

/// Replace planned pipe/socket stdin and stdout with `/dev/null`, leaving
/// stderr intact. Descriptors are never reclassified here; see [`ReleasePlan`].
pub(crate) fn release_pipes(plan: ReleasePlan) -> io::Result<()> {
    if !plan.stdin && !plan.stdout {
        return Ok(());
    }

    // Preserve bytes accepted before this process-wide stream transition.
    // Keep the lock through dup2 so other Rust stdout writers cannot intervene.
    let mut stdout = io::stdout().lock();
    if plan.stdout {
        stdout.flush()?;
    }
    // Selected targets are open, so open() cannot reuse any of their numbers.
    let source = OpenOptions::new().read(true).write(true).open("/dev/null")?;

    for target in plan.targets() {
        loop {
            // SAFETY: source stays alive; target is a standard descriptor that
            // was an inherited stream when the plan was captured, intentionally
            // replaced process-wide after the child spawns.
            if unsafe { libc::dup2(source.as_raw_fd(), target) } != -1 {
                break;
            }
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::Interrupted {
                return Err(err);
            }
        }
    }
    Ok(())
}

/// Reject outputs resolving to stdin/stdout streams that will be released.
pub(crate) fn validate_output_paths(
    options: &crate::run::RunOptions,
    plan: ReleasePlan,
) -> io::Result<()> {
    let streams = plan.targets().map(descriptor_stat).collect::<io::Result<Vec<_>>>()?;
    for (flag, path) in [
        ("--out", Some(options.output_path.as_ref())),
        ("--trace", options.trace_path.as_deref()),
        ("--export-markdown", options.markdown_path.as_deref()),
    ] {
        if let Some(path) = path {
            validate_output_path(flag, path, &streams)?;
        }
    }
    Ok(())
}

/// Compare filesystem identity, including symlinks and standard-fd aliases.
fn validate_output_path(flag: &str, path: &Path, streams: &[libc::stat]) -> io::Result<()> {
    // On macOS /dev/fd entries have devfs metadata, not the descriptor's
    // underlying pipe identity. Resolve standard-fd paths explicitly as well.
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let descriptor = match resolved.to_str() {
        Some("/dev/stdin") => Some(libc::STDIN_FILENO),
        Some("/dev/stdout") => Some(libc::STDOUT_FILENO),
        Some("/dev/stderr") => Some(libc::STDERR_FILENO),
        Some(value) => value.strip_prefix("/dev/fd/").and_then(|fd| fd.parse().ok()),
        None => None,
    };
    let aliases_stream = descriptor.and_then(|fd| descriptor_stat(fd).ok()).is_some_and(|target| {
        streams.iter().any(|stat| target.st_dev == stat.st_dev && target.st_ino == stat.st_ino)
    });
    let same_inode = std::fs::metadata(path).is_ok_and(|metadata| {
        streams.iter().any(|stat| {
            i128::from(metadata.dev()) == i128::from(stat.st_dev) && metadata.ino() == stat.st_ino
        })
    });
    if aliases_stream || same_inode {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{flag} points to a stdin/stdout pipe being released ({}); use a separate output file",
                path.display()
            ),
        ));
    }
    Ok(())
}

/// Read descriptor metadata without taking ownership of the descriptor.
fn descriptor_stat(fd: RawFd) -> io::Result<libc::stat> {
    let mut stat = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: stat points to writable space for one stat structure.
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fstat succeeded and initialized stat.
    Ok(unsafe { stat.assume_init() })
}

/// Identify streaming descriptors; already-closed standard descriptors are fine.
fn is_pipe_or_socket(fd: RawFd) -> io::Result<bool> {
    let stat = match descriptor_stat(fd) {
        Ok(stat) => stat,
        Err(err) if err.raw_os_error() == Some(libc::EBADF) => return Ok(false),
        Err(err) => return Err(err),
    };
    let kind = stat.st_mode & libc::S_IFMT;
    Ok(kind == libc::S_IFIFO || kind == libc::S_IFSOCK)
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        os::{fd::AsRawFd, unix::net::UnixStream},
        process::{Command, Stdio},
    };

    use super::{ReleasePlan, descriptor_stat, release_pipes};

    const HELPER: &str = "TRICORD_STDIO_FD_REUSE_HELPER";

    /// Closing a standard descriptor is process-wide, so run in a subprocess.
    #[test]
    fn release_ignores_internal_descriptor_reusing_closed_stdin() {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "stdio::tests::fd_reuse_helper", "--nocapture", "--quiet"])
            .env(HELPER, "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("spawn isolated helper");
        assert!(
            output.status.success(),
            "helper failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn fd_reuse_helper() {
        if std::env::var_os(HELPER).is_none() {
            return;
        }
        // SAFETY: only this isolated helper process closes its stdin.
        assert_eq!(unsafe { libc::close(libc::STDIN_FILENO) }, 0);
        let plan = ReleasePlan::capture().unwrap();
        assert!(!plan.stdin && plan.stdout, "plan: {plan:?}");

        // An internal descriptor allocated after capture takes the vacant number,
        // as signal-hook's socket pair does in run_command_releasing_pipes.
        let (mut internal, mut peer) = UnixStream::pair().unwrap();
        assert_eq!(internal.as_raw_fd(), libc::STDIN_FILENO);

        release_pipes(plan).unwrap();

        // The internal socket must survive; /dev/null in its place would read EOF.
        peer.write_all(b"x").unwrap();
        let mut byte = [0];
        internal.read_exact(&mut byte).expect("internal socket was replaced");
        assert_eq!(&byte, b"x");
        let stdout = descriptor_stat(libc::STDOUT_FILENO).unwrap();
        assert_eq!(stdout.st_mode & libc::S_IFMT, libc::S_IFCHR, "stdout must be /dev/null");
        // Avoid libtest writing to the released stdout.
        std::process::exit(0);
    }
}
