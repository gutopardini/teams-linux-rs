//! Bounded, in-memory undo and redo for the active composer.
use crate::message_body::ComposerFormatting;
use std::collections::VecDeque;

const MAX_STATES: usize = 80;
const MAX_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub text: String,
    pub formatting: ComposerFormatting,
}

#[derive(Default)]
pub struct History {
    key: Option<String>,
    undo: VecDeque<Snapshot>,
    redo: VecDeque<Snapshot>,
}

impl History {
    pub fn reset(&mut self) { *self = Self::default(); }

    fn context(&mut self, key: &str) {
        if self.key.as_deref() != Some(key) {
            self.reset();
            self.key = Some(key.to_owned());
        }
    }

    fn push(stack: &mut VecDeque<Snapshot>, snapshot: Snapshot) {
        if snapshot.text.len() > MAX_BYTES { stack.clear(); return; }
        if stack.back() == Some(&snapshot) { return; }
        stack.push_back(snapshot);
        while stack.len() > MAX_STATES || stack.iter().map(|s| s.text.len()).sum::<usize>() > MAX_BYTES {
            stack.pop_front();
        }
    }

    pub fn record(&mut self, key: &str, before: Snapshot) {
        self.context(key);
        Self::push(&mut self.undo, before);
        self.redo.clear();
    }

    pub fn step(&mut self, key: &str, current: Snapshot, redo: bool) -> Option<Snapshot> {
        self.context(key);
        let (from, to) = if redo { (&mut self.redo, &mut self.undo) } else { (&mut self.undo, &mut self.redo) };
        let snapshot = from.pop_back()?;
        Self::push(to, current);
        Some(snapshot)
    }
}
