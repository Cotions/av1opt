//! The command line as a person meets it: the real binary, its messages and exit codes, with
//! stand-ins for ffmpeg and ffprobe, so that no video is ever encoded. Every test has its own
//! folders below the target folder.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
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
