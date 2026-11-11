//! Runs ffmpeg with resource limits, progress reporting and VMAF parsing.

use crate::events::{Cancelled, Event, Hooks};
use anyhow::{Context, Result, bail};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::time::Instant;

/// What an ffmpeg/ffprobe pair can do. Encoding needs the libsvtav1 encoder and the libvmaf
/// filter, which many distribution builds of ffmpeg lack.
#[derive(serde::Serialize, Clone, Debug)]
pub struct Capabilities {
    /// First line of `ffmpeg -version`; None when ffmpeg does not run.
    pub ffmpeg_version: Option<String>,
    pub ffprobe_ok: bool,
    pub svtav1: bool,
    pub vmaf: bool,
}

impl Capabilities {
    pub fn probe(ffmpeg: &str, ffprobe: &str) -> Self {
        // A program that hangs (a wrong path pointing at something else) is given up on after 10 s.
        let out = |prog: &str, arg: &str| -> Option<String> {
            let mut child = Command::new(prog)
                .args(["-hide_banner", arg])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .ok()?;
            let mut stdout = child.stdout.take()?;
            let reader = std::thread::spawn(move || {
                let mut s = String::new();
                let _ = stdout.read_to_string(&mut s);
                s
            });
            let start = Instant::now();
            let status = loop {
                match child.try_wait() {
                    Ok(Some(st)) => break st,
                    Ok(None) if start.elapsed() < std::time::Duration::from_secs(10) => std::thread::sleep(std::time::Duration::from_millis(20)),
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return None;
                    }
                }
            };
            let text = reader.join().ok()?;
            status.success().then_some(text)
        };
        let ffmpeg_version = out(ffmpeg, "-version").and_then(|v| v.lines().next().map(str::to_owned));
        // Listing lines look like " V....D libsvtav1   SVT-AV1..." and " ... libvmaf   VV->V ...".
        let lists = |list: Option<String>, name: &str| list.is_some_and(|l| l.lines().any(|x| x.split_whitespace().nth(1) == Some(name)));
        let (svtav1, vmaf) = if ffmpeg_version.is_some() {
            (lists(out(ffmpeg, "-encoders"), "libsvtav1"), lists(out(ffmpeg, "-filters"), "libvmaf"))
        } else {
            (false, false)
        };
        let ffprobe_ok = out(ffprobe, "-version").is_some();
        Capabilities { ffmpeg_version, ffprobe_ok, svtav1, vmaf }
    }

    /// Why encoding cannot work with these tools, if it cannot.
    pub fn problem(&self, ffmpeg: &str, ffprobe: &str) -> Option<String> {
        let mut missing = Vec::new();
        if self.ffmpeg_version.is_none() {
            return Some(format!("cannot run ffmpeg ({ffmpeg})"));
        }
        if !self.svtav1 {
            missing.push("the libsvtav1 encoder");
        }
        if !self.vmaf {
            missing.push("the libvmaf filter");
        }
        if !missing.is_empty() {
            return Some(format!("{ffmpeg} lacks {}; av1opt needs an ffmpeg build with libsvtav1 and libvmaf", missing.join(" and ")));
        }
        (!self.ffprobe_ok).then(|| format!("cannot run ffprobe ({ffprobe})"))
    }
}

/// The child is killed when av1opt goes away without stopping it (kill -9, a crash): an ffmpeg
/// left behind would write a file nobody finishes, and one left behind paused would sit on
/// its memory until someone found it. The signal follows the thread that started the child;
/// every run here is started and waited for by the same thread.
fn die_with_parent(c: &mut Command) {
    use std::os::unix::process::CommandExt;
    let parent = std::process::id();
    // SAFETY: the closure runs in the forked child before exec and only makes async-signal-safe
    // calls (prctl, getppid).
    unsafe {
        c.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // The parent went away before the request was in place: nothing will signal us.
            if libc::getppid() as u32 != parent {
                return Err(std::io::Error::other("av1opt is gone"));
            }
            Ok(())
        });
    }
}

/// See [`Tools::stopwatch`].
pub struct Stopwatch {
    start: Instant,
    /// Time the job had spent paused when the count began.
    paused: std::time::Duration,
}

impl Stopwatch {
    /// Seconds worked since the count began. A pause is not work: an encode held for the
    /// night would otherwise teach the time estimates that this machine is slow, and time
    /// out a probe that never got to run.
    pub fn secs(&self, tools: &Tools) -> f64 {
        self.start.elapsed().saturating_sub(tools.paused_time().saturating_sub(self.paused)).as_secs_f64()
    }
}

pub struct Tools {
    pub ffmpeg: String,
    pub ffprobe: String,
    /// Prepended to every ffmpeg call, e.g. a systemd-run scope with a memory cap.
    prefix: Vec<String>,
    /// Progress listener and cancel switch (the UI); None on the command line.
    pub hooks: Option<Arc<Hooks>>,
    /// First line of `ffmpeg -version`, asked for once.
    version: std::sync::OnceLock<String>,
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
        Tools { ffmpeg, ffprobe, prefix, hooks: None, version: std::sync::OnceLock::new() }
    }

    /// Which ffmpeg build this is (the first line of `ffmpeg -version`; empty if it won't
    /// say). Measurements kept between runs only count for the build that made them.
    pub fn encoder_version(&self) -> &str {
        self.version.get_or_init(|| {
            Command::new(&self.ffmpeg)
                .args(["-hide_banner", "-version"])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .ok()
                .and_then(|o| String::from_utf8_lossy(&o.stdout).lines().next().map(str::to_owned))
                .unwrap_or_default()
        })
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
        die_with_parent(&mut c);
        c
    }

    /// Reports progress to the listener, if any.
    pub fn emit(&self, e: Event) {
        if let Some(h) = &self.hooks {
            h.emit(e);
        }
    }

    /// Starts counting working time: the time that passes, less what the job spends paused.
    pub fn stopwatch(&self) -> Stopwatch {
        Stopwatch { start: Instant::now(), paused: self.paused_time() }
    }

    fn paused_time(&self) -> std::time::Duration {
        self.hooks.as_ref().map_or(std::time::Duration::ZERO, |h| h.paused_time())
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
        cmd.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
        let child = self.spawn(&mut cmd)?;
        let out = self.finish(child)?;
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        if !out.status.success() {
            bail!("ffmpeg failed ({}):\n{}", out.status, tail(&stderr, 15));
        }
        Ok(stderr)
    }

    /// Runs ffmpeg and returns what it wrote to stdout. A run still going after `timeout` (a
    /// damaged file, a stalled network drive) is stopped and counts as failed.
    pub fn run_capture(&self, args: &[String], timeout: std::time::Duration) -> Result<Vec<u8>> {
        let mut cmd = self.command_for(args);
        cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = self.spawn(&mut cmd)?;
        let id = child.id();
        let (mut stdout, mut stderr) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
        let out_reader = std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = stdout.read_to_end(&mut b);
            b
        });
        let err_reader = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = stderr.read_to_string(&mut s);
            s
        });
        let watch = self.stopwatch();
        let status = loop {
            match child.try_wait() {
                Ok(Some(st)) => break Some(st),
                Ok(None) if watch.secs(self) < timeout.as_secs_f64() => std::thread::sleep(std::time::Duration::from_millis(5)),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
            }
        };
        if let Some(h) = &self.hooks {
            h.untrack(id);
        }
        let (out, stderr) = (out_reader.join().unwrap_or_default(), err_reader.join().unwrap_or_default());
        self.check_cancel()?;
        match status {
            Some(st) if st.success() => Ok(out),
            Some(st) => bail!("ffmpeg failed ({st}):\n{}", tail(&stderr, 5)),
            None => bail!("ffmpeg gave no answer within {} s", timeout.as_secs()),
        }
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
        cmd.args(["-progress", "pipe:1", "-nostats", "-v", "error"]).args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = self.spawn(&mut cmd)?;
        let mut stderr = child.stderr.take().unwrap();
        let err_reader = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = stderr.read_to_string(&mut s);
            s
        });

        let watch = self.stopwatch();
        let (mut done_secs, mut fps, mut frames) = (0.0_f64, String::new(), 0u64);
        // Read errors end the loop instead of returning, so the child is always reaped
        // (and untracked) below.
        for line in BufReader::new(child.stdout.take().unwrap()).lines().map_while(Result::ok) {
            if let Some(us) = line.strip_prefix("out_time_us=").and_then(|v| v.parse::<f64>().ok()) {
                done_secs = us / 1e6;
            } else if let Some(n) = line.strip_prefix("frame=").and_then(|v| v.trim().parse().ok()) {
                frames = n;
            } else if let Some(f) = line.strip_prefix("fps=") {
                fps = f.to_string();
            } else if line.starts_with("progress=") && total_secs > 0.0 {
                let frac = (done_secs / total_secs).clamp(0.0, 1.0);
                let elapsed = watch.secs(self);
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
