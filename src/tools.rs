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
    /// What each ffmpeg run may use, enforced by a systemd scope around it.
    cap: Option<String>,
    /// What each of several chunk encodes running side by side may use: their part of the cap.
    side_by_side_cap: Option<String>,
    /// Progress listener and cancel switch: the page's, or the one signals use on the command
    /// line. None where nothing can stop the run.
    pub hooks: Option<Arc<Hooks>>,
    /// First line of `ffmpeg -version`, asked for once.
    version: std::sync::OnceLock<String>,
}

impl Tools {
    pub fn new(ffmpeg: String, ffprobe: String, memory_limit: Option<String>) -> Self {
        let cap = memory_limit.filter(|_| {
            let works = systemd_scope_works();
            if !works {
                eprintln!("warning: systemd-run --user unavailable, running ffmpeg without a memory limit");
            }
            works
        });
        Tools { ffmpeg, ffprobe, side_by_side_cap: cap.clone(), cap, hooks: None, version: std::sync::OnceLock::new() }
    }

    /// Sets what each chunk encode gets when several run at once. Every other ffmpeg run is
    /// alone (a search, a one-pass encode, a check) and has the whole cap.
    pub fn sharing_between_chunks(mut self, part: Option<String>) -> Self {
        if self.cap.is_some() {
            self.side_by_side_cap = part;
        }
        self
    }

    /// The tools for `at_once` chunk encodes running side by side, if that is more than one.
    pub fn for_chunks_at_once(&self, at_once: usize) -> Option<Tools> {
        (at_once > 1).then(|| Tools {
            ffmpeg: self.ffmpeg.clone(),
            ffprobe: self.ffprobe.clone(),
            cap: self.side_by_side_cap.clone(),
            side_by_side_cap: self.side_by_side_cap.clone(),
            hooks: self.hooks.clone(),
            version: self.version.clone(),
        })
    }

    /// What to say of an ffmpeg run that failed. Killed outright under a cap, the likely reason
    /// is the cap, and the log says nothing of it.
    fn failure(&self, status: std::process::ExitStatus, log: &str, lines: usize) -> anyhow::Error {
        use std::os::unix::process::ExitStatusExt;
        let why = match &self.cap {
            Some(cap) if status.signal() == Some(libc::SIGKILL) => {
                format!(": it was killed, most likely for going over its memory cap of {cap}; raise --memory-limit, or lower --workers if chunks are encoded side by side")
            }
            _ => String::new(),
        };
        anyhow::anyhow!("ffmpeg failed ({status}){why}:\n{}", tail(log, lines))
    }

    /// Which ffmpeg build this is (the first line of `ffmpeg -version`; empty if it won't
    /// say). Measurements kept between runs only count for the build that made them.
    pub fn encoder_version(&self) -> &str {
        self.version.get_or_init(|| {
            let ffmpeg = Command::new(&self.ffmpeg)
                .args(["-hide_banner", "-version"])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .ok()
                .and_then(|o| String::from_utf8_lossy(&o.stdout).lines().next().map(str::to_owned))
                .unwrap_or_default();
            // An ffmpeg that loads the encoder from the system stays the same ffmpeg when
            // the encoder is updated; the encoder names itself when it is started.
            let one_frame = ["-hide_banner", "-f", "lavfi", "-i", "color=black:s=64x64:r=1", "-frames:v", "1", "-c:v", "libsvtav1", "-preset", "13", "-f", "null", "-"];
            let svt = Command::new(&self.ffmpeg).args(one_frame).stdin(Stdio::null()).stdout(Stdio::null()).output().ok().and_then(|o| svt_version(&String::from_utf8_lossy(&o.stderr)));
            match svt {
                Some(v) => format!("{ffmpeg} | {v}"),
                None => ffmpeg,
            }
        })
    }

    /// Builds the ffmpeg command; with AV1OPT_DEBUG set, prints it first.
    fn command_for(&self, args: &[String]) -> Command {
        if std::env::var_os("AV1OPT_DEBUG").is_some() && !args.is_empty() {
            let quoted: Vec<String> = args.iter().map(|a| if a.contains([' ', ';', '[']) { format!("'{a}'") } else { a.clone() }).collect();
            eprintln!("+ ffmpeg {}", quoted.join(" "));
        }
        let mut c = match &self.cap {
            Some(cap) => {
                let mut c = Command::new("systemd-run");
                // --collect: an ffmpeg killed before systemd had put it in its scope (a cancel
                // at the wrong moment) leaves a failed unit, which would otherwise stay listed.
                c.args(["--user", "--scope", "--collect", "-q", "-p", &format!("MemoryMax={cap}"), "-p", "MemorySwapMax=0"]).arg(&self.ffmpeg);
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
        let child = cmd.spawn().map_err(|e| match e.kind() {
            // What `path_arg` makes of a name that cannot be passed on.
            std::io::ErrorKind::InvalidInput => anyhow::anyhow!("{NOT_TEXT}"),
            _ => anyhow::Error::new(e).context("failed to start ffmpeg"),
        })?;
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
        self.run_in(None, args)
    }

    /// Like [`Tools::run`], with ffmpeg's working directory set to `dir`.
    fn run_in(&self, dir: Option<&Path>, args: &[String]) -> Result<String> {
        let mut cmd = self.command_for(args);
        if let Some(d) = dir {
            cmd.current_dir(d);
        }
        cmd.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
        let child = self.spawn(&mut cmd)?;
        let out = self.finish(child)?;
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        if !out.status.success() {
            return Err(self.failure(out.status, &stderr, 15));
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
            Some(st) => Err(self.failure(st, &stderr, 5)),
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
        self.run_with_progress_log(dir, args, total_secs, label).map(|(frames, _)| frames)
    }

    /// Like [`Tools::run_with_progress_in`], also returning what ffmpeg wrote to its log
    /// (errors only, unless `args` raise the level with `-v`).
    pub fn run_with_progress_log(&self, dir: Option<&Path>, args: &[String], total_secs: f64, label: &str) -> Result<(u64, String)> {
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
        let listened_to = self.hooks.as_ref().is_some_and(|h| !h.is_on_terminal());
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
                if listened_to {
                    self.emit(Event::Progress { label: label.to_string(), frac, fps: fps.trim().parse().unwrap_or(0.0), eta_secs: eta });
                } else {
                    eprint!("\r{label} {:5.1}%  {fps:>6} fps  ETA {}   ", frac * 100.0, hms(eta));
                }
            }
        }
        if !listened_to {
            eprintln!();
        }
        let out = self.finish(child)?;
        let status = out.status;
        let stderr = err_reader.join().unwrap_or_default();
        if !status.success() {
            return Err(self.failure(status, &stderr, 15));
        }
        Ok((frames, stderr))
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

/// (frame number, score) of every frame in a libvmaf JSON log.
pub(crate) fn parse_vmaf_frames(log: &[u8]) -> Result<Vec<(u64, f64)>> {
    let v: serde_json::Value = serde_json::from_slice(log).context("libvmaf's log is not JSON")?;
    let frames = v["frames"].as_array().context("no frames in libvmaf's log")?;
    frames
        .iter()
        .map(|f| Some((f["frameNum"].as_u64()?, f["metrics"]["vmaf"].as_f64()?)))
        .collect::<Option<Vec<_>>>()
        .context("a frame without a score in libvmaf's log")
}

impl Tools {
    /// VMAF of every scored frame of `distorted`, which holds the clips of the concat list
    /// `references` end to end: (frame number, score) pairs. Frames are paired by index, as in
    /// [`Tools::vmaf`]. libvmaf's log is written in `dir` and removed.
    #[allow(clippy::too_many_arguments)]
    pub fn vmaf_frames(&self, distorted: &Path, references: &Path, dir: &Path, model: &str, pix_fmt: &str, threads: usize, subsample: u32) -> Result<Vec<(u64, f64)>> {
        let reference = ["-f".into(), "concat".into(), "-safe".into(), "0".into(), "-i".into(), path_arg(references)];
        self.vmaf_log(distorted, &reference, dir, model, pix_fmt, threads, subsample)
    }

    /// VMAF of every scored frame of `distorted` against the one clip `reference`, as
    /// [`Tools::vmaf_frames`].
    #[allow(clippy::too_many_arguments)]
    pub fn vmaf_each(&self, distorted: &Path, reference: &Path, dir: &Path, model: &str, pix_fmt: &str, threads: usize, subsample: u32) -> Result<Vec<(u64, f64)>> {
        self.vmaf_log(distorted, &["-i".into(), path_arg(reference)], dir, model, pix_fmt, threads, subsample)
    }

    #[allow(clippy::too_many_arguments)]
    fn vmaf_log(&self, distorted: &Path, reference: &[String], dir: &Path, model: &str, pix_fmt: &str, threads: usize, subsample: u32) -> Result<Vec<(u64, f64)>> {
        // By relative name, with ffmpeg run in `dir`: a path in a filter graph needs escaping.
        // One log per thread, so runs in the same folder at the same time keep apart.
        let log_name = format!("vmaf-frames-{:?}.json", std::thread::current().id()).replace(['(', ')'], "");
        let filter = format!(
            "[0:v]format={pix_fmt},settb=1,setpts=N[d];[1:v]format={pix_fmt},settb=1,setpts=N[r];\
             [d][r]libvmaf=model=version={model}:n_threads={threads}:n_subsample={}:shortest=1:log_fmt=json:log_path={log_name}",
            subsample.max(1)
        );
        let args: Vec<String> = ["-i".into(), path_arg(distorted)]
            .into_iter()
            .chain(reference.iter().cloned())
            .chain(["-lavfi", &filter, "-f", "null", "-"].map(String::from))
            .collect();
        self.run_in(Some(dir), &args)?;
        let log = dir.join(log_name);
        let text = std::fs::read(&log).context("libvmaf wrote no log")?;
        let _ = std::fs::remove_file(&log);
        parse_vmaf_frames(&text)
    }

    /// Whether each packet (= frame) of the first video stream is a key frame, in the order
    /// they are stored, without decoding.
    pub fn key_packets(&self, path: &Path) -> Result<Vec<bool>> {
        let mut command = Command::new(&self.ffprobe);
        command.args(["-v", "error", "-select_streams", "v:0", "-show_entries", "packet=flags", "-of", "csv=p=0"]).arg(probe_arg(path));
        let out = crate::probe::run_ffprobe(command, crate::probe::Reads::all_of(path))?;
        if !out.status.success() {
            bail!("cannot read the packets of {}", path.display());
        }
        Ok(String::from_utf8_lossy(&out.stdout).lines().filter(|l| !l.trim().is_empty()).map(|l| l.contains('K')).collect())
    }

    /// Number of video packets (= frames) in the first video stream, without decoding. A file
    /// with no frames in it counts none; one that cannot be read is an error.
    pub fn count_frames(&self, path: &Path) -> Result<u64> {
        let mut command = Command::new(&self.ffprobe);
        command.args(["-v", "error", "-select_streams", "v:0", "-count_packets", "-show_entries", "stream=nb_read_packets", "-of", "csv=p=0"]).arg(probe_arg(path));
        let out = crate::probe::run_ffprobe(command, crate::probe::Reads::all_of(path))?;
        frames_counted(&out, path)
    }
}

/// What ffprobe's count of packets comes to. ffmpeg leaves a clip it had no frame for as a
/// header with nothing behind it, and ffprobe ends such a file with "End of file".
fn frames_counted(out: &Output, path: &Path) -> Result<u64> {
    let said = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        if crate::probe::one_line(&said).ends_with("End of file") {
            return Ok(0);
        }
        bail!("{}: {}", cannot_count(path), crate::probe::reason(&said, Path::new(&probe_arg(path))));
    }
    let count = String::from_utf8_lossy(&out.stdout);
    let count = count.trim().trim_end_matches(',');
    if count.is_empty() {
        return Ok(0);
    }
    count.parse().with_context(|| format!("{}: ffprobe said {count:?}", cannot_count(path)))
}

/// Every file counted is one av1opt has just written, so the original is still as it was.
fn cannot_count(path: &Path) -> String {
    let name = path.file_name().unwrap_or(path.as_os_str()).to_string_lossy();
    format!("the original is untouched, but the frames of {name}, a file av1opt had just made from it, could not be counted (running it again may get past this)")
}

/// Why a file cannot be worked on, where its name is not text.
/// How a message ends when a finished encode was turned down.
pub const NOT_WRITTEN: &str = "no output was written and the original is untouched";

/// How a message ends when av1opt or ffmpeg went wrong in a way that ends the encode, and
/// the user has nothing to change but can report it.
pub const FAILED_INSIDE: &str = "this went wrong inside av1opt or ffmpeg, no output was written and the original is untouched. AV1OPT_DEBUG=1 prints the ffmpeg commands and --keep-temp keeps the files for a bug report";

/// The same where the encode goes on: a stretch that cannot be encoded again leaves the
/// video to the spot checks, and the message ends up in brackets in the note that says so.
pub const WENT_WRONG_INSIDE: &str = "this went wrong inside av1opt or ffmpeg; AV1OPT_DEBUG=1 prints the ffmpeg commands and --keep-temp keeps the files for a bug report";

pub const NOT_TEXT: &str = "its path is not valid UTF-8, which av1opt cannot pass on; rename it";

/// A path as an ffmpeg argument. Absolute, because ffmpeg takes what stands before a colon in
/// a relative name for a protocol (`a:b/x.mkv`: "Protocol not found").
///
/// Arguments are text here. A name that is not cannot be written as text without becoming
/// another name, possibly that of another file; it gets a NUL, which no file name holds and
/// no program can be started with, so that ffmpeg never opens the wrong file (see `spawn`).
pub fn path_arg(p: &Path) -> String {
    let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    match abs.to_str() {
        Some(s) => s.to_owned(),
        None => format!("{}\0", abs.to_string_lossy()),
    }
}

/// A path as an ffprobe argument: absolute for the same reason, and so that a name that
/// starts with a dash (`-i.mp4`, given after `--` or found in a folder) is not an option.
pub fn probe_arg(p: &Path) -> std::ffi::OsString {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf()).into_os_string()
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

/// What SVT-AV1 calls itself in what ffmpeg prints as it starts the encoder
/// (`Svt[info]: SVT [version]:\tSVT-AV1 Encoder Lib v4.2.0`).
fn svt_version(log: &str) -> Option<String> {
    let v = log.lines().find_map(|l| l.split_once("SVT [version]:"))?.1.trim();
    (!v.is_empty()).then(|| v.to_string())
}

/// A real ffmpeg and ffprobe for a test that needs them (`AV1OPT_FFMPEG`, `AV1OPT_FFPROBE`).
/// Without them such a test says so and passes: the plain `cargo test` needs no ffmpeg.
#[cfg(test)]
pub(crate) fn real_tools(test: &str) -> Option<Tools> {
    match (std::env::var("AV1OPT_FFMPEG"), std::env::var("AV1OPT_FFPROBE")) {
        (Ok(ffmpeg), Ok(ffprobe)) => Some(Tools::new(ffmpeg, ffprobe, None)),
        _ => {
            eprintln!("{test}: skipped, AV1OPT_FFMPEG and AV1OPT_FFPROBE are not set");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capped(cap: &str, part: &str) -> Tools {
        Tools { ffmpeg: "ffmpeg".into(), ffprobe: "ffprobe".into(), cap: Some(cap.into()), side_by_side_cap: Some(part.into()), hooks: None, version: Default::default() }
    }

    fn cap_of(tools: &Tools) -> String {
        let c = tools.command_for(&[]);
        assert_eq!(c.get_program(), "systemd-run");
        c.get_args().filter_map(|a| a.to_str()?.strip_prefix("MemoryMax=")).collect()
    }

    #[test]
    fn only_chunks_encoded_side_by_side_share_the_memory_cap() {
        let tools = capped("12G", "6144M");
        assert_eq!(cap_of(&tools), "12G");
        assert!(tools.for_chunks_at_once(1).is_none(), "one at a time has all of it");
        assert_eq!(cap_of(&tools.for_chunks_at_once(2).unwrap()), "6144M");
        // No cap in force stays no cap, whatever part was worked out.
        let free = Tools { cap: None, side_by_side_cap: None, ..capped("1G", "1G") }.sharing_between_chunks(Some("1M".into()));
        assert_eq!(free.for_chunks_at_once(2).unwrap().command_for(&[]).get_program(), "ffmpeg");
    }

    #[test]
    fn being_killed_under_a_cap_is_put_down_to_the_cap() {
        use std::os::unix::process::ExitStatusExt;
        let killed = std::process::ExitStatus::from_raw(libc::SIGKILL);
        let said = capped("12G", "6G").for_chunks_at_once(2).unwrap().failure(killed, "the log", 5).to_string();
        assert!(said.contains("--memory-limit") && said.contains("cap of 6G") && said.contains("the log"), "{said}");
        // Without a cap, or for another ending, there is nothing to blame on one.
        let free = Tools { cap: None, ..capped("1G", "1G") };
        assert!(!free.failure(killed, "", 5).to_string().contains("--memory-limit"));
        let ended = std::process::ExitStatus::from_raw(1 << 8);
        assert!(!capped("1G", "1G").failure(ended, "", 5).to_string().contains("--memory-limit"));
    }

    #[test]
    fn a_clip_without_frames_counts_none_and_a_failure_stays_a_failure() {
        use std::os::unix::process::ExitStatusExt;
        let path = Path::new("/v/clip.mkv");
        let said = |code: i32, stdout: &str, stderr: &str| Output { status: std::process::ExitStatus::from_raw(code << 8), stdout: stdout.into(), stderr: stderr.into() };
        assert_eq!(frames_counted(&said(0, "120,\n", ""), path).unwrap(), 120);
        assert_eq!(frames_counted(&said(0, "", ""), path).unwrap(), 0);
        // What ffprobe says of the header ffmpeg leaves when it had no frame to write.
        assert_eq!(frames_counted(&said(1, "", "[matroska @ 0x1] Duplicate element\n/v/clip.mkv: End of file\n"), path).unwrap(), 0);
        let e = frames_counted(&said(1, "", "/v/clip.mkv: Input/output error\n"), path).unwrap_err().to_string();
        assert!(e.starts_with("the original is untouched") && e.contains("clip.mkv") && e.ends_with("): Input/output error"), "{e}");
        assert!(frames_counted(&said(0, "many", ""), path).is_err());
    }

    #[test]
    fn a_name_that_is_not_text_never_reaches_ffmpeg_as_another_name() {
        use std::os::unix::ffi::OsStrExt;
        let odd = Path::new(std::ffi::OsStr::from_bytes(b"/v/a\xff.mkv"));
        assert_eq!(path_arg(Path::new("/v/a b.mkv")), "/v/a b.mkv");
        assert_ne!(path_arg(odd), "/v/a\u{fffd}.mkv");
        let tools = Tools::new("true".into(), "true".into(), None);
        let e = tools.run(&["-i".into(), path_arg(odd)]).unwrap_err();
        assert_eq!(e.to_string(), NOT_TEXT);
        // An ffmpeg that is not there is still reported as that.
        let none = Tools::new("/nonexistent/ffmpeg".into(), "true".into(), None);
        assert!(none.run(&[]).unwrap_err().to_string().contains("failed to start ffmpeg"));
    }

    #[test]
    fn the_encoder_names_itself() {
        let log = "Svt[info]: -------------------------------------------\nSvt[info]: SVT [version]:\tSVT-AV1 Encoder Lib v4.2.0-210-gf2c3fb786\nSvt[info]: SVT [build]  :\tGCC 16.2.0\t 64 bit\n";
        assert_eq!(svt_version(log).as_deref(), Some("SVT-AV1 Encoder Lib v4.2.0-210-gf2c3fb786"));
        assert_eq!(svt_version("Unknown encoder 'libsvtav1'\n"), None);
        assert_eq!(svt_version("Svt[info]: SVT [version]:\n"), None);
    }
}
