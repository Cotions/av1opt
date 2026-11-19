//! Runs ffmpeg with resource limits, progress reporting and VMAF parsing.

use crate::events::{Cancelled, Event, Hooks};
use anyhow::{Context, Result, bail};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

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

/// How long a short question to a tool (its version, its encoders, one black frame) may take.
/// Each takes well under a second, also on a machine busy with two encodes (0.05 s for the
/// frame at a load of 17 on 12 threads); a program that hangs, as a wrong path pointing at
/// something else may, is given up on.
const QUESTION_LIMIT: std::time::Duration = std::time::Duration::from_secs(10);

/// What `command` printed, its output and its errors, if it ended within `limit`. One still
/// going then is killed, and there is no answer.
fn answer_within(mut command: Command, limit: std::time::Duration) -> Option<(std::process::ExitStatus, String, String)> {
    let mut child = command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().ok()?;
    let read = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut s = String::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_string(&mut s);
            }
            s
        })
    };
    let (stdout, stderr) = (read(child.stdout.take().map(|p| Box::new(p) as _)), read(child.stderr.take().map(|p| Box::new(p) as _)));
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() < limit => std::thread::sleep(std::time::Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    Some((status, stdout.join().ok()?, stderr.join().ok()?))
}

impl Capabilities {
    pub fn probe(ffmpeg: &str, ffprobe: &str) -> Self {
        let out = |prog: &str, arg: &str| -> Option<String> {
            let mut command = Command::new(prog);
            command.args(["-hide_banner", arg]);
            let (status, text, _) = answer_within(command, QUESTION_LIMIT)?;
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

    /// Why videos cannot be read with these tools, if they cannot: all that a run needs that
    /// encodes nothing.
    pub fn cannot_read(&self, ffmpeg: &str, ffprobe: &str) -> Option<String> {
        if self.ffmpeg_version.is_none() {
            return Some(format!("cannot run ffmpeg ({ffmpeg})"));
        }
        (!self.ffprobe_ok).then(|| format!("cannot run ffprobe ({ffprobe})"))
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

/// The containers a video may come in, by the names ffmpeg knows them under. A file is opened
/// as what its contents look like, whatever it is called, and some of what ffmpeg reads is no
/// video but a list of other files: a DASH manifest or a concat playlist named `.mkv` would
/// have ffmpeg read those and encode them into the output. None of these leads anywhere else.
///
/// Matroska and WebM, the MP4 family, AVI, ASF (`.wmv`), FLV, MPEG transport and program
/// streams (`.ts`, `.m2ts`, `.mts`, `.mpg`, `.vob`) are what the scan looks for. Ogg, RealMedia, WTV,
/// MXF, DV and NUT are what else is found in libraries and can be named to `encode`, as are
/// bare streams: MPEG video, MPEG-4 (`.m4v`), H.264, HEVC, VC-1, YUV4MPEG, AV1 as IVF or OBU.
/// The last two are also what av1opt's own chunks and repairs are stored as.
pub const VIDEO_FORMATS: &str = "matroska,webm,mov,mp4,m4a,3gp,3g2,mj2,avi,asf,flv,mpegts,mpeg,ogg,rm,wtv,mxf,dv,nut,mpegvideo,m4v,h264,hevc,vc1,yuv4mpegpipe,ivf,obu,av1";

/// A progress line. On a terminal it is written over in place; anywhere else (a log file, a
/// pipe) carriage returns would pile up on one endless line, so a plain line is written now
/// and then instead.
pub struct Ticker {
    on_terminal: bool,
    last: std::sync::Mutex<Option<Instant>>,
}

impl Ticker {
    /// How often a plain line is written where nobody watches it change.
    const PLAIN_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

    pub fn new() -> Ticker {
        use std::io::IsTerminal;
        Ticker { on_terminal: std::io::stderr().is_terminal(), last: std::sync::Mutex::new(None) }
    }

    pub fn show(&self, text: &str) {
        if self.on_terminal {
            eprint!("\r{text}   ");
            return;
        }
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_none_or(|at| at.elapsed() >= Self::PLAIN_EVERY) {
            *last = Some(Instant::now());
            eprintln!("{text}");
        }
    }

    /// Ends the line written over in place.
    pub fn end(&self) {
        if self.on_terminal {
            eprintln!();
        }
    }
}

/// `-format_whitelist` and `-protocol_whitelist` for an input forced to `format` (`-f`), or for
/// a video whose format ffmpeg is to find out. Files only: nothing av1opt opens is reached
/// through a network, a pipe or anything else.
fn input_limits(format: Option<&str>) -> [String; 4] {
    let formats = match format {
        None => VIDEO_FORMATS,
        // av1opt's own list of its own lossless clips: the list, and the clips it names.
        Some("concat") => "concat,matroska",
        // Named by av1opt itself (its chapters and cut subtitles, generated test pictures).
        Some(named) => named,
    };
    ["-format_whitelist".into(), formats.into(), "-protocol_whitelist".into(), "file".into()]
}

/// `args` with the limits in front of every input. Done here, on the way to ffmpeg, so that no
/// call can leave them out.
fn limit_inputs(args: &[String]) -> Vec<String> {
    let mut limited = Vec::with_capacity(args.len() + 4);
    let mut format = None;
    let mut arg = args.iter().peekable();
    while let Some(a) = arg.next() {
        match a.as_str() {
            "-f" => format = arg.peek().map(|f| f.as_str()),
            "-i" => limited.extend(input_limits(format.take())),
            _ => {}
        }
        limited.push(a.clone());
    }
    limited
}

/// An ffprobe command that can only read a file in one of [`VIDEO_FORMATS`].
pub fn probe_command(ffprobe: &str) -> Command {
    let mut command = Command::new(ffprobe);
    command.args(input_limits(None));
    command
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

/// How long an ffmpeg may go without moving before it is stopped. An encode at its slowest
/// finishes a frame, or reads the next, in far less; a file or drive that stopped answering
/// would otherwise hold the run for ever.
const STALL_LIMIT: Duration = Duration::from_secs(600);

/// The lines of `-progress` that change when work gets done. The others do not say so: ffmpeg
/// keeps printing all of them on time, the same values, while it is stuck.
const MOVING_PROGRESS: [&str; 3] = ["frame=", "out_time_us=", "total_size="];

/// How many times [`STALL_LIMIT`] an ffmpeg gets while it keeps the processor busy. It is working
/// on something that has no result to show yet: the largest pictures at the slowest presets
/// take minutes a frame, and the encoder hands frames over in groups. One spinning for ever on
/// a damaged file still ends.
const BUSY_STALL_FACTOR: u32 = 6;

/// What `/proc/<pid>/io` counts as read and written so far by the whole process.
fn bytes_moved(pid: u32) -> Option<(u64, u64)> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/io")).ok()?;
    let field = |name: &str| text.lines().find_map(|l| l.strip_prefix(name)?.trim().parse().ok());
    Some((field("rchar:")?, field("wchar:")?))
}

/// Processor time used so far by all the threads of process `pid`.
fn processor_time(pid: u32) -> Option<Duration> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Counted from the bracket that ends the program's name: user time is field 14, system
    // time field 15, and the first after the bracket is field 3.
    let mut fields = stat.rsplit_once(')')?.1.split_whitespace().skip(11);
    let ticks = fields.next()?.parse::<u64>().ok()? + fields.next()?.parse::<u64>().ok()?;
    // SAFETY: sysconf takes a number and touches no memory of ours.
    let per_second = u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).ok().filter(|t| *t > 0)?;
    Some(Duration::from_millis(ticks * 1000 / per_second))
}

/// What has been seen of a run, to tell whether it has stood still for too long.
#[derive(Default)]
struct Stillness {
    signs: Option<[u64; 3]>,
    last_move: Duration,
    processor_at_move: Duration,
}

impl Stillness {
    /// Takes in what the run shows `worked` into it. True once it has shown nothing new for
    /// `limit`, or for [`BUSY_STALL_FACTOR`] times that if it has used at least a quarter of a
    /// processor meanwhile.
    fn too_long(&mut self, signs: [u64; 3], processor: Duration, worked: Duration, limit: Duration) -> bool {
        if self.signs != Some(signs) {
            (self.signs, self.last_move, self.processor_at_move) = (Some(signs), worked, processor);
            return false;
        }
        let still = worked.saturating_sub(self.last_move);
        let busy = processor.saturating_sub(self.processor_at_move) * 4 >= still;
        still >= if busy { limit * BUSY_STALL_FACTOR } else { limit }
    }
}

/// Picks out of ffmpeg's `-progress` lines the ones that say something new.
#[derive(Default)]
struct ProgressLines([String; MOVING_PROGRESS.len()]);

impl ProgressLines {
    fn moved(&mut self, line: &str) -> bool {
        let Some(i) = MOVING_PROGRESS.iter().position(|m| line.starts_with(m)) else { return false };
        let new = self.0[i] != line;
        if new {
            line.clone_into(&mut self.0[i]);
        }
        new
    }
}

/// Kills an ffmpeg that has stopped moving, for as long as this value lives. It has moved when
/// it read anything, when its reported progress changed, or, on a run that reports none, when
/// it wrote anything. On a run that reports progress the writes say nothing, as the reports
/// are writes themselves. Time spent paused does not count.
struct Watchdog {
    /// Dropping this ends the watch at once, however the run went.
    _running: mpsc::Sender<()>,
    stalled: Arc<AtomicBool>,
    progress: Arc<AtomicU64>,
}

impl Watchdog {
    fn start(pid: u32, limit: Duration, hooks: Option<Arc<Hooks>>, reports_progress: bool) -> Self {
        let (running, ended) = mpsc::channel::<()>();
        let (stalled, progress) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicU64::new(0)));
        let (stall, moved) = (stalled.clone(), progress.clone());
        std::thread::spawn(move || {
            let paused = || hooks.as_ref().map_or(Duration::ZERO, |h| h.paused_time());
            let (began, paused_before, identity) = (Instant::now(), paused(), crate::files::process_start(pid));
            let worked = || began.elapsed().saturating_sub(paused().saturating_sub(paused_before));
            let mut stillness = Stillness::default();
            while let Err(mpsc::RecvTimeoutError::Timeout) = ended.recv_timeout((limit / 4).clamp(Duration::from_millis(50), Duration::from_secs(5))) {
                // Counters that cannot be read prove nothing, so they never condemn a run.
                let (Some((read, written)), Some(processor)) = (bytes_moved(pid), processor_time(pid)) else {
                    stillness = Stillness { last_move: worked(), ..Stillness::default() };
                    continue;
                };
                let signs = [read, if reports_progress { 0 } else { written }, moved.load(Ordering::Relaxed)];
                if stillness.too_long(signs, processor, worked(), limit) {
                    // Still the process that was started, not one given its number since.
                    if identity.is_some() && crate::files::process_start(pid) == identity {
                        stall.store(true, Ordering::SeqCst);
                        // SAFETY: kill takes plain numbers and touches no memory of ours.
                        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
                    }
                    return;
                }
            }
        });
        Watchdog { _running: running, stalled, progress }
    }

    fn progress_moved(&self) {
        self.progress.fetch_add(1, Ordering::Relaxed);
    }

    fn stalled(&self) -> bool {
        self.stalled.load(Ordering::SeqCst)
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

/// Waits for a child and says how it ended and how much processor time it used.
fn wait_with_processor_time(pid: u32) -> std::io::Result<(std::process::ExitStatus, f64)> {
    use std::os::unix::process::ExitStatusExt;
    let mut status = 0;
    // SAFETY: an all-zero rusage is valid, and wait4 only writes to the two places it is given.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: as above; the pointers live across the call.
        if unsafe { libc::wait4(pid as libc::pid_t, &mut status, 0, &mut usage) } >= 0 {
            break;
        }
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
    let secs = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    Ok((std::process::ExitStatus::from_raw(status), secs(usage.ru_utime) + secs(usage.ru_stime)))
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
    /// Whether ffmpeg and ffprobe can open files by way of av1opt's descriptors, asked once.
    reach: std::sync::OnceLock<bool>,
    /// How long a run may go without moving, see [`Watchdog`].
    stall_limit: Duration,
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
        Tools { ffmpeg, ffprobe, side_by_side_cap: cap.clone(), cap, hooks: None, version: std::sync::OnceLock::new(), reach: std::sync::OnceLock::new(), stall_limit: STALL_LIMIT }
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
            reach: self.reach.clone(),
            stall_limit: self.stall_limit,
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
            let ask = |args: &[&str]| {
                let mut command = Command::new(&self.ffmpeg);
                command.args(args);
                answer_within(command, QUESTION_LIMIT)
            };
            let ffmpeg = ask(&["-hide_banner", "-version"]).and_then(|(_, out, _)| out.lines().next().map(str::to_owned)).unwrap_or_default();
            // An ffmpeg that loads the encoder from the system stays the same ffmpeg when
            // the encoder is updated; the encoder names itself when it is started.
            let one_frame = ["-hide_banner", "-f", "lavfi", "-i", "color=black:s=64x64:r=1", "-frames:v", "1", "-c:v", "libsvtav1", "-preset", "13", "-f", "null", "-"];
            let svt = ask(&one_frame).and_then(|(_, _, errors)| svt_version(&errors));
            match svt {
                Some(v) => format!("{ffmpeg} | {v}"),
                None => ffmpeg,
            }
        })
    }

    /// Whether ffmpeg and ffprobe, started as every run starts them, can open a file in
    /// `folder`, a path of one of av1opt's descriptors (see [`crate::files::HeldFolder`]). Run
    /// in another view of the system's processes (a sandbox or a container of their own) they
    /// find another process or none under that number. Asked once, with a file av1opt writes
    /// there and the tools read back; a tool that does not answer within the time of a short
    /// question counts as one that cannot.
    pub fn reach(&self, folder: &Path) -> bool {
        *self.reach.get_or_init(|| {
            let Ok(token) = crate::files::random_text(8) else { return false };
            let file = folder.join(".av1opt-reach-check");
            if crate::files::write_new(&file, token.as_bytes()).is_err() {
                return false;
            }
            let read = path_arg(&file);
            let ffmpeg = self.command_for(&["-v", "error"], &["-f", "data", "-i", &read, "-map", "0", "-c", "copy", "-f", "data", "-"].map(String::from));
            let ffmpeg_reads = answer_within(ffmpeg, QUESTION_LIMIT).is_some_and(|(status, out, _)| status.success() && out == token);
            let mut ffprobe = Command::new(&self.ffprobe);
            ffprobe.args(["-v", "error", "-f", "data", "-show_entries", "format=size", "-of", "csv=p=0"]).arg(&read);
            let ffprobe_reads = ffmpeg_reads && answer_within(ffprobe, QUESTION_LIMIT).is_some_and(|(status, out, _)| status.success() && out.trim() == token.len().to_string());
            let _ = std::fs::remove_file(&file);
            ffprobe_reads
        })
    }

    /// Builds the ffmpeg command, `first` and then `args` with their inputs limited; with
    /// AV1OPT_DEBUG set, prints it first.
    fn command_for(&self, first: &[&str], args: &[String]) -> Command {
        let args = limit_inputs(args);
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
        // SVT-AV1 prints its config banner to stderr on every run; errors only. No running
        // statistics either: they are writes, and would look like work to the watchdog.
        c.env("SVT_LOG", "1").arg("-hide_banner").arg("-nostdin").arg("-nostats").args(first).args(args);
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

    fn watchdog(&self, child: &Child, reports_progress: bool) -> Watchdog {
        Watchdog::start(child.id(), self.stall_limit, self.hooks.clone(), reports_progress)
    }

    /// What a run the watchdog stopped failed of.
    fn stalled(&self) -> anyhow::Error {
        let secs = self.stall_limit.as_secs();
        let time = if secs >= 60 { format!("{} minutes", secs / 60) } else { format!("{secs} seconds") };
        anyhow::anyhow!("ffmpeg stopped making progress: it read, wrote and encoded nothing for {time}, so it was stopped. The file may be damaged or its drive may have stalled; the original has not been touched")
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
        let mut cmd = self.command_for(&[], args);
        if let Some(d) = dir {
            cmd.current_dir(d);
        }
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
        let child = self.spawn(&mut cmd)?;
        let watchdog = self.watchdog(&child, false);
        let out = self.finish(child)?;
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        if !out.status.success() {
            // Asked first: the watchdog kills outright, as going over a memory cap does.
            if watchdog.stalled() {
                return Err(self.stalled());
            }
            return Err(self.failure(out.status, &stderr, 15));
        }
        Ok(stderr)
    }

    /// Runs ffmpeg like [`Tools::run`] and returns the processor time it used, user and system,
    /// in seconds. Asked of this one process, not of all children: the page's server has others
    /// running at the same time.
    pub fn run_timed(&self, args: &[String]) -> Result<f64> {
        let mut cmd = self.command_for(&[], args);
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
        let mut child = self.spawn(&mut cmd)?;
        let watchdog = self.watchdog(&child, false);
        let id = child.id();
        // Read beside the wait: ffmpeg would stop once the pipe is full.
        let mut pipe = child.stderr.take().context("ffmpeg's error output is not connected")?;
        let reader = std::thread::spawn(move || {
            let mut text = Vec::new();
            let _ = pipe.read_to_end(&mut text);
            text
        });
        let waited = wait_with_processor_time(id);
        let stderr = reader.join().unwrap_or_default();
        if let Some(h) = &self.hooks {
            h.untrack(id);
        }
        self.check_cancel()?;
        let (status, secs) = waited.context("cannot wait for ffmpeg")?;
        if !status.success() {
            if watchdog.stalled() {
                return Err(self.stalled());
            }
            return Err(self.failure(status, &String::from_utf8_lossy(&stderr), 15));
        }
        Ok(secs)
    }

    /// Runs ffmpeg and returns what it wrote to stdout. A run still going after `timeout` (a
    /// damaged file, a stalled network drive) is stopped and counts as failed.
    pub fn run_capture(&self, args: &[String], timeout: std::time::Duration) -> Result<Vec<u8>> {
        let mut cmd = self.command_for(&[], args);
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
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
            None => bail!("{NO_ANSWER} {} s", timeout.as_secs()),
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
        let mut cmd = self.command_for(&["-progress", "pipe:1", "-v", "error"], args);
        if let Some(d) = dir {
            cmd.current_dir(d);
        }
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = self.spawn(&mut cmd)?;
        let watchdog = self.watchdog(&child, true);
        let mut stderr = child.stderr.take().unwrap();
        let err_reader = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = stderr.read_to_string(&mut s);
            s
        });

        let watch = self.stopwatch();
        let listened_to = self.hooks.as_ref().is_some_and(|h| !h.is_on_terminal());
        let (mut done_secs, mut fps, mut frames) = (0.0_f64, String::new(), 0u64);
        let mut progress_lines = ProgressLines::default();
        let ticker = Ticker::new();
        // Read errors end the loop instead of returning, so the child is always reaped
        // (and untracked) below.
        for line in BufReader::new(child.stdout.take().unwrap()).lines().map_while(Result::ok) {
            if progress_lines.moved(&line) {
                watchdog.progress_moved();
            }
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
                    ticker.show(&format!("{label} {:5.1}%  {fps:>6} fps  ETA {}", frac * 100.0, hms(eta)));
                }
            }
        }
        if !listened_to {
            ticker.end();
        }
        let out = self.finish(child)?;
        let status = out.status;
        let stderr = err_reader.join().unwrap_or_default();
        if !status.success() {
            if watchdog.stalled() {
                return Err(self.stalled());
            }
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
        let mut command = probe_command(&self.ffprobe);
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
        let mut command = probe_command(&self.ffprobe);
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

/// How the error of an ffmpeg run stopped for taking too long begins.
pub const NO_ANSWER: &str = "ffmpeg gave no answer within";

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
        Tools { ffmpeg: "ffmpeg".into(), ffprobe: "ffprobe".into(), cap: Some(cap.into()), side_by_side_cap: Some(part.into()), hooks: None, version: Default::default(), reach: Default::default(), stall_limit: STALL_LIMIT }
    }

    fn cap_of(tools: &Tools) -> String {
        let c = tools.command_for(&[], &[]);
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
        assert_eq!(free.for_chunks_at_once(2).unwrap().command_for(&[], &[]).get_program(), "ffmpeg");
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

    /// Tools whose ffmpeg is `script`, stopped after `limit` seconds without moving.
    fn stand_in(name: &str, script: &str, limit: u64) -> (Tools, std::path::PathBuf) {
        let dir = room(name);
        let ffmpeg = dir.join("ffmpeg");
        crate::files::write_script(&ffmpeg, script);
        let mut tools = Tools::new(ffmpeg.to_string_lossy().into_owned(), "ffprobe".into(), None);
        tools.stall_limit = Duration::from_secs(limit);
        (tools, dir)
    }

    fn progress_run(tools: &Tools) -> Result<(u64, String)> {
        tools.run_with_progress_log(None, &strings(&["out.mkv"]), 0.0, "test")
    }

    #[test]
    fn an_ffmpeg_repeating_the_same_progress_is_stopped() {
        // What a stuck ffmpeg does: the reports keep coming and say the same thing. Only shell
        // built-ins: what a program started here reads would be counted for the shell when it
        // ends. The loop has an end so that a watchdog taken in by the reports fails the test.
        let (tools, dir) = stand_in("repeats", "echo $$ > \"$(dirname \"$0\")/pid\"\ni=0\nwhile [ $i -lt 20000000 ]; do i=$((i+1)); printf 'frame=5\\nfps=0\\nout_time_us=1000\\ntotal_size=10\\nspeed=0x\\nprogress=continue\\n'; done\n", 1);
        let e = progress_run(&tools).unwrap_err().to_string();
        assert!(e.contains("stopped making progress") && e.contains("1 seconds") && e.contains("not been touched"), "{e}");
        let pid = std::fs::read_to_string(dir.join("pid")).unwrap();
        assert!(!Path::new("/proc").join(pid.trim()).exists(), "the stopped ffmpeg is gone");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_question_to_a_tool_that_hangs_is_given_up_on() {
        let dir = room("question");
        let (answers, hangs) = (dir.join("answers"), dir.join("hangs"));
        crate::files::write_script(&answers, "echo out; echo errors >&2\n");
        crate::files::write_script(&hangs, "exec sleep 600\n");
        let (status, out, errors) = answer_within(Command::new(&answers), Duration::from_secs(10)).unwrap();
        assert!(status.success() && out == "out\n" && errors == "errors\n");
        let began = Instant::now();
        assert!(answer_within(Command::new(&hangs), Duration::from_millis(300)).is_none());
        assert!(began.elapsed() < Duration::from_secs(5), "waited {:?}", began.elapsed());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_ffmpeg_that_does_nothing_at_all_is_stopped() {
        let (tools, dir) = stand_in("silent", "exec sleep 600\n", 1);
        let began = Instant::now();
        let e = tools.run(&strings(&["out.mkv"])).unwrap_err().to_string();
        assert!(e.contains("stopped making progress"), "{e}");
        let e = progress_run(&tools).unwrap_err().to_string();
        assert!(e.contains("stopped making progress"), "{e}");
        assert!(began.elapsed() < Duration::from_secs(60), "{:?}", began.elapsed());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn any_sign_of_work_puts_off_the_stop() {
        let (second, minute, limit) = (Duration::from_secs(1), Duration::from_secs(60), Duration::from_secs(600));
        let at = |minutes: u32| minute * minutes;
        // Nothing new and an idle processor: stopped when the limit is reached, not before.
        let mut s = Stillness::default();
        assert!(!s.too_long([1, 2, 3], second, at(0), limit));
        assert!(!s.too_long([1, 2, 3], second, at(9), limit));
        assert!(s.too_long([1, 2, 3], second, at(10), limit));
        // Bytes read, bytes written or progress reported each start the wait again.
        for moved in [[2, 2, 3], [1, 3, 3], [1, 2, 4]] {
            let mut s = Stillness::default();
            assert!(!s.too_long([1, 2, 3], second, at(0), limit));
            assert!(!s.too_long([1, 2, 3], second, at(9), limit));
            assert!(!s.too_long(moved, second, at(9), limit), "{moved:?}");
            assert!(!s.too_long(moved, second, at(18), limit), "{moved:?}");
            assert!(s.too_long(moved, second, at(19), limit), "{moved:?}");
        }
        // A quarter of a processor in use is work: it gets six times as long, and no more.
        let mut s = Stillness::default();
        assert!(!s.too_long([1, 2, 3], Duration::ZERO, at(0), limit));
        assert!(!s.too_long([1, 2, 3], at(15), at(59), limit));
        assert!(s.too_long([1, 2, 3], at(15), at(60), limit));
        // Less than that is what waiting costs.
        let mut s = Stillness::default();
        assert!(!s.too_long([1, 2, 3], Duration::ZERO, at(0), limit));
        assert!(s.too_long([1, 2, 3], at(2), at(10), limit));
    }

    #[test]
    fn only_progress_lines_with_something_new_count() {
        let mut lines = ProgressLines::default();
        assert!(lines.moved("frame=5"));
        assert!(!lines.moved("frame=5"));
        assert!(lines.moved("out_time_us=1000"));
        assert!(lines.moved("total_size=10"));
        assert!(!lines.moved("out_time_us=1000"));
        // These are printed anew every time, whatever is happening.
        assert!(!lines.moved("fps=3.5") && !lines.moved("speed=0.1x") && !lines.moved("progress=continue"));
        assert!(lines.moved("frame=6"));
    }

    #[test]
    fn the_processor_time_of_a_process_is_read() {
        let began = processor_time(std::process::id()).unwrap();
        let until = Instant::now() + Duration::from_millis(300);
        while Instant::now() < until {
            std::hint::black_box(0);
        }
        assert!(processor_time(std::process::id()).unwrap() > began);
        assert_eq!(processor_time(u32::MAX), None);
    }

    #[test]
    fn a_timed_run_counts_the_processor_time_of_ffmpeg_and_not_its_waiting() {
        let (tools, dir) = stand_in("timed", "i=0; while [ $i -lt 300000 ]; do i=$((i+1)); done\nsleep 0.5\n", 60);
        let t = Instant::now();
        let busy = tools.run_timed(&[]).unwrap();
        let clock = t.elapsed().as_secs_f64();
        assert!(busy > 0.05 && busy < clock - 0.3, "{busy} of {clock} seconds");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_timed_run_that_fails_says_what_ffmpeg_said() {
        let (tools, dir) = stand_in("timed-fail", "echo 'it went wrong' >&2\nexit 3\n", 60);
        let e = format!("{:#}", tools.run_timed(&[]).unwrap_err());
        assert!(e.contains("it went wrong"), "{e}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_pause_is_not_a_stall() {
        let (mut tools, dir) = stand_in("paused", "exec sleep 0.2\n", 1);
        let hooks = Arc::new(Hooks::new(|_| {}));
        tools.hooks = Some(hooks.clone());
        hooks.pause();
        let resume = hooks.clone();
        let later = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(4));
            resume.resume();
        });
        tools.run(&strings(&["out.mkv"])).unwrap();
        later.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failure_of_ffmpeg_is_not_called_a_stall() {
        let (tools, dir) = stand_in("fails", "echo 'it broke' >&2\nexit 3\n", 60);
        let e = tools.run(&strings(&["out.mkv"])).unwrap_err().to_string();
        assert!(e.contains("it broke") && !e.contains("stopped making progress"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn every_input_is_limited_to_what_it_is_meant_to_be() {
        let limits = |formats: &str| strings(&["-format_whitelist", formats, "-protocol_whitelist", "file"]);
        let expect = |parts: &[&[String]]| parts.concat();
        // A video of the user's, a second one, and what is written is left as it is.
        let args = strings(&["-y", "-ss", "3", "-i", "/v/a.mkv", "-i", "/v/b.mp4", "-map", "0:v", "-f", "ivf", "/o/c.ivf"]);
        assert_eq!(limit_inputs(&args), expect(&[&strings(&["-y", "-ss", "3"]), &limits(VIDEO_FORMATS), &strings(&["-i", "/v/a.mkv"]), &limits(VIDEO_FORMATS), &strings(&["-i", "/v/b.mp4", "-map", "0:v", "-f", "ivf", "/o/c.ivf"])]));
        // A format av1opt names for an input of its own goes for that input only.
        let args = strings(&["-f", "concat", "-safe", "0", "-i", "/t/l.txt", "-i", "/t/d.mkv", "-f", "ffmetadata", "-i", "/t/c.ffmeta"]);
        assert_eq!(
            limit_inputs(&args),
            expect(&[&strings(&["-f", "concat", "-safe", "0"]), &limits("concat,matroska"), &strings(&["-i", "/t/l.txt"]), &limits(VIDEO_FORMATS), &strings(&["-i", "/t/d.mkv", "-f", "ffmetadata"]), &limits("ffmetadata"), &strings(&["-i", "/t/c.ffmeta"])])
        );
        // Neither list holds what leads on to other files.
        for leads_on in ["concat", "dash", "hls", "image2", "sdp", "rtsp", "tty", "lavfi"] {
            assert!(!VIDEO_FORMATS.split(',').any(|f| f == leads_on), "{leads_on}");
        }
    }

    /// One short clip in `container`, made by the ffmpeg under test.
    fn clip(ffmpeg: &str, path: &Path, codec: &[&str]) {
        let made = Command::new(ffmpeg)
            .args(["-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i", "testsrc2=s=176x144:r=25:d=1"])
            .args(codec)
            .arg(path)
            .output()
            .unwrap();
        assert!(made.status.success(), "{}: {}", path.display(), String::from_utf8_lossy(&made.stderr));
    }

    fn room(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("av1opt-formats-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn every_container_the_scan_looks_for_is_still_read() {
        let Some(tools) = real_tools("every_container_the_scan_looks_for_is_still_read") else { return };
        let dir = room("accepted");
        let mpeg4: &[&str] = &["-c:v", "mpeg4", "-pix_fmt", "yuv420p"];
        let made: &[(&str, &[&str])] = &[
            ("mp4", mpeg4), ("mkv", mpeg4), ("m4v", mpeg4), ("mov", mpeg4), ("avi", mpeg4), ("ts", mpeg4), ("m2ts", mpeg4), ("mts", mpeg4), ("3gp", mpeg4),
            ("webm", &["-c:v", "libsvtav1", "-preset", "12", "-pix_fmt", "yuv420p"]),
            ("wmv", &["-c:v", "wmv2", "-pix_fmt", "yuv420p"]),
            ("flv", &["-c:v", "flv1", "-pix_fmt", "yuv420p"]),
            ("mpg", &["-c:v", "mpeg1video", "-pix_fmt", "yuv420p"]),
            ("mpeg", &["-c:v", "mpeg2video", "-pix_fmt", "yuv420p"]),
            ("vob", &["-c:v", "mpeg2video", "-pix_fmt", "yuv420p"]),
        ];
        // Every one the scan knows is made here, so a new one cannot be forgotten.
        let mut named: Vec<&str> = made.iter().map(|m| m.0).collect();
        named.sort();
        let mut scanned = crate::scan::VIDEO_EXTS.to_vec();
        scanned.sort();
        assert_eq!(named, scanned);
        for (ext, codec) in made {
            let path = dir.join(format!("clip.{ext}"));
            clip(&tools.ffmpeg, &path, codec);
            let size = std::fs::metadata(&path).unwrap().len();
            let info = crate::probe::probe(&tools.ffprobe, &path, size).unwrap_or_else(|e| panic!("{ext}: {e:#}"));
            assert_eq!((info.width, info.height), (176, 144), "{ext}");
            assert!(tools.count_frames(&path).unwrap_or_else(|e| panic!("{ext}: {e:#}")) >= 9, "{ext}");
            tools.run(&["-v".into(), "error".into(), "-i".into(), path_arg(&path), "-frames:v".into(), "1".into(), "-f".into(), "null".into(), "-".into()]).unwrap_or_else(|e| panic!("{ext}: {e:#}"));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_that_leads_to_other_files_is_refused_by_probe_and_by_ffmpeg() {
        let Some(tools) = real_tools("a_file_that_leads_to_other_files_is_refused_by_probe_and_by_ffmpeg") else { return };
        let dir = room("refused");
        let secret = dir.join("secret.mp4");
        clip(&tools.ffmpeg, &secret, &["-c:v", "mpeg4", "-pix_fmt", "yuv420p"]);
        let manifest = format!(
            "<?xml version=\"1.0\"?><MPD xmlns=\"urn:mpeg:dash:schema:mpd:2011\" type=\"static\" mediaPresentationDuration=\"PT1S\" minBufferTime=\"PT1S\" profiles=\"urn:mpeg:dash:profile:isoff-on-demand:2011\"><Period><AdaptationSet mimeType=\"video/mp4\"><Representation id=\"1\" bandwidth=\"1000\" width=\"176\" height=\"144\"><BaseURL>{}</BaseURL></Representation></AdaptationSet></Period></MPD>",
            secret.display()
        );
        let playlist = "ffconcat version 1.0\nfile secret.mp4\nduration 1\n";
        // Each was shown to work on the ffmpeg itself: the limits are what stops them.
        for (name, text) in [("dash.mkv", manifest.as_str()), ("playlist.mkv", playlist)] {
            let path = dir.join(name);
            std::fs::write(&path, text).unwrap();
            let free = Command::new(&tools.ffprobe).args(["-v", "error", "-show_format"]).arg(&path).output().unwrap();
            assert!(free.status.success(), "{name} no longer opens without the limits: the test proves nothing");
            let e = format!("{:#}", crate::probe::probe(&tools.ffprobe, &path, text.len() as u64).unwrap_err());
            assert!(e.contains("not a video file in a format av1opt reads"), "{name}: {e}");
            let e = format!("{:#}", tools.run(&["-v".into(), "error".into(), "-i".into(), path_arg(&path), "-f".into(), "null".into(), "-".into()]).unwrap_err());
            assert!(e.contains("not on whitelist"), "{name}: {e}");
            assert_eq!(tools.count_frames(&path).ok(), None, "{name}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_list_of_own_clips_and_generated_pictures_still_open() {
        let Some(tools) = real_tools("a_list_of_own_clips_and_generated_pictures_still_open") else { return };
        let dir = room("own");
        let (a, list) = (dir.join("a.mkv"), dir.join("list.txt"));
        tools.run(&strings(&["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=s=64x64:r=10:d=1", "-c:v", "ffv1"]).into_iter().chain([path_arg(&a)]).collect::<Vec<_>>()).unwrap();
        std::fs::write(&list, format!("file '{0}'\nfile '{0}'\n", a.display())).unwrap();
        tools.run(&strings(&["-v", "error", "-f", "concat", "-safe", "0", "-i"]).into_iter().chain([path_arg(&list)]).chain(strings(&["-f", "null", "-"])).collect::<Vec<_>>()).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
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
