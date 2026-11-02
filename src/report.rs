//! Human-readable summary and per-file CSV for a scan.

use crate::scan::FileResult;
use crate::estimate::{Verdict, bits_per_pixel};
use anyhow::Result;
use std::collections::BTreeMap;
use std::path::Path;

/// Size with a unit that keeps small folders readable (GB for big totals, MB below 1 GB).
fn gb(bytes: f64) -> String {
    if bytes.abs() >= 1e9 { format!("{:.1} GB", bytes / 1e9) } else { format!("{:.0} MB", bytes / 1e6) }
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

pub fn print(results: &[FileResult], top: usize) {
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
    cands.sort_by(|a, b| (b.size as f64 - b.estimate.likely).total_cmp(&(a.size as f64 - a.estimate.likely)));
    println!("\n== Top {top} candidates by estimated savings ==");
    for r in cands.iter().take(top) {
        let i = r.info.as_ref().unwrap();
        println!(
            "{:>9} -> ~{:>9}  {:5} {:>4}x{:<4} {:>5.1} Mbps  {}",
            gb(r.size as f64),
            gb(r.estimate.likely),
            i.codec,
            i.width,
            i.height,
            i.video_bitrate / 1e6,
            r.path.display()
        );
    }
}

pub fn write_csv(results: &[FileResult], path: &Path) -> Result<()> {
    let mut w = csv::Writer::from_path(path)?;
    w.write_record([
        "path", "verdict", "size_bytes", "est_size_likely", "est_size_low", "est_size_high", "codec", "width",
        "height", "fps", "duration_s", "video_mbps", "bpp", "pix_fmt", "hdr", "interlaced", "audio_streams",
        "subtitle_streams", "error",
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
        w.write_record(&row)?;
    }
    w.flush()?;
    Ok(())
}
