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
            bail!("{} was written by a newer av1opt (format {})", p.display(), t.format);
        }
        Ok(t)
    }

    pub fn save(&self) -> Result<PathBuf> {
        let p = path().context("no HOME for the tuning file")?;
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&Tuning { format: FORMAT, classes: self.classes.clone() })?)?;
        std::fs::rename(&tmp, &p)?;
        Ok(p)
    }

    /// The settings for a video of this size, if its class was tuned.
    pub fn pick(&self, width: u32, height: u32) -> Option<&Choice> {
        self.classes.get(&Class::of(width, height))
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
}
