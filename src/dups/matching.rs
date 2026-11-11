//! Compares fingerprints, groups videos that show the same thing, and suggests which copy of
//! each group to keep.

use super::phash::distance;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Seconds from the start, and before the end, at which frames are compared. A video uses
/// the entries up to half its length, so a cut copy shares its first entries with the full one.
pub const LADDER: &[f64] = &[1.5, 4.0, 10.0, 20.0, 45.0, 100.0, 220.0, 500.0, 1100.0, 2400.0, 5200.0];

/// The ladder for a video of this length; very short clips get one frame per side.
pub fn ladder(duration: f64) -> Vec<f64> {
    let v: Vec<f64> = LADDER.iter().copied().filter(|&t| t <= duration / 2.0).collect();
    if v.is_empty() { vec![(duration / 4.0).max(0.0)] } else { v }
}

/// Frame hashes of one video; None where the frame was flat or could not be read.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Fingerprint {
    /// `head[i]` is the frame `ladder[i]` seconds after the start.
    pub head: Vec<Option<u64>>,
    /// `tail[i]` is the frame `ladder[i]` seconds before the end.
    pub tail: Vec<Option<u64>>,
}

impl Fingerprint {
    pub fn usable(&self) -> usize {
        self.head.iter().chain(&self.tail).filter(|h| h.is_some()).count()
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// The same video from start to end (another resolution, bitrate, codec or container).
    Same,
    /// Same beginning, different length or ending: one is cut short, or they share an intro.
    SameStart,
    /// Same ending, different length or beginning.
    SameEnd,
}

#[derive(Clone, Copy, Debug)]
pub struct Link {
    pub a: usize,
    pub b: usize,
    pub kind: Kind,
    /// Share of the compared frames that matched (0-1).
    pub score: f32,
}

/// Matching frames and frames both sides have, for one end of two videos.
fn side(x: &[Option<u64>], y: &[Option<u64>], max_dist: u32) -> (u32, u32) {
    let (mut matched, mut common) = (0, 0);
    for (a, b) in x.iter().zip(y) {
        if let (Some(a), Some(b)) = (a, b) {
            common += 1;
            matched += u32::from(distance(*a, *b) <= max_dist);
        }
    }
    (matched, common)
}

/// How two videos relate, if they do. `max_dist` is the most bits two frame hashes may differ
/// by and still count as the same picture.
pub fn compare(dur_a: f64, a: &Fingerprint, dur_b: f64, b: &Fingerprint, max_dist: u32) -> Option<(Kind, f32)> {
    let (hm, hc) = side(&a.head, &b.head, max_dist);
    let (tm, tc) = side(&a.tail, &b.tail, max_dist);
    let (m, c) = (hm + tm, hc + tc);
    let close = (dur_a - dur_b).abs() <= (0.02 * dur_a.max(dur_b)).max(1.5);
    // Whole video: three quarters of all compared frames, from both ends together, so two
    // episodes with the same intro don't count.
    if close && c >= 2 && m >= 2 && m * 4 >= c * 3 {
        return Some((Kind::Same, m as f32 / c as f32));
    }
    // One end only: needs at least three frames and four in five to match, which reaches
    // past a few seconds of shared logo.
    let end = |m: u32, c: u32| c >= 3 && m * 5 >= c * 4;
    match (end(hm, hc), end(tm, tc)) {
        (true, false) => Some((Kind::SameStart, hm as f32 / hc as f32)),
        (false, true) => Some((Kind::SameEnd, tm as f32 / tc as f32)),
        // Both ends match but the lengths differ: something was cut from the middle.
        (true, true) => Some((Kind::SameStart, m as f32 / c as f32)),
        (false, false) => None,
    }
}

/// Every related pair among `items` (duration, fingerprint). Checks all pairs: a few seconds
/// for tens of thousands of videos.
pub fn links(items: &[(f64, &Fingerprint)], max_dist: u32) -> Vec<Link> {
    (0..items.len())
        .into_par_iter()
        .flat_map_iter(|i| {
            let (da, fa) = items[i];
            (i + 1..items.len()).filter_map(move |j| {
                let (db, fb) = items[j];
                compare(da, fa, db, fb, max_dist).map(|(kind, score)| Link { a: i, b: j, kind, score })
            })
        })
        .collect()
}

/// What the keep suggestion looks at.
#[derive(Clone, Debug)]
pub struct Media {
    pub path: PathBuf,
    pub size: u64,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub duration_s: f64,
    pub video_bitrate: f64,
    /// Files with the same number here have the same bytes (as far as sampled).
    pub identical: Option<u32>,
}

/// Where the user would rather keep files, and where never.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Prefs {
    /// Folders in order of preference: between equally good copies, the first listed wins.
    #[serde(default)]
    pub prefer: Vec<PathBuf>,
    /// Text in a path that marks a copy as the one to drop (e.g. "trash"), whatever its quality.
    #[serde(default)]
    pub avoid: Vec<String>,
}

impl Prefs {
    fn rank(&self, p: &Path) -> usize {
        self.prefer.iter().position(|d| p.starts_with(d)).unwrap_or(self.prefer.len())
    }

    fn avoided(&self, p: &Path) -> bool {
        let s = p.to_string_lossy().to_lowercase();
        self.avoid.iter().any(|a| !a.is_empty() && s.contains(&a.to_lowercase()))
    }
}

/// Rough bits-per-quality of a codec against H.264, to compare bitrates across codecs.
fn codec_efficiency(codec: &str) -> f64 {
    match codec {
        "av1" => 2.0,
        "hevc" | "vp9" => 1.5,
        "h264" => 1.0,
        "vp8" => 0.8,
        _ => 0.6,
    }
}

#[derive(Serialize, Clone, Debug)]
pub struct Group {
    pub kind: Kind,
    /// Lowest frame-match share of the pairs that put this group together.
    pub score: f32,
    /// Indexes into the file list, best copy first.
    pub members: Vec<usize>,
    /// Why the first member is the suggested one to keep.
    pub reason: String,
}

/// Picks the copy to keep: never one in an avoided place if there is another, then the
/// highest resolution, then the most bitrate for its codec, then the preferred folder.
/// Returns its position in `members` and the reason.
pub fn choose(members: &[usize], media: &[Media], prefs: &Prefs, kind: Kind) -> (usize, String) {
    let m = |i: usize| &media[members[i]];
    let mut cand: Vec<usize> = (0..members.len()).collect();
    let mut why: Vec<&str> = Vec::new();
    let mut narrow = |cand: &mut Vec<usize>, label: &'static str, keep: &dyn Fn(usize) -> bool| {
        let next: Vec<usize> = cand.iter().copied().filter(|&i| keep(i)).collect();
        if !next.is_empty() && next.len() < cand.len() {
            *cand = next;
            why.push(label);
        }
    };
    narrow(&mut cand, "not in an avoided place", &|i| !prefs.avoided(&m(i).path));
    if kind != Kind::Same {
        // Different lengths: the longer one holds everything the shorter one does.
        let longest = cand.iter().map(|&i| m(i).duration_s).fold(0.0, f64::max);
        narrow(&mut cand, "longest", &|i| m(i).duration_s >= longest - 1.0);
    }
    let pixels = |i: usize| m(i).width as f64 * m(i).height as f64;
    let most = cand.iter().map(|&i| pixels(i)).fold(0.0, f64::max);
    narrow(&mut cand, "highest resolution", &|i| pixels(i) >= most * 0.9);
    let quality = |i: usize| m(i).video_bitrate * codec_efficiency(&m(i).codec);
    let best = cand.iter().map(|&i| quality(i)).fold(0.0, f64::max);
    narrow(&mut cand, "highest bitrate for its codec", &|i| quality(i) >= best * 0.85);
    let rank = cand.iter().map(|&i| prefs.rank(&m(i).path)).min().unwrap_or(0);
    narrow(&mut cand, "preferred folder", &|i| prefs.rank(&m(i).path) == rank);
    if cand.len() > 1 {
        let all_identical = m(cand[0]).identical.is_some() && cand.iter().all(|&i| m(i).identical == m(cand[0]).identical);
        let largest = cand.iter().map(|&i| m(i).size).max().unwrap_or(0);
        if !all_identical {
            narrow(&mut cand, "largest file", &|i| m(i).size == largest);
        }
        if cand.len() > 1 {
            cand.sort_by(|&a, &b| m(a).path.cmp(&m(b).path));
            why.push(if all_identical { "identical files, first by name" } else { "first by name" });
        }
    }
    (cand[0], why.join(", "))
}

/// Turns pair links into groups. Videos linked as the same (directly or through others) form
/// one group; start/end matches between such groups are listed as groups of two.
pub fn groups(links: &[Link], media: &[Media], prefs: &Prefs) -> Vec<Group> {
    let mut parent: Vec<usize> = (0..media.len()).collect();
    fn find(p: &mut [usize], mut x: usize) -> usize {
        while p[x] != x {
            p[x] = p[p[x]];
            x = p[x];
        }
        x
    }
    for l in links.iter().filter(|l| l.kind == Kind::Same) {
        let (a, b) = (find(&mut parent, l.a), find(&mut parent, l.b));
        if a != b {
            parent[a.max(b)] = a.min(b);
        }
    }
    let mut same: HashMap<usize, (Vec<usize>, f32)> = HashMap::new();
    for i in 0..media.len() {
        let r = find(&mut parent, i);
        same.entry(r).or_insert_with(|| (Vec::new(), 1.0)).0.push(i);
    }
    for l in links.iter().filter(|l| l.kind == Kind::Same) {
        let r = find(&mut parent, l.a);
        let e = same.get_mut(&r).expect("root has a group");
        e.1 = e.1.min(l.score);
    }

    let ordered = |mut members: Vec<usize>, kind: Kind, score: f32| -> Group {
        let (keep, reason) = choose(&members, media, prefs, kind);
        members.swap(0, keep);
        members[1..].sort_by(|&a, &b| media[a].path.cmp(&media[b].path));
        Group { kind, score, members, reason }
    };
    let mut out: Vec<Group> = same
        .iter()
        .filter(|(_, (m, _))| m.len() > 1)
        .map(|(_, (m, score))| ordered(m.clone(), Kind::Same, *score))
        .collect();
    // The copy each whole-video group keeps (a video by itself is its own).
    let mut kept: Vec<usize> = (0..media.len()).collect();
    for g in &out {
        for &m in &g.members {
            kept[m] = g.members[0];
        }
    }

    // One entry per pair of groups. Shown with the copies that are kept where those match
    // (a pair named by a spare copy sends the reader looking for a file about to go), else
    // with the best-matching pair of files.
    let worth = |l: &Link| ((kept[l.a] == l.a) as u8 + (kept[l.b] == l.b) as u8, l.score);
    let mut partial: HashMap<(usize, usize), Link> = HashMap::new();
    for l in links.iter().filter(|l| l.kind != Kind::Same) {
        let (a, b) = (find(&mut parent, l.a), find(&mut parent, l.b));
        if a == b {
            continue;
        }
        let e = partial.entry((a.min(b), a.max(b))).or_insert(*l);
        if worth(l) > worth(e) {
            *e = *l;
        }
    }
    out.extend(partial.values().map(|l| ordered(vec![l.a, l.b], l.kind, l.score)));

    // Whole-video groups first, biggest win first; ties by path so runs list the same way.
    let spare = |g: &Group| g.members[1..].iter().map(|&i| media[i].size).sum::<u64>();
    out.sort_by(|a, b| {
        (a.kind != Kind::Same)
            .cmp(&(b.kind != Kind::Same))
            .then(spare(b).cmp(&spare(a)))
            .then_with(|| media[a.members[0]].path.cmp(&media[b.members[0]].path))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake video: frame hashes are far apart for different `content`, and position `i`
    /// of the same content always hashes the same.
    fn fp(content: u64, duration: f64) -> Fingerprint {
        let h = |i: usize, end: u64| Some((content * 1000 + end * 100 + i as u64).wrapping_mul(0x9E3779B97F4A7C15));
        let n = ladder(duration).len();
        Fingerprint { head: (0..n).map(|i| h(i, 0)).collect(), tail: (0..n).map(|i| h(i, 1)).collect() }
    }

    fn media(path: &str, w: u32, h: u32, codec: &str, mbps: f64, size: u64) -> Media {
        Media { path: path.into(), size, codec: codec.into(), width: w, height: h, duration_s: 600.0, video_bitrate: mbps * 1e6, identical: None }
    }

    #[test]
    fn ladders() {
        assert_eq!(ladder(45.0), vec![1.5, 4.0, 10.0, 20.0]);
        assert_eq!(ladder(600.0).len(), 7);
        assert_eq!(ladder(2.0), vec![0.5]);
        assert_eq!(ladder(0.0), vec![0.0]);
        // A cut copy uses the first entries of the full video's ladder.
        assert!(ladder(600.0).starts_with(&ladder(90.0)));
    }

    #[test]
    fn same_video_matches_with_a_few_bad_frames() {
        let a = fp(1, 600.0);
        let mut b = a.clone();
        b.head[2] = Some(!b.head[2].unwrap());
        b.tail[0] = None;
        let (kind, score) = compare(600.0, &a, 600.4, &b, 10).unwrap();
        assert_eq!(kind, Kind::Same);
        assert!((score - 12.0 / 13.0).abs() < 1e-6);
        assert_eq!(compare(600.0, &a, 600.0, &fp(2, 600.0), 10), None);
    }

    #[test]
    fn shared_intro_is_not_a_match() {
        // Two episodes of equal length whose first 45 s are the same.
        let a = fp(1, 600.0);
        let mut b = fp(2, 600.0);
        b.head[..5].clone_from_slice(&a.head[..5]);
        assert_eq!(compare(600.0, &a, 600.0, &b, 10), None);
    }

    #[test]
    fn cut_copies_match_at_one_end() {
        let full = fp(1, 600.0);
        // Ending cut off: the start ladder is a prefix, the end ladder shows other frames.
        let mut short = fp(9, 90.0);
        short.head = full.head[..short.head.len()].to_vec();
        assert_eq!(compare(600.0, &full, 90.0, &short, 10).unwrap().0, Kind::SameStart);
        // Intro cut off: only the end lines up.
        let mut late = fp(8, 500.0);
        late.tail = full.tail[..late.tail.len()].to_vec();
        assert_eq!(compare(600.0, &full, 500.0, &late, 10).unwrap().0, Kind::SameEnd);
        // Two frames are not enough to call it.
        let mut tiny = fp(7, 9.0);
        tiny.head = full.head[..tiny.head.len()].to_vec();
        assert_eq!(compare(600.0, &full, 9.0, &tiny, 10), None);
    }

    #[test]
    fn flat_frames_are_not_evidence() {
        let blank = Fingerprint { head: vec![None; 7], tail: vec![None; 7] };
        assert_eq!(compare(600.0, &blank, 600.0, &blank, 10), None);
        assert_eq!(blank.usable(), 0);
    }

    #[test]
    fn keeps_the_best_copy() {
        let prefs = Prefs { prefer: vec!["/lib".into()], avoid: vec!["Trash".into()] };
        let m = vec![
            media("/stage/a_720.mp4", 1280, 720, "h264", 4.0, 300),
            media("/stage/a_1080.mp4", 1920, 1080, "h264", 6.0, 450),
            media("/lib/a_1080_low.mp4", 1920, 1080, "h264", 2.0, 150),
            media("/stage/.trash/a_4k.mp4", 3840, 2160, "h264", 30.0, 2000),
        ];
        let (i, why) = choose(&[0, 1, 2, 3], &m, &prefs, Kind::Same);
        assert_eq!(i, 1, "{why}");
        assert!(why.contains("highest resolution") && why.contains("highest bitrate"), "{why}");
        // AV1 at a third of the H.264 bitrate is no worse; the preferred folder decides.
        let m = vec![media("/stage/b.mp4", 1920, 1080, "h264", 6.0, 450), media("/lib/b.mkv", 1920, 1080, "av1", 2.8, 210)];
        let (i, why) = choose(&[0, 1], &m, &prefs, Kind::Same);
        assert_eq!((i, why.as_str()), (1, "preferred folder"));
        // Only avoided copies left: still picks one.
        let m = vec![media("/x/trash/c.mp4", 640, 360, "h264", 1.0, 10), media("/y/Trash/c.mp4", 1280, 720, "h264", 1.0, 10)];
        assert_eq!(choose(&[0, 1], &m, &prefs, Kind::Same).0, 1);
    }

    #[test]
    fn identical_files_keep_the_first_by_name() {
        let mut m = vec![media("/b/x.mp4", 1920, 1080, "h264", 6.0, 450), media("/a/x.mp4", 1920, 1080, "h264", 6.0, 450)];
        for x in &mut m {
            x.identical = Some(0);
        }
        let (i, why) = choose(&[0, 1], &m, &Prefs::default(), Kind::Same);
        assert_eq!((i, why.as_str()), (1, "identical files, first by name"));
    }

    #[test]
    fn the_longer_copy_wins_for_cut_videos() {
        let mut m = vec![media("/a/full_480.mp4", 854, 480, "h264", 1.0, 100), media("/a/clip_1080.mp4", 1920, 1080, "h264", 6.0, 90)];
        m[1].duration_s = 90.0;
        let (i, why) = choose(&[0, 1], &m, &Prefs::default(), Kind::SameStart);
        assert_eq!((i, why.as_str()), (0, "longest"));
    }

    #[test]
    fn groups_chain_and_list_partials_once() {
        let m: Vec<Media> = (0..6).map(|i| media(&format!("/v/{i}.mp4"), 1920, 1080, "h264", 6.0 - i as f64 * 0.1, 100 - i)).collect();
        let l = |a, b, kind, score| Link { a, b, kind, score };
        // 0-1-2 chain into one group; 3 starts like both 0 and 1; 4 and 5 are unrelated.
        let links = [l(0, 1, Kind::Same, 1.0), l(1, 2, Kind::Same, 0.8), l(0, 3, Kind::SameStart, 0.9), l(1, 3, Kind::SameStart, 1.0)];
        let g = groups(&links, &m, &Prefs::default());
        assert_eq!(g.len(), 2);
        assert_eq!((g[0].kind, g[0].members.clone(), g[0].score), (Kind::Same, vec![0, 1, 2], 0.8));
        // The pair is shown with the copy the group keeps (0), though 1 matched better.
        assert_eq!((g[1].kind, g[1].members.clone(), g[1].score), (Kind::SameStart, vec![0, 3], 0.9));
        // Where the kept copy does not match (its start differs), the best pair is shown.
        let g = groups(&[l(0, 1, Kind::Same, 1.0), l(1, 3, Kind::SameStart, 0.7), l(2, 3, Kind::SameStart, 0.7)], &m, &Prefs::default());
        let starts: Vec<Vec<usize>> = g.iter().filter(|g| g.kind == Kind::SameStart).map(|g| g.members.clone()).collect();
        assert!(starts.contains(&vec![1, 3]) && starts.contains(&vec![2, 3]), "{starts:?}");
    }

    #[test]
    fn all_pairs() {
        let fps = [fp(1, 600.0), fp(2, 600.0), fp(1, 600.0), fp(3, 60.0), fp(3, 60.0)];
        let items: Vec<(f64, &Fingerprint)> = fps.iter().map(|f| (if f.head.len() == 7 { 600.0 } else { 60.0 }, f)).collect();
        let mut found: Vec<(usize, usize)> = links(&items, 10).iter().map(|l| (l.a, l.b)).collect();
        found.sort();
        assert_eq!(found, vec![(0, 2), (3, 4)]);
    }
}
