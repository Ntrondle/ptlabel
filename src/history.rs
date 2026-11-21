//! Undo/redo for label edits (whole-label snapshots) and the File › Open Recent list.

use std::time::{Duration, Instant};

/// Undo steps kept.
pub const MAX_STEPS: usize = 100;
/// A change becomes one undo step once the value has been still this long.
pub const IDLE: Duration = Duration::from_millis(500);
/// Entries in File › Open Recent.
pub const MAX_RECENT: usize = 10;

/// Snapshot undo stack. The caller passes the current value once per frame (`observe`), which
/// coalesces a burst of changes (typing, a slider drag) into one step.
pub struct History<T> {
    undo: Vec<T>,
    redo: Vec<T>,
    /// Last committed state.
    head: T,
    /// Value at the previous `observe`, and when it last changed.
    seen: T,
    changed: Option<Instant>,
}

impl<T: Clone + PartialEq> History<T> {
    pub fn new(cur: T) -> Self {
        Self { undo: vec![], redo: vec![], seen: cur.clone(), head: cur, changed: None }
    }

    /// Once per frame, before the UI edits `cur`. `busy`: an edit is still going on (pointer held);
    /// `boundary`: a new interaction starts or focus moved, so a pending change ends here.
    pub fn observe(&mut self, cur: &T, now: Instant, busy: bool, boundary: bool) {
        if *cur != self.seen {
            self.seen = cur.clone();
            self.changed = Some(now);
        }
        let idle = self.changed.is_none_or(|t| now.duration_since(t) >= IDLE);
        if boundary || (idle && !busy) {
            self.commit(cur);
        }
    }

    /// Make `cur` an undo step if it differs from the last one (drops the redo steps).
    pub fn commit(&mut self, cur: &T) {
        if *cur == self.head {
            return;
        }
        self.undo.push(std::mem::replace(&mut self.head, cur.clone()));
        if self.undo.len() > MAX_STEPS {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    /// The state before `cur` (a pending change is committed first, so it is what gets undone).
    pub fn undo(&mut self, cur: &T) -> Option<T> {
        self.commit(cur);
        let prev = self.undo.pop()?;
        self.redo.push(std::mem::replace(&mut self.head, prev.clone()));
        self.seen = prev.clone();
        Some(prev)
    }

    /// The state undone last; `None` after a new change.
    pub fn redo(&mut self, cur: &T) -> Option<T> {
        self.commit(cur);
        let next = self.redo.pop()?;
        self.undo.push(std::mem::replace(&mut self.head, next.clone()));
        self.seen = next.clone();
        Some(next)
    }
}

/// Put `path` first in the recent-files list: no duplicates, at most `MAX_RECENT`.
pub fn push_recent(list: &mut Vec<String>, path: &str) {
    list.retain(|p| p != path);
    list.insert(0, path.to_string());
    list.truncate(MAX_RECENT);
}

/// Drop entries whose file no longer exists.
pub fn prune_recent(list: &mut Vec<String>) {
    list.retain(|p| std::path::Path::new(p).is_file());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(t: Instant, n: u64) -> Instant {
        t + Duration::from_millis(n)
    }

    #[test]
    fn typing_burst_is_one_step() {
        let t = Instant::now();
        let mut h = History::new(String::new());
        for (i, s) in ["H", "He", "Hel", "Hell", "Hello"].iter().enumerate() {
            h.observe(&s.to_string(), ms(t, i as u64 * 100), false, false);
        }
        h.observe(&"Hello".into(), ms(t, 1000), false, false);
        assert_eq!(h.undo(&"Hello".into()), Some(String::new()));
        assert_eq!(h.undo(&String::new()), None);
        assert_eq!(h.redo(&String::new()), Some("Hello".into()));
    }

    #[test]
    fn drag_waits_for_release() {
        let t = Instant::now();
        let mut h = History::new(0);
        h.observe(&0, ms(t, 0), true, true); // press
        h.observe(&5, ms(t, 100), true, false);
        h.observe(&9, ms(t, 300), true, false);
        h.observe(&9, ms(t, 2000), true, false); // held still: not done yet
        assert!(h.undo.is_empty());
        h.observe(&9, ms(t, 2100), false, false); // released
        assert_eq!(h.undo, vec![0]);
        assert_eq!(h.head, 9);
    }

    #[test]
    fn boundary_splits_quick_edits() {
        let t = Instant::now();
        let mut h = History::new(0);
        h.observe(&1, ms(t, 0), false, false);
        h.observe(&1, ms(t, 100), true, true); // next click starts: 1 is its own step
        h.observe(&2, ms(t, 200), false, false);
        assert_eq!(h.undo(&2), Some(1));
        assert_eq!(h.undo(&1), Some(0));
    }

    #[test]
    fn undo_commits_pending_and_new_edit_drops_redo() {
        let t = Instant::now();
        let mut h = History::new(0);
        h.observe(&1, t, false, false);
        assert_eq!(h.undo(&1), Some(0)); // pending 1 undone right away
        assert_eq!(h.redo(&3), None); // 3 is a new edit: redo is gone
        assert_eq!(h.undo(&3), Some(0));
    }

    #[test]
    fn capped() {
        let mut h = History::new(0);
        for i in 1..=MAX_STEPS + 20 {
            h.commit(&i);
        }
        let mut n = 0;
        let mut cur = MAX_STEPS + 20;
        while let Some(p) = h.undo(&cur) {
            cur = p;
            n += 1;
        }
        assert_eq!((n, cur), (MAX_STEPS, 20));
    }

    #[test]
    fn recent_list() {
        let mut l = vec![];
        for i in 0..12 {
            push_recent(&mut l, &format!("/f{i}"));
        }
        push_recent(&mut l, "/f5");
        assert_eq!(l.len(), MAX_RECENT);
        assert_eq!(&l[..3], ["/f5", "/f11", "/f10"]);
        assert_eq!(l.iter().filter(|p| *p == "/f5").count(), 1);
        let mut l = vec![concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml").to_string(), "/no/such.ptlabel".into()];
        prune_recent(&mut l);
        assert_eq!(l.len(), 1);
    }
}
