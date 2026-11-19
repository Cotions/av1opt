//! Whose turn it is among requests from the page that each start an ffmpeg.

use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex, Weak};

/// At most `most` runs at a time, and one per clip. Nothing here is held by a run but its own
/// turn and its own clip, so one that hangs delays only those waiting for the same clip.
pub struct Turns {
    most: usize,
    running: Mutex<usize>,
    freed: Condvar,
    clips: Mutex<BTreeMap<String, Weak<Mutex<()>>>>,
}

/// A turn, given back when dropped.
pub struct Turn<'a>(&'a Turns);

impl Turns {
    pub const fn new(most: usize) -> Self {
        Turns { most, running: Mutex::new(0), freed: Condvar::new(), clips: Mutex::new(BTreeMap::new()) }
    }

    /// The lock of one clip, the same one for everybody asking for `key` while it is in use.
    pub fn clip(&self, key: &str) -> Arc<Mutex<()>> {
        let mut clips = self.clips.lock().unwrap_or_else(|e| e.into_inner());
        clips.retain(|_, clip| clip.strong_count() > 0);
        if let Some(clip) = clips.get(key).and_then(Weak::upgrade) {
            return clip;
        }
        let clip = Arc::new(Mutex::new(()));
        clips.insert(key.to_string(), Arc::downgrade(&clip));
        clip
    }

    /// Waits for a turn.
    pub fn wait(&self) -> Turn<'_> {
        let mut running = self.running.lock().unwrap_or_else(|e| e.into_inner());
        while *running >= self.most {
            running = self.freed.wait(running).unwrap_or_else(|e| e.into_inner());
        }
        *running += 1;
        Turn(self)
    }
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        *self.0.running.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
        self.0.freed.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[test]
    fn no_more_than_the_allowed_number_run_and_everybody_gets_a_turn() {
        let turns = Turns::new(2);
        let (now, most, done) = (AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0));
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    let _turn = turns.wait();
                    most.fetch_max(now.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(10));
                    now.fetch_sub(1, Ordering::SeqCst);
                    done.fetch_add(1, Ordering::SeqCst);
                });
            }
        });
        assert_eq!(done.load(Ordering::SeqCst), 8);
        assert!(most.load(Ordering::SeqCst) <= 2);
    }

    #[test]
    fn a_clip_has_one_lock_while_it_is_in_use_and_is_then_forgotten() {
        let turns = Turns::new(1);
        let (a, again, b) = (turns.clip("a"), turns.clip("a"), turns.clip("b"));
        assert!(Arc::ptr_eq(&a, &again));
        assert!(!Arc::ptr_eq(&a, &b));
        drop((a, again, b));
        let _c = turns.clip("c");
        assert_eq!(turns.clips.lock().unwrap().len(), 1);
    }
}
