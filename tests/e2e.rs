//! End-to-end tests: the real binary and a real ffmpeg, on clips generated on the spot.
//!
//! They are left out of a plain `cargo test` because they need an ffmpeg with libsvtav1,
//! libvmaf, libx264 and libx265 (`AV1OPT_FFMPEG` / `AV1OPT_FFPROBE`, else the ones in the
//! path). Run them with:
//!
//!     cargo test --test e2e -- --ignored
//!
//! Every test has its own folders for videos, outputs, history and caches below the target
//! folder, so nothing of the user's is read or written.

#[cfg(feature = "ui")]
mod common;

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn ffmpeg_bin() -> String {
    std::env::var("AV1OPT_FFMPEG").unwrap_or_else(|_| "ffmpeg".into())
}

fn ffprobe_bin() -> String {
    std::env::var("AV1OPT_FFPROBE").unwrap_or_else(|_| "ffprobe".into())
}

/// Quick settings: the tests are about what comes out, not how well it is compressed.
const FAST: [&str; 6] = ["--preset", "10", "--samples", "2", "--sample-secs", "2"];

struct Case {
    dir: PathBuf,
}

struct Run {
    code: Option<i32>,
    err: String,
}

impl Run {
    fn ok(&self) -> &Run {
        assert_eq!(self.code, Some(0), "av1opt failed:\n{}", self.err);
        self
    }

    fn has(&self, text: &str) -> &Run {
        assert!(self.err.contains(text), "expected {text:?} in:\n{}", self.err);
        self
    }
}

impl Case {
    fn new(name: &str) -> Case {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("e2e-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        for d in ["data/av1opt", "cache", "videos", "out"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        Case { dir }
    }

    fn videos(&self) -> PathBuf {
        self.dir.join("videos")
    }

    fn out(&self) -> PathBuf {
        self.dir.join("out")
    }

    /// Runs ffmpeg in the videos folder (relative names land there).
    fn ffmpeg(&self, args: &[&str]) {
        let o = Command::new(ffmpeg_bin())
            .current_dir(self.videos())
            .args(["-v", "error", "-nostdin", "-y"])
            .args(args)
            .output()
            .expect("run ffmpeg");
        assert!(o.status.success(), "ffmpeg {args:?} failed:\n{}", String::from_utf8_lossy(&o.stderr));
    }

    /// An H.264 + AAC clip of moving test patterns: `secs` long, 24 fps.
    fn clip(&self, name: &str, size: &str, secs: u32) -> PathBuf {
        let (v, a) = (format!("testsrc2=s={size}:r=24:d={secs}"), format!("sine=f=440:d={secs}"));
        self.ffmpeg(&[
            "-f", "lavfi", "-i", &v, "-f", "lavfi", "-i", &a, "-c:v", "libx264", "-preset", "ultrafast", "-crf", "16", "-pix_fmt", "yuv420p",
            "-c:a", "aac", "-b:a", "64k", "-shortest", name,
        ]);
        self.videos().join(name)
    }

    /// 24 s of four very different scenes, 640x360: enough work to stop an encode midway.
    fn scenes_clip(&self, name: &str) -> PathBuf {
        scenes_clip(&self.videos(), name);
        self.videos().join(name)
    }

    fn command(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_av1opt"));
        c.current_dir(&self.dir)
            .env("XDG_DATA_HOME", self.dir.join("data"))
            .env("XDG_CACHE_HOME", self.dir.join("cache"))
            .env("AV1OPT_HISTORY", self.dir.join("data/av1opt/history.jsonl"))
            .env("AV1OPT_FFMPEG", ffmpeg_bin())
            .env("AV1OPT_FFPROBE", ffprobe_bin())
            .stdin(Stdio::null());
        c
    }

    fn av1opt<S: AsRef<std::ffi::OsStr>>(&self, args: &[S]) -> Run {
        let o = self.command().args(args).output().expect("run av1opt");
        Run { code: o.status.code(), err: format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)) }
    }

    /// `av1opt encode` with the quick settings, into `out`.
    fn encode(&self, inputs: &[&Path], extra: &[&str]) -> Run {
        self.encode_into(&self.out(), inputs, extra)
    }

    fn encode_into(&self, out: &Path, inputs: &[&Path], extra: &[&str]) -> Run {
        let mut args: Vec<std::ffi::OsString> = vec!["encode".into(), "-o".into(), out.into()];
        args.extend(FAST.iter().map(Into::into));
        args.extend(extra.iter().map(Into::into));
        args.extend(inputs.iter().map(Into::into));
        self.av1opt(&args)
    }

    fn report(&self, stem: &str) -> Value {
        self.report_in(&self.out(), stem)
    }

    fn report_in(&self, out: &Path, stem: &str) -> Value {
        let p = out.join(format!("{stem}.av1opt.json"));
        serde_json::from_slice(&std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))).unwrap()
    }

    /// Names in the output folder, sorted.
    fn outputs(&self) -> Vec<String> {
        self.outputs_in(&self.out())
    }

    fn outputs_in(&self, dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }
}

fn scenes_clip(dir: &Path, name: &str) {
    let o = Command::new(ffmpeg_bin())
        .current_dir(dir)
        .args(["-v", "error", "-nostdin", "-y"])
        // Two samples of 2 s lie at 5.5 s and 16.5 s of these 24: each wholly inside one scene,
        // the second and the third, which score far apart.
        .args(["-f", "lavfi", "-i", "testsrc2=s=640x360:r=24:d=5", "-f", "lavfi", "-i", "mandelbrot=s=640x360:r=24,trim=duration=7"])
        // Seeded: these two draw at random otherwise, and a test must see the same clip twice.
        .args(["-f", "lavfi", "-i", "sierpinski=s=640x360:r=24:seed=1,trim=duration=7"])
        .args(["-f", "lavfi", "-i", "life=s=640x360:r=24:mold=10:ratio=0.1:seed=1:death_color=#C83232:life_color=#00ff00,trim=duration=5"])
        .args(["-f", "lavfi", "-i", "sine=f=440:d=24"])
        .args(["-filter_complex", "[0:v]format=yuv420p[a];[1:v]format=yuv420p[b];[2:v]format=yuv420p[c];[3:v]format=yuv420p[d];[a][b][c][d]concat=n=4:v=1:a=0[v]"])
        .args(["-map", "[v]", "-map", "4:a", "-c:v", "libx264", "-preset", "ultrafast", "-crf", "16", "-c:a", "aac", "-b:a", "64k", "-bitexact", "-fflags", "+bitexact", name])
        .output()
        .expect("run ffmpeg");
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
}

/// Running ffmpeg processes whose command line names something under `dir`, each with its
/// state letter (`R` running, `S` sleeping, `T` stopped).
fn ffmpegs_under(dir: &Path) -> Vec<(u32, char)> {
    let needle = dir.to_string_lossy().into_owned();
    let mut found = Vec::new();
    for e in std::fs::read_dir("/proc").unwrap().flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
        let Ok(comm) = std::fs::read_to_string(e.path().join("comm")) else { continue };
        if comm.trim() != "ffmpeg" {
            continue;
        }
        let cmdline = std::fs::read(e.path().join("cmdline")).unwrap_or_default();
        if !String::from_utf8_lossy(&cmdline).contains(&needle) {
            continue;
        }
        let stat = std::fs::read_to_string(e.path().join("stat")).unwrap_or_default();
        if let Some(state) = stat.rsplit(')').next().and_then(|rest| rest.trim().chars().next()) {
            found.push((pid, state));
        }
    }
    found
}

/// Waits for `done` to hold, looking every 50 ms; fails after `secs`.
fn wait_until(what: &str, secs: u64, mut done: impl FnMut() -> bool) {
    let t = std::time::Instant::now();
    while !done() {
        assert!(t.elapsed().as_secs() < secs, "gave up waiting for {what} after {secs} s");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Entries of `dir` whose name starts with `prefix`.
fn entries(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    rd.flatten().map(|e| e.path()).filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(prefix))).collect()
}

fn ffprobe(args: &[&str], path: &Path) -> String {
    let o = Command::new(ffprobe_bin()).args(["-v", "error"]).args(args).arg(path).output().expect("run ffprobe");
    assert!(o.status.success(), "ffprobe {} failed:\n{}", path.display(), String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// Streams, container and chapters as ffprobe reports them.
fn probe(path: &Path) -> Value {
    serde_json::from_str(&ffprobe(&["-show_streams", "-show_format", "-show_chapters", "-of", "json"], path)).unwrap()
}

fn streams<'a>(p: &'a Value, kind: &str) -> Vec<&'a Value> {
    p["streams"].as_array().unwrap().iter().filter(|s| s["codec_type"] == kind && s["disposition"]["attached_pic"] != 1).collect()
}

/// Frames in the first video stream, counted by decoding.
fn frames(path: &Path) -> u64 {
    let out = ffprobe(&["-select_streams", "v:0", "-count_frames", "-show_entries", "stream=nb_read_frames", "-of", "csv=p=0"], path);
    out.trim().trim_end_matches(',').parse().unwrap_or_else(|_| panic!("frame count of {}: {out:?}", path.display()))
}

fn duration(path: &Path) -> f64 {
    probe(path)["format"]["duration"].as_str().and_then(|d| d.parse().ok()).unwrap_or_else(|| panic!("no duration: {}", path.display()))
}

/// A fingerprint of a stream's packets: equal for a stream that was copied, not re-encoded.
fn packets_md5(path: &Path, stream: &str) -> String {
    let o = Command::new(ffmpeg_bin())
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(path)
        .args(["-map", stream, "-c", "copy", "-f", "md5", "-"])
        .output()
        .expect("run ffmpeg");
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

/// Mean PSNR between frames of `out` and frames of `src`, paired in order. `pick_out` and
/// `pick_src` are ffmpeg `select` expressions on the frame number `n` ("1" takes all).
/// (Both sides are renumbered in one time base: left in their own, the frame times round
/// differently and the filter pairs a frame with its neighbour.)
fn psnr(out: &Path, pick_out: &str, src: &Path, pick_src: &str) -> f64 {
    let graph = format!(
        "[0:v:0]select='{pick_out}',settb=1/24,setpts=N,format=yuv420p[d];[1:v:0]select='{pick_src}',settb=1/24,setpts=N,format=yuv420p[r];[d][r]psnr=shortest=1"
    );
    let o = Command::new(ffmpeg_bin())
        .args(["-v", "info", "-nostdin", "-i"])
        .arg(out)
        .arg("-i")
        .arg(src)
        .args(["-lavfi", &graph, "-an", "-sn", "-f", "null", "-"])
        .output()
        .expect("run ffmpeg");
    let err = String::from_utf8_lossy(&o.stderr);
    let found = err.lines().filter(|l| l.contains("PSNR")).find_map(|l| l.split("average:").nth(1)?.split_whitespace().next()?.parse::<f64>().ok());
    found.unwrap_or_else(|| panic!("no PSNR in:\n{err}"))
}

/// The output's frames are the source's `kept` frames, in step: paired one off in either
/// direction they match far worse. `kept` is a `select` expression on the source's `n`.
fn assert_in_step(out: &Path, src: &Path, kept: &str) {
    let right = psnr(out, "1", src, kept);
    // One frame early, one frame late.
    let early = psnr(out, "gte(n,1)", src, kept);
    let late = psnr(out, "1", src, &format!("({kept})*gte(n,1)"));
    assert!(right > 28.0 && right > early + 3.0 && right > late + 3.0, "{}: PSNR in step {right:.1}, one early {early:.1}, one late {late:.1}", out.display());
}

fn tag<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    let tags = v["tags"].as_object()?;
    tags.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).and_then(|(_, v)| v.as_str())
}

fn mtime(path: &Path) -> std::time::SystemTime {
    std::fs::metadata(path).unwrap().modified().unwrap()
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn a_source_changed_during_its_encode_leaves_no_output() {
    let c = Case::new("source-changed");
    let src = c.clip("a.mp4", "320x180", 6);
    let mut child = c.command().arg("encode").arg("-o").arg(c.out()).args(FAST).arg(&src).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut stderr, &mut text).unwrap();
        text
    });
    // The work folder appears once the encode is under way, and goes when it ends.
    let started = std::time::Instant::now();
    let working = || std::fs::read_dir(c.out()).unwrap().flatten().any(|e| e.file_name().to_string_lossy().starts_with(".av1opt-tmp-"));
    while !working() {
        assert!(child.try_wait().unwrap().is_none(), "the encode ended before its work folder was seen");
        assert!(started.elapsed() < std::time::Duration::from_secs(120), "no work folder after two minutes");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    // What a download going on does to a file allocated ahead: same size, other contents, a new
    // date. The last byte of this file is in a tag, so the clip still plays.
    let later = std::fs::metadata(&src).unwrap().modified().unwrap() + std::time::Duration::from_secs(5);
    let file = std::fs::File::options().read(true).write(true).open(&src).unwrap();
    {
        use std::os::unix::fs::FileExt;
        let end = file.metadata().unwrap().len() - 1;
        let mut last = [0u8];
        file.read_exact_at(&mut last, end).unwrap();
        file.write_all_at(&[last[0] ^ 1], end).unwrap();
    }
    file.set_modified(later).unwrap();
    let status = child.wait().unwrap();
    let err = reader.join().unwrap();
    assert!(!status.success(), "the encode was taken as good:\n{err}");
    assert!(err.contains("changed or went away while it was being encoded"), "{err}");
    assert!(!c.out().join("a.mp4").exists() && !c.out().join("a.av1opt.json").exists());
    // Left alone from here, the file encodes, and its report records the date it now has.
    c.encode(&[&src], &[]).ok().has("encoded 1, skipped 0, failed 0, already done 0");
    let seconds = later.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    assert_eq!(c.report("a")["source_mtime"].as_u64(), Some(seconds));
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn a_video_is_encoded_checked_reported_and_not_done_twice() {
    let c = Case::new("whole");
    let src = c.clip("a.mp4", "320x180", 6);
    let before = std::fs::read(&src).unwrap();
    c.encode(&[&src], &[]).ok().has("encoded 1, skipped 0, failed 0, already done 0");

    let out = c.out().join("a.mp4");
    let p = probe(&out);
    let v = streams(&p, "video");
    assert_eq!(v.len(), 1);
    assert_eq!((v[0]["codec_name"].as_str(), v[0]["width"].as_u64(), v[0]["height"].as_u64()), (Some("av1"), Some(320), Some(180)));
    assert_eq!(v[0]["pix_fmt"], "yuv420p10le");
    assert_eq!(frames(&out), frames(&src));
    assert_in_step(&out, &src, "1");
    assert!((duration(&out) - duration(&src)).abs() < 0.1);
    // AAC is copied, not encoded a second time.
    assert_eq!(streams(&p, "audio")[0]["codec_name"], "aac");
    assert_eq!(packets_md5(&out, "0:a:0"), packets_md5(&src, "0:a:0"));

    let r = c.report("a");
    assert_eq!(r["format"], 1);
    assert_eq!(r["mode"], "whole");
    assert_eq!(r["input"].as_str().map(Path::new), Some(std::fs::canonicalize(&src).unwrap().as_path()));
    assert_eq!(r["output_bytes"].as_u64(), Some(std::fs::metadata(&out).unwrap().len()));
    assert_eq!(r["source_bytes"].as_u64(), Some(before.len() as u64));
    let crf = r["crf"].as_f64().unwrap();
    assert!((20.0..=55.0).contains(&crf) && (crf * 4.0).fract() == 0.0, "crf {crf}");
    // The CRF chosen was measured (or lies between two that were), and met the target.
    let trials = r["search"].as_array().unwrap();
    assert!(!trials.is_empty());
    assert!(trials.iter().any(|t| t["mean_vmaf"].as_f64().unwrap() >= 93.0), "{trials:?}");
    assert!(!r["verify_vmaf"].as_array().unwrap().is_empty());

    // Only the output, its report and the log are left; the source is untouched.
    assert_eq!(c.outputs(), ["a.av1opt.json", "a.mp4", "av1opt-log.tsv"]);
    assert_eq!(std::fs::read(&src).unwrap(), before);
    let history = std::fs::read_to_string(c.dir.join("data/av1opt/history.jsonl")).unwrap();
    assert_eq!(history.lines().count(), 1, "{history}");

    // A second run finds it done and leaves it alone.
    let made = mtime(&out);
    c.encode(&[&src], &[]).ok().has("encoded 0, skipped 0, failed 0, already done 1");
    assert_eq!(mtime(&out), made);
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn the_search_takes_many_short_clips_where_the_whole_video_is_checked() {
    let c = Case::new("samples");
    let src = c.clip("a.mp4", "320x180", 20);
    let run = |extra: &[&str]| {
        let mut args: Vec<std::ffi::OsString> = vec!["encode".into(), "-o".into(), c.out().into(), "--preset".into(), "10".into(), "--overwrite".into()];
        args.extend(extra.iter().map(Into::into));
        args.push(src.clone().into());
        c.av1opt(&args)
    };
    // One pass, checked along its whole length: the search wants the mean, from one-second
    // clips, and a video this short is all of them.
    let r = run(&[]);
    r.ok().has("sample 1/20 at 0:00:00 (1s)").has("sample 20/20 at 0:00:19 (1s)").has("verifying: the whole video against its source").has("encoded 1,");
    assert_eq!(c.report("a")["mode"], "whole");
    // A search that has to mind the worst stretch (scenes may be encoded apart) keeps clips
    // long enough to score like one, as does a search told what clips to take.
    run(&["--scenes=auto"]).ok().has("sample 2/2 at").has("(4s)");
    run(&["--samples", "1"]).ok().has("sample 1/1 at").has("(4s)");
    run(&["--sample-secs", "2"]).ok().has("sample 5/5 at").has("(2s)");
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn subtitles_chapters_and_languages_follow_the_container() {
    let c = Case::new("subs");
    c.clip("plain.mp4", "320x180", 6);
    std::fs::write(c.videos().join("subs.srt"), "1\n00:00:01,000 --> 00:00:02,500\nFirst line\n\n2\n00:00:03,000 --> 00:00:05,000\nSecond line\n").unwrap();
    let chapters = ";FFMETADATA1\ntitle=The Title\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=3000\ntitle=One\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=3000\nEND=6000\ntitle=Two\n";
    std::fs::write(c.videos().join("meta.txt"), chapters).unwrap();
    c.ffmpeg(&[
        "-i", "plain.mp4", "-i", "subs.srt", "-i", "meta.txt", "-map", "0", "-map", "1", "-map_metadata", "2", "-map_chapters", "2", "-c", "copy", "-c:s", "srt",
        "-metadata:s:a:0", "language=fre", "-metadata:s:a:0", "title=Commentary", "-metadata:s:s:0", "language=ger", "src.mkv",
    ]);
    let src = c.videos().join("src.mkv");

    let check = |out: &Path, sub_codec: &str| {
        let p = probe(out);
        let subs = streams(&p, "subtitle");
        assert_eq!(subs.len(), 1, "{}", out.display());
        assert_eq!(subs[0]["codec_name"], sub_codec);
        assert_eq!(tag(subs[0], "language"), Some("ger"));
        let audio = streams(&p, "audio");
        assert_eq!(tag(audio[0], "language"), Some("fre"));
        let chapters: Vec<&str> = p["chapters"].as_array().unwrap().iter().filter_map(|ch| tag(ch, "title")).collect();
        assert_eq!(chapters, ["One", "Two"]);
        assert_eq!(tag(&p["format"], "title"), Some("The Title"));
        // The subtitle lines are still there, at their times.
        let text = ffprobe(&["-select_streams", "s:0", "-show_entries", "packet=pts_time", "-of", "csv=p=0"], out);
        let times: Vec<f64> = text.lines().filter_map(|l| l.trim().trim_end_matches(',').parse().ok()).collect();
        assert!(times.iter().any(|t| (t - 1.0).abs() < 0.05) && times.iter().any(|t| (t - 3.0).abs() < 0.05), "{times:?}");
    };
    c.encode(&[&src], &["--container", "mkv"]).ok();
    check(&c.out().join("src.mkv"), "subrip");
    assert_eq!(tag(streams(&probe(&c.out().join("src.mkv")), "audio")[0], "title"), Some("Commentary"));

    let mp4 = c.dir.join("out-mp4");
    c.encode_into(&mp4, &[&src], &[]).ok();
    check(&mp4.join("src.mp4"), "mov_text");
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn ten_bit_and_hdr_signalling_survive() {
    let c = Case::new("hdr");
    let x265 = |name: &str, extra: &[&str]| {
        let mut args = vec!["-f", "lavfi", "-i", "testsrc2=s=320x180:r=24:d=6", "-c:v", "libx265", "-preset", "ultrafast", "-crf", "14", "-pix_fmt", "yuv420p10le", "-tag:v", "hvc1"];
        args.extend(extra);
        args.push(name);
        c.ffmpeg(&args);
        c.videos().join(name)
    };
    let sdr = x265("sdr10.mp4", &["-x265-params", "log-level=error"]);
    // (`setparams`, because ffmpeg takes the colours from the frames, not from output options.)
    let pq = x265("pq.mp4", &[
        "-vf", "setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc",
        "-x265-params", "log-level=error:hdr10=1:repeat-headers=1:master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,50):max-cll=1000,400",
    ]);
    let hlg = x265("hlg.mp4", &["-vf", "setparams=color_primaries=bt2020:color_trc=arib-std-b67:colorspace=bt2020nc", "-x265-params", "log-level=error"]);
    c.encode(&[&sdr, &pq, &hlg], &[]).ok().has("encoded 3,");

    let video = |name: &str| streams(&probe(&c.out().join(name)), "video")[0].clone();
    for name in ["sdr10.mp4", "pq.mp4", "hlg.mp4"] {
        let v = video(name);
        assert_eq!((v["codec_name"].as_str(), v["pix_fmt"].as_str()), (Some("av1"), Some("yuv420p10le")), "{name}");
        assert_eq!(frames(&c.out().join(name)), 144, "{name}");
    }
    let v = video("pq.mp4");
    assert_eq!((v["color_transfer"].as_str(), v["color_primaries"].as_str(), v["color_space"].as_str()), (Some("smpte2084"), Some("bt2020"), Some("bt2020nc")));
    let side = serde_json::to_string(&v["side_data_list"]).unwrap();
    assert!(side.contains("Mastering display metadata") && side.contains("Content light level metadata"), "{side}");
    assert!(side.contains("\"max_content\":1000") && side.contains("\"max_average\":400"), "{side}");
    assert!(side.contains("\"max_luminance\":\"10000000/10000\""), "{side}");
    let v = video("hlg.mp4");
    assert_eq!((v["color_transfer"].as_str(), v["color_primaries"].as_str()), (Some("arib-std-b67"), Some("bt2020")));
    // Nothing of the kind is made up for a plain 10-bit source.
    let v = video("sdr10.mp4");
    assert!(v["side_data_list"].is_null(), "{v}");
    assert_ne!(v["color_transfer"], "smpte2084");
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1, libvmaf and libx265"]
fn an_hdr_video_keeps_its_mastering_data_when_scenes_are_asked_for() {
    let c = Case::new("hdr-scenes");
    c.ffmpeg(&[
        "-f", "lavfi", "-i", "testsrc2=s=320x240:r=24:d=6", "-vf", "setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc",
        "-c:v", "libx265", "-preset", "ultrafast", "-crf", "14", "-pix_fmt", "yuv420p10le", "-tag:v", "hvc1",
        "-x265-params", "log-level=error:hdr10=1:repeat-headers=1:master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,1):max-cll=1000,400",
        "pq.mp4",
    ]);
    let src = c.videos().join("pq.mp4");
    for container in ["mp4", "mkv"] {
        let out = c.dir.join(format!("out-{container}"));
        c.encode_into(&out, &[&src], &["--scenes=always", "--min-savings", "0", "--container", container]).ok().has("encoded in one pass instead of per scene");
        let file = out.join(format!("pq.{container}"));
        let side = serde_json::to_string(&streams(&probe(&file), "video")[0]["side_data_list"]).unwrap();
        assert!(side.contains("Mastering display metadata") && side.contains("\"max_content\":1000") && side.contains("\"max_average\":400"), "{container}: {side}");
        assert_eq!(c.report_in(&out, "pq")["mode"], "whole");
    }
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn sound_tracks_are_copied_converted_or_shrunk() {
    let c = Case::new("audio");
    c.ffmpeg(&[
        "-f", "lavfi", "-i", "testsrc2=s=320x180:r=24:d=6", "-f", "lavfi", "-i", "anoisesrc=d=6:c=pink:r=48000:a=0.3,aformat=channel_layouts=stereo",
        "-map", "0:v", "-map", "1:a", "-map", "1:a", "-map", "1:a", "-map", "1:a",
        "-c:v", "libx264", "-preset", "ultrafast", "-crf", "16", "-pix_fmt", "yuv420p",
        "-c:a:0", "aac", "-b:a:0", "96k", "-c:a:1", "ac3", "-b:a:1", "192k", "-c:a:2", "libmp3lame", "-b:a:2", "320k", "-c:a:3", "flac",
        "-metadata:s:a:0", "language=eng", "-metadata:s:a:1", "language=fre", "-metadata:s:a:2", "language=ger", "-metadata:s:a:3", "language=spa", "src.mkv",
    ]);
    let src = c.videos().join("src.mkv");
    let tracks = |out: &Path| -> Vec<(String, String)> {
        streams(&probe(out), "audio").iter().map(|a| (a["codec_name"].as_str().unwrap().to_string(), tag(a, "language").unwrap_or("").to_string())).collect()
    };
    let named = |list: &[(&str, &str)]| -> Vec<(String, String)> { list.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect() };

    // Kept: AAC and MP3 as they are, the others become Opus. Order and languages stay.
    c.encode(&[&src], &["--container", "mkv"]).ok();
    let out = c.out().join("src.mkv");
    assert_eq!(tracks(&out), named(&[("aac", "eng"), ("opus", "fre"), ("mp3", "ger"), ("opus", "spa")]));
    assert_eq!(packets_md5(&out, "0:a:0"), packets_md5(&src, "0:a:0"));
    assert_eq!(packets_md5(&out, "0:a:2"), packets_md5(&src, "0:a:2"));

    // Shrunk: the 320 kbps MP3 goes too, the 96 kbps AAC stays, and the report says so.
    let lean = c.dir.join("out-shrink");
    c.encode_into(&lean, &[&src], &["--container", "mkv", "--audio", "shrink"]).ok();
    let out = lean.join("src.mkv");
    assert_eq!(tracks(&out), named(&[("aac", "eng"), ("opus", "fre"), ("opus", "ger"), ("opus", "spa")]));
    assert_eq!(packets_md5(&out, "0:a:0"), packets_md5(&src, "0:a:0"));
    let r: Value = serde_json::from_slice(&std::fs::read(lean.join("src.av1opt.json")).unwrap()).unwrap();
    let notes: Vec<&str> = r["notes"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    assert_eq!(notes, ["audio 1: ac3 -> Opus 128 kbps", "audio 2: mp3 320 kbps -> Opus 96 kbps", "audio 3: flac -> Opus 128 kbps"]);
    // Every track still lasts as long as the video.
    for a in streams(&probe(&out), "audio") {
        let secs = tag(a, "DURATION").map(|d| d.split(':').fold(0.0, |acc, part| acc * 60.0 + part.parse::<f64>().unwrap_or(0.0))).unwrap_or(0.0);
        assert!((secs - 6.0).abs() < 0.15, "{a}");
    }
}

/// Energy of each channel of audio track `track`, second by second: `[second][channel]`.
fn channel_energy(path: &Path, track: usize, channels: usize) -> Vec<Vec<f64>> {
    let o = Command::new(ffmpeg_bin())
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(path)
        .args(["-map", &format!("0:a:{track}"), "-ar", "48000", "-f", "f32le", "-"])
        .output()
        .expect("run ffmpeg");
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let samples: Vec<f32> = o.stdout.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
    samples.chunks(48000 * channels).map(|second| (0..channels).map(|ch| second.iter().skip(ch).step_by(channels).map(|x| f64::from(*x).powi(2)).sum()).collect()).collect()
}

/// Second `k` of the clip sounds in channel `k` alone: one tone per channel, one after the other.
fn tone_per_channel(channels: usize, layout: &str, secs: u32) -> String {
    let tone = |k: usize| format!("if(between(t\\,{k}.1\\,{k}.9)\\,0.5*sin(2*PI*{}*t)\\,0)", if k == 3 && layout.contains("5.1") { 80 } else { 440 + 110 * k });
    format!("aevalsrc={}:c={layout}:s=48000:d={secs}", (0..channels).map(tone).collect::<Vec<_>>().join("|"))
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn surround_sound_libopus_would_refuse_keeps_every_channel_in_its_place() {
    let c = Case::new("surround");
    // ffmpeg's AC-3 decoder calls ordinary 5.1 "5.1(side)", which libopus refuses; 2.1 has
    // no Opus layout of three channels at all. (The low frequencies are a low tone.)
    let side = tone_per_channel(6, "5.1(side)", 8);
    let low = "aevalsrc=if(between(t\\,0.1\\,0.9)\\,0.5*sin(2*PI*440*t)\\,0)|if(between(t\\,1.1\\,1.9)\\,0.5*sin(2*PI*550*t)\\,0)|if(between(t\\,2.1\\,2.9)\\,0.5*sin(2*PI*60*t)\\,0):c=2.1:s=48000:d=8";
    c.ffmpeg(&[
        "-f", "lavfi", "-i", "testsrc2=s=320x180:r=24:d=8", "-f", "lavfi", "-i", &side, "-f", "lavfi", "-i", low,
        "-map", "0:v", "-map", "1:a", "-map", "2:a", "-c:v", "libx264", "-preset", "ultrafast", "-crf", "16", "-pix_fmt", "yuv420p",
        "-c:a:0", "ac3", "-b:a:0", "384k", "-c:a:1", "ac3", "src.mkv",
    ]);
    let src = c.videos().join("src.mkv");
    let layouts = |p: &Path| streams(&probe(p), "audio").iter().map(|a| a["channel_layout"].as_str().unwrap_or("").to_string()).collect::<Vec<_>>();
    assert_eq!(layouts(&src), ["5.1(side)", "2.1"]);
    for container in ["mp4", "mkv"] {
        let out_dir = c.dir.join(format!("out-{container}"));
        c.encode_into(&out_dir, &[&src], &["--container", container]).ok();
        let out = out_dir.join(format!("src.{container}"));
        assert_eq!(layouts(&out), ["5.1", "5.1"], "{container}");
        let rep = c.report_in(&out_dir, "src");
        assert!(rep["notes"].to_string().contains("audio 0: ac3 -> Opus 384 kbps, 5.1(side) as 5.1"), "{rep}");
        assert!(rep["notes"].to_string().contains("audio 1: ac3 -> Opus 192 kbps, 2.1 as 5.1: its channels where they were, 3 silent"), "{rep}");
        // Each second's tone is in the channel it was in, and nowhere else to speak of.
        let (before, after) = (channel_energy(&src, 0, 6), channel_energy(&out, 0, 6));
        for k in 0..6 {
            let loudest = (0..6).max_by(|&a, &b| after[k][a].total_cmp(&after[k][b])).unwrap();
            assert_eq!(loudest, k, "{container}: second {k}: {:?}", after[k]);
            assert!((after[k][k] / before[k][k] - 1.0).abs() < 0.3, "{container}: channel {k} went from {} to {}", before[k][k], after[k][k]);
            assert!((0..6).filter(|&j| j != k).all(|j| after[k][j] < after[k][k] / 100.0), "{container}: second {k}: {:?}", after[k]);
        }
        // 2.1 in 5.1: left, right and low frequencies in theirs; centre and surrounds silent.
        let after = channel_energy(&out, 1, 6);
        for (k, place) in [(0, 0), (1, 1), (2, 3)] {
            let loudest = (0..6).max_by(|&a, &b| after[k][a].total_cmp(&after[k][b])).unwrap();
            assert_eq!(loudest, place, "{container}: second {k}: {:?}", after[k]);
        }
        assert!(after.iter().all(|second| [2, 4, 5].iter().all(|&j| second[j] < 1e-3)), "{container}: {after:?}");
    }
    // Channels Opus has no place for: kept as they are where the container holds them, and
    // refused before any encoding where it does not.
    c.ffmpeg(&["-f", "lavfi", "-i", "testsrc2=s=320x180:r=24:d=4", "-f", "lavfi", "-i", "sine=d=4,aformat=channel_layouts=7.1(wide)",
        "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", "-c:a", "pcm_s24le", "wide.mov"]);
    let wide = c.videos().join("wide.mov");
    let r = c.encode_into(&c.dir.join("wide-mp4"), &[&wide], &[]);
    r.has("audio 0 (pcm_s24le, 7.1(wide), 8 channels) cannot be kept").has("--container mkv keeps it as it is").has("failed 1");
    assert!(!r.err.contains("searching CRF"), "refused after the search:\n{}", r.err);
    c.encode_into(&c.dir.join("wide-mkv"), &[&wide], &["--container", "mkv"]).ok();
    let out = c.dir.join("wide-mkv/wide.mkv");
    assert_eq!(streams(&probe(&out), "audio")[0]["codec_name"], "pcm_s24le");
    assert_eq!(packets_md5(&out, "0:a:0"), packets_md5(&wide, "0:a:0"));
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn removed_sections_are_gone_to_the_frame() {
    let c = Case::new("remove");
    let src = c.clip("talk.mp4", "320x180", 10);
    assert_eq!(frames(&src), 240);
    // Out of order and overlapping: 2 s to 4 s and 8 s to the end go, 144 frames stay.
    c.encode(&[&src], &["--remove", "8-end", "--remove", "0:02-3.5", "--remove", "3-4"]).ok();
    let out = c.out().join("talk.mp4");
    assert_eq!(frames(&out), 144);
    assert_in_step(&out, &src, "lt(n,48)+between(n,96,191)");
    let p = probe(&out);
    assert!((duration(&out) - 6.0).abs() < 0.1, "{}", p["format"]);
    let a = streams(&p, "audio");
    assert_eq!(a[0]["codec_name"], "opus");
    assert!((a[0]["duration"].as_str().unwrap().parse::<f64>().unwrap() - 6.0).abs() < 0.1, "{}", a[0]);
    let r = c.report("talk");
    assert_eq!(r["removed"].as_array().map(Vec::len), Some(2), "{r}");

    // The same output is done; other sections for the same output are refused, not mixed up.
    c.encode(&[&src], &["--remove", "8-end", "--remove", "2-4"]).ok().has("already done 1");
    let r = c.encode(&[&src], &["--remove", "1-2"]);
    assert_eq!(r.code, Some(3), "{}", r.err);
    r.has("other sections removed");
    // And two files at once are refused before anything is encoded.
    let other = c.clip("other.mp4", "320x180", 4);
    let r = c.encode(&[&src, &other], &["--remove", "1-2"]);
    assert_ne!(r.code, Some(0));
    r.has("--remove applies to one video");
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn tags_are_kept_dropped_set_and_filled_from_info_files() {
    let c = Case::new("meta");
    c.clip("plain.mp4", "320x180", 6);
    c.ffmpeg(&["-i", "plain.mp4", "-c", "copy", "-metadata", "title=Old Title", "-metadata", "artist=Someone", "-metadata", "comment=A note", "-metadata:s:a:0", "language=fre", "clip.mp4"]);
    let src = c.videos().join("clip.mp4");
    let then = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_500_000_000);
    std::fs::File::options().write(true).open(&src).unwrap().set_modified(then).unwrap();
    let format = |out: &Path| probe(out)["format"].clone();

    // By default the tags come along, and the date does not.
    c.encode(&[&src], &[]).ok();
    let f = format(&c.out().join("clip.mp4"));
    assert_eq!((tag(&f, "title"), tag(&f, "artist"), tag(&f, "comment")), (Some("Old Title"), Some("Someone"), Some("A note")));
    assert_ne!(mtime(&c.out().join("clip.mp4")), then);

    // Stripped: tags go, the track language stays, the date is the source's when asked.
    let bare = c.dir.join("out-bare");
    c.encode_into(&bare, &[&src], &["--strip-metadata", "--keep-dates"]).ok();
    let p = probe(&bare.join("clip.mp4"));
    assert_eq!((tag(&p["format"], "title"), tag(&p["format"], "artist"), tag(&p["format"], "comment")), (None, None, None));
    assert_eq!(tag(streams(&p, "audio")[0], "language"), Some("fre"));
    assert_eq!(mtime(&bare.join("clip.mp4")), then);

    // Set one by one: a template over the source's tags and the file name.
    let set = c.dir.join("out-set");
    c.encode_into(&set, &[&src], &["--meta", "title={title} ({file})", "--meta", "genre=Documentary", "--meta", "album={nothing|artist}"]).ok();
    let f = format(&set.join("clip.mp4"));
    assert_eq!((tag(&f, "title"), tag(&f, "genre"), tag(&f, "album"), tag(&f, "artist")), (Some("Old Title (clip)"), Some("Documentary"), Some("Someone"), Some("Someone")));

    // A yt-dlp info file next to the video fills the youtube preset.
    let info = r#"{"title": "From YouTube", "uploader": "A Channel", "upload_date": "20240131", "webpage_url": "https://example.com/watch?v=1", "description": "Two\nlines"}"#;
    std::fs::write(c.videos().join("plain.info.json"), info).unwrap();
    let yt = c.dir.join("out-yt");
    c.encode_into(&yt, &[&c.videos().join("plain.mp4")], &["--container", "mkv", "--meta-preset", "youtube"]).ok();
    let f = format(&yt.join("plain.mkv"));
    assert_eq!((tag(&f, "title"), tag(&f, "artist"), tag(&f, "date"), tag(&f, "purl")), (Some("From YouTube"), Some("A Channel"), Some("20240131"), Some("https://example.com/watch?v=1")));
    assert_eq!(tag(&f, "description"), Some("Two\nlines"));
    let r: Value = serde_json::from_slice(&std::fs::read(yt.join("plain.av1opt.json")).unwrap()).unwrap();
    assert_eq!(r["tags"]["title"], "From YouTube");
    // MP4 has no place for the URL tag: said in a note, the other tags are written.
    let yt4 = c.dir.join("out-yt4");
    c.encode_into(&yt4, &[&c.videos().join("plain.mp4")], &["--meta-preset", "youtube"]).ok();
    let f = format(&yt4.join("plain.mp4"));
    assert_eq!((tag(&f, "title"), tag(&f, "artist"), tag(&f, "purl")), (Some("From YouTube"), Some("A Channel"), None));
    let r: Value = serde_json::from_slice(&std::fs::read(yt4.join("plain.av1opt.json")).unwrap()).unwrap();
    assert!(r["notes"].to_string().contains("purl"), "{}", r["notes"]);
    // Without an info file, or told not to read it, the preset goes by the source's own tags.
    let own = c.dir.join("out-own");
    c.encode_into(&own, &[&src], &["--container", "mkv", "--meta-preset", "youtube"]).ok();
    assert_eq!((tag(&format(&own.join("clip.mkv")), "title"), tag(&format(&own.join("clip.mkv")), "artist")), (Some("Old Title"), Some("Someone")));
    let blind = c.dir.join("out-blind");
    c.encode_into(&blind, &[&c.videos().join("plain.mp4")], &["--container", "mkv", "--meta-preset", "youtube", "--no-info-json"]).ok();
    assert_eq!(tag(&format(&blind.join("plain.mkv")), "title"), None);
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn odd_names_and_folders_are_handled() {
    let c = Case::new("names");
    let folder = c.videos().join("a 'quoted' folder: with, [brackets] & 100%");
    std::fs::create_dir_all(&folder).unwrap();
    let plain = c.clip("plain.mp4", "320x180", 4);
    let names = ["it's [one]: a, b; c.mp4", "-dash first %05d #.mp4", "ünï cödé 日本 \"q\".mp4", "two.dots.and spaces .mp4"];
    for n in names {
        std::fs::copy(&plain, folder.join(n)).unwrap();
    }
    std::fs::remove_file(&plain).unwrap();
    // The output folder is given relative to where av1opt runs, with a colon in it.
    let out = "o ut's [a]:b,c";
    let mut args: Vec<std::ffi::OsString> = vec!["encode".into(), "-o".into(), out.into()];
    args.extend(FAST.iter().map(Into::into));
    args.push(c.videos().into());
    c.av1opt(&args).ok().has("encoded 4, skipped 0, failed 0");
    let mirrored = c.dir.join(out).join("a 'quoted' folder: with, [brackets] & 100%");
    for n in names {
        assert_eq!(frames(&mirrored.join(n)), 96, "{n}");
        assert!(mirrored.join(n).with_extension("av1opt.json").exists(), "{n}");
    }
    // Scene mode builds more ffmpeg arguments out of the same names.
    let scenes = c.dir.join("out, scenes: 'x'");
    c.encode_into(&scenes, &[&folder.join(names[0]), &folder.join(names[1])], &["--scenes"]).ok().has("encoded 2, skipped 0, failed 0");
    assert_eq!(frames(&scenes.join(names[1])), 96);
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn files_not_worth_it_are_skipped_once_and_bad_ones_fail_alone() {
    let c = Case::new("skips");
    let good = c.clip("good.mp4", "320x180", 4);
    // Already AV1.
    c.ffmpeg(&["-i", "good.mp4", "-c:v", "libsvtav1", "-preset", "12", "-crf", "40", "-c:a", "copy", "av1.mp4"]);
    // Already squeezed dry: nothing left to save at this quality.
    c.ffmpeg(&["-f", "lavfi", "-i", "testsrc2=s=320x180:r=24:d=4", "-c:v", "libx264", "-preset", "veryslow", "-crf", "42", "-pix_fmt", "yuv420p", "lean.mp4"]);
    // Not a video at all, and a sound-only file.
    std::fs::write(c.videos().join("broken.mp4"), vec![7u8; 50_000]).unwrap();
    c.ffmpeg(&["-f", "lavfi", "-i", "sine=f=440:d=3", "-c:a", "aac", "sound.mp4"]);

    let r = c.encode(&[&c.videos()], &[]);
    assert_eq!(r.code, Some(3), "{}", r.err);
    r.has("encoded 1, skipped 2, failed 2, already done 0");
    assert_eq!(c.outputs(), ["av1opt-log.tsv", "good.av1opt.json", "good.mp4"]);
    assert_eq!(frames(&good), frames(&c.out().join("good.mp4")));

    // Again: the skipped ones are not searched again, the failed ones are tried again.
    let r = c.encode(&[&c.videos()], &[]);
    r.has("encoded 0, skipped 2, failed 2, already done 1");
    assert_eq!(r.err.matches("skipped earlier").count(), 2, "{}", r.err);
    // Unless asked, or forced.
    let r = c.encode(&[&c.videos().join("lean.mp4"), &c.videos().join("av1.mp4")], &["--retry-skipped"]);
    assert_eq!(r.err.matches("skipped earlier").count(), 0, "{}", r.err);
    r.ok().has("encoded 0, skipped 2, failed 0");
    c.encode(&[&c.videos().join("av1.mp4")], &["--force"]).ok().has("encoded 1, skipped 0");
    assert_eq!(frames(&c.out().join("av1.mp4")), 96);
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn an_encode_never_lands_on_its_source_and_a_changed_source_is_noticed() {
    let c = Case::new("alias");
    let src = c.clip("film.mp4", "320x180", 4);
    let before = std::fs::read(&src).unwrap();
    // The source under the name the output would get: a second name for the same file.
    std::fs::hard_link(&src, c.out().join("film.mp4")).unwrap();
    let r = c.encode(&[&src], &["--overwrite"]);
    assert_eq!(r.code, Some(3), "{}", r.err);
    r.has("the output would be written over this same video");
    assert_eq!(std::fs::read(&src).unwrap(), before);
    // An output folder that is the folder of the videos by another way.
    let link = c.dir.join("same");
    std::os::unix::fs::symlink(c.videos(), &link).unwrap();
    for out in [link.clone(), c.videos().join("../videos")] {
        let r = c.encode_into(&out, &[&src], &["--overwrite"]);
        assert_eq!(r.code, Some(3), "{}", r.err);
        r.has("the output would be written over this same video");
        assert_eq!(std::fs::read(&src).unwrap(), before);
    }

    // Encoded for real; then the same name holds another video.
    std::fs::remove_file(c.out().join("film.mp4")).unwrap();
    c.encode(&[&src], &[]).ok().has("encoded 1, skipped 0, failed 0");
    let made = std::fs::read(c.out().join("film.mp4")).unwrap();
    c.encode(&[&src], &[]).ok().has("already done 1");
    c.clip("film.mp4", "320x180", 6);
    let r = c.encode(&[&src], &[]);
    assert_eq!(r.code, Some(3), "{}", r.err);
    r.has("was made from this file when it was another size");
    assert_eq!(std::fs::read(c.out().join("film.mp4")).unwrap(), made);
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn folders_are_mirrored_and_same_names_do_not_collide_silently() {
    let c = Case::new("layout");
    let one = c.clip("one.mp4", "320x180", 4);
    for d in ["show/season 1", "show/season 2", "other"] {
        std::fs::create_dir_all(c.videos().join(d)).unwrap();
    }
    std::fs::copy(&one, c.videos().join("show/season 1/e1.mp4")).unwrap();
    std::fs::copy(&one, c.videos().join("show/season 2/e1.mp4")).unwrap();
    std::fs::copy(&one, c.videos().join("other/e1.mp4")).unwrap();
    std::fs::write(c.videos().join("show/notes.txt"), "not a video").unwrap();
    std::fs::remove_file(&one).unwrap();

    c.encode(&[&c.videos().join("show")], &[]).ok().has("encoded 2, skipped 0, failed 0");
    assert!(c.out().join("season 1/e1.mp4").exists() && c.out().join("season 2/e1.mp4").exists());
    assert!(!c.out().join("notes.txt").exists());

    // Two files named alike, given one by one, would land on one output: the second says so.
    let flat = c.dir.join("out-flat");
    let r = c.encode_into(&flat, &[&c.videos().join("show/season 1/e1.mp4"), &c.videos().join("other/e1.mp4")], &[]);
    assert_eq!(r.code, Some(3), "{}", r.err);
    r.has("encoded 1, skipped 0, failed 1").has("output name collision");

    // The folders they sit in, given together, each keep their name: nothing collides.
    let apart = c.dir.join("out-apart");
    let r = c.encode_into(&apart, &[&c.videos().join("show/season 1"), &c.videos().join("other")], &[]);
    r.ok().has("encoded 2, skipped 0, failed 0");
    assert!(apart.join("season 1/e1.mp4").exists() && apart.join("other/e1.mp4").exists(), "{:?}", c.outputs_in(&apart));
}

/// The first video stream as ffprobe sees it (covers left out).
fn video(path: &Path) -> Value {
    streams(&probe(path), "video")[0].clone()
}

/// Start of the first sound track minus start of the video, in seconds.
fn sound_lead(path: &Path) -> f64 {
    let p = probe(path);
    let start = |s: &Value| s["start_time"].as_str().and_then(|t| t.parse::<f64>().ok()).unwrap_or_else(|| panic!("no start time: {s}"));
    start(streams(&p, "audio")[0]) - start(streams(&p, "video")[0])
}

/// When each frame of the first video stream is shown, from the first one, in seconds.
fn frame_times(path: &Path) -> Vec<f64> {
    let out = ffprobe(&["-select_streams", "v:0", "-show_entries", "frame=best_effort_timestamp_time", "-of", "csv=p=0"], path);
    let mut t: Vec<f64> = out.lines().filter_map(|l| l.trim().trim_end_matches(',').parse().ok()).collect();
    t.sort_by(f64::total_cmp);
    let first = t.first().copied().unwrap_or(0.0);
    t.iter().map(|x| x - first).collect()
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn interlaced_video_comes_out_progressive() {
    let c = Case::new("interlaced");
    // 50 fields a second, top field first, as a DVD or a TV recording has them.
    c.ffmpeg(&[
        "-f", "lavfi", "-i", "testsrc2=s=352x288:r=50:d=6", "-f", "lavfi", "-i", "sine=f=440:d=6", "-vf", "tinterlace=interleave_top,fieldorder=tff",
        "-c:v", "mpeg2video", "-flags", "+ilme+ildct", "-q:v", "3", "-c:a", "mp2", "-b:a", "128k", "tv.ts",
    ]);
    let src = c.videos().join("tv.ts");
    assert_eq!(video(&src)["field_order"], "tt");
    c.encode(&[&src], &[]).ok().has("encoded 1,");
    let out = c.out().join("tv.mp4");
    let v = video(&out);
    assert_eq!(v["field_order"], "progressive", "{v}");
    assert_eq!(v["r_frame_rate"], "25/1");
    assert_eq!(frames(&out), frames(&src));
    assert_eq!(streams(&probe(&out), "audio")[0]["codec_name"], "opus");
    assert!((sound_lead(&out) - sound_lead(&src)).abs() < 0.03, "{} against {}", sound_lead(&out), sound_lead(&src));
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn variable_frame_rate_keeps_every_frame_at_its_time() {
    let c = Case::new("vfr");
    // The first two seconds have all 24 frames, the rest every third: a screen recording.
    c.ffmpeg(&[
        "-f", "lavfi", "-i", "testsrc2=s=320x180:r=24:d=8", "-f", "lavfi", "-i", "sine=f=440:d=8", "-vf", "select='lt(n,48)+not(mod(n,3))'", "-fps_mode", "vfr",
        "-c:v", "libx264", "-preset", "ultrafast", "-crf", "16", "-pix_fmt", "yuv420p", "-c:a", "aac", "-b:a", "64k", "screen.mkv",
    ]);
    let src = c.videos().join("screen.mkv");
    let want = frame_times(&src);
    assert_eq!(want.len(), 96);
    for (extra, dir) in [(&[][..], "out"), (&["--scenes"][..], "out-scenes"), (&["--container", "mkv"][..], "out-mkv")] {
        let out_dir = c.dir.join(dir);
        c.encode_into(&out_dir, &[&src], extra).ok().has("encoded 1,");
        let out = out_dir.join(if dir == "out-mkv" { "screen.mkv" } else { "screen.mp4" });
        let got = frame_times(&out);
        assert_eq!(got.len(), want.len(), "{dir}");
        let worst = got.iter().zip(&want).map(|(a, b)| (a - b).abs()).fold(0.0, f64::max);
        assert!(worst < 0.012, "{dir}: a frame is {worst:.3} s from its place");
        assert_in_step(&out, &src, "1");
        assert!((sound_lead(&out) - sound_lead(&src)).abs() < 0.03, "{dir}");
    }
    // Matroska's header names 24 fps here whatever the frames do. Scene mode cuts chunks at
    // a constant rate, which would show 94 frames twice: it notices and encodes in one pass.
    let r: Value = serde_json::from_slice(&std::fs::read(c.dir.join("out-scenes/screen.av1opt.json")).unwrap()).unwrap();
    assert_eq!(r["mode"], "whole");
    assert!(r["notes"].to_string().contains("uneven times (96 frames; at a constant 24 fps 0 would be dropped and 94 shown twice)"), "{}", r["notes"]);
    assert!(std::fs::read_dir(c.dir.join("out-scenes")).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().starts_with(".av1opt-")));

    // A camera whose clock drifts: every 25th frame stays a quarter longer, on a 90 kHz
    // clock, under a header that names a rate. Counted in frames of that rate, each frame
    // was moved to the nearest whole one (up to half a frame off, and a spot check then
    // compared frames with their neighbours). In one pass too, every frame keeps its tick.
    c.ffmpeg(&[
        "-f", "lavfi", "-i", "testsrc2=s=320x180:r=24:d=8", "-f", "lavfi", "-i", "sine=f=440:d=8", "-vf", "settb=1/90000,setpts='N*3750+floor(N/25)*900'",
        "-fps_mode", "passthrough", "-enc_time_base", "1:90000", "-video_track_timescale", "90000",
        "-c:v", "libx264", "-preset", "ultrafast", "-crf", "16", "-pix_fmt", "yuv420p", "-c:a", "aac", "-b:a", "64k", "drift.mp4",
    ]);
    let src = c.videos().join("drift.mp4");
    let out_dir = c.dir.join("out-drift");
    c.encode_into(&out_dir, &[&src], &[]).ok().has("encoded 1,");
    let (got, want) = (frame_times(&out_dir.join("drift.mp4")), frame_times(&src));
    assert_eq!((got.len(), want.len()), (192, 192));
    assert!(want.windows(2).any(|w| w[1] - w[0] > 0.05), "the source's frames should come unevenly");
    let worst = got.iter().zip(&want).map(|(a, b)| (a - b).abs()).fold(0.0, f64::max);
    assert!(worst < 0.00002, "a frame is {worst:.6} s from its place");
    assert_in_step(&out_dir.join("drift.mp4"), &src, "1");
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn a_phone_video_stays_the_way_up_it_plays() {
    let c = Case::new("rotated");
    c.clip("flat.mp4", "320x180", 4);
    c.ffmpeg(&["-display_rotation", "90", "-i", "flat.mp4", "-c", "copy", "phone.mp4"]);
    let src = c.videos().join("phone.mp4");
    c.encode(&[&src], &[]).ok().has("encoded 1,");
    let out = c.out().join("phone.mp4");
    // Played, both are 180 wide and 320 high, and show the same picture (ffmpeg turns frames
    // by the rotation it finds, so the comparison is of what a player shows).
    let shown = |p: &Path| -> (u64, u64) {
        let v = video(p);
        let turned = v["side_data_list"].as_array().and_then(|l| l.iter().find_map(|d| d["rotation"].as_f64())).is_some_and(|r| (r.abs() - 90.0).abs() < 1.0 || (r.abs() - 270.0).abs() < 1.0);
        let (w, h) = (v["width"].as_u64().unwrap(), v["height"].as_u64().unwrap());
        if turned { (h, w) } else { (w, h) }
    };
    assert_eq!(shown(&src), (180, 320));
    assert_eq!(shown(&out), (180, 320));
    assert_eq!(frames(&out), 96);
    assert_in_step(&out, &src, "1");
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn cover_art_is_kept_and_never_taken_for_the_video() {
    let c = Case::new("covers");
    c.clip("plain.mp4", "320x180", 4);
    c.ffmpeg(&["-f", "lavfi", "-i", "color=c=red:s=64x64", "-frames:v", "1", "cover.jpg"]);
    // The cover after the video, the cover before it, and as a Matroska attachment.
    c.ffmpeg(&["-i", "plain.mp4", "-i", "cover.jpg", "-map", "0", "-map", "1", "-c", "copy", "-disposition:v:1", "attached_pic", "after.mp4"]);
    c.ffmpeg(&["-i", "plain.mp4", "-i", "cover.jpg", "-map", "1", "-map", "0", "-c", "copy", "-disposition:v:0", "attached_pic", "before.mp4"]);
    c.ffmpeg(&["-i", "plain.mp4", "-attach", "cover.jpg", "-metadata:s:t", "mimetype=image/jpeg", "-metadata:s:t", "filename=folder.jpg", "-c", "copy", "attached.mkv"]);
    std::fs::remove_file(c.videos().join("plain.mp4")).unwrap();
    std::fs::remove_file(c.videos().join("cover.jpg")).unwrap();

    let covers = |p: &Path| -> Vec<Value> {
        probe(p)["streams"].as_array().unwrap().iter().filter(|s| s["disposition"]["attached_pic"] == 1 || s["codec_type"] == "attachment").cloned().collect()
    };
    c.encode(&[&c.videos()], &[]).ok().has("encoded 3, skipped 0, failed 0");
    for name in ["after.mp4", "before.mp4", "attached.mp4"] {
        let out = c.out().join(name);
        let v = video(&out);
        assert_eq!((v["codec_name"].as_str(), v["width"].as_u64()), (Some("av1"), Some(320)), "{name}");
        assert_eq!(frames_of(&out, &v), 96, "{name}");
        let cv = covers(&out);
        assert_eq!(cv.len(), 1, "{name}");
        assert_eq!((cv[0]["codec_name"].as_str(), cv[0]["width"].as_u64()), (Some("mjpeg"), Some(64)), "{name}");
    }
    // In Matroska a cover is an attachment with a file name (ffprobe shows a picture
    // attachment as a cover stream carrying that name), the one it had if it had one.
    let mkv = c.dir.join("out-mkv");
    c.encode_into(&mkv, &[&c.videos()], &["--container", "mkv"]).ok().has("encoded 3, skipped 0, failed 0");
    for name in ["after.mkv", "before.mkv", "attached.mkv"] {
        let cv = covers(&mkv.join(name));
        assert_eq!(cv.len(), 1, "{name}");
        assert_eq!((cv[0]["codec_name"].as_str(), tag(&cv[0], "mimetype")), (Some("mjpeg"), Some("image/jpeg")), "{name}: {}", cv[0]);
        assert!(tag(&cv[0], "filename").is_some_and(|f| f.ends_with(".jpg")), "{name}: {}", cv[0]);
        assert_eq!(frames_of(&mkv.join(name), &video(&mkv.join(name))), 96, "{name}");
    }
    assert_eq!(tag(&covers(&mkv.join("attached.mkv"))[0], "filename"), Some("folder.jpg"));
}

/// Frames of one given video stream, counted by decoding.
fn frames_of(path: &Path, stream: &Value) -> u64 {
    let spec = stream["index"].as_u64().unwrap().to_string();
    let out = ffprobe(&["-select_streams", &spec, "-count_frames", "-show_entries", "stream=nb_read_frames", "-of", "csv=p=0"], path);
    out.trim().trim_end_matches(',').parse().unwrap_or_else(|_| panic!("frame count of {}: {out:?}", path.display()))
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn odd_sizes_full_chroma_and_wide_pixels() {
    let c = Case::new("shapes");
    // Odd width and height, 4:4:4.
    c.ffmpeg(&["-f", "lavfi", "-i", "testsrc2=s=321x181:r=24:d=4", "-c:v", "libx264", "-preset", "ultrafast", "-crf", "16", "-pix_fmt", "yuv444p", "odd.mkv"]);
    // 4:2:2, 10 bits.
    c.ffmpeg(&["-f", "lavfi", "-i", "testsrc2=s=320x180:r=24:d=4", "-c:v", "libx264", "-preset", "ultrafast", "-crf", "16", "-pix_fmt", "yuv422p10le", "p422.mkv"]);
    // A DVD's pixels: 352x288 shown 16:9.
    c.ffmpeg(&["-f", "lavfi", "-i", "testsrc2=s=352x288:r=25:d=4", "-vf", "setsar=16/11", "-c:v", "libx264", "-preset", "ultrafast", "-crf", "16", "-pix_fmt", "yuv420p", "wide.mp4"]);
    // Full-range colour, as phones and screen recorders write.
    c.ffmpeg(&["-f", "lavfi", "-i", "testsrc2=s=320x180:r=24:d=4", "-vf", "scale=out_range=full,setparams=range=full", "-c:v", "libx264", "-preset", "ultrafast", "-crf", "16", "-pix_fmt", "yuvj420p", "full.mp4"]);
    c.encode(&[&c.videos()], &[]).ok().has("encoded 4, skipped 0, failed 0");

    let v = video(&c.out().join("odd.mp4"));
    assert_eq!((v["width"].as_u64(), v["height"].as_u64(), v["pix_fmt"].as_str()), (Some(320), Some(180), Some("yuv420p10le")));
    assert_eq!(frames(&c.out().join("odd.mp4")), 96);
    let v = video(&c.out().join("p422.mp4"));
    assert_eq!(v["pix_fmt"], "yuv420p10le");
    assert_in_step(&c.out().join("p422.mp4"), &c.videos().join("p422.mkv"), "1");
    let (src, out) = (video(&c.videos().join("wide.mp4")), video(&c.out().join("wide.mp4")));
    assert_eq!(src["sample_aspect_ratio"], "16:11");
    assert_eq!((out["sample_aspect_ratio"].as_str(), out["display_aspect_ratio"].as_str()), (Some("16:11"), src["display_aspect_ratio"].as_str()));
    let (src, out) = (video(&c.videos().join("full.mp4")), video(&c.out().join("full.mp4")));
    assert_eq!(src["color_range"], "pc");
    assert_eq!(out["color_range"], "pc", "{out}");
    assert_in_step(&c.out().join("full.mp4"), &c.videos().join("full.mp4"), "1");
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn sound_and_picture_start_as_far_apart_as_before() {
    let c = Case::new("sync");
    c.clip("plain.mp4", "320x180", 6);
    // A transport stream starts well after zero; a delayed sound track; a late video.
    c.ffmpeg(&["-i", "plain.mp4", "-c", "copy", "-muxdelay", "1.4", "stream.ts"]);
    c.ffmpeg(&["-i", "plain.mp4", "-itsoffset", "0.5", "-i", "plain.mp4", "-map", "0:v", "-map", "1:a", "-c", "copy", "late-sound.mkv"]);
    c.ffmpeg(&["-itsoffset", "0.5", "-i", "plain.mp4", "-i", "plain.mp4", "-map", "0:v", "-map", "1:a", "-c", "copy", "late-picture.mkv"]);
    std::fs::remove_file(c.videos().join("plain.mp4")).unwrap();
    for container in ["mp4", "mkv"] {
        let out_dir = c.dir.join(format!("out-{container}"));
        c.encode_into(&out_dir, &[&c.videos()], &["--container", container]).ok().has("encoded 3, skipped 0, failed 0");
        for name in ["stream.ts", "late-sound.mkv", "late-picture.mkv"] {
            let (src, out) = (c.videos().join(name), out_dir.join(name).with_extension(container));
            let (before, after) = (sound_lead(&src), sound_lead(&out));
            assert!((before - after).abs() < 0.03, "{name} to {container}: sound led by {before:.3} s, now {after:.3} s");
            assert_eq!(frames(&out), 144, "{name} to {container}");
        }
    }
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn other_containers_codecs_and_frame_rates_go_in() {
    let c = Case::new("formats");
    let make = |name: &str, rate: &str, codec: &[&str]| {
        let v = format!("testsrc2=s=320x180:r={rate}:d=4");
        let mut args = vec!["-f", "lavfi", "-i", &v, "-f", "lavfi", "-i", "sine=f=440:d=4"];
        args.extend(codec);
        args.push(name);
        c.ffmpeg(&args);
    };
    make("old.avi", "25", &["-c:v", "mpeg4", "-q:v", "3", "-c:a", "libmp3lame", "-b:a", "128k"]);
    make("web.webm", "30", &["-c:v", "libvpx-vp9", "-deadline", "realtime", "-cpu-used", "8", "-crf", "20", "-b:v", "0", "-c:a", "libopus", "-b:a", "64k"]);
    make("film.mkv", "24000/1001", &["-c:v", "libx264", "-preset", "ultrafast", "-crf", "16", "-bf", "3", "-c:a", "ac3", "-b:a", "192k"]);
    make("ntsc.mov", "30000/1001", &["-c:v", "libx264", "-preset", "ultrafast", "-crf", "16", "-pix_fmt", "yuv420p", "-c:a", "pcm_s16le"]);
    make("game.mp4", "60", &["-c:v", "libx265", "-preset", "ultrafast", "-crf", "16", "-x265-params", "log-level=error", "-tag:v", "hvc1", "-c:a", "aac"]);
    make("disc.mpg", "25", &["-c:v", "mpeg2video", "-q:v", "3", "-c:a", "mp2", "-b:a", "128k"]);
    c.encode(&[&c.videos()], &[]).ok().has("encoded 6, skipped 0, failed 0");
    for (name, rate, sound) in [("old.avi", "25/1", "mp3"), ("web.webm", "30/1", "opus"), ("film.mkv", "24000/1001", "opus"), ("ntsc.mov", "30000/1001", "opus"), ("game.mp4", "60/1", "aac"), ("disc.mpg", "25/1", "opus")] {
        let (src, out) = (c.videos().join(name), c.out().join(name).with_extension("mp4"));
        let p = probe(&out);
        assert_eq!(streams(&p, "video")[0]["r_frame_rate"], rate, "{name}");
        assert_eq!(streams(&p, "audio")[0]["codec_name"], sound, "{name}");
        assert_eq!(frames(&out), frames(&src), "{name}");
        assert_in_step(&out, &src, "1");
        assert!((duration(&out) - duration(&src)).abs() < 0.1, "{name}: {} against {}", duration(&out), duration(&src));
    }
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn raw_dv_is_encoded_at_its_own_frame_rate() {
    let c = Case::new("dv");
    // ffprobe reads its average rate as 60000 a second, from the demuxer's clock.
    c.ffmpeg(&[
        "-f", "lavfi", "-i", "testsrc2=s=720x576:r=25:d=4", "-f", "lavfi", "-i", "sine=f=440:d=4:r=48000",
        "-c:v", "dvvideo", "-pix_fmt", "yuv420p", "-c:a", "pcm_s16le", "-ac", "2", "-f", "dv", "tape.dv",
    ]);
    let src = c.videos().join("tape.dv");
    assert_eq!(video(&src)["avg_frame_rate"], "60000/1", "the case this is about");
    c.encode(&[&src], &[]).ok().has("720x576 25.00 fps").has("encoded 1, skipped 0, failed 0");
    let out = c.out().join("tape.mp4");
    assert_eq!(video(&out)["r_frame_rate"], "25/1");
    assert_eq!(frames(&out), frames(&src));
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn very_short_videos_are_encoded_or_refused_cleanly() {
    let c = Case::new("short");
    for (name, n) in [("one-second.mp4", "24"), ("half.mp4", "12"), ("three-frames.mp4", "3"), ("one-frame.mp4", "1")] {
        c.ffmpeg(&["-f", "lavfi", "-i", "testsrc2=s=320x180:r=24:d=4", "-frames:v", n, "-c:v", "libx264", "-preset", "ultrafast", "-crf", "16", "-pix_fmt", "yuv420p", name]);
    }
    let r = c.encode(&[&c.videos()], &["--force"]);
    assert!(!r.err.contains("panicked"), "{}", r.err);
    // Whatever was written is whole; whatever was not left nothing behind.
    for (name, n) in [("one-second.mp4", 24), ("half.mp4", 12), ("three-frames.mp4", 3), ("one-frame.mp4", 1)] {
        let out = c.out().join(name);
        if out.exists() {
            assert_eq!(frames(&out), n, "{name}");
            assert!(out.with_extension("av1opt.json").exists(), "{name}");
        } else {
            assert!(!out.with_extension("av1opt.json").exists(), "{name}");
        }
    }
    assert!(c.outputs().iter().all(|n| !n.starts_with(".av1opt-")), "{:?}", c.outputs());
    // As it stands: a second is enough, less is skipped with the reason.
    r.ok().has("encoded 1, skipped 3, failed 0");
    assert_eq!(r.err.matches("skipped: no usable duration").count(), 3, "{}", r.err);
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn a_video_that_starts_late_is_measured_and_cut_from_where_it_starts() {
    // Matroska states when such a file ends, not how long it is, and an MPEG-TS seek lands
    // past the place it was sent to and gives no frames, without an error.
    let c = Case::new("late");
    let base = c.clip("base.mkv", "320x180", 4);
    for (name, format) in [("late-mkv.mkv", "matroska"), ("late-ts.ts", "mpegts")] {
        c.ffmpeg(&["-i", "base.mkv", "-c", "copy", "-output_ts_offset", "5", "-f", format, name]);
    }
    let want = frames(&base);
    std::fs::remove_file(&base).unwrap();
    c.encode(&[&c.videos()], &[]).ok().has("encoded 2, skipped 0, failed 0");
    for stem in ["late-mkv", "late-ts"] {
        let out = c.out().join(format!("{stem}.mp4"));
        assert_eq!(frames(&out), want, "{stem}");
        assert!((duration(&out) - 4.0).abs() < 0.2, "{stem}: {}", duration(&out));
        // Read as 9 s long, its samples and checks would lie past the end of the picture.
        let checks = c.report(stem)["verify_vmaf"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect::<Vec<_>>();
        assert!(!checks.is_empty() && checks.iter().all(|v| *v > 80.0), "{stem}: {checks:?}");
    }
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn a_stream_whose_seeks_land_late_is_cut_at_the_frames_asked_for() {
    // Key frames eight seconds apart in a file with no index: a seek lands on the next one, past
    // the sample, the spot check or the scene it was sent for. Cut from there, the checks compared
    // other frames than the encode's, and scenes came out short.
    let c = Case::new("late-seek");
    c.scenes_clip("scenes.mp4");
    c.ffmpeg(&[
        "-i", "scenes.mp4", "-map", "0:v", "-c:v", "libx264", "-preset", "ultrafast", "-crf", "16", "-pix_fmt", "yuv420p",
        "-g", "192", "-keyint_min", "192", "-sc_threshold", "0", "-f", "mpegts", "gop.ts",
    ]);
    std::fs::remove_file(c.videos().join("scenes.mp4")).unwrap();
    let src = c.videos().join("gop.ts");
    c.encode(&[&src], &["--scenes=always"]).ok().has("encoded 1, skipped 0, failed 0");
    let report = c.report("gop");
    assert!(report["chunks"].as_array().unwrap().len() > 1, "{report}");
    let checks: Vec<f64> = report["verify_vmaf"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect();
    assert!(!checks.is_empty() && checks.iter().all(|v| *v > 80.0), "{checks:?}");
    assert_eq!(frames(&c.out().join("gop.mp4")), 576);
}

/// Start of the first packet of a stream, in the file's timestamps.
fn first_packet(path: &Path, stream: &str) -> f64 {
    let out = ffprobe(&["-select_streams", stream, "-read_intervals", "%+#32", "-show_entries", "packet=pts_time", "-of", "csv=p=0"], path);
    out.lines().filter_map(|l| l.trim().trim_end_matches(',').parse::<f64>().ok()).fold(f64::INFINITY, f64::min)
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn a_picture_inside_longer_sound_keeps_its_place_in_every_container() {
    // Sound from 3 s to 15 s, the picture from 5 s to 13 s: Matroska states where the picture
    // ends and MP4 how long it is, and the check of the length has to see past that.
    let c = Case::new("picture-inside");
    c.ffmpeg(&[
        "-f", "lavfi", "-i", "testsrc2=s=320x180:r=24:d=8", "-f", "lavfi", "-i", "sine=f=440:d=12", "-map", "0", "-map", "1",
        "-c:v", "libx264", "-preset", "ultrafast", "-crf", "16", "-pix_fmt", "yuv420p", "-c:a", "aac", "-b:a", "64k", "base.mkv",
    ]);
    for name in ["inside.mkv", "inside.mp4"] {
        c.ffmpeg(&["-itsoffset", "5", "-i", "base.mkv", "-itsoffset", "3", "-i", "base.mkv", "-map", "0:v", "-map", "1:a", "-c", "copy", name]);
    }
    std::fs::remove_file(c.videos().join("base.mkv")).unwrap();
    for name in ["inside.mkv", "inside.mp4"] {
        for container in ["mp4", "mkv"] {
            let out_dir = c.dir.join(format!("out-{name}-{container}"));
            let src = c.videos().join(name);
            c.encode_into(&out_dir, &[&src], &["--container", container]).ok().has("encoded 1, skipped 0, failed 0");
            let out = out_dir.join("inside").with_extension(container);
            assert_eq!(frames(&out), 192, "{name} to {container}");
            let lead = |p: &Path| first_packet(p, "v:0") - first_packet(p, "a:0");
            assert!((lead(&out) - lead(&src)).abs() < 0.03, "{name} to {container}: the sound led by {:.3} s, now by {:.3} s", lead(&src), lead(&out));
            assert!(c.report_in(&out_dir, "inside")["verify_vmaf"].as_array().unwrap().iter().all(|v| v.as_f64().unwrap() > 80.0), "{name} to {container}");
        }
    }
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn a_file_cut_off_is_refused_with_the_reason_and_leaves_nothing() {
    let c = Case::new("cutoff");
    // Far before its stated end: the samples have no frames to come from.
    let long = c.clip("long.mkv", "320x180", 60);
    let bytes = std::fs::read(&long).unwrap();
    std::fs::write(&long, &bytes[..bytes.len() / 10]).unwrap();
    c.encode(&[&long], &[]).has("failed 1").has("no picture at").has("ends earlier than it says");
    // Near it: the samples are fine, and the encode comes out too short.
    let short = c.clip("short.mkv", "320x180", 4);
    let bytes = std::fs::read(&short).unwrap();
    std::fs::write(&short, &bytes[..bytes.len() * 6 / 10]).unwrap();
    c.encode(&[&short], &[]).has("failed 1").has("the source may be cut off or damaged");
    assert_eq!(c.outputs(), ["av1opt-log.tsv"]);
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn a_name_with_no_room_for_its_report_is_refused_before_the_work() {
    let c = Case::new("longname");
    let clip = c.clip("clip.mkv", "320x180", 2);
    // As long as a name can be here and still leave room for `.av1opt.json`, then one byte
    // more. (How long that is depends on the filesystem.)
    let takes = |n: &usize| std::fs::write(c.out().join("n".repeat(*n)), b"").is_ok() && std::fs::remove_file(c.out().join("n".repeat(*n))).is_ok();
    let longest = (1..=255).rev().find(takes).unwrap();
    let stem = longest - ".av1opt.json".len();
    let (fits, over) = (c.videos().join(format!("{}.mkv", "f".repeat(stem))), c.videos().join(format!("{}.mkv", "o".repeat(stem + 1))));
    std::fs::copy(&clip, &fits).unwrap();
    std::fs::rename(&clip, &over).unwrap();
    let r = c.encode(&[&over], &[]);
    r.has("failed 1").has("the file name is too long");
    assert!(!r.err.contains("sample 1/"), "{}", r.err);
    assert_eq!(c.outputs(), ["av1opt-log.tsv"]);
    c.encode(&[&fits], &[]).ok().has("encoded 1,");
    assert_eq!(c.outputs().len(), 3, "{:?}", c.outputs());
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn a_name_that_is_not_text_is_refused_and_no_other_file_is_taken_for_it() {
    use std::os::unix::ffi::OsStrExt;
    let c = Case::new("nottext");
    // What the name turns into when it is made text: another video, there to be opened.
    let other = c.clip("w\u{fffd}.mkv", "320x180", 2);
    let odd = c.videos().join(std::ffi::OsStr::from_bytes(b"w\xff.mkv"));
    std::fs::copy(&other, &odd).unwrap();
    c.encode(&[&odd], &[]).has("failed 1").has("not valid UTF-8");
    assert_eq!(c.outputs(), ["av1opt-log.tsv"]);
    // A scan of the folder goes through and lists both.
    let r = c.av1opt(&[std::ffi::OsStr::new("scan"), c.videos().as_os_str()]);
    r.ok().has("found 2 video files");
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn a_file_that_leads_to_other_videos_is_not_encoded_even_when_forced() {
    let c = Case::new("not-a-video");
    let elsewhere = c.dir.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::rename(c.clip("secret.mp4", "320x180", 4), elsewhere.join("secret.mp4")).unwrap();
    let secret = elsewhere.join("secret.mp4");
    let manifest = format!(
        "<?xml version=\"1.0\"?><MPD xmlns=\"urn:mpeg:dash:schema:mpd:2011\" type=\"static\" mediaPresentationDuration=\"PT3S\" minBufferTime=\"PT1S\" profiles=\"urn:mpeg:dash:profile:isoff-on-demand:2011\"><Period><AdaptationSet mimeType=\"video/mp4\"><Representation id=\"1\" bandwidth=\"1000\" width=\"320\" height=\"180\"><BaseURL>{}</BaseURL></Representation></AdaptationSet></Period></MPD>",
        secret.display()
    );
    std::fs::write(c.videos().join("dash.mkv"), manifest).unwrap();
    std::fs::write(c.videos().join("list.mkv"), format!("ffconcat version 1.0\nfile {}\nduration 4\n", secret.display())).unwrap();
    for name in ["dash.mkv", "list.mkv"] {
        c.encode(&[&c.videos().join(name)], &["--force"]).has("failed 1").has("not a video file in a format av1opt reads");
    }
    let made: Vec<String> = c.outputs().into_iter().filter(|n| n != "av1opt-log.tsv").collect();
    assert!(made.is_empty(), "{made:?}");
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn a_temp_folder_laid_out_ahead_of_the_run_is_not_used() {
    // What someone who can write to the output folder could do: the old folder name was the
    // process number, known here because the shell becomes av1opt under its own number.
    let c = Case::new("planted-temp");
    let src = c.clip("clip.mp4", "320x180", 4);
    let (report, video) = (c.dir.join("victim-report"), c.dir.join("victim-video"));
    std::fs::write(&report, b"precious report").unwrap();
    std::fs::write(&video, b"precious video").unwrap();
    let script = "t=\"$1/.av1opt-tmp-$$\"; mkdir \"$t\"; ln -s \"$2\" \"$t/clip.av1opt.json\"; ln -s \"$3\" \"$t/partial.mp4\"; shift 3; exec \"$@\"";
    let template = c.command();
    let mut sh = Command::new("sh");
    sh.current_dir(&c.dir).stdin(Stdio::null()).envs(template.get_envs().filter_map(|(k, v)| Some((k.to_owned(), v?.to_owned()))));
    let o = sh
        .args(["-c", script, "sh"])
        .args([c.out().as_os_str(), report.as_os_str(), video.as_os_str()])
        .arg(env!("CARGO_BIN_EXE_av1opt"))
        .arg("encode").arg("-o").arg(c.out()).args(FAST).arg("--force").arg(&src)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(std::fs::read(&report).unwrap(), b"precious report");
    assert_eq!(std::fs::read(&video).unwrap(), b"precious video");
    let out = c.out().join("clip.mp4");
    assert!(std::fs::symlink_metadata(&out).unwrap().file_type().is_file(), "the output is a file of its own");
    assert_eq!(frames(&out), 96);
    // The run's own folder is gone, and the one laid out is still as it was left.
    let left: Vec<PathBuf> = entries(&c.out(), ".av1opt-tmp-");
    assert_eq!(left.len(), 1, "{left:?}");
    assert!(left[0].join("partial.mp4").is_symlink());
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn sound_that_runs_on_after_the_picture_is_kept_whole() {
    // A recording left running: 12 s of picture under 30 s of sound. The container says 30 s,
    // so samples and spot checks used to be asked of seconds that have no frames.
    let c = Case::new("tail");
    // MP4 states a stream's length in the stream, Matroska in a tag.
    for name in ["tail.mp4", "tagged.mkv"] {
        c.ffmpeg(&[
            "-f", "lavfi", "-i", "testsrc2=s=320x180:r=24:d=12", "-f", "lavfi", "-i", "sine=f=440:d=30", "-c:v", "libx264", "-preset", "ultrafast",
            "-crf", "16", "-pix_fmt", "yuv420p", "-c:a", "aac", "-b:a", "64k", name,
        ]);
    }
    let r = c.encode(&[&c.videos()], &[]);
    r.ok().has("encoded 2, skipped 0, failed 0");
    for (name, stem) in [("tail.mp4", "tail"), ("tagged.mkv", "tagged")] {
        let out = c.out().join(format!("{stem}.mp4"));
        assert_eq!(frames(&out), 288, "{name}");
        assert!((duration(&out) - 30.0).abs() < 0.2, "{name}: {}", duration(&out));
        assert_eq!(packets_md5(&out, "a:0"), packets_md5(&c.videos().join(name), "a:0"), "{name}");
        let report = c.report(stem);
        let notes = report["notes"].to_string();
        assert!(notes.contains("the picture stops at 0:00:12, sound or subtitles run on to 0:00:30"), "{name}: {notes}");
        assert!(report["verify_vmaf"].as_array().is_some_and(|v| v.len() == 2), "{name}: {report}");
    }
}

/// Settings slow enough to catch an encode in the middle.
const SLOW: [&str; 6] = ["--preset", "6", "--samples", "2", "--sample-secs", "2"];

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn scenes_get_their_own_crf_and_a_killed_run_goes_on_where_it_stopped() {
    let c = Case::new("resume");
    let src = c.scenes_clip("s.mp4");
    let args = || {
        let mut a: Vec<std::ffi::OsString> = vec!["encode".into(), "-o".into(), c.out().into(), "--scenes".into()];
        a.extend(SLOW.iter().map(Into::into));
        a.push(src.clone().into());
        a
    };
    let mut child = c.command().args(args()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    wait_until("the first finished chunk", 180, || entries(&c.out(), ".av1opt-work-").iter().any(|w| w.join("chunk00000.obu").exists()));
    // Killed outright, as by a power cut or the out-of-memory killer: its ffmpeg goes too.
    child.kill().unwrap();
    child.wait().unwrap();
    wait_until("ffmpeg to end with av1opt", 5, || ffmpegs_under(&c.dir).is_empty());
    assert!(!c.out().join("s.mp4").exists() && !c.out().join("s.av1opt.json").exists());
    // Touched meanwhile: its contents are what the chunks were made from.
    std::fs::File::options().write(true).open(&src).unwrap().set_modified(mtime(&src) + std::time::Duration::from_secs(3600)).unwrap();

    let r = c.av1opt(&args());
    r.ok().has("resuming earlier run: crf").has("another date than when its finished chunks were made").has("encoded 1, skipped 0, failed 0");
    let kept: u32 = r.err.split("resuming: ").nth(1).and_then(|t| t.split_whitespace().next()?.parse().ok()).unwrap_or_else(|| panic!("no chunk kept:\n{}", r.err));
    assert!((1..4).contains(&kept), "{kept} chunks kept");
    // The search was not run again.
    assert!(!r.err.contains("searching CRF"), "{}", r.err);

    let out = c.out().join("s.mp4");
    assert_eq!(frames(&out), 576);
    assert_in_step(&out, &src, "1");
    assert_eq!(packets_md5(&out, "0:a:0"), packets_md5(&src, "0:a:0"));
    let rep = c.report("s");
    assert_eq!(rep["mode"], "scenes");
    let chunks = rep["chunks"].as_array().unwrap();
    assert_eq!(chunks.len(), 4, "{rep}");
    // Four kinds of picture do not all want the same CRF, and each chunk met the target.
    let crfs: Vec<f64> = chunks.iter().map(|ch| ch["crf"].as_f64().unwrap()).collect();
    assert!(crfs.iter().cloned().fold(0.0, f64::max) - crfs.iter().cloned().fold(99.0, f64::min) >= 3.0, "{crfs:?}");
    for ch in chunks {
        assert!(ch["vmaf"].as_f64().unwrap() > 92.0, "{ch}");
    }
    assert_eq!(chunks.iter().map(|ch| ch["frames"].as_u64().unwrap()).sum::<u64>(), 576);
    assert_eq!(c.outputs(), ["av1opt-log.tsv", "s.av1opt.json", "s.mp4"]);
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn scenes_cut_by_the_frames_own_times_survive_a_kill_two_workers_matroska_and_fields() {
    let c = Case::new("own-times");
    // The four kinds of picture of `scenes_clip` on a clock that drifts: every 25th frame
    // stays a quarter longer, so no constant rate holds them all.
    c.ffmpeg(&[
        "-f", "lavfi", "-i", "testsrc2=s=640x360:r=24:d=6", "-f", "lavfi", "-i", "mandelbrot=s=640x360:r=24,trim=duration=6",
        "-f", "lavfi", "-i", "sierpinski=s=640x360:r=24:seed=1,trim=duration=6",
        "-f", "lavfi", "-i", "life=s=640x360:r=24:mold=10:ratio=0.1:seed=1:death_color=#C83232:life_color=#00ff00,trim=duration=6",
        "-f", "lavfi", "-i", "sine=f=440:d=24",
        "-filter_complex", "[0:v]format=yuv420p[a];[1:v]format=yuv420p[b];[2:v]format=yuv420p[c];[3:v]format=yuv420p[d];[a][b][c][d]concat=n=4:v=1:a=0,settb=1/90000,setpts='N*3750+floor(N/25)*900'[v]",
        "-map", "[v]", "-map", "4:a", "-fps_mode", "passthrough", "-enc_time_base", "1:90000", "-video_track_timescale", "90000",
        "-c:v", "libx264", "-preset", "ultrafast", "-crf", "16", "-c:a", "aac", "-b:a", "64k", "d.mp4",
    ]);
    let src = c.videos().join("d.mp4");
    let want = frame_times(&src);
    assert_eq!(want.len(), 576);
    let furthest = |out: &Path| {
        let got = frame_times(out);
        assert_eq!(got.len(), want.len(), "{}", out.display());
        got.iter().zip(&want).map(|(a, b)| (a - b).abs()).fold(0.0, f64::max)
    };

    // Two chunks at a time, killed after the first is done, then run again.
    let args = || {
        let mut a: Vec<std::ffi::OsString> = vec!["encode".into(), "-o".into(), c.out().into(), "--scenes".into(), "--workers".into(), "2".into()];
        a.extend(SLOW.iter().map(Into::into));
        a.push(src.clone().into());
        a
    };
    let mut child = c.command().args(args()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    wait_until("the first finished chunk", 180, || entries(&c.out(), ".av1opt-work-").iter().any(|w| w.join("chunk00000.obu").exists()));
    child.kill().unwrap();
    child.wait().unwrap();
    wait_until("ffmpeg to end with av1opt", 5, || ffmpegs_under(&c.dir).is_empty());
    assert!(!c.out().join("d.mp4").exists());
    let r = c.av1opt(&args());
    r.ok().has("resuming earlier run: crf").has("encoded 1, skipped 0, failed 0");
    assert!(!r.err.contains("searching CRF"), "{}", r.err);
    let out = c.out().join("d.mp4");
    let worst = furthest(&out);
    assert!(worst < 0.00002, "a frame is {worst:.6} s from its place");
    assert_in_step(&out, &src, "1");
    assert_eq!(packets_md5(&out, "0:a:0"), packets_md5(&src, "0:a:0"));
    let rep = c.report("d");
    assert_eq!(rep["mode"], "scenes");
    assert!(rep["notes"].to_string().contains("scenes are cut and joined by the source's own timestamps"), "{rep}");
    let chunks = rep["chunks"].as_array().unwrap();
    assert_eq!((chunks.len(), chunks.iter().map(|ch| ch["frames"].as_u64().unwrap()).sum::<u64>()), (4, 576), "{rep}");
    assert_eq!(video(&out)["field_order"], "progressive");
    assert_eq!(c.outputs(), ["av1opt-log.tsv", "d.av1opt.json", "d.mp4"]);

    // Matroska counts in milliseconds: every frame within half of one.
    let mkv = c.dir.join("mkv");
    c.encode_into(&mkv, &[&src], &["--scenes", "--container", "mkv"]).ok().has("encoded 1,");
    let worst = furthest(&mkv.join("d.mkv"));
    assert!(worst < 0.00051, "a frame is {worst:.6} s from its place");
    assert_in_step(&mkv.join("d.mkv"), &src, "1");

    // Fields woven into frames, on such a clock too: the deinterlaced frames keep the
    // times the woven ones had.
    c.ffmpeg(&[
        "-f", "lavfi", "-i", "testsrc2=s=352x288:r=50:d=6", "-f", "lavfi", "-i", "mandelbrot=s=352x288:r=50,trim=duration=6", "-f", "lavfi", "-i", "sine=f=440:d=12",
        "-filter_complex", "[0:v]format=yuv420p[a];[1:v]format=yuv420p[b];[a][b]concat=n=2:v=1:a=0,tinterlace=interleave_top,fieldorder=tff,settb=1/90000,setpts='N*3600+floor(N/25)*900'[v]",
        "-map", "[v]", "-map", "2:a", "-fps_mode", "passthrough", "-enc_time_base", "1:90000", "-video_track_timescale", "90000",
        "-c:v", "libx264", "-preset", "ultrafast", "-crf", "14", "-flags", "+ildct+ilme", "-x264opts", "tff=1", "-c:a", "aac", "-b:a", "64k", "fields.mp4",
    ]);
    let woven = c.videos().join("fields.mp4");
    assert_eq!(video(&woven)["field_order"], "tt");
    let il = c.dir.join("fields");
    c.encode_into(&il, &[&woven], &["--scenes"]).ok().has("shown twice): each keeps its own").has("encoded 1,");
    let (got, want) = (frame_times(&il.join("fields.mp4")), frame_times(&woven));
    assert_eq!((got.len(), want.len()), (300, 300));
    let worst = got.iter().zip(&want).map(|(a, b)| (a - b).abs()).fold(0.0, f64::max);
    assert!(worst < 0.00002, "a frame is {worst:.6} s from its place");
    assert_eq!(video(&il.join("fields.mp4"))["field_order"], "progressive");
    assert_eq!(c.report_in(&il, "fields")["mode"], "scenes");
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn scenes_are_encoded_apart_only_where_the_samples_disagree() {
    let c = Case::new("auto");
    let uneven = c.scenes_clip("uneven.mp4");
    let even = c.clip("even.mp4", "320x180", 12);
    let r = c.encode(&[&uneven, &even], &["--scenes=auto"]);
    r.ok().has("encoded 2, skipped 0, failed 0");
    let rep = c.report("uneven");
    assert_eq!(rep["mode"], "scenes", "{}", r.err);
    let notes = |rep: &Value| rep["notes"].as_array().unwrap().iter().map(|n| n.as_str().unwrap().to_string()).collect::<Vec<_>>().join("\n");
    assert!(notes(&rep).contains("quality differs along the video") && notes(&rep).contains("each scene gets its own CRF"), "{rep}");
    assert_in_step(&c.out().join("uneven.mp4"), &uneven, "1");
    let rep = c.report("even");
    assert_eq!(rep["mode"], "whole");
    assert!(!notes(&rep).contains("quality differs"), "{rep}");
    assert!(entries(&c.out(), ".av1opt-").is_empty());

    // Without the setting the same video gets one CRF, the one its mean asks for. The whole
    // encode is then scored against the source, and the stretches that came out below the
    // floor are encoded again by themselves and put in place: every frame where it was,
    // the sound untouched.
    let whole = c.dir.join("whole");
    let r = c.encode_into(&whole, &[&uneven], &[]);
    r.ok().has("verifying: the whole video against its source").has("encoding again:").has("whole video as kept:");
    assert!(!r.err.contains("looking for the CRF that keeps it"), "{}", r.err);
    let rep = c.report_in(&whole, "uneven");
    assert_eq!(rep["mode"], "whole");
    assert!(notes(&rep).contains("a CRF per scene (--scenes) would even it out") && notes(&rep).contains("encoded again at CRF"), "{rep}");
    // The video's CRF is the highest that reached the mean.
    let chosen = rep["crf"].as_f64().unwrap();
    let trials = rep["search"].as_array().unwrap();
    assert!(trials.iter().all(|t| t["crf"].as_f64().unwrap() <= chosen || t["mean_vmaf"].as_f64().unwrap() < 93.0), "{rep}");
    let w = &rep["whole"];
    let stretches = w["stretches"].as_array().unwrap();
    assert!(!stretches.is_empty(), "{rep}");
    for st in stretches {
        assert!(st["crf"].as_f64().unwrap() < chosen, "{rep}");
        assert!(st["vmaf_before"].as_f64().unwrap() < 89.0, "{rep}");
        assert!(st["vmaf"].as_f64().unwrap() >= 89.0 || st["out_of_reach"] == true, "{rep}");
    }
    assert!(w["lowest_vmaf"].as_f64().unwrap() > w["lowest_before"].as_f64().unwrap(), "{rep}");
    assert!(stretches.iter().any(|st| st["out_of_reach"] == true) || w["lowest_vmaf"].as_f64().unwrap() >= 89.0, "{rep}");
    assert_eq!(rep["mean_vmaf"], w["mean_vmaf"]);
    let out = whole.join("uneven.mp4");
    let (got, want) = (frame_times(&out), frame_times(&uneven));
    assert_eq!(got.len(), want.len());
    let worst = got.iter().zip(&want).map(|(a, b)| (a - b).abs()).fold(0.0, f64::max);
    assert!(worst < 0.00002, "a frame is {worst:.6} s from its place");
    assert_in_step(&out, &uneven, "1");
    assert_eq!(packets_md5(&out, "0:a:0"), packets_md5(&uneven, "0:a:0"));
    assert_eq!(video(&out)["field_order"], "progressive");
    assert_eq!(c.outputs_in(&whole), ["av1opt-log.tsv", "uneven.av1opt.json", "uneven.mp4"]);

    // An easy half and a hard one with every sixth frame left out, in a Matroska file whose
    // header names a constant rate all the same: that scenes can't be cut apart at it shows
    // only once they have been looked for. With frames that stay twice as long as others,
    // a chunk's score would not be what is seen either: one CRF it is, held down for the
    // worst sample too.
    // (Colour bars that stand still: a sample of them has to score 8 above one of the hard
    // half for the worst of the two to lie 4 below their mean.)
    let halves = [
        "-f", "lavfi", "-i", "smptebars=s=640x360:r=24:d=12",
        "-f", "lavfi", "-i", "life=s=640x360:r=24:mold=10:ratio=0.1:seed=1:death_color=#C83232:life_color=#00ff00,trim=duration=12",
        "-f", "lavfi", "-i", "sine=f=440:d=24",
    ];
    let x264 = ["-map", "[v]", "-map", "2:a", "-c:v", "libx264", "-preset", "ultrafast", "-crf", "12", "-c:a", "aac", "-b:a", "64k"];
    let joined = "[0:v]format=yuv420p[a];[1:v]format=yuv420p[b];[a][b]concat=n=2:v=1:a=0";
    c.ffmpeg(&[&halves[..], &["-filter_complex", &format!("{joined},select='mod(n,6)'[v]"), "-fps_mode", "vfr"], &x264[..], &["gaps.mkv"]].concat());
    let gaps = c.videos().join("gaps.mkv");
    let late = c.dir.join("late");
    let r = c.encode_into(&late, &[&gaps], &["--scenes=auto"]);
    // What verification then makes of it is not this test's matter: its spot check of the
    // hard half reads within a fraction of a point of the line.
    r.has("each scene gets its own CRF").has("frames come at uneven times").has("looking for the CRF that keeps it at 90 or above").has("to keep the worst sample at VMAF 90");
    assert!(entries(&late, ".av1opt-").is_empty());

    // The same two halves with every frame there, each 25th staying a quarter longer: the
    // clock of a camera that drifts. At any constant rate some frames would be dropped and
    // others shown twice. Each scene still gets its CRF, cut and joined by the frames' own
    // times: every one of them is where it was, to the tick, and the sound as it was.
    c.ffmpeg(&[
        &halves[..],
        &["-filter_complex", &format!("{joined},settb=1/90000,setpts='N*3750+floor(N/25)*900'[v]"), "-fps_mode", "passthrough", "-enc_time_base", "1:90000", "-video_track_timescale", "90000"],
        &x264[..],
        &["drift.mp4"],
    ].concat());
    let drift = c.videos().join("drift.mp4");
    let own = c.dir.join("own");
    let r = c.encode_into(&own, &[&drift], &["--scenes=auto"]);
    r.ok().has("each scene gets its own CRF").has("fps 2 of 576 would be dropped and 7 shown twice): each keeps its own");
    let rep = c.report_in(&own, "drift");
    assert_eq!(rep["mode"], "scenes", "{rep}");
    assert!(rep["notes"].to_string().contains("scenes are cut and joined by the source's own timestamps"), "{rep}");
    assert_eq!(rep["chunks"].as_array().unwrap().iter().map(|ch| ch["frames"].as_u64().unwrap()).sum::<u64>(), 576, "{rep}");
    let (got, want) = (frame_times(&own.join("drift.mp4")), frame_times(&drift));
    assert_eq!((got.len(), want.len()), (576, 576));
    let worst = got.iter().zip(&want).map(|(a, b)| (a - b).abs()).fold(0.0, f64::max);
    assert!(worst < 0.00002, "a frame is {worst:.6} s from its place");
    assert_in_step(&own.join("drift.mp4"), &drift, "1");
    assert_eq!(packets_md5(&own.join("drift.mp4"), "0:a:0"), packets_md5(&drift, "0:a:0"));
    assert!(rep["verify_vmaf"].as_array().unwrap().iter().all(|v| v.as_f64().unwrap() >= 89.0), "{rep}");
    assert!(entries(&own, ".av1opt-").is_empty());

    // Three hard seconds between the two samples (at 5.5 s and 16.5 s) and inside the spot
    // check at 13 s: the samples agree, the spot check fails, its stretch becomes a sample,
    // and the second encode at the CRF found with it stands.
    c.ffmpeg(&[
        "-f", "lavfi", "-i", "testsrc2=s=640x360:r=24:d=13",
        "-f", "lavfi", "-i", "life=s=640x360:r=24:mold=10:ratio=0.1:seed=1:death_color=#C83232:life_color=#00ff00,trim=duration=3",
        "-f", "lavfi", "-i", "testsrc2=s=640x360:r=24:d=8",
        "-filter_complex", "[0:v]format=yuv420p[a];[1:v]format=yuv420p[b];[2:v]format=yuv420p[c];[a][b][c]concat=n=3:v=1:a=0[v]",
        "-map", "[v]", "-c:v", "libx264", "-preset", "ultrafast", "-crf", "12", "hidden.mkv",
    ]);
    let again = c.dir.join("again");
    let r = c.encode_into(&again, &[&c.videos().join("hidden.mkv")], &["--scenes=auto"]);
    r.ok().has("encoded 1,").has("a stretch the samples missed: searching again with it as a sample");
    let rep = c.report_in(&again, "hidden");
    assert_eq!(rep["mode"], "whole");
    assert!(notes(&rep).contains("encoded a second time, after a spot check at"), "{rep}");
    assert!(rep["verify_vmaf"].as_array().unwrap().iter().all(|v| v.as_f64().unwrap() >= 89.0), "{rep}");
    // The CRF in the report is the one of the encode that was kept, and among its trials.
    let chosen = rep["crf"].as_f64().unwrap();
    let first: f64 = r.err.split("chose crf ").nth(1).and_then(|t| t.split_whitespace().next()?.parse().ok()).unwrap();
    assert!(chosen < first, "CRF {chosen} after {first}");
    assert_eq!(c.outputs_in(&again), ["av1opt-log.tsv", "hidden.av1opt.json", "hidden.mp4"]);
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn a_stopped_encode_leaves_nothing_half_done_and_its_trials_are_remembered() {
    let c = Case::new("stopped");
    let src = c.clip("s.mp4", "640x360", 24);
    let args = |extra: &[&str]| {
        let mut a: Vec<std::ffi::OsString> = vec!["encode".into(), "-o".into(), c.out().into()];
        a.extend(SLOW.iter().map(Into::into));
        a.extend(extra.iter().map(Into::into));
        a.push(src.clone().into());
        a
    };
    let mut child = c.command().args(args(&[])).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    let this_run = format!(".av1opt-tmp-{}-", child.id());
    wait_until("the encode to start writing", 180, || entries(&c.out(), &this_run).iter().any(|tmp| std::fs::metadata(tmp.join("partial.mp4")).is_ok_and(|m| m.len() > 0)));
    // Ctrl-C in a terminal reaches av1opt and ffmpeg alike; `kill` reaches av1opt only.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let status = child.wait().unwrap();
    assert!(!status.success());
    wait_until("ffmpeg to end with av1opt", 5, || ffmpegs_under(&c.dir).is_empty());
    assert!(c.outputs().iter().all(|n| n.starts_with(".av1opt-tmp-")), "{:?}", c.outputs());

    // The next run clears the leftovers away, reuses the trials and finishes.
    let r = c.av1opt(&args(&[]));
    r.ok().has("encoded 1, skipped 0, failed 0").has("(measured earlier)");
    assert!(!r.err.lines().any(|l| l.trim_start().starts_with("crf ") && !l.contains("measured earlier")), "a trial was measured again:\n{}", r.err);
    assert_eq!(c.outputs(), ["av1opt-log.tsv", "s.av1opt.json", "s.mp4"]);
    assert_eq!(frames(&c.out().join("s.mp4")), 576);
    assert_in_step(&c.out().join("s.mp4"), &src, "1");

    // Asked to, every trial is measured again; the answer is the same or next to it.
    let crf = c.report("s")["crf"].as_f64().unwrap();
    let r = c.av1opt(&args(&["--overwrite", "--fresh-search"]));
    r.ok().has("encoded 1, skipped 0, failed 0");
    assert!(!r.err.contains("measured earlier"), "{}", r.err);
    assert!((c.report("s")["crf"].as_f64().unwrap() - crf).abs() <= 1.0, "{} then {crf}", c.report("s")["crf"]);
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn an_output_takes_its_originals_place_and_gives_it_back() {
    let c = Case::new("replace");
    c.clip("plain.mp4", "320x180", 4);
    std::fs::create_dir_all(c.videos().join("films")).unwrap();
    c.ffmpeg(&["-i", "plain.mp4", "-c", "copy", "films/movie.mkv"]);
    std::fs::remove_file(c.videos().join("plain.mp4")).unwrap();
    let src = c.videos().join("films/movie.mkv");
    let original = std::fs::read(&src).unwrap();
    c.encode(&[&c.videos()], &[]).ok().has("encoded 1,");
    let out = c.out().join("films/movie.mp4");
    let encoded = std::fs::read(&out).unwrap();
    let trash = c.dir.join("data/Trash/files");
    let out_arg = c.out();
    let replace = |extra: &[&str]| {
        // The originals' folder is named too: a report is not followed out of the folders given.
        let mut a: Vec<std::ffi::OsString> = vec!["replace".into(), out_arg.clone().into(), c.videos().into()];
        a.extend(extra.iter().map(Into::into));
        c.av1opt(&a)
    };

    // Without --apply it only says what it would do.
    replace(&[]).ok();
    assert_eq!((std::fs::read(&src).unwrap(), std::fs::read(&out).unwrap()), (original.clone(), encoded.clone()));
    assert!(entries(&trash, "").is_empty());

    // Applied: the AV1 file sits where the original was, under its name with the new
    // ending; the original is in the trash, whole.
    replace(&["--apply"]).ok();
    let placed = c.videos().join("films/movie.mp4");
    assert_eq!(std::fs::read(&placed).unwrap(), encoded);
    assert!(!src.exists() && !out.exists());
    assert_eq!(std::fs::read(trash.join("movie.mkv")).unwrap(), original);
    assert_eq!(video(&placed)["codec_name"], "av1");
    // A second go has nothing left to do and changes nothing.
    replace(&["--apply"]);
    assert_eq!(std::fs::read(&placed).unwrap(), encoded);
    assert_eq!(entries(&trash, "").len(), 1);

    // And back: both files where they were, byte for byte, the trash empty.
    replace(&["--undo", "--apply"]).ok();
    assert_eq!((std::fs::read(&src).unwrap(), std::fs::read(&out).unwrap()), (original, encoded));
    assert!(!placed.exists());
    assert!(entries(&trash, "").is_empty(), "{:?}", entries(&trash, ""));
    // After that it can be replaced again.
    replace(&["--apply"]).ok();
    assert!(placed.exists() && !src.exists());
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn a_source_completed_after_its_encode_is_not_replaced_and_one_only_touched_is() {
    use std::os::unix::fs::FileExt;
    const MEGABYTE: usize = 1 << 20;
    let c = Case::new("hole");
    let src = c.clip("a.mp4", "320x180", 4);
    // A download that has allocated its file: what it has not got yet reads as zeros. Players
    // and ffmpeg stop at the end of the video, so the encode goes through as it would for a
    // file whose missing part they cannot tell from the rest.
    let mut original = std::fs::read(&src).unwrap();
    original.resize(original.len() + 8 * MEGABYTE, 0);
    std::fs::write(&src, &original).unwrap();
    c.encode(&[&src], &[]).ok().has("encoded 1,");
    assert!(c.report("a")["source_hash"].is_string(), "{}", c.report("a"));
    let encoded = std::fs::read(c.out().join("a.mp4")).unwrap();
    let trash = c.dir.join("data/Trash/files");
    let out_arg = c.out();
    let replace = |extra: &[&str]| {
        // The originals' folder is named too: a report is not followed out of the folders given.
        let mut a: Vec<std::ffi::OsString> = vec!["replace".into(), out_arg.clone().into(), c.videos().into()];
        a.extend(extra.iter().map(Into::into));
        c.av1opt(&a)
    };
    let later = mtime(&src) + std::time::Duration::from_secs(3600);

    // The rest arrives: same size, a new date, and none of it where the samples are taken (the
    // first and the last megabyte and the one in the middle).
    let hole = (original.len() - 3 * MEGABYTE) as u64;
    assert!(hole as usize > original.len() / 2 + MEGABYTE);
    let file = std::fs::File::options().write(true).open(&src).unwrap();
    file.write_all_at(&vec![0xab; MEGABYTE], hole).unwrap();
    file.set_modified(later).unwrap();
    let completed = std::fs::read(&src).unwrap();
    for extra in [&["--apply"][..], &["--apply", "--accept-changed-date"][..]] {
        replace(extra).ok().has("reading all of").has("the original changed since the encode");
        assert_eq!(std::fs::read(&src).unwrap(), completed);
        assert!(entries(&trash, "").is_empty(), "{:?}", entries(&trash, ""));
    }

    // The same bytes as were encoded under another date: only touched, or copied without its
    // dates. It is the file that was encoded.
    file.write_all_at(&vec![0; MEGABYTE], hole).unwrap();
    file.set_modified(later + std::time::Duration::from_secs(60)).unwrap();
    replace(&["--apply"]).ok().has("1 original");
    assert_eq!(std::fs::read(&src).unwrap(), encoded, "the output took its place");
    assert_eq!(std::fs::read(trash.join("a.mp4")).unwrap(), original);
}

/// Every file below `dir` with its size and modification time.
fn snapshot(dir: &Path) -> Vec<(PathBuf, u64, std::time::SystemTime)> {
    let mut all = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let m = e.metadata().unwrap();
            if m.is_dir() {
                todo.push(e.path());
            }
            all.push((e.path(), if m.is_dir() { 0 } else { m.len() }, m.modified().unwrap()));
        }
    }
    all.sort();
    all
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn scan_sorts_a_folder_into_candidates_and_touches_nothing() {
    let c = Case::new("scan");
    c.clip("big.mp4", "640x360", 12);
    std::fs::create_dir_all(c.videos().join("sub")).unwrap();
    c.ffmpeg(&["-i", "big.mp4", "-c:v", "libsvtav1", "-preset", "12", "-crf", "45", "-c:a", "copy", "sub/done.mp4"]);
    std::fs::write(c.videos().join("sub/broken.mkv"), vec![7u8; 30_000]).unwrap();
    std::fs::write(c.videos().join("notes.txt"), "not a video").unwrap();
    let before = snapshot(&c.videos());

    let csv = c.dir.join("scan.csv");
    let r = c.av1opt(&["scan".as_ref(), c.videos().as_os_str(), "--csv".as_ref(), csv.as_os_str()]);
    r.ok().has("1 candidates").has("skip: already AV1");
    assert_eq!(snapshot(&c.videos()), before);

    let text = std::fs::read_to_string(&csv).unwrap();
    let mut rows = text.lines();
    let header: Vec<&str> = rows.next().unwrap().split(',').collect();
    let col = |name: &str| header.iter().position(|h| *h == name).unwrap_or_else(|| panic!("no column {name}"));
    let rows: Vec<Vec<&str>> = rows.map(|l| l.split(',').collect()).collect();
    let row = |file: &str| rows.iter().find(|r| r[col("path")].ends_with(file)).unwrap_or_else(|| panic!("{file} is not in:\n{text}"));
    assert_eq!(rows.len(), 3, "{text}");
    let big = row("big.mp4");
    assert_eq!((big[col("verdict")], big[col("codec")], big[col("width")], big[col("audio_streams")]), ("candidate", "h264", "640", "1"));
    let (size, likely, low, high): (u64, u64, u64, u64) =
        (big[col("size_bytes")].parse().unwrap(), big[col("est_size_likely")].parse().unwrap(), big[col("est_size_low")].parse().unwrap(), big[col("est_size_high")].parse().unwrap());
    assert!(low <= likely && likely <= high && high < size, "{big:?}");
    assert_eq!(row("done.mp4")[col("verdict")], "skip: already AV1");
    // One line a spreadsheet can show, without the demuxer's detail or the path again.
    assert_eq!((row("broken.mkv")[col("verdict")], row("broken.mkv")[col("error")]), ("skip: unreadable", "ffprobe: Invalid data found when processing input"));

    // What to encode in the hours there are: the candidate, and nothing when it cannot fit.
    let list = c.dir.join("tonight.txt");
    let r = c.av1opt(&["scan".as_ref(), c.videos().as_os_str(), "--hours".as_ref(), "8".as_ref(), "--list".as_ref(), list.as_os_str()]);
    r.ok().has("== Most saved in 8.0 h of encoding ==").has("1 video, about");
    assert_eq!(std::fs::read_to_string(&list).unwrap(), format!("{}\n", c.videos().join("big.mp4").display()));
    let r = c.av1opt(&["scan".as_ref(), c.videos().as_os_str(), "--hours".as_ref(), "0.0001".as_ref(), "--list".as_ref(), list.as_os_str()]);
    r.ok().has("no candidate fits: the shortest takes about");
    assert_eq!(std::fs::read_to_string(&list).unwrap(), "");
    assert_eq!(snapshot(&c.videos()), before);
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn duplicates_are_grouped_with_the_copy_worth_keeping_first() {
    let c = Case::new("dups");
    std::fs::create_dir_all(c.videos().join("sub")).unwrap();
    let x264 = ["-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p"];
    let make = |args: &[&str], out: &str| {
        let mut a: Vec<&str> = args.to_vec();
        a.extend(x264);
        a.push(out);
        c.ffmpeg(&a);
    };
    // A minute each, so the frames compared reach well into the video.
    make(&["-f", "lavfi", "-i", "testsrc2=s=320x180:r=24:d=60", "-crf", "20"], "a.mp4");
    make(&["-i", "a.mp4", "-vf", "scale=160:90", "-crf", "30"], "sub/a-small.mkv");
    make(&["-i", "a.mp4", "-t", "30", "-crf", "22"], "sub/a-start.mp4");
    make(&["-ss", "25", "-i", "a.mp4", "-crf", "22"], "sub/a-end.mp4");
    make(&["-f", "lavfi", "-i", "mandelbrot=s=320x180:r=24,trim=duration=60", "-crf", "20"], "b.mp4");
    std::fs::copy(c.videos().join("b.mp4"), c.videos().join("sub/b-copy.mp4")).unwrap();
    make(&["-f", "lavfi", "-i", "sierpinski=s=320x180:r=24:seed=1,trim=duration=60", "-crf", "20"], "alone.mp4");
    let before = snapshot(&c.videos());

    let json = c.dir.join("dups.json");
    let run = || c.av1opt(&["dups".as_ref(), c.videos().as_os_str(), "--json".as_ref(), json.as_os_str()]);
    run().ok().has("2 groups of the same video: 2 spare copies").has("2 pairs sharing only a start or an ending");
    assert_eq!(snapshot(&c.videos()), before);
    let d: Value = serde_json::from_slice(&std::fs::read(&json).unwrap()).unwrap();
    let name = |i: &Value| d["files"][i.as_u64().unwrap() as usize]["path"].as_str().unwrap().rsplit("/videos/").next().unwrap().to_string();
    let mut groups: Vec<(String, Vec<String>, String)> = d["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| (g["kind"].as_str().unwrap().to_string(), g["members"].as_array().unwrap().iter().map(name).collect(), g["reason"].as_str().unwrap().to_string()))
        .collect();
    groups.sort();
    let g = |kind: &str, members: &[&str], reason: &str| (kind.to_string(), members.iter().map(|m| m.to_string()).collect::<Vec<_>>(), reason.to_string());
    assert_eq!(
        groups,
        [
            // The copy to keep comes first; cut copies are paired with it, not with a spare.
            g("same", &["a.mp4", "sub/a-small.mkv"], "highest resolution"),
            g("same", &["b.mp4", "sub/b-copy.mp4"], "identical files, first by name"),
            g("same_end", &["a.mp4", "sub/a-end.mp4"], "longest"),
            g("same_start", &["a.mp4", "sub/a-start.mp4"], "longest"),
        ]
    );
    assert!(d["files"].as_array().unwrap().iter().all(|f| !f["path"].as_str().unwrap().ends_with("alone.mp4")));
    assert_eq!(d["stats"]["fresh"], 7);
    // The second look reads no frame again.
    run().ok().has("0 fingerprinted now");
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn a_benchmark_ranks_settings_remembers_its_encodes_and_tunes_the_encoder() {
    let c = Case::new("bench");
    c.clip("a.mp4", "320x180", 12);
    c.ffmpeg(&["-f", "lavfi", "-i", "mandelbrot=s=320x180:r=24,trim=duration=12", "-c:v", "libx264", "-preset", "ultrafast", "-crf", "16", "-pix_fmt", "yuv420p", "b.mp4"]);
    let json = c.dir.join("bench.json");
    let before = snapshot(&c.videos());
    let run = |extra: &[&str]| {
        let mut a: Vec<std::ffi::OsString> = vec!["bench".into(), c.videos().into()];
        a.extend(["--presets", "8,10", "--params", "tune=0", "--samples", "1", "--sample-secs", "2", "--json"].iter().map(Into::into));
        a.push(json.clone().into());
        a.extend(extra.iter().map(Into::into));
        c.av1opt(&a)
    };
    run(&[]).ok().has("3 settings on 2 videos at VMAF 93");
    assert_eq!(snapshot(&c.videos()), before);
    let b: Value = serde_json::from_slice(&std::fs::read(&json).unwrap()).unwrap();
    // The default settings are measured too, as what the others are compared with.
    let configs: Vec<(u64, &str)> = b["configs"].as_array().unwrap().iter().map(|k| (k["preset"].as_u64().unwrap(), k["params"].as_str().unwrap())).collect();
    assert_eq!(configs, [(5, "tune=0"), (8, "tune=0"), (10, "tune=0")]);
    assert_eq!(b["videos"].as_array().unwrap().len(), 2);
    let rows = b["summaries"][0]["rows"].as_array().unwrap();
    assert_eq!((rows[0]["size"].as_f64(), rows[0]["time"].as_f64()), (Some(1.0), Some(1.0)));
    // A faster preset takes less time for the same quality, and every setting was measured.
    assert!(rows[2]["time"].as_f64().unwrap() < 1.0, "{rows:?}");
    assert!(rows.iter().all(|r| r["missing"] == 0), "{rows:?}");
    let fresh = b["fresh"].as_u64().unwrap();
    assert!(fresh >= 6 && b["cached"] == 0, "{fresh} fresh, {} cached", b["cached"]);

    // A second run measures nothing again, and --save makes `encode --tuned` use the pick.
    run(&["--save", "fastest"]).ok().has("from earlier runs").has("`av1opt encode --tuned` uses");
    let b: Value = serde_json::from_slice(&std::fs::read(&json).unwrap()).unwrap();
    assert_eq!((b["fresh"].as_u64(), b["cached"].as_u64()), (Some(0), Some(fresh)));
    let tuning_file = c.dir.join("data/av1opt/tuning.json");
    let tuning: Value = serde_json::from_slice(&std::fs::read(&tuning_file).unwrap()).unwrap();
    // A class the default settings win has no entry, and is encoded with the default.
    let pick = tuning["classes"]["sd"]["preset"].as_u64().unwrap_or(5);
    let encode = |c: &Case, extra: &str| {
        let out = c.dir.join(extra);
        let r = c.av1opt(&["encode".as_ref(), "-o".as_ref(), out.as_os_str(), "--tuned".as_ref(), "--samples".as_ref(), "2".as_ref(), "--sample-secs".as_ref(), "2".as_ref(), c.videos().join("a.mp4").as_os_str()]);
        let report = c.report_in(&out, "a");
        (r, report)
    };
    let (r, report) = encode(&c, "tuned");
    r.ok().has("encoded 1,");
    assert_eq!(report["preset"].as_u64(), Some(pick));

    // The default of an earlier version, saved as if it had won: it is passed over, and said so.
    std::fs::write(&tuning_file, r#"{"format":1,"classes":{"sd":{"preset":6,"params":"tune=0","size":1.0,"time":1.0,"videos":2,"target_vmaf":93.0,"made":1}}}"#).unwrap();
    let (r, report) = encode(&c, "stale");
    r.ok().has("encoded 1,").has("tuned settings for 720p and below are not used").has("av1opt bench");
    assert_eq!(report["preset"].as_u64(), Some(5));
}

impl Case {
    /// A program named ffmpeg that runs `body` in bash and then does what the real one does
    /// (`$real` in `body`): the way to see, or spoil, what av1opt asks of ffmpeg.
    fn stand_in_ffmpeg(&self, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = self.dir.join("bin/ffmpeg");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("#!/bin/bash\nreal='{}'\n{body}\nexec \"$real\" \"$@\"\n", ffmpeg_bin())).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }
}

fn is_av1_of(out: &Path, src: &Path) {
    assert_eq!(video(out)["codec_name"], "av1", "{}", out.display());
    assert_eq!(frames(out), frames(src), "{}", out.display());
}

fn made_from(report: &Value, src: &Path) {
    assert_eq!(report["input"].as_str().map(Path::new), Some(std::fs::canonicalize(src).unwrap().as_path()), "{report}");
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn a_folder_that_is_its_own_output_keeps_its_originals_and_encodes_the_rest() {
    let c = Case::new("ownfolder");
    let dir = c.videos();
    let (a_mkv, a_mp4, b_mkv) = (c.clip("a.mkv", "320x180", 4), c.clip("a.mp4", "320x180", 4), c.clip("b.mkv", "320x180", 4));
    let originals: Vec<(&PathBuf, Vec<u8>)> = [&a_mkv, &a_mp4, &b_mkv].map(|p| (p, std::fs::read(p).unwrap())).into();
    let (b_mp4, b_report) = (dir.join("b.mp4"), dir.join("b.av1opt.json"));
    // Both of the pair are turned away in every run, and no original has changed after it.
    let pair_refused = |r: &Run| {
        assert_eq!(r.code, Some(3), "{}", r.err);
        r.has(&format!("FAILED: its output would take the place of another input, {}", a_mp4.display()));
        r.has("FAILED: the output would be written over this same video, so nothing was written; choose another output folder");
        for (path, bytes) in &originals {
            assert!(std::fs::read(path).unwrap() == *bytes, "{} changed", path.display());
        }
    };

    for first_run in [&[][..], &["--overwrite"]] {
        let r = c.encode_into(&dir, &[&dir], first_run);
        pair_refused(&r);
        r.has("encoded 1, skipped 0, failed 2, already done 0");
        is_av1_of(&b_mp4, &b_mkv);
        made_from(&c.report_in(&dir, "b"), &b_mkv);
        let encoded = std::fs::read(&b_mp4).unwrap();

        // The encode now lies among the videos: its source is done, and it is no new work.
        let r = c.encode_into(&dir, &[&dir], &[]);
        pair_refused(&r);
        r.has("encoded 0, skipped 1, failed 2, already done 1").has(&format!("already done: {}", b_mp4.display()));
        r.has(&format!("skipped: {} is the encode of {} from an earlier run", b_mp4.display(), std::fs::canonicalize(&b_mkv).unwrap().display()));
        assert!(std::fs::read(&b_mp4).unwrap() == encoded, "the earlier encode changed");

        // Told to overwrite, the encode is still an input of this run and is not written over.
        let r = c.encode_into(&dir, &[&dir], &["--overwrite"]);
        pair_refused(&r);
        r.has("encoded 0, skipped 1, failed 3, already done 0").has(&format!("its output would take the place of another input, {}", b_mp4.display()));
        assert!(std::fs::read(&b_mp4).unwrap() == encoded, "the earlier encode changed");

        std::fs::remove_file(&b_mp4).unwrap();
        std::fs::remove_file(&b_report).unwrap();
    }
    assert!(entries(&dir, ".av1opt-").is_empty(), "{:?}", entries(&dir, ".av1opt-"));
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn two_inputs_with_one_output_name_leave_the_output_to_the_first_under_overwrite() {
    let c = Case::new("twins");
    let first = c.clip("e.mkv", "320x180", 4);
    // Another length than the first, so that the output shows whose it is.
    c.ffmpeg(&["-f", "lavfi", "-i", "testsrc2=s=320x180:r=25:d=5", "-f", "lavfi", "-i", "sine=f=440:d=5", "-c:v", "mpeg4", "-q:v", "3", "-c:a", "libmp3lame", "-b:a", "128k", "e.avi"]);
    let second = c.videos().join("e.avi");
    let before = (std::fs::read(&first).unwrap(), std::fs::read(&second).unwrap());

    let r = c.encode(&[&first, &second], &["--overwrite"]);
    assert_eq!(r.code, Some(3), "{}", r.err);
    r.has("encoded 1, skipped 0, failed 1, already done 0");
    r.has(&format!("FAILED: its output is also that of another input, {}; give one of them another output folder", first.display()));
    is_av1_of(&c.out().join("e.mp4"), &first);
    made_from(&c.report("e"), &first);
    assert_eq!(c.outputs(), ["av1opt-log.tsv", "e.av1opt.json", "e.mp4"]);
    assert!((std::fs::read(&first).unwrap(), std::fs::read(&second).unwrap()) == before, "a source changed");
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn an_output_whose_report_was_deleted_is_left_alone_as_not_verified() {
    let c = Case::new("noreport");
    let src = c.clip("a.mp4", "320x180", 4);
    c.encode(&[&src], &[]).ok().has("encoded 1, skipped 0, failed 0, already done 0");
    let out = c.out().join("a.mp4");
    let (bytes, made) = (std::fs::read(&out).unwrap(), mtime(&out));
    std::fs::remove_file(c.out().join("a.av1opt.json")).unwrap();

    let r = c.encode(&[&src], &[]);
    assert_eq!(r.code, Some(3), "{}", r.err);
    r.has("encoded 0, skipped 0, failed 0, already done 0, not verified 1").has(&format!("{} exists, not made by this run: not verified, left alone", out.display()));
    assert!(std::fs::read(&out).unwrap() == bytes && mtime(&out) == made, "the output was written again");
    assert_eq!(c.outputs(), ["a.mp4", "av1opt-log.tsv"]);
    // One encode in the history: the second run did not make another.
    let history = std::fs::read_to_string(c.dir.join("data/av1opt/history.jsonl")).unwrap();
    assert_eq!(history.lines().count(), 1, "{history}");
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn a_memory_cap_reaches_ffmpeg_as_written_and_a_bad_one_is_refused() {
    let c = Case::new("memcap");
    let src = c.clip("m.mp4", "320x180", 2);
    let seen = c.dir.join("cap");
    // av1opt's own calls start this way, and only they run under the cap. What the kernel
    // holds this process to is written down; nothing needs encoding to know that.
    let ffmpeg = c.stand_in_ffmpeg(&format!(
        "if [ \"$2\" = -nostdin ]; then cat \"/sys/fs/cgroup$(cut -d: -f3 /proc/self/cgroup)/memory.max\" > '{}'; exit 1; fi",
        seen.display()
    ));
    let ffmpeg = ffmpeg.to_str().unwrap();

    for bad in ["garbage", "0", "12X"] {
        let r = c.encode(&[&src], &["--ffmpeg", ffmpeg, "--memory-limit", bad]);
        assert_eq!(r.code, Some(2), "{bad}: {}", r.err);
        r.has("a memory cap is auto, none, infinity, or an amount such as 12G, 1.5G, 800M or 40%");
        assert!(!seen.exists(), "{bad} got as far as ffmpeg");
    }
    assert!(c.outputs().is_empty(), "{:?}", c.outputs());

    let memory_kb: f64 = std::fs::read_to_string("/proc/meminfo").unwrap().lines().find_map(|l| l.strip_prefix("MemTotal:")?.trim().strip_suffix("kB")?.trim().parse().ok()).unwrap();
    let share = |percent: f64| memory_kb * 1024.0 * percent / 100.0;
    const MIB: f64 = 1024.0 * 1024.0;
    let scenes: &[&str] = &["--scenes", "--workers", "2"];
    for (cap, extra, bytes) in [
        ("1.5G", &[][..], 1536.0 * MIB),
        ("800m", &[], 800.0 * MIB),
        ("40%", &[], share(40.0)),
        // Workers share the cap only as chunks run side by side; this first run is alone.
        ("1.5g", scenes, 1536.0 * MIB),
        ("50.5%", scenes, share(50.5)),
    ] {
        let _ = std::fs::remove_file(&seen);
        let mut args = vec!["--ffmpeg", ffmpeg, "--memory-limit", cap];
        args.extend(extra);
        let r = c.encode(&[&src], &args);
        assert!(!r.err.contains("a memory cap is"), "{cap}: {}", r.err);
        if r.err.contains("systemd-run --user unavailable") {
            continue;
        }
        // Not there: systemd-run turned the value down and ffmpeg never started.
        let held = std::fs::read_to_string(&seen).unwrap_or_else(|e| panic!("{cap} did not reach ffmpeg ({e}):\n{}", r.err));
        let held: f64 = held.trim().parse().unwrap_or_else(|_| panic!("{cap}: the cap in force is {held:?}"));
        assert!((held - bytes).abs() <= bytes * 0.001, "{cap}: held to {held} bytes, expected {bytes}");
    }
}

/// Image subtitles as Blu-ray discs carry them, the contents of a `.sup` file: a dot on the
/// screen for a second from each of `cues` (in seconds). ffmpeg can copy these but cannot
/// make them, so they are written out here.
fn image_subtitles(cues: &[u32]) -> Vec<u8> {
    const PICTURE: [u8; 5] = [1, 64, 0, 180, 0x10];
    const WINDOW: [u8; 10] = [1, 0, 0, 10, 0, 10, 0, 2, 0, 1];
    let mut out = Vec::new();
    let mut segment = |secs: u32, kind: u8, data: &[u8]| {
        let time = (secs * 90_000).to_be_bytes();
        out.extend_from_slice(b"PG");
        out.extend_from_slice(&time);
        out.extend_from_slice(&time);
        out.push(kind);
        out.extend_from_slice(&(data.len() as u16).to_be_bytes());
        out.extend_from_slice(data);
    };
    for (n, &at) in cues.iter().enumerate() {
        let shown = (2 * n as u16).to_be_bytes();
        segment(at, 0x16, &[&PICTURE[..], &shown, &[0x80, 0, 0, 1, 0, 0, 0, 0, 0, 10, 0, 10]].concat());
        segment(at, 0x17, &WINDOW);
        segment(at, 0x14, &[0, 0, 1, 235, 128, 128, 255]);
        segment(at, 0x15, &[0, 0, 0, 0xC0, 0, 0, 8, 0, 2, 0, 1, 1, 1, 0, 0]);
        segment(at, 0x80, &[]);
        let cleared = (2 * n as u16 + 1).to_be_bytes();
        segment(at + 1, 0x16, &[&PICTURE[..], &cleared, &[0, 0, 0, 0]].concat());
        segment(at + 1, 0x17, &WINDOW);
        segment(at + 1, 0x80, &[]);
    }
    out
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn a_long_track_that_is_not_kept_passes_the_length_check_and_a_cut_picture_does_not() {
    let c = Case::new("longtrack");
    // Six seconds of picture and sound, and a last subtitle a minute in: the container says
    // a minute, and MP4 takes no image subtitles.
    std::fs::write(c.videos().join("late.sup"), image_subtitles(&[1, 60])).unwrap();
    c.ffmpeg(&[
        "-f", "lavfi", "-i", "testsrc2=s=320x180:r=24:d=6", "-f", "lavfi", "-i", "sine=f=440:d=6", "-i", "late.sup", "-map", "0", "-map", "1", "-map", "2", "-c:v", "libx264", "-preset",
        "ultrafast", "-crf", "16", "-pix_fmt", "yuv420p", "-c:a", "aac", "-b:a", "64k", "-c:s", "copy", "long.mkv",
    ]);
    let src = c.videos().join("long.mkv");
    assert!(duration(&src) >= 59.0, "{}", duration(&src));

    c.encode(&[&src], &[]).ok().has("encoded 1, skipped 0, failed 0");
    let out = c.out().join("long.mp4");
    is_av1_of(&out, &src);
    assert!((duration(&out) - 6.0).abs() < 0.2, "{}", duration(&out));
    let p = probe(&out);
    assert!(streams(&p, "subtitle").is_empty());
    assert_eq!(packets_md5(&out, "a:0"), packets_md5(&src, "a:0"));
    let notes = c.report("long")["notes"].to_string();
    assert!(notes.contains("subtitle 0 (hdmv_pgs_subtitle) dropped"), "{notes}");

    // The same file with an encode that stops halfway: what a long container forgives is
    // not the picture's own length.
    let halfway = c.stand_in_ffmpeg("if [[ \"${@: -1}\" == *partial.mp4 ]]; then exec \"$real\" \"${@:1:$#-1}\" -t 3 \"${@: -1}\"; fi");
    let other = c.dir.join("out-halfway");
    let r = c.encode_into(&other, &[&src], &["--ffmpeg", halfway.to_str().unwrap()]);
    assert_eq!(r.code, Some(3), "{}", r.err);
    r.has("encoded 0, skipped 0, failed 1").has("the finished video lasts 3.0 seconds where it should last 6.0");
    assert_eq!(c.outputs_in(&other), ["av1opt-log.tsv"]);
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn a_report_set_aside_for_a_newer_encode_still_brings_the_original_back() {
    let c = Case::new("aside");
    let replace = |what: &Path, extra: &[&str]| {
        let mut a: Vec<std::ffi::OsString> = vec!["replace".into(), what.into(), "--anywhere".into()];
        a.extend(extra.iter().map(Into::into));
        c.av1opt(&a)
    };
    let read = |p: &Path| std::fs::read(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    let json = |p: &Path| serde_json::from_slice::<Value>(&read(p)).unwrap();
    let trash = c.dir.join("data/Trash/files");

    // An encode takes its original's place; the original is in the trash.
    let src = c.clip("x.mkv", "320x180", 4);
    let original = read(&src);
    c.encode(&[&src], &[]).ok().has("encoded 1,");
    let (out, report) = (c.out().join("x.mp4"), c.out().join("x.av1opt.json"));
    let first = read(&out);
    replace(&c.out(), &["--apply"]).ok();
    let placed = c.videos().join("x.mp4");
    assert!(read(&placed) == first && !src.exists() && !out.exists());
    assert!(read(&trash.join("x.mkv")) == original);

    // Another video of that name is encoded to the place the first came from.
    std::fs::create_dir(c.videos().join("later")).unwrap();
    let later = c.clip("later/x.mkv", "320x180", 5);
    c.encode(&[&later], &[]).ok().has("encoded 1,").has("was kept as");
    let (newer, newer_report) = (read(&out), read(&report));
    is_av1_of(&out, &later);
    made_from(&json(&report), &later);
    assert!(json(&report).get("replaced").is_none(), "{}", json(&report));

    // The first report is kept under a name that is still a report's.
    let found = entries(&c.out(), "x.replaced-");
    assert_eq!(found.len(), 1, "{found:?}");
    let aside = &found[0];
    let name = aside.file_name().unwrap().to_str().unwrap();
    let stem = name.strip_suffix(".av1opt.json").unwrap_or_else(|| panic!("{name}"));
    assert!(json(aside)["replaced"].is_object(), "{}", json(aside));
    made_from_gone(&json(aside), &src);
    replace(&c.out(), &["--undo"]).ok().has("1 original can go back to their place");

    // Undone from it: the original is back, the first encode beside its report, and the
    // newer encode and its report are as they were.
    replace(aside, &["--undo", "--apply"]).ok().has("1 original back in place");
    assert!(read(&src) == original, "the original came back changed");
    assert!(!placed.exists());
    assert!(read(&c.out().join(format!("{stem}.mp4"))) == first, "the first encode changed");
    assert!(read(&out) == newer && read(&report) == newer_report, "the newer encode was touched");
    assert!(entries(&trash, "").is_empty(), "{:?}", entries(&trash, ""));

    // The older encode no longer offers itself; the newer one still does, for its own source.
    assert_eq!(json(aside)["superseded"], true, "{}", json(aside));
    replace(aside, &["--apply"]).ok().has("a newer encode took the place of this one");
    assert!(read(&src) == original);
    replace(&report, &["--apply"]).ok().has("1 original replaced");
    assert!(read(&c.videos().join("later/x.mp4")) == newer && !later.exists());
    assert!(read(&src) == original, "the first original was touched");
}

/// A report's source by the path it had: that file has since gone to the trash.
fn made_from_gone(report: &Value, src: &Path) {
    let folder = std::fs::canonicalize(src.parent().unwrap()).unwrap();
    assert_eq!(report["input"].as_str().map(Path::new), Some(folder.join(src.file_name().unwrap()).as_path()), "{report}");
}

#[cfg(feature = "ui")]
mod queue {
    use super::*;
    use common::Server;
    use serde_json::json;
    use std::time::{Duration, Instant};

    fn head(s: &Server, id: u64) -> Value {
        let heads = s.get("/api/jobs/heads").json();
        heads["jobs"].as_array().unwrap().iter().find(|j| j["id"] == id).cloned().unwrap_or_else(|| panic!("job {id} is not in {heads}"))
    }

    fn status(s: &Server, id: u64) -> String {
        head(s, id)["status"].as_str().unwrap().to_string()
    }

    fn add(s: &Server, input: &Path, options: Value) -> u64 {
        let r = s.post("/api/jobs", &json!({ "inputs": [input], "roots": [], "out_dir": s.dir.join("out"), "options": options }));
        assert_eq!(r.status, 200, "{}", r.body);
        r.json()["id"].as_u64().unwrap()
    }

    #[test]
    #[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
    fn a_folder_queued_with_a_name_that_is_not_text_loses_only_that_file() {
        use std::os::unix::ffi::OsStrExt;
        let s = Server::start_with_tools("e2e-nottext", &ffmpeg_bin(), &ffprobe_bin(), |d| {
            small_clip(&d.join("videos/good.mp4"), &H264, 3600);
            std::fs::copy(d.join("videos/good.mp4"), d.join("videos").join(std::ffi::OsStr::from_bytes(b"w\xff.mp4"))).unwrap();
        });
        let id = add(&s, &s.dir.join("videos"), json!({ "preset": 12 }));
        let job = s.get(&format!("/api/jobs/{id}")).json();
        let files = job["files"].as_array().unwrap();
        assert_eq!(files.len(), 1, "{job}");
        assert!(files[0]["input"].as_str().unwrap().ends_with("/good.mp4"), "{job}");
        assert_eq!(s.post(&format!("/api/jobs/{id}/cancel"), &json!({})).status, 200);
        assert_eq!(s.get("/api/state").status, 200);
    }

    #[test]
    #[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
    fn a_paused_queue_holds_its_encode_in_place_and_leaves_the_wait_out_of_the_timings() {
        let s = Server::start_with_tools("e2e-pause", &ffmpeg_bin(), &ffprobe_bin(), |d| scenes_clip(&d.join("videos"), "s.mp4"));
        let src = s.dir.join("videos/s.mp4");
        let began = Instant::now();
        let id = add(&s, &src, json!({ "preset": 6, "scenes": true }));
        wait_until("the encode to run ffmpeg", 60, || status(&s, id) == "running" && !ffmpegs_under(&s.dir).is_empty());

        // Paused: every ffmpeg of the job is stopped, not ended, and stays so.
        assert_eq!(s.post("/api/queue/pause", &json!({ "paused": true })).status, 200);
        wait_until("ffmpeg to be held", 10, || {
            let f = ffmpegs_under(&s.dir);
            !f.is_empty() && f.iter().all(|(_, state)| *state == 'T')
        });
        let held = ffmpegs_under(&s.dir);
        let heads = s.get("/api/jobs/heads").json();
        assert_eq!(heads["queue"]["hold"]["why"], "paused", "{heads}");
        assert_eq!(head(&s, id)["paused"], true);
        std::thread::sleep(Duration::from_millis(2500));
        assert_eq!(ffmpegs_under(&s.dir), held, "a held ffmpeg ended or another started");
        assert_eq!(status(&s, id), "running");

        assert_eq!(s.post("/api/queue/pause", &json!({ "paused": false })).status, 200);
        wait_until("the encode to finish", 300, || status(&s, id) != "running");
        let wall = began.elapsed().as_secs_f64();
        let h = head(&s, id);
        assert_eq!((h["status"].as_str(), h["counts"]["ok"].as_u64(), h["error"].as_str()), (Some("done"), Some(1), None), "{h}");
        assert!(ffmpegs_under(&s.dir).is_empty());
        let out = s.dir.join("out/s.mp4");
        assert_eq!(frames(&out), 576);
        assert_in_step(&out, &src, "1");
        // The 2.5 s it was held are in neither timing.
        let r: Value = serde_json::from_slice(&std::fs::read(out.with_extension("av1opt.json")).unwrap()).unwrap();
        let worked = r["search_secs"].as_f64().unwrap() + r["encode_secs"].as_f64().unwrap();
        assert!(worked < wall - 2.0, "worked {worked:.1} s of {wall:.1} s on the clock");

        // The report is in History, and the output can take the original's place and give it back.
        let report = out.with_extension("av1opt.json");
        let state = |s: &Server| s.get("/api/history").json()["reports"][0]["replace"]["state"].as_str().unwrap_or("").to_string();
        assert_eq!(state(&s), "ready");
        let (original, encoded) = (std::fs::read(&src).unwrap(), std::fs::read(&out).unwrap());
        let r = s.post("/api/replace", &json!({ "report": report }));
        assert_eq!(r.status, 200, "{}", r.body);
        assert_eq!(std::fs::read(&src).unwrap(), encoded);
        assert_eq!(std::fs::read(s.dir.join("data/Trash/files/s.mp4")).unwrap(), original);
        assert_eq!(state(&s), "replaced");
        let r = s.post("/api/replace/undo", &json!({ "report": report }));
        assert_eq!(r.status, 200, "{}", r.body);
        assert_eq!((std::fs::read(&src).unwrap(), std::fs::read(&out).unwrap()), (original, encoded));
        assert_eq!(state(&s), "ready");
    }

    #[test]
    #[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
    fn cancelled_jobs_stop_at_once_come_back_when_asked_and_a_restart_goes_on() {
        let mut s = Server::start_with_tools("e2e-cancel", &ffmpeg_bin(), &ffprobe_bin(), |d| scenes_clip(&d.join("videos"), "s.mp4"));
        let src = s.dir.join("videos/s.mp4");
        let id = add(&s, &src, json!({ "preset": 6, "scenes": true }));
        wait_until("the encode to run ffmpeg", 60, || status(&s, id) == "running" && !ffmpegs_under(&s.dir).is_empty());
        assert_eq!(s.post(&format!("/api/jobs/{id}/cancel"), &json!({})).status, 200);
        wait_until("the job to stop", 20, || status(&s, id) == "cancelled");
        wait_until("ffmpeg to end", 5, || ffmpegs_under(&s.dir).is_empty());
        assert!(!s.dir.join("out/s.mp4").exists());

        // Cancelled while held: it ends all the same, and the pause stays on.
        let again = s.post(&format!("/api/jobs/{id}/again"), &json!({})).json()["id"].as_u64().unwrap();
        wait_until("the encode to run ffmpeg", 60, || status(&s, again) == "running" && !ffmpegs_under(&s.dir).is_empty());
        s.post("/api/queue/pause", &json!({ "paused": true }));
        wait_until("ffmpeg to be held", 10, || ffmpegs_under(&s.dir).iter().all(|(_, state)| *state == 'T') && !ffmpegs_under(&s.dir).is_empty());
        assert_eq!(s.post(&format!("/api/jobs/{again}/cancel"), &json!({})).status, 200);
        wait_until("the held job to stop", 20, || status(&s, again) == "cancelled");
        wait_until("ffmpeg to end", 5, || ffmpegs_under(&s.dir).is_empty());
        assert_eq!(s.get("/api/jobs/heads").json()["queue"]["paused"], true);

        // A paused queue starts nothing.
        let third = s.post(&format!("/api/jobs/{again}/again"), &json!({})).json()["id"].as_u64().unwrap();
        std::thread::sleep(Duration::from_millis(1500));
        assert_eq!(status(&s, third), "queued");
        assert!(ffmpegs_under(&s.dir).is_empty());
        s.post("/api/queue/pause", &json!({ "paused": false }));
        wait_until("chunks to be encoded", 180, || entries(&s.dir.join("out"), ".av1opt-work-").iter().any(|w| w.join("chunk00000.obu").exists()));

        // The server is stopped in the middle (the app is closed): ffmpeg ends with it, and
        // the next start takes the job up again, keeping the chunks that were done.
        assert!(s.terminate(), "server did not stop on SIGTERM");
        wait_until("ffmpeg to end with the server", 5, || ffmpegs_under(&s.dir).is_empty());
        let s2 = Server::start_in(&s.dir, &ffmpeg_bin(), &ffprobe_bin());
        let heads = s2.get("/api/jobs/heads").json();
        let id = heads["jobs"][0]["id"].as_u64().unwrap_or_else(|| panic!("no job after the restart: {heads}"));
        wait_until("the encode to finish", 300, || !matches!(status(&s2, id).as_str(), "queued" | "running"));
        let h = head(&s2, id);
        assert_eq!((h["status"].as_str(), h["counts"]["ok"].as_u64()), (Some("done"), Some(1)), "{h}");
        let out = s2.dir.join("out/s.mp4");
        assert_eq!(frames(&out), 576);
        assert_in_step(&out, &s2.dir.join("videos/s.mp4"), "1");
        assert!(entries(&s2.dir.join("out"), ".av1opt-").is_empty());
    }

    /// A short H.264 clip under `dir`, last written `age` seconds ago.
    fn small_clip(path: &Path, codec: &[&str], age: u64) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let o = Command::new(ffmpeg_bin())
            .args(["-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i", "testsrc2=s=320x240:r=24:d=6"])
            .args(codec)
            .arg(path)
            .output()
            .expect("run ffmpeg");
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        let f = std::fs::File::options().write(true).open(path).unwrap();
        f.set_modified(std::time::SystemTime::now() - Duration::from_secs(age)).unwrap();
    }

    const H264: [&str; 6] = ["-c:v", "libx264", "-preset", "ultrafast", "-crf", "16"];

    #[test]
    #[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
    fn a_watched_folder_queues_the_new_videos_worth_encoding_and_only_those() {
        let s = Server::start_with_tools("e2e-watch", &ffmpeg_bin(), &ffprobe_bin(), |d| small_clip(&d.join("videos/there.mp4"), &H264, 3600));
        let (dir, out) = (s.dir.join("videos"), s.dir.join("out"));
        let options = json!({ "preset": 10 });
        let r = s.post("/api/watch", &json!({ "path": dir, "out_dir": out, "options": options }));
        assert_eq!(r.status, 200, "{}", r.body);
        let id = r.json()["id"].as_u64().unwrap();
        let look = |s: &Server| {
            let r = s.post(&format!("/api/watch/{id}/look"), &json!({}));
            assert_eq!(r.status, 200, "{}", r.body);
            r.json()["watches"][0].clone()
        };
        let counts = |w: &Value| [&w["fresh"], &w["queued"], &w["left"], &w["before"]].map(|n| n.as_u64().unwrap());
        let jobs = |s: &Server| s.get("/api/jobs/heads").json()["jobs"].as_array().unwrap().clone();

        // The video that was there is not asked for, however often the folder is looked at.
        look(&s);
        let w = look(&s);
        assert_eq!((counts(&w), w["error"].as_str()), ([0, 0, 0, 1], None), "{w}");
        assert!(jobs(&s).is_empty());

        // Three turn up: one worth encoding, one in AV1 already, one still being written.
        small_clip(&dir.join("new/fresh.mp4"), &H264, 600);
        small_clip(&dir.join("new/done.mkv"), &["-c:v", "libsvtav1", "-preset", "12", "-crf", "40"], 600);
        small_clip(&dir.join("new/arriving.mp4"), &H264, 0);
        let w = look(&s);
        assert_eq!(counts(&w), [3, 0, 0, 1], "{w}");
        assert!(jobs(&s).is_empty(), "queued at first sight");
        let w = look(&s);
        assert_eq!((counts(&w), w["error"].as_str()), ([1, 1, 1, 1], None), "{w}");

        // One job, said to come from the watched folder, with the folder's settings.
        let all = jobs(&s);
        assert_eq!(all.len(), 1, "{all:?}");
        let job = all[0]["id"].as_u64().unwrap();
        let h = head(&s, job);
        assert_eq!(h["watch"].as_str(), std::fs::canonicalize(&dir).unwrap().to_str(), "{h}");
        assert_eq!(h["total"], 1, "{h}");
        wait_until("the encode to finish", 300, || !matches!(status(&s, job).as_str(), "queued" | "running"));
        let h = head(&s, job);
        assert_eq!((h["status"].as_str(), h["counts"]["ok"].as_u64(), h["error"].as_str()), (Some("done"), Some(1), None), "{h}");
        // The output keeps its place under the watched folder; originals are as they were.
        assert_eq!(frames(&out.join("new/fresh.mp4")), 144);
        let codec = |p: &Path| streams(&probe(p), "video")[0]["codec_name"].as_str().unwrap().to_string();
        assert_eq!((codec(&out.join("new/fresh.mp4")), codec(&dir.join("new/fresh.mp4"))), ("av1".into(), "h264".into()));
        assert!(!out.join("new/done.mkv").exists() && !out.join("there.mp4").exists());

        // A scan of the folder and the pick for a night: the two H.264 videos not yet in
        // AV1 that are long enough, best value first, and none when there is no time at all.
        let scan = s.post("/api/scans", &json!({ "paths": [dir] })).json()["id"].as_u64().unwrap();
        wait_until("the scan to end", 60, || s.get(&format!("/api/scans/{scan}")).json()["status"] == "done");
        let pick = |hours: f64| s.post(&format!("/api/scans/{scan}/pick"), &json!({ "hours": hours }));
        let p = pick(8.0).json();
        let names: Vec<&str> = p["paths"].as_array().unwrap().iter().map(|p| p.as_str().unwrap().rsplit('/').next().unwrap()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, ["arriving.mp4", "fresh.mp4", "there.mp4"], "{p}");
        assert!(p["secs"].as_f64().unwrap() > 0.0 && p["saved"].as_f64().unwrap() > 0.0, "{p}");
        assert_eq!(pick(1e-7).json()["paths"], json!([]));
        assert_eq!((pick(0.0).status, s.post("/api/scans/999/pick", &json!({ "hours": 8 })).status), (400, 404));

        // Looked at again, nothing is queued twice; the one being written is still waited for.
        let w = look(&s);
        assert_eq!(counts(&w), [1, 1, 1, 1], "{w}");
        assert_eq!(jobs(&s).len(), 1);
        // The folder a job was queued from by hand is not replaced by the watched folder's.
        let saved: Value = serde_json::from_slice(&std::fs::read(s.data("ui.json")).unwrap()).unwrap();
        assert!(saved["out_dir"].is_null(), "{saved}");
    }
}

/// Slide shows and time lapses: a clip of a second or two holds a frame or two, and scoring
/// every third frame then leaves clips without a score.
#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn videos_of_a_few_frames_a_second_are_measured_on_clips_that_hold_enough_of_them() {
    let c = Case::new("low-rate");
    let long = [("one-fps.mp4", "1", "40"), ("two-fps.mp4", "2", "30"), ("half-fps.mp4", "1/2", "60")];
    // Too short for more than one clip, which is then all there is to encode.
    let short = [("five-slides.mp4", "1", "5"), ("two-fps-short.mp4", "2", "4")];
    for (name, rate, secs) in long.iter().chain(&short) {
        let picture = format!("testsrc2=s=320x180:r={rate}:d={secs}");
        c.ffmpeg(&["-f", "lavfi", "-i", &picture, "-c:v", "libx264", "-preset", "ultrafast", "-crf", "16", "-pix_fmt", "yuv420p", name]);
    }
    // The commands are printed: they show how the clips were encoded.
    let encode = |sources: &[(&str, &str, &str)]| {
        let inputs: Vec<PathBuf> = sources.iter().map(|(name, _, _)| c.videos().join(name)).collect();
        let o = c.command().env("AV1OPT_DEBUG", "1").args(["encode", "--preset", "10", "--force", "-o"]).arg(c.out()).args(&inputs).output().unwrap();
        let log = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
        assert!(!log.contains("panicked"), "{log}");
        assert_eq!(o.status.code(), Some(0), "{log}");
        assert!(log.contains(&format!("encoded {}, skipped 0, failed 0", sources.len())), "{log}");
        for (name, _, _) in sources {
            let (src, out) = (c.videos().join(name), c.out().join(name));
            assert_eq!(frames(&out), frames(&src), "{name}");
            assert!((duration(&out) - duration(&src)).abs() < 2.1, "{name}: {} against {}", duration(&out), duration(&src));
        }
        log
    };
    let log = encode(&long);
    // Encoded end to end, not clip by clip.
    assert!(log.contains("samples.txt"), "{log}");
    assert!(!log.contains("sample0.crf"), "{log}");
    encode(&short);
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn wide_pixels_keep_their_shape_when_stretches_are_encoded_again() {
    let c = Case::new("shape-repair");
    c.scenes_clip("uneven.mp4");
    // 640x360 of 16:15 pixels, shown 256:135, as a DVD or an anamorphic camera makes them. The
    // stretches encoded again are joined from a bare stream, which has no shape of its own.
    c.ffmpeg(&["-i", "uneven.mp4", "-vf", "setsar=16/15", "-c:v", "libx264", "-preset", "ultrafast", "-crf", "16", "-c:a", "copy", "wide.mkv"]);
    let src = c.videos().join("wide.mkv");
    for container in ["mp4", "mkv"] {
        let out_dir = c.dir.join(format!("out-{container}"));
        c.encode_into(&out_dir, &[&src], &["--container", container]).ok().has("encoding again:");
        let v = video(&out_dir.join(format!("wide.{container}")));
        // ffmpeg's -aspect takes terms up to 255, so 256:135 comes back as 201:106.
        let ratio = |key: &str| v[key].as_str().and_then(|r| r.split_once(':')).map(|(n, d)| n.parse::<f64>().unwrap() / d.parse::<f64>().unwrap());
        let (pixel, shown) = (ratio("sample_aspect_ratio").unwrap_or(1.0), ratio("display_aspect_ratio").unwrap_or(0.0));
        assert!((pixel / (16.0 / 15.0) - 1.0).abs() < 0.001 && (shown / (256.0 / 135.0) - 1.0).abs() < 0.001, "{container}: {v}");
    }
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn sound_keeps_its_place_against_the_picture_when_stretches_are_encoded_again() {
    let c = Case::new("sync-repair");
    c.scenes_clip("uneven.mp4");
    // Transport streams, as television is recorded: one whose sound starts 0.3 s after its
    // picture, one whose picture starts 0.3 s after its sound. Joined again after repair, the
    // first lost the 0.3 s and the second could not be measured at all.
    c.ffmpeg(&["-i", "uneven.mp4", "-itsoffset", "0.3", "-i", "uneven.mp4", "-map", "0:v", "-map", "1:a", "-c", "copy", "late-sound.ts"]);
    c.ffmpeg(&["-itsoffset", "0.3", "-i", "uneven.mp4", "-i", "uneven.mp4", "-map", "0:v", "-map", "1:a", "-c", "copy", "late-picture.ts"]);
    for name in ["late-sound.ts", "late-picture.ts"] {
        let src = c.videos().join(name);
        for container in ["mp4", "mkv"] {
            let out_dir = c.dir.join(format!("out-{container}"));
            c.encode_into(&out_dir, &[&src], &["--container", container]).ok().has("encoding again:");
            let out = out_dir.join(name).with_extension(container);
            let (before, after) = (sound_lead(&src), sound_lead(&out));
            assert!((before - after).abs() < 1.0 / 24.0, "{name} to {container}: sound led by {before:.3} s, now {after:.3} s");
            assert_eq!(frames(&out), 576, "{name} to {container}");
            assert_in_step(&out, &src, "1");
        }
    }
}

/// What a player shows as a track's name: Matroska's title, MP4's name as ffmpeg reads it.
fn track_name(stream: &Value) -> Option<&str> {
    tag(stream, "title").or_else(|| tag(stream, "name"))
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn track_names_languages_and_marks_are_read_back_on_both_routes() {
    let c = Case::new("track-names");
    c.scenes_clip("uneven.mp4");
    std::fs::write(c.videos().join("signs.srt"), "1\n00:00:01,000 --> 00:00:02,500\nA sign\n").unwrap();
    // MP4 keeps a track's name in its user data, which ffmpeg reads as `name` and writes from
    // `title`: copied as tags, MP4 to MP4 lost every name.
    c.ffmpeg(&[
        "-i", "uneven.mp4", "-i", "signs.srt", "-map", "0", "-map", "1", "-c", "copy", "-c:s", "mov_text",
        "-metadata:s:v:0", "title=Main picture", "-metadata:s:v:0", "language=deu", "-metadata:s:a:0", "title=Commentary",
        "-metadata:s:a:0", "language=fra", "-metadata:s:s:0", "title=Signs", "-metadata:s:s:0", "language=eng", "named.mp4",
    ]);
    let src = c.videos().join("named.mp4");
    let p = probe(&src);
    assert_eq!(track_name(streams(&p, "audio")[0]), Some("Commentary"), "{p}");
    // Into MP4 and Matroska, after stretches were encoded again and the video joined from a
    // bare stream, which has no name, language or mark of its own.
    for container in ["mp4", "mkv"] {
        let out_dir = c.dir.join(format!("out-{container}"));
        c.encode_into(&out_dir, &[&src], &["--container", container]).ok().has("encoding again:");
        let p = probe(&out_dir.join(format!("named.{container}")));
        let (v, a, s) = (streams(&p, "video")[0], streams(&p, "audio")[0], streams(&p, "subtitle")[0]);
        for (stream, name, language) in [(v, "Main picture", "deu"), (a, "Commentary", "fra"), (s, "Signs", "eng")] {
            assert_eq!((track_name(stream), tag(stream, "language")), (Some(name), Some(language)), "{container}: {stream}");
        }
        assert_eq!(v["disposition"]["default"], 1, "{container}: {v}");
        if container == "mkv" {
            // Under its title, where players look, and not in a tag NAME as well.
            assert!(tag(a, "title").is_some() && a["tags"].get("NAME").is_none(), "{a}");
        }
    }
    // The one pass and the cut name them too, on a picture easy enough to need no repair.
    c.clip("easy.mp4", "320x180", 8);
    c.ffmpeg(&[
        "-i", "easy.mp4", "-c", "copy", "-metadata:s:v:0", "title=Main picture", "-metadata:s:v:0", "language=deu", "-metadata:s:a:0", "title=Commentary",
        "-metadata:s:a:0", "language=fra", "easy-named.mp4",
    ]);
    let src = c.videos().join("easy-named.mp4");
    for (folder, extra) in [("plain", None), ("cut", Some("6-end"))] {
        let out_dir = c.dir.join(folder);
        let mut args = vec!["--container", "mkv"];
        if let Some(range) = extra {
            args.extend(["--remove", range]);
        }
        c.encode_into(&out_dir, &[&src], &args).ok();
        let p = probe(&out_dir.join("easy-named.mkv"));
        let (v, a) = (streams(&p, "video")[0], streams(&p, "audio")[0]);
        for (stream, name, language) in [(v, "Main picture", "deu"), (a, "Commentary", "fra")] {
            assert_eq!((track_name(stream), tag(stream, "language")), (Some(name), Some(language)), "{folder}: {stream}");
        }
    }
}

#[test]
#[ignore = "needs ffmpeg with libsvtav1 and libvmaf"]
fn chapters_stay_in_place_after_a_cut_wherever_the_file_starts() {
    let c = Case::new("cut-chapters");
    c.clip("plain.mp4", "320x180", 10);
    // Chapters at 0-4, 4-7 and 7-10 s from the picture's start, written in the file's own
    // times, which for the second file start at 1.4 s (-output_ts_offset leaves chapters
    // where they are, so they are moved by hand).
    for (name, start) in [("zero.mkv", "0"), ("late.mkv", "1.4")] {
        let at = |secs: u32| secs * 1000 + if start == "0" { 0 } else { 1400 };
        let meta = format!(
            ";FFMETADATA1\n[CHAPTER]\nTIMEBASE=1/1000\nSTART={}\nEND={}\ntitle=One\n[CHAPTER]\nTIMEBASE=1/1000\nSTART={}\nEND={}\ntitle=Two\n[CHAPTER]\nTIMEBASE=1/1000\nSTART={}\nEND={}\ntitle=Three\n",
            at(0), at(4), at(4), at(7), at(7), at(10)
        );
        std::fs::write(c.videos().join("meta.txt"), meta).unwrap();
        c.ffmpeg(&["-i", "plain.mp4", "-f", "ffmetadata", "-i", "meta.txt", "-map", "0", "-map_chapters", "1", "-c", "copy", "-output_ts_offset", start, name]);
        let src = c.videos().join(name);
        let begins = probe(&src)["format"]["start_time"].as_str().and_then(|t| t.parse::<f64>().ok()).unwrap();
        assert!((begins - start.parse::<f64>().unwrap()).abs() < 0.01, "{name} starts at {begins}");
        let out_dir = c.dir.join(name.trim_end_matches(".mkv"));
        c.encode_into(&out_dir, &[&src], &["--container", "mkv", "--remove", "2-4"]).ok();
        let p = probe(&out_dir.join(name));
        let got: Vec<(f64, f64, String)> = p["chapters"]
            .as_array()
            .unwrap()
            .iter()
            .map(|ch| (ch["start_time"].as_str().unwrap().parse().unwrap(), ch["end_time"].as_str().unwrap().parse().unwrap(), tag(ch, "title").unwrap_or("").to_string()))
            .collect();
        let want = [(0.0, 2.0, "One"), (2.0, 5.0, "Two"), (5.0, 8.0, "Three")];
        assert_eq!(got.len(), want.len(), "{name}: {got:?}");
        for ((s, e, t), (ws, we, wt)) in got.iter().zip(want) {
            assert!((s - ws).abs() < 0.05 && (e - we).abs() < 0.05 && t == wt, "{name}: {got:?}");
        }
    }
}
