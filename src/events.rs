//! Progress events and cancellation for front ends other than the terminal (the `ui` web app).
//! The command line passes no hooks and keeps printing to stderr; the encode code paths are the
//! same either way.

use serde::Serialize;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// Something that happened while encoding, in the order it happened.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// Batch position: file `index` (0-based) of `total` is starting.
    FileStart { index: usize, total: usize, input: PathBuf, output: PathBuf },
    /// Probe result of the file being encoded.
    Source { codec: String, width: u32, height: u32, fps: f64, duration_s: f64, video_mbps: f64, hdr: bool },
    /// A new step: "sampling", "searching", "scenes", "chunks", "encoding", "muxing", "verifying".
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

/// Listener plus cancel switch, shared by every ffmpeg run of one job.
pub struct Hooks {
    sink: Box<dyn Fn(Event) + Send + Sync>,
    cancelled: AtomicBool,
    children: Mutex<Vec<u32>>,
}

impl Hooks {
    pub fn new(sink: impl Fn(Event) + Send + Sync + 'static) -> Self {
        Hooks { sink: Box::new(sink), cancelled: AtomicBool::new(false), children: Mutex::new(Vec::new()) }
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
        for &pid in self.children.lock().unwrap().iter() {
            // SAFETY: plain kill(2) on a child we started and have not reaped yet.
            unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        }
    }

    pub fn track(&self, pid: u32) {
        let mut c = self.children.lock().unwrap();
        c.push(pid);
        // Cancel may have raced with the spawn.
        if self.is_cancelled() {
            // SAFETY: as in `cancel`.
            unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        }
    }

    pub fn untrack(&self, pid: u32) {
        self.children.lock().unwrap().retain(|&p| p != pid);
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
