//! Encoder settings chosen from benchmark results, one per resolution class, kept in
//! `$XDG_DATA_HOME/av1opt/tuning.json`. `encode --tuned` uses them; nothing else does.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Version of `tuning.json`. Bump it when a field changes meaning or is removed; a newer file
/// is refused rather than misread.
pub const FORMAT: u32 = 1;

/// Videos are tuned by size class: encoder settings that pay off at 4K often don't at 480p.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    Sd,
    Hd,
    Uhd,
}

impl Class {
    pub const ALL: [Class; 3] = [Class::Sd, Class::Hd, Class::Uhd];

    /// By pixel count, so portrait and cropped videos land with their landscape peers. The 15%
    /// margin keeps 1920x1088 and the like with 1080p.
    pub fn of(width: u32, height: u32) -> Class {
        let px = width as f64 * height as f64;
        if px <= 1280.0 * 720.0 * 1.15 {
            Class::Sd
        } else if px <= 1920.0 * 1080.0 * 1.15 {
            Class::Hd
        } else {
            Class::Uhd
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Class::Sd => "720p and below",
            Class::Hd => "1080p",
            Class::Uhd => "above 1080p",
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Class::Sd => "sd",
            Class::Hd => "hd",
            Class::Uhd => "uhd",
        }
    }
}

impl std::str::FromStr for Class {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Class> {
        Class::ALL.into_iter().find(|c| c.id() == s).with_context(|| format!("unknown class '{s}' (sd, hd, uhd)"))
    }
}

/// The settings for one class, with where they came from (shown to the user, not used).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Choice {
    pub preset: u8,
    pub params: String,
    /// Size and encode time against the default settings in the benchmark (1.0 = the same).
    #[serde(default)]
    pub size: f64,
    #[serde(default)]
    pub time: f64,
    /// Videos of this class the benchmark measured.
    #[serde(default)]
    pub videos: usize,
    #[serde(default)]
    pub target_vmaf: f64,
    /// Unix time the choice was saved.
    #[serde(default)]
    pub made: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Tuning {
    pub format: u32,
    pub classes: BTreeMap<Class, Choice>,
}

fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    Some(base.join("av1opt/tuning.json"))
}

impl Tuning {
    /// The saved tuning; empty when there is none yet.
    pub fn load() -> Result<Tuning> {
        let Some(p) = path() else { return Ok(Tuning::default()) };
        let bytes = match std::fs::read(&p) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Tuning::default()),
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", p.display())),
        };
        let t: Tuning = serde_json::from_slice(&bytes).with_context(|| format!("{} is damaged", p.display()))?;
        if t.format > FORMAT {
            bail!("{} was saved by a newer version of av1opt (format {}) and was left as it is; update av1opt to use it, or encode without --tuned", p.display(), t.format);
        }
        Ok(t)
    }

    pub fn save(&self) -> Result<PathBuf> {
        let p = path().context("no HOME for the tuning file")?;
        if let Some(dir) = p.parent() {
            crate::files::private_folder(dir)?;
        }
        crate::files::replace_with(&p, &serde_json::to_vec_pretty(&Tuning { format: FORMAT, classes: self.classes.clone() })?)?;
        Ok(p)
    }

    /// The settings for a video of this size, if its class was tuned.
    pub fn pick(&self, width: u32, height: u32) -> Option<&Choice> {
        self.classes.get(&Class::of(width, height))
    }

    /// The preset a video of this size is encoded at: that of its class, if it was tuned.
    pub fn preset_for(&self, width: u32, height: u32, otherwise: u8) -> u8 {
        self.pick(width, height).map_or(otherwise, |c| c.preset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes_go_by_pixel_count() {
        for (w, h, want) in [
            (640, 360, Class::Sd), (1280, 720, Class::Sd), (720, 1280, Class::Sd), (1440, 720, Class::Sd),
            (1920, 1080, Class::Hd), (1080, 1920, Class::Hd), (1920, 1088, Class::Hd), (1920, 800, Class::Hd),
            (2560, 1440, Class::Uhd), (3840, 2160, Class::Uhd), (3840, 1600, Class::Uhd),
        ] {
            assert_eq!(Class::of(w, h), want, "{w}x{h}");
        }
        for c in Class::ALL {
            assert_eq!(c.id().parse::<Class>().unwrap(), c);
        }
        assert!("4k".parse::<Class>().is_err());
    }

    #[test]
    fn file_layout_is_stable() {
        let mut t = Tuning::default();
        t.classes.insert(Class::Hd, Choice { preset: 4, params: "tune=0:enable-qm=1".into(), size: 0.93, time: 1.8, videos: 5, target_vmaf: 93.0, made: 7 });
        let v = serde_json::to_value(&t).unwrap();
        assert_eq!(v["classes"]["hd"]["preset"], 4);
        assert_eq!(v["classes"]["hd"]["params"], "tune=0:enable-qm=1");
        // Files with only the settings (written by hand) are fine.
        let back: Tuning = serde_json::from_str(r#"{"format":1,"classes":{"uhd":{"preset":8,"params":"tune=0"}}}"#).unwrap();
        assert_eq!(back.pick(3840, 2160).unwrap().preset, 8);
        assert!(back.pick(1920, 1080).is_none());
    }

    #[test]
    fn the_margin_ends_exactly_fifteen_percent_above_720p_and_1080p() {
        assert_eq!(Class::of(1_059_840, 1), Class::Sd);
        assert_eq!(Class::of(1_059_841, 1), Class::Hd);
        assert_eq!(Class::of(2_384_640, 1), Class::Hd);
        assert_eq!(Class::of(2_384_641, 1), Class::Uhd);
    }

    #[test]
    fn sizes_between_the_common_ones_land_where_their_pixels_say() {
        for (w, h, want) in [
            (854, 480, Class::Sd), (1366, 768, Class::Sd), (1600, 900, Class::Hd), (1440, 1080, Class::Hd),
            (2048, 1152, Class::Hd), (2560, 1080, Class::Uhd), (4096, 2160, Class::Uhd), (7680, 4320, Class::Uhd),
        ] {
            assert_eq!(Class::of(w, h), want, "{w}x{h}");
            assert_eq!(Class::of(h, w), want, "{h}x{w}");
        }
    }

    #[test]
    fn sizes_without_a_picture_or_beyond_any_picture_do_not_overflow() {
        assert_eq!(Class::of(0, 0), Class::Sd);
        assert_eq!(Class::of(3840, 0), Class::Sd);
        assert_eq!(Class::of(u32::MAX, u32::MAX), Class::Uhd);
    }

    #[test]
    fn a_class_is_named_exactly_or_not_at_all() {
        assert_eq!(Class::ALL.map(Class::label), ["720p and below", "1080p", "above 1080p"]);
        assert_eq!(Class::ALL.map(Class::id), ["sd", "hd", "uhd"]);
        for bad in ["", "HD", " hd", "hd ", "1080p", "ultra"] {
            let e = bad.parse::<Class>().unwrap_err().to_string();
            assert!(e.contains(&format!("'{bad}'")) && e.contains("sd, hd, uhd"), "{e}");
        }
    }

    #[test]
    fn a_video_gets_the_settings_of_its_own_class_only() {
        let choice = |preset| Choice { preset, params: "tune=0".into(), size: 1.0, time: 1.0, videos: 1, target_vmaf: 93.0, made: 0 };
        let mut t = Tuning::default();
        assert_eq!(t.pick(1920, 1080), None);
        t.classes.insert(Class::Sd, choice(8));
        t.classes.insert(Class::Uhd, choice(4));
        assert_eq!(t.pick(720, 1280).unwrap().preset, 8);
        assert_eq!(t.pick(2160, 3840).unwrap().preset, 4);
        assert_eq!(t.pick(1920, 1080), None);
        assert_eq!((t.preset_for(3840, 2160, 5), t.preset_for(1920, 1080, 5)), (4, 5));
    }

    #[test]
    fn tuning_survives_the_file_and_is_written_smallest_class_first() {
        let choice = |preset| Choice { preset, params: "tune=0:enable-qm=1".into(), size: 0.9, time: 1.5, videos: 3, target_vmaf: 93.0, made: 12 };
        let t = Tuning { format: FORMAT, classes: [(Class::Uhd, choice(4)), (Class::Sd, choice(8)), (Class::Hd, choice(6))].into() };
        let text = serde_json::to_string(&t).unwrap();
        assert_eq!(serde_json::from_str::<Tuning>(&text).unwrap(), t);
        let at = |key: &str| text.find(key).unwrap();
        assert!(at("\"sd\"") < at("\"hd\"") && at("\"hd\"") < at("\"uhd\""), "{text}");
    }

    #[test]
    fn a_file_without_the_settings_themselves_is_refused() {
        for bad in [
            r#"{"format":1}"#,
            r#"{"classes":{}}"#,
            r#"{"format":1,"classes":{"hd":{"params":"tune=0"}}}"#,
            r#"{"format":1,"classes":{"hd":{"preset":6}}}"#,
            r#"{"format":1,"classes":{"hd":{"preset":300,"params":""}}}"#,
            r#"{"format":1,"classes":{"8k":{"preset":6,"params":""}}}"#,
        ] {
            assert!(serde_json::from_str::<Tuning>(bad).is_err(), "{bad}");
        }
    }
}
