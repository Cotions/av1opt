//! When the encode queue may work: the user's pause switch, and the hours of the day encodes
//! are allowed in. While it is held, queued jobs wait and the running one stops where it is
//! (its ffmpeg processes are stopped, not ended) until the hold is lifted.

use super::{App, now};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::Mutex;

/// Hours encodes may run in, as minutes after local midnight: from `from` up to `to`, through
/// midnight when `to` is the earlier of the two.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
pub struct Schedule {
    pub on: bool,
    pub from: u16,
    pub to: u16,
}

impl Default for Schedule {
    /// Off; the hours offered first are the night.
    fn default() -> Self {
        Schedule { on: false, from: 23 * 60, to: 7 * 60 }
    }
}

const DAY: u16 = 24 * 60;

impl Schedule {
    /// True when encodes may run at `minute` of the day.
    pub fn allows(&self, minute: u16) -> bool {
        if !self.on || self.from == self.to {
            return true;
        }
        if self.from < self.to { (self.from..self.to).contains(&minute) } else { minute >= self.from || minute < self.to }
    }

    /// Seconds from `minute`:`second` of the day until the hours next begin.
    pub fn opens_in(&self, minute: u16, second: u16) -> u64 {
        let minutes = (self.from + DAY - minute % DAY) % DAY;
        (minutes as u64 * 60).saturating_sub(second as u64)
    }
}

/// `"23:00"` as minutes after midnight.
pub fn parse_clock(s: &str) -> Result<u16> {
    let (h, m) = s.trim().split_once(':').with_context(|| format!("not a time of day: {s}"))?;
    let (h, m): (u16, u16) = (h.parse().with_context(|| format!("not a time of day: {s}"))?, m.parse().with_context(|| format!("not a time of day: {s}"))?);
    if h > 23 || m > 59 {
        bail!("not a time of day: {s}");
    }
    Ok(h * 60 + m)
}

pub fn clock(minute: u16) -> String {
    format!("{:02}:{:02}", minute / 60 % 24, minute % 60)
}

/// Minute and second of the local day.
fn local_time() -> (u16, u16) {
    // SAFETY: time() with a null pointer only returns the time; localtime_r fills the tm we
    // own and is the thread-safe variant.
    let tm = unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        tm
    };
    ((tm.tm_hour * 60 + tm.tm_min) as u16, tm.tm_sec as u16)
}

/// Why the queue is not working.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Hold {
    /// The user paused it.
    Paused,
    /// Outside the hours encodes may run in; they begin again at this time (Unix seconds).
    Hours { opens: u64 },
}

#[derive(Default)]
pub struct State {
    /// "Run now" outside the hours: the schedule is ignored until this time (Unix seconds),
    /// which is when the hours next begin, so it holds again when they end.
    ignore_hours_until: Mutex<Option<u64>>,
    /// The hold last put into effect, to tell changes from repeats.
    last: Mutex<Option<Hold>>,
}

fn hold_at(paused: bool, schedule: Schedule, ignore_hours_until: Option<u64>, now: u64, (minute, second): (u16, u16)) -> Option<Hold> {
    if paused {
        return Some(Hold::Paused);
    }
    if schedule.allows(minute) || ignore_hours_until.is_some_and(|t| now < t) {
        return None;
    }
    Some(Hold::Hours { opens: now + schedule.opens_in(minute, second) })
}

/// The hold in force right now, if any.
pub fn current(app: &App) -> Option<Hold> {
    let (paused, schedule) = {
        let s = app.settings.lock().unwrap();
        (s.queue_paused, s.schedule)
    };
    hold_at(paused, schedule, *app.hold.ignore_hours_until.lock().unwrap(), now(), local_time())
}

/// What the page shows and edits.
pub fn json(app: &App) -> Value {
    let (paused, schedule) = {
        let s = app.settings.lock().unwrap();
        (s.queue_paused, s.schedule)
    };
    let hold = match current(app) {
        None => Value::Null,
        Some(Hold::Paused) => json!({ "why": "paused" }),
        Some(Hold::Hours { opens }) => json!({ "why": "hours", "opens": opens }),
    };
    json!({
        "hold": hold,
        "paused": paused,
        "schedule": { "on": schedule.on, "from": clock(schedule.from), "to": clock(schedule.to) },
    })
}

/// Puts the hold in force into effect: the running job is stopped or let go, the worker is
/// woken to start (or not start) the next one, and open pages are told when something changed.
pub fn sync(app: &App) {
    let hold = current(app);
    super::jobs::set_held(app, hold.is_some());
    let mut last = app.hold.last.lock().unwrap();
    // The time the hours begin moves by a second now and then; only the kind of hold counts.
    let kind = |h: &Option<Hold>| h.map(|h| matches!(h, Hold::Paused));
    if kind(&last) != kind(&hold) {
        *last = hold;
        drop(last);
        app.broadcast(json!({ "kind": "jobs" }));
    }
}

/// Checks the clock for as long as the server runs.
pub fn start_clock(app: Arc<App>) {
    sync(&app);
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(10));
            sync(&app);
        }
    });
}

/// `{ "paused": bool }`: the user's pause switch. Kept across restarts.
pub fn set_paused(app: &App, body: &Value) -> Result<Value> {
    let paused = body["paused"].as_bool().context("missing paused")?;
    app.settings.lock().unwrap().queue_paused = paused;
    app.save_settings();
    sync(app);
    Ok(json(app))
}

/// `{ "on": bool, "from": "23:00", "to": "07:00" }`: the hours encodes may run in.
pub fn set_schedule(app: &App, body: &Value) -> Result<Value> {
    let on = body["on"].as_bool().context("missing on")?;
    let from = parse_clock(body["from"].as_str().context("missing from")?)?;
    let to = parse_clock(body["to"].as_str().context("missing to")?)?;
    if on && from == to {
        bail!("the hours begin and end at the same time");
    }
    app.settings.lock().unwrap().schedule = Schedule { on, from, to };
    // New hours start with a clean slate.
    *app.hold.ignore_hours_until.lock().unwrap() = None;
    app.save_settings();
    sync(app);
    Ok(json(app))
}

/// Encodes run now although the hours have not begun; the schedule counts again once they have.
pub fn run_now(app: &App) -> Result<Value> {
    if let Some(Hold::Hours { opens }) = current(app) {
        *app.hold.ignore_hours_until.lock().unwrap() = Some(opens);
    }
    sync(app);
    Ok(json(app))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NIGHT: Schedule = Schedule { on: true, from: 23 * 60, to: 7 * 60 };
    const DAYTIME: Schedule = Schedule { on: true, from: 9 * 60, to: 17 * 60 + 30 };

    #[test]
    fn hours_may_run_through_midnight() {
        for (minute, night, day) in [(0, true, false), (6 * 60 + 59, true, false), (7 * 60, false, false), (9 * 60, false, true), (17 * 60 + 29, false, true), (17 * 60 + 30, false, false), (22 * 60 + 59, false, false), (23 * 60, true, false), (DAY - 1, true, false)] {
            assert_eq!(NIGHT.allows(minute), night, "night at {}", clock(minute));
            assert_eq!(DAYTIME.allows(minute), day, "day at {}", clock(minute));
        }
        // Switched off, or with no hours left out, any time will do.
        assert!(Schedule { on: false, ..NIGHT }.allows(12 * 60));
        assert!(Schedule { on: true, from: 600, to: 600 }.allows(0));
    }

    #[test]
    fn the_wait_ends_when_the_hours_begin() {
        assert_eq!(NIGHT.opens_in(22 * 60, 0), 3600);
        assert_eq!(NIGHT.opens_in(22 * 60 + 59, 30), 30);
        assert_eq!(NIGHT.opens_in(7 * 60, 0), 16 * 3600);
        assert_eq!(DAYTIME.opens_in(17 * 60 + 30, 0), (15 * 60 + 30) * 60);
        assert_eq!(DAYTIME.opens_in(8 * 60 + 59, 59), 1);
    }

    #[test]
    fn pause_wins_and_run_now_lasts_until_the_hours_begin() {
        let noon = (12 * 60, 0);
        let opens = 1_000_000 + 11 * 3600;
        assert_eq!(hold_at(false, NIGHT, None, 1_000_000, noon), Some(Hold::Hours { opens }));
        assert_eq!(hold_at(true, NIGHT, None, 1_000_000, noon), Some(Hold::Paused));
        assert_eq!(hold_at(true, Schedule::default(), None, 1_000_000, noon), Some(Hold::Paused));
        assert_eq!(hold_at(false, Schedule::default(), None, 1_000_000, noon), None);
        // In the hours: nothing holds.
        assert_eq!(hold_at(false, NIGHT, None, 1_000_000, (2 * 60, 0)), None);
        // "Run now" at noon: free until 23:00. The next morning the hours end as usual.
        assert_eq!(hold_at(false, NIGHT, Some(opens), 1_000_000, noon), None);
        assert_eq!(hold_at(false, NIGHT, Some(opens), opens - 1, (22 * 60 + 59, 59)), None);
        assert_eq!(hold_at(false, NIGHT, Some(opens), opens + 9 * 3600, (8 * 60, 0)), Some(Hold::Hours { opens: opens + 24 * 3600 }));
        // A pause still stops a queue that was told to run now.
        assert_eq!(hold_at(true, NIGHT, Some(opens), 1_000_000, noon), Some(Hold::Paused));
    }

    #[test]
    fn times_of_day_are_read_and_written() {
        assert_eq!(parse_clock("23:00").unwrap(), 23 * 60);
        assert_eq!(parse_clock(" 7:05 ").unwrap(), 7 * 60 + 5);
        assert_eq!(clock(7 * 60 + 5), "07:05");
        for bad in ["24:00", "12:60", "noon", "12", "-1:00", ""] {
            assert!(parse_clock(bad).is_err(), "{bad}");
        }
    }
}
