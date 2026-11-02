//! Runs ffmpeg with resource limits, progress reporting and VMAF parsing.

use anyhow::{Context, Result, bail};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

pub struct Tools {
    pub ffmpeg: String,
    pub ffprobe: String,
    /// Prepended to every ffmpeg call, e.g. a systemd-run scope with a memory cap.
    prefix: Vec<String>,
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
        Tools { ffmpeg, ffprobe, prefix }
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

    /// Runs ffmpeg and returns its stderr. Fails with the tail of stderr on error.
    pub fn run(&self, args: &[String]) -> Result<String> {
        let out = self.command_for(args).args(args).output().context("failed to start ffmpeg")?;
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        if !out.status.success() {
            bail!("ffmpeg failed ({}):\n{}", out.status, tail(&stderr, 15));
        }
        Ok(stderr)
    }

    /// Runs a long ffmpeg job, printing percent done, encode fps and ETA on one line.
    pub fn run_with_progress(&self, args: &[String], total_secs: f64, label: &str) -> Result<()> {
        let mut child = self
            .command_for(args)
            .args(["-progress", "pipe:1", "-nostats", "-v", "error"])
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to start ffmpeg")?;
        let mut stderr = child.stderr.take().unwrap();
        let err_reader = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = stderr.read_to_string(&mut s);
            s
        });

        let start = Instant::now();
        let (mut done_secs, mut fps) = (0.0_f64, String::new());
        for line in BufReader::new(child.stdout.take().unwrap()).lines() {
            let line = line?;
            if let Some(us) = line.strip_prefix("out_time_us=").and_then(|v| v.parse::<f64>().ok()) {
                done_secs = us / 1e6;
            } else if let Some(f) = line.strip_prefix("fps=") {
                fps = f.to_string();
            } else if line.starts_with("progress=") && total_secs > 0.0 {
                let frac = (done_secs / total_secs).clamp(0.0, 1.0);
                let elapsed = start.elapsed().as_secs_f64();
                let eta = if frac > 0.01 { elapsed / frac - elapsed } else { 0.0 };
                eprint!("\r{label} {:5.1}%  {fps:>6} fps  ETA {}   ", frac * 100.0, hms(eta));
            }
        }
        eprintln!();
        let status = child.wait()?;
        let stderr = err_reader.join().unwrap_or_default();
        if !status.success() {
            bail!("ffmpeg failed ({status}):\n{}", tail(&stderr, 15));
        }
        Ok(())
    }

    /// VMAF of `distorted` against `reference`, pairing frames by index rather than timestamp
    /// (encoders and muxers shift start times, which made libvmaf compare neighbouring frames).
    /// `offset` > 0 skips that many leading distorted frames; < 0 skips reference frames.
    pub fn vmaf(&self, distorted: &Path, reference: &Path, model: &str, pix_fmt: &str, threads: usize, offset: i32) -> Result<f64> {
        let trim = |skip: bool| if skip { format!("trim=start_frame={},", offset.unsigned_abs()) } else { String::new() };
        let filter = format!(
            "[0:v]{}format={pix_fmt},settb=1,setpts=N[d];[1:v]{}format={pix_fmt},settb=1,setpts=N[r];\
             [d][r]libvmaf=model=version={model}:n_threads={threads}:shortest=1",
            trim(offset > 0),
            trim(offset < 0)
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
