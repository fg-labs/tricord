//! Isolated signal regression for the real wait4 interruption path.

use std::{
    cell::RefCell,
    io::{self, Read},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use super::{ExitUsage, wait_with_usage};

thread_local! {
    static EINTR_OBSERVER: RefCell<Option<mpsc::Sender<()>>> = const { RefCell::new(None) };
}

/// Notify only the waiting thread's observer, after wait4 has returned EINTR.
pub(super) fn observed_interruption() {
    EINTR_OBSERVER.with_borrow(|observer| {
        if let Some(observer) = observer {
            let _ = observer.send(());
        }
    });
}

fn worker(name: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--ignored", "--exact", name, "--nocapture"]);
    command
}

#[test]
fn wait_retries_actual_eintr() {
    assert!(worker("run::eintr_tests::eintr_subprocess").status().unwrap().success());
}

extern "C" fn harmless_signal(_: libc::c_int) {}

#[test]
#[ignore = "isolated process with a process-wide signal disposition"]
fn eintr_subprocess() {
    // Bound failures such as accidentally enabling SA_RESTART or retrying forever.
    thread::spawn(|| {
        thread::sleep(Duration::from_secs(10));
        std::process::exit(124);
    });

    // SAFETY: sigaction is POD; all-zero is valid before initializing its fields.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = harmless_signal as *const () as usize;
    action.sa_flags = 0;
    // SAFETY: the mask is writable, the handler is valid, and SIGUSR1 is catchable.
    // No SA_RESTART: wait4 must return EINTR when this thread handles SIGUSR1.
    unsafe {
        assert_eq!(libc::sigemptyset(&raw mut action.sa_mask), 0);
        assert_eq!(libc::sigaction(libc::SIGUSR1, &raw const action, std::ptr::null_mut()), 0);
    }
    // Do not depend on the caller's inherited signal mask.
    // SAFETY: all-zero is valid storage for sigemptyset to initialize.
    let mut mask: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: the mask is initialized before use; only this thread's mask changes.
    unsafe {
        assert_eq!(libc::sigemptyset(&raw mut mask), 0);
        assert_eq!(libc::sigaddset(&raw mut mask, libc::SIGUSR1), 0);
        assert_eq!(
            libc::pthread_sigmask(libc::SIG_UNBLOCK, &raw const mask, std::ptr::null_mut()),
            0
        );
    }

    let mut child =
        worker("run::eintr_tests::gated_cpu_worker").stdin(Stdio::piped()).spawn().unwrap();
    // Keep the child alive until wait4 really reports EINTR. wait_with_usage
    // normally closes child.stdin, so the signal sender must own this gate.
    let gate = child.stdin.take().unwrap();
    let (observed_tx, observed_rx) = mpsc::channel();
    EINTR_OBSERVER.with_borrow_mut(|observer| *observer = Some(observed_tx));
    // pthread_t is a pointer on macOS and an integer on Linux. On macOS,
    // carry its address across Send without dereferencing thread internals.
    // SAFETY: pthread_self returns the ID of this live waiting thread.
    let target = unsafe { libc::pthread_self() };
    #[cfg(target_os = "macos")]
    let target = target as usize;
    let sender = thread::spawn(move || {
        #[cfg(target_os = "macos")]
        let target = target as libc::pthread_t;
        let deadline = Instant::now() + Duration::from_secs(5);
        let observed = loop {
            match observed_rx.recv_timeout(Duration::from_millis(1)) {
                Ok(()) => break true,
                Err(mpsc::RecvTimeoutError::Disconnected) => break false,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if Instant::now() >= deadline {
                break false;
            }
            // SAFETY: the waiting test thread remains alive through join.
            assert_eq!(unsafe { libc::pthread_kill(target, libc::SIGUSR1) }, 0);
        };
        drop(gate);
        observed
    });

    let waited = wait_with_usage(&mut child);
    // Join even when wait fails: never signal a thread that has exited.
    let interrupted = sender.join().unwrap();
    EINTR_OBSERVER.with_borrow_mut(|observer| *observer = None);
    assert!(interrupted, "wait4 never returned EINTR");
    let (status, usage) = waited.unwrap();
    assert_eq!(status.code(), Some(23));
    assert!(usage.cpu_time >= 0.05, "lost CPU after retry: {usage:?}");
}

fn own_cpu_seconds() -> f64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: the pointer names writable, aligned rusage storage.
    assert_eq!(unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) }, 0);
    // SAFETY: getrusage succeeded and initialized usage.
    ExitUsage::from_rusage(&unsafe { usage.assume_init() }).cpu_time
}

#[test]
#[ignore = "gated workload invoked by the isolated EINTR test"]
fn gated_cpu_worker() {
    io::stdin().read_to_end(&mut Vec::new()).unwrap();
    let start = own_cpu_seconds();
    while own_cpu_seconds() - start < 0.05 {
        for value in 0..10_000_u64 {
            std::hint::black_box(value.wrapping_mul(value));
        }
    }
    std::process::exit(23);
}
