//! The command line as a person meets it: the real binary, its messages and exit codes, with
//! stand-ins for ffmpeg and ffprobe, so that no video is ever encoded. Every test has its own
//! folders below the target folder.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

/// What the stand-in ffprobe says of any file: a minute of plain H.264.
const A_VIDEO: &str = r#"{"format":{"duration":"60","size":"2","bit_rate":"8000000"},"streams":[{"index":0,"codec_type":"video","codec_name":"h264","width":640,"height":360,"avg_frame_rate":"24/1","r_frame_rate":"24/1","pix_fmt":"yuv420p"}]}"#;

struct Case {
    dir: PathBuf,
}

struct Run {
    code: Option<i32>,
    said: String,
}

impl Run {
    fn of(o: Output) -> Run {
        Run { code: o.status.code(), said: format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)) }
    }

    #[track_caller]
    fn ended(&self, code: i32) -> &Run {
        assert_eq!(self.code, Some(code), "{}", self.said);
        self
    }

    #[track_caller]
    fn has(&self, text: &str) -> &Run {
        assert!(self.said.contains(text), "expected {text:?} in:\n{}", self.said);
        self
    }

    #[track_caller]
    fn lacks(&self, text: &str) -> &Run {
        assert!(!self.said.contains(text), "did not expect {text:?} in:\n{}", self.said);
        self
    }
}

impl Case {
    fn new(name: &str) -> Case {
        Case::with_ffmpeg(name, "")
    }

    /// `on_a_run` is the shell text the stand-in ffmpeg goes through when it is asked to work.
    fn with_ffmpeg(name: &str, on_a_run: &str) -> Case {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("cli-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        for d in ["data", "cache", "config", "videos", "bin"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        let case = Case { dir };
        case.tool("ffmpeg", &format!("case \"$2\" in\n-version) echo 'ffmpeg version 8.0'; exit 0;;\n-encoders) echo ' V....D libsvtav1 SVT-AV1'; exit 0;;\n-filters) echo ' ... libvmaf VV->V VMAF'; exit 0;;\nesac\n{on_a_run}\n"));
        case.tool("ffprobe", &format!("if [ \"$2\" = -version ]; then echo 'ffprobe version 8.0'; exit 0; fi\necho '{A_VIDEO}'\n"));
        case
    }

    /// Writes a tool script, and returns once it can be run: while it was open for writing, a
    /// program started by another test at that moment may have got a copy of it, and until
    /// that program is under way the system answers `Text file busy`.
    fn tool(&self, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = self.dir.join("bin").join(name);
        std::fs::write(&path, format!("#!/bin/sh\n[ \"$1\" = --ready ] && exit 0\n{body}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        loop {
            match Command::new(&path).arg("--ready").status() {
                Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => std::thread::sleep(Duration::from_millis(5)),
                done => {
                    done.unwrap();
                    return path;
                }
            }
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// A file under `videos`, with bytes of its own.
    fn video(&self, name: &str) -> PathBuf {
        let path = self.dir.join("videos").join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("the video called {name}")).unwrap();
        path
    }

    fn command<S: AsRef<std::ffi::OsStr>>(&self, args: &[S]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_av1opt"));
        c.current_dir(&self.dir)
            .args(args)
            .env("XDG_DATA_HOME", self.path("data"))
            .env("XDG_CACHE_HOME", self.path("cache"))
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("AV1OPT_HISTORY", self.path("data/history.jsonl"))
            .env("AV1OPT_FFMPEG", self.path("bin/ffmpeg"))
            .env("AV1OPT_FFPROBE", self.path("bin/ffprobe"))
            .stdin(Stdio::null());
        c
    }

    fn run<S: AsRef<std::ffi::OsStr>>(&self, args: &[S]) -> Run {
        Run::of(self.command(args).output().unwrap())
    }
}

/// A run under way, which does not outlive its test.
struct Started(Child);

impl Drop for Started {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Case {
    /// An `encode` of one video under way, its messages going to the file `said`, with ffmpeg
    /// (which writes its process number to `pid` and then waits) started.
    fn encoding(&self) -> (Started, u32) {
        use std::os::unix::process::CommandExt;
        self.video("a.mkv");
        let said = std::fs::File::create(self.path("said")).unwrap();
        let mut command = self.command(&["encode", "videos/a.mkv", "-o", "out", "--memory-limit", "none"]);
        command.stdout(Stdio::null()).stderr(said);
        // SAFETY: only signal(2) between fork and exec. The test may itself be running with
        // Ctrl-C ignored, as a background job is, and the program would inherit that.
        unsafe {
            command.pre_exec(|| {
                libc::signal(libc::SIGINT, libc::SIG_DFL);
                Ok(())
            });
        }
        let run = Started(command.spawn().unwrap());
        let pid = self.path("pid");
        soon("ffmpeg to start", || std::fs::read_to_string(&pid).is_ok_and(|p| p.ends_with('\n')));
        (run, text(&pid).trim().parse().unwrap())
    }

    fn left_in_out(&self, start: &str) -> Vec<String> {
        let names = std::fs::read_dir(self.path("out")).unwrap().flatten();
        names.map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with(start)).collect()
    }
}

fn send(run: &Started, signal: i32) {
    // SAFETY: kill(2) on a child that has not been waited for yet.
    assert_eq!(unsafe { libc::kill(run.0.id() as i32, signal) }, 0);
}

fn soon(what: &str, done: impl Fn() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < Duration::from_secs(30), "still waiting for: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn text(path: &Path) -> String {
    String::from_utf8_lossy(&std::fs::read(path).unwrap()).into_owned()
}

#[test]
fn a_report_named_over_a_video_leaves_the_video_as_it_was() {
    let c = Case::new("report-over-video");
    let (a, b) = (c.video("a.mkv"), c.video("b.mp4"));
    let before = (text(&a), text(&b));
    let videos = c.path("videos");
    let v = videos.to_str().unwrap();
    let (a_name, b_name) = (a.to_str().unwrap(), b.to_str().unwrap());
    let quick: &[&str] = &["--presets", "8", "--params", "tune=0"];
    let runs: Vec<Vec<&str>> = vec![
        vec!["scan", v, "--hours", "1", "--list", a_name],
        vec!["scan", v, "--csv", b_name],
        vec!["dups", v, "--csv", a_name],
        vec!["dups", v, "--json", b_name],
        [&["bench", v, "--csv", a_name], quick].concat(),
        [&["bench", v, "--json", b_name], quick].concat(),
    ];
    for args in runs {
        let r = c.run(&args);
        r.ended(1).has("is a video").has("nothing was written");
        // Said before the work: nothing was looked at yet.
        r.lacks("found 2 video files");
        assert_eq!((text(&a), text(&b)), before, "{args:?}");
    }
    assert_eq!(std::fs::read_dir(&videos).unwrap().count(), 2, "nothing is left beside the videos");
}

#[test]
fn a_report_takes_the_place_of_its_own_earlier_self_only() {
    let c = Case::new("report-again");
    c.video("a.mkv");
    let (list, csv) = (c.path("picked.txt"), c.path("all.csv"));
    let args = ["scan", "videos", "--hours", "1", "--list", "picked.txt", "--csv", "all.csv"];
    c.run(&args).ended(0).has("wrote picked.txt").has("wrote all.csv");
    assert!(text(&csv).starts_with("path,verdict,"));
    c.run(&args).ended(0);

    // A file of notes under that name is the person's.
    std::fs::write(&csv, "my notes\n").unwrap();
    c.run(&args).ended(1).has("does not look like an earlier CSV report").has("nothing was written");
    assert_eq!(text(&csv), "my notes\n");
    std::fs::write(&list, "my notes\n").unwrap();
    std::fs::remove_file(&csv).unwrap();
    c.run(&args).ended(1).has("does not look like an earlier list of paths");
    assert_eq!(text(&list), "my notes\n");
    assert!(!csv.exists(), "refused as a whole, before anything is written");
}

#[test]
fn tools_named_by_a_relative_path_are_found_from_every_folder() {
    let c = Case::with_ffmpeg("relative-tools", "echo \"$0\" >> \"$(dirname \"$0\")/../started\"\nexit 1");
    c.video("a.mkv");
    let started = c.path("started");
    // On the command line, and from the environment (where the Case's own tools are replaced).
    c.run(&["encode", "videos/a.mkv", "-o", "out", "--ffmpeg", "./bin/ffmpeg", "--ffprobe", "./bin/ffprobe"]).lacks("cannot run");
    assert_eq!(text(&started).lines().next(), c.path("bin/ffmpeg").to_str());
    std::fs::remove_file(&started).unwrap();
    let from_env = c.command(&["encode", "videos/a.mkv", "-o", "out2"]).env("AV1OPT_FFMPEG", "bin/ffmpeg").env("AV1OPT_FFPROBE", "bin/ffprobe").output().unwrap();
    assert_eq!(text(&started).lines().next(), c.path("bin/ffmpeg").to_str(), "{}", Run::of(from_env).said);
}

#[test]
fn a_scenes_value_written_with_a_space_is_not_taken_for_a_video() {
    let c = Case::new("scenes-space");
    c.video("a.mkv");
    for word in ["auto", "off", "always"] {
        c.run(&["encode", "--scenes", word, "videos/a.mkv", "-o", "out"]).ended(2).has(&format!("--scenes={word}"));
        assert!(!c.path("out").exists(), "nothing was begun");
    }
    // A video that really has that name is one.
    std::fs::write(c.path("auto"), "a video without an extension").unwrap();
    c.run(&["encode", "auto", "-o", "out"]).lacks("--scenes=auto").has("[1/1] auto");
}

#[test]
fn a_second_run_leaves_a_file_being_encoded_and_goes_on_to_the_next() {
    // Work on a.mkv goes on until the test says otherwise; work on any other file fails at once.
    let c = Case::with_ffmpeg(
        "lock",
        "here=\"$(dirname \"$0\")/..\"\ncase \"$*\" in *a.mkv*) if [ ! -e \"$here/free\" ]; then touch \"$here/began\"; exec sleep 60; fi;; esac\nexit 1",
    );
    c.video("a.mkv");
    c.video("b.mkv");
    let first = c.command(&["encode", "videos/a.mkv", "-o", "out"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    let mut first = Started(first);
    soon("the first run to be encoding", || c.path("began").exists());

    let second = c.run(&["encode", "videos", "-o", "out"]);
    second.has("skipped: another av1opt is encoding this now").has("[2/2]");
    // It is not noted as a file to skip from now on.
    let log = text(&c.path("out").join("av1opt-log.tsv"));
    assert!(!log.contains("a.mkv"), "{log}");

    // A run that was killed holds nothing, though its lock file is still there.
    first.0.kill().unwrap();
    first.0.wait().unwrap();
    std::fs::write(c.path("free"), "").unwrap();
    c.run(&["encode", "videos/a.mkv", "-o", "out"]).lacks("another av1opt");
    let left: Vec<_> = std::fs::read_dir(c.path("out")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    assert!(!left.iter().any(|n| n.starts_with(".av1opt-lock")), "{left:?}");
}

#[test]
fn a_run_killed_under_the_memory_cap_says_so_and_had_all_of_it() {
    let c = Case::with_ffmpeg("memory-cap", "kill -9 $$");
    c.video("a.mkv");
    // Stands in for systemd: notes the scope asked for and runs what is to go in it.
    c.tool(
        "systemd-run",
        "echo \"$*\" >> \"$(dirname \"$0\")/../scopes\"\nwhile [ $# -gt 0 ]; do case \"$1\" in --user|--scope|--collect|-q) shift;; -p) shift 2;; *) break;; esac; done\nexec \"$@\"\n",
    );
    let path = format!("{}:{}", c.path("bin").display(), std::env::var("PATH").unwrap());
    let args = ["encode", "videos/a.mkv", "-o", "out", "--memory-limit", "2G", "--scenes", "--workers", "2"];
    let run = Run::of(c.command(&args).env("PATH", path).output().unwrap());
    run.has("memory cap of 2G").has("--memory-limit");
    // What runs before any chunk runs alone, so it is not held to a worker's part.
    let scopes = text(&c.path("scopes"));
    assert!(scopes.contains("MemoryMax=2G") && !scopes.contains("MemoryMax=1024M"), "{scopes}");
}

/// For the runs of the encode, which start this way (asking the encoder its version does not).
const WRITES_ITS_PID_AND_WAITS: &str = "[ \"$2\" = -nostdin ] || exit 0\necho $$ > \"$(dirname \"$0\")/../pid\"\nexec sleep 60";

#[test]
fn a_signal_stops_ffmpeg_and_clears_the_temporary_folder_but_not_the_work() {
    for (signal, code) in [(libc::SIGTERM, 143), (libc::SIGINT, 130)] {
        let c = Case::with_ffmpeg(&format!("signal-{signal}"), WRITES_ITS_PID_AND_WAITS);
        std::fs::create_dir_all(c.path("out/.av1opt-work-0123")).unwrap();
        std::fs::write(c.path("out/.av1opt-work-0123/chunk"), "hours of work").unwrap();
        let (mut run, ffmpeg) = c.encoding();
        assert_eq!(c.left_in_out(".av1opt-tmp-").len(), 1, "the run has its temporary folder");

        send(&run, signal);
        assert_eq!(run.0.wait().unwrap().code(), Some(code));
        assert!(!Path::new(&format!("/proc/{ffmpeg}")).exists(), "ffmpeg was ended and waited for");
        assert_eq!(c.left_in_out(".av1opt-tmp-"), [""; 0], "the temporary folder went");
        assert_eq!(c.left_in_out(".av1opt-lock-"), [""; 0]);
        assert_eq!(text(&c.path("out/.av1opt-work-0123/chunk")), "hours of work");
        let said = text(&c.path("said"));
        assert!(said.contains("stopping") && said.contains("kept for the next run") && said.contains(".av1opt-work-0123"), "{said}");
        assert!(!said.contains("FAILED"), "{said}");
        // Nothing about the file was noted: the next run takes it up again.
        assert!(!std::fs::read_to_string(c.path("out/av1opt-log.tsv")).unwrap_or_default().contains("a.mkv"));
    }
}

#[test]
fn a_second_signal_ends_the_run_at_once() {
    // This ffmpeg takes no notice of being asked to stop.
    let c = Case::with_ffmpeg("signal-twice", &format!("trap '' TERM\n{WRITES_ITS_PID_AND_WAITS}"));
    let (mut run, ffmpeg) = c.encoding();
    send(&run, libc::SIGTERM);
    soon("the first signal to be taken up", || text(&c.path("said")).contains("stopping"));
    assert!(run.0.try_wait().unwrap().is_none(), "still waiting for ffmpeg");
    send(&run, libc::SIGINT);
    assert_eq!(run.0.wait().unwrap().code(), Some(130));
    soon("ffmpeg to go with it", || !Path::new(&format!("/proc/{ffmpeg}")).exists());
}

#[test]
fn the_exit_status_tells_failed_files_from_errors_and_wrong_usage() {
    let c = Case::with_ffmpeg("exit-codes", "[ \"$2\" = -nostdin ] && exit 1\nexit 0");
    // Nothing to do, or nowhere to put it, stops the run.
    c.run(&["encode", "videos", "-o", "out"]).ended(1).has("no video files");
    assert!(!c.path("out").exists(), "a refused run leaves no folder");
    c.run(&["encode", "videos", "-o", "out", "--tuned"]).ended(1).has("--tuned");
    assert!(!c.path("out").exists(), "a refused run leaves no folder");

    c.video("a.mkv");
    c.video("b.mkv");
    c.run(&["encode", "videos", "-o", "out", "--workers", "9"]).ended(2);
    assert!(!c.path("out").exists());
    c.run(&["encode", "videos", "-o", "out"]).ended(3).has("FAILED").has("[2/2]").has("failed 2");
    assert!(c.path("out").is_dir());

    std::fs::write(c.path("afile"), "someone's").unwrap();
    c.run(&["encode", "videos", "-o", "afile"]).ended(1).has("afile");
    assert_eq!(text(&c.path("afile")), "someone's");
}

#[test]
fn a_file_at_the_outputs_name_without_a_good_report_is_not_counted_as_done() {
    let c = Case::with_ffmpeg("unverified", "exit 1");
    c.video("a.mkv");
    std::fs::create_dir(c.path("out")).unwrap();
    std::fs::write(c.path("out/a.mp4"), "").unwrap();
    c.run(&["encode", "videos", "-o", "out"])
        .ended(3)
        .has("out/a.mp4 exists, not made by this run: not verified, left alone (it has no report)")
        .has("already done 0, not verified 1")
        .lacks("already done:");
    std::fs::write(c.path("out/a.av1opt.json"), "{ cut off").unwrap();
    c.run(&["encode", "videos", "-o", "out"]).ended(3).has("its report cannot be read").has("not verified 1");
    assert_eq!(text(&c.path("out/a.mp4")), "", "it is left as it was");
}

#[test]
fn an_old_skip_does_not_stand_against_force_or_another_target() {
    // ffmpeg fails whatever it is asked to do, which shows that the file was gone on with.
    let c = Case::with_ffmpeg("old-skip", "echo 'stand-in ffmpeg: no' >&2; exit 1");
    let video = c.video("a.mkv");
    let size = std::fs::metadata(&video).unwrap().len();
    std::fs::create_dir(c.path("out")).unwrap();
    let line = format!("99999999999\tskip\t{}\t{size}\ttoo lean\n", std::fs::canonicalize(&video).unwrap().display());
    std::fs::write(c.path("out/av1opt-log.tsv"), line).unwrap();
    let encode = |extra: &[&str]| c.run(&[&["encode", "videos", "-o", "out", "--memory-limit", "none"], extra].concat());
    encode(&[]).ended(0).has("skipped earlier: too lean");
    encode(&["--force"]).ended(3).lacks("skipped earlier");
    encode(&["--target-vmaf", "50"]).ended(3).lacks("skipped earlier");
}

#[test]
fn a_file_is_not_remembered_as_unreadable_because_ffprobe_failed_in_silence() {
    let c = Case::new("silent-ffprobe");
    c.video("a.mkv");
    let working = text(&c.path("bin/ffprobe"));
    c.tool("ffprobe", "if [ \"$2\" = -version ]; then exit 0; fi\nexit 1\n");
    c.run(&["scan", "videos"]).ended(0).has("unreadable");
    assert!(!text(&c.path("cache/av1opt/probe-cache-v4.json")).contains("a.mkv"));
    // With ffprobe working again the same file is read, not answered from the cache.
    c.tool("ffprobe", working.trim_start_matches("#!/bin/sh\n[ \"$1\" = --ready ] && exit 0\n"));
    c.run(&["scan", "videos"]).ended(0).has("h264").lacks("unreadable");
}

#[test]
fn the_duplicate_finder_does_not_take_a_broken_ffmpeg_for_broken_videos() {
    let c = Case::with_ffmpeg("dups-ffmpeg", "echo 'stand-in ffmpeg: cannot go on' >&2; exit 1");
    c.video("a.mkv");
    c.video("b.mkv");
    let cache = c.path("cache/av1opt/dups-cache-v2.json");

    // A program that is not there stops the run before any file is looked at.
    c.run(&["dups", "videos", "--ffmpeg", "/nonexistent/ffmpeg"]).ended(1).has("cannot run ffmpeg (/nonexistent/ffmpeg)");
    assert!(!cache.exists());

    // One that starts and then fails on every video: said at the end, and nothing is remembered.
    c.run(&["dups", "videos"]).ended(3).has("warning: 2 of 2 videos could not be read").has("stand-in ffmpeg: cannot go on");
    assert_eq!(text(&cache), "{}");

    // One that runs well and finds no frame says that of the files, and that is kept.
    c.tool("ffmpeg", "case \"$2\" in\n-version) echo 'ffmpeg version 8.0';;\nesac\nexit 0\n");
    c.run(&["dups", "videos"]).ended(3).has("no frame could be read");
    assert!(text(&cache).contains("no frame could be read"));
}

#[test]
fn a_path_that_cannot_be_read_is_an_error_and_a_folder_further_down_a_warning() {
    use std::os::unix::fs::PermissionsExt;
    let c = Case::new("unread-paths");
    c.video("a.mkv");
    c.video("closed/b.mkv");
    let closed = c.path("videos/closed");
    let set_mode = |mode| std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(mode)).unwrap();

    let commands: [&[&str]; 4] = [&["scan"], &["dups"], &["bench"], &["encode", "-o", "out"]];
    for command in commands {
        let run = |path: &str| c.run(&[command, &[path]].concat());
        run("no/such/folder").ended(1).has("cannot read").has("no/such/folder").has("No such file");
        assert!(!c.path("out").exists());
    }

    set_mode(0o000);
    for command in commands {
        c.run(&[command, &["videos/closed"]].concat()).ended(1).has("cannot read").has("videos/closed").has("Permission denied");
    }
    let warning = "1 folder could not be read, so the videos in it are left out";
    c.run(&["scan", "videos"]).ended(0).has(warning);
    c.run(&["dups", "videos"]).has(warning);
    c.run(&["bench", "videos"]).has(warning);
    c.run(&["encode", "videos", "-o", "out"]).has(warning).has("folders not read 1");
    set_mode(0o755);
}
