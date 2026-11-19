//! Container tags on the output: keep or drop the source's, and add tags from templates.
//!
//! A template is plain text with `{field}` placeholders. Fields come from a yt-dlp
//! `.info.json` next to the video when there is one, then from the source's own tags, and
//! `{file}` is the file name without extension. `{a|b|c}` takes the first field that has a
//! value. A tag whose placeholders all come up empty is left out rather than written blank.
//! The presets mirror where yt-dlp (`--embed-metadata`) and common players put things.

use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct MetaOptions {
    /// Copy the source's global tags and chapters. Stream languages and track names are
    /// always kept: players need them to pick audio and subtitle tracks.
    pub keep: bool,
    /// Give the output the source's modification time.
    pub keep_dates: bool,
    /// Read `<name>.info.json` (yt-dlp `--write-info-json`) next to the video.
    pub info_json: bool,
    /// Tag name and template, applied in order (later entries win).
    pub tags: Vec<(String, String)>,
}

impl Default for MetaOptions {
    fn default() -> Self {
        MetaOptions { keep: true, keep_dates: false, info_json: true, tags: Vec::new() }
    }
}

#[derive(Serialize)]
pub struct Preset {
    pub id: &'static str,
    pub name: &'static str,
    pub about: &'static str,
    pub tags: &'static [(&'static str, &'static str)],
}

pub const PRESETS: &[Preset] = &[
    Preset {
        id: "youtube",
        name: "YouTube / yt-dlp",
        about: "The tags yt-dlp --embed-metadata writes, filled from the video's .info.json",
        tags: &[
            ("title", "{track|title}"),
            ("date", "{upload_date}"),
            ("description", "{description}"),
            ("synopsis", "{description}"),
            ("purl", "{webpage_url}"),
            ("comment", "{webpage_url}"),
            ("track", "{track_number}"),
            ("artist", "{artist|artists|creator|creators|uploader|uploader_id}"),
            ("composer", "{composer|composers}"),
            ("genre", "{genre|genres}"),
            ("album", "{album}"),
            ("album_artist", "{album_artist|album_artists}"),
            ("disc", "{disc_number}"),
            ("show", "{series}"),
            ("season_number", "{season_number}"),
            ("episode_id", "{episode|episode_id}"),
            ("episode_sort", "{episode_number}"),
        ],
    },
    Preset {
        id: "movie",
        name: "Movie",
        about: "Title, year, genre and plot, as Plex, Jellyfin and Kodi read them",
        tags: &[
            ("title", "{title|file}"),
            ("date", "{release_year|release_date|date|upload_date}"),
            ("genre", "{genre|genres}"),
            ("description", "{description}"),
            ("synopsis", "{synopsis|description}"),
            ("comment", "{comment|webpage_url}"),
        ],
    },
    Preset {
        id: "tv",
        name: "TV episode",
        about: "Show, season and episode numbers, episode title",
        tags: &[
            ("show", "{series|show}"),
            ("season_number", "{season_number}"),
            ("episode_sort", "{episode_number|episode_sort}"),
            ("episode_id", "{episode_id|episode}"),
            ("title", "{episode|title|file}"),
            ("network", "{network|channel}"),
            ("date", "{release_date|date|upload_date}"),
            ("description", "{description}"),
        ],
    },
    Preset {
        id: "music",
        name: "Music video",
        about: "Artist, album and track, as music players read them",
        tags: &[
            ("title", "{track|title|file}"),
            ("artist", "{artist|artists|creator|uploader}"),
            ("album_artist", "{album_artist|album_artists}"),
            ("album", "{album}"),
            ("track", "{track_number|track}"),
            ("genre", "{genre|genres}"),
            ("date", "{release_year|date|upload_date}"),
        ],
    },
];

pub fn preset(id: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.id == id)
}

const MAX_TAGS: usize = 64;
/// Long enough for any YouTube description; well under the kernel's per-argument limit.
const MAX_VALUE: usize = 32 * 1024;
/// yt-dlp info files are mostly format lists; real ones stay far below this.
const MAX_INFO_JSON: u64 = 64 * 1024 * 1024;

/// Checks tag names and templates given by the user.
pub fn validate(tags: &[(String, String)]) -> anyhow::Result<()> {
    if tags.len() > MAX_TAGS {
        anyhow::bail!("at most {MAX_TAGS} tags");
    }
    for (k, v) in tags {
        if k.is_empty() || k.len() > 64 || !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            anyhow::bail!("tag name '{k}': use letters, digits, '_' or '-'");
        }
        if v.len() > MAX_VALUE {
            anyhow::bail!("tag '{k}' is longer than {} KiB", MAX_VALUE / 1024);
        }
    }
    Ok(())
}

/// Parses `KEY=TEMPLATE` from the command line.
pub fn parse_tag(s: &str) -> Result<(String, String), String> {
    let (k, v) = s.split_once('=').ok_or("expected KEY=VALUE")?;
    let tag = (k.trim().to_string(), v.to_string());
    validate(std::slice::from_ref(&tag)).map_err(|e| e.to_string())?;
    Ok(tag)
}

/// Preset tags followed by `extra` (which override them; an empty template removes a tag).
pub fn with_preset(preset_id: Option<&str>, extra: &[(String, String)]) -> anyhow::Result<Vec<(String, String)>> {
    let mut tags: Vec<(String, String)> = match preset_id.filter(|p| !p.is_empty() && *p != "none") {
        Some(id) => preset(id)
            .ok_or_else(|| anyhow::anyhow!("unknown metadata preset '{id}' (one of {})", PRESETS.iter().map(|p| p.id).collect::<Vec<_>>().join(", ")))?
            .tags
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        None => Vec::new(),
    };
    for (k, v) in extra {
        tags.retain(|(t, _)| !t.eq_ignore_ascii_case(k));
        if !v.is_empty() {
            tags.push((k.clone(), v.clone()));
        }
    }
    validate(&tags)?;
    Ok(tags)
}

/// The yt-dlp info file for `input`: `<name>.info.json` in the same folder.
pub fn sidecar(input: &Path) -> Option<(PathBuf, Value)> {
    let stem = input.file_stem()?.to_string_lossy();
    let path = input.with_file_name(format!("{stem}.info.json"));
    // Whoever can write beside the video can leave anything under this name: what is taken for
    // the info file is a plain file of a sensible size, and not a link to some other file.
    let v: Value = serde_json::from_slice(&crate::files::read_regular(&path, MAX_INFO_JSON).ok()?).ok()?;
    v.is_object().then_some((path, v))
}

/// What a resolve found, for the report and the page's preview.
#[derive(Serialize, Default, Debug)]
pub struct Resolved {
    /// Tags to write, in order.
    pub tags: Vec<(String, String)>,
    pub info_json: Option<PathBuf>,
}

/// Fills the templates for one input. `source` are the source's tags (any key case).
pub fn resolve(o: &MetaOptions, input: &Path, source: &BTreeMap<String, String>) -> Resolved {
    let info = if o.info_json && !o.tags.is_empty() { sidecar(input) } else { None };
    let stem = input.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let lookup = |field: &str| -> Option<String> {
        if field == "file" {
            return Some(stem.clone()).filter(|s| !s.is_empty());
        }
        if let Some((_, j)) = &info
            && let Some(v) = field_text(&j[field])
        {
            return Some(v);
        }
        source.iter().find(|(k, _)| k.eq_ignore_ascii_case(field)).map(|(_, v)| v.clone()).filter(|v| !v.trim().is_empty())
    };
    let mut tags: Vec<(String, String)> = Vec::new();
    for (k, t) in &o.tags {
        let Some(mut v) = render(t, &lookup) else { continue };
        if v.len() > MAX_VALUE {
            let mut end = MAX_VALUE;
            while !v.is_char_boundary(end) {
                end -= 1;
            }
            v.truncate(end);
        }
        tags.retain(|(t, _)| !t.eq_ignore_ascii_case(k));
        // Already in the kept source tags as is: nothing to write.
        if o.keep && source.iter().any(|(sk, sv)| sk.eq_ignore_ascii_case(k) && *sv == v) {
            continue;
        }
        tags.push((k.clone(), v));
    }
    Resolved { tags, info_json: info.map(|(p, _)| std::path::absolute(&p).unwrap_or(p)) }
}

/// A yt-dlp field as tag text: lists are joined, numbers keep their integer form.
fn field_text(v: &Value) -> Option<String> {
    let s = match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.as_i64().map_or_else(|| n.to_string(), |i| i.to_string()),
        Value::Array(a) => a.iter().filter_map(field_text).collect::<Vec<_>>().join(", "),
        _ => return None,
    };
    // Tags travel as program arguments and end up in players' lists: a NUL would stop ffmpeg
    // from starting at all (after the whole encode), other control characters are noise.
    let s: String = s.chars().filter(|c| !c.is_control() || matches!(c, '\n' | '\t')).collect();
    Some(s).filter(|s| !s.trim().is_empty())
}

/// Expands a template. None if it has placeholders and none of them found a value.
/// `{{` and `}}` are literal braces.
fn render(t: &str, lookup: &impl Fn(&str) -> Option<String>) -> Option<String> {
    let mut out = String::new();
    let (mut holes, mut filled) = (0, 0);
    let mut rest = t;
    while let Some(i) = rest.find(['{', '}']) {
        out.push_str(&rest[..i]);
        let c = rest.as_bytes()[i];
        if rest[i + 1..].starts_with(c as char) {
            out.push(c as char);
            rest = &rest[i + 2..];
            continue;
        }
        if c == b'}' {
            out.push('}');
            rest = &rest[i + 1..];
            continue;
        }
        let Some(j) = rest[i..].find('}') else {
            out.push_str(&rest[i..]);
            rest = "";
            break;
        };
        holes += 1;
        if let Some(v) = rest[i + 1..i + j].split('|').map(str::trim).find_map(lookup) {
            out.push_str(&v);
            filled += 1;
        }
        rest = &rest[i + j + 1..];
    }
    out.push_str(rest);
    (holes == 0 || filled > 0).then_some(out).filter(|s| !s.trim().is_empty())
}

/// ffmpeg arguments for the output's global metadata, taken from input number `src`.
pub fn ffmpeg_args(o: &MetaOptions, r: &Resolved, src: usize) -> Vec<String> {
    let mut a = if o.keep {
        vec!["-map_metadata".into(), src.to_string(), "-map_chapters".into(), src.to_string()]
    } else {
        // Global only (`:g`): stream languages and track names still follow their streams.
        vec!["-map_metadata:g".into(), "-1".into(), "-map_chapters".into(), "-1".into()]
    };
    for (k, v) in &r.tags {
        a.extend(["-metadata".into(), format!("{k}={v}")]);
    }
    a
}

/// Tags that were asked for but are not in the output (MP4 only stores a fixed set).
pub fn missing(r: &Resolved, out: &BTreeMap<String, String>) -> Vec<String> {
    r.tags.iter().filter(|(k, _)| !out.keys().any(|o| o.eq_ignore_ascii_case(k))).map(|(k, _)| k.clone()).collect()
}

/// Copies the source's modification time to the output.
pub fn copy_mtime(input: &Path, output: &Path) -> std::io::Result<()> {
    let t = std::fs::metadata(input)?.modified()?;
    std::fs::File::options().write(true).open(output)?.set_modified(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(f: &str) -> Option<String> {
        match f {
            "title" => Some("Clip".into()),
            "uploader" => Some("Someone".into()),
            _ => None,
        }
    }

    #[test]
    fn templates() {
        assert_eq!(render("{title}", &lookup).as_deref(), Some("Clip"));
        assert_eq!(render("{artist|uploader}", &lookup).as_deref(), Some("Someone"));
        assert_eq!(render("{nope}", &lookup), None);
        assert_eq!(render("by {nope}", &lookup), None);
        assert_eq!(render("{title} by {nope}", &lookup).as_deref(), Some("Clip by "));
        assert_eq!(render("plain", &lookup).as_deref(), Some("plain"));
        assert_eq!(render("{{x}} {title}", &lookup).as_deref(), Some("{x} Clip"));
        assert_eq!(render("", &lookup), None);
    }

    #[test]
    fn fields() {
        assert_eq!(field_text(&serde_json::json!(20231105)).as_deref(), Some("20231105"));
        assert_eq!(field_text(&serde_json::json!(["a", "b"])).as_deref(), Some("a, b"));
        assert_eq!(field_text(&serde_json::json!(null)), None);
        assert_eq!(field_text(&serde_json::json!({"a": 1})), None);
    }

    #[test]
    fn presets_and_overrides() {
        let t = with_preset(Some("movie"), &[("genre".into(), "Drama".into()), ("comment".into(), String::new())]).unwrap();
        assert!(t.contains(&("genre".into(), "Drama".into())));
        assert!(!t.iter().any(|(k, _)| k == "comment"));
        assert!(with_preset(Some("bogus"), &[]).is_err());
        assert!(validate(&[("bad key".into(), "x".into())]).is_err());
    }

    #[test]
    fn sidecar_and_source() {
        let dir = std::env::temp_dir().join(format!("av1opt-meta-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let video = dir.join("My clip [abc].mp4");
        std::fs::write(&video, b"").unwrap();
        std::fs::write(dir.join("My clip [abc].info.json"), r#"{"title":"From yt","upload_date":"20231105","uploader":"Chan","webpage_url":"https://youtu.be/abc","tags":["x"]}"#).unwrap();
        let o = MetaOptions { tags: with_preset(Some("youtube"), &[]).unwrap(), ..Default::default() };
        let src = BTreeMap::from([("ALBUM".to_string(), "Old album".to_string())]);
        let r = resolve(&o, &video, &src);
        let get = |k: &str| r.tags.iter().find(|(t, _)| t == k).map(|(_, v)| v.as_str());
        assert_eq!(get("title"), Some("From yt"));
        assert_eq!(get("date"), Some("20231105"));
        assert_eq!(get("artist"), Some("Chan"));
        assert_eq!(get("purl"), Some("https://youtu.be/abc"));
        assert_eq!(get("album"), None, "kept from the source already");
        assert_eq!(get("show"), None);
        assert!(r.info_json.is_some());
        let strip = MetaOptions { keep: false, ..o.clone() };
        assert_eq!(resolve(&strip, &video, &src).tags.iter().find(|(t, _)| t == "album").map(|(_, v)| v.as_str()), Some("Old album"));
        let off = MetaOptions { info_json: false, ..o };
        assert_eq!(resolve(&off, &video, &src).tags.iter().find(|(t, _)| t == "title"), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_a_plain_file_is_taken_for_the_info_file() {
        let dir = std::env::temp_dir().join(format!("av1opt-meta-planted-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pipe = std::ffi::CString::new(dir.join("piped.info.json").to_str().unwrap()).unwrap();
        // SAFETY: `pipe` is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(pipe.as_ptr(), 0o644) }, 0);
        let (tell, told) = std::sync::mpsc::channel();
        let video = dir.join("piped.mp4");
        std::thread::spawn(move || {
            let _ = tell.send(sidecar(&video).is_none());
        });
        assert_eq!(told.recv_timeout(std::time::Duration::from_secs(1)), Ok(true));
        // Nor is what some other file holds taken in through a link to it.
        std::fs::write(dir.join("private.json"), r#"{"title":"Private"}"#).unwrap();
        std::os::unix::fs::symlink(dir.join("private.json"), dir.join("linked.info.json")).unwrap();
        assert!(sidecar(&dir.join("linked.mp4")).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
