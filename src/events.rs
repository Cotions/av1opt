//! Progress events and cancellation for front ends other than the terminal (the `ui` web app).
//! The command line passes no hooks and keeps printing to stderr; the encode code paths are the
//! same either way.

use serde::Serialize;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Something that happened while encoding, in the order it happened.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// Batch position: file `index` (0-based) of `total` is starting.
    FileStart { index: usize, total: usize, input: PathBuf, output: PathBuf },
    /// Probe result of the file being encoded.
    Source { codec: String, width: u32, height: u32, fps: f64, duration_s: f64, video_mbps: f64, hdr: bool },
    /// A new step: "sampling", "searching", "scenes", "chunks", "encoding", "muxing", "verifying",
    /// "repairing".
    Phase { phase: String, detail: String },
    /// One CRF search trial over all samples.
    Trial { crf: f64, mean_vmaf: f64, min_vmaf: f64, size_ratio: f64 },
    /// CRF chosen by the search.
    Chosen { crf: f64, vmaf: f64, predicted_savings: f64 },
    /// Long ffmpeg job (full encode, scene detection) progress.
    Progress { label: String, frac: f64, fps: f64, eta_secs: f64 },
    /// Scene mode: chunk plan known; `done` chunks are already encoded from an earlier run.
    ChunkPlan { chunks: usize, done: usize },
    /// Scene mode: one chunk finished. `frac`/`eta_secs` cover all chunks still to do.
    Chunk { index: usize, chunks: usize, start_secs: f64, frames: u64, crf: f64, vmaf: f64, bytes: u64, corrected: bool, frac: f64, eta_secs: f64 },
    /// Verification spot check.
    SpotCheck { at_secs: f64, vmaf: f64 },
    Note { text: String },
    /// File finished: status is "ok", "skip", "error", "done_before" or "cancelled".
    FileEnd { index: usize, status: String, detail: String, input_bytes: u64, output_bytes: u64 },
}

/// Listener plus cancel and pause switches, shared by every ffmpeg run of one job.
pub struct Hooks {
    sink: Box<dyn Fn(Event) + Send + Sync>,
    cancelled: AtomicBool,
    running: Mutex<Running>,
}

/// The job's ffmpeg processes and whether they are held. One lock for both: a process started
/// while a pause is being set is stopped by one side or the other.
#[derive(Default)]
struct Running {
    children: Vec<u32>,
    /// Since when the job is paused.
    paused: Option<Instant>,
    /// Time spent paused before that.
    paused_before: Duration,
}

fn signal(pid: u32, sig: libc::c_int) {
    // SAFETY: plain kill(2) on a child we started and have not reaped yet.
    unsafe { libc::kill(pid as i32, sig) };
}

#[cfg_attr(not(feature = "ui"), allow(dead_code))]
impl Hooks {
    pub fn new(sink: impl Fn(Event) + Send + Sync + 'static) -> Self {
        Hooks { sink: Box::new(sink), cancelled: AtomicBool::new(false), running: Mutex::default() }
    }

    pub fn emit(&self, e: Event) {
        (self.sink)(e)
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Stops the job: running ffmpeg processes are terminated, new ones refuse to start.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        for &pid in self.running.lock().unwrap().children.iter() {
            signal(pid, libc::SIGTERM);
            // A paused process only sees the request once it runs again.
            signal(pid, libc::SIGCONT);
        }
    }

    /// Holds the job where it is: its ffmpeg processes stop using the processor (they keep
    /// their memory) until [`Hooks::resume`]. Ones started meanwhile are held at once.
    pub fn pause(&self) {
        let mut r = self.running.lock().unwrap();
        if r.paused.is_none() && !self.is_cancelled() {
            r.paused = Some(Instant::now());
            r.children.iter().for_each(|&pid| signal(pid, libc::SIGSTOP));
        }
    }

    pub fn resume(&self) {
        let mut r = self.running.lock().unwrap();
        if let Some(since) = r.paused.take() {
            r.paused_before += since.elapsed();
            r.children.iter().for_each(|&pid| signal(pid, libc::SIGCONT));
        }
    }

    pub fn is_paused(&self) -> bool {
        self.running.lock().unwrap().paused.is_some()
    }

    /// All the time the job has spent paused so far.
    pub fn paused_time(&self) -> Duration {
        let r = self.running.lock().unwrap();
        r.paused_before + r.paused.map_or(Duration::ZERO, |since| since.elapsed())
    }

    pub fn track(&self, pid: u32) {
        let mut r = self.running.lock().unwrap();
        r.children.push(pid);
        // Cancel may have raced with the spawn.
        if self.is_cancelled() {
            signal(pid, libc::SIGTERM);
        } else if r.paused.is_some() {
            signal(pid, libc::SIGSTOP);
        }
    }

    pub fn untrack(&self, pid: u32) {
        self.running.lock().unwrap().children.retain(|&p| p != pid);
    }
}

/// The user stopped the job. Not a failure: batch runs don't log it, and the next run resumes.
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// True if `e` (or anything it wraps) is a cancellation.
pub fn is_cancelled(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.is::<Cancelled>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command};

    fn sleeper() -> Child {
        Command::new("sleep").arg("60").spawn().unwrap()
    }

    /// Process state letter from /proc: `T` is stopped, `S` sleeping.
    fn state(pid: u32) -> char {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        stat[stat.rfind(')').unwrap() + 2..].chars().next().unwrap()
    }

    fn wait_for_state(pid: u32, want: char) {
        for _ in 0..200 {
            if state(pid) == want {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("process {pid} is {}, not {want}", state(pid));
    }

    #[test]
    fn pause_holds_the_processes_and_its_time_is_counted() {
        let hooks = Hooks::new(|_| {});
        let mut a = sleeper();
        hooks.track(a.id());
        assert!(!hooks.is_paused());
        assert_eq!(hooks.paused_time(), Duration::ZERO);

        hooks.pause();
        assert!(hooks.is_paused());
        wait_for_state(a.id(), 'T');
        // One started during the pause is held at once.
        let mut b = sleeper();
        hooks.track(b.id());
        wait_for_state(b.id(), 'T');
        std::thread::sleep(Duration::from_millis(60));
        // A second pause does not start the count again.
        hooks.pause();
        let held = hooks.paused_time();
        assert!(held >= Duration::from_millis(60), "{held:?}");

        hooks.resume();
        assert!(!hooks.is_paused());
        wait_for_state(a.id(), 'S');
        wait_for_state(b.id(), 'S');
        let after = hooks.paused_time();
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(hooks.paused_time(), after, "the count goes on after the pause ended");
        assert!(after >= held);

        // A second pause adds to the first.
        hooks.pause();
        std::thread::sleep(Duration::from_millis(30));
        hooks.resume();
        assert!(hooks.paused_time() >= after + Duration::from_millis(30));

        for c in [&mut a, &mut b] {
            hooks.untrack(c.id());
            c.kill().unwrap();
            c.wait().unwrap();
        }
    }

    #[test]
    fn cancel_ends_a_paused_job() {
        use std::os::unix::process::ExitStatusExt;
        let hooks = Hooks::new(|_| {});
        let mut a = sleeper();
        hooks.track(a.id());
        hooks.pause();
        wait_for_state(a.id(), 'T');
        hooks.cancel();
        assert_eq!(a.wait().unwrap().signal(), Some(libc::SIGTERM));
        // Nothing holds a cancelled job, and what it starts is ended at once.
        hooks.pause();
        let mut b = sleeper();
        hooks.track(b.id());
        assert_eq!(b.wait().unwrap().signal(), Some(libc::SIGTERM));
    }
}
