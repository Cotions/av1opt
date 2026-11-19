//! Target-quality CRF search: encode short sample clips at several CRFs and pick the
//! highest CRF (smallest file) whose mean VMAF still meets the target.

use crate::probe::MediaInfo;
use crate::tools::{Tools, path_arg};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub struct Sample {
    pub reference: PathBuf,
    pub secs: f64,
    /// Frames in the reference.
    pub frames: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Trial {
    /// Integer for real trials; the chosen trial may be interpolated to a quarter step.
    pub crf: f64,
    pub mean_vmaf: f64,
    pub min_vmaf: f64,
    /// Predicted video stream size relative to the source video stream.
    pub size_ratio: f64,
}

/// SVT-AV1 preset used when none is given. Against 6, at equal VMAF, 5 was 6.0%, 5.5% and
/// 4.9% smaller on three sets of clips (every clip, and by VMAF NEG, XPSNR and SSIMULACRA2
/// alike) for 28% more processor time in the encode; a real 15-minute video came to
/// 191 MB instead of 205 MB. Preset 4 is a further 3.3%, 4.4% and 3.4% smaller than 5 on
/// those sets (every clip, all four measures) for 22 to 29% more processor time in the
/// encode, about a fifth more for a whole run, since search, check and repairs cost the
/// same at either: the default since 2026-11-19, by the owner's choice of bytes over time.
/// The one clip at 60 frames a second gained only 1% from that step. Preset 3 takes 16% off
/// preset 6 (on one set of clips) for three times its time.
pub const DEFAULT_PRESET: u8 = 4;

/// SVT-AV1 parameters used when none are given. SVT-AV1's own default is tune=1 (PSNR).
pub const DEFAULT_PARAMS: &str = "tune=0";

#[derive(Clone)]
pub struct EncodeSettings {
    pub preset: u8,
    pub pix_fmt_out: &'static str,
    /// SVT-AV1 parameters as `key=value:key=value`, without the CRF.
    pub params: String,
    /// The key frame gap in frames, for a source whose frames do not come at the rate it
    /// declares (see [`keyint_frames`]); `None` leaves [`KEYINT`] to the encoder.
    pub keyint_frames: Option<u32>,
}

/// Key frames at most this far apart, unless the parameters set `keyint` themselves.
/// SVT-AV1's own default is about five seconds. Every key frame is paid for in bits, and at
/// ten seconds the same VMAF took 3.3% less on eight 10-second clips of real videos (2.6%
/// less on four of 40 seconds), with VMAF NEG, XPSNR and SSIMULACRA2 agreeing and the encode
/// no slower. Seeking lands on key frames: a player shows the frame asked for after decoding
/// from the one before it, up to ten seconds of video instead of five.
pub const KEYINT: &str = "10s";
/// [`KEYINT`] in seconds.
const KEYINT_SECS: f64 = 10.0;

/// [`KEYINT`] counted in frames of the rate the source's frames really come at, where that is
/// not the rate it declares (`vfr`: the average is more than 0.5% off the declared rate), and
/// `None` where it is, which leaves the encoder's arguments as they were. SVT-AV1 turns seconds
/// into frames by the rate it is given, and the one pass gives it the declared one: a 36 s
/// source declared at 48 fps whose frames come at 24 had its key frames at 0 and 20 s, so the
/// weak stretch the whole-video check found took all 36 s to encode again. The cuts the other
/// encodes are made from run at the real rate, and had theirs 10 s apart. Counted in frames
/// of that rate, the one pass's come at 0, 10, 20 and 30 s too, and 30 s were encoded again.
/// (SVT-AV1 makes a GOP of `10s` one frame longer than one given in frames: at 30 fps `10s`
/// put key frames at frames 0, 301 and 602, `300` at 0, 300 and 600.)
pub fn keyint_frames(info: &MediaInfo) -> Option<u32> {
    (info.vfr && info.fps > 0.0).then(|| (KEYINT_SECS * info.fps).round().max(1.0) as u32)
}

impl EncodeSettings {
    /// Settings for encoding `info`'s video: its key frame gap counted as [`keyint_frames`]
    /// says.
    pub fn for_source(info: &MediaInfo, preset: u8, params: &str) -> Self {
        EncodeSettings { preset, pix_fmt_out: "yuv420p10le", params: params.to_string(), keyint_frames: keyint_frames(info) }
    }

    /// The parameters the encoder is given besides the CRF: the set in use and what is added
    /// to every set unless it names the key itself: the key frame gap (see [`KEYINT`]), and
    /// quantization matrices (coarser steps for the fine detail an eye misses first; SVT-AV1
    /// leaves them off). With them the same VMAF took 1.1% less on eight clips, less on every
    /// one of them, with VMAF NEG and SSIMULACRA2 saying the same and XPSNR seeing no
    /// difference, for 0.5% more encoding time.
    pub fn full_params(&self) -> String {
        let sets = |key: &str| self.params.split(':').any(|p| p.split_once('=').is_some_and(|(k, _)| k.trim() == key));
        let own = (!self.params.is_empty()).then(|| self.params.clone());
        let keyint = self.keyint_frames.map_or(KEYINT.to_string(), |n| n.to_string());
        let implied = [("keyint", keyint), ("enable-qm", "1".to_string())].into_iter().filter(|(k, _)| !sets(k)).map(|(k, v)| format!("{k}={v}"));
        own.into_iter().chain(implied).collect::<Vec<_>>().join(":")
    }

    /// SVT-AV1 takes CRF in quarter steps through its own parameters (ffmpeg's -crf is integer).
    /// The CRF is written first: SVT-AV1 reads the list in order, and on reading `crf` it
    /// also sets `aq-mode` back to 2, so an `aq-mode` written before it would be dropped
    /// without a word.
    pub fn video_args(&self, crf: f64) -> Vec<String> {
        let params = format!("crf={}:{}", quarter(crf), self.full_params());
        // Stream specifiers keep these off copied cover-art streams.
        ["-c:v:0", "libsvtav1", "-preset:v:0", &self.preset.to_string(), "-pix_fmt:v:0", self.pix_fmt_out, "-svtav1-params:v:0", &params]
            .map(String::from)
            .to_vec()
    }
}

/// Checks and tidies a `key=value:key=value` list of SVT-AV1 parameters. Parameters the CRF
/// search sets itself are refused.
pub fn parse_params(s: &str) -> Result<String> {
    const OWNED: [&str; 9] = ["crf", "qp", "rc", "tbr", "mbr", "preset", "passes", "pass", "aq-mode-crf"];
    let mut out: Vec<String> = Vec::new();
    for part in s.split(':').map(str::trim).filter(|p| !p.is_empty()) {
        let Some((k, v)) = part.split_once('=').map(|(k, v)| (k.trim(), v.trim())) else { bail!("SVT-AV1 parameter '{part}' is not key=value") };
        let plain = |x: &str| !x.is_empty() && x.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
        if !plain(k) || !plain(v) {
            bail!("SVT-AV1 parameter '{part}': names and values are letters, digits, '-', '_' and '.'");
        }
        if OWNED.contains(&k) {
            bail!("SVT-AV1 parameter '{k}' is set by av1opt (CRF search and --preset)");
        }
        if out.iter().any(|o| o.split('=').next() == Some(k)) {
            bail!("SVT-AV1 parameter '{k}' is given twice");
        }
        out.push(format!("{k}={v}"));
    }
    Ok(out.join(":"))
}

/// Tries `params` on a few generated frames: ffmpeg only warns about names SVT-AV1 doesn't
/// know and carries on without them, which would quietly encode with other settings.
pub fn check_params(tools: &Tools, preset: u8, params: &str) -> Result<()> {
    let enc = EncodeSettings { preset: preset.max(8), pix_fmt_out: "yuv420p10le", params: params.to_string(), keyint_frames: None };
    let mut args: Vec<String> = ["-v", "warning", "-f", "lavfi", "-i", "testsrc2=s=256x144:r=24:d=0.5"].map(String::from).to_vec();
    args.extend(enc.video_args(35.0));
    args.extend(["-f", "null", "-"].map(String::from));
    let stderr = match tools.run(&args) {
        Ok(s) => s,
        Err(e) if crate::events::is_cancelled(&e) => return Err(e),
        Err(e) => {
            let text = format!("{e:#}");
            let why = text.lines().find(|l| l.contains("Svt[error]")).map(|l| l.split("Svt[error]:").last().unwrap_or(l).trim().to_string());
            bail!("SVT-AV1 refuses '{params}'{}", why.map_or(String::new(), |w| format!(": {w}")));
        }
    };
    if let Some(bad) = stderr.lines().find_map(|l| l.split("Error parsing option ").nth(1)) {
        bail!("SVT-AV1 doesn't know the parameter '{}' (in '{params}')", bad.split(':').next().unwrap_or(bad).trim());
    }
    Ok(())
}

/// Rounds down to SVT-AV1's quarter-step CRF grid.
pub fn quarter(crf: f64) -> f64 {
    (crf * 4.0).floor() / 4.0
}

/// The nearest step of SVT-AV1's quarter-step CRF grid.
pub fn nearest_quarter(crf: f64) -> f64 {
    (crf * 4.0).round() / 4.0
}

/// Lossless reference pixel format matching the source bit depth.
/// Codec of the lossless references (sample clips, scene chunks). Every trial reads each clip
/// twice, as encoder input and as VMAF reference, so what counts is how cheap it is to
/// decode: FFV1 took 40 ms of processor time per 1080p frame, a fifth of a whole trial;
/// this one takes 8 ms, for files twice the size (250 MB for 8 s of 1080p, 700 MB of 4K).
pub const REFERENCE_CODEC: &str = "ffvhuff";

pub fn reference_pix_fmt(info: &MediaInfo) -> &'static str {
    if info.pix_fmt.contains("10") || info.pix_fmt.contains("12") { "yuv420p10le" } else { "yuv420p" }
}

/// Whether the source's picture uses every value (0 to 255 in 8 bits) rather than broadcast
/// video's 16 to 235, by its tag. ffmpeg also takes the old `yuvj` formats as full range when
/// a file has no tag. A picture stored as RGB is made limited range by [`source_filters`].
pub fn full_range(info: &MediaInfo) -> bool {
    let untagged = matches!(info.color_range.as_str(), "" | "unknown");
    !is_rgb(info) && (info.color_range == "pc" || (untagged && info.pix_fmt.starts_with("yuvj")))
}

/// The output format of every lossless cut of this source: its bit depth in 4:2:0, and its
/// range. Left to choose, ffmpeg turned a full-range source into limited range in the cut
/// (luma mean 127.3 down to 125.3 on a phone-like clip), while the one pass kept it full: the
/// stretches encoded again from such cuts said limited range in their sequence headers, the
/// rest of the video full, and decoders that keep the first header's range for the whole file
/// showed a jump in brightness at every joint. With the range named, the cut holds the
/// source's very values (the decoded frames are byte for byte the same).
pub fn reference_format(info: &MediaInfo) -> Vec<&'static str> {
    let mut args = vec!["-pix_fmt", reference_pix_fmt(info)];
    if full_range(info) {
        args.extend(["-color_range", "pc"]);
    }
    args
}

pub fn vmaf_model(info: &MediaInfo) -> &'static str {
    if info.width.max(info.height) >= 3200 { "vmaf_4k_v0.6.1" } else { "vmaf_v0.6.1" }
}

/// Constant frame rate used for references, so frames line up for VMAF even on VFR sources.
pub fn cfr(info: &MediaInfo) -> String {
    const STD: [(f64, &str); 8] = [
        (23.976, "24000/1001"), (24.0, "24"), (25.0, "25"), (29.97, "30000/1001"),
        (30.0, "30"), (50.0, "50"), (59.94, "60000/1001"), (60.0, "60"),
    ];
    // A steady stream is on the grid it declares. Its average can sit elsewhere: a webcam's
    // 30 fps with drifting frame times averages 29.96, and a 30000/1001 grid would drop a
    // frame in every thousand of it.
    let fps = if !info.vfr && info.nominal_fps > 0.0 { info.nominal_fps } else { info.fps };
    // Closest standard rate, not the first within tolerance: 24 must not become 23.976,
    // which drops a frame every ~42 and misaligns VMAF comparisons.
    STD.iter()
        .min_by(|a, b| (a.0 - fps).abs().total_cmp(&(b.0 - fps).abs()))
        .filter(|(f, _)| (f - fps).abs() / f < 0.01)
        .map_or_else(|| format!("{fps:.3}"), |(_, s)| s.to_string())
}

/// Numeric value of [`cfr`], i.e. frames per second of the constant-rate stream.
/// That rate as people write it: "23.98", "30", "27.3".
pub fn cfr_label(info: &MediaInfo) -> String {
    let s = format!("{:.2}", cfr_value(info));
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

pub fn cfr_value(info: &MediaInfo) -> f64 {
    let r = cfr(info);
    match r.split_once('/') {
        Some((n, d)) => n.parse::<f64>().unwrap_or(0.0) / d.parse::<f64>().unwrap_or(1.0),
        None => r.parse().unwrap_or(info.fps),
    }
}

/// Filters every encode of this source needs: deinterlace, and crop to even dimensions
/// (4:2:0 AV1 can't have odd sizes). The same filters go into the references, so VMAF
/// compares like with like.
pub fn source_filters(info: &MediaInfo) -> Vec<String> {
    let mut f = Vec::new();
    if info.interlaced {
        f.push("bwdif=mode=send_frame".to_string());
    }
    if info.width % 2 == 1 || info.height % 2 == 1 {
        f.push(format!("crop={}:{}:0:0", info.width & !1, info.height & !1));
    }
    // RGB is turned into brightness and colour by a matrix, which ffmpeg takes as BT.601 when
    // it is not told one, and the encode then said none: a browser takes untagged HD as
    // BT.709 and showed it a little off. Named here, the matrix is what the encode is tagged
    // with, and the clips VMAF compares are made with the same one.
    if is_rgb(info) {
        let matrix = if info.color_primaries == "bt2020" { "bt2020" } else { "bt709" };
        f.push(format!("scale=out_color_matrix={matrix}:out_range=tv"));
    }
    // A full-range picture made 10-bit is spread over the range one way when it comes as
    // 4:2:0 and another when it comes as 4:2:2 or 4:4:4 (ffmpeg multiplies the second by 4,
    // the first by 1023/255): the one pass, made from the source, and a stretch encoded again
    // from a 4:2:0 cut differed by 1.45 in their 10-bit luma mean, on the same frames. Made
    // 4:2:0 first, as the cut is, both take the same way (the frames came out byte for byte
    // alike). Limited range takes the same way either way, and is left as it was.
    let cut_as = reference_pix_fmt(info);
    let same_layout = info.pix_fmt == cut_as || (info.pix_fmt == "yuvj420p" && cut_as == "yuv420p");
    if full_range(info) && !same_layout {
        f.push(format!("format={cut_as}"));
    }
    f
}

/// Whether the source's picture is stored as red, green and blue rather than as brightness
/// and colour.
pub fn is_rgb(info: &MediaInfo) -> bool {
    info.color_space == "gbr" || ["gbr", "rgb", "bgr", "argb", "abgr", "0rgb", "0bgr", "x2rgb", "x2bgr"].iter().any(|p| info.pix_fmt.starts_with(p))
}

/// Score every 3rd frame: ~45% less VMAF time. The mean of a trial's samples stays within
/// ~0.1 of scoring every frame (one 8 s sample by itself: within ~0.25). Must be odd:
/// SVT-AV1's hierarchical GOP alternates better and worse frames, so an even stride sees only
/// one kind (measured +0.2 VMAF with a stride of 2).
pub const VMAF_STRIDE: u32 = 3;

pub const PREROLL_SECS: f64 = 2.0;

/// How far before its first try a cut seeks when that try landed past the clip: more than the
/// distance between key frames of any ordinary video.
pub const SEEK_BACK_SECS: f64 = 30.0;

/// Moves a seek point to a quarter of a frame before the nearest frame. A seek that lands on
/// a frame (or half-way between two) is decided by the last digit of a timestamp, and
/// containers round those differently: Matroska keeps milliseconds, so the same frame is at
/// 2.2 s in an MP4 source and at 2.200 or 2.199 s in its MKV output. A quarter frame is
/// further from both edges than any such rounding.
/// `first_frame` is the time of the video's first frame after the start of the file.
pub fn snap_seek(t: f64, fps: f64, first_frame: f64) -> f64 {
    if !(fps > 0.0 && t > 0.0) {
        return t.max(0.0);
    }
    let frame = ((t - first_frame) * fps + 0.25).round();
    (first_frame + (frame - 0.25) / fps).max(0.0)
}

/// Where a clip lies in its file, in the file's time (see [`clip_place`]).
#[derive(Debug, PartialEq)]
struct ClipPlace {
    /// Where decoding starts, a run-in before the clip. None where the clip is so near the
    /// first frame that decoding from the start costs no more.
    seek: Option<f64>,
    /// A quarter of a frame before the clip's first frame: the first frame from here on is
    /// the clip's, whatever the last digit of its timestamp (see [`snap_seek`]).
    from: f64,
}

/// The place of the clip at `start` seconds of a file whose frames come `rate` a second, the
/// first of them at `first_frame`. It depends on nothing but these, so that a file and its
/// encode, and a cut that seeks and one that decodes from the start, take the same frames.
fn clip_place(start: f64, rate: f64, first_frame: f64) -> ClipPlace {
    let into_picture = (start - first_frame).max(0.0);
    if rate.is_nan() || rate <= 0.0 {
        return ClipPlace { seek: (into_picture > PREROLL_SECS).then_some(start - PREROLL_SECS), from: start };
    }
    if into_picture > PREROLL_SECS {
        let seek = snap_seek(start - PREROLL_SECS, rate, first_frame);
        let run_in = (PREROLL_SECS * rate).round() / rate;
        // (A seek moved to the very start has lost its quarter of a frame.)
        let from = if seek > 0.0 { seek + run_in } else { first_frame + run_in - 0.25 / rate };
        ClipPlace { seek: Some(seek).filter(|s| *s > 0.0), from }
    } else {
        ClipPlace { seek: None, from: first_frame + ((into_picture * rate).round() - 0.25) / rate }
    }
}

/// Lossless, constant-frame-rate cut of `input` starting at `start` seconds (moved by less
/// than a frame, see [`clip_place`]). Returns how many frames it holds, which is more than
/// none. `is_source` applies [`source_filters`]; pass false when cutting from an
/// already-encoded output. `first_frame` is the time of this file's first video frame after
/// the start of the file, and `file_start` the timestamp that start has.
#[allow(clippy::too_many_arguments)]
pub fn extract_reference(
    tools: &Tools,
    input: &Path,
    info: &MediaInfo,
    is_source: bool,
    first_frame: f64,
    file_start: f64,
    start: f64,
    secs: f64,
    out: &Path,
) -> Result<u64> {
    let place = clip_place(start, cfr_value(info), first_frame);
    let cut = |seek: Option<f64>| -> Result<u64> {
        let mut vf = if is_source { source_filters(info) } else { Vec::new() };
        // The frames are chosen by the file's own timestamps (`-copyts`): where the decoder
        // happens to start must not move the clip. Nor what else the file holds: counting from
        // the start, ffmpeg starts an MPEG-TS at the first frame of the streams it reads, here
        // the picture alone, so a picture 0.3 s behind the sound was cut 0.3 s late and a
        // stretch put back after repair scored VMAF 2.3 against it. Decoding has to start
        // before the clip: a seek can land past the place it was sent to (MPEG-TS, no index,
        // key frames far apart), and then the first frame seen is not the clip's first. Such a
        // cut keeps nothing, and is made again from further back.
        let from = place.from + file_start;
        vf.push(match seek {
            Some(_) => format!("select='gte(t,{from:.6})*if(eq(n,0),st(0,lt(t,{from:.6})),ld(0))'"),
            None => format!("select='gte(t,{from:.6})'"),
        });
        // Time counts from the first frame kept before frames are put on the constant-rate
        // grid: every frame is then at a whole number of frame times, give or take the
        // container's rounding, and lands in its own slot. Counted from the seek point instead,
        // a cut half-way between two frames puts every frame on the edge between two slots,
        // where millisecond rounding decides: an MKV output then came out with every third
        // frame doubled and the next one dropped, while its MP4 source did not (seen as a spot
        // check of VMAF 70).
        vf.push("setpts=PTS-STARTPTS".into());
        vf.push(format!("fps={}", cfr(info)));
        let mut args: Vec<String> = vec!["-y".into(), "-v".into(), "error".into(), "-copyts".into()];
        if let Some(seek) = seek {
            args.extend(["-ss".into(), format!("{seek:.6}")]);
        }
        args.extend(
            [
                "-i", &path_arg(input),
                "-t", &format!("{secs:.3}"),
                "-map", &format!("0:{}", if is_source { info.video_index } else { 0 }), "-an", "-sn",
                "-vf", &vf.join(","), "-c:v", REFERENCE_CODEC,
            ]
            .map(String::from),
        );
        args.extend(reference_format(info).into_iter().map(String::from));
        args.push(path_arg(out));
        tools.run(&args)?;
        // ffmpeg ends without complaint when it has nothing to write.
        tools.count_frames(out)
    };
    // The nearest place first: a run-in is all most files need. Decoding from the start finds
    // the clip in any file, at the price of the whole way there.
    let further_back = place.seek.map(|s| s - SEEK_BACK_SECS).filter(|s| *s > 0.0);
    for seek in [place.seek, further_back].into_iter().flatten().map(Some).chain([None]) {
        let frames = cut(seek)?;
        if frames > 0 {
            return Ok(frames);
        }
    }
    bail!("no picture at {} in {}: the video ends earlier than it says, or is damaged", crate::tools::hms(start), input.display())
}

/// When a file's first video frame is shown, counted from the start of the file (other
/// streams can start earlier).
pub fn first_frame(info: &MediaInfo) -> f64 {
    info.picture_start()
}

/// Clips placed along the picture, as (start, length) in the file's time, which is what
/// [`extract_reference`] takes: where sound comes first, the picture starts later than the file.
pub fn in_file_time(info: &MediaInfo, along_picture: Vec<(f64, f64)>) -> Vec<(f64, f64)> {
    let first = first_frame(info);
    along_picture.into_iter().map(|(start, secs)| (start + first, secs)).collect()
}

/// How long a sample clip is unless `--sample-secs` says otherwise: about what a spot check
/// looks at, so that a sample can score as low as a spot check of the same stretch will.
pub const SAMPLE_SECS: f64 = 4.0;

/// How many sample clips a video of this length gets unless `--samples` says otherwise:
/// one for about every two minutes, so that two minutes that are harder than the rest
/// can't lie between samples, with 8 at least and 32 at most.
pub fn sample_count(duration: f64) -> usize {
    ((duration / 120.0).round() as usize).clamp(8, 32)
}

/// Up to `count` clips of `secs` seconds, at least their own length apart, each in the middle
/// of its share of the whole video. Returns (start, length) pairs.
fn spread_clips(duration: f64, count: usize, secs: f64) -> Vec<(f64, f64)> {
    if duration <= 0.0 {
        return Vec::new();
    }
    let secs = secs.min(duration);
    let n = ((duration / (secs * 2.0)) as usize).clamp(1, count.max(1));
    let span = duration - secs;
    (0..n).map(|i| (span * (i as f64 + 0.5) / n as f64, secs)).collect()
}

/// Sample start times: up to `count` clips of `secs` seconds, spread over the whole video.
/// The start and the end count like the rest: leaving 5% out at each end read 0.13 to 0.37
/// VMAF high on the three videos it was measured on.
pub fn sample_positions(duration: f64, count: usize, secs: f64) -> Vec<(f64, f64)> {
    spread_clips(duration, count, secs)
}

/// How long a sample clip is when the search only has to find the video's mean, which is
/// when the whole video is checked after its encode (see [`crate::repair`]): the weak
/// stretches are found there, so a sample need not be long enough to score like one.
/// Measured on three 15-minute videos (2026-11-18): a sample encoded among others scores
/// within 0.2 VMAF of the same frames in the encode of the whole video, at 1 s as at 4 s.
/// What the samples' mean misses of the video's is where they lie, not how they are encoded:
/// 8 clips of 4 s were off by 2.4 VMAF on one video (1.3 on average over where the grid can
/// lie), 64 clips of 1 s by 0.3. So the same seconds go to many short clips.
pub const MEAN_SAMPLE_SECS: f64 = 1.0;

/// The fewest frames a sample clip is cut to hold. Every [`VMAF_STRIDE`]-th frame is scored, so
/// a clip under that many frames can end up without a score, and its trial is then encoded
/// clip by clip. Twice the stride gives each clip two scores.
pub const MIN_SAMPLE_FRAMES: f64 = 2.0 * VMAF_STRIDE as f64;

/// How long a sample clip wanted at `secs` seconds has to be in this video: as asked, unless
/// the frame rate is so low (slide shows, time lapses) that it would hold too few frames.
pub fn clip_secs(info: &MediaInfo, secs: f64) -> f64 {
    let rate = cfr_value(info);
    if rate > 0.0 { secs.max(MIN_SAMPLE_FRAMES / rate) } else { secs }
}

/// A video shorter than this is sampled whole: it costs no more seconds of encoding than the
/// 64 clips of a long video, and leaves nothing for the clips to miss.
const SAMPLED_WHOLE_BELOW: f64 = 64.0;

/// How many sample clips a search for the mean alone takes: one for about every 8 seconds
/// of video, 8 at least and 64 at most; for a video shorter than [`SAMPLED_WHOLE_BELOW`], as
/// many as fill it.
pub fn mean_sample_count(duration: f64) -> usize {
    if duration < SAMPLED_WHOLE_BELOW {
        return ((duration / MEAN_SAMPLE_SECS) as usize).max(1);
    }
    ((duration / 8.0).round() as usize).clamp(8, 64)
}

/// Sample start times for a search that only needs the video's mean: `count` clips of `secs`
/// seconds, each in the middle of its share of the whole video. The start and the end are
/// part of the mean like the rest (leaving 5% out at each end read 0.24 VMAF high on a
/// video whose first minute was its hardest). Clips that can fill the video do: they lie
/// side by side, a little longer than `secs` so that nothing is left over. They stay
/// separate clips because weak ones are counted one by one (see [`lifted_mean`]).
pub fn mean_sample_positions(duration: f64, count: usize, secs: f64) -> Vec<(f64, f64)> {
    if duration <= 0.0 {
        return Vec::new();
    }
    let fitting = ((duration / secs.min(duration)) as usize).max(1);
    if count >= fitting {
        let share = duration / fitting as f64;
        return (0..fitting).map(|i| (i as f64 * share, share)).collect();
    }
    spread_clips(duration, count, secs)
}

/// Cuts the sample clips at `pos` ((start, length) pairs in source seconds), a few at a time:
/// a cut is a seek, a second or so of decoding and a small lossless encode, and a search for
/// the mean wants up to 64 of them.
pub fn extract_samples(tools: &Tools, input: &Path, info: &MediaInfo, tmp: &Path, pos: Vec<(f64, f64)>) -> Result<Vec<Sample>> {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let n = pos.len();
    let workers = (std::thread::available_parallelism().map_or(4, |c| c.get()) / 3).clamp(1, 4);
    let (next, failed) = (AtomicUsize::new(0), AtomicBool::new(false));
    let done: Vec<std::sync::Mutex<Option<Result<Sample>>>> = (0..n).map(|_| std::sync::Mutex::new(None)).collect();
    let first = first_frame(info);
    std::thread::scope(|s| {
        for _ in 0..workers.min(n) {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    if i >= n || failed.load(Ordering::SeqCst) {
                        break;
                    }
                    let (start, secs) = pos[i];
                    let reference = tmp.join(format!("sample{i}.ref.mkv"));
                    eprintln!("  sample {}/{n} at {} ({secs:.0}s)", i + 1, crate::tools::hms(start));
                    tools.emit(crate::events::Event::Phase { phase: "sampling".into(), detail: format!("sample {}/{n} at {}", i + 1, crate::tools::hms(start)) });
                    let cut = extract_reference(tools, input, info, true, first, info.start_time, start, secs, &reference).map(|frames| Sample { reference, secs, frames });
                    if cut.is_err() {
                        failed.store(true, Ordering::SeqCst);
                    }
                    *done[i].lock().unwrap_or_else(|e| e.into_inner()) = Some(cut);
                }
            });
        }
    });
    // In order, and the first failure as it is; clips never started after one are not missed.
    let mut samples = Vec::with_capacity(n);
    for slot in done {
        match slot.into_inner().unwrap_or_else(|e| e.into_inner()) {
            Some(Ok(sample)) => samples.push(sample),
            Some(Err(e)) => return Err(e),
            None => {}
        }
    }
    if samples.len() != n {
        bail!("not every sample clip could be cut from the video, so it was not measured and is left as it was; AV1OPT_DEBUG=1 prints the ffmpeg commands");
    }
    Ok(samples)
}

/// CRF trials measured in earlier runs, kept in `$XDG_CACHE_HOME/av1opt/trials-v2.jsonl`: an
/// encode that was stopped, failed after its search, or is run again with another target or
/// container doesn't measure the same CRFs twice. A trial counts only for the same file
/// (path, size, date), the same sample clips and the same ffmpeg build.
pub struct TrialCache {
    path: Option<PathBuf>,
    /// What the trials were measured on, as one string.
    file: String,
    /// By encoder settings (see [`settings_key`]) and CRF in quarter steps.
    known: std::collections::HashMap<(String, u32), Trial>,
}

/// Everything about an encode besides the CRF, as the cache names it.
fn settings_key(preset: u8, pix_fmt: &str, params: &str) -> String {
    format!("{preset}|{pix_fmt}|{params}")
}

#[derive(Serialize, Deserialize)]
struct CacheLine {
    file: String,
    preset: u8,
    pix_fmt: String,
    params: String,
    time: u64,
    #[serde(flatten)]
    trial: Trial,
}

/// The cache is tidied once it holds more lines than this: the newest half stays.
const CACHE_MAX_LINES: usize = 40_000;
/// Version of the way a trial is measured, part of every cache key. Raise it when the same
/// file, clips and settings would now give another score or size: other clips cut from the
/// positions, another way of scoring or averaging them. What was measured before then is
/// measured again.
const MEASURING_VERSION: u32 = 1;

impl TrialCache {
    /// A cache that remembers nothing and writes nothing.
    pub fn off() -> Self {
        TrialCache { path: None, file: String::new(), known: Default::default() }
    }

    /// Opens the cache for one video and its sample positions. `reuse` false starts empty
    /// (trials are measured again) but still records what is measured. `lift_to` is the mark
    /// of [`lifted_mean`]: a mean counted that way is another number, kept apart.
    /// `full_range` is [`full_range`] of the source: its clips were cut in limited range
    /// before (see [`reference_format`]) and scored otherwise, so what was measured on them
    /// then is kept apart from what is measured now. A mark of their own leaves every
    /// limited-range source's trials as they were, which a new [`MEASURING_VERSION`] would
    /// have thrown away with them.
    pub fn open(input: &Path, size: u64, positions: &[(f64, f64)], encoder: &str, reuse: bool, lift_to: Option<f64>, full_range: bool) -> Self {
        let Ok(dir) = crate::bench::cache_dir() else { return Self::off() };
        let mut encoder = match lift_to {
            Some(mark) => format!("{encoder}|lifted to {mark}"),
            None => encoder.to_string(),
        };
        if full_range {
            encoder.push_str("|full range kept");
        }
        // Version 1 was measured on clips cut by where the decoder started: for some files
        // (MPEG-TS most of all) they were not the frames their positions say.
        let _ = std::fs::remove_file(dir.join("trials-v1.jsonl"));
        Self::open_at(dir.join("trials-v2.jsonl"), input, size, positions, &encoder, reuse)
    }

    fn open_at(path: PathBuf, input: &Path, size: u64, positions: &[(f64, f64)], encoder: &str, reuse: bool) -> Self {
        let mtime = std::fs::metadata(input).ok().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
        let abs = std::fs::canonicalize(input).unwrap_or_else(|_| input.to_path_buf());
        let clips: Vec<String> = positions.iter().map(|(start, secs)| format!("{start:.3}+{secs:.3}")).collect();
        let file = format!("{}|{size}|{mtime}|{}|{encoder}|measured {MEASURING_VERSION}", abs.display(), clips.join(","));
        let mut known = std::collections::HashMap::new();
        // Line by line from the bytes: a line that is not text is passed over, the rest count.
        let bytes = std::fs::read(&path).unwrap_or_default();
        let lines: Vec<&str> = bytes.split(|b| *b == b'\n').filter_map(|l| std::str::from_utf8(l).ok()).filter(|l| !l.is_empty()).collect();
        if reuse {
            // The file name is in every line; skip the JSON of the others.
            let needle = serde_json::to_string(&file).unwrap_or_default();
            for l in lines.iter().filter(|l| l.contains(&needle)) {
                if let Ok(c) = serde_json::from_str::<CacheLine>(l)
                    && c.file == file
                {
                    known.insert((settings_key(c.preset, &c.pix_fmt, &c.params), quarters(c.trial.crf)), c.trial);
                }
            }
        }
        if lines.len() > CACHE_MAX_LINES {
            let keep = &lines[lines.len() - CACHE_MAX_LINES / 2..];
            let tmp = path.with_extension("jsonl.tmp");
            if std::fs::write(&tmp, keep.join("\n") + "\n").is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
        TrialCache { path: Some(path), file, known }
    }

    fn get(&self, enc: &EncodeSettings, crf: f64) -> Option<Trial> {
        self.known.get(&(settings_key(enc.preset, enc.pix_fmt_out, &enc.full_params()), quarters(crf))).copied()
    }

    /// Every trial known for these settings at a whole CRF, lowest CRF first.
    fn whole(&self, enc: &EncodeSettings) -> Vec<Trial> {
        let key = settings_key(enc.preset, enc.pix_fmt_out, &enc.full_params());
        let mut all: Vec<Trial> = self.known.iter().filter(|((k, q), _)| *k == key && q % 4 == 0).map(|(_, t)| *t).collect();
        all.sort_by(|a, b| a.crf.total_cmp(&b.crf));
        all
    }

    fn put(&mut self, enc: &EncodeSettings, t: Trial) {
        self.known.insert((settings_key(enc.preset, enc.pix_fmt_out, &enc.full_params()), quarters(t.crf)), t);
        let Some(path) = &self.path else { return };
        let time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let line = CacheLine { file: self.file.clone(), preset: enc.preset, pix_fmt: enc.pix_fmt_out.to_string(), params: enc.full_params(), time, trial: t };
        // Losing the memory of one trial is not worth failing the encode for.
        if let Ok(json) = serde_json::to_string(&line) {
            let _ = crate::files::append_line(path, &json, true);
        }
    }
}

/// A CRF as a whole number of quarter steps.
fn quarters(crf: f64) -> u32 {
    (crf * 4.0).round().max(0.0) as u32
}

/// Where trials are run for one video: cuts the sample clips when the first trial that
/// isn't known from an earlier run needs them, and remembers every trial.
pub struct Lab<'a> {
    pub tools: &'a Tools,
    pub info: &'a MediaInfo,
    input: &'a Path,
    tmp: &'a Path,
    positions: Vec<(f64, f64)>,
    samples: Vec<Sample>,
    vmaf_threads: usize,
    cache: TrialCache,
    /// CRFs (in quarter steps) of the trials a search has listed.
    shown: std::collections::HashSet<u32>,
    /// Samples below this count as this in a trial's mean (see [`lifted_mean`]).
    lift_to: Option<f64>,
}

impl<'a> Lab<'a> {
    pub fn new(tools: &'a Tools, input: &'a Path, info: &'a MediaInfo, tmp: &'a Path, positions: Vec<(f64, f64)>, vmaf_threads: usize, cache: TrialCache) -> Self {
        Lab { tools, info, input, tmp, positions, samples: Vec::new(), vmaf_threads, cache, shown: Default::default(), lift_to: None }
    }

    /// Trials whose mean is the one expected after the weak stretches are encoded again.
    /// The cache has to have been opened for the same mark ([`TrialCache::open`]).
    pub fn lifting(mut self, lift_to: Option<f64>) -> Self {
        self.lift_to = lift_to;
        self
    }

    /// The trial at `crf`, and whether it was known from an earlier run.
    pub fn trial(&mut self, enc: &EncodeSettings, crf: f64) -> Result<(Trial, bool)> {
        if let Some(t) = self.cache.get(enc, crf) {
            return Ok((t, true));
        }
        if self.samples.is_empty() {
            self.samples = extract_samples(self.tools, self.input, self.info, self.tmp, self.positions.clone())?;
        }
        let t = trial(self.tools, self.info, &self.samples, self.tmp, enc, crf, self.vmaf_threads, self.info.video_bitrate / 8.0, self.lift_to)?;
        self.cache.put(enc, t);
        Ok((t, false))
    }

    /// Deletes the sample clips (they are large); a later trial would cut them again.
    pub fn discard_samples(&mut self) {
        for s in self.samples.drain(..) {
            let _ = std::fs::remove_file(s.reference);
        }
    }
}

#[derive(Clone, Copy)]
pub struct SearchParams {
    pub target_vmaf: f64,
    pub min_crf: u8,
    pub max_crf: u8,
    /// The first CRF tried; the closer to the answer, the fewer trials.
    pub start_crf: u8,
    /// Give up once the answer is known to come out larger than this share of the source
    /// video (None: search to the end whatever the size).
    pub max_size_ratio: Option<f64>,
    /// VMAF the worst sample has to reach as well (None: only the mean counts).
    pub worst_sample: Option<f64>,
    /// Which way the answer is rounded to a quarter step.
    pub landing: Landing,
}

/// How the search settles on a quarter-step CRF between its measurements.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Landing {
    /// Toward quality. For encodes nothing corrects afterwards: flooring cost 0.3 to 0.7% of the
    /// bytes on simulated whole videos.
    SafeSide,
    /// As near the crossing as the quarter steps allow. Only for an encode whose mean is measured
    /// and brought to the target afterwards: on its own this left 15 to 44% of sample placements
    /// under the target.
    Nearest,
}

impl Landing {
    fn round(self, crf: f64) -> f64 {
        match self {
            Landing::SafeSide => quarter(crf),
            Landing::Nearest => nearest_quarter(crf),
        }
    }
}

impl SearchParams {
    /// What a trial is worth to the search: its mean, lowered by what its worst sample
    /// lacks. It reaches the target when the mean does and the worst sample its own mark.
    pub fn score(&self, t: &Trial) -> f64 {
        match self.worst_sample {
            Some(floor) => t.mean_vmaf.min(t.min_vmaf + (self.target_vmaf - floor)),
            None => t.mean_vmaf,
        }
    }
}

/// What the CRF search concluded.
pub enum Outcome {
    /// Encode at this trial's CRF; its scores and size are what the line through the two
    /// nearest measurements predicts.
    Found(Trial),
    /// Even the lowest CRF allowed misses the target.
    OutOfReach,
    /// This trial misses the target and is already larger than wanted; a lower CRF, which
    /// the target needs, is larger still.
    TooLarge(Trial),
}

/// Ends the search from inside a trial.
#[derive(Debug)]
struct TooLarge;

impl std::fmt::Display for TooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the encode would be too large")
    }
}

impl std::error::Error for TooLarge {}

/// First guess when nothing is known about the video: measured curves of eight clips of
/// different kinds cross VMAF 93 at CRF 35 on average (28 to 39).
pub const START_CRF: u8 = 35;

/// Winning CRF (None if even the minimum fails) and every (crf, score) evaluated.
pub type CrfSearch = (Option<u8>, Vec<(u8, f64)>);
/// Usual VMAF drop per CRF step for SVT-AV1 around [`START_CRF`] (measured on the same clips:
/// 0.3 to 0.6 there, 0.47 on average, and about 0.5 where the score passes 93).
pub const VMAF_PER_CRF: f64 = 0.5;

/// How much that drop grows with each CRF step: on the measured curves it went from about
/// 0.15 VMAF per step at CRF 22 to 0.65 at CRF 44.
const SLOPE_RISE: f64 = 0.02;

/// CRF where a curve of the usual shape through one measured point reaches `target`. A
/// straight line with the usual slope steps too far toward low CRFs, where the curve flattens,
/// and not far enough toward high ones.
fn usual_crossing(crf: f64, score: f64, target: f64) -> f64 {
    let slope = (VMAF_PER_CRF + SLOPE_RISE * (crf - START_CRF as f64)).max(0.1);
    // score - target = slope·x + SLOPE_RISE/2·x², x being the way to go. Past the flat top
    // of that parabola no CRF reaches the target: the top is as far as the shape can tell.
    let reach = (slope * slope + 2.0 * SLOPE_RISE * (score - target)).max(0.0);
    crf + (reach.sqrt() - slope) / SLOPE_RISE
}

/// Finds the highest CRF in `min..=max` whose score is >= `target`, assuming the score falls
/// as CRF rises. Instead of halving the range, it interpolates between the nearest passing
/// and failing CRFs, which usually needs 3-4 evaluations instead of 5-6.
/// Returns the winning CRF (None if even `min` fails) and every (crf, score) evaluated.
pub fn find_crf(start: u8, min: u8, max: u8, target: f64, mut eval: impl FnMut(u8) -> Result<f64>) -> Result<CrfSearch> {
    if min > max {
        bail!("--min-crf {min} is above --max-crf {max}");
    }
    let mut tried: BTreeMap<u8, f64> = BTreeMap::new();
    let mut crf = start.clamp(min, max);
    loop {
        let score = eval(crf)?;
        tried.insert(crf, score);
        // Highest passing and lowest failing CRF seen so far.
        let pass = tried.iter().filter(|(_, v)| **v >= target).map(|(c, v)| (*c, *v)).max_by_key(|(c, _)| *c);
        let fail = tried.iter().filter(|(_, v)| **v < target).map(|(c, v)| (*c, *v)).min_by_key(|(c, _)| *c);
        // With scores on one side of the target only: along the line through the two nearest
        // of them, or along the usual shape of the curve when there is just one.
        let step = |c: u8, v: f64, other: Option<(&u8, &f64)>| {
            let to = match other {
                Some((&b, &vb)) => (v - target).abs() / ((vb - v).abs() / c.abs_diff(b) as f64).clamp(0.1, 2.0),
                None => (usual_crossing(c as f64, v, target) - c as f64).abs(),
            };
            to.round().max(1.0) as u8
        };
        let next = match (pass, fail) {
            (Some((p, _)), Some((f, _))) if f <= p + 1 => None,
            (Some((p, vp)), Some((f, vf))) => {
                let frac = if vp > vf { (vp - target) / (vp - vf) } else { 0.5 };
                let c = p as f64 + frac * (f - p) as f64;
                Some((c.round() as u8).clamp(p + 1, f - 1))
            }
            (Some((p, _)), None) if p >= max => None,
            (Some((p, vp)), None) => Some(p.saturating_add(step(p, vp, tried.range(..p).next_back())).min(max)),
            (None, Some((f, _))) if f <= min => None,
            (None, Some((f, vf))) => Some(f.saturating_sub(step(f, vf, tried.range(f + 1..).next())).max(min)),
            (None, None) => unreachable!("just inserted a trial"),
        };
        match next {
            // Non-monotonic scores could revisit a CRF; stop rather than loop.
            Some(c) if !tried.contains_key(&c) => crf = c,
            _ => {
                let best = tried.iter().filter(|(_, v)| **v >= target).map(|(c, _)| *c).max();
                return Ok((best, tried.into_iter().collect()));
            }
        }
    }
}

/// Where the score crosses the target, and the measurements the answer rests on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Crossing {
    /// Quarter-step CRF expected to just meet the target.
    pub crf: f64,
    /// The two measured CRFs it was read from (the same one twice: `crf` itself was measured).
    pub from: (u8, u8),
}

/// How far an estimate may sit from the two measurements it is read from: the product of its
/// distances to them, in CRF steps. A straight line through two points of a curve is off by
/// about half the curvature times this product, so a small product means a small error
/// whether the estimate lies between the points or just outside them.
const MAX_REACH: f64 = 6.0;

/// Curvature assumed when an estimate lies outside its two measurements (VMAF per CRF step
/// squared). Scores fall faster as the CRF rises, so outside the points the straight line
/// promises more than the encoder gives; the estimate is moved toward quality by that much.
const CURVATURE: f64 = 0.03;

/// With every measurement failing, an answer read off them lies below all of them and rests
/// on the curve bending as assumed. Up to this product of its distances to the two (see
/// [`MAX_REACH`]) that is taken on trust; further off, the CRF is measured first.
const MAX_UNMEASURED_REACH: f64 = 2.0;

enum Step {
    Try(u8),
    Done(Option<Crossing>),
}

fn next_step(tried: &BTreeMap<u8, f64>, min: u8, max: u8, target: f64, landing: Landing) -> Step {
    // The highest passing CRF, and the lowest failing one above it (a failing score below a
    // passing one is noise).
    let pass = tried.iter().rev().find(|(_, v)| **v >= target).map(|(c, v)| (*c, *v));
    let fail = tried.iter().find(|(c, v)| **v < target && pass.is_none_or(|p| **c > p.0)).map(|(c, v)| (*c, *v));
    match (pass, fail) {
        (Some((p, vp)), Some((f, vf))) => {
            let (lo, hi) = (p as f64, f as f64);
            let x = lo + (vp - target) / (vp - vf) * (hi - lo);
            if f - p <= 1 || (x - lo) * (hi - x) <= MAX_REACH {
                return Step::Done(Some(Crossing { crf: landing.round(x).clamp(lo, hi - 0.25), from: (p, f) }));
            }
            Step::Try((x.round() as u8).clamp(p + 1, f - 1))
        }
        (Some((p, _)), None) if p >= max => Step::Done(Some(Crossing { crf: p as f64, from: (p, p) })),
        (Some((p, vp)), None) => {
            let Some((&b, &vb)) = tried.range(..p).next_back() else {
                return Step::Try((usual_crossing(p as f64, vp, target).round() as u8).clamp(p + 1, max));
            };
            let slope = ((vb - vp) / (p - b) as f64).clamp(0.1, 2.0);
            let x = p as f64 + (vp - target) / slope;
            let reach = (x - p as f64) * (x - b as f64);
            if reach <= MAX_REACH && x <= max as f64 {
                let safe = x - CURVATURE * reach / (2.0 * slope);
                return Step::Done(Some(Crossing { crf: landing.round(safe).max(p as f64), from: (b, p) }));
            }
            Step::Try((x.round() as u8).clamp(p + 1, max))
        }
        (None, Some((f, _))) if f <= min => Step::Done(None),
        (None, Some((f, vf))) => {
            let Some((&b, &vb)) = tried.range(f + 1..).next() else {
                return Step::Try((usual_crossing(f as f64, vf, target).round().max(0.0) as u8).clamp(min, f - 1));
            };
            let slope = ((vf - vb) / (b - f) as f64).clamp(0.1, 2.0);
            let x = f as f64 - (target - vf) / slope;
            let reach = (f as f64 - x) * (b as f64 - x);
            // (Never the failing CRF itself, which rounding to the nearest step could give.)
            let safe = landing.round(x - CURVATURE * reach / (2.0 * slope)).min(f as f64 - 0.25);
            if reach <= MAX_REACH && safe >= min as f64 {
                if reach > MAX_UNMEASURED_REACH {
                    return Step::Try((safe.round() as u8).clamp(min, f - 1));
                }
                return Step::Done(Some(Crossing { crf: safe, from: (f, b) }));
            }
            Step::Try((x.round().max(0.0) as u8).clamp(min, f - 1))
        }
        (None, None) => unreachable!("called after a trial"),
    }
}

/// Mean VMAF by the CRF it was measured at.
pub type Scores = Vec<(u8, f64)>;

/// Finds the quarter-step CRF in `min..=max` where the score crosses `target`, assuming the
/// score falls as CRF rises, with as few evaluations as it can: each new CRF is where a line
/// through the two nearest measurements crosses the target, and the search stops as soon as
/// that crossing is close enough to its measurements to be trusted ([`MAX_REACH`]), instead
/// of narrowing down to two neighbouring CRFs as [`find_crf`] does. Usually 2-3 evaluations.
/// Returns the crossing (None if even `min` fails) and every (crf, score) evaluated.
#[allow(clippy::too_many_arguments)]
fn find_crossing_landing(
    known: &[(u8, f64)],
    start: u8,
    min: u8,
    max: u8,
    target: f64,
    landing: Landing,
    mut eval: impl FnMut(u8) -> Result<f64>,
) -> Result<(Option<Crossing>, Scores)> {
    if min > max {
        bail!("--min-crf {min} is above --max-crf {max}");
    }
    let mut tried: BTreeMap<u8, f64> = known.iter().filter(|(c, _)| (min..=max).contains(c)).copied().collect();
    let start = start.clamp(min, max);
    // Scores known beforehand that have the target between them say more than a guess. Known
    // scores on one side only may be far off: the guess goes first while it can still tell
    // something they don't.
    let pass = tried.iter().rev().find(|(_, v)| **v >= target).map(|(c, _)| *c);
    let fail = tried.iter().find(|(c, v)| **v < target && pass.is_none_or(|p| **c > p)).map(|(c, _)| *c);
    let guess_first = match (pass, fail) {
        (Some(_), Some(_)) => false,
        (Some(p), None) => start > p,
        (None, Some(f)) => start < f,
        (None, None) => true,
    };
    let mut next = if guess_first { Step::Try(start) } else { next_step(&tried, min, max, target, landing) };
    loop {
        let found = match next {
            Step::Try(c) if !tried.contains_key(&c) => {
                tried.insert(c, eval(c)?);
                next = next_step(&tried, min, max, target, landing);
                continue;
            }
            // Scores that don't fall with the CRF could send the search back to a CRF it
            // has measured; settle for the highest one that passed.
            Step::Try(_) => tried.iter().rev().find(|(_, v)| **v >= target).map(|(c, _)| Crossing { crf: *c as f64, from: (*c, *c) }),
            Step::Done(found) => found,
        };
        return Ok((found, tried.into_iter().collect()));
    }
}

/// Runs [`find_crossing_landing`] with real sample encodes. Returns the outcome and all trials.
pub fn search(lab: &mut Lab, enc: &EncodeSettings, p: &SearchParams) -> Result<(Outcome, Vec<Trial>)> {
    let tools = lab.tools;
    // A second search of the same video (see `SearchParams::worst_sample`) does not list
    // the first one's trials again.
    let show = |t: &Trial, known: bool, shown: &mut std::collections::HashSet<u32>| {
        if !shown.insert(quarters(t.crf)) {
            return;
        }
        eprintln!(
            "  crf {:2}: VMAF {:.2} (worst sample {:.2}), size ~{:.0}% of source{}",
            t.crf, t.mean_vmaf, t.min_vmaf, t.size_ratio * 100.0, if known { " (measured earlier)" } else { "" }
        );
        tools.emit(crate::events::Event::Trial { crf: t.crf, mean_vmaf: t.mean_vmaf, min_vmaf: t.min_vmaf, size_ratio: t.size_ratio });
    };
    // A trial that misses the target while already too large settles the matter, unless a
    // higher CRF is known to pass: then its miss was noise.
    let too_large = |t: &Trial, all: &BTreeMap<u8, Trial>| {
        p.score(t) < p.target_vmaf
            && p.max_size_ratio.is_some_and(|max| t.size_ratio > max)
            && !all.values().any(|u| u.crf > t.crf && p.score(u) >= p.target_vmaf)
    };
    // Trials from earlier runs count as if they had just been measured.
    let mut trials: BTreeMap<u8, Trial> = BTreeMap::new();
    for t in lab.cache.whole(enc) {
        let crf = t.crf as u8;
        if (p.min_crf..=p.max_crf).contains(&crf) {
            show(&t, true, &mut lab.shown);
            trials.insert(crf, t);
        }
    }
    let known: Vec<(u8, f64)> = trials.iter().map(|(c, t)| (*c, p.score(t))).collect();
    let found = match trials.values().find(|t| too_large(t, &trials)) {
        Some(_) => Err(TooLarge.into()),
        None => find_crossing_landing(&known, p.start_crf, p.min_crf, p.max_crf, p.target_vmaf, p.landing, |crf| {
            let (t, known) = lab.trial(enc, crf as f64)?;
            show(&t, known, &mut lab.shown);
            trials.insert(crf, t);
            if too_large(&t, &trials) {
                return Err(TooLarge.into());
            }
            Ok(p.score(&t))
        }),
    };
    let outcome = match found {
        Ok((Some(c), _)) => Outcome::Found(expected(&trials[&c.from.0], &trials[&c.from.1], c.crf)),
        Ok((None, _)) => Outcome::OutOfReach,
        // The trial that ended the search is the lowest failing one so far or below it.
        Err(e) if e.is::<TooLarge>() => match trials.values().filter(|t| p.score(t) < p.target_vmaf).min_by(|a, b| a.crf.total_cmp(&b.crf)) {
            Some(t) => Outcome::TooLarge(*t),
            None => return Err(e),
        },
        Err(e) => return Err(e),
    };
    Ok((outcome, trials.into_values().collect()))
}

/// VMAF lost per CRF step around `crf`, read off the two trials nearest to it; the usual
/// figure where there are not two to go by.
pub fn slope_at(trials: &[Trial], crf: f64) -> f64 {
    let mut near: Vec<&Trial> = trials.iter().collect();
    near.sort_by(|a, b| (a.crf - crf).abs().total_cmp(&(b.crf - crf).abs()));
    match near[..] {
        [a, b, ..] if a.crf != b.crf => ((a.mean_vmaf - b.mean_vmaf) / (b.crf - a.crf)).clamp(0.1, 2.0),
        _ => VMAF_PER_CRF,
    }
}

/// What an encode at `crf` should give, read off the line through two measured trials:
/// scores change linearly with the CRF over a few steps, sizes by a constant factor per step.
fn expected(a: &Trial, b: &Trial, crf: f64) -> Trial {
    if a.crf == b.crf || crf == a.crf {
        return Trial { crf, ..*a };
    }
    let w = (crf - a.crf) / (b.crf - a.crf);
    let mix = |x: f64, y: f64| x + (y - x) * w;
    let size_ratio = if a.size_ratio > 0.0 && b.size_ratio > 0.0 { mix(a.size_ratio.ln(), b.size_ratio.ln()).exp() } else { mix(a.size_ratio, b.size_ratio) };
    Trial { crf, mean_vmaf: mix(a.mean_vmaf, b.mean_vmaf), min_vmaf: mix(a.min_vmaf, b.min_vmaf), size_ratio }
}

/// Encodes the samples at `crf` and scores them against their references.
///
/// The samples are encoded end to end as one clip and scored in one run: the encoder and
/// libvmaf start once instead of once per sample (a tenth to a sixth of a trial's time), and
/// no sample begins on a keyframe it would not get in the real encode. Checked frame for
/// frame against the same stretches of a whole encode (four clips, two CRFs each): the mean
/// over the samples came out 0.03 VMAF higher on average and at most 0.16 off, where samples
/// encoded one by one came out 0.04 lower and at most 0.13 off.
#[allow(clippy::too_many_arguments)]
pub fn trial(
    tools: &Tools,
    info: &MediaInfo,
    samples: &[Sample],
    tmp: &Path,
    enc: &EncodeSettings,
    crf: f64,
    threads: usize,
    source_bps: f64,
    lift_to: Option<f64>,
) -> Result<Trial> {
    if samples.is_empty() {
        bail!("the video reports no length, so no sample clips could be cut from it and it was not measured; check that it plays");
    }
    let joined = if samples.len() > 1 { trial_joined(tools, info, samples, tmp, enc, crf, threads)? } else { None };
    let (scores, bytes) = match joined {
        Some(r) => r,
        None => trial_each(tools, info, samples, tmp, enc, crf, threads)?,
    };
    let secs: f64 = samples.iter().map(|s| s.secs).sum();
    let mean = lifted_mean(&scores, lift_to);
    let min = scores.iter().copied().fold(f64::INFINITY, f64::min);
    let ratio = if source_bps > 0.0 { bytes as f64 / secs / source_bps } else { 1.0 };
    Ok(Trial { crf, mean_vmaf: mean, min_vmaf: min, size_ratio: ratio })
}

/// How far above the floor of the whole-video check a sample is counted when it scores
/// below that: see [`lifted_mean`]. Only safe together with the check of the mean after the
/// repairs ([`crate::repair`]): alone, 2.0 put 10 to 55% of simulated sample placements under
/// the target.
pub const LIFT_ABOVE_FLOOR: f64 = 2.0;

/// How far below the target the search aims the mean counted that way, where the whole video
/// is scored afterwards and what is missing is made up by encoding its next-lowest GOPs again.
/// The samples are off by 0.1 to 0.25 either way, so aiming at the target itself ended above it
/// by 0.18 on average; 0.3 low did no better than 0.2, and 0.1 about the same. The counted mean
/// is nearly flat in the CRF, so 0.2 here lowers the mean before repairs by 0.4 to 1.1.
pub const AIM_LOW: f64 = 0.2;

/// The mean a video will have once its weak stretches have been encoded again, as far as
/// its samples can tell: every sample below `lift_to` counts as `lift_to`.
///
/// A one-pass encode is scored along its whole length afterwards and the stretches below
/// the floor are encoded again at a lower CRF ([`crate::repair`]). That lifts the mean: a
/// search that aims the plain mean of the samples at the target ends above it by as much,
/// and on a video whose quality is uneven that is a lot. Measured on a 15-minute video with
/// a hard first minute: aimed at 93, the encode scored 93.8 before and 94.6 after 4:28 of
/// it had been encoded again, 196 MB; an earlier build that happened to aim lower made
/// 167 MB of it at 92.8 with the same floor held. The cheapest encode that holds both the
/// mean and the floor is the one whose mean reaches the target after the repairs: counted
/// this way the same video came to 185 MB at 93.8, and another to 373 MB at 93.2 where it
/// had been 430 MB at 94.2, the lowest five seconds of both where they were.
///
/// What the samples can know of that was checked on seven whole encodes at one CRF with every
/// third frame scored, repairs imitated on those scores: the mean foretold by samples of one
/// second, less the mean after the imitated repairs, was -0.02 to -0.47 counting them as
/// floor + 1.5 and -0.24 to +0.08 as floor + 2 (at floor + 2.5, +0.02 to +0.18: too much
/// everywhere). A stretch is encoded again by whole groups of pictures, so frames already above
/// the mark rise with it. Real runs agreed: floor + 1.5 fell short by 0.09 to 0.14. What the
/// mark still misses is made up after the encode (see [`crate::repair::repair`]).
pub fn lifted_mean(scores: &[f64], lift_to: Option<f64>) -> f64 {
    let floor = lift_to.unwrap_or(f64::NEG_INFINITY);
    scores.iter().map(|s| s.max(floor)).sum::<f64>() / scores.len().max(1) as f64
}

/// Each sample encoded and scored by itself: the score of each, and the bytes of all.
fn trial_each(tools: &Tools, info: &MediaInfo, samples: &[Sample], tmp: &Path, enc: &EncodeSettings, crf: f64, threads: usize) -> Result<(Vec<f64>, u64)> {
    let (mut scores, mut bytes) = (Vec::new(), 0u64);
    for (i, s) in samples.iter().enumerate() {
        let out = tmp.join(format!("sample{i}.crf{crf}.mkv"));
        let mut args: Vec<String> = ["-y", "-v", "error", "-i", &path_arg(&s.reference)].map(String::from).to_vec();
        args.extend(enc.video_args(crf));
        args.push(path_arg(&out));
        tools.run(&args)?;
        scores.push(tools.vmaf(&out, &s.reference, vmaf_model(info), reference_pix_fmt(info), threads, 0, VMAF_STRIDE)?);
        bytes += std::fs::metadata(&out)?.len();
        std::fs::remove_file(&out)?;
    }
    Ok((scores, bytes))
}

/// A line of a concat demuxer list.
fn concat_line(path: &Path) -> String {
    format!("file '{}'\n", path.to_string_lossy().replace('\'', "'\\''"))
}

/// The samples encoded end to end as one clip: the score of each, and the bytes of all.
/// None when the frames of the encode do not line up with the samples (never seen; the
/// caller then measures each sample by itself).
fn trial_joined(tools: &Tools, info: &MediaInfo, samples: &[Sample], tmp: &Path, enc: &EncodeSettings, crf: f64, threads: usize) -> Result<Option<(Vec<f64>, u64)>> {
    // Absolute: a relative name in a concat list counts from the list's folder.
    let list = tmp.join("samples.txt");
    // A list is read line by line: a folder with a line break in its name cannot be listed.
    if samples.iter().any(|s| s.reference.to_string_lossy().contains(['\n', '\r'])) {
        return Ok(None);
    }
    let lines = samples.iter().map(|s| Ok(concat_line(&std::path::absolute(&s.reference)?))).collect::<Result<String>>()?;
    crate::files::write_new(&list, lines.as_bytes())?;
    let out = tmp.join(format!("samples.crf{crf}.mkv"));
    let mut args: Vec<String> = ["-y", "-v", "error", "-f", "concat", "-safe", "0", "-i", &path_arg(&list)].map(String::from).to_vec();
    args.extend(enc.video_args(crf));
    args.push(path_arg(&out));
    tools.run(&args)?;
    let frames = tools.vmaf_frames(&out, &list, tmp, vmaf_model(info), reference_pix_fmt(info), threads, VMAF_STRIDE)?;
    let bytes = std::fs::metadata(&out)?.len();
    std::fs::remove_file(&out)?;
    Ok(split_scores(&frames, &samples.iter().map(|s| s.frames).collect::<Vec<_>>(), VMAF_STRIDE as u64).map(|scores| (scores, bytes)))
}

/// Mean score of each clip from the scored frames (number, score) of clips of `lengths`
/// frames laid end to end. None unless the scored frames reach the end of the last clip and
/// stop there, to within the `stride` between scored frames.
fn split_scores(frames: &[(u64, f64)], lengths: &[u64], stride: u64) -> Option<Vec<f64>> {
    let total: u64 = lengths.iter().sum();
    let last = frames.iter().map(|f| f.0).max()?;
    if last >= total || last + stride < total {
        return None;
    }
    let mut start = 0;
    lengths
        .iter()
        .map(|&n| {
            let own: Vec<f64> = frames.iter().filter(|f| (start..start + n).contains(&f.0)).map(|f| f.1).collect();
            start += n;
            (!own.is_empty()).then(|| own.iter().sum::<f64>() / own.len() as f64)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The search as encodes nothing corrects afterwards have it.
    fn find_crossing(known: &[(u8, f64)], start: u8, min: u8, max: u8, target: f64, eval: impl FnMut(u8) -> Result<f64>) -> Result<(Option<Crossing>, Scores)> {
        find_crossing_landing(known, start, min, max, target, Landing::SafeSide, eval)
    }

    #[test]
    fn sample_clips_of_slow_videos_are_long_enough_to_be_scored() {
        let clip = |fps, secs| clip_secs(&info_with_fps(fps), secs);
        // Ordinary videos keep the lengths that were asked for.
        assert_eq!(clip(24.0, MEAN_SAMPLE_SECS), MEAN_SAMPLE_SECS);
        assert_eq!(clip(6.0, MEAN_SAMPLE_SECS), MEAN_SAMPLE_SECS);
        assert_eq!(clip(1.0, 10.0), 10.0);
        // Slower ones get the time their frames take.
        assert_eq!(clip(2.0, MEAN_SAMPLE_SECS), 3.0);
        assert_eq!(clip(1.0, MEAN_SAMPLE_SECS), 6.0);
        assert_eq!(clip(0.5, SAMPLE_SECS), 12.0);
        // A rate nobody knows says nothing about the frames.
        assert_eq!(clip(0.0, 4.0), 4.0);
        // Every clip laid out for a 40 s video at 1 fps then holds a scored frame or two.
        for (_, secs) in mean_sample_positions(40.0, mean_sample_count(40.0), clip(1.0, MEAN_SAMPLE_SECS)) {
            assert!(secs >= MIN_SAMPLE_FRAMES, "{secs}");
        }
    }

    #[test]
    fn joined_samples_are_scored_one_by_one() {
        // Three clips of 5, 4 and 6 frames; every third frame is scored.
        let frames: Vec<(u64, f64)> = [(0, 90.0), (3, 92.0), (6, 80.0), (9, 70.0), (12, 72.0)].to_vec();
        assert_eq!(split_scores(&frames, &[5, 4, 6], 3), Some(vec![91.0, 80.0, 71.0]));
        // In any order, as a log may list them.
        let mut mixed = frames.clone();
        mixed.reverse();
        assert_eq!(split_scores(&mixed, &[5, 4, 6], 3), Some(vec![91.0, 80.0, 71.0]));
        // The encode came out shorter or longer than the samples: not to be trusted.
        assert_eq!(split_scores(&frames[..4], &[5, 4, 6], 3), None);
        assert_eq!(split_scores(&frames, &[5, 4, 3], 3), None);
        assert_eq!(split_scores(&frames, &[5, 4, 9], 3), None);
        // A clip too short to hold a scored frame, and no frames at all.
        assert_eq!(split_scores(&[(0, 90.0), (3, 91.0)], &[2, 1, 2], 3), None);
        assert_eq!(split_scores(&[], &[5], 3), None);
        assert_eq!(concat_line(Path::new("/a b/it's.mkv")), "file '/a b/it'\\''s.mkv'\n");
    }

    #[test]
    fn paths_given_to_ffmpeg_cannot_pass_for_a_protocol() {
        let rel = path_arg(Path::new("a:b/x.mkv"));
        assert!(rel.starts_with('/') && rel.ends_with("/a:b/x.mkv"), "{rel}");
        assert_eq!(path_arg(Path::new("/lib/a:b/x.mkv")), "/lib/a:b/x.mkv");
    }

    /// Highest CRF passing by brute force, for checking `find_crf`.
    fn brute(min: u8, max: u8, target: f64, f: impl Fn(u8) -> f64) -> Option<u8> {
        (min..=max).filter(|&c| f(c) >= target).max()
    }

    fn check(min: u8, max: u8, target: f64, f: impl Fn(u8) -> f64 + Copy) -> usize {
        let (got, tried) = find_crf(START_CRF, min, max, target, |c| Ok(f(c))).unwrap();
        assert_eq!(got, brute(min, max, target, f), "target {target}, tried {tried:?}");
        tried.len()
    }

    #[test]
    fn finds_exact_answer_on_linear_curves() {
        for slope in [0.2, 0.36, 0.5, 0.9] {
            for target in [85.0, 90.0, 93.0, 95.0, 97.0] {
                let n = check(20, 55, target, |c| 105.0 - slope * c as f64);
                assert!(n <= 6, "slope {slope} target {target}: {n} trials");
            }
        }
    }

    #[test]
    fn finds_exact_answer_on_curved_scores() {
        // VMAF flattens near 100 and drops faster at high CRF.
        let f = |c: u8| 100.0 - 0.002 * (c as f64).powi(2) - 0.05 * c as f64;
        for target in [88.0, 91.0, 93.0, 95.0] {
            check(18, 60, target, f);
        }
    }

    #[test]
    fn handles_unreachable_and_always_passing_targets() {
        let (got, _) = find_crf(START_CRF, 20, 55, 99.0, |c| Ok(90.0 - c as f64 * 0.1)).unwrap();
        assert_eq!(got, None);
        let (got, _) = find_crf(START_CRF, 20, 55, 50.0, |_| Ok(99.0)).unwrap();
        assert_eq!(got, Some(55));
    }

    #[test]
    fn needs_fewer_trials_than_binary_search() {
        // Realistic curve from the pilot: ~0.36 VMAF per CRF, plus a little curvature.
        let f = |c: u8| 103.5 - 0.30 * c as f64 - 0.0015 * (c as f64).powi(2);
        let targets: Vec<f64> = (0..=40).map(|i| 86.0 + i as f64 * 0.25).collect();
        let total: usize = targets.iter().map(|&t| check(20, 55, t, f)).sum();
        let avg = total as f64 / targets.len() as f64;
        eprintln!("average trials: {avg:.2}");
        // Binary search over 20..=55 needs ~5.2 on average.
        assert!(avg <= 4.0, "average {avg:.2} trials");
    }

    /// Exact crossing of a curve given at integer CRFs: between the highest passing CRF and
    /// the next one.
    fn crossing(min: u8, max: u8, target: f64, f: impl Fn(u8) -> f64) -> Option<f64> {
        let p = brute(min, max, target, &f)?;
        if p == max {
            return Some(max as f64);
        }
        Some(p as f64 + (f(p) - target) / (f(p) - f(p + 1)))
    }

    /// Runs `find_crossing` on a curve; returns (trials, chosen - exact crossing, score at the
    /// chosen CRF - target).
    fn cross(start: u8, min: u8, max: u8, target: f64, f: impl Fn(u8) -> f64 + Copy) -> (usize, f64, f64) {
        let (got, tried) = find_crossing(&[], start, min, max, target, |c| Ok(f(c))).unwrap();
        let (got, want) = (got.expect("a crossing"), crossing(min, max, target, f).expect("reachable"));
        let lo = got.crf.floor() as u8;
        let score = if lo >= max { f(max) } else { f(lo) + (f(lo + 1) - f(lo)) * (got.crf - lo as f64) };
        assert!(got.crf >= min as f64 && got.crf <= max as f64, "{got:?}");
        assert!(tried.iter().any(|t| t.0 == got.from.0) && tried.iter().any(|t| t.0 == got.from.1), "{got:?} {tried:?}");
        (tried.len(), got.crf - want, score - target)
    }

    #[test]
    fn crossing_is_found_in_few_trials_on_smooth_curves() {
        // Shapes like measured ones: flat near the top, falling faster as the CRF rises.
        let curves: [&dyn Fn(u8) -> f64; 4] = [
            &|c| (100.4 - 0.10 * c as f64 - 0.0052 * (c as f64).powi(2)).min(99.5),
            &|c| (101.0 + 0.25 * c as f64 - 0.0142 * (c as f64).powi(2)).min(99.5),
            &|c| (103.5 - 0.02 * c as f64 - 0.0065 * (c as f64).powi(2)).min(99.5),
            &|c| 105.0 - 0.36 * c as f64,
        ];
        let (mut trials, mut cases) = (0, 0);
        for f in curves {
            for target in (0..=16).map(|i| 88.0 + i as f64 * 0.5) {
                let Some(want) = crossing(20, 55, target, f) else { continue };
                if want >= 55.0 {
                    continue;
                }
                let (n, off, miss) = cross(START_CRF, 20, 55, target, f);
                assert!(n <= 4, "target {target}: {n} trials");
                // Never more than a hair below the target, never far above it.
                assert!(miss > -0.05, "target {target}: {miss:+.3} VMAF");
                assert!(off > -0.75 && off < 0.15, "target {target}: {off:+.2} CRF from the crossing");
                trials += n;
                cases += 1;
            }
        }
        let avg = trials as f64 / cases as f64;
        eprintln!("average trials: {avg:.2} over {cases} cases");
        assert!(avg <= 3.0, "average {avg:.2} trials");
    }

    #[test]
    fn a_good_first_guess_needs_two_trials() {
        let f = |c: u8| 100.4 - 0.10 * c as f64 - 0.0052 * (c as f64).powi(2);
        for target in [90.0, 92.0, 93.0, 94.5, 96.0] {
            let want = crossing(20, 55, target, f).unwrap();
            for d in [-2i32, -1, 0, 1, 2] {
                let start = (want.round() as i32 + d) as u8;
                let (n, off, miss) = cross(start, 20, 55, target, f);
                assert_eq!(n, 2, "target {target}, start {start}");
                assert!(miss > -0.05 && off > -0.75, "target {target}, start {start}: {off:+.2} CRF, {miss:+.3} VMAF");
            }
        }
    }

    #[test]
    fn crossing_handles_the_ends_of_the_range() {
        // Out of reach: the lowest CRF is tried before giving up.
        let (got, tried) = find_crossing(&[], START_CRF, 20, 55, 99.0, |c| Ok(90.0 - c as f64 * 0.1)).unwrap();
        assert_eq!(got, None);
        assert!(tried.iter().any(|t| t.0 == 20), "{tried:?}");
        // Always passing: the highest CRF, measured.
        let (got, tried) = find_crossing(&[], START_CRF, 20, 55, 50.0, |_| Ok(99.0)).unwrap();
        assert_eq!(got, Some(Crossing { crf: 55.0, from: (55, 55) }));
        assert!(tried.len() <= 3, "{tried:?}");
        // One CRF allowed.
        let (got, tried) = find_crossing(&[], START_CRF, 30, 30, 93.0, |_| Ok(94.0)).unwrap();
        assert_eq!((got.map(|c| c.crf), tried.len()), (Some(30.0), 1));
        let (got, _) = find_crossing(&[], START_CRF, 30, 30, 93.0, |_| Ok(92.0)).unwrap();
        assert_eq!(got, None);
        assert!(find_crossing(&[], START_CRF, 40, 30, 93.0, |_| Ok(95.0)).is_err());
        // The crossing sits just above the lowest CRF: nothing below `min` is ever chosen.
        let (got, _) = find_crossing(&[], START_CRF, 28, 55, 93.0, |c| Ok(93.1 - (c as f64 - 28.0) * 0.4)).unwrap();
        let got = got.unwrap();
        assert!(got.crf >= 28.0 && got.crf <= 28.25, "{got:?}");
    }

    #[test]
    fn crossing_survives_scores_that_do_not_fall_steadily() {
        // Bumps of up to 0.3 VMAF on a 0.36/CRF slope, so neighbouring CRFs can swap order.
        for seed in 0..40u32 {
            let f = move |c: u8| 105.0 - 0.36 * c as f64 + 0.3 * (((c as u32 * 7919 + seed * 104729) % 1000) as f64 / 500.0 - 1.0);
            for target in [90.0, 93.0, 95.0] {
                let (got, tried) = find_crossing(&[], START_CRF, 20, 55, target, |c| Ok(f(c))).unwrap();
                let got = got.expect("reachable");
                assert!(tried.len() <= 6, "seed {seed}: {} trials", tried.len());
                // Within the bumps of where the smooth curve crosses.
                let smooth = (105.0 - target) / 0.36;
                assert!((got.crf - smooth).abs() < 2.5, "seed {seed} target {target}: {} vs {smooth:.2}", got.crf);
            }
        }
        // Flat, then a cliff: still ends.
        let (got, tried) = find_crossing(&[], START_CRF, 20, 55, 93.0, |c| Ok(if c < 41 { 96.0 } else { 80.0 })).unwrap();
        assert!(tried.len() <= 7, "{tried:?}");
        assert!(got.unwrap().crf < 41.0);
    }

    /// Mean VMAF at CRF 22..=46 of real clips of different kinds (SVT-AV1 preset 6, tune 0,
    /// four 8-second samples), measured with `av1opt curve`. The searches are held to these as
    /// well as to made-up shapes.
    const MEASURED: [[f64; 25]; 8] = [
        // 1080x1920 archive_W_h264_highbpp.mp4
        [97.53, 97.45, 97.34, 97.16, 96.97, 96.78, 96.59, 96.36, 96.12, 95.79, 95.46, 95.09, 94.72,
         94.27, 93.79, 93.31, 92.79, 92.23, 91.74, 91.12, 90.49, 89.81, 89.15, 88.44, 87.77],
        // 576x1024 archive_feedvault_h264.mp4
        [97.93, 97.79, 97.60, 97.31, 97.04, 96.78, 96.50, 96.22, 95.91, 95.51, 95.07, 94.65, 94.25,
         93.74, 93.17, 92.66, 92.12, 91.54, 91.01, 90.40, 89.77, 89.09, 88.44, 87.76, 87.13],
        // 1920x1080 h264_1080p_landscape.mp4
        [96.79, 96.68, 96.54, 96.31, 96.11, 95.90, 95.70, 95.50, 95.29, 95.00, 94.71, 94.42, 94.15,
         93.81, 93.45, 93.12, 92.77, 92.38, 92.05, 91.61, 91.21, 90.74, 90.30, 89.84, 89.43],
        // 1284x720 h264_720p.mp4
        [94.95, 94.76, 94.51, 94.11, 93.75, 93.39, 93.07, 92.75, 92.40, 91.93, 91.49, 91.06, 90.63,
         90.10, 89.60, 89.13, 88.63, 88.03, 87.50, 86.87, 86.23, 85.57, 84.91, 84.19, 83.57],
        // 1282x720 h264_high_bitrate.mp4
        [95.06, 94.96, 94.82, 94.59, 94.42, 94.23, 94.03, 93.84, 93.65, 93.39, 93.17, 92.89, 92.65,
         92.35, 92.02, 91.75, 91.41, 91.08, 90.74, 90.35, 89.97, 89.53, 89.13, 88.72, 88.30],
        // 1080x1920 h264_vertical_1920.mp4
        [99.06, 98.97, 98.87, 98.65, 98.45, 98.24, 98.04, 97.79, 97.55, 97.16, 96.70, 96.26, 95.86,
         95.29, 94.74, 94.18, 93.55, 92.87, 92.26, 91.50, 90.75, 89.82, 88.96, 88.04, 87.35],
        // 1920x1080 hevc.mov
        [98.84, 98.71, 98.52, 98.22, 97.92, 97.61, 97.30, 96.97, 96.64, 96.16, 95.68, 95.21, 94.73,
         94.13, 93.56, 92.99, 92.40, 91.71, 91.18, 90.46, 89.83, 89.04, 88.35, 87.59, 86.89],
        // 1280x720 mixed.mp4
        [96.23, 96.08, 95.89, 95.61, 95.30, 95.04, 94.77, 94.49, 94.19, 93.77, 93.42, 93.03, 92.65,
         92.18, 91.71, 91.25, 90.78, 90.23, 89.81, 89.22, 88.66, 87.99, 87.33, 86.75, 86.18],
    ];

    fn measured(curve: &[f64; 25]) -> impl Fn(u8) -> f64 + Copy + '_ {
        move |crf| curve[(crf - 22) as usize]
    }

    /// Targets a curve crosses well inside the measured range, with the exact crossing.
    fn measured_targets(curve: &[f64; 25]) -> Vec<(f64, f64)> {
        (0..=18)
            .map(|i| 88.0 + i as f64 * 0.5)
            .filter_map(|target| Some((target, crossing(22, 46, target, measured(curve))?)))
            .filter(|(_, want)| (23.0..=45.0).contains(want))
            .collect()
    }

    #[test]
    fn measured_curves_take_two_or_three_trials() {
        // From the fixed first guess, then from guesses near the answer, as history gives them.
        let (mut far, mut near) = (Vec::new(), Vec::new());
        for (i, curve) in MEASURED.iter().enumerate() {
            for (target, want) in measured_targets(curve) {
                let run = |start: u8| {
                    let (n, off, miss) = cross(start, 22, 46, target, measured(curve));
                    assert!(n <= if start == START_CRF { 4 } else { 3 }, "clip {i}, target {target}, start {start}: {n} trials");
                    // Never more than a hair below the target, never far above it.
                    assert!(miss > -0.05, "clip {i}, target {target}, start {start}: {miss:+.3} VMAF");
                    assert!(off > -0.8 && off < 0.15, "clip {i}, target {target}, start {start}: {off:+.2} CRF from the crossing");
                    n
                };
                far.push(run(START_CRF));
                near.extend((-3..=3).map(|d| run((want.round() as i32 + d).clamp(22, 46) as u8)));
            }
        }
        let mean = |n: &[usize]| n.iter().sum::<usize>() as f64 / n.len() as f64;
        eprintln!("trials: {:.2} from CRF {START_CRF} ({} cases), {:.2} from near the answer ({} cases)", mean(&far), far.len(), mean(&near), near.len());
        // (A few of these measure a CRF the failing trials alone would have settled on.)
        assert!(mean(&far) <= 2.35 && mean(&near) <= 2.05);
    }

    #[test]
    fn measured_curves_take_three_trials_down_to_neighbouring_crfs() {
        let mut counts = Vec::new();
        for curve in &MEASURED {
            for (target, _) in measured_targets(curve) {
                counts.push(check(22, 46, target, measured(curve)));
            }
        }
        let mean = counts.iter().sum::<usize>() as f64 / counts.len() as f64;
        eprintln!("trials: {mean:.2} ({} cases), at most {}", counts.len(), counts.iter().max().unwrap());
        assert!(mean <= 3.2 && counts.iter().all(|n| *n <= 5));
    }

    #[test]
    fn the_worst_sample_can_hold_the_crf_down() {
        // Mean 93 at CRF 40; the worst sample is 7 below the mean there and gains 0.9 a step.
        let trial = |crf: u8| Trial { crf: crf as f64, mean_vmaf: 93.0 + (40.0 - crf as f64) * 0.45, min_vmaf: 86.0 + (40.0 - crf as f64) * 0.9, size_ratio: 0.1 };
        let p = |worst_sample| SearchParams { target_vmaf: 93.0, min_crf: 20, max_crf: 55, start_crf: 41, max_size_ratio: None, worst_sample, landing: Landing::SafeSide };
        let find = |p: &SearchParams| find_crossing(&[], p.start_crf, p.min_crf, p.max_crf, p.target_vmaf, |c| Ok(p.score(&trial(c)))).unwrap().0.unwrap().crf;
        assert_eq!(find(&p(None)), 40.0);
        // 89 for the worst sample is reached at CRF 36.67: a quarter step on the safe side.
        assert_eq!(find(&p(Some(89.0))), 36.5);
        // A mark the worst sample already meets changes nothing.
        assert_eq!(find(&p(Some(86.0))), 40.0);
        assert_eq!(p(Some(89.0)).score(&trial(40)), 90.0);
        assert_eq!(p(Some(89.0)).score(&trial(30)), 97.5);
    }

    #[test]
    fn known_scores_spare_trials() {
        let f = |c: u8| 100.0 - 0.012 * (c as f64 - 10.0).powi(2);
        let run = |known: &[(u8, f64)], start: u8, target: f64| {
            let mut asked = Vec::new();
            let (got, _) = find_crossing(known, start, 20, 55, target, |c| {
                asked.push(c);
                Ok(f(c))
            })
            .unwrap();
            (got.map(|c| c.crf), asked)
        };
        let (fresh, asked) = run(&[], 35, 93.0);
        let fresh = fresh.unwrap();
        assert!(asked.len() >= 2, "{asked:?}");
        // The same search again, knowing its own trials, measures nothing.
        let known: Vec<(u8, f64)> = asked.iter().map(|c| (*c, f(*c))).collect();
        let (again, asked_again) = run(&known, 35, 93.0);
        assert_eq!(again, Some(fresh));
        assert!(asked_again.is_empty(), "{asked_again:?}");
        // Another target between known scores starts from them, not from the guess.
        let (got, asked) = run(&[(30, f(30)), (40, f(40))], 22, 92.0);
        assert!(!asked.contains(&22), "{asked:?}");
        assert!(asked.len() <= 1, "{asked:?}");
        let x = got.unwrap();
        assert!(f(x.floor() as u8) >= 92.0 && (30.0..40.0).contains(&x), "{x}");
        // A known pass far below: the guess still goes first, and beyond the pass.
        let (_, asked) = run(&[(20, f(20))], 36, 91.0);
        assert_eq!(asked[0], 36, "{asked:?}");
        // A guess the known scores already rule out is not measured.
        let (_, asked) = run(&[(40, f(40))], 36, 85.0);
        assert!(asked.iter().all(|c| *c > 40), "{asked:?}");
        // Known scores outside the allowed range are left out.
        let (got, asked) = run(&[(10, 99.9), (60, 50.0)], 35, 93.0);
        assert_eq!((got, asked.len()), (Some(fresh), 2));
    }

    #[test]
    fn trials_are_remembered_per_file_clips_and_settings() {
        let dir = std::env::temp_dir().join(format!("av1opt-trials-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let video = dir.join("a \"video\".mkv");
        std::fs::write(&video, b"12345").unwrap();
        let store = dir.join("trials-v2.jsonl");
        let clips = [(10.0, 8.0), (50.0, 8.0)];
        let enc = EncodeSettings { preset: 6, pix_fmt_out: "yuv420p10le", params: "tune=0".into(), keyint_frames: None };
        let open = |clips: &[(f64, f64)], encoder: &str, reuse: bool| TrialCache::open_at(store.clone(), &video, 5, clips, encoder, reuse);

        let mut c = open(&clips, "ffmpeg 1", true);
        assert!(c.get(&enc, 35.0).is_none());
        c.put(&enc, Trial { crf: 35.0, mean_vmaf: 94.5, min_vmaf: 92.0, size_ratio: 0.4 });
        c.put(&enc, Trial { crf: 38.0, mean_vmaf: 93.5, min_vmaf: 91.0, size_ratio: 0.3 });
        c.put(&enc, Trial { crf: 38.5, mean_vmaf: 93.2, min_vmaf: 90.5, size_ratio: 0.28 });
        assert_eq!(c.get(&enc, 35.0).unwrap().mean_vmaf, 94.5);

        // The next run knows them; the whole CRFs come back in order.
        let c = open(&clips, "ffmpeg 1", true);
        assert_eq!(c.get(&enc, 38.5).unwrap().size_ratio, 0.28);
        assert_eq!(c.whole(&enc).iter().map(|t| t.crf).collect::<Vec<_>>(), [35.0, 38.0]);
        // Other settings, other clips, another ffmpeg: measured again.
        assert!(c.get(&EncodeSettings { preset: 4, ..enc.clone() }, 35.0).is_none());
        assert!(c.get(&EncodeSettings { params: "tune=0:enable-qm=1".into(), ..enc.clone() }, 35.0).is_none());
        assert!(c.get(&EncodeSettings { pix_fmt_out: "yuv420p", ..enc.clone() }, 35.0).is_none());
        assert!(open(&[(10.0, 8.0)], "ffmpeg 1", true).get(&enc, 35.0).is_none());
        assert!(open(&clips, "ffmpeg 2", true).get(&enc, 35.0).is_none());
        // Asked to start fresh: nothing is known, but what is measured is kept.
        let mut fresh = open(&clips, "ffmpeg 1", false);
        assert!(fresh.get(&enc, 35.0).is_none());
        fresh.put(&enc, Trial { crf: 35.0, mean_vmaf: 94.6, min_vmaf: 92.0, size_ratio: 0.4 });
        assert_eq!(open(&clips, "ffmpeg 1", true).get(&enc, 35.0).unwrap().mean_vmaf, 94.6);
        // A file that changed is another file.
        std::fs::write(&video, b"123456").unwrap();
        assert!(TrialCache::open_at(store.clone(), &video, 6, &clips, "ffmpeg 1", true).get(&enc, 35.0).is_none());
        // A cache that can't be written still works for the run.
        let mut off = TrialCache::off();
        off.put(&enc, Trial { crf: 30.0, mean_vmaf: 96.0, min_vmaf: 95.0, size_ratio: 0.6 });
        assert!(off.get(&enc, 30.0).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_damaged_line_leaves_the_other_trials_and_another_way_of_measuring_starts_over() {
        let dir = std::env::temp_dir().join(format!("av1opt-trial-lines-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (video, store) = (dir.join("v.mkv"), dir.join("t.jsonl"));
        std::fs::write(&video, b"12345").unwrap();
        let enc = EncodeSettings { preset: 6, pix_fmt_out: "yuv420p10le", params: "tune=0".into(), keyint_frames: None };
        let clips = [(1.0, 1.0)];
        let open = || TrialCache::open_at(store.clone(), &video, 5, &clips, "ffmpeg 1", true);
        let mut c = open();
        c.put(&enc, Trial { crf: 35.0, mean_vmaf: 92.0, min_vmaf: 85.0, size_ratio: 0.4 });
        c.put(&enc, Trial { crf: 36.0, mean_vmaf: 91.0, min_vmaf: 84.0, size_ratio: 0.3 });
        let good = std::fs::read(&store).unwrap();
        let cut = good.iter().position(|b| *b == b'\n').unwrap() + 1;
        let damaged = [&good[..cut], b"\xff not text\n", &good[cut..]].concat();
        std::fs::write(&store, damaged).unwrap();
        let c = open();
        assert!(c.get(&enc, 35.0).is_some() && c.get(&enc, 36.0).is_some());

        // The same trials, as a former way of measuring wrote them, count for nothing.
        let now = format!("|measured {MEASURING_VERSION}");
        let earlier = String::from_utf8(good).unwrap().replace(&now, "|measured 0");
        assert!(!earlier.contains(&now), "{earlier}");
        std::fs::write(&store, earlier).unwrap();
        assert!(open().get(&enc, 35.0).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn expectations_follow_the_two_trials() {
        let a = Trial { crf: 30.0, mean_vmaf: 94.0, min_vmaf: 92.0, size_ratio: 0.40 };
        let b = Trial { crf: 34.0, mean_vmaf: 92.0, min_vmaf: 89.0, size_ratio: 0.25 };
        let mid = expected(&a, &b, 32.0);
        assert!((mid.mean_vmaf - 93.0).abs() < 1e-9 && (mid.min_vmaf - 90.5).abs() < 1e-9);
        // Sizes shrink by a constant factor per step: the geometric mean halfway.
        assert!((mid.size_ratio - (0.40f64 * 0.25).sqrt()).abs() < 1e-9);
        // Outside the two trials the line carries on.
        let out = expected(&a, &b, 29.0);
        assert!((out.mean_vmaf - 94.5).abs() < 1e-9 && out.size_ratio > 0.40);
        // A measured CRF is reported as measured.
        let same = expected(&a, &a, 30.0);
        assert_eq!((same.crf, same.mean_vmaf, same.size_ratio), (30.0, 94.0, 0.40));
    }

    #[test]
    fn params_are_tidied_and_checked() {
        assert_eq!(parse_params(" tune=0 : enable-qm=1:qm-min=0: ").unwrap(), "tune=0:enable-qm=1:qm-min=0");
        assert_eq!(parse_params("").unwrap(), "");
        assert_eq!(parse_params("keyint=10s:ac-bias=1.5").unwrap(), "keyint=10s:ac-bias=1.5");
        for bad in ["tune", "crf=30", "tune=0:tune=1", "a=b c", "x='1'", "=1", "preset=4", "tune=0,enable-qm=1"] {
            assert!(parse_params(bad).is_err(), "{bad}");
        }
        let enc = |params: &str| EncodeSettings { preset: 6, pix_fmt_out: "yuv420p10le", params: params.into(), keyint_frames: None };
        assert_eq!(enc("tune=0").video_args(35.3).last().unwrap(), "crf=35.25:tune=0:keyint=10s:enable-qm=1");
        // What the user's own parameters name is left alone.
        assert_eq!(enc("tune=0:keyint=5s").video_args(35.0).last().unwrap(), "crf=35:tune=0:keyint=5s:enable-qm=1");
        assert_eq!(enc("enable-qm=0:tune=0").video_args(35.0).last().unwrap(), "crf=35:enable-qm=0:tune=0:keyint=10s");
        assert_eq!(enc("").video_args(35.0).last().unwrap(), "crf=35:keyint=10s:enable-qm=1");
        // The CRF comes before a parameter it would otherwise set back.
        assert_eq!(enc("aq-mode=1").video_args(35.0).last().unwrap(), "crf=35:aq-mode=1:keyint=10s:enable-qm=1");
    }

    #[test]
    fn a_source_whose_frames_come_slower_than_it_says_has_its_key_frame_gap_counted_in_frames() {
        let steady = MediaInfo { fps: 29.97, nominal_fps: 29.97, ..MediaInfo::default() };
        assert_eq!(keyint_frames(&steady), None);
        let settings = EncodeSettings::for_source(&steady, 6, "tune=0");
        assert_eq!(settings.video_args(35.0).last().unwrap(), "crf=35:tune=0:keyint=10s:enable-qm=1");

        // Declared at 60, frames at 30: in seconds the encoder would count 60 to a second.
        let uneven = MediaInfo { fps: 30.0, nominal_fps: 60.0, vfr: true, ..MediaInfo::default() };
        assert_eq!(keyint_frames(&MediaInfo { fps: 29.97, ..uneven.clone() }), Some(300));
        assert_eq!(keyint_frames(&MediaInfo { fps: 23.976, ..uneven.clone() }), Some(240));
        let settings = EncodeSettings::for_source(&uneven, 6, "tune=0");
        assert_eq!(settings.video_args(35.0).last().unwrap(), "crf=35:tune=0:keyint=300:enable-qm=1");
        // A gap the user sets is still theirs, and the CRF still comes first.
        let own = EncodeSettings::for_source(&uneven, 6, "keyint=5s");
        assert_eq!(own.video_args(35.0).last().unwrap(), "crf=35:keyint=5s:enable-qm=1");
        assert_eq!(EncodeSettings::for_source(&uneven, 6, "").video_args(35.0).last().unwrap(), "crf=35:keyint=300:enable-qm=1");
    }

    #[test]
    fn rejects_inverted_range() {
        assert!(find_crf(START_CRF, 40, 30, 93.0, |_| Ok(95.0)).is_err());
    }

    #[test]
    fn single_value_range() {
        let (got, tried) = find_crf(START_CRF, 30, 30, 93.0, |_| Ok(94.0)).unwrap();
        assert_eq!((got, tried.len()), (Some(30), 1));
    }

    fn info_with_fps(fps: f64) -> MediaInfo {
        MediaInfo {
            video_index: 0, codec: "h264".into(), width: 1920, height: 1080, fps, duration_s: 60.0,
            video_bitrate: 5e6, pix_fmt: "yuv420p".into(), hdr: false, dolby_vision: false, color_transfer: String::new(), interlaced: false, vfr: false,
            start_time: 0.0, video_start: 0.0, covers: vec![], attachments: 0, tags: Default::default(), video_s: 0.0, nominal_fps: 0.0, time_base: (0, 0),
            audio_streams: 0, subtitle_streams: 0, audio: vec![], subtitle_codecs: vec![], subtitle_dispositions: vec![], other_streams: vec![],
            ..Default::default()
        }
    }

    #[test]
    fn a_clip_is_placed_by_the_frames_of_its_file() {
        let place = |start, rate, first| clip_place(start, rate, first);
        // 3.3 s at 30 fps is frame 99: the seek goes two seconds before, a quarter of a frame
        // before frame 39, and the clip starts a quarter of a frame before frame 99.
        let p = place(3.3, 30.0, 0.0);
        assert!((p.seek.unwrap() - 38.75 / 30.0).abs() < 1e-9 && (p.from - 98.75 / 30.0).abs() < 1e-9, "{p:?}");
        // Near the first frame there is nothing to seek past.
        let p = place(0.5, 25.0, 0.0);
        assert!(p.seek.is_none() && (p.from - 12.75 / 25.0).abs() < 1e-9, "{p:?}");
        assert!(place(0.0, 30.0, 0.0).from < 0.0);
        // Sound starts two seconds before the picture: 3 s of the file is 1 s of the picture,
        // and was cut as if it were two (the frames a run-in would have seen were not there).
        let p = place(3.0, 30.0, 2.0);
        assert!(p.seek.is_none() && (p.from - (2.0 + 29.75 / 30.0)).abs() < 1e-9, "{p:?}");
        // Before the picture: from its first frame.
        assert!((place(1.0, 30.0, 2.0).from - (2.0 - 0.25 / 30.0)).abs() < 1e-9);
        // A clip just past the run-in seeks to the very start, a quarter of a frame early.
        let p = place(2.004, 30.0, 0.0);
        assert!(p.seek.is_none() && (p.from - 59.75 / 30.0).abs() < 1e-9, "{p:?}");
        // Without a rate there is no grid to go by.
        assert_eq!(place(5.0, 0.0, 0.0), ClipPlace { seek: Some(3.0), from: 5.0 });
    }

    /// The hash of every frame of `path`, in order.
    fn frame_hashes(ffmpeg: &str, path: &Path, filter: Option<&str>) -> Vec<String> {
        let mut c = std::process::Command::new(ffmpeg);
        c.args(["-v", "error", "-nostdin", "-i"]).arg(path).args(["-map", "0:v:0", "-fps_mode", "passthrough", "-pix_fmt", "yuv420p"]);
        if let Some(f) = filter {
            c.args(["-vf", f]);
        }
        let out = c.args(["-f", "framemd5", "-"]).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).lines().filter(|l| !l.starts_with('#')).filter_map(|l| Some(l.rsplit(',').next()?.trim().to_string())).collect()
    }

    #[test]
    fn a_clip_holds_the_frames_a_decode_from_the_start_finds() {
        let Some(tools) = crate::tools::real_tools("a_clip_holds_the_frames_a_decode_from_the_start_finds") else { return };
        let ffmpeg = std::env::var("AV1OPT_FFMPEG").unwrap();
        let dir = std::env::temp_dir().join(format!("av1opt-clips-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A transport stream has no index, and a seek in it lands on the next key frame: with
        // two seconds between them inside the run-in, with eight past the clip altogether.
        // Sound 0.3 s ahead of the picture puts the picture's first frame that far into a
        // transport stream.
        for (name, gop, format, sound_first) in [("two.ts", "50", "mpegts", false), ("eight.ts", "200", "mpegts", false), ("plain.mkv", "50", "matroska", false), ("late.ts", "50", "mpegts", true)] {
            let path = dir.join(name);
            let mut made = std::process::Command::new(&ffmpeg);
            made.args(["-v", "error", "-nostdin", "-y", "-itsoffset", if sound_first { "0.3" } else { "0" }, "-f", "lavfi", "-i", "testsrc2=s=160x120:r=25:d=16"]);
            if sound_first {
                made.args(["-f", "lavfi", "-i", "sine=d=16", "-c:a", "aac"]);
            }
            let made = made
                .args(["-c:v", "libx264", "-preset", "ultrafast", "-bf", "2"])
                .args(["-pix_fmt", "yuv420p", "-g", gop, "-keyint_min", gop, "-sc_threshold", "0", "-f", format])
                .arg(&path)
                .output()
                .unwrap();
            assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));
            let info = crate::probe::probe(&tools.ffprobe, &path, std::fs::metadata(&path).unwrap().len()).unwrap();
            assert_eq!(first_frame(&info) > 0.2, sound_first, "{name}: {info:?}");
            let all = frame_hashes(&ffmpeg, &path, None);
            assert_eq!(all.len(), 400, "{name}");
            for start in [0.0, 1.0, 2.2, 3.32, 9.0, 15.0] {
                let clip = dir.join("clip.mkv");
                let frames = extract_reference(&tools, &path, &info, true, first_frame(&info), info.start_time, start, 1.0, &clip).unwrap() as usize;
                let first = ((clip_place(start, 25.0, first_frame(&info)).from - first_frame(&info)) * 25.0).ceil() as usize;
                let got = frame_hashes(&ffmpeg, &clip, None);
                assert_eq!(got.len(), frames, "{name} at {start}");
                assert_eq!(got, all[first..(first + 25).min(all.len())], "{name} at {start}: not frames {first} on");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn seeks_stay_clear_of_frame_edges() {
        for fps in [23.976, 25.0, 30.0, 59.94] {
            for first in [0.0, 0.021, 0.5] {
                for i in 0..400 {
                    let t = i as f64 * 0.0137;
                    let s = snap_seek(t, fps, first);
                    assert!(s >= 0.0 && (s - t).abs() <= 0.75 / fps + 1e-9, "{t} -> {s} at {fps}");
                    if s > 0.0 {
                        // A quarter of a frame before a frame: nowhere near a frame or the
                        // middle between two, where a millisecond decides.
                        let phase = ((s - first) * fps).rem_euclid(1.0);
                        assert!((phase - 0.75).abs() < 1e-6, "{t} -> {s} at {fps}: phase {phase}");
                    }
                }
            }
        }
        // The very start stays the very start, and nonsense rates change nothing.
        assert_eq!(snap_seek(0.0, 30.0, 0.0), 0.0);
        assert_eq!(snap_seek(0.004, 30.0, 0.0), 0.0);
        assert_eq!(snap_seek(3.21, 0.0, 0.0), 3.21);
        // 2.2167 s is frame 66.5 at 30 fps: moved to a quarter before frame 67.
        assert!((snap_seek(2.2167 - 2.0, 30.0, 0.0) - 6.75 / 30.0).abs() < 1e-9);
    }

    #[test]
    fn cfr_picks_closest_standard_rate() {
        for (fps, want) in [
            (24.0, "24"), (23.976, "24000/1001"), (23.98, "24000/1001"), (25.0, "25"),
            (29.97, "30000/1001"), (30.0, "30"), (29.999, "30"), (59.94, "60000/1001"),
            (60.0, "60"), (59.997, "60"), (12.0, "12.000"), (120.0, "120.000"),
        ] {
            assert_eq!(cfr(&info_with_fps(fps)), want, "fps {fps}");
        }
    }

    #[test]
    fn cfr_goes_by_the_declared_rate_of_a_steady_stream() {
        let info = |fps, nominal_fps, vfr| MediaInfo { nominal_fps, vfr, ..info_with_fps(fps) };
        // 30 fps declared, 29.96 on average: the average alone is nearest to 30000/1001.
        assert_eq!(cfr(&info_with_fps(29.96)), "30000/1001");
        assert_eq!(cfr(&info(29.96, 30.0, false)), "30");
        assert_eq!(cfr(&info(29.89, 30.0, false)), "30");
        assert_eq!(cfr(&info(23.976, 24000.0 / 1001.0, false)), "24000/1001");
        // A variable-rate stream declares whatever fits all its frame times: not a grid.
        assert_eq!(cfr(&info(27.3, 120.0, true)), "27.300");
        assert_eq!(cfr(&info(29.97, 90000.0, true)), "30000/1001");
        // As a message names them.
        assert_eq!([23.976, 30.0, 27.3, 59.94].map(|f| cfr_label(&info_with_fps(f))), ["23.98", "30", "27.3", "59.94"]);
    }

    #[test]
    fn source_filters_fix_odd_sizes_and_interlacing() {
        let mut i = info_with_fps(25.0);
        assert!(source_filters(&i).is_empty());
        (i.width, i.height, i.interlaced) = (641, 361, true);
        assert_eq!(source_filters(&i), ["bwdif=mode=send_frame", "crop=640:360:0:0"]);
        // RGB is made YUV by the BT.709 matrix, or BT.2020's where its primaries are those.
        let rgb = |pix_fmt: &str, color_space: &str, color_primaries: &str| MediaInfo {
            pix_fmt: pix_fmt.into(),
            color_space: color_space.into(),
            color_primaries: color_primaries.into(),
            ..info_with_fps(25.0)
        };
        assert_eq!(source_filters(&rgb("gbrp", "gbr", "")), ["scale=out_color_matrix=bt709:out_range=tv"]);
        assert_eq!(source_filters(&rgb("rgb24", "", "")), ["scale=out_color_matrix=bt709:out_range=tv"]);
        assert_eq!(source_filters(&rgb("gbrp10le", "gbr", "bt2020")), ["scale=out_color_matrix=bt2020:out_range=tv"]);
        assert!(source_filters(&rgb("yuv444p", "bt709", "bt709")).is_empty());
        assert!(source_filters(&rgb("gray", "", "")).is_empty());
    }

    #[test]
    fn full_range_sources_are_cut_in_their_own_range_and_limited_ones_as_before() {
        let source = |pix_fmt: &str, color_range: &str| MediaInfo { pix_fmt: pix_fmt.into(), color_range: color_range.into(), ..info_with_fps(25.0) };
        // Limited range, tagged or not: the cut's arguments and filters are what they were.
        for (pix_fmt, range) in [("yuv420p", ""), ("yuv420p", "tv"), ("yuv420p", "unknown"), ("yuv422p", "tv"), ("yuv420p10le", "tv"), ("yuv444p10le", "")] {
            let s = source(pix_fmt, range);
            assert!(!full_range(&s), "{pix_fmt} {range}");
            assert_eq!(reference_format(&s), ["-pix_fmt", reference_pix_fmt(&s)], "{pix_fmt} {range}");
            assert!(source_filters(&s).is_empty(), "{pix_fmt} {range}");
        }
        // Full range by the tag, whatever the format's name, and by `yuvj` where there is no tag.
        for (pix_fmt, range, cut_as) in [
            ("yuvj420p", "pc", "yuv420p"),
            ("yuvj420p", "", "yuv420p"),
            ("yuv420p", "pc", "yuv420p"),
            ("yuvj422p", "pc", "yuv420p"),
            ("yuvj444p", "unknown", "yuv420p"),
            ("yuv420p10le", "pc", "yuv420p10le"),
            ("yuv422p10le", "pc", "yuv420p10le"),
        ] {
            let s = source(pix_fmt, range);
            assert!(full_range(&s), "{pix_fmt} {range}");
            assert_eq!(reference_format(&s), ["-pix_fmt", cut_as, "-color_range", "pc"], "{pix_fmt} {range}");
            // Made 4:2:0 before the encoder sees it, as the cut is, unless it is already.
            let made_as_the_cut = (pix_fmt != cut_as && pix_fmt != "yuvj420p").then(|| format!("format={cut_as}"));
            assert_eq!(source_filters(&s), Vec::from_iter(made_as_the_cut), "{pix_fmt} {range}");
        }
        // A tag says what it says: yuvj marked limited is limited.
        assert!(!full_range(&source("yuvj420p", "tv")));
        // RGB is made limited range by its own filter, whatever its tag.
        let rgb = MediaInfo { color_space: "gbr".into(), ..source("gbrp", "pc") };
        assert!(!full_range(&rgb));
        assert_eq!(source_filters(&rgb), ["scale=out_color_matrix=bt709:out_range=tv"]);
    }

    #[test]
    fn sample_positions_stay_inside_video() {
        for dur in [0.5, 3.0, 10.0, 45.0, 600.0, 7200.0] {
            let pos = sample_positions(dur, 4, 8.0);
            assert!(!pos.is_empty());
            for (start, len) in pos {
                assert!(start >= 0.0 && start + len <= dur + 1e-9, "dur {dur}: {start}+{len}");
            }
        }
        assert!(sample_positions(0.0, 4, 8.0).is_empty());
    }

    #[test]
    fn samples_below_the_mark_count_as_the_mark() {
        let scores = [96.0, 95.0, 93.0, 90.0, 86.0, 80.0];
        assert_eq!(lifted_mean(&scores, None), 90.0);
        // 90 and below count as 90.5: (96 + 95 + 93 + 90.5 * 3) / 6.
        assert!((lifted_mean(&scores, Some(90.5)) - 92.583333).abs() < 1e-5);
        // Nothing below the mark: the plain mean.
        assert_eq!(lifted_mean(&scores, Some(70.0)), 90.0);
        assert_eq!(lifted_mean(&[], Some(90.0)), 0.0);
        // A mean counted that way is not taken for a plain one from an earlier run.
        let dir = std::env::temp_dir().join(format!("av1opt-lift-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let video = dir.join("v.mkv");
        std::fs::write(&video, b"12345").unwrap();
        let enc = EncodeSettings { preset: 6, pix_fmt_out: "yuv420p10le", params: "tune=0".into(), keyint_frames: None };
        let clips = [(1.0, 1.0)];
        let mut plain = TrialCache::open_at(dir.join("t.jsonl"), &video, 5, &clips, "ffmpeg 1", true);
        plain.put(&enc, Trial { crf: 35.0, mean_vmaf: 92.0, min_vmaf: 85.0, size_ratio: 0.4 });
        assert!(TrialCache::open_at(dir.join("t.jsonl"), &video, 5, &clips, "ffmpeg 1", true).get(&enc, 35.0).is_some());
        assert!(TrialCache::open_at(dir.join("t.jsonl"), &video, 5, &clips, "ffmpeg 1|lifted to 90.5", true).get(&enc, 35.0).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_longer_video_gets_more_samples() {
        // Eight up to a quarter of an hour, then one for every two minutes more.
        assert_eq!([10.0, 600.0, 900.0, 1019.0].map(sample_count), [8; 4]);
        assert_eq!([1021.0, 1800.0, 3600.0, 3840.0, 7200.0, 36000.0].map(sample_count), [9, 15, 30, 32, 32, 32]);
        // No more than two minutes and a bit from one sample to the next, up to an hour.
        for dur in [900.0, 1500.0, 2400.0, 3600.0] {
            let pos = sample_positions(dur, sample_count(dur), SAMPLE_SECS);
            assert_eq!(pos.len(), sample_count(dur));
            assert!(pos.windows(2).all(|w| w[1].0 - w[0].0 < 125.0), "{dur}: {pos:?}");
        }
        // A short one gets what fits, with as much between two samples as they are long.
        assert_eq!(sample_positions(20.0, sample_count(20.0), SAMPLE_SECS).len(), 2);
        // For the mean alone: many short clips over the whole length, apart from each other.
        assert_eq!([64.0, 240.0, 512.0, 900.0, 7200.0].map(mean_sample_count), [8, 30, 64, 64, 64]);
        for dur in [64.0, 100.0, 900.0, 7200.0] {
            let pos = mean_sample_positions(dur, mean_sample_count(dur), MEAN_SAMPLE_SECS);
            assert_eq!(pos.len(), mean_sample_count(dur).min((dur / 2.0) as usize).max(1), "{dur}");
            assert!(pos[0].0 >= 0.0 && pos.last().unwrap().0 + pos.last().unwrap().1 <= dur + 1e-9, "{dur}");
            assert!(pos.windows(2).all(|w| w[0].0 + w[0].1 <= w[1].0), "{dur}: clips overlap");
            // Each clip sits in the middle of its share.
            let share = (dur - pos[0].1) / pos.len() as f64;
            assert!(pos.iter().enumerate().all(|(i, p)| (p.0 - share * (i as f64 + 0.5)).abs() < 1e-9), "{dur}");
        }
    }

    #[test]
    fn a_short_video_is_sampled_whole() {
        for dur in [0.4, 1.0, 3.5, 10.0, 47.3, 63.9] {
            let pos = mean_sample_positions(dur, mean_sample_count(dur), MEAN_SAMPLE_SECS);
            assert_eq!(pos.len(), (dur as usize).max(1), "{dur}");
            // Side by side from the first frame to the last, nothing left over.
            assert_eq!(pos[0].0, 0.0, "{dur}");
            assert!(pos.windows(2).all(|w| (w[0].0 + w[0].1 - w[1].0).abs() < 1e-9), "{dur}: {pos:?}");
            let (start, secs) = pos[pos.len() - 1];
            assert!((start + secs - dur).abs() < 1e-9, "{dur}: {pos:?}");
            // Clips of a second at least, so that each still says something of its own.
            assert!(dur < 1.0 || pos.iter().all(|p| p.1 >= MEAN_SAMPLE_SECS), "{dur}: {pos:?}");
        }
    }

    #[test]
    fn clips_reach_both_ends_of_the_video() {
        // The first and last 5% used to be left out: 45 s at each end of this one.
        let pos = sample_positions(900.0, 8, 4.0);
        let share = (900.0 - 4.0) / 8.0;
        assert_eq!(pos.len(), 8);
        assert!(pos.iter().enumerate().all(|(i, p)| (p.0 - share * (i as f64 + 0.5)).abs() < 1e-9), "{pos:?}");
        assert!(pos[0].0 < 45.0 * 2.0 && pos[7].0 + 4.0 > 900.0 - 45.0 * 2.0, "{pos:?}");
    }

    #[test]
    fn the_answer_is_the_quarter_nearest_the_true_crossing_and_stays_between_the_trials() {
        // Curved as the measured curves are: the slope grows by 0.03 VMAF with each CRF step.
        let f = |c: f64| 112.0 - 0.15 * c - 0.015 * c * c;
        let crossing = |target: f64| (-0.15 + (0.15f64 * 0.15 + 0.06 * (112.0 - target)).sqrt()) / 0.03;
        let (lo, hi) = (32u8, 35u8);
        let tried: BTreeMap<u8, f64> = [(lo, f(lo as f64)), (hi, f(hi as f64))].into();
        let mut below = 0;
        for i in 1..60 {
            let target = f(hi as f64) + (f(lo as f64) - f(hi as f64)) * i as f64 / 60.0;
            let Step::Done(Some(c)) = next_step(&tried, 20, 55, target, Landing::Nearest) else { panic!("target {target}: not settled") };
            let want = crossing(target);
            assert!(c.crf >= lo as f64 && c.crf <= hi as f64 - 0.25, "target {target}: {c:?}");
            assert_eq!(c.crf * 4.0, (c.crf * 4.0).round(), "{c:?}");
            // The clamp keeps a crossing right at the failing trial a quarter step below it.
            let room = (want - (hi as f64 - 0.25)).max(0.0);
            assert!((c.crf - want).abs() <= 0.125 + 0.01 + room, "target {target}: {} for a crossing at {want:.3}", c.crf);
            below += usize::from(c.crf < want);
        }
        // As often on one side as on the other, where flooring was below it every time.
        assert!((20..=40).contains(&below), "{below} of 59 below the crossing");
    }

    #[test]
    fn a_crossing_is_never_settled_on_a_crf_that_failed() {
        // Both trials fail, and the first only just: the crossing rounds to it.
        let tried: BTreeMap<u8, f64> = [(30, 92.99), (32, 92.0)].into();
        let Step::Done(Some(c)) = next_step(&tried, 20, 55, 93.0, Landing::Nearest) else { panic!("not settled") };
        assert_eq!(c.crf, 29.75);
    }

    #[test]
    fn with_only_failing_trials_a_far_answer_is_measured_first() {
        // 0.5 a step: 40 and 38 fail by 2.5 and 1.5, so the line crosses at about 35, three steps
        // below anything measured.
        let f = |c: u8| 93.0 + (35.0 - c as f64) * 0.5;
        let mut asked = Vec::new();
        let (got, tried) = find_crossing(&[(40, f(40))], 38, 20, 55, 93.0, |c| {
            asked.push(c);
            Ok(f(c))
        })
        .unwrap();
        assert!(asked.len() >= 2, "{asked:?}");
        let got = got.unwrap();
        // It rests on a CRF that passed, or on one right next to what was measured.
        let lowest = tried.iter().map(|t| t.0).min().unwrap() as f64;
        assert!(got.crf >= lowest - 1.0, "{got:?} {tried:?}");
        assert!(tried.iter().any(|t| t.1 >= 93.0), "no passing trial: {tried:?}");
        // Close by, the answer is still taken from the two failing trials: nothing to gain.
        let tried: BTreeMap<u8, f64> = [(36, 92.9), (37, 92.4)].into();
        assert!(matches!(next_step(&tried, 20, 55, 93.0, Landing::SafeSide), Step::Done(Some(_))));
    }

    /// Mean of what `one` gives for each target and start on the measured curves, the way the
    /// reviewer's simulation went over them.
    fn over_measured(landing: Landing) -> (f64, f64, f64) {
        let (mut off, mut over, mut trials, mut cases) = (0.0, 0.0, 0.0, 0.0);
        for curve in &MEASURED {
            let f = measured(curve);
            for target in (0..=45).map(|i| 88.0 + i as f64 * 0.2) {
                let Some(want) = crossing(22, 46, target, f) else { continue };
                if !(23.0..=45.0).contains(&want) {
                    continue;
                }
                let near = (-3..=3).map(|d| (want.round() as i32 + d).clamp(22, 46) as u8);
                for start in std::iter::once(START_CRF).chain(near) {
                    let (got, tried) = find_crossing_landing(&[], start, 22, 46, target, landing, |c| Ok(f(c))).unwrap();
                    let got = got.expect("reachable").crf;
                    let lo = got.floor() as u8;
                    let score = if lo >= 46 { f(46) } else { f(lo) + (f(lo + 1) - f(lo)) * (got - lo as f64) };
                    off += got - want;
                    over += score - target;
                    trials += tried.len() as f64;
                    cases += 1.0;
                }
            }
        }
        (off / cases, over / cases, trials / cases)
    }

    #[test]
    fn on_measured_curves_the_nearest_landing_has_no_lean() {
        let (safe_off, safe_over, _) = over_measured(Landing::SafeSide);
        let (off, over, trials) = over_measured(Landing::Nearest);
        eprintln!("safe side: {safe_off:+.3} CRF, {safe_over:+.3} VMAF; nearest: {off:+.3} CRF, {over:+.3} VMAF, {trials:.2} trials");
        assert!(safe_off < -0.1 && safe_over > 0.04, "{safe_off} {safe_over}");
        assert!(off.abs() < 0.04 && over.abs() < 0.02, "{off} {over}");
        assert!(trials < 2.5, "{trials}");
    }

    #[test]
    fn the_slope_comes_from_the_two_nearest_trials() {
        let t = |crf: f64, mean_vmaf: f64| Trial { crf, mean_vmaf, min_vmaf: 80.0, size_ratio: 0.3 };
        let trials = [t(30.0, 96.0), t(38.0, 93.0), t(40.0, 92.0)];
        assert!((slope_at(&trials, 38.75) - 0.5).abs() < 1e-9);
        // Nothing to read it from: the usual one.
        assert_eq!(slope_at(&trials[..1], 38.0), VMAF_PER_CRF);
        // Noise cannot make it absurd.
        assert_eq!(slope_at(&[t(38.0, 93.0), t(39.0, 93.5)], 38.5), 0.1);
    }
}
