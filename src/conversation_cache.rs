//! Bounded, session-local histories. Ownership moves on chat switches.
use crate::{GraphMessage, HistoryViewport};
use std::collections::VecDeque;

const MAX_CHATS: usize = 8;
const MAX_MESSAGES: usize = 4_000;
const MAX_ESTIMATED_BYTES: usize = 16 * 1024 * 1024;

pub struct History {
    pub messages: Vec<GraphMessage>,
    pub before: Option<String>,
    pub viewport: Option<HistoryViewport>,
    pub follow_tail: bool,
    pub receipts: Vec<ost::api::ReadReceipt>,
}

#[derive(Default)]
pub struct Cache {
    entries: VecDeque<(String, History, usize)>,
    bytes: usize,
    messages: usize,
}

impl Cache {
    pub fn histories(&self) -> impl Iterator<Item=(&str, &History)> {
        self.entries.iter().map(|(id,history,_)|(id.as_str(),history))
    }
    pub fn insert(&mut self, id: String, history: History) {
        self.take(&id);
        let bytes = history.messages.capacity() * std::mem::size_of::<GraphMessage>()
            + history.messages.iter().map(message_bytes).sum::<usize>();
        if history.messages.len() > MAX_MESSAGES || bytes > MAX_ESTIMATED_BYTES {
            return;
        }
        self.bytes += bytes;
        self.messages += history.messages.len();
        self.entries.push_back((id, history, bytes));
        while self.entries.len() > MAX_CHATS || self.bytes > MAX_ESTIMATED_BYTES || self.messages > MAX_MESSAGES {
            if let Some((_, history, bytes)) = self.entries.pop_front() {
                self.bytes -= bytes;
                self.messages -= history.messages.len();
            }
        }
    }

    pub fn take(&mut self, id: &str) -> Option<History> {
        let index = self.entries.iter().position(|entry| entry.0 == id)?;
        let (_, history, bytes) = self.entries.remove(index)?;
        self.bytes -= bytes;
        self.messages -= history.messages.len();
        Some(history)
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

// Estimated payload accounting, not RSS/GPU measurement. Shared rich documents
// are charged per message, conservatively avoiding unbounded text retention.
fn message_bytes(message: &GraphMessage) -> usize {
    message.body.capacity() + message.id.capacity()
        + message.author.capacity() + message.timestamp.capacity() + message.date.capacity() + message.time.capacity()
        + message.rich.as_ref().map_or(0, |doc| doc.estimated_heap_bytes())
        + message.forwarded.as_ref().map_or(0, |f| f.content.capacity() + f.rich.as_ref().map_or(0, |d| d.estimated_heap_bytes()))
        + message.images.iter().map(|i| i.id.capacity() + i.url.capacity()).sum::<usize>()
        + message.files.iter().map(|f| f.name.capacity() + f.object_url.capacity() + f.share_url.as_ref().map_or(0, String::capacity)).sum::<usize>()
        + message.quoted_body.as_ref().map_or(0, String::capacity)
        + message.quoted_context.as_ref().map_or(0,|context|context.message_id.capacity()+context.sender.capacity()+context.author.capacity()+context.preview.capacity())
        + message.quoted_contexts.iter().map(|context| context.message_id.capacity()+context.sender.capacity()+context.author.capacity()+context.preview.capacity()).sum::<usize>()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history(count: usize, bytes: usize) -> History {
        History {
            messages: (0..count).map(|_| GraphMessage { body: "x".repeat(bytes), ..Default::default() }).collect(),
            before: Some("cursor".into()), viewport: None, follow_tail: false, receipts: vec![],
        }
    }

    #[test]
    fn histories_move_without_cloning_and_keep_their_cursor() {
        let mut cache = Cache::default();
        let history = history(2, 64);
        let pointer = history.messages.as_ptr();
        cache.insert("a".into(), history);
        let history = cache.take("a").unwrap();
        assert_eq!(history.messages.as_ptr(), pointer);
        assert_eq!(history.before.as_deref(), Some("cursor"));
        assert!(!history.follow_tail);
        assert!(cache.take("a").is_none());
        assert_eq!(cache.bytes, 0);
        assert_eq!(cache.messages, 0);
    }

    #[test]
    fn chat_count_payload_and_message_budgets_evict_oldest_entries() {
        let mut cache = Cache::default();
        for index in 0..10 { cache.insert(index.to_string(), history(1, 32)); }
        assert!(cache.take("0").is_none());
        assert!(cache.take("1").is_none());
        assert!(cache.take("9").is_some());
        cache.clear();
        cache.insert("a".into(), history(3_000, 1));
        cache.insert("b".into(), history(2_000, 1));
        assert!(cache.take("a").is_none());
        assert_eq!(cache.messages, 2_000);
        cache.insert("oversized".into(), history(1, MAX_ESTIMATED_BYTES + 1));
        assert!(cache.take("oversized").is_none());
        cache.clear();
        assert!(cache.entries.is_empty());
        assert_eq!(cache.bytes, 0);
    }
}
