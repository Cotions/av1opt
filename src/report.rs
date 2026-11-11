//! Human-readable summary and per-file CSV for a scan.

use crate::scan::FileResult;
use crate::estimate::{TimeModel, Verdict, bits_per_pixel};
use anyhow::Result;
use std::collections::BTreeMap;
use std::path::Path;

/// Size with a unit that keeps small folders readable (GB for big totals, MB below 1 GB).
pub fn gb(bytes: f64) -> String {
    if bytes.abs() >= 1e9 { format!("{:.1} GB", bytes / 1e9) } else { format!("{:.0} MB", bytes / 1e6) }
}

/// A length of time for people: minutes, hours, then days.
pub fn span(secs: f64) -> String {
    if secs < 60.0 {
        "<1 min".into()
    } else if secs < 5400.0 {
        format!("{:.0} min", secs / 60.0)
    } else if secs < 172_800.0 {
        format!("{:.1} h", secs / 3600.0)
    } else {
        format!("{:.1} days", secs / 86_400.0)
    }
}

/// What the list of top candidates is ordered by.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum TopBy {
    /// Most bytes saved first.
    #[default]
    Savings,
    /// Most bytes saved per hour of encoding first: where to start when time is short.
    PerHour,
}

#[derive(Default)]
struct Bucket {
    files: usize,
    size: f64,
    candidates: usize,
    saved_low: f64,
    saved_likely: f64,
    saved_high: f64,
}

impl Bucket {
    fn add(&mut self, r: &FileResult) {
        self.files += 1;
        self.size += r.size as f64;
        if r.verdict == Verdict::Candidate {
            self.candidates += 1;
            // Optimistic estimate = smallest new file = most saved.
            self.saved_high += r.size as f64 - r.estimate.low;
            self.saved_likely += r.size as f64 - r.estimate.likely;
            self.saved_low += r.size as f64 - r.estimate.high;
        }
    }

    fn savings(&self) -> String {
        if self.candidates == 0 {
            return "-".into();
        }
        format!("{} ({} - {})", gb(self.saved_likely), gb(self.saved_low), gb(self.saved_high))
    }
}

fn top_folder(r: &FileResult) -> String {
    let rel = r.path.strip_prefix(&r.root).unwrap_or(&r.path);
    let first = rel.components().next().map(|c| c.as_os_str().to_string_lossy().into_owned());
    let root = r.root.display().to_string();
    match first {
        Some(f) if rel.components().count() > 1 => format!("{}/{}", root.trim_end_matches('/'), f),
        _ => root,
    }
}

pub fn print(results: &[FileResult], top: usize, time: &TimeModel, by: TopBy) {
    let mut all = Bucket::default();
    let mut by_verdict: BTreeMap<Verdict, Bucket> = BTreeMap::new();
    let mut by_codec: BTreeMap<String, Bucket> = BTreeMap::new();
    let mut by_folder: BTreeMap<String, Bucket> = BTreeMap::new();
    for r in results {
        all.add(r);
        by_verdict.entry(r.verdict).or_default().add(r);
        let codec = r.info.as_ref().map_or("unreadable".into(), |i| i.codec.clone());
        by_codec.entry(codec).or_default().add(r);
        by_folder.entry(top_folder(r)).or_default().add(r);
    }

    println!("\n== Summary ==");
    println!("{} videos, {}", all.files, gb(all.size));
    println!("{} candidates, estimated savings {}", all.candidates, all.savings());
    println!("(likely, with pessimistic - optimistic range in brackets)");
    let secs_of = |r: &FileResult| r.info.as_ref().map_or(0.0, |i| time.secs(i));
    let saved_of = |r: &FileResult| r.size as f64 - r.estimate.likely;
    // Bytes saved per second of encoding; a time that isn't known sorts last.
    let rate_of = |r: &FileResult| match secs_of(r) {
        s if s > 0.0 => saved_of(r) / s,
        _ => 0.0,
    };
    if all.candidates > 0 {
        let total: f64 = results.iter().filter(|r| r.verdict == Verdict::Candidate).map(secs_of).sum();
        let basis = match time.samples {
            0 => "a rough guess until a few files are encoded on this machine".to_string(),
            1 => "speed going by 1 past encode on this machine".to_string(),
            n => format!("speed learned from {n} past encodes on this machine"),
        };
        println!("encoding them: about {} at preset {} ({basis})", span(total), time.preset);
    }

    println!("\n== By verdict ==");
    for (v, b) in &by_verdict {
        println!("{:34} {:>6} files  {:>10}", v.label(), b.files, gb(b.size));
    }

    let table = |title: &str, map: &BTreeMap<String, Bucket>| {
        println!("\n== {title} ==");
        let mut rows: Vec<_> = map.iter().collect();
        rows.sort_by(|a, b| b.1.saved_likely.total_cmp(&a.1.saved_likely).then(b.1.size.total_cmp(&a.1.size)));
        println!("{:40} {:>6} {:>10} {:>6}  savings", "", "files", "size", "cand.");
        for (k, b) in rows.iter().take(15) {
            let k: String = if k.chars().count() > 40 { k.chars().take(39).collect::<String>() + "…" } else { k.to_string() };
            println!("{:40} {:>6} {:>10} {:>6}  {}", k, b.files, gb(b.size), b.candidates, b.savings());
        }
    };
    table("By codec", &by_codec);
    table("By folder", &by_folder);

    let mut cands: Vec<&FileResult> = results.iter().filter(|r| r.verdict == Verdict::Candidate).collect();
    match by {
        TopBy::Savings => cands.sort_by(|a, b| saved_of(b).total_cmp(&saved_of(a))),
        TopBy::PerHour => cands.sort_by(|a, b| rate_of(b).total_cmp(&rate_of(a))),
    }
    let what = if by == TopBy::PerHour { "estimated savings per hour of encoding" } else { "estimated savings" };
    println!("\n== Top {top} candidates by {what} ==");
    for r in cands.iter().take(top) {
        let i = r.info.as_ref().unwrap();
        println!(
            "{:>9} -> ~{:>9}  {:>8}  {:>11}  {:5} {:>4}x{:<4} {:>5.1} Mbps  {}",
            gb(r.size as f64),
            gb(r.estimate.likely),
            span(secs_of(r)),
            format!("{}/h", gb(rate_of(r) * 3600.0)),
            i.codec,
            i.width,
            i.height,
            i.video_bitrate / 1e6,
            r.path.display()
        );
    }
}

pub fn write_csv(results: &[FileResult], path: &Path, time: &TimeModel) -> Result<()> {
    let mut w = csv::Writer::from_path(path)?;
    w.write_record([
        "path", "verdict", "size_bytes", "est_size_likely", "est_size_low", "est_size_high", "codec", "width",
        "height", "fps", "duration_s", "video_mbps", "bpp", "pix_fmt", "hdr", "interlaced", "audio_streams",
        "subtitle_streams", "error", "est_encode_secs",
    ])?;
    for r in results {
        let mut row = vec![
            r.path.display().to_string(),
            r.verdict.label().to_string(),
            r.size.to_string(),
            format!("{:.0}", r.estimate.likely),
            format!("{:.0}", r.estimate.low),
            format!("{:.0}", r.estimate.high),
        ];
        match &r.info {
            Some(i) => row.extend([
                i.codec.clone(),
                i.width.to_string(),
                i.height.to_string(),
                format!("{:.3}", i.fps),
                format!("{:.1}", i.duration_s),
                format!("{:.2}", i.video_bitrate / 1e6),
                format!("{:.4}", bits_per_pixel(i)),
                i.pix_fmt.clone(),
                i.hdr.to_string(),
                i.interlaced.to_string(),
                i.audio_streams.to_string(),
                i.subtitle_streams.to_string(),
            ]),
            None => row.extend(std::iter::repeat_n(String::new(), 12)),
        }
        row.push(r.error.clone().unwrap_or_default());
        // New columns go last so the ones before keep their place.
        row.push(r.info.as_ref().map(|i| time.secs(i)).filter(|s| *s > 0.0).map_or(String::new(), |s| format!("{s:.0}")));
        w.write_record(&row)?;
    }
    w.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_read_like_people_say_them() {
        assert_eq!(span(20.0), "<1 min");
        assert_eq!(span(600.0), "10 min");
        assert_eq!(span(5399.0), "90 min");
        assert_eq!(span(7200.0), "2.0 h");
        assert_eq!(span(36.5 * 3600.0), "36.5 h");
        assert_eq!(span(3.0 * 86_400.0), "3.0 days");
    }
}
