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
                let o = out_start.clamp(base, base + len - secs);
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
    let out = std::process::Command::new(ffprobe).args(["-v", "error", "-show_chapters", "-of", "json"]).arg(path).output()?;
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
}
