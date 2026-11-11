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
        let mut names: Vec<String> = std::fs::read_dir(self.out()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }
}

fn scenes_clip(dir: &Path, name: &str) {
    let o = Command::new(ffmpeg_bin())
        .current_dir(dir)
        .args(["-v", "error", "-nostdin", "-y"])
        .args(["-f", "lavfi", "-i", "testsrc2=s=640x360:r=24:d=6", "-f", "lavfi", "-i", "mandelbrot=s=640x360:r=24,trim=duration=6"])
        // Seeded: these two draw at random otherwise, and a test must see the same clip twice.
        .args(["-f", "lavfi", "-i", "sierpinski=s=640x360:r=24:seed=1,trim=duration=6"])
        .args(["-f", "lavfi", "-i", "life=s=640x360:r=24:mold=10:ratio=0.1:seed=1:death_color=#C83232:life_color=#00ff00,trim=duration=6"])
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
    assert_eq!(r.code, Some(2), "{}", r.err);
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
    assert_eq!(r.code, Some(2), "{}", r.err);
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
    assert_eq!(r.code, Some(2), "{}", r.err);
    r.has("encoded 1, skipped 0, failed 1").has("output name collision");
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

    let r = c.av1opt(&args());
    r.ok().has("resuming earlier run: crf").has("encoded 1, skipped 0, failed 0");
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

    // Without the setting the same video gets one CRF: one low enough for its worst sample,
    // which verification would otherwise have thrown the finished encode away for.
    let whole = c.dir.join("whole");
    let r = c.encode_into(&whole, &[&uneven], &[]);
    r.ok().has("looking for the CRF that keeps it at 89 or above");
    let rep = c.report_in(&whole, "uneven");
    assert_eq!(rep["mode"], "whole");
    assert!(notes(&rep).contains("a CRF per scene (--scenes) would even it out") && notes(&rep).contains("to keep the worst sample at VMAF 89"), "{rep}");
    let chosen = rep["crf"].as_f64().unwrap();
    let mean_only = rep["search"].as_array().unwrap().iter().filter(|t| t["mean_vmaf"].as_f64().unwrap() >= 93.0).map(|t| t["crf"].as_f64().unwrap()).fold(0.0, f64::max);
    assert!(chosen <= mean_only - 2.0, "CRF {chosen}, the mean alone allowed {mean_only}");
    assert!(rep["verify_vmaf"].as_array().unwrap().iter().all(|v| v.as_f64().unwrap() >= 89.0), "{rep}");
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
    let tmp = c.out().join(format!(".av1opt-tmp-{}", child.id()));
    wait_until("the encode to start writing", 180, || std::fs::metadata(tmp.join("partial.mp4")).is_ok_and(|m| m.len() > 0));
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
        let mut a: Vec<std::ffi::OsString> = vec!["replace".into(), out_arg.clone().into()];
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
    assert_eq!(configs, [(6, "tune=0"), (8, "tune=0"), (10, "tune=0")]);
    assert_eq!(b["videos"].as_array().unwrap().len(), 2);
    let rows = b["summaries"][0]["rows"].as_array().unwrap();
    assert_eq!((rows[0]["size"].as_f64(), rows[0]["time"].as_f64()), (Some(1.0), Some(1.0)));
    // A faster preset takes less time for the same quality, and every setting was measured.
    assert!(rows[2]["time"].as_f64().unwrap() < 1.0, "{rows:?}");
    assert!(rows.iter().all(|r| r["missing"] == 0), "{rows:?}");
    let fresh = b["fresh"].as_u64().unwrap();
    assert!(fresh >= 6 && b["cached"] == 0, "{fresh} fresh, {} cached", b["cached"]);

    // A second run measures nothing again, and --save makes `encode --tuned` use the pick.
    run(&["--save", "fastest"]).ok().has("from earlier runs").has("`av1opt encode --tuned` uses these");
    let b: Value = serde_json::from_slice(&std::fs::read(&json).unwrap()).unwrap();
    assert_eq!((b["fresh"].as_u64(), b["cached"].as_u64()), (Some(0), Some(fresh)));
    let tuning: Value = serde_json::from_slice(&std::fs::read(c.dir.join("data/av1opt/tuning.json")).unwrap()).unwrap();
    let pick = tuning["classes"]["sd"]["preset"].as_u64().unwrap();
    let r = c.av1opt(&["encode".as_ref(), "-o".as_ref(), c.out().as_os_str(), "--tuned".as_ref(), "--samples".as_ref(), "2".as_ref(), "--sample-secs".as_ref(), "2".as_ref(), c.videos().join("a.mp4").as_os_str()]);
    r.ok().has("encoded 1,");
    assert_eq!(c.report("a")["preset"].as_u64(), Some(pick));
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
