//! What the program says on its two output streams, for every module: these macros take the
//! place of the standard ones, which panic when the stream is gone.
//!
//! Standard error is where progress goes, and it can vanish under a running program (`av1opt ui
//! 2>&1 | tee log` and `tee` dies, a terminal is closed). That is no reason to stop anything,
//! so a failed write there is ignored. Standard output is what a command was run for: once
//! nobody reads it (`av1opt scan ... | head`) the command has nothing left to do, and it ends
//! the way programs end on a closed pipe.

use std::fmt::Arguments;
use std::io::Write;

macro_rules! eprintln {
    () => { $crate::output::to_stderr(format_args!("\n")) };
    ($($arg:tt)*) => { $crate::output::to_stderr(format_args!("{}\n", format_args!($($arg)*))) };
}

macro_rules! eprint {
    ($($arg:tt)*) => { $crate::output::to_stderr(format_args!($($arg)*)) };
}

macro_rules! println {
    () => { $crate::output::to_stdout(format_args!("\n")) };
    ($($arg:tt)*) => { $crate::output::to_stdout(format_args!("{}\n", format_args!($($arg)*))) };
}

/// Written in one piece, so that lines from several threads do not run into each other.
pub fn to_stderr(text: Arguments) {
    let _ = std::io::stderr().write_all(text.to_string().as_bytes());
}

pub fn to_stdout(text: Arguments) {
    if let Err(e) = std::io::stdout().write_all(text.to_string().as_bytes()) {
        stdout_failed(&e);
    }
}

fn stdout_failed(e: &std::io::Error) -> ! {
    if e.kind() == std::io::ErrorKind::BrokenPipe {
        // Rust starts programs with this signal ignored. Put back, it ends the process as it
        // would any other command in a pipeline, and the shell reports the usual status.
        // SAFETY: plain signal(2) and raise(3) with constants; nothing is shared.
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            libc::raise(libc::SIGPIPE);
        }
    }
    eprintln!("error: cannot write the results to standard output: {e}");
    std::process::exit(1);
}
