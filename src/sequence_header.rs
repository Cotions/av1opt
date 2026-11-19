//! The sequence header of an AV1 stream, and whether pieces encoded apart can be joined into
//! one stream: a decoder reads the header again at every key frame that carries one, and some
//! take what the first one said for the whole file (NVIDIA's parser keeps the first header's
//! range), so a piece has to say what the stream it goes into says.

use anyhow::{Result, bail};

/// One field of a sequence header as the AV1 specification names it (section 5.5), with the
/// place of its bits.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub name: String,
    pub value: u64,
    /// Where its bits start in the header, and how many there are.
    pub at: usize,
    pub bits: usize,
}

/// What two sequence headers differ in that does not keep them from being joined, with what
/// each says, as a report says it.
#[derive(Debug, Clone, PartialEq)]
pub struct Difference {
    /// "level" or "superblock size".
    pub what: &'static str,
    pub stream: String,
    pub piece: String,
}

struct Bits<'a> {
    data: &'a [u8],
    at: usize,
    fields: Vec<Field>,
}

impl Bits<'_> {
    fn read(&mut self, bits: usize) -> Result<u64> {
        if self.at + bits > self.data.len() * 8 {
            bail!("the sequence header ends early");
        }
        let mut v = 0u64;
        for _ in 0..bits {
            v = (v << 1) | ((self.data[self.at / 8] >> (7 - self.at % 8)) & 1) as u64;
            self.at += 1;
        }
        Ok(v)
    }

    fn f(&mut self, name: impl Into<String>, bits: usize) -> Result<u64> {
        let at = self.at;
        let value = self.read(bits)?;
        self.fields.push(Field { name: name.into(), value, at, bits });
        Ok(value)
    }

    /// The specification's uvlc(): a count of zeros, a one, then as many bits more.
    fn uvlc(&mut self, name: &str) -> Result<u64> {
        let at = self.at;
        let mut zeros = 0;
        while self.read(1)? == 0 {
            zeros += 1;
            if zeros >= 32 {
                bail!("the sequence header has a number too long to be one");
            }
        }
        let value = self.read(zeros)? + (1u64 << zeros) - 1;
        self.fields.push(Field { name: name.into(), value, at, bits: self.at - at });
        Ok(value)
    }
}

/// Every field of the sequence header `payload` (the OBU's contents, without its OBU header),
/// in the order they are written.
pub fn parse(payload: &[u8]) -> Result<Vec<Field>> {
    let mut b = Bits { data: payload, at: 0, fields: Vec::new() };
    let profile = b.f("seq_profile", 3)?;
    b.f("still_picture", 1)?;
    let reduced = b.f("reduced_still_picture_header", 1)? == 1;
    let mut decoder_model = false;
    let mut buffer_delay_bits = 0;
    if reduced {
        b.f("seq_level_idx[0]", 5)?;
    } else {
        if b.f("timing_info_present_flag", 1)? == 1 {
            b.f("num_units_in_display_tick", 32)?;
            b.f("time_scale", 32)?;
            if b.f("equal_picture_interval", 1)? == 1 {
                b.uvlc("num_ticks_per_picture_minus_1")?;
            }
            decoder_model = b.f("decoder_model_info_present_flag", 1)? == 1;
            if decoder_model {
                buffer_delay_bits = b.f("buffer_delay_length_minus_1", 5)? as usize + 1;
                b.f("num_units_in_decoding_tick", 32)?;
                b.f("buffer_removal_time_length_minus_1", 5)?;
                b.f("frame_presentation_time_length_minus_1", 5)?;
            }
        }
        let display_delay = b.f("initial_display_delay_present_flag", 1)? == 1;
        let points = b.f("operating_points_cnt_minus_1", 5)? + 1;
        for i in 0..points {
            b.f(format!("operating_point_idc[{i}]"), 12)?;
            if b.f(format!("seq_level_idx[{i}]"), 5)? > 7 {
                b.f(format!("seq_tier[{i}]"), 1)?;
            }
            if decoder_model && b.f(format!("decoder_model_present_for_this_op[{i}]"), 1)? == 1 {
                b.f(format!("decoder_buffer_delay[{i}]"), buffer_delay_bits)?;
                b.f(format!("encoder_buffer_delay[{i}]"), buffer_delay_bits)?;
                b.f(format!("low_delay_mode_flag[{i}]"), 1)?;
            }
            if display_delay && b.f(format!("initial_display_delay_present_for_this_op[{i}]"), 1)? == 1 {
                b.f(format!("initial_display_delay_minus_1[{i}]"), 4)?;
            }
        }
    }
    let width_bits = b.f("frame_width_bits_minus_1", 4)? as usize + 1;
    let height_bits = b.f("frame_height_bits_minus_1", 4)? as usize + 1;
    b.f("max_frame_width_minus_1", width_bits)?;
    b.f("max_frame_height_minus_1", height_bits)?;
    if !reduced && b.f("frame_id_numbers_present_flag", 1)? == 1 {
        b.f("delta_frame_id_length_minus_2", 4)?;
        b.f("additional_frame_id_length_minus_1", 3)?;
    }
    b.f("use_128x128_superblock", 1)?;
    b.f("enable_filter_intra", 1)?;
    b.f("enable_intra_edge_filter", 1)?;
    if !reduced {
        b.f("enable_interintra_compound", 1)?;
        b.f("enable_masked_compound", 1)?;
        b.f("enable_warped_motion", 1)?;
        b.f("enable_dual_filter", 1)?;
        let order_hint = b.f("enable_order_hint", 1)? == 1;
        if order_hint {
            b.f("enable_jnt_comp", 1)?;
            b.f("enable_ref_frame_mvs", 1)?;
        }
        let screen_content = if b.f("seq_choose_screen_content_tools", 1)? == 1 { 2 } else { b.f("seq_force_screen_content_tools", 1)? };
        if screen_content > 0 && b.f("seq_choose_integer_mv", 1)? == 0 {
            b.f("seq_force_integer_mv", 1)?;
        }
        if order_hint {
            b.f("order_hint_bits_minus_1", 3)?;
        }
    }
    b.f("enable_superres", 1)?;
    b.f("enable_cdef", 1)?;
    b.f("enable_restoration", 1)?;
    // color_config()
    let high_bitdepth = b.f("high_bitdepth", 1)? == 1;
    let twelve_bit = profile == 2 && high_bitdepth && b.f("twelve_bit", 1)? == 1;
    let mono = profile != 1 && b.f("mono_chrome", 1)? == 1;
    let (mut primaries, mut transfer, mut matrix) = (2, 2, 2);
    if b.f("color_description_present_flag", 1)? == 1 {
        primaries = b.f("color_primaries", 8)?;
        transfer = b.f("transfer_characteristics", 8)?;
        matrix = b.f("matrix_coefficients", 8)?;
    }
    if mono {
        b.f("color_range", 1)?;
    } else if (primaries, transfer, matrix) == (1, 13, 0) {
        // sRGB: full range and 4:4:4, said by saying nothing.
    } else {
        b.f("color_range", 1)?;
        let (x, y) = match profile {
            0 => (1, 1),
            1 => (0, 0),
            _ if twelve_bit => {
                let x = b.f("subsampling_x", 1)?;
                (x, if x == 1 { b.f("subsampling_y", 1)? } else { 0 })
            }
            _ => (1, 0),
        };
        if (x, y) == (1, 1) {
            b.f("chroma_sample_position", 2)?;
        }
    }
    if !mono {
        b.f("separate_uv_delta_q", 1)?;
    }
    b.f("film_grain_params_present", 1)?;
    Ok(b.fields)
}

/// The payload of the first sequence header OBU in `data`, a run of OBUs in the low-overhead
/// format (an IVF frame, or the start of an OBU file); `None` where none comes before the
/// data runs out.
pub fn first_in(data: &[u8]) -> Option<&[u8]> {
    const SEQUENCE_HEADER: u8 = 1;
    let mut i = 0;
    while i < data.len() {
        let header = data[i];
        let (kind, extension, has_size) = ((header >> 3) & 15, (header >> 2) & 1 == 1, (header >> 1) & 1 == 1);
        i += 1 + extension as usize;
        let size = if has_size {
            let mut size = 0usize;
            let mut k = 0;
            loop {
                let byte = *data.get(i)?;
                size |= ((byte & 0x7f) as usize) << (7 * k);
                i += 1;
                k += 1;
                if byte & 0x80 == 0 {
                    break;
                }
                if k == 8 {
                    return None;
                }
            }
            size
        } else {
            data.len().checked_sub(i)?
        };
        let payload = data.get(i..i.checked_add(size)?)?;
        if kind == SEQUENCE_HEADER {
            return Some(payload);
        }
        i += size;
    }
    None
}

/// What a field is about, for those a report names, and whether pieces that differ in it may
/// still be joined. Level and superblock size may: a level says how much a decoder must keep
/// up with (SVT-AV1 picks it from the frame rate and the size of what it encodes: a source
/// declared at 60 fps whose frames come at 30 got level 3.0 for the one pass and 2.1 for a
/// stretch of it), and a superblock size is chosen anew for every sequence (SVT-AV1 preset 5
/// takes 128x128 from CRF 58 up); neither changes how a frame is decoded, and no decoder
/// tried on such joins (dav1d, libaom, Chrome, Firefox) took them otherwise. A different range, bit depth,
/// profile, picture size or chroma layout changes what the frames mean, so does any field
/// not named here, and those are refused.
fn kind_of(name: &str) -> (&'static str, bool) {
    let base = name.split('[').next().unwrap_or(name);
    match base {
        "seq_level_idx" | "seq_tier" => ("level", true),
        "use_128x128_superblock" => ("superblock size", true),
        "color_range" => ("range", false),
        "high_bitdepth" | "twelve_bit" => ("bit depth", false),
        "seq_profile" => ("profile", false),
        "frame_width_bits_minus_1" | "frame_height_bits_minus_1" | "max_frame_width_minus_1" | "max_frame_height_minus_1" => ("picture size", false),
        "mono_chrome" | "subsampling_x" | "subsampling_y" | "chroma_sample_position" => ("chroma layout", false),
        _ => ("", false),
    }
}

/// A value of a field as a report says it.
fn shown(name: &str, value: u64) -> String {
    match name.split('[').next().unwrap_or(name) {
        // Levels 2.0 to 7.3 are numbered from 0, four to a major number; 31 is none in particular.
        "seq_level_idx" if value == 31 => "unconstrained".into(),
        "seq_level_idx" => format!("{}.{}", 2 + value / 4, value % 4),
        "seq_tier" => if value == 1 { "high tier" } else { "main tier" }.into(),
        "use_128x128_superblock" => if value == 1 { "128x128" } else { "64x64" }.into(),
        "color_range" => if value == 1 { "full" } else { "limited" }.into(),
        "max_frame_width_minus_1" | "max_frame_height_minus_1" => (value + 1).to_string(),
        _ => value.to_string(),
    }
}

/// Whether a piece whose sequence header is `piece` can go into a stream whose header is
/// `stream` (both OBU payloads): the harmless differences if it can (none for the same bytes),
/// else why not, naming what differs.
pub fn joinable(stream: &[u8], piece: &[u8]) -> std::result::Result<Vec<Difference>, String> {
    if stream == piece {
        return Ok(Vec::new());
    }
    let (Ok(a), Ok(b)) = (parse(stream), parse(piece)) else {
        return Err("its sequence header cannot be read".into());
    };
    let mut allowed: Vec<Difference> = Vec::new();
    // By name, in the order they are written: a field one header has and the other has not
    // (a tier is written only above level 3.3) differs too. A flag that decides which fields
    // follow comes before them, so it is what a refusal names.
    let value_in = |fields: &[Field], name: &str| fields.iter().find(|f| f.name == name).map(|f| f.value);
    let names: Vec<&str> = a.iter().chain(b.iter().filter(|y| !a.iter().any(|x| x.name == y.name))).map(|f| f.name.as_str()).collect();
    for name in names {
        // An unwritten tier is the main tier.
        let unwritten = name.starts_with("seq_tier").then_some(0);
        let (x, y) = (value_in(&a, name).or(unwritten), value_in(&b, name).or(unwritten));
        if x == y {
            continue;
        }
        let said = |v: Option<u64>| v.map_or("none".to_string(), |v| shown(name, v));
        match kind_of(name) {
            (what, true) => {
                if !allowed.iter().any(|d| d.what == what) {
                    allowed.push(Difference { what, stream: said(x), piece: said(y) });
                }
            }
            ("", false) => return Err(format!("its sequence header differs in {name} ({}, where the stream it goes into says {})", said(y), said(x))),
            (what, false) => return Err(format!("its {what} differs ({name} {}, where the stream it goes into says {})", said(y), said(x))),
        }
    }
    // Where every field agrees the bytes differ after the last (trailing bits), which says
    // nothing of the frames.
    Ok(allowed)
}

/// The note a report gives for `pieces` joined despite `differences` to what `rest` says, or
/// none.
pub fn note(differences: &[Difference], pieces: &str, rest: &str) -> Option<String> {
    if differences.is_empty() {
        return None;
    }
    let said: Vec<String> = differences.iter().map(|d| format!("{} {} where {rest} says {}", d.what, d.piece, d.stream)).collect();
    Some(format!(
        "{pieces} say another {} in their sequence headers ({}): this only tells a decoder what to be ready for, not how to decode, and is left as it is",
        differences.iter().map(|d| d.what).collect::<Vec<_>>().join(" and "),
        said.join("; ")
    ))
}

/// Sequence headers for tests here and of the joins.
#[cfg(test)]
pub mod sample {
    use super::parse;

    /// A sequence header SVT-AV1 wrote through ffmpeg: 640x360 at 30 fps, 10-bit 4:2:0, full
    /// range, level 2.1, 64x64 superblocks.
    pub const FULL_RANGE_640: &str = "0200000d6627fb39d5f3a020203480";

    pub fn bytes(hex: &str) -> Vec<u8> {
        (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect()
    }

    /// `header` with the field `name` set to `value`, in its own bits.
    pub fn with(header: &[u8], name: &str, value: u64) -> Vec<u8> {
        let fields = parse(header).unwrap();
        let f = fields.iter().find(|f| f.name == name).unwrap_or_else(|| panic!("no {name} in {fields:?}"));
        let mut out = header.to_vec();
        for k in 0..f.bits {
            let bit = (value >> (f.bits - 1 - k)) & 1;
            let at = f.at + k;
            out[at / 8] = (out[at / 8] & !(0x80 >> (at % 8))) | ((bit as u8) << (7 - at % 8));
        }
        out
    }

    /// [`FULL_RANGE_640`] with one field set otherwise.
    pub fn header(name: &str, value: u64) -> Vec<u8> {
        with(&bytes(FULL_RANGE_640), name, value)
    }

    /// `rest` after what the first frame of an encode starts with: a temporal delimiter and
    /// the sequence header `header`, each an OBU with its size.
    pub fn starting(header: &[u8], rest: &[u8]) -> Vec<u8> {
        let mut v = vec![0x12, 0x00, 0x0a, header.len() as u8];
        v.extend(header);
        v.extend(rest);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::sample::{FULL_RANGE_640, bytes, with};
    use super::*;

    fn value(header: &[u8], name: &str) -> u64 {
        parse(header).unwrap().into_iter().find(|f| f.name == name).unwrap().value
    }

    #[test]
    fn a_header_svt_av1_wrote_is_read_field_by_field() {
        let h = bytes(FULL_RANGE_640);
        assert_eq!(value(&h, "seq_profile"), 0);
        assert_eq!(value(&h, "seq_level_idx[0]"), 1);
        assert_eq!(value(&h, "max_frame_width_minus_1") + 1, 640);
        assert_eq!(value(&h, "max_frame_height_minus_1") + 1, 360);
        assert_eq!(value(&h, "use_128x128_superblock"), 0);
        assert_eq!(value(&h, "high_bitdepth"), 1);
        assert_eq!(value(&h, "mono_chrome"), 0);
        assert_eq!(value(&h, "color_range"), 1);
        // The same encoder's limited-range header differs in that bit alone.
        assert_eq!(with(&h, "color_range", 0), bytes("0200000d6627fb39d5f3a020202480"));
        // Its header for a source declared at 60 fps differs in the level alone (3.0).
        assert_eq!(with(&h, "seq_level_idx[0]", 4), bytes("020000256627fb39d5f3a020203480"));
    }

    #[test]
    fn pieces_that_differ_in_what_frames_mean_are_refused_and_level_or_superblock_size_are_not() {
        let h = bytes(FULL_RANGE_640);
        assert_eq!(joinable(&h, &h), Ok(Vec::new()));
        for (field, value, what) in [
            ("color_range", 0, "range"),
            ("high_bitdepth", 0, "bit depth"),
            ("max_frame_width_minus_1", 719, "picture size"),
            ("max_frame_height_minus_1", 479, "picture size"),
            ("mono_chrome", 1, "chroma layout"),
            ("chroma_sample_position", 2, "chroma layout"),
        ] {
            let e = joinable(&h, &with(&h, field, value)).unwrap_err();
            assert!(e.contains(&format!("its {what} differs")) && e.contains(field), "{field}: {e}");
        }
        assert_eq!(joinable(&h, &with(&h, "color_range", 0)).unwrap_err(), "its range differs (color_range limited, where the stream it goes into says full)");
        // A profile that lays out the colour fields otherwise is refused by its name too.
        assert!(joinable(&h, &with(&h, "seq_profile", 1)).unwrap_err().contains("its profile differs"));
        // Any other field, by its name in the specification.
        assert!(joinable(&h, &with(&h, "enable_cdef", 0)).unwrap_err().contains("enable_cdef"));
        // Level and superblock size go in, each said once.
        let level = with(&h, "seq_level_idx[0]", 4);
        assert_eq!(joinable(&h, &level), Ok(vec![Difference { what: "level", stream: "2.1".into(), piece: "3.0".into() }]));
        let both = with(&level, "use_128x128_superblock", 1);
        let d = joinable(&h, &both).unwrap();
        assert_eq!(d.iter().map(|d| d.what).collect::<Vec<_>>(), ["level", "superblock size"]);
        assert_eq!(
            note(&d, "the stretches encoded again", "the rest").unwrap(),
            "the stretches encoded again say another level and superblock size in their sequence headers (level 3.0 where the rest says 2.1; superblock size 128x128 where the rest says 64x64): this only tells a decoder what to be ready for, not how to decode, and is left as it is"
        );
        assert_eq!(note(&[], "some scenes", "the first"), None);
        // But not with another range beside them.
        assert!(joinable(&h, &with(&both, "color_range", 0)).unwrap_err().contains("range"));
        // Bytes that are no header.
        assert!(joinable(&h, &[0xff]).is_err());
    }

    #[test]
    fn the_first_sequence_header_is_found_among_the_obus() {
        let h = bytes(FULL_RANGE_640);
        // A temporal delimiter, the header, then a frame: each with its size.
        let mut unit = vec![0x12, 0x00, 0x0a, h.len() as u8];
        unit.extend(&h);
        unit.extend([0x32, 0x03, 1, 2, 3]);
        assert_eq!(first_in(&unit), Some(&h[..]));
        // With an extension byte before the size.
        let mut extended = vec![0x12, 0x00, 0x0e, 0x00, h.len() as u8];
        extended.extend(&h);
        assert_eq!(first_in(&extended), Some(&h[..]));
        // None in a frame that has none, or that is cut off inside it.
        assert_eq!(first_in(&[0x12, 0x00, 0x32, 0x03, 1, 2, 3]), None);
        assert_eq!(first_in(&unit[..10]), None);
        assert_eq!(first_in(&[]), None);
    }
}
