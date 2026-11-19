//! The numbers a request may carry, with the ranges the command line gives the same settings.
//! They are looked at in the request as sent, before it is read into its types: a value that
//! does not fit there would otherwise come back as a message that names no setting, and a value
//! left out still means the default.

use anyhow::{Result, anyhow};
use serde_json::Value;

/// How much of a refused value the message repeats.
const SHOWN_CHARS: usize = 40;

pub struct Field {
    key: &'static str,
    label: &'static str,
    low: f64,
    low_allowed: bool,
    high: f64,
    whole: bool,
    /// A list of such numbers.
    list: bool,
    note: &'static str,
}

const fn number(key: &'static str, label: &'static str, low: f64, high: f64) -> Field {
    Field { key, label, low, low_allowed: true, high, whole: false, list: false, note: "" }
}

const fn whole(key: &'static str, label: &'static str, low: f64, high: f64) -> Field {
    Field { whole: true, ..number(key, label, low, high) }
}

/// As `parse_target` in main.rs.
pub const TARGET_VMAF: Field = Field { low_allowed: false, ..number("target_vmaf", "target VMAF", 0.0, 100.0) };
pub const PRESET: Field = whole("preset", "preset", 0.0, 13.0);
pub const WORKERS: Field = whole("workers", "workers", 1.0, 8.0);
/// As `parse_min_savings` in main.rs: near 1 no encode could pass.
const MIN_SAVINGS: Field = Field { note: " (0.2 is 20%)", ..number("min_savings", "minimum savings", 0.0, 0.95) };
const PRESETS: Field = Field { list: true, ..whole("presets", "each of the presets", 0.0, 13.0) };
// The command line narrows these two to the same ranges with a clamp. It takes any number of
// videos; the page's limit stays, a benchmark of more would run for days.
const MAX_VIDEOS: Field = whole("max_videos", "videos", 1.0, 50.0);
const SAMPLES: Field = whole("samples", "clips per video", 1.0, 8.0);
const SAMPLE_SECS: Field = number("sample_secs", "clip length in seconds", 2.0, 30.0);
const TOLERANCE: Field = whole("tolerance", "tolerance", 0.0, 20.0);

pub const JOB: &[Field] = &[TARGET_VMAF, PRESET, WORKERS, MIN_SAVINGS];
pub const SCAN: &[Field] = &[TARGET_VMAF, PRESET, MIN_SAVINGS];
pub const BENCH: &[Field] = &[TARGET_VMAF, PRESETS, MAX_VIDEOS, SAMPLES, SAMPLE_SECS];
pub const DUPS: &[Field] = &[TOLERANCE];

impl Field {
    fn holds(&self, n: f64) -> bool {
        (n > self.low || (self.low_allowed && n == self.low)) && n <= self.high && (!self.whole || n.fract() == 0.0)
    }

    fn takes(&self, value: &Value) -> bool {
        // A whole number written as 5.0 is no whole number to the types behind these fields.
        let n = if self.whole { value.as_u64().map(|n| n as f64) } else { value.as_f64() };
        n.is_some_and(|n| self.holds(n))
    }

    fn refusal(&self, shown: &str) -> anyhow::Error {
        let range = match (self.whole, self.low_allowed) {
            (true, _) => format!("a whole number from {} to {}", self.low, self.high),
            (false, true) => format!("a number from {} to {}", self.low, self.high),
            (false, false) => format!("a number above {} and up to {}", self.low, self.high),
        };
        anyhow!("{} must be {range}{}; it was {shown}", self.label, self.note)
    }

    /// For a value that is already a number, such as one in a saved job.
    pub fn check(&self, n: f64) -> Result<()> {
        if self.holds(n) { Ok(()) } else { Err(self.refusal(&n.to_string())) }
    }
}

fn shown(value: &Value) -> String {
    let text = match value {
        Value::Null => return "empty".to_string(),
        other => other.to_string(),
    };
    let mut chars = text.chars();
    let start: String = chars.by_ref().take(SHOWN_CHARS).collect();
    if chars.next().is_some() { format!("{start}…") } else { start }
}

/// A run of text without a space longer than this is shortened in a message to the page.
const LONG_WORD: usize = 100;
/// What a message to the page holds at most.
const LONG_MESSAGE: usize = 1000;

/// A message as the page is sent it. Messages repeat what they refuse: a path, a parameter
/// list, the value serde could not read. A value of 12 KB came back whole and the page's notice
/// covered the dialog it was about. Each run of text without a space keeps its start and its
/// end (where a path has its file name) and says how long it was; a message of many short
/// words ends after `LONG_MESSAGE` characters.
pub fn shortened(message: &str) -> String {
    let mut out = String::with_capacity(message.len().min(LONG_MESSAGE * 4));
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        let n = word.chars().count();
        if n > LONG_WORD {
            // What closes the value in the sentence stays after the count.
            let value = word.trim_end_matches([':', ';', ',', ')', '\'', '"']);
            let (closing, n) = (&word[value.len()..], value.chars().count());
            let start: String = value.chars().take(SHOWN_CHARS).collect();
            let end: String = value.chars().skip(n.saturating_sub(SHOWN_CHARS * 3 / 4)).collect();
            out.push_str(&format!("{start}…{end} ({n} characters){closing}"));
        } else {
            out.push_str(word);
        }
        word.clear();
    };
    for c in message.chars() {
        if c.is_whitespace() {
            flush(&mut word, &mut out);
            out.push(c);
        } else {
            word.push(c);
        }
    }
    flush(&mut word, &mut out);
    let n = out.chars().count();
    if n > LONG_MESSAGE {
        let start: String = out.chars().take(LONG_MESSAGE).collect();
        return format!("{start}… ({n} characters in all)");
    }
    out
}

/// Refuses the first of `fields` that `body` has and that is out of its range or no number.
pub fn check(body: &Value, fields: &[Field]) -> Result<()> {
    for field in fields {
        let Some(value) = body.get(field.key) else { continue };
        let bad = match (field.list, value) {
            (true, Value::Array(items)) => items.iter().find(|item| !field.takes(item)),
            (true, other) => Some(other),
            (false, one) => (!field.takes(one)).then_some(one),
        };
        if let Some(bad) = bad {
            return Err(field.refusal(&shown(bad)));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use serde_json::json;

    fn refusal(body: Value, fields: &[Field]) -> String {
        check(&body, fields).unwrap_err().to_string()
    }

    #[test]
    fn values_out_of_range_are_refused_by_the_name_of_their_setting() {
        assert_eq!(refusal(json!({ "target_vmaf": 0 }), JOB), "target VMAF must be a number above 0 and up to 100; it was 0");
        assert_eq!(refusal(json!({ "target_vmaf": 100.5 }), SCAN), "target VMAF must be a number above 0 and up to 100; it was 100.5");
        assert_eq!(refusal(json!({ "preset": 14 }), JOB), "preset must be a whole number from 0 to 13; it was 14");
        assert_eq!(refusal(json!({ "preset": 1.5 }), SCAN), "preset must be a whole number from 0 to 13; it was 1.5");
        assert_eq!(refusal(json!({ "workers": 0 }), JOB), "workers must be a whole number from 1 to 8; it was 0");
        assert_eq!(refusal(json!({ "min_savings": 1.2 }), JOB), "minimum savings must be a number from 0 to 0.95 (0.2 is 20%); it was 1.2");
        assert_eq!(refusal(json!({ "tolerance": -1 }), DUPS), "tolerance must be a whole number from 0 to 20; it was -1");
        assert_eq!(refusal(json!({ "presets": [4, 14] }), BENCH), "each of the presets must be a whole number from 0 to 13; it was 14");
        assert!(refusal(json!({ "presets": 4 }), BENCH).contains("it was 4"));
        assert!(refusal(json!({ "max_videos": 51 }), BENCH).starts_with("videos must"));
        assert!(refusal(json!({ "samples": 9 }), BENCH).starts_with("clips per video must"));
        assert!(refusal(json!({ "sample_secs": 1 }), BENCH).starts_with("clip length in seconds must"));
    }

    #[test]
    fn what_a_page_sends_for_a_field_it_could_not_read_is_refused_too() {
        // NaN becomes null on its way through JSON.
        assert_eq!(refusal(json!({ "target_vmaf": null }), JOB), "target VMAF must be a number above 0 and up to 100; it was empty");
        assert!(refusal(json!({ "workers": "two" }), JOB).ends_with("it was \"two\""));
        let long = refusal(json!({ "preset": "x".repeat(5000) }), JOB);
        assert!(long.len() < 120 && long.ends_with('…'), "{long}");
    }

    #[test]
    fn a_message_quotes_a_long_value_by_its_ends_and_its_length() {
        assert_eq!(shortened("cannot open /videos/a b.mkv"), "cannot open /videos/a b.mkv");
        let path = format!("/videos/{}/last.mkv", "x".repeat(12_000));
        let said = shortened(&format!("cannot open {path}: File name too long"));
        assert!(said.starts_with("cannot open /videos/xxx") && said.ends_with("x/last.mkv (12017 characters): File name too long"), "{said}");
        assert!(said.len() < 140, "{said}");
        // A long value in short words.
        let said = shortened(&format!("SVT-AV1 refuses '{}'", "a ".repeat(6000)));
        assert!(said.chars().count() < LONG_MESSAGE + 40 && said.ends_with("characters in all)"), "{said}");
        // Letters of more than one byte are cut between letters.
        let said = shortened(&"é".repeat(500));
        assert!(said.contains('…') && said.ends_with("(500 characters)"), "{said}");
    }

    #[test]
    fn good_and_left_out_values_pass() {
        check(&json!({}), JOB).unwrap();
        check(&json!({ "target_vmaf": 100, "preset": 0, "workers": 8, "min_savings": 0 }), JOB).unwrap();
        check(&json!({ "target_vmaf": 0.5, "preset": 13, "min_savings": 0.95 }), SCAN).unwrap();
        check(&json!({ "presets": [0, 13], "max_videos": 50, "samples": 1, "sample_secs": 2.5 }), BENCH).unwrap();
        check(&json!({ "tolerance": 0 }), DUPS).unwrap();
    }

    #[test]
    fn the_ranges_are_those_of_the_command_line() {
        for text in ["0", "0.01", "50", "99.9", "100", "100.01", "-1", "nan", "inf"] {
            let n: f64 = text.parse().unwrap();
            assert_eq!(TARGET_VMAF.check(n).is_ok(), crate::parse_target(text).is_ok(), "{text}");
        }
        for text in ["0", "0.2", "0.95", "0.951", "1", "-0.1"] {
            let n: f64 = text.parse().unwrap();
            assert_eq!(MIN_SAVINGS.check(n).is_ok(), crate::parse_min_savings(text).is_ok(), "min savings {text}");
        }
        let takes = |args: &[&str]| crate::Cli::try_parse_from([&["av1opt"], args].concat()).is_ok();
        for n in -1..=25 {
            let (text, number) = (n.to_string(), f64::from(n));
            assert_eq!(PRESET.check(number).is_ok(), takes(&["encode", "-o", "out", "--preset", &text, "in"]), "preset {n}");
            assert_eq!(WORKERS.check(number).is_ok(), takes(&["encode", "-o", "out", "--workers", &text, "in"]), "workers {n}");
            assert_eq!(TOLERANCE.check(number).is_ok(), takes(&["dups", "--tolerance", &text, "in"]), "tolerance {n}");
        }
    }
}
