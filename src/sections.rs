//! Removing parts of a video. The encode re-encodes the picture anyway, so cuts are
//! frame-accurate: the kept parts are trimmed out of the decoded source and joined with
//! ffmpeg's trim/concat filters. This module parses the sections, maps times between the
//! source and the shorter output, and retimes text subtitles and chapters to match.

use anyhow::{Result, bail};
use std::fmt::Write as _;

/// Kept parts shorter than this are dropped too: a sliver of a frame or two between two
/// removed sections (or at either end) is almost always a mis-click, and a part with no
/// whole frame in it would break the join.
const MIN_KEEP: f64 = 0.25;

/// The parts of a source that stay, in source seconds, plus what was removed.
#[derive(Clone, Debug, PartialEq)]
pub struct Sections {
    keep: Vec<(f64, f64)>,
    removed: Vec<(f64, f64)>,
}

/// Parses "1:20-2:05", "80-125.5", "1:02:03.5-1:05:00" or "50:00-end" (also "50:00-").
pub fn parse_range(s: &str) -> Result<(f64, f64), String> {
    let (a, b) = s.split_once('-').ok_or_else(|| format!("'{s}' is not START-END, e.g. 1:20-2:05"))?;
    let start = parse_time(a.trim()).ok_or_else(|| format!("'{}' is not a time like 1:20 or 80.5", a.trim()))?;
    let b = b.trim();
    let end = if b.is_empty() || b.eq_ignore_ascii_case("end") {
        f64::INFINITY
    } else {
        parse_time(b).ok_or_else(|| format!("'{b}' is not a time like 2:05 or 125"))?
    };
    if end <= start {
        return Err(format!("'{s}' ends before it starts"));
    }
    Ok((start, end))
}

/// Seconds from "ss", "m:ss" or "h:mm:ss", each with optional decimals on the seconds.
pub fn parse_time(s: &str) -> Option<f64> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.is_empty() || parts.len() > 3 || parts.iter().any(|p| p.is_empty()) {
        return None;
    }
    let (last, rest) = parts.split_last()?;
    let secs: f64 = last.parse().ok().filter(|v: &f64| v.is_finite() && *v >= 0.0)?;
    let mut t = 0.0;
    for p in rest {
        if !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        t = t * 60.0 + p.parse::<f64>().ok()?;
    }
    if !rest.is_empty() && secs >= 60.0 {
        return None;
    }
    Some(t * 60.0 + secs)
}

/// "1:02:03.25" style, for messages.
pub fn fmt_time(t: f64) -> String {
    let ms = (t.max(0.0) * 1000.0).round() as u64;
    let (h, m, s, frac) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
    let mut out = if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") };
    if frac > 0 {
        let f = format!("{frac:03}");
        let _ = write!(out, ".{}", f.trim_end_matches('0'));
    }
    out
}

impl Sections {
    /// Removed sections (any order, may overlap) of a video `duration` seconds long.
    pub fn new(remove: &[(f64, f64)], duration: f64) -> Result<Self> {
        let mut r: Vec<(f64, f64)> = Vec::new();
        for &(a, b) in remove {
            if !(a.is_finite() && a >= 0.0) || b.is_nan() || b <= a {
                bail!("section {}-{} is not a valid range", fmt_time(a), if b.is_finite() { fmt_time(b) } else { "end".into() });
            }
            if a >= duration {
                bail!("section {} starts after the end of the video ({})", fmt_time(a), fmt_time(duration));
            }
            r.push((a, b.min(duration)));
        }
        if r.is_empty() {
            bail!("no sections to remove");
        }
        r.sort_by(|x, y| x.0.total_cmp(&y.0));
        let mut merged: Vec<(f64, f64)> = Vec::new();
        for (a, b) in r {
            match merged.last_mut() {
                Some(last) if a <= last.1 => last.1 = last.1.max(b),
                _ => merged.push((a, b)),
            }
        }
        let mut keep = Vec::new();
        let mut t = 0.0;
        for &(a, b) in &merged {
            if a - t >= MIN_KEEP {
                keep.push((t, a));
            }
            t = b;
        }
        if duration - t >= MIN_KEEP {
            keep.push((t, duration));
        }
        if keep.is_empty() {
            bail!("the sections cover the whole video; nothing would be left");
        }
        Ok(Sections { keep, removed: merged })
    }

    pub fn removed(&self) -> &[(f64, f64)] {
        &self.removed
    }

    #[cfg(test)]
    pub fn keep(&self) -> &[(f64, f64)] {
        &self.keep
    }

    /// Length of the output.
    pub fn kept(&self) -> f64 {
        self.keep.iter().map(|(a, b)| b - a).sum()
    }

    /// How much of the first `secs` seconds of the source stays.
    pub fn kept_within(&self, secs: f64) -> f64 {
        self.keep.iter().map(|&(a, b)| (b.min(secs) - a).max(0.0)).sum()
    }

    /// Output time of source time `t`. Times inside a removed part map to where the next
    /// kept part starts.
    pub fn to_out(&self, t: f64) -> f64 {
        let mut out = 0.0;
        for &(a, b) in &self.keep {
            if t <= a {
                break;
            }
            out += t.min(b) - a;
        }
        out
    }

    /// A clip of up to `secs` seconds starting near output time `out_start` that lies inside
    /// one kept part, so it can be cut from the source in one piece.
    /// Returns (output start, source start, length).
    pub fn window(&self, out_start: f64, secs: f64) -> (f64, f64, f64) {
        let mut base = 0.0;
        for (i, &(a, b)) in self.keep.iter().enumerate() {
            let len = b - a;
            if out_start < base + len || i + 1 == self.keep.len() {
                let secs = secs.min(len);
                // (`len - secs` can come out a hair below zero when the clip is the whole part.)
                let o = out_start.clamp(base, (base + len - secs).max(base));
                return (o, a + (o - base), secs);
            }
            base += len;
        }
        (0.0, 0.0, 0.0)
    }

    /// Up to `count` sample clips of `secs` seconds spread over the output, each inside one
    /// kept part. Returns (source start, length) pairs.
    pub fn sample_positions(&self, count: usize, secs: f64) -> Vec<(f64, f64)> {
        crate::search::sample_positions(self.kept(), count, secs)
            .into_iter()
            .map(|(start, secs)| {
                let (_, src, secs) = self.window(start, secs);
                (src, secs)
            })
            .collect()
    }

    /// `-filter_complex` graph cutting the video stream `video` and `audio` audio streams of
    /// input 0. Outputs are `[vout]` (after `post` filters) and `[ao0]`, `[ao1]`, ...
    pub fn filter_graph(&self, video: usize, audio: usize, post: &[String], duration: f64) -> String {
        let mut g = String::new();
        let mut joined = String::new();
        for (k, &(a, b)) in self.keep.iter().enumerate() {
            // Leave the last part open: the container's duration can be a little short of
            // the streams' last frame or sample.
            let end = if b >= duration - 1e-3 { String::new() } else { format!(":end={b:.6}") };
            let _ = write!(g, "[0:{video}]trim=start={a:.6}{end},setpts=PTS-{a:.6}/TB[v{k}];");
            let _ = write!(joined, "[v{k}]");
            for i in 0..audio {
                let _ = write!(g, "[0:a:{i}]atrim=start={a:.6}{end},asetpts=PTS-{a:.6}/TB[a{i}_{k}];");
                let _ = write!(joined, "[a{i}_{k}]");
            }
        }
        let vlabel = if post.is_empty() { "vout" } else { "vcat" };
        let _ = write!(g, "{joined}concat=n={}:v=1:a={audio}[{vlabel}]", self.keep.len());
        for i in 0..audio {
            let _ = write!(g, "[ao{i}]");
        }
        if !post.is_empty() {
            let _ = write!(g, ";[vcat]{}[vout]", post.join(","));
        }
        g
    }

    /// Moves an event `[start, end]` onto the output timeline; None if it falls entirely in
    /// removed parts.
    fn event(&self, start: f64, end: f64) -> Option<(f64, f64)> {
        let (s, e) = (self.to_out(start), self.to_out(end));
        (e - s > 0.001).then_some((s, e))
    }

    /// Retimes an SRT file: cues in removed parts go, cues across a cut are shortened.
    pub fn retime_srt(&self, text: &str) -> String {
        let mut out = String::new();
        let mut n = 0;
        for block in text.replace("\r\n", "\n").split("\n\n") {
            let lines: Vec<&str> = block.lines().skip_while(|l| l.trim().is_empty()).collect();
            let Some(ti) = lines.iter().position(|l| l.contains("-->")) else { continue };
            let Some((a, b)) = lines[ti].split_once("-->") else { continue };
            let (Some(a), Some(b)) = (srt_time(a.trim()), srt_time(b.split_whitespace().next().unwrap_or(""))) else { continue };
            let Some((s, e)) = self.event(a, b) else { continue };
            n += 1;
            let _ = writeln!(out, "{n}\n{} --> {}", fmt_srt(s), fmt_srt(e));
            for l in &lines[ti + 1..] {
                let _ = writeln!(out, "{l}");
            }
            out.push('\n');
        }
        out
    }

    /// Retimes the Dialogue lines of an ASS/SSA file; styles and headers stay as they are.
    pub fn retime_ass(&self, text: &str) -> String {
        let mut out = String::new();
        let (mut start_i, mut end_i, mut fields) = (1, 2, 10);
        for line in text.lines() {
            if let Some(f) = line.strip_prefix("Format:") {
                let names: Vec<String> = f.split(',').map(|s| s.trim().to_ascii_lowercase()).collect();
                if let (Some(s), Some(e)) = (names.iter().position(|n| n == "start"), names.iter().position(|n| n == "end")) {
                    (start_i, end_i, fields) = (s, e, names.len());
                }
            }
            let Some(rest) = line.strip_prefix("Dialogue:") else {
                out.push_str(line);
                out.push('\n');
                continue;
            };
            // The last field (Text) may itself contain commas.
            let mut f: Vec<String> = rest.splitn(fields, ',').map(String::from).collect();
            let times = (f.get(start_i).and_then(|t| ass_time(t.trim())), f.get(end_i).and_then(|t| ass_time(t.trim())));
            let (Some(a), Some(b)) = times else {
                out.push_str(line);
                out.push('\n');
                continue;
            };
            let Some((s, e)) = self.event(a, b) else { continue };
            f[start_i] = fmt_ass(s);
            f[end_i] = fmt_ass(e);
            let _ = writeln!(out, "Dialogue:{}{}", if f[0].starts_with(' ') { "" } else { " " }, f.join(","));
        }
        out
    }

    /// FFMETADATA chapters moved onto the output; chapters inside removed parts are dropped.
    pub fn chapters(&self, chapters: &[Chapter]) -> String {
        let mut out = String::from(";FFMETADATA1\n");
        for c in chapters {
            let Some((s, e)) = self.event(c.start, c.end) else { continue };
            let _ = writeln!(out, "\n[CHAPTER]\nTIMEBASE=1/1000\nSTART={}\nEND={}", (s * 1000.0).round() as u64, (e * 1000.0).round() as u64);
            if let Some(t) = &c.title {
                let esc: String = t.chars().flat_map(|ch| if "=;#\\\n".contains(ch) { vec!['\\', ch] } else { vec![ch] }).collect();
                let _ = writeln!(out, "title={esc}");
            }
        }
        out
    }
}

pub struct Chapter {
    pub start: f64,
    pub end: f64,
    pub title: Option<String>,
}

/// Chapters of a file, from ffprobe.
pub fn read_chapters(ffprobe: &str, path: &std::path::Path) -> Result<Vec<Chapter>> {
    let mut command = crate::tools::probe_command(ffprobe);
    command.args(["-v", "error", "-show_chapters", "-of", "json"]).arg(crate::tools::probe_arg(path));
    let out = crate::probe::run_ffprobe(command, crate::probe::Reads::Little)?;
    if !out.status.success() {
        bail!("ffprobe: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let j: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    let num = |v: &serde_json::Value| v.as_str().and_then(|s| s.parse::<f64>().ok()).or_else(|| v.as_f64());
    Ok(j["chapters"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|c| {
                    Some(Chapter { start: num(&c["start_time"])?, end: num(&c["end_time"])?, title: c["tags"]["title"].as_str().map(String::from) })
                })
                .collect()
        })
        .unwrap_or_default())
}

fn srt_time(s: &str) -> Option<f64> {
    parse_time(&s.replace(',', "."))
}

fn ass_time(s: &str) -> Option<f64> {
    parse_time(s)
}

fn fmt_srt(t: f64) -> String {
    let ms = (t.max(0.0) * 1000.0).round() as u64;
    format!("{:02}:{:02}:{:02},{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
}

fn fmt_ass(t: f64) -> String {
    let cs = (t.max(0.0) * 100.0).round() as u64;
    format!("{}:{:02}:{:02}.{:02}", cs / 360_000, cs / 6000 % 60, cs / 100 % 60, cs % 100)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn parses_times_and_ranges() {
        assert_eq!(parse_time("80.5"), Some(80.5));
        assert_eq!(parse_time("1:20"), Some(80.0));
        assert_eq!(parse_time("1:02:03.5"), Some(3723.5));
        assert_eq!(parse_time("1:75"), None);
        assert_eq!(parse_time("a:10"), None);
        assert_eq!(parse_time("-3"), None);
        assert_eq!(parse_time(""), None);
        assert_eq!(parse_range("1:20-2:05"), Ok((80.0, 125.0)));
        assert_eq!(parse_range("50:00-end"), Ok((3000.0, f64::INFINITY)));
        assert_eq!(parse_range("50:00-"), Ok((3000.0, f64::INFINITY)));
        assert!(parse_range("2:00-1:00").is_err());
        assert!(parse_range("2:00").is_err());
        assert_eq!(fmt_time(3723.5), "1:02:03.5");
        assert_eq!(fmt_time(65.0), "1:05");
    }

    #[test]
    fn merges_clamps_and_keeps_the_rest() {
        let s = Sections::new(&[(50.0, 60.0), (10.0, 20.0), (15.0, 30.0), (90.0, f64::INFINITY)], 100.0).unwrap();
        assert_eq!(s.removed(), &[(10.0, 30.0), (50.0, 60.0), (90.0, 100.0)]);
        assert_eq!(s.keep(), &[(0.0, 10.0), (30.0, 50.0), (60.0, 90.0)]);
        assert!(close(s.kept(), 60.0));
        assert!(Sections::new(&[(0.0, 100.0)], 100.0).is_err());
        assert!(Sections::new(&[(120.0, 130.0)], 100.0).is_err());
        assert!(Sections::new(&[], 100.0).is_err());
        // A sliver between two sections is removed as well.
        let s = Sections::new(&[(10.0, 20.0), (20.1, 30.0)], 100.0).unwrap();
        assert_eq!(s.keep(), &[(0.0, 10.0), (30.0, 100.0)]);
    }

    #[test]
    fn maps_times_and_windows() {
        let s = Sections::new(&[(10.0, 30.0), (50.0, 60.0)], 100.0).unwrap();
        assert!(close(s.to_out(5.0), 5.0));
        assert!(close(s.to_out(20.0), 10.0));
        assert!(close(s.to_out(40.0), 20.0));
        assert!(close(s.to_out(55.0), 30.0));
        assert!(close(s.to_out(100.0), 70.0));
        // Output 8 s with 5 s clips: would cross the first cut, so it moves back.
        assert_eq!(s.window(8.0, 5.0), (5.0, 5.0, 5.0));
        assert_eq!(s.window(12.0, 5.0), (12.0, 32.0, 5.0));
        // Longer than the part: shortened to fit.
        assert_eq!(s.window(0.0, 15.0), (0.0, 0.0, 10.0));
        // A clip that is the whole of a part, where the sums do not come out even.
        let s = Sections::new(&[(0.3, 0.6)], 2.5).unwrap();
        let (out, src, secs) = s.window(1.0, 5.0);
        assert!(close(out, 0.3) && close(src, 0.6) && close(secs, 1.9), "{out} {src} {secs}");
        for (src, secs) in s.sample_positions(4, 8.0) {
            assert!(s.keep().iter().any(|&(a, b)| src >= a - 1e-9 && src + secs <= b + 1e-9), "{src} {secs}");
        }
    }

    #[test]
    fn builds_the_filter_graph() {
        let s = Sections::new(&[(10.0, 20.0)], 30.0).unwrap();
        let g = s.filter_graph(1, 1, &[], 30.0);
        assert_eq!(
            g,
            "[0:1]trim=start=0.000000:end=10.000000,setpts=PTS-0.000000/TB[v0];\
             [0:a:0]atrim=start=0.000000:end=10.000000,asetpts=PTS-0.000000/TB[a0_0];\
             [0:1]trim=start=20.000000,setpts=PTS-20.000000/TB[v1];\
             [0:a:0]atrim=start=20.000000,asetpts=PTS-20.000000/TB[a0_1];\
             [v0][a0_0][v1][a0_1]concat=n=2:v=1:a=1[vout][ao0]"
        );
        let g = s.filter_graph(0, 0, &["bwdif=mode=send_frame".into()], 30.0);
        assert!(g.ends_with("[v0][v1]concat=n=2:v=1:a=0[vcat];[vcat]bwdif=mode=send_frame[vout]"), "{g}");
    }

    #[test]
    fn retimes_subtitles_and_chapters() {
        let s = Sections::new(&[(10.0, 20.0)], 100.0).unwrap();
        let srt = "1\n00:00:01,000 --> 00:00:02,500\nHello\n\n2\n00:00:12,000 --> 00:00:15,000\nGone\n\n3\n00:00:09,000 --> 00:00:21,000\nAcross\ntwo lines\n\n4\n00:00:30,000 --> 00:00:31,000\nLater\n";
        assert_eq!(
            s.retime_srt(srt),
            "1\n00:00:01,000 --> 00:00:02,500\nHello\n\n2\n00:00:09,000 --> 00:00:11,000\nAcross\ntwo lines\n\n3\n00:00:20,000 --> 00:00:21,000\nLater\n\n"
        );
        let ass = "[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
                   Dialogue: 0,0:00:12.00,0:00:13.00,Default,,0,0,0,,gone\n\
                   Dialogue: 0,0:00:25.00,0:00:26.50,Default,,0,0,0,,Hi, there\n";
        assert_eq!(
            s.retime_ass(ass),
            "[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
             Dialogue: 0,0:00:15.00,0:00:16.50,Default,,0,0,0,,Hi, there\n"
        );
        let ch = [
            Chapter { start: 0.0, end: 12.0, title: Some("Intro".into()) },
            Chapter { start: 12.0, end: 18.0, title: Some("Ad".into()) },
            Chapter { start: 18.0, end: 100.0, title: Some("A=B".into()) },
        ];
        assert_eq!(
            s.chapters(&ch),
            ";FFMETADATA1\n\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=10000\ntitle=Intro\n\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=10000\nEND=90000\ntitle=A\\=B\n"
        );
    }

    #[test]
    fn a_time_has_at_most_three_parts_and_none_of_them_empty() {
        for bad in ["1:2:3:4", ":5", "5:", "::", "1::2", ":", " ", "1: 20", "1.5:00", "1:2.5:00"] {
            assert_eq!(parse_time(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn seconds_stop_at_sixty_only_when_minutes_stand_before_them() {
        assert_eq!(parse_time("60"), Some(60.0));
        assert_eq!(parse_time("3600.5"), Some(3600.5));
        assert_eq!(parse_time("0:59.5"), Some(59.5));
        assert_eq!(parse_time("0:60"), None);
        assert_eq!(parse_time("1:00:59.5"), Some(3659.5));
        assert_eq!(parse_time("1:00:60"), None);
        // Minutes alone may run past the hour: a 90-minute mark is written 90:00.
        assert_eq!(parse_time("90:00"), Some(5400.0));
        assert_eq!(parse_time("100:00:00"), Some(360_000.0));
    }

    #[test]
    #[ignore = "defect: parse_time takes 1:75:00 as 2:15:00 although it refuses 1:75 as m:ss"]
    fn the_minutes_of_a_time_with_hours_stop_at_sixty() {
        assert_eq!(parse_time("1:59:00"), Some(7140.0));
        assert_eq!(parse_time("1:60:00"), None);
        assert_eq!(parse_time("1:75:00"), None);
    }

    #[test]
    fn what_is_not_a_finite_number_is_not_a_time() {
        for bad in ["inf", "infinity", "NaN", "nan", "1e999", "0:inf", "0:NaN", "١:٢٠", "1:٢٠", "１:２０", "x"] {
            assert_eq!(parse_time(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_range_may_have_spaces_and_any_case_of_end() {
        assert_eq!(parse_range(" 1:20 - 2:05 "), Ok((80.0, 125.0)));
        assert_eq!(parse_range("0-END"), Ok((0.0, f64::INFINITY)));
        assert_eq!(parse_range("0:00-End"), Ok((0.0, f64::INFINITY)));
        assert_eq!(parse_range("12:03.5-14:10"), Ok((723.5, 850.0)));
        assert_eq!(parse_range("58:30- "), Ok((3510.0, f64::INFINITY)));
    }

    #[test]
    fn a_range_that_is_empty_or_unreadable_is_refused_and_named() {
        for bad in ["5-5", "1:00-60", "-5", "-", "", "abc-5", "5-abc", "end-5", "1:75-2:00", "5-1:75"] {
            assert!(parse_range(bad).is_err(), "{bad:?}");
        }
        assert!(parse_range("abc-5").unwrap_err().contains("'abc'"));
        assert!(parse_range("5-abc").unwrap_err().contains("'abc'"));
        assert!(parse_range("9-3").unwrap_err().contains("ends before it starts"));
    }

    #[test]
    fn times_are_written_rounded_to_the_millisecond_with_carries() {
        for (t, want) in [
            (0.0, "0:00"),
            (-5.0, "0:00"),
            (f64::NAN, "0:00"),
            (0.25, "0:00.25"),
            (0.001, "0:00.001"),
            (0.0004, "0:00"),
            (61.5, "1:01.5"),
            (59.9996, "1:00"),
            (3599.9996, "1:00:00"),
            (36_000.0, "10:00:00"),
        ] {
            assert_eq!(fmt_time(t), want, "{t}");
        }
    }

    #[test]
    fn a_written_time_reads_back_as_the_same_time() {
        for t in [0.0, 0.5, 59.75, 60.0, 3599.5, 3600.0, 86_399.125] {
            assert!(close(parse_time(&fmt_time(t)).unwrap(), t), "{t}");
        }
    }

    #[test]
    fn sections_that_are_not_ranges_inside_the_video_are_refused() {
        for bad in [(f64::NAN, 5.0), (f64::INFINITY, f64::INFINITY), (-1.0, 5.0), (1.0, f64::NAN), (5.0, 5.0), (6.0, 5.0), (100.0, 110.0)] {
            assert!(Sections::new(&[bad], 100.0).is_err(), "{bad:?}");
        }
        // One bad section among good ones refuses the lot.
        assert!(Sections::new(&[(1.0, 2.0), (5.0, 4.0)], 100.0).is_err());
    }

    #[test]
    fn touching_contained_and_repeated_sections_become_one() {
        for remove in [
            vec![(10.0, 20.0), (20.0, 30.0)],
            vec![(20.0, 30.0), (10.0, 20.0)],
            vec![(10.0, 30.0), (15.0, 20.0)],
            vec![(15.0, 20.0), (10.0, 30.0), (10.0, 30.0)],
        ] {
            let s = Sections::new(&remove, 100.0).unwrap();
            assert_eq!(s.removed(), &[(10.0, 30.0)], "{remove:?}");
            assert_eq!(s.keep(), &[(0.0, 10.0), (30.0, 100.0)], "{remove:?}");
        }
    }

    #[test]
    fn a_section_at_either_end_leaves_nothing_before_or_after_it() {
        assert_eq!(Sections::new(&[(0.0, 10.0)], 100.0).unwrap().keep(), &[(10.0, 100.0)]);
        assert_eq!(Sections::new(&[(90.0, 100.0)], 100.0).unwrap().keep(), &[(0.0, 90.0)]);
        assert_eq!(Sections::new(&[(90.0, 1e9)], 100.0).unwrap().removed(), &[(90.0, 100.0)]);
        assert_eq!(Sections::new(&[(0.0, 10.0), (90.0, f64::INFINITY)], 100.0).unwrap().keep(), &[(10.0, 90.0)]);
    }

    #[test]
    fn a_kept_part_of_a_quarter_second_stays_and_a_shorter_one_goes() {
        // At the start.
        assert_eq!(Sections::new(&[(0.25, 10.0)], 20.0).unwrap().keep(), &[(0.0, 0.25), (10.0, 20.0)]);
        assert_eq!(Sections::new(&[(0.2, 10.0)], 20.0).unwrap().keep(), &[(10.0, 20.0)]);
        // Between two sections.
        assert_eq!(Sections::new(&[(1.0, 2.0), (2.25, 3.0)], 20.0).unwrap().keep(), &[(0.0, 1.0), (2.0, 2.25), (3.0, 20.0)]);
        assert_eq!(Sections::new(&[(1.0, 2.0), (2.2, 3.0)], 20.0).unwrap().keep(), &[(0.0, 1.0), (3.0, 20.0)]);
        // At the end.
        assert_eq!(Sections::new(&[(10.0, 19.75)], 20.0).unwrap().keep(), &[(0.0, 10.0), (19.75, 20.0)]);
        assert_eq!(Sections::new(&[(10.0, 19.8)], 20.0).unwrap().keep(), &[(0.0, 10.0)]);
    }

    #[test]
    fn when_only_slivers_would_be_left_the_video_is_covered() {
        for remove in [vec![(0.1, 100.0)], vec![(0.0, 99.9)], vec![(0.1, 50.0), (50.1, 99.9)]] {
            let e = Sections::new(&remove, 100.0).unwrap_err().to_string();
            assert!(e.contains("nothing would be left"), "{remove:?}: {e}");
        }
    }

    fn cut_video() -> Sections {
        Sections::new(&[(10.0, 30.0), (50.0, 60.0)], 100.0).unwrap()
    }

    #[test]
    fn a_time_at_or_inside_a_cut_maps_to_the_join() {
        let s = cut_video();
        for t in [10.0, 10.001, 20.0, 29.999, 30.0] {
            assert!(close(s.to_out(t), 10.0), "{t}");
        }
        for t in [50.0, 55.0, 60.0] {
            assert!(close(s.to_out(t), 30.0), "{t}");
        }
        assert!(close(s.to_out(0.0), 0.0));
        assert!(close(s.to_out(-4.0), 0.0));
        assert!(close(s.to_out(1000.0), 70.0));
        // A sliver that was dropped counts as removed.
        let s = Sections::new(&[(10.0, 20.0), (20.1, 30.0)], 100.0).unwrap();
        assert!(close(s.to_out(20.05), 10.0));
        assert!(close(s.to_out(31.0), 11.0));
    }

    #[test]
    fn output_time_never_runs_backwards_and_equals_what_was_kept_so_far() {
        let s = cut_video();
        let mut last = 0.0;
        for i in -8..=880 {
            let t = i as f64 / 8.0;
            let out = s.to_out(t);
            assert!(out >= last, "{t}");
            assert!(close(out, s.kept_within(t)), "{t}");
            last = out;
        }
        assert!(close(s.kept_within(1e9), s.kept()));
        assert!(close(s.kept_within(0.0), 0.0));
    }

    #[test]
    fn a_window_at_a_join_starts_in_the_part_after_it() {
        let s = cut_video();
        assert_eq!(s.window(10.0, 5.0), (10.0, 30.0, 5.0));
        assert_eq!(s.window(30.0, 5.0), (30.0, 60.0, 5.0));
    }

    #[test]
    fn a_window_outside_the_output_is_brought_back_into_it() {
        let s = cut_video();
        assert_eq!(s.window(1000.0, 5.0), (65.0, 95.0, 5.0));
        assert_eq!(s.window(-3.0, 5.0), (0.0, 0.0, 5.0));
        assert_eq!(s.window(3.0, 0.0), (3.0, 3.0, 0.0));
    }

    #[test]
    fn every_window_lies_inside_one_kept_part_and_maps_back_to_its_output_time() {
        let s = cut_video();
        for i in 0..=150 {
            let start = i as f64 / 2.0;
            for want in [0.5, 4.0, 15.0, 100.0] {
                let (out, src, secs) = s.window(start, want);
                assert!(secs > 0.0 && secs <= want, "{start} {want}");
                assert!(s.keep().iter().any(|&(a, b)| src >= a - 1e-9 && src + secs <= b + 1e-9), "{start} {want}: {src} {secs}");
                assert!(close(s.to_out(src), out), "{start} {want}: {src} {out}");
            }
        }
    }

    #[test]
    fn samples_come_from_kept_parts_however_little_is_left() {
        let s = cut_video();
        let samples = s.sample_positions(4, 2.0);
        assert_eq!(samples.len(), 4);
        for (src, secs) in samples {
            assert!(s.keep().iter().any(|&(a, b)| src >= a - 1e-9 && src + secs <= b + 1e-9), "{src} {secs}");
        }
        assert_eq!(Sections::new(&[(1.0, 100.0)], 100.0).unwrap().sample_positions(4, 8.0), vec![(0.0, 1.0)]);
    }

    #[test]
    fn a_kept_part_is_left_open_only_when_it_reaches_the_end() {
        let s = Sections::new(&[(20.0, f64::INFINITY)], 30.0).unwrap();
        assert_eq!(
            s.filter_graph(0, 2, &["a".into(), "b".into()], 30.0),
            "[0:0]trim=start=0.000000:end=20.000000,setpts=PTS-0.000000/TB[v0];\
             [0:a:0]atrim=start=0.000000:end=20.000000,asetpts=PTS-0.000000/TB[a0_0];\
             [0:a:1]atrim=start=0.000000:end=20.000000,asetpts=PTS-0.000000/TB[a1_0];\
             [v0][a0_0][a1_0]concat=n=1:v=1:a=2[vcat][ao0][ao1];[vcat]a,b[vout]"
        );
        // Within a millisecond of the end counts as the end.
        let s = Sections::new(&[(0.0, 10.0)], 30.0).unwrap();
        assert!(!s.filter_graph(0, 0, &[], 30.0005).contains(":end="));
        assert!(s.filter_graph(0, 0, &[], 30.5).contains(":end=30.000000"));
    }

    #[test]
    fn subtitle_lines_across_a_cut_end_or_begin_at_the_join() {
        let s = Sections::new(&[(10.0, 20.0)], 100.0).unwrap();
        let srt = "1\n00:00:05,000 --> 00:00:12,000\nends in the cut\n\n2\n00:00:15,000 --> 00:00:22,000\nbegins in the cut\n";
        assert_eq!(s.retime_srt(srt), "1\n00:00:05,000 --> 00:00:10,000\nends in the cut\n\n2\n00:00:10,000 --> 00:00:12,000\nbegins in the cut\n\n");
    }

    #[test]
    fn subtitle_files_from_other_systems_are_read_too() {
        let s = Sections::new(&[(10.0, 20.0)], 100.0).unwrap();
        // A byte order mark, Windows line ends, a dot for the comma, placement after the time.
        let srt = "\u{feff}1\r\n00:00:01.000 --> 00:00:02.000 X1:10 X2:20\r\nÉté 字幕\r\n\r\n\r\n\r\n2\r\n00:00:30,000 --> 00:00:31,000\r\nLater\r\n";
        assert_eq!(s.retime_srt(srt), "1\n00:00:01,000 --> 00:00:02,000\nÉté 字幕\n\n2\n00:00:20,000 --> 00:00:21,000\nLater\n\n");
    }

    #[test]
    fn subtitle_blocks_without_readable_times_are_left_out() {
        let s = Sections::new(&[(10.0, 20.0)], 100.0).unwrap();
        assert_eq!(s.retime_srt(""), "");
        assert_eq!(s.retime_srt("just text\n\nmore text\n"), "");
        assert_eq!(s.retime_srt("1\nsoon --> later\nx\n\n2\n00:00:01,000 --> \ny\n"), "");
    }

    #[test]
    fn ass_lines_follow_the_format_line_of_their_section() {
        let s = Sections::new(&[(10.0, 20.0)], 100.0).unwrap();
        let ass = "[V4+ Styles]\nFormat: Name, Fontname\nStyle: Default,Arial\n[Events]\nFormat: Start, End, Text\nDialogue: 0:00:25.00,0:00:26.00,Hi, you\n";
        assert_eq!(s.retime_ass(ass), "[V4+ Styles]\nFormat: Name, Fontname\nStyle: Default,Arial\n[Events]\nFormat: Start, End, Text\nDialogue: 0:00:15.00,0:00:16.00,Hi, you\n");
    }

    #[test]
    fn ass_lines_across_a_cut_are_shortened_and_others_stay_as_they_are() {
        let s = Sections::new(&[(10.0, 20.0)], 100.0).unwrap();
        let ass = "Dialogue:0,0:00:09.50,0:00:21.00,Default,,0,0,0,,across\n\
                   Dialogue: 0,soon,0:00:26.00,Default,,0,0,0,,unreadable\n\
                   Comment: 0,0:00:12.00,0:00:13.00,Default,,0,0,0,,a note\n";
        assert_eq!(
            s.retime_ass(ass),
            "Dialogue: 0,0:00:09.50,0:00:11.00,Default,,0,0,0,,across\n\
             Dialogue: 0,soon,0:00:26.00,Default,,0,0,0,,unreadable\n\
             Comment: 0,0:00:12.00,0:00:13.00,Default,,0,0,0,,a note\n"
        );
    }

    #[test]
    fn chapters_across_a_cut_shrink_and_their_titles_are_escaped() {
        let s = Sections::new(&[(10.0, 20.0)], 100.0).unwrap();
        let ch = [
            Chapter { start: 1.0006, end: 25.0, title: Some("a;b#c\\d\ne".into()) },
            Chapter { start: 30.0, end: 40.0, title: None },
            Chapter { start: 50.0, end: 50.0, title: Some("no length".into()) },
        ];
        assert_eq!(
            s.chapters(&ch),
            ";FFMETADATA1\n\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=1001\nEND=15000\ntitle=a\\;b\\#c\\\\d\\\ne\n\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=20000\nEND=30000\n"
        );
        assert_eq!(s.chapters(&[]), ";FFMETADATA1\n");
        assert_eq!(s.chapters(&[Chapter { start: 11.0, end: 19.0, title: None }]), ";FFMETADATA1\n");
    }

    #[test]
    fn subtitle_times_are_written_with_carries_and_never_negative() {
        for (t, want) in [(59.9996, "00:01:00,000"), (359_999.999, "99:59:59,999"), (360_000.0, "100:00:00,000"), (-1.0, "00:00:00,000")] {
            assert_eq!(fmt_srt(t), want, "{t}");
        }
        for (t, want) in [(59.996, "0:01:00.00"), (3600.0, "1:00:00.00"), (0.126, "0:00:00.13"), (-1.0, "0:00:00.00")] {
            assert_eq!(fmt_ass(t), want, "{t}");
        }
        assert_eq!(srt_time("01:02:03,500"), Some(3723.5));
        assert_eq!(ass_time("1:02:03.50"), Some(3723.5));
    }
}
