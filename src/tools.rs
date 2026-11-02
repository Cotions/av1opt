//! Runs ffmpeg with resource limits, progress reporting and VMAF parsing.

use crate::events::{Cancelled, Event, Hooks};
use anyhow::{Context, Result, bail};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::time::Instant;

pub struct Tools {
    pub ffmpeg: String,
    pub ffprobe: String,
    /// Prepended to every ffmpeg call, e.g. a systemd-run scope with a memory cap.
    prefix: Vec<String>,
    /// Progress listener and cancel switch (the UI); None on the command line.
    pub hooks: Option<Arc<Hooks>>,
}

impl Tools {
    pub fn new(ffmpeg: String, ffprobe: String, memory_limit: Option<String>) -> Self {
        let prefix = match memory_limit {
            Some(limit) if systemd_scope_works() => [
                "systemd-run", "--user", "--scope", "-q", "-p", &format!("MemoryMax={limit}"), "-p", "MemorySwapMax=0",
            ]
            .map(String::from)
            .to_vec(),
            Some(_) => {
                eprintln!("warning: systemd-run --user unavailable, running ffmpeg without a memory limit");
                Vec::new()
            }
            None => Vec::new(),
        };
        Tools { ffmpeg, ffprobe, prefix, hooks: None }
    }

    /// Builds the ffmpeg command; with AV1OPT_DEBUG set, prints it first.
    fn command_for(&self, args: &[String]) -> Command {
        if std::env::var_os("AV1OPT_DEBUG").is_some() && !args.is_empty() {
            let quoted: Vec<String> = args.iter().map(|a| if a.contains([' ', ';', '[']) { format!("'{a}'") } else { a.clone() }).collect();
            eprintln!("+ ffmpeg {}", quoted.join(" "));
        }
        let mut c = match self.prefix.split_first() {
            Some((first, rest)) => {
                let mut c = Command::new(first);
                c.args(rest).arg(&self.ffmpeg);
                c
            }
            None => Command::new(&self.ffmpeg),
        };
        // SVT-AV1 prints its config banner to stderr on every run; errors only.
        c.env("SVT_LOG", "1").arg("-hide_banner").arg("-nostdin");
        c
    }

    /// Reports progress to the listener, if any.
    pub fn emit(&self, e: Event) {
        if let Some(h) = &self.hooks {
            h.emit(e);
        }
    }

    /// Fails with [`Cancelled`] once the user asked to stop.
    pub fn check_cancel(&self) -> Result<()> {
        match &self.hooks {
            Some(h) if h.is_cancelled() => Err(Cancelled.into()),
            _ => Ok(()),
        }
    }

    /// Starts a child that a cancel request can kill.
    fn spawn(&self, cmd: &mut Command) -> Result<Child> {
        self.check_cancel()?;
        let child = cmd.spawn().context("failed to start ffmpeg")?;
        if let Some(h) = &self.hooks {
            h.track(child.id());
        }
        Ok(child)
    }

    fn finish(&self, child: Child) -> Result<Output> {
        let id = child.id();
        let out = child.wait_with_output();
        if let Some(h) = &self.hooks {
            h.untrack(id);
        }
        self.check_cancel()?;
        Ok(out?)
    }

    /// Runs ffmpeg and returns its stderr. Fails with the tail of stderr on error.
    pub fn run(&self, args: &[String]) -> Result<String> {
        let mut cmd = self.command_for(args);
        cmd.args(args).stdout(Stdio::null()).stderr(Stdio::piped());
        let child = self.spawn(&mut cmd)?;
        let out = self.finish(child)?;
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        if !out.status.success() {
            bail!("ffmpeg failed ({}):\n{}", out.status, tail(&stderr, 15));
        }
        Ok(stderr)
    }

    /// Runs a long ffmpeg job, printing percent done, encode fps and ETA on one line.
    /// Returns the number of frames ffmpeg output.
    pub fn run_with_progress(&self, args: &[String], total_secs: f64, label: &str) -> Result<u64> {
        self.run_with_progress_in(None, args, total_secs, label)
    }

    /// Like [`Tools::run_with_progress`], with ffmpeg's working directory set to `dir` (lets
    /// filters write files by relative name, avoiding filter-graph path escaping).
    pub fn run_with_progress_in(&self, dir: Option<&Path>, args: &[String], total_secs: f64, label: &str) -> Result<u64> {
        let mut cmd = self.command_for(args);
        if let Some(d) = dir {
            cmd.current_dir(d);
        }
        cmd.args(["-progress", "pipe:1", "-nostats", "-v", "error"]).args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = self.spawn(&mut cmd)?;
        let mut stderr = child.stderr.take().unwrap();
        let err_reader = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = stderr.read_to_string(&mut s);
            s
        });

        let start = Instant::now();
        let (mut done_secs, mut fps, mut frames) = (0.0_f64, String::new(), 0u64);
        for line in BufReader::new(child.stdout.take().unwrap()).lines() {
            let line = line?;
            if let Some(us) = line.strip_prefix("out_time_us=").and_then(|v| v.parse::<f64>().ok()) {
                done_secs = us / 1e6;
            } else if let Some(n) = line.strip_prefix("frame=").and_then(|v| v.trim().parse().ok()) {
                frames = n;
            } else if let Some(f) = line.strip_prefix("fps=") {
                fps = f.to_string();
            } else if line.starts_with("progress=") && total_secs > 0.0 {
                let frac = (done_secs / total_secs).clamp(0.0, 1.0);
                let elapsed = start.elapsed().as_secs_f64();
                let eta = if frac > 0.01 { elapsed / frac - elapsed } else { 0.0 };
                if self.hooks.is_some() {
                    self.emit(Event::Progress { label: label.to_string(), frac, fps: fps.trim().parse().unwrap_or(0.0), eta_secs: eta });
                } else {
                    eprint!("\r{label} {:5.1}%  {fps:>6} fps  ETA {}   ", frac * 100.0, hms(eta));
                }
            }
        }
        if self.hooks.is_none() {
            eprintln!();
        }
        let out = self.finish(child)?;
        let status = out.status;
        let stderr = err_reader.join().unwrap_or_default();
        if !status.success() {
            bail!("ffmpeg failed ({status}):\n{}", tail(&stderr, 15));
        }
        Ok(frames)
    }

    /// VMAF of `distorted` against `reference`, pairing frames by index rather than timestamp
    /// (encoders and muxers shift start times, which made libvmaf compare neighbouring frames).
    /// `offset` > 0 skips that many leading distorted frames; < 0 skips reference frames.
    /// `subsample` > 1 scores only every n-th frame (faster, slightly noisier).
    #[allow(clippy::too_many_arguments)]
    pub fn vmaf(
        &self,
        distorted: &Path,
        reference: &Path,
        model: &str,
        pix_fmt: &str,
        threads: usize,
        offset: i32,
        subsample: u32,
    ) -> Result<f64> {
        let trim = |skip: bool| if skip { format!("trim=start_frame={},", offset.unsigned_abs()) } else { String::new() };
        let filter = format!(
            "[0:v]{}format={pix_fmt},settb=1,setpts=N[d];[1:v]{}format={pix_fmt},settb=1,setpts=N[r];\
             [d][r]libvmaf=model=version={model}:n_threads={threads}:n_subsample={}:shortest=1",
            trim(offset > 0),
            trim(offset < 0),
            subsample.max(1)
        );
        let args: Vec<String> = ["-i".into(), path_arg(distorted), "-i".into(), path_arg(reference)]
            .into_iter()
            .chain(["-lavfi", &filter, "-f", "null", "-"].map(String::from))
            .collect();
        let stderr = self.run(&args)?;
        stderr
            .lines()
            .find_map(|l| l.split("VMAF score: ").nth(1))
            .and_then(|v| v.trim().parse().ok())
            .context("no VMAF score in ffmpeg output")
    }
}

impl Tools {
    /// Number of video packets (= frames) in the first video stream, without decoding.
    pub fn count_frames(&self, path: &Path) -> Result<u64> {
        let out = Command::new(&self.ffprobe)
            .args(["-v", "error", "-select_streams", "v:0", "-count_packets", "-show_entries", "stream=nb_read_packets", "-of", "csv=p=0"])
            .arg(path)
            .output()
            .context("failed to run ffprobe")?;
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .trim_end_matches(',')
            .parse()
            .with_context(|| format!("cannot count frames in {}", path.display()))
    }
}

pub fn path_arg(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

pub fn hms(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

fn tail(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

fn in_path(bin: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

/// systemd-run can exist without a user manager (e.g. over plain SSH); check it actually works.
fn systemd_scope_works() -> bool {
    in_path("systemd-run")
        && Command::new("systemd-run")
            .args(["--user", "--scope", "-q", "true"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
}
