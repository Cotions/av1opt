//! The arithmetic of the benchmark, apart from any encoding: where a setting's size/quality
//! curve crosses the target, how settings compare across videos, which ones are worth having.

use crate::tuning::Class;
use serde::{Deserialize, Serialize};

/// One encode of a video's sample clips at one CRF.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Measure {
    pub crf: u8,
    /// Mean VMAF over the clips, and the worst clip.
    pub vmaf: f64,
    pub vmaf_min: f64,
    pub bytes: u64,
    /// Length of the clips together, and how long encoding them took.
    pub clip_secs: f64,
    pub encode_secs: f64,
}

impl Measure {
    fn bitrate(&self) -> f64 {
        self.bytes as f64 * 8.0 / self.clip_secs.max(1e-9)
    }

    /// Encode seconds per second of video.
    fn cost(&self) -> f64 {
        self.encode_secs / self.clip_secs.max(1e-9)
    }
}

/// What a setting needs to give a video exactly the target quality.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub crf: f64,
    /// Video bits per second.
    pub bitrate: f64,
    /// Encode seconds per second of video.
    pub cost: f64,
    pub vmaf: f64,
    /// The highest CRF allowed still beats the target, so this is that CRF's result: the file
    /// could be smaller still, and the quality is above the target.
    pub capped: bool,
}

/// Interpolates between the last CRF that meets `target` and the first that misses it: size on
/// a log scale (bitrate falls by a steady factor per CRF step), time linearly. None when no
/// CRF tried reaches the target.
pub fn at_target(measures: &[Measure], target: f64) -> Option<Point> {
    let pass = measures.iter().filter(|m| m.vmaf >= target).max_by_key(|m| m.crf)?;
    let fail = measures.iter().filter(|m| m.vmaf < target && m.crf > pass.crf).min_by_key(|m| m.crf);
    let Some(fail) = fail else {
        return Some(Point { crf: pass.crf as f64, bitrate: pass.bitrate(), cost: pass.cost(), vmaf: pass.vmaf, capped: true });
    };
    let frac = ((pass.vmaf - target) / (pass.vmaf - fail.vmaf)).clamp(0.0, 1.0);
    let mix = |a: f64, b: f64| a + (b - a) * frac;
    Some(Point {
        crf: mix(pass.crf as f64, fail.crf as f64),
        bitrate: mix(pass.bitrate().max(1.0).ln(), fail.bitrate().max(1.0).ln()).exp(),
        cost: mix(pass.cost(), fail.cost()),
        vmaf: target,
        capped: false,
    })
}

/// One setting over a set of videos, against the baseline setting on the same videos.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Row {
    /// Index into the report's settings.
    pub config: usize,
    /// Typical size and encode time relative to the baseline (geometric means; 1.0 = the same).
    /// NaN (null in JSON) when the setting has no result on any video.
    #[serde(deserialize_with = "nan_for_null")]
    pub size: f64,
    #[serde(deserialize_with = "nan_for_null")]
    pub time: f64,
    /// The video where this setting did worst and best on size.
    #[serde(deserialize_with = "nan_for_null")]
    pub size_worst: f64,
    #[serde(deserialize_with = "nan_for_null")]
    pub size_best: f64,
    /// Mean CRF at the target.
    pub crf: f64,
    /// Video seconds encoded per second (all videos together).
    pub speed: f64,
    /// Videos where this setting failed or could not reach the target although the baseline
    /// did. The numbers above then cover the other videos only, and the setting is never picked.
    pub missing: usize,
    /// No other setting is both smaller and faster.
    pub pareto: bool,
}

/// JSON has no NaN: it is written as null and read back here.
fn nan_for_null<'de, D: serde::Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    Ok(Option::<f64>::deserialize(d)?.unwrap_or(f64::NAN))
}

/// The settings worth choosing between, as indexes into the report's settings.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Picks {
    /// Smallest files, whatever the time.
    pub smallest: usize,
    /// Smallest files without taking noticeably longer than the baseline.
    pub balanced: usize,
    /// Fastest without noticeably bigger files than the baseline.
    pub fastest: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    /// None for all videos together.
    pub class: Option<Class>,
    /// Videos the baseline was measured on; the settings are compared on these.
    pub videos: usize,
    /// Videos left out because the baseline failed or could not reach the target on them.
    pub left_out: usize,
    pub rows: Vec<Row>,
    pub picks: Picks,
}

/// Differences below this are within what repeating the benchmark changes.
const NOISE: f64 = 0.01;
/// "Not noticeably slower" and "not noticeably bigger" for the picks.
const TIME_SLACK: f64 = 1.10;
const SIZE_SLACK: f64 = 1.02;

/// Compares the settings on the videos the baseline has a result for. `points[v][c]` is video
/// `v` under setting `c`. A setting without a result on one of those videos is compared on the
/// rest and marked; it can't be picked. None when the baseline has no result at all.
pub fn summarize(class: Option<Class>, points: &[&[Option<Point>]], configs: usize, baseline: usize) -> Option<Summary> {
    if baseline >= configs {
        return None;
    }
    let usable: Vec<&[Option<Point>]> = points.iter().copied().filter(|p| p.len() == configs && p[baseline].is_some()).collect();
    if usable.is_empty() {
        return None;
    }
    let geomean = |v: &[f64]| if v.is_empty() { f64::NAN } else { (v.iter().map(|x| x.max(1e-12).ln()).sum::<f64>() / v.len() as f64).exp() };
    let mut rows: Vec<Row> = (0..configs)
        .map(|c| {
            // (this setting, the baseline) on every video this setting has a result for.
            let pairs: Vec<(Point, Point)> = usable.iter().filter_map(|v| Some((v[c]?, v[baseline]?))).collect();
            let sizes: Vec<f64> = pairs.iter().map(|(p, b)| p.bitrate / b.bitrate.max(1e-9)).collect();
            let times: Vec<f64> = pairs.iter().map(|(p, b)| p.cost / b.cost.max(1e-9)).collect();
            let n = pairs.len().max(1) as f64;
            Row {
                config: c,
                size: geomean(&sizes),
                time: geomean(&times),
                size_worst: sizes.iter().copied().fold(f64::NAN, f64::max),
                size_best: sizes.iter().copied().fold(f64::NAN, f64::min),
                crf: pairs.iter().map(|(p, _)| p.crf).sum::<f64>() / n,
                speed: pairs.len() as f64 / pairs.iter().map(|(p, _)| p.cost).sum::<f64>().max(1e-9),
                missing: usable.len() - pairs.len(),
                pareto: false,
            }
        })
        .collect();
    let dominated = |r: &Row| rows.iter().any(|o| o.config != r.config && o.missing == 0 && o.size <= r.size && o.time <= r.time && (o.size < r.size || o.time < r.time));
    let front: Vec<bool> = rows.iter().map(|r| r.missing == 0 && !dominated(r)).collect();
    for (r, f) in rows.iter_mut().zip(front) {
        r.pareto = f;
    }

    // A pick replaces the baseline only when it wins by more than the noise.
    let best = |keep: &dyn Fn(&Row) -> bool, by: &dyn Fn(&Row) -> f64, gain: f64| {
        rows.iter().filter(|r| r.missing == 0 && keep(r)).min_by(|a, b| by(a).total_cmp(&by(b))).filter(|r| by(r) < 1.0 - gain).map_or(baseline, |r| r.config)
    };
    let picks = Picks {
        smallest: best(&|_| true, &|r| r.size, NOISE),
        balanced: best(&|r| r.time <= TIME_SLACK, &|r| r.size, NOISE),
        fastest: best(&|r| r.size <= SIZE_SLACK, &|r| r.time, 5.0 * NOISE),
    };
    Some(Summary { class, videos: usable.len(), left_out: points.len() - usable.len(), rows, picks })
}

/// Picks up to `max` of `items` (already in a stable order): the same number from every class
/// as far as they go, spread evenly through each class's list rather than its first few.
pub fn spread<T: Clone>(items: &[(Class, T)], max: usize) -> Vec<T> {
    if items.len() <= max {
        return items.iter().map(|(_, t)| t.clone()).collect();
    }
    let by_class: Vec<Vec<&T>> = Class::ALL.iter().map(|c| items.iter().filter(|(k, _)| k == c).map(|(_, t)| t).collect()).collect();
    let mut quota = [0usize; 3];
    let mut left = max;
    // Hand out places one at a time, so a class with few videos doesn't waste any.
    while left > 0 {
        let mut gave = false;
        for (q, list) in quota.iter_mut().zip(&by_class) {
            if left > 0 && *q < list.len() {
                *q += 1;
                left -= 1;
                gave = true;
            }
        }
        if !gave {
            break;
        }
    }
    let mut out = Vec::new();
    for (q, list) in quota.iter().zip(&by_class) {
        for i in 0..*q {
            // Middles of `q` equal stretches of the list.
            out.push(list[(2 * i + 1) * list.len() / (2 * q)].clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(crf: u8, vmaf: f64, kbps: f64, cost: f64) -> Measure {
        Measure { crf, vmaf, vmaf_min: vmaf - 1.0, bytes: (kbps * 1000.0 / 8.0 * 10.0) as u64, clip_secs: 10.0, encode_secs: cost * 10.0 }
    }

    fn p(bitrate: f64, cost: f64) -> Option<Point> {
        Some(Point { crf: 35.0, bitrate, cost, vmaf: 93.0, capped: false })
    }

    #[test]
    fn target_is_interpolated_between_neighbours() {
        let ms = [m(30, 95.0, 4000.0, 2.0), m(34, 93.5, 2000.0, 1.6), m(35, 92.5, 1000.0, 1.4), m(40, 90.0, 500.0, 1.0)];
        let pt = at_target(&ms, 93.0).unwrap();
        assert!((pt.crf - 34.5).abs() < 1e-9);
        // Halfway on a log scale between 2000 and 1000 kbps.
        assert!((pt.bitrate - (2000.0e3_f64 * 1000.0e3).sqrt()).abs() < 1.0, "{}", pt.bitrate);
        assert!((pt.cost - 1.5).abs() < 1e-9);
        assert!(!pt.capped);
        assert_eq!(pt.vmaf, 93.0);
        // Exactly on a measured CRF.
        let pt = at_target(&ms, 93.5).unwrap();
        assert!((pt.crf - 34.0).abs() < 1e-9 && (pt.bitrate - 2000.0e3).abs() < 1.0);
    }

    #[test]
    fn target_out_of_range() {
        let ms = [m(30, 92.0, 4000.0, 2.0), m(20, 92.9, 9000.0, 3.0)];
        assert_eq!(at_target(&ms, 93.0), None);
        assert_eq!(at_target(&[], 93.0), None);
        // Even the highest CRF passes: its numbers, flagged.
        let ms = [m(50, 96.0, 300.0, 1.0), m(55, 95.0, 200.0, 0.9)];
        let pt = at_target(&ms, 93.0).unwrap();
        assert!(pt.capped && pt.crf == 55.0 && pt.vmaf == 95.0);
    }

    #[test]
    fn scores_that_wobble_use_the_highest_passing_crf() {
        // 36 passes again after 35 failed: the answer lies above 36.
        let ms = [m(34, 93.4, 2000.0, 1.0), m(35, 92.9, 1800.0, 1.0), m(36, 93.1, 1600.0, 1.0), m(37, 92.1, 1400.0, 1.0)];
        let pt = at_target(&ms, 93.0).unwrap();
        assert!(pt.crf > 36.0 && pt.crf < 37.0, "{}", pt.crf);
    }

    #[test]
    fn rows_compare_against_the_baseline() {
        // Settings: 0 = baseline, 1 = smaller but slower, 2 = faster but bigger, 3 = worse at both.
        let a = [p(1000.0, 1.0), p(900.0, 2.0), p(1100.0, 0.5), p(1200.0, 2.5)];
        let b = [p(4000.0, 3.0), p(3200.0, 6.0), p(4400.0, 1.5), p(4100.0, 9.0)];
        // The baseline failed here: nothing to compare with, the video is left out.
        let c = [None, p(100.0, 0.1), p(100.0, 0.1), p(100.0, 0.1)];
        let s = summarize(None, &[&a, &b, &c], 4, 0).unwrap();
        assert_eq!((s.videos, s.left_out), (2, 1));
        let r = &s.rows;
        assert!((r[0].size - 1.0).abs() < 1e-12 && (r[0].time - 1.0).abs() < 1e-12);
        assert!((r[1].size - (0.9_f64 * 0.8).sqrt()).abs() < 1e-12);
        assert!((r[1].time - 2.0).abs() < 1e-12);
        assert!((r[1].size_worst - 0.9).abs() < 1e-12 && (r[1].size_best - 0.8).abs() < 1e-12);
        assert!((r[2].time - 0.5).abs() < 1e-12);
        // Two videos in 1 + 3 encode seconds per video second.
        assert!((r[0].speed - 0.5).abs() < 1e-12);
        assert_eq!(r.iter().map(|x| x.pareto).collect::<Vec<_>>(), [true, true, true, false]);
        assert!(r.iter().all(|x| x.missing == 0));
        assert_eq!(s.picks, Picks { smallest: 1, balanced: 0, fastest: 0 });
    }

    #[test]
    fn a_setting_that_fails_somewhere_is_shown_but_never_picked() {
        // Setting 1 is far smaller and faster where it works, but misses the target on `b`.
        let a = [p(1000.0, 1.0), p(500.0, 0.5), p(960.0, 1.0)];
        let b = [p(2000.0, 1.0), None, p(1920.0, 1.0)];
        let s = summarize(None, &[&a, &b], 3, 0).unwrap();
        assert_eq!((s.videos, s.left_out), (2, 0));
        assert_eq!(s.rows[1].missing, 1);
        assert!((s.rows[1].size - 0.5).abs() < 1e-12);
        assert!(!s.rows[1].pareto);
        // It doesn't push others off the front either: only setting 2 beats the baseline.
        assert!(!s.rows[0].pareto && s.rows[2].pareto);
        assert_eq!(s.picks, Picks { smallest: 2, balanced: 2, fastest: 0 });
        // A setting with no result anywhere has no numbers.
        let only = [p(1000.0, 1.0), None];
        let s = summarize(None, &[&only], 2, 0).unwrap();
        assert!(s.rows[1].size.is_nan() && s.rows[1].missing == 1 && !s.rows[1].pareto);
        let back: Summary = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert!(back.rows[1].size.is_nan() && back.rows[0] == s.rows[0]);
        assert_eq!(s.picks, Picks { smallest: 0, balanced: 0, fastest: 0 });
    }

    #[test]
    fn picks_respect_their_limits_and_the_noise() {
        // 1: 5% smaller at the same speed; 2: 12% smaller but 3x the time;
        // 3: 30% faster and 1% bigger; 4: twice as fast but 8% bigger.
        let v = [p(1000.0, 1.0), p(950.0, 1.05), p(880.0, 3.0), p(1010.0, 0.7), p(1080.0, 0.5)];
        let s = summarize(Some(Class::Hd), &[&v], 5, 0).unwrap();
        assert_eq!(s.picks, Picks { smallest: 2, balanced: 1, fastest: 3 });
        assert_eq!(s.class, Some(Class::Hd));
        // Wins inside the noise leave the baseline in place.
        let v = [p(1000.0, 1.0), p(995.0, 1.0), p(1000.0, 0.97)];
        assert_eq!(summarize(None, &[&v], 3, 0).unwrap().picks, Picks { smallest: 0, balanced: 0, fastest: 0 });
        // No baseline result, or a baseline that isn't there.
        assert!(summarize(None, &[&[None, p(1.0, 1.0)][..]], 2, 0).is_none());
        assert!(summarize(None, &[&v], 3, 3).is_none());
    }

    #[test]
    fn spread_takes_from_every_class_and_through_each_list() {
        let mut items: Vec<(Class, u32)> = (0..20).map(|i| (Class::Hd, i)).collect();
        items.extend((100..102).map(|i| (Class::Uhd, i)));
        items.extend((200..210).map(|i| (Class::Sd, i)));
        // Few enough: all of them, in order.
        assert_eq!(spread(&items[..5], 8).len(), 5);
        let got = spread(&items, 8);
        assert_eq!(got.len(), 8);
        // Both 4K videos, three of each other class.
        assert_eq!(got.iter().filter(|&&x| (100..200).contains(&x)).count(), 2);
        assert_eq!(got.iter().filter(|&&x| x < 100).count(), 3);
        // Spread over the list of 20, not its first three.
        let hd: Vec<u32> = got.iter().copied().filter(|&x| x < 100).collect();
        assert_eq!(hd, [3, 10, 16]);
        assert!(spread(&items, 0).is_empty());
    }
}
