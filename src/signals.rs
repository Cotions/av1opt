//! Ctrl-C and `kill` on the command line. The run stops the way a job on the page is stopped: its
//! ffmpeg processes are ended and its work unwinds, which removes its temporary folders, so that
//! lossless clips do not stay on the disk. Per-scene work folders stay: a later run resumes from
//! them.

use crate::events::Hooks;
use crate::tools::Tools;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

/// The first signal that came, or 0.
static RECEIVED: AtomicI32 = AtomicI32::new(0);
/// The end of the pipe that wakes the thread doing the stopping.
static WAKE: AtomicI32 = AtomicI32::new(-1);

/// Runs in the middle of anything, so it only does what is safe there: an atomic, `write`, `_exit`.
extern "C" fn on_signal(signal: libc::c_int) {
    if RECEIVED.swap(signal, Ordering::SeqCst) != 0 {
        // Asked twice: the person does not want to wait. The ffmpeg processes die with us.
        // SAFETY: _exit ends the process and may be called from a signal handler.
        unsafe { libc::_exit(128 + signal) };
    }
    // SAFETY: write may be called from a signal handler, on a pipe that is never closed, from a
    // buffer that lives through the call. The error number is another thread's to read.
    unsafe {
        let errno = *libc::__errno_location();
        libc::write(WAKE.load(Ordering::SeqCst), [1u8].as_ptr().cast(), 1);
        *libc::__errno_location() = errno;
    }
}

/// From here on SIGINT and SIGTERM stop what `tools` is running. A signal that was being ignored
/// when the program started (a shell's background job, for Ctrl-C) stays ignored.
pub fn stopping_on_signals(mut tools: Tools) -> Tools {
    let hooks = Arc::new(Hooks::on_terminal());
    tools.hooks = Some(hooks.clone());
    let mut ends = [0; 2];
    // SAFETY: pipe2 fills the two numbers of the array it is given.
    if unsafe { libc::pipe2(ends.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        eprintln!("warning: cannot prepare for Ctrl-C ({}): stopping now would leave temporary files", std::io::Error::last_os_error());
        return tools;
    }
    let [wake_up, wake] = ends;
    WAKE.store(wake, Ordering::SeqCst);
    std::thread::spawn(move || {
        let mut byte = 0u8;
        // SAFETY: read into one byte that lives through the call, on the pipe made above.
        while unsafe { libc::read(wake_up, (&raw mut byte).cast(), 1) } != 1 {
            if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                return;
            }
        }
        eprintln!("\nstopping: ending ffmpeg and removing temporary files (once more to quit at once)");
        hooks.cancel();
    });
    for signal in [libc::SIGINT, libc::SIGTERM] {
        // SAFETY: both structures are plain data for which zero is valid, and they outlive the
        // calls; the handler is a function that stays for the life of the program.
        unsafe {
            let mut before: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(signal, std::ptr::null(), &mut before) != 0 || before.sa_sigaction == libc::SIG_IGN {
                continue;
            }
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as usize;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
    }
    tools
}

/// What the program ends with when a signal stopped it: 128 and the signal's number, as a shell
/// reports a program killed by it.
pub fn exit_code() -> Option<i32> {
    match RECEIVED.load(Ordering::SeqCst) {
        0 => None,
        signal => Some(128 + signal),
    }
}

#[cfg(test)]
thread_local! {
    /// The step at which a unit test of this thread stops what it runs (see [`stop_point`]).
    pub(crate) static STOP_AT: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
}

/// A named moment between two of the steps that change the disk when an encode is finished or
/// files are moved for `replace`, where the tests stop a run to see what the next one makes of
/// what is left. With `AV1OPT_STOP_AT` set to its name the process kills itself there, as a
/// power cut or `kill -9` would end it: nothing is cleaned up. In the unit tests, the step in
/// [`STOP_AT`] panics instead, which leaves the files as a kill would where nothing that is
/// dropped on the way out changes the disk.
pub fn stop_point(step: &str) {
    #[cfg(test)]
    if STOP_AT.get() == Some(step) {
        std::panic::panic_any(format!("stopped at {step}"));
    }
    if std::env::var_os("AV1OPT_STOP_AT").is_some_and(|s| s == step) {
        // SAFETY: kill with this process's own number ends it; no memory is involved.
        unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
    }
}
