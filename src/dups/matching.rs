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

/// Where in a video, as shares of its length, the frames that stand for its middle are taken.
/// Counted from the length and not in seconds, so that two copies of one video agree and two
/// episodes with the same opening and credits are compared where they differ.
pub const MIDDLE: [f64; 5] = [1.0 / 6.0, 2.0 / 6.0, 3.0 / 6.0, 4.0 / 6.0, 5.0 / 6.0];

/// What became of one sampled frame.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Frame {
    /// The hash of a picture.
    Picture(u64),
    /// Black, a fade, a plain colour: nothing to tell it from any other such frame.
    Flat,
    /// There is no such frame to be had: past the end of the data, or damaged.
    Unread,
}

/// The sampled frames of one video.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Fingerprint {
    /// `head[i]` is the frame `ladder[i]` seconds after the start.
    pub head: Vec<Frame>,
    /// `tail[i]` is the frame `ladder[i]` seconds before the end.
    pub tail: Vec<Frame>,
    /// `middle[i]` is the frame at the share `MIDDLE[i]` of the length.
    pub middle: Vec<Frame>,
    /// A frame was given up on because ffmpeg took too long, which says nothing lasting about
    /// the file: such a fingerprint is used once and not remembered.
    #[serde(skip)]
    pub out_of_time: bool,
}

impl Fingerprint {
    fn frames(&self) -> impl Iterator<Item = &Frame> {
        self.head.iter().chain(&self.tail).chain(&self.middle)
    }

    pub fn usable(&self) -> usize {
        self.frames().filter(|f| matches!(f, Frame::Picture(_))).count()
    }

    /// Whether frames the file says it has could not be read: it is damaged, or stops before
    /// its stated length (the frames counted from the end are the first to go).
    pub fn damaged(&self) -> bool {
        self.frames().any(|f| *f == Frame::Unread)
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

/// Matching frames and frames compared, for one part of two videos. A flat frame says nothing
/// and is left out. A frame that could not be read counts against the match: leaving it out
/// would let a copy that lost most of its data agree perfectly with what remains.
fn side(x: &[Frame], y: &[Frame], max_dist: u32) -> (u32, u32) {
    let (mut matched, mut common) = (0, 0);
    for (a, b) in x.iter().zip(y) {
        match (a, b) {
            (Frame::Picture(a), Frame::Picture(b)) => {
                common += 1;
                matched += u32::from(distance(*a, *b) <= max_dist);
            }
            (Frame::Unread, _) | (_, Frame::Unread) => common += 1,
            _ => {}
        }
    }
    (matched, common)
}

/// The fewest frames of the middle two videos must both show to be called the same video.
const MIN_MIDDLE: u32 = 3;

/// How two videos relate, if they do. `max_dist` is the most bits two frame hashes may differ
/// by and still count as the same picture.
pub fn compare(dur_a: f64, a: &Fingerprint, dur_b: f64, b: &Fingerprint, max_dist: u32) -> Option<(Kind, f32)> {
    let (hm, hc) = side(&a.head, &b.head, max_dist);
    let (tm, tc) = side(&a.tail, &b.tail, max_dist);
    let (mm, mc) = side(&a.middle, &b.middle, max_dist);
    let (m, c) = (hm + tm, hc + tc);
    let close = (dur_a - dur_b).abs() <= (0.02 * dur_a.max(dur_b)).max(1.5);
    // Whole video: three quarters of all compared frames, some from each end and most of the
    // middle. Both ends can agree over a long way in two episodes with one opening and one set
    // of credits: only what lies between them tells such videos apart.
    let (all_matched, all_compared) = (m + mm, c + mc);
    if close && hm > 0 && tm > 0 && mc >= MIN_MIDDLE && mm * 5 >= mc * 4 && all_matched * 4 >= all_compared * 3 {
        return Some((Kind::Same, all_matched as f32 / all_compared as f32));
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
    /// Frames of it could not be read: see [`Fingerprint::damaged`].
    pub damaged: bool,
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

/// Picks the copy to keep: never a damaged one if there is a whole one, nor one in an avoided
/// place if there is another, then the highest resolution, then the most bitrate for its codec,
/// then the preferred folder. Returns its position in `members` and the reason.
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
    // Before anything else: a higher resolution is worth nothing in a file that breaks off.
    narrow(&mut cand, "the others are damaged or incomplete", &|i| !m(i).damaged);
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

    // Whole-video groups first, biggest win first. Ties go by the paths of all the members:
    // the groups come out of a hash map, and two can share the copy they keep.
    let spare = |g: &Group| g.members[1..].iter().map(|&i| media[i].size).sum::<u64>();
    let paths = |g: &'_ Group| g.members.iter().map(|&i| media[i].path.clone()).collect::<Vec<_>>();
    out.sort_by(|a, b| (a.kind != Kind::Same).cmp(&(b.kind != Kind::Same)).then(spare(b).cmp(&spare(a))).then_with(|| paths(a).cmp(&paths(b))));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake video: frame hashes are far apart for different `content`, and position `i`
    /// of the same content always hashes the same.
    fn fp(content: u64, duration: f64) -> Fingerprint {
        let h = |i: usize, end: u64| Frame::Picture((content * 1000 + end * 100 + i as u64).wrapping_mul(0x9E3779B97F4A7C15));
        let n = ladder(duration).len();
        Fingerprint { head: (0..n).map(|i| h(i, 0)).collect(), tail: (0..n).map(|i| h(i, 1)).collect(), middle: (0..MIDDLE.len()).map(|i| h(i, 2)).collect(), ..Default::default() }
    }

    fn media(path: &str, w: u32, h: u32, codec: &str, mbps: f64, size: u64) -> Media {
        Media { path: path.into(), size, codec: codec.into(), width: w, height: h, duration_s: 600.0, video_bitrate: mbps * 1e6, identical: None, damaged: false }
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
        b.head[2] = flipped(b.head[2]);
        b.tail[0] = Frame::Flat;
        let (kind, score) = compare(600.0, &a, 600.4, &b, 10).unwrap();
        assert_eq!(kind, Kind::Same);
        assert!((score - 17.0 / 18.0).abs() < 1e-6);
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
        let blank = Fingerprint { head: vec![Frame::Flat; 7], tail: vec![Frame::Flat; 7], middle: vec![Frame::Flat; 5], ..Default::default() };
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

    /// A hash `bits` bits away from `h`.
    fn off(h: u64, bits: u32) -> u64 {
        h ^ ((1u64 << bits) - 1)
    }

    /// The picture farthest from this frame's.
    fn flipped(f: Frame) -> Frame {
        match f {
            Frame::Picture(h) => Frame::Picture(!h),
            other => other,
        }
    }

    /// Five pictures of a middle, the same for all videos made with [`sides`].
    fn shared_middle() -> Vec<Frame> {
        frames(900, MIDDLE.len(), MIDDLE.len()).0
    }

    fn sides(head: &[Frame], tail: &[Frame]) -> Fingerprint {
        Fingerprint { head: head.to_vec(), tail: tail.to_vec(), middle: shared_middle(), ..Default::default() }
    }

    /// `n` frames, of which the first `same` are the ones `base` has and the others are far off.
    fn frames(base: u64, n: usize, same: usize) -> (Vec<Frame>, Vec<Frame>) {
        let a: Vec<Frame> = (0..n as u64).map(|i| Frame::Picture((base + i).wrapping_mul(0x9E3779B97F4A7C15))).collect();
        let b = a.iter().enumerate().map(|(i, f)| if i < same { *f } else { flipped(*f) }).collect();
        (a, b)
    }

    /// A video whose start and end are those of `a`, and of whose middle only the first
    /// `same` frames are.
    fn same_ends_as(a: &Fingerprint, other_content: u64, same: usize) -> Fingerprint {
        let mut b = fp(other_content, 600.0);
        b.head.clone_from(&a.head);
        b.tail.clone_from(&a.tail);
        b.middle[..same].clone_from_slice(&a.middle[..same]);
        b
    }

    #[test]
    fn shared_ends_with_another_middle_are_not_the_same_video() {
        let a = fp(1, 600.0);
        // Everything at both ends agrees, and at most three of the five frames between them.
        for same in 0..=3 {
            assert_eq!(compare(600.0, &a, 600.0, &same_ends_as(&a, 2, same), 10).map(|r| r.0), Some(Kind::SameStart), "{same}");
        }
        // Four of five is what a copy with a damaged or odd frame still reaches.
        assert_eq!(compare(600.0, &a, 600.0, &same_ends_as(&a, 2, 4), 10).map(|r| r.0), Some(Kind::Same));
        assert_eq!(compare(600.0, &a, 600.0, &a, 10), Some((Kind::Same, 1.0)));
    }

    #[test]
    fn without_three_frames_of_the_middle_in_common_nothing_is_the_same_video() {
        let a = fp(1, 600.0);
        let mut b = a.clone();
        b.middle[..3].fill(Frame::Flat);
        assert_eq!(compare(600.0, &a, 600.0, &b, 10).map(|r| r.0), Some(Kind::SameStart));
        b.middle[2] = a.middle[2];
        assert_eq!(compare(600.0, &a, 600.0, &b, 10).map(|r| r.0), Some(Kind::Same));
        b.middle.clear();
        assert_eq!(compare(600.0, &a, 600.0, &b, 10).map(|r| r.0), Some(Kind::SameStart));
    }

    #[test]
    fn a_video_uses_the_ladder_up_to_half_its_length() {
        assert_eq!(ladder(3.0), vec![1.5]);
        assert_eq!(ladder(2.9), vec![2.9 / 4.0]);
        assert_eq!(ladder(8.0), vec![1.5, 4.0]);
        assert_eq!(ladder(7.9), vec![1.5]);
        assert_eq!(ladder(-5.0), vec![0.0]);
        assert_eq!(ladder(f64::NAN), vec![0.0]);
    }

    #[test]
    fn the_number_of_frames_read_is_the_one_the_readme_gives() {
        for secs in [600.0, 900.0] {
            assert_eq!(ladder(secs).len() * 2, 14, "{secs}");
        }
        assert_eq!(ladder(10_400.0).len() * 2, 22);
        assert_eq!(ladder(1e7).len() * 2, 22);
    }

    #[test]
    fn frames_count_as_the_same_up_to_the_tolerance_and_no_further() {
        let h = 0x1234_5678_9ABC_DEF0_u64 >> 1;
        let same = |bits| side(&[Frame::Picture(h)], &[Frame::Picture(off(h, bits))], 10);
        assert_eq!(same(0), (1, 1));
        assert_eq!(same(10), (1, 1));
        assert_eq!(same(11), (0, 1));
    }

    #[test]
    fn only_frames_both_sides_have_are_compared() {
        let p = Frame::Picture(1);
        assert_eq!(side(&[], &[], 10), (0, 0));
        assert_eq!(side(&[p], &[], 10), (0, 0));
        assert_eq!(side(&[Frame::Flat, p, p], &[p, Frame::Flat, p, p], 10), (1, 1));
    }

    #[test]
    fn a_frame_that_could_not_be_read_counts_against_the_match() {
        let (p, gone) = (Frame::Picture(1), Frame::Unread);
        assert_eq!(side(&[gone, p], &[p, p], 10), (1, 2));
        assert_eq!(side(&[p, p], &[p, gone], 10), (1, 2));
        assert_eq!(side(&[gone], &[gone], 10), (0, 1));
        assert_eq!(side(&[gone], &[Frame::Flat], 10), (0, 1));
    }

    #[test]
    fn a_copy_cut_off_in_the_middle_is_not_the_same_video() {
        // The file says it is as long as the other, and its data stops after a third.
        let whole = fp(1, 600.0);
        let mut cut = whole.clone();
        cut.tail.fill(Frame::Unread);
        cut.middle[2..].fill(Frame::Unread);
        cut.head[6] = Frame::Unread;
        assert!(cut.damaged() && !whole.damaged());
        let (kind, score) = compare(600.0, &whole, 600.0, &cut, 10).unwrap();
        assert_eq!((kind, score), (Kind::SameStart, 6.0 / 7.0));
        // With every frame of one end gone there is nothing of that end to go by, however
        // well the rest agrees.
        let mut no_end = whole.clone();
        no_end.tail.fill(Frame::Unread);
        assert_eq!(compare(600.0, &whole, 600.0, &no_end, 10).map(|r| r.0), Some(Kind::SameStart));
        let mut flat_end = whole.clone();
        flat_end.tail.fill(Frame::Flat);
        assert_eq!(compare(600.0, &whole, 600.0, &flat_end, 10).map(|r| r.0), Some(Kind::SameStart));
    }

    #[test]
    fn a_damaged_copy_is_kept_only_if_nothing_whole_is_left() {
        let mut m = vec![media("/a/big.mp4", 3840, 2160, "h264", 40.0, 900), media("/b/small.mp4", 640, 360, "h264", 1.0, 10)];
        m[0].damaged = true;
        assert_eq!(pick(&m, &Prefs::default(), Kind::Same), (1, "the others are damaged or incomplete".to_string()));
        // It also goes before where a copy lies, and for cut copies before the length.
        let prefs = Prefs { prefer: vec![], avoid: vec!["/b/".into()] };
        assert_eq!(pick(&m, &prefs, Kind::Same).0, 1);
        m[0].duration_s = 7200.0;
        assert_eq!(pick(&m, &Prefs::default(), Kind::SameStart).0, 1);
        // With every copy damaged the usual rules choose among them.
        m[1].damaged = true;
        assert_eq!(pick(&m, &Prefs::default(), Kind::Same).0, 0);
    }

    #[test]
    fn lengths_count_as_the_same_within_two_percent_or_a_second_and_a_half() {
        let (a, b) = frames(1, 8, 8);
        let (x, y) = (sides(&a, &a), sides(&b, &b));
        let same = |d1, d2| compare(d1, &x, d2, &y, 10).map(|r| r.0);
        // 2% of the longer one.
        assert_eq!(same(1000.0, 980.0), Some(Kind::Same));
        assert_eq!(same(980.0, 1000.0), Some(Kind::Same));
        assert_eq!(same(1000.0, 979.0), Some(Kind::SameStart));
        // A second and a half where that is more than 2%.
        assert_eq!(same(10.0, 11.5), Some(Kind::Same));
        assert_eq!(same(10.0, 11.6), Some(Kind::SameStart));
    }

    #[test]
    fn three_quarters_of_all_frames_make_the_same_video() {
        // Six frames at each end and the five of the middle, which all agree: 13 of 17 do it.
        let whole = |head, tail| {
            let (a, b) = frames(1, 6, head);
            let (c, d) = frames(50, 6, tail);
            compare(600.0, &sides(&a, &c), 600.0, &sides(&b, &d), 10)
        };
        assert_eq!(whole(4, 4), Some((Kind::Same, 13.0 / 17.0)));
        // One short of it: not the whole video, and neither end agrees enough on its own.
        assert_eq!(whole(4, 3), None);
    }

    #[test]
    fn frames_of_both_ends_are_added_up_for_the_whole_video() {
        // Half of one end and all of the other, with the middle: 11 of 13.
        let (a, b) = frames(1, 4, 2);
        let (c, d) = frames(50, 4, 4);
        assert_eq!(compare(600.0, &sides(&a, &c), 600.0, &sides(&b, &d), 10), Some((Kind::Same, 11.0 / 13.0)));
    }

    #[test]
    fn one_end_needs_three_frames_and_four_in_five_of_them() {
        let start = |n, same, tail_same| {
            let (a, b) = frames(1, n, same);
            let (c, d) = frames(50, 5, tail_same);
            compare(600.0, &sides(&a, &c), 300.0, &sides(&b, &d), 10)
        };
        assert_eq!(start(5, 4, 0), Some((Kind::SameStart, 0.8)));
        assert_eq!(start(5, 3, 0), None);
        assert_eq!(start(3, 3, 0), Some((Kind::SameStart, 1.0)));
        assert_eq!(start(2, 2, 0), None);
        // The same at the other end.
        let (a, b) = frames(1, 5, 0);
        let (c, d) = frames(50, 5, 4);
        assert_eq!(compare(600.0, &sides(&a, &c), 300.0, &sides(&b, &d), 10), Some((Kind::SameEnd, 0.8)));
        // Both ends agreeing with different lengths: cut in the middle, counted over both.
        assert_eq!(start(5, 5, 4), Some((Kind::SameStart, 0.9)));
    }

    #[test]
    fn a_video_and_its_copy_relate_the_same_way_in_either_order() {
        let full = fp(1, 600.0);
        let mut short = fp(9, 90.0);
        short.head = full.head[..short.head.len()].to_vec();
        assert_eq!(compare(600.0, &full, 90.0, &short, 10), compare(90.0, &short, 600.0, &full, 10));
    }

    #[test]
    fn no_videos_or_one_have_no_pairs() {
        assert!(links(&[], 10).is_empty());
        let f = fp(1, 600.0);
        assert!(links(&[(600.0, &f)], 10).is_empty());
    }

    fn pick(m: &[Media], prefs: &Prefs, kind: Kind) -> (usize, String) {
        choose(&(0..m.len()).collect::<Vec<_>>(), m, prefs, kind)
    }

    #[test]
    fn a_tenth_less_resolution_counts_as_the_same() {
        // 90% of the pixels: the higher bitrate of the smaller picture decides.
        let m = [media("/a", 1000, 1000, "h264", 1.0, 1), media("/b", 1000, 900, "h264", 5.0, 1)];
        assert_eq!(pick(&m, &Prefs::default(), Kind::Same), (1, "highest bitrate for its codec".to_string()));
        // Less than that: the bigger picture wins whatever its bitrate.
        let m = [media("/a", 1000, 1000, "h264", 1.0, 1), media("/b", 1000, 899, "h264", 5.0, 1)];
        assert_eq!(pick(&m, &Prefs::default(), Kind::Same), (0, "highest resolution".to_string()));
    }

    #[test]
    fn bitrates_within_fifteen_percent_count_as_the_same() {
        let prefs = Prefs { prefer: vec!["/lib".into()], avoid: vec![] };
        let m = [media("/x/a", 1920, 1080, "h264", 10.0, 1), media("/lib/a", 1920, 1080, "h264", 8.5, 1)];
        assert_eq!(pick(&m, &prefs, Kind::Same), (1, "preferred folder".to_string()));
        let m = [media("/x/a", 1920, 1080, "h264", 10.0, 1), media("/lib/a", 1920, 1080, "h264", 8.4, 1)];
        assert_eq!(pick(&m, &prefs, Kind::Same), (0, "highest bitrate for its codec".to_string()));
    }

    #[test]
    fn codecs_are_weighed_as_the_readme_says() {
        assert_eq!(codec_efficiency("av1"), 2.0);
        assert_eq!(codec_efficiency("hevc"), 1.5);
        assert_eq!(codec_efficiency("vp9"), 1.5);
        assert_eq!(codec_efficiency("h264"), 1.0);
        assert!(codec_efficiency("mpeg2video") < 1.0);
        // AV1 at 3 counts as 6, more than 15% under H.264 at 7.1, which stays ahead.
        let m = [media("/a", 1920, 1080, "av1", 3.0, 1), media("/b", 1920, 1080, "h264", 7.1, 1)];
        assert_eq!(pick(&m, &Prefs::default(), Kind::Same).0, 1);
        // At 4 it is the other way round: 8 against 7.1 is close, and the larger file decides.
        let m = [media("/a", 1920, 1080, "av1", 4.0, 1), media("/b", 1920, 1080, "h264", 7.1, 9)];
        assert_eq!(pick(&m, &Prefs::default(), Kind::Same), (1, "largest file".to_string()));
    }

    #[test]
    fn the_rules_apply_in_the_order_the_readme_gives() {
        let prefs = Prefs { prefer: vec!["/lib".into()], avoid: vec!["trash".into()] };
        // Avoided beats resolution.
        let m = [media("/trash/a", 3840, 2160, "h264", 40.0, 9), media("/x/a", 640, 360, "h264", 1.0, 1)];
        assert_eq!(pick(&m, &prefs, Kind::Same), (1, "not in an avoided place".to_string()));
        // Resolution beats bitrate and the preferred folder.
        let m = [media("/lib/a", 1280, 720, "h264", 40.0, 9), media("/x/a", 1920, 1080, "h264", 1.0, 1)];
        assert_eq!(pick(&m, &prefs, Kind::Same), (1, "highest resolution".to_string()));
        // Bitrate beats the preferred folder.
        let m = [media("/lib/a", 1920, 1080, "h264", 1.0, 9), media("/x/a", 1920, 1080, "h264", 4.0, 1)];
        assert_eq!(pick(&m, &prefs, Kind::Same), (1, "highest bitrate for its codec".to_string()));
        // The preferred folder beats the size of the file.
        let m = [media("/x/a", 1920, 1080, "h264", 4.0, 9), media("/lib/a", 1920, 1080, "h264", 4.0, 1)];
        assert_eq!(pick(&m, &prefs, Kind::Same), (1, "preferred folder".to_string()));
        // The size of the file beats the name.
        let m = [media("/a", 1920, 1080, "h264", 4.0, 1), media("/b", 1920, 1080, "h264", 4.0, 2)];
        assert_eq!(pick(&m, &prefs, Kind::Same), (1, "largest file".to_string()));
        // And the name is what is left.
        let m = [media("/b", 1920, 1080, "h264", 4.0, 2), media("/a", 1920, 1080, "h264", 4.0, 2)];
        assert_eq!(pick(&m, &prefs, Kind::Same), (1, "first by name".to_string()));
    }

    #[test]
    fn the_reason_names_every_rule_that_decided_something() {
        let prefs = Prefs { prefer: vec![], avoid: vec!["trash".into()] };
        let m = [media("/trash/a", 1920, 1080, "h264", 4.0, 1), media("/x/a", 640, 360, "h264", 1.0, 1), media("/x/b", 1920, 1080, "h264", 4.0, 1)];
        assert_eq!(pick(&m, &prefs, Kind::Same), (2, "not in an avoided place, highest resolution".to_string()));
    }

    #[test]
    fn the_first_preferred_folder_listed_wins_and_folders_match_whole_names() {
        let prefs = Prefs { prefer: vec!["/one".into(), "/two".into()], avoid: vec![] };
        let m = [media("/two/a", 1920, 1080, "h264", 4.0, 1), media("/one/a", 1920, 1080, "h264", 4.0, 1), media("/zero/a", 1920, 1080, "h264", 4.0, 1)];
        assert_eq!(pick(&m, &prefs, Kind::Same), (1, "preferred folder".to_string()));
        // /one2 is not inside /one.
        assert_eq!(prefs.rank(Path::new("/one2/a")), 2);
        assert_eq!(prefs.rank(Path::new("/one/deep/a")), 0);
        assert_eq!(prefs.rank(Path::new("/two/a")), 1);
    }

    #[test]
    fn avoided_text_is_found_anywhere_in_any_case_and_empty_text_avoids_nothing() {
        let prefs = Prefs { prefer: vec![], avoid: vec!["TRASH".into(), "corbeille é".into(), String::new()] };
        assert!(prefs.avoided(Path::new("/x/.trash/a.mp4")));
        assert!(prefs.avoided(Path::new("/x/a-Trashed.mp4")));
        assert!(prefs.avoided(Path::new("/x/Corbeille É/a.mp4")));
        assert!(!prefs.avoided(Path::new("/x/a.mp4")));
    }

    #[test]
    fn with_a_cut_copy_the_longest_goes_first_unless_it_is_avoided() {
        let prefs = Prefs { prefer: vec![], avoid: vec!["trash".into()] };
        let mut m = vec![media("/trash/full", 640, 360, "h264", 1.0, 9), media("/x/clip", 1920, 1080, "h264", 6.0, 1)];
        m[1].duration_s = 90.0;
        assert_eq!(pick(&m, &prefs, Kind::SameEnd), (1, "not in an avoided place".to_string()));
        // Lengths within a second are not told apart: the next rule decides.
        let mut m = vec![media("/x/a", 640, 360, "h264", 1.0, 9), media("/x/b", 1920, 1080, "h264", 6.0, 1)];
        m[1].duration_s = 599.0;
        assert_eq!(pick(&m, &Prefs::default(), Kind::SameStart), (1, "highest resolution".to_string()));
        // For copies of one whole video the length is not a rule at all.
        m[1].duration_s = 500.0;
        assert_eq!(pick(&m, &Prefs::default(), Kind::Same).0, 1);
    }

    #[test]
    fn copies_marked_identical_are_never_told_apart_by_anything_but_the_name() {
        let mut m = vec![media("/b", 1920, 1080, "h264", 4.0, 7), media("/a", 1920, 1080, "h264", 4.0, 7), media("/c", 1920, 1080, "h264", 4.0, 7)];
        m[0].identical = Some(3);
        m[1].identical = Some(3);
        // The third one is not known to be the same bytes, so this is not an identical set.
        assert_eq!(pick(&m, &Prefs::default(), Kind::Same), (1, "first by name".to_string()));
        m[2].identical = Some(4);
        assert_eq!(pick(&m, &Prefs::default(), Kind::Same), (1, "first by name".to_string()));
        m[2].identical = Some(3);
        assert_eq!(pick(&m, &Prefs::default(), Kind::Same), (1, "identical files, first by name".to_string()));
    }

    #[test]
    fn a_copy_of_unknown_bitrate_does_not_stop_the_choice() {
        let m = [media("/a", 1920, 1080, "h264", f64::NAN, 1), media("/b", 1920, 1080, "h264", 4.0, 1)];
        assert_eq!(pick(&m, &Prefs::default(), Kind::Same).0, 1);
        let m = [media("/b", 1920, 1080, "h264", 0.0, 1), media("/a", 1920, 1080, "h264", 0.0, 1)];
        assert_eq!(pick(&m, &Prefs::default(), Kind::Same), (1, "first by name".to_string()));
    }

    fn numbered(sizes: &[u64]) -> Vec<Media> {
        sizes.iter().enumerate().map(|(i, &size)| media(&format!("/v/{i}.mp4"), 1920, 1080, "h264", 4.0, size)).collect()
    }

    fn link(a: usize, b: usize, kind: Kind, score: f32) -> Link {
        Link { a, b, kind, score }
    }

    #[test]
    fn without_links_there_are_no_groups() {
        assert!(groups(&[], &[], &Prefs::default()).is_empty());
        assert!(groups(&[], &numbered(&[1, 2]), &Prefs::default()).is_empty());
    }

    #[test]
    fn the_copy_kept_comes_first_and_the_others_follow_by_name() {
        let m = numbered(&[1, 2, 3, 9]);
        let links = [link(0, 3, Kind::Same, 1.0), link(1, 3, Kind::Same, 1.0), link(2, 3, Kind::Same, 1.0)];
        let g = groups(&links, &m, &Prefs::default());
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].members, vec![3, 0, 1, 2]);
        assert_eq!(g[0].reason, "largest file");
    }

    #[test]
    fn a_groups_score_is_that_of_its_weakest_link() {
        let m = numbered(&[1, 2, 3]);
        let g = groups(&[link(0, 1, Kind::Same, 0.9), link(1, 2, Kind::Same, 0.76)], &m, &Prefs::default());
        assert_eq!(g[0].score, 0.76);
    }

    #[test]
    fn whole_videos_come_first_and_the_most_space_to_win_leads() {
        let m = numbered(&[50, 10, 50, 40, 90, 80, 7, 6]);
        let links = [link(6, 7, Kind::SameStart, 1.0), link(0, 1, Kind::Same, 1.0), link(2, 3, Kind::Same, 1.0), link(4, 5, Kind::SameEnd, 1.0)];
        let g = groups(&links, &m, &Prefs::default());
        let order: Vec<Vec<usize>> = g.iter().map(|g| g.members.clone()).collect();
        // Whole videos: 40 to win before 10. Then the others: 80 to win before 6.
        assert_eq!(order, vec![vec![2, 3], vec![0, 1], vec![4, 5], vec![6, 7]]);
    }

    #[test]
    fn groups_with_as_much_to_win_are_listed_by_the_name_of_the_copy_kept() {
        let m = numbered(&[9, 1, 9, 1]);
        let g = groups(&[link(2, 3, Kind::Same, 1.0), link(0, 1, Kind::Same, 1.0)], &m, &Prefs::default());
        assert_eq!(g[0].members, vec![0, 1]);
        assert_eq!(g[1].members, vec![2, 3]);
    }

    #[test]
    fn pairs_that_tie_are_listed_the_same_way_every_run() {
        let m = numbered(&[9, 1, 1, 1, 1, 1]);
        let links: Vec<Link> = (1..6).map(|b| link(0, b, Kind::SameStart, 1.0)).collect();
        let order = || groups(&links, &m, &Prefs::default()).iter().map(|g| g.members.clone()).collect::<Vec<_>>();
        let first = order();
        for _ in 0..50 {
            assert_eq!(order(), first);
        }
    }

    #[test]
    fn a_match_at_one_end_inside_a_group_of_copies_adds_nothing() {
        let m = numbered(&[3, 2, 1]);
        let g = groups(&[link(0, 1, Kind::Same, 1.0), link(1, 2, Kind::Same, 1.0), link(0, 2, Kind::SameEnd, 0.9)], &m, &Prefs::default());
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].kind, Kind::Same);
    }

    #[test]
    fn of_two_links_between_groups_the_better_scored_shows_when_neither_names_more_kept_copies() {
        let m = numbered(&[5, 4, 3]);
        let g = groups(&[link(0, 2, Kind::SameStart, 0.8), link(1, 2, Kind::SameEnd, 0.9), link(0, 1, Kind::Same, 1.0)], &m, &Prefs::default());
        assert_eq!(g.len(), 2);
        // 0 is the copy its group keeps, so that pair is shown although the other scored more.
        assert_eq!((g[1].kind, g[1].members.clone(), g[1].score), (Kind::SameStart, vec![0, 2], 0.8));
        // Between two videos on their own, the better of the two links is the one shown.
        let m = numbered(&[5, 4]);
        let g = groups(&[link(0, 1, Kind::SameStart, 0.8), link(0, 1, Kind::SameEnd, 0.9)], &m, &Prefs::default());
        assert_eq!((g.len(), g[0].kind, g[0].score), (1, Kind::SameEnd, 0.9));
    }
}
