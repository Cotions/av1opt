//! Perceptual hashes of small grayscale frames: the same picture at another resolution, bitrate
//! or codec, or inside black bars, gives nearly the same 63 bits.

/// Side of the square thumbnails ffmpeg hands over.
pub const THUMB: usize = 64;
/// Side of the picture the hash is computed from (after cutting black bars).
const SIDE: usize = 32;
/// Low frequencies kept per axis.
const FREQ: usize = 8;

/// The part of a thumbnail that holds picture rather than black bars: `[x0, x1) x [y0, y1)`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bounds {
    pub x0: usize,
    pub x1: usize,
    pub y0: usize,
    pub y1: usize,
}

impl Bounds {
    pub const FULL: Bounds = Bounds { x0: 0, x1: THUMB, y0: 0, y1: THUMB };
}

/// Mean and standard deviation of a frame's pixels.
fn spread(px: &[u8]) -> (f64, f64) {
    let n = px.len().max(1) as f64;
    let mean = px.iter().map(|&p| p as f64).sum::<f64>() / n;
    let var = px.iter().map(|&p| (p as f64 - mean).powi(2)).sum::<f64>() / n;
    (mean, var.sqrt())
}

/// A frame with almost no detail (black, a fade, a plain colour) hashes to noise and would
/// match every other such frame, so it is left out of comparisons.
pub fn is_flat(px: &[u8]) -> bool {
    spread(px).1 < 5.0
}

/// Finds black bars shared by all `frames` (each `THUMB * THUMB` bytes). A row or column counts
/// as a bar only if it is dark in every frame that shows something, so one dark scene doesn't
/// cut into the picture. At most 40% is cut from each side.
pub fn content_bounds(frames: &[&[u8]]) -> Bounds {
    const DARK: f64 = 12.0;
    let max_cut = THUMB * 2 / 5;
    let mut row_max = [0.0_f64; THUMB];
    let mut col_max = [0.0_f64; THUMB];
    let mut used = 0;
    for f in frames.iter().filter(|f| f.len() == THUMB * THUMB && !is_flat(f)) {
        used += 1;
        for y in 0..THUMB {
            let m = f[y * THUMB..(y + 1) * THUMB].iter().map(|&p| p as f64).sum::<f64>() / THUMB as f64;
            row_max[y] = row_max[y].max(m);
        }
        for x in 0..THUMB {
            let m = (0..THUMB).map(|y| f[y * THUMB + x] as f64).sum::<f64>() / THUMB as f64;
            col_max[x] = col_max[x].max(m);
        }
    }
    if used == 0 {
        return Bounds::FULL;
    }
    let lead = |v: &[f64; THUMB]| v.iter().take_while(|&&m| m <= DARK).count().min(max_cut);
    let trail = |v: &[f64; THUMB]| v.iter().rev().take_while(|&&m| m <= DARK).count().min(max_cut);
    Bounds { x0: lead(&col_max), x1: THUMB - trail(&col_max), y0: lead(&row_max), y1: THUMB - trail(&row_max) }
}

/// Box-filters the `b` part of a thumbnail down (or up) to `SIDE x SIDE`.
fn resample(px: &[u8], b: Bounds) -> [f64; SIDE * SIDE] {
    // Weights of the source pixels under destination pixel `i` along one axis.
    let spans = |lo: usize, hi: usize| -> Vec<Vec<(usize, f64)>> {
        let scale = (hi - lo) as f64 / SIDE as f64;
        (0..SIDE)
            .map(|i| {
                let (a, z) = (lo as f64 + i as f64 * scale, lo as f64 + (i + 1) as f64 * scale);
                let mut w = Vec::new();
                let mut s = a.floor() as usize;
                while (s as f64) < z && s < hi {
                    let cover = (z.min(s as f64 + 1.0) - a.max(s as f64)).max(0.0);
                    if cover > 1e-9 {
                        w.push((s, cover / scale));
                    }
                    s += 1;
                }
                w
            })
            .collect()
    };
    let (xs, ys) = (spans(b.x0, b.x1), spans(b.y0, b.y1));
    let mut out = [0.0; SIDE * SIDE];
    for (j, wy) in ys.iter().enumerate() {
        for (i, wx) in xs.iter().enumerate() {
            let mut v = 0.0;
            for &(y, fy) in wy {
                for &(x, fx) in wx {
                    v += px[y * THUMB + x] as f64 * fy * fx;
                }
            }
            out[j * SIDE + i] = v;
        }
    }
    out
}

/// 63-bit perceptual hash of one thumbnail: the signs of its 63 lowest non-constant DCT
/// frequencies against their median. None for frames without enough detail.
pub fn hash(px: &[u8], bounds: Bounds) -> Option<u64> {
    if px.len() != THUMB * THUMB || is_flat(px) {
        return None;
    }
    let img = resample(px, bounds);
    // After cutting bars the rest can be flat too (bars around a fade).
    let mean = img.iter().sum::<f64>() / img.len() as f64;
    let sd = (img.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / img.len() as f64).sqrt();
    if sd < 5.0 {
        return None;
    }
    let cos: Vec<f64> = (0..FREQ * SIDE)
        .map(|k| {
            let (u, x) = (k / SIDE, k % SIDE);
            ((2 * x + 1) as f64 * u as f64 * std::f64::consts::PI / (2 * SIDE) as f64).cos()
        })
        .collect();
    // Rows first, then columns: only the FREQ lowest frequencies of each are needed.
    let mut rows = [0.0; SIDE * FREQ];
    for y in 0..SIDE {
        for u in 0..FREQ {
            rows[y * FREQ + u] = (0..SIDE).map(|x| img[y * SIDE + x] * cos[u * SIDE + x]).sum();
        }
    }
    let mut coef = [0.0; FREQ * FREQ];
    for v in 0..FREQ {
        for u in 0..FREQ {
            coef[v * FREQ + u] = (0..SIDE).map(|y| rows[y * FREQ + u] * cos[v * SIDE + y]).sum();
        }
    }
    // coef[0] is the average brightness: dropped, so brighter or darker copies still match.
    let mut sorted: Vec<f64> = coef[1..].to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let median = sorted[sorted.len() / 2];
    Some(coef[1..].iter().enumerate().fold(0u64, |h, (i, &c)| if c > median { h | 1 << i } else { h }))
}

/// Number of differing bits.
pub fn distance(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A deterministic picture with structure at several scales; `seed` picks another picture.
    pub fn picture(seed: u32, w: usize, h: usize) -> Vec<u8> {
        let s = seed as f64;
        (0..w * h)
            .map(|i| {
                let (x, y) = ((i % w) as f64 / w as f64, (i / w) as f64 / h as f64);
                let v = 128.0
                    + 60.0 * ((x * (3.0 + s % 5.0) + s).sin() * (y * (2.0 + s % 3.0) + s * 0.7).cos())
                    + 40.0 * ((x + y * (1.0 + s % 4.0)) * (5.0 + s % 7.0)).sin()
                    + 20.0 * ((x * 11.0 - y * 13.0 + s) * 2.0).cos();
                v.clamp(0.0, 255.0) as u8
            })
            .collect()
    }

    /// Nearest-neighbour scale of a `w x h` picture into the `into` box of a black thumbnail.
    pub fn thumb(src: &[u8], w: usize, h: usize, into: Bounds) -> Vec<u8> {
        let mut t = vec![0u8; THUMB * THUMB];
        for y in into.y0..into.y1 {
            for x in into.x0..into.x1 {
                let sx = (x - into.x0) * w / (into.x1 - into.x0);
                let sy = (y - into.y0) * h / (into.y1 - into.y0);
                t[y * THUMB + x] = src[sy * w + sx];
            }
        }
        t
    }

    #[test]
    fn same_picture_survives_scaling_and_noise() {
        let big = picture(3, 640, 360);
        let a = thumb(&big, 640, 360, Bounds::FULL);
        // The same picture from a quarter-size, slightly brighter and noisier copy.
        let small: Vec<u8> = (0..160 * 90).map(|i| big[(i / 160) * 4 * 640 + (i % 160) * 4].saturating_add(12 + (i % 3) as u8)).collect();
        let b = thumb(&small, 160, 90, Bounds::FULL);
        let (ha, hb) = (hash(&a, Bounds::FULL).unwrap(), hash(&b, Bounds::FULL).unwrap());
        assert!(distance(ha, hb) <= 4, "distance {}", distance(ha, hb));
    }

    #[test]
    fn different_pictures_are_far_apart() {
        let h: Vec<u64> = (1..8).map(|s| hash(&thumb(&picture(s, 320, 180), 320, 180, Bounds::FULL), Bounds::FULL).unwrap()).collect();
        for i in 0..h.len() {
            for j in i + 1..h.len() {
                assert!(distance(h[i], h[j]) > 14, "pictures {i} and {j}: distance {}", distance(h[i], h[j]));
            }
        }
    }

    #[test]
    fn flat_frames_have_no_hash() {
        assert_eq!(hash(&vec![0u8; THUMB * THUMB], Bounds::FULL), None);
        assert_eq!(hash(&vec![200u8; THUMB * THUMB], Bounds::FULL), None);
        let near_black: Vec<u8> = (0..THUMB * THUMB).map(|i| (i % 3) as u8).collect();
        assert_eq!(hash(&near_black, Bounds::FULL), None);
    }

    #[test]
    fn black_bars_are_found_and_ignored() {
        let pic = picture(5, 640, 272);
        let full = thumb(&pic, 640, 272, Bounds::FULL);
        // Letterboxed into a 4:3 frame, and pillarboxed.
        let letter = Bounds { x0: 0, x1: THUMB, y0: 14, y1: 50 };
        let pillar = Bounds { x0: 20, x1: 44, y0: 0, y1: THUMB };
        for boxed in [letter, pillar] {
            let t = thumb(&pic, 640, 272, boxed);
            let found = content_bounds(&[&t]);
            assert_eq!(found, boxed);
            let d = distance(hash(&full, Bounds::FULL).unwrap(), hash(&t, found).unwrap());
            // Within what counts as the same picture, even from these roughly scaled thumbnails.
            assert!(d <= 10, "{boxed:?}: distance {d}");
        }
        assert_eq!(content_bounds(&[&full]), Bounds::FULL);
    }

    #[test]
    fn a_dark_scene_does_not_move_the_bounds() {
        let pic = thumb(&picture(2, 320, 180), 320, 180, Bounds::FULL);
        // Second frame: black top half with detail below.
        let mut dark = pic.clone();
        dark[..THUMB * THUMB / 2].fill(0);
        assert_eq!(content_bounds(&[&pic, &dark]), Bounds::FULL);
        // On its own it would lose the capped 40%.
        assert_eq!(content_bounds(&[&dark]).y0, THUMB * 2 / 5);
        // Flat frames say nothing about bars.
        assert_eq!(content_bounds(&[&vec![0u8; THUMB * THUMB]]), Bounds::FULL);
    }
}
