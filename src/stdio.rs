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

/// Replace pipe/socket stdin and stdout with `/dev/null`, leaving stderr intact.
pub(crate) fn release_pipes() -> io::Result<()> {
    let stdin_is_pipe = is_pipe_or_socket(libc::STDIN_FILENO)?;
    let stdout_is_pipe = is_pipe_or_socket(libc::STDOUT_FILENO)?;
    if !stdin_is_pipe && !stdout_is_pipe {
        return Ok(());
    }

    // Preserve bytes accepted before this process-wide stream transition.
    // Keep the lock through dup2 so other Rust stdout writers cannot intervene.
    let mut stdout = io::stdout().lock();
    if stdout_is_pipe {
        stdout.flush()?;
    }
    // Selected targets are open, so open() cannot reuse any of their numbers.
    let source = OpenOptions::new().read(true).write(true).open("/dev/null")?;

    for (target, replace) in
        [(libc::STDIN_FILENO, stdin_is_pipe), (libc::STDOUT_FILENO, stdout_is_pipe)]
    {
        if replace {
            loop {
                // SAFETY: source stays alive; target is a standard descriptor
                // intentionally replaced process-wide after the child spawns.
                if unsafe { libc::dup2(source.as_raw_fd(), target) } != -1 {
                    break;
                }
                let err = io::Error::last_os_error();
                if err.kind() != io::ErrorKind::Interrupted {
                    return Err(err);
                }
            }
        }
    }
    Ok(())
}

/// Reject outputs resolving to stdin/stdout streams that will be released.
pub(crate) fn validate_output_paths(options: &crate::run::RunOptions) -> io::Result<()> {
    let mut streams = Vec::new();
    for fd in [libc::STDIN_FILENO, libc::STDOUT_FILENO] {
        if is_pipe_or_socket(fd)? {
            streams.push(descriptor_stat(fd)?);
        }
    }
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
