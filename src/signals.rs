//! Forward `SIGINT`, `SIGTERM`, and `SIGHUP` from `tricorder` to the child
//! process group while the run is in flight.
//!
//! The child is spawned in its own process group (see
//! [`crate::run::run_command`]) so that signals delivered to `tricorder` do not
//! reach the child by default. This module installs a small handler thread
//! that re-delivers a few interesting signals to the child's process group and
//! records which signal arrived so the caller can compute the right exit code
//! (POSIX convention: `128 + signum`).

use std::{io, thread::JoinHandle};

use signal_hook::{
    consts::{SIGHUP, SIGINT, SIGTERM},
    iterator::{Handle, Signals},
};

/// Signals that we re-deliver to the child process group.
const FORWARDED: [i32; 3] = [SIGINT, SIGTERM, SIGHUP];

/// Owns the signal-forwarding thread; cleaned up on drop.
pub struct SignalForwarder {
    handle: Handle,
    thread: Option<JoinHandle<()>>,
}

impl SignalForwarder {
    /// Install handlers for the forwarded signals and start a thread that
    /// re-delivers them to the process group identified by `child_pgid`.
    ///
    /// `child_pgid` should be the *positive* PID of the child (which is also
    /// its process group leader because we spawned it with `setpgid(0, 0)`).
    /// Internally we send to the negative PID to address the whole group.
    ///
    /// # Errors
    /// Returns any I/O error from registering the signal handlers.
    pub fn install(child_pgid: i32) -> io::Result<Self> {
        let mut signals = Signals::new(FORWARDED)?;
        let handle = signals.handle();
        let thread =
            std::thread::Builder::new().name("tricord-signals".into()).spawn(move || {
                for sig in &mut signals {
                    forward_to_group(child_pgid, sig);
                }
            })?;
        Ok(Self { handle, thread: Some(thread) })
    }
}

impl Drop for SignalForwarder {
    fn drop(&mut self) {
        self.handle.close();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Send `sig` to every process in `pgid`'s process group.
///
/// Errors are ignored — by the time we forward, the child may have already
/// exited and there's nothing useful to do with `ESRCH`.
fn forward_to_group(pgid: i32, sig: i32) {
    // SAFETY: `kill(2)` is a thin syscall wrapper that takes plain integer
    // arguments and returns an integer; passing `-pgid` addresses the whole
    // process group per POSIX. Both arguments are fully owned by the caller.
    unsafe {
        libc::kill(-pgid, sig);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forwarder_installs_and_drops_cleanly() {
        // The interesting behavior — actually delivering forwarded signals
        // to a child process group — is exercised by the end-to-end exit
        // code tests in `tests/integration.rs`. Here we just confirm the
        // happy path constructs and tears down without panicking.
        let pid = std::process::id();
        #[allow(clippy::cast_possible_wrap)]
        let forwarder = SignalForwarder::install(pid as i32).expect("install");
        drop(forwarder);
    }
}
