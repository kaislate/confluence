//! Non-modal notifications: info disappears after a few seconds, errors stay
//! until dismissed, and a repeated error is one note with a count.

use std::time::{Duration, Instant};

/// How long an info note stays.
pub const INFO_FOR: Duration = Duration::from_secs(4);
/// Notes shown at once.
pub const MAX_SHOWN: usize = 5;
/// The same error again within this long adds to the existing note.
pub const MERGE_WITHIN: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, PartialEq)]
pub struct Note {
    pub id: u64,
    pub text: String,
    pub error: bool,
    /// How many times this note was raised (merged repeats).
    pub count: u32,
    pub at: Instant,
}

#[derive(Default)]
pub struct Notes {
    items: Vec<Note>,
    next: u64,
}

impl Notes {
    pub fn info(&mut self, text: String, now: Instant) {
        self.add(text, false, now);
    }

    pub fn error(&mut self, text: String, now: Instant) {
        if let Some(last) = self.items.iter_mut().rev().find(|n| n.error && n.text == text) {
            if now.saturating_duration_since(last.at) < MERGE_WITHIN {
                last.count += 1;
                last.at = now;
                return;
            }
        }
        self.add(text, true, now);
    }

    fn add(&mut self, text: String, error: bool, now: Instant) {
        self.next += 1;
        self.items.push(Note { id: self.next, text, error, count: 1, at: now });
    }

    pub fn dismiss(&mut self, id: u64) {
        self.items.retain(|n| n.id != id);
    }

    pub fn prune(&mut self, now: Instant) {
        self.items.retain(|n| n.error || now.saturating_duration_since(n.at) < INFO_FOR);
    }

    /// Newest first, at most [`MAX_SHOWN`].
    pub fn shown(&self) -> Vec<&Note> {
        let mut v: Vec<&Note> = self.items.iter().collect();
        v.sort_by(|a, b| b.at.cmp(&a.at).then(b.id.cmp(&a.id)));
        v.truncate(MAX_SHOWN);
        v
    }

    /// True while an info note is waiting to expire (the UI keeps repainting).
    pub fn has_info(&self) -> bool {
        self.items.iter().any(|n| !n.error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_expires_and_errors_stay() {
        let mut n = Notes::default();
        let t0 = Instant::now();
        n.info("Added VASIO 1".into(), t0);
        n.error("cannot open".into(), t0);
        n.prune(t0 + INFO_FOR);
        let texts: Vec<_> = n.shown().iter().map(|x| x.text.clone()).collect();
        assert_eq!(texts, vec!["cannot open".to_string()]);
    }

    #[test]
    fn newest_first_and_at_most_five() {
        let mut n = Notes::default();
        let t0 = Instant::now();
        for i in 0..7 {
            n.error(format!("e{i}"), t0 + Duration::from_secs(i * 3));
        }
        let shown = n.shown();
        assert_eq!(shown.len(), MAX_SHOWN);
        assert_eq!(shown[0].text, "e6");
    }

    #[test]
    fn identical_errors_merge_into_one() {
        let mut n = Notes::default();
        let t0 = Instant::now();
        for i in 0..20 {
            n.error("lost the engine (pipe broken)".into(), t0 + Duration::from_millis(i * 30));
        }
        let shown = n.shown();
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].count, 20);
        n.error("lost the engine (pipe broken)".into(), t0 + Duration::from_secs(60));
        assert_eq!(n.shown().len(), 2, "much later it is a new note");
    }

    #[test]
    fn dismissing_removes_one_note() {
        let mut n = Notes::default();
        let t0 = Instant::now();
        n.error("a".into(), t0);
        n.error("b".into(), t0);
        let id = n.shown()[0].id;
        n.dismiss(id);
        assert_eq!(n.shown().len(), 1);
        assert!(!n.has_info());
    }
}
