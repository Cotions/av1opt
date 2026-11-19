//! What the page remembers of its forms and views between sessions: the encode dialog's
//! options, the folder the picker was last in, the values of the scan, duplicate and benchmark
//! forms. A browser keeps such things for one origin, and the port, and with it the origin, is
//! new at every start; so they are kept here, in `ui.json`, and sent with `/api/state`.
//!
//! What is kept is no more than a remembered form value. Nothing here is acted on: the page
//! reads it back into its fields, and what it sends from them is checked as typed input is.

use super::App;
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// How many keys the page may keep, how long each name may be, and how much each value and
/// all of them may take as JSON. The page has about twenty; what it keeps of a form is a few
/// hundred bytes, the encode dialog's tags perhaps a few thousand.
const MOST_KEYS: usize = 64;
const MOST_KEY_CHARS: usize = 40;
const MOST_VALUE_BYTES: usize = 16 * 1024;
const MOST_BYTES: usize = 64 * 1024;

/// Folder lists the server keeps in settings of its own, as it fills them itself when a scan,
/// a duplicate search or a benchmark starts: the page's name for each and the field. One list
/// each, so that the page and the server cannot remember two different ones.
const FOLDER_LISTS: [&str; 4] = ["scanRoots", "dupRoots", "dupPrefer", "benchRoots"];
const MOST_FOLDERS: usize = 200;
const MOST_PATH_BYTES: usize = 4096;

fn folder_list<'a>(s: &'a mut super::Settings, key: &str) -> Option<&'a mut Vec<PathBuf>> {
    match key {
        "scanRoots" => Some(&mut s.scan_paths),
        "dupRoots" => Some(&mut s.dup_paths),
        "dupPrefer" => Some(&mut s.dup_prefer),
        "benchRoots" => Some(&mut s.bench_paths),
        _ => None,
    }
}

fn size(v: &Value) -> usize {
    serde_json::to_vec(v).map_or(usize::MAX, |b| b.len())
}

fn good_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= MOST_KEY_CHARS && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.')
}

/// The folders of a list as the page sent them: absolute paths in text, not too many.
fn folders(value: &Value) -> Result<Vec<PathBuf>> {
    let Some(items) = value.as_array() else { bail!("a list of folders has to be a list") };
    if items.len() > MOST_FOLDERS {
        bail!("a list of folders is kept with {MOST_FOLDERS} at most; this one has {}", items.len());
    }
    items
        .iter()
        .map(|item| match item.as_str() {
            Some(p) if p.starts_with('/') && p.len() <= MOST_PATH_BYTES && !p.contains('\0') => Ok(PathBuf::from(p)),
            _ => bail!("a list of folders holds absolute paths, as text"),
        })
        .collect()
}

/// Whether `page` can be kept with `key` set to `value` (or taken out, for null): the limits
/// above, for what the page sends and for what a file read back holds.
fn fits(page: &BTreeMap<String, Value>, key: &str, value: &Value) -> Result<()> {
    if !good_key(key) {
        bail!("a remembered value is named by up to {MOST_KEY_CHARS} letters, digits, dots or underscores");
    }
    if value.is_null() {
        return Ok(());
    }
    let bytes = size(value);
    if bytes > MOST_VALUE_BYTES {
        bail!("a remembered value may take {MOST_VALUE_BYTES} bytes; this one takes {bytes}");
    }
    if !page.contains_key(key) && page.len() >= MOST_KEYS {
        bail!("the page may remember {MOST_KEYS} values, and has as many already");
    }
    let others: usize = page.iter().filter(|(k, _)| *k != key).map(|(k, v)| k.len() + size(v)).sum();
    if others + key.len() + bytes > MOST_BYTES {
        bail!("what the page remembers may take {MOST_BYTES} bytes in all, and this would take more");
    }
    Ok(())
}

/// The page's values as a settings file holds them. A file edited by hand can hold anything
/// there; what is not a map of values is left out, and the file's other settings still count.
pub fn read<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<BTreeMap<String, Value>, D::Error> {
    Ok(match <Value as serde::Deserialize>::deserialize(d)? {
        Value::Object(map) => kept(map.into_iter().collect()),
        _ => BTreeMap::new(),
    })
}

/// What of a settings file's page values is kept: those within the limits, in their order.
/// The rest is left out as if it had never been.
fn kept(read: BTreeMap<String, Value>) -> BTreeMap<String, Value> {
    let mut page = BTreeMap::new();
    for (key, value) in read {
        if !value.is_null() && !FOLDER_LISTS.contains(&key.as_str()) && fits(&page, &key, &value).is_ok() {
            page.insert(key, value);
        }
    }
    page
}

/// `POST /api/prefs` with `{ "key": ..., "value": ... }`: remembers one value, or forgets it
/// for null. One key at a time, so that two tabs each remembering their own keep both.
pub fn set(app: &App, body: &Value) -> Result<Value> {
    let Some(key) = body.get("key").and_then(Value::as_str) else { bail!("a remembered value needs a key, as text") };
    let value = body.get("value").cloned().unwrap_or(Value::Null);
    {
        let mut s = app.settings.lock().unwrap();
        if let Some(list) = folder_list(&mut s, key) {
            *list = if value.is_null() { Vec::new() } else { folders(&value)? };
        } else {
            fits(&s.page, key, &value)?;
            if value.is_null() {
                s.page.remove(key);
            } else {
                s.page.insert(key.to_string(), value);
            }
        }
    }
    app.save_settings();
    Ok(json!({}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_sizes_are_bounded() {
        let page = BTreeMap::new();
        for key in ["", "a b", "a/b", "ä", &"k".repeat(41)] {
            assert!(fits(&page, key, &json!(1)).is_err(), "{key:?}");
        }
        fits(&page, &"k".repeat(40), &json!(1)).unwrap();
        fits(&page, "scan.vmaf_2", &json!({ "a": [1, 2] })).unwrap();
        assert!(fits(&page, "big", &json!("x".repeat(MOST_VALUE_BYTES))).unwrap_err().to_string().contains("16384 bytes"));
        let full: BTreeMap<String, Value> = (0..MOST_KEYS).map(|i| (format!("k{i}"), json!(i))).collect();
        assert!(fits(&full, "one_more", &json!(1)).is_err());
        // A key it has is changed in place, and forgetting is always allowed.
        fits(&full, "k3", &json!("changed")).unwrap();
        fits(&full, "one_more", &Value::Null).unwrap();
        let heavy: BTreeMap<String, Value> = (0..4).map(|i| (format!("k{i}"), json!("x".repeat(15 * 1024)))).collect();
        assert!(fits(&heavy, "k5", &json!("x".repeat(8 * 1024))).unwrap_err().to_string().contains("in all"));
        fits(&heavy, "k0", &json!("x".repeat(8 * 1024))).unwrap();
    }

    #[test]
    fn a_file_keeps_only_what_the_page_could_have_sent() {
        let read: BTreeMap<String, Value> = [
            ("good".to_string(), json!({ "preset": 6 })),
            ("bad key".to_string(), json!(1)),
            ("huge".to_string(), json!("x".repeat(MOST_VALUE_BYTES + 1))),
            ("nothing".to_string(), Value::Null),
            ("scanRoots".to_string(), json!(["/videos"])),
        ]
        .into_iter()
        .collect();
        assert_eq!(kept(read).into_iter().collect::<Vec<_>>(), [("good".to_string(), json!({ "preset": 6 }))]);
    }

    #[test]
    fn folder_lists_hold_absolute_paths_only() {
        assert_eq!(folders(&json!(["/a", "/b c"])).unwrap(), [PathBuf::from("/a"), PathBuf::from("/b c")]);
        for bad in [json!("/a"), json!(["relative"]), json!([1]), json!(["/a\u{0}b"]), json!([format!("/{}", "x".repeat(MOST_PATH_BYTES))])] {
            assert!(folders(&bad).is_err(), "{bad}");
        }
        assert!(folders(&json!(vec!["/a"; MOST_FOLDERS + 1])).is_err());
    }
}
