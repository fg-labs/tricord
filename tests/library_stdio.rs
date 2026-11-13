//! Library stdio contracts are exercised in subprocesses, never in the test runner.

use std::{
    io::{self, Read, Write},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use tricord::{OutputFormat, RunOptions, SchemaMode, run_command, run_command_releasing_pipes};

const HELPER_MODE: &str = "TRICORD_LIBRARY_STDIO_TEST_MODE";
const HELPER_DIR: &str = "TRICORD_LIBRARY_STDIO_TEST_DIR";
const TIMEOUT: Duration = Duration::from_secs(5);

fn wait_until<F: FnMut() -> bool>(timeout: Duration, mut condition: F) {
    let deadline = Instant::now() + timeout;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for subprocess handshake");
        thread::sleep(Duration::from_millis(10));
    }
}

struct Helper {
    child: Child,
    release: PathBuf,
}

impl Helper {
    fn spawn(mode: &str, directory: &Path) -> Self {
        let stdin = if mode == "regular" {
            Stdio::from(std::fs::File::open(directory.join("input")).unwrap())
        } else {
            Stdio::piped()
        };
        let stdout = if mode == "regular" {
            Stdio::from(std::fs::File::create(directory.join("output")).unwrap())
        } else {
            Stdio::piped()
        };
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "library_stdio_helper", "--nocapture", "--quiet"])
            .env(HELPER_MODE, mode)
            .env(HELPER_DIR, directory)
            .stdin(stdin)
            .stdout(stdout)
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn isolated library caller");
        Self { child, release: directory.join("release") }
    }

    fn finish(&mut self) {
        wait_until(TIMEOUT, || self.child.try_wait().unwrap().is_some());
        assert!(self.child.wait().unwrap().success(), "library helper failed");
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        // The measured command has its own process group. Release it even when an
        // assertion fails before killing/reaping the helper that is waiting on it.
        let _ = std::fs::write(&self.release, b"");
        let deadline = Instant::now() + Duration::from_secs(1);
        while matches!(self.child.try_wait(), Ok(None)) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Read through EOF without risking an unbounded blocking read on a regression.
fn read_stdout_to_eof(output: &mut std::process::ChildStdout) -> Vec<u8> {
    let fd = output.as_raw_fd();
    // SAFETY: output owns this valid descriptor throughout the calls.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    assert_ne!(flags, -1);
    // SAFETY: F_SETFL changes only the flags of the owned stdout reader.
    assert_ne!(unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) }, -1);
    let deadline = Instant::now() + TIMEOUT;
    let mut bytes = Vec::new();
    loop {
        let mut buffer = [0; 1024];
        match output.read(&mut buffer) {
            Ok(0) => return bytes,
            Ok(count) => bytes.extend_from_slice(&buffer[..count]),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "stdout did not reach EOF");
                thread::sleep(Duration::from_millis(10));
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => panic!("read helper stdout: {err}"),
        }
    }
}

#[test]
fn default_library_api_preserves_stdin_and_stdout_after_returning() {
    let tmp = tempfile::tempdir().unwrap();
    let mut helper = Helper::spawn("default", tmp.path());
    wait_until(TIMEOUT, || tmp.path().join("returned").exists());
    assert!(helper.child.try_wait().unwrap().is_none());

    // Send input only after run_command has returned, then have the caller echo
    // it through its original stdout. Either redirected descriptor breaks this.
    let mut input = helper.child.stdin.take().unwrap();
    input.write_all(b"after-return:payload\n").expect("stdin must remain open after return");
    drop(input);
    std::fs::write(&helper.release, b"").unwrap();
    helper.finish();
    let output = read_stdout_to_eof(helper.child.stdout.as_mut().unwrap());
    assert!(output.ends_with(b"after-return:payload\n"), "stdout: {output:?}");
    assert!(tmp.path().join("timing.tsv").exists());
}

#[test]
fn opt_in_library_api_releases_both_pipes_before_child_exits() {
    let tmp = tempfile::tempdir().unwrap();
    let mut helper = Helper::spawn("release", tmp.path());
    wait_until(TIMEOUT, || tmp.path().join("child-ready").exists());
    let mut input = helper.child.stdin.take().unwrap();
    let mut attempts = 0;
    wait_until(TIMEOUT, || {
        // Bound writes well below pipe capacity even if stdin is never released.
        attempts += 1;
        assert!(attempts <= 600, "stdin did not produce a broken pipe");
        match input.write_all(b"x") {
            Err(err) if err.kind() == io::ErrorKind::BrokenPipe => true,
            Ok(()) => false,
            Err(err) => panic!("write helper stdin: {err}"),
        }
    });
    let output = read_stdout_to_eof(helper.child.stdout.as_mut().unwrap());
    assert!(output.ends_with(b"child-payload\n"), "stdout: {output:?}");
    assert!(helper.child.try_wait().unwrap().is_none(), "caller must still be awaiting child");
    assert!(!tmp.path().join("returned").exists());
    assert!(!tmp.path().join("timing.tsv").exists());

    std::fs::write(&helper.release, b"").unwrap();
    helper.finish();
    assert!(tmp.path().join("returned").exists());
    assert!(tmp.path().join("timing.tsv").exists());
}

#[test]
fn opt_in_library_api_preserves_regular_file_streams() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("input"), b"regular-file:payload\n").unwrap();
    let mut helper = Helper::spawn("regular", tmp.path());
    helper.finish();
    let output = std::fs::read(tmp.path().join("output")).unwrap();
    assert!(output.ends_with(b"regular-file:payload\n"), "stdout: {output:?}");
    assert!(tmp.path().join("timing.tsv").exists());
}

#[test]
fn opt_in_library_api_handles_initially_closed_standard_descriptors() {
    for mode in ["closed-stdin", "closed-stdout"] {
        let tmp = tempfile::tempdir().unwrap();
        let mut helper = Helper::spawn(mode, tmp.path());
        helper.finish();
        assert!(tmp.path().join("returned").exists(), "mode: {mode}");
        assert!(tmp.path().join("timing.tsv").exists(), "mode: {mode}");
    }
}

#[test]
fn opt_in_library_api_releases_socket_streams() {
    let tmp = tempfile::tempdir().unwrap();
    let mut helper = Helper::spawn("socket", tmp.path());
    helper.finish();
    assert!(tmp.path().join("returned").exists());
    assert!(tmp.path().join("timing.tsv").exists());
}

#[test]
fn opt_in_library_api_flushes_preexisting_stdout() {
    let tmp = tempfile::tempdir().unwrap();
    let mut helper = Helper::spawn("buffered", tmp.path());
    helper.finish();
    let output = read_stdout_to_eof(helper.child.stdout.as_mut().unwrap());
    assert!(output.ends_with(b"buffered-prefix"), "stdout: {output:?}");
    assert!(tmp.path().join("timing.tsv").exists());
}

#[test]
fn opt_in_library_api_propagates_stdout_flush_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let mut helper = Helper::spawn("flush-error", tmp.path());
    wait_until(TIMEOUT, || tmp.path().join("buffered").exists());
    drop(helper.child.stdout.take().unwrap());
    std::fs::write(&helper.release, b"").unwrap();
    helper.finish();
    assert!(tmp.path().join("flush-failed").exists());
    assert!(!tmp.path().join("timing.tsv").exists());
}

#[test]
fn library_stdio_helper() {
    let Ok(mode) = std::env::var(HELPER_MODE) else { return };
    let directory = PathBuf::from(std::env::var_os(HELPER_DIR).unwrap());
    let options = RunOptions {
        interval: Duration::from_millis(10),
        output_path: directory.join("timing.tsv").into_boxed_path(),
        format: OutputFormat::Tsv,
        force_summary: false,
        trace_path: None,
        markdown_path: None,
        schema_mode: SchemaMode::Full,
    };
    let mut socket_peer = if mode == "socket" {
        let (socket, peer) = std::os::unix::net::UnixStream::pair().unwrap();
        // SAFETY: the socket stays open while dup2 installs owned standard-fd copies
        // in this isolated helper only.
        assert_ne!(unsafe { libc::dup2(socket.as_raw_fd(), libc::STDIN_FILENO) }, -1);
        // SAFETY: same valid socket descriptor as above.
        assert_ne!(unsafe { libc::dup2(socket.as_raw_fd(), libc::STDOUT_FILENO) }, -1);
        Some(peer)
    } else {
        None
    };
    if let Some(fd) = match mode.as_str() {
        "closed-stdin" => Some(libc::STDIN_FILENO),
        "closed-stdout" => Some(libc::STDOUT_FILENO),
        _ => None,
    } {
        // SAFETY: only this isolated helper intentionally closes its standard fd.
        assert_eq!(unsafe { libc::close(fd) }, 0);
    }
    if mode == "buffered" || mode == "flush-error" {
        io::stdout().write_all(b"buffered-prefix").unwrap();
    }
    if mode == "flush-error" {
        std::fs::write(directory.join("buffered"), b"").unwrap();
        wait_until(TIMEOUT, || directory.join("release").exists());
        let err = run_command_releasing_pipes("sleep", &["10".into()], &options)
            .expect_err("flush must fail when downstream closes");
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
        std::fs::write(directory.join("flush-failed"), b"").unwrap();
        std::process::exit(0);
    }
    let outcome = match mode.as_str() {
        "default" => run_command("true", &[], &options).unwrap(),
        "regular" | "closed-stdin" | "closed-stdout" | "socket" | "buffered" => {
            run_command_releasing_pipes("true", &[], &options).unwrap()
        }
        "release" => run_command_releasing_pipes(
            "sh",
            &[
                "-c".into(),
                concat!(
                    "exec 0<&-; printf 'child-payload\\n'; exec 1>&-; ",
                    ": > \"$1/child-ready\"; ",
                    "i=0; while [ ! -e \"$1/release\" ] && [ \"$i\" -lt 1500 ]; do ",
                    "sleep 0.01; i=$((i + 1)); done; test -e \"$1/release\""
                )
                .into(),
                "sh".into(),
                directory.to_str().unwrap().into(),
            ],
            &options,
        )
        .unwrap(),
        _ => panic!("unknown helper mode: {mode}"),
    };
    assert!(outcome.status.success());
    std::fs::write(directory.join("returned"), b"").unwrap();
    if let Some(peer) = socket_peer.as_mut() {
        peer.set_nonblocking(true).unwrap();
        let mut byte = [0];
        wait_until(TIMEOUT, || match peer.read(&mut byte) {
            Ok(0) => true,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => false,
            result => panic!("expected socket EOF, got {result:?}"),
        });
        assert_eq!(io::stdin().read(&mut byte).unwrap(), 0);
        io::stdout().write_all(b"redirected socket stdout").unwrap();
        io::stdout().flush().unwrap();
    }
    if mode == "closed-stdin" {
        let mut byte = [0];
        assert_eq!(io::stdin().read(&mut byte).unwrap(), 0);
        io::stdout().write_all(b"redirected stdout remains usable").unwrap();
        io::stdout().flush().unwrap();
    } else if mode == "closed-stdout" {
        let mut byte = [0];
        assert_eq!(io::stdin().read(&mut byte).unwrap(), 0);
        // SAFETY: fcntl only checks whether stdout is open in this helper.
        assert_eq!(unsafe { libc::fcntl(libc::STDOUT_FILENO, libc::F_GETFD) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EBADF));
    }
    if mode == "default" || mode == "regular" {
        if mode == "default" {
            wait_until(Duration::from_secs(15), || directory.join("release").exists());
        }
        let mut input = String::new();
        io::stdin().read_to_string(&mut input).unwrap();
        let expected =
            if mode == "default" { "after-return:payload\n" } else { "regular-file:payload\n" };
        assert_eq!(input, expected);
        io::stdout().write_all(input.as_bytes()).unwrap();
        io::stdout().flush().unwrap();
    }
    // Avoid libtest writing a completion message after stdout has been released.
    std::process::exit(0);
}
