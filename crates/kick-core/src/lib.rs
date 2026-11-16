//! Transport-independent models and bounded presentation state.

pub mod provider;

pub use provider::{CatalogProvider, ChatProvider, MockProvider, append_recent_chat};

use std::collections::VecDeque;
use std::fmt;

pub const DEFAULT_CHAT_CAPACITY: usize = 500;
pub const MAX_CHAT_CAPACITY: usize = 1_000;
pub const DEFAULT_CHAT_PAYLOAD_BUDGET: usize = 2 * 1024 * 1024;
pub const MAX_CHAT_PAYLOAD_BUDGET: usize = 2 * 1024 * 1024;
pub const MAX_CHAT_ID_BYTES: usize = 128;
pub const MAX_CHAT_AUTHOR_BYTES: usize = 128;
pub const MAX_CHAT_TEXT_BYTES: usize = 4 * 1024;

#[derive(Clone, PartialEq, Eq)]
pub struct StreamSummary {
    pub channel_slug: String,
    pub channel_name: String,
    pub title: String,
    pub category: Option<String>,
    pub viewer_count: Option<u64>,
    /// May be a signed URL; do not format or log its value.
    pub thumbnail_url: Option<String>,
    /// Public profile image URL; may be absent for anonymous or offline records.
    pub profile_picture_url: Option<String>,
}

impl fmt::Debug for StreamSummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let thumbnail_url = self.thumbnail_url.as_ref().map(|_| "<redacted>");
        let profile_picture_url = self.profile_picture_url.as_ref().map(|_| "<redacted>");
        formatter
            .debug_struct("StreamSummary")
            .field("channel_slug", &self.channel_slug)
            .field("channel_name", &self.channel_name)
            .field("title", &self.title)
            .field("category", &self.category)
            .field("viewer_count", &self.viewer_count)
            .field("thumbnail_url", &thumbnail_url)
            .field("profile_picture_url", &profile_picture_url)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ChatMessage {
    pub id: String,
    pub author: String,
    pub text: String,
    pub timestamp_ms: Option<u64>,
    pub author_slug: Option<String>,
    pub badges: Vec<ChatBadge>,
    /// Set for a channel-point redemption rendered as a chat event.
    pub reward: Option<ChatReward>,
}

/// Display fields from a `RewardRedeemedEvent`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatReward {
    pub title: String,
    pub user_input: Option<String>,
    /// `#RRGGBB` from the reward configuration, when the event supplies it.
    pub background_color: Option<String>,
}

/// One chat badge such as a subscriber month count or a moderator mark.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatBadge {
    pub kind: String,
    pub text: String,
}

/// A small ring buffer with both an entry limit and a UTF-8 payload budget.
///
/// The payload budget counts the retained `id`, `author`, and `text` bytes.
/// Entry count and per-field limits also bound the collection and string
/// allocation overhead; this is a data bound, not a process-RAM measurement.
pub struct BoundedChat {
    messages: VecDeque<ChatMessage>,
    capacity: usize,
    payload_budget: usize,
    retained_payload_bytes: usize,
}

impl BoundedChat {
    pub fn default_live_chat() -> Self {
        Self::new(DEFAULT_CHAT_CAPACITY)
    }

    /// Creates a history buffer. Requested limits are clamped to hard ceilings.
    pub fn new(capacity: usize) -> Self {
        Self::with_limits(capacity, DEFAULT_CHAT_PAYLOAD_BUDGET)
    }

    pub fn with_limits(capacity: usize, payload_budget: usize) -> Self {
        let capacity = capacity.min(MAX_CHAT_CAPACITY);
        let payload_budget = payload_budget.min(MAX_CHAT_PAYLOAD_BUDGET);
        Self {
            messages: VecDeque::with_capacity(capacity),
            capacity,
            payload_budget,
            retained_payload_bytes: 0,
        }
    }

    pub fn push(&mut self, mut message: ChatMessage) {
        if self.capacity == 0 || self.payload_budget == 0 {
            return;
        }

        message.id = bounded_string(message.id, MAX_CHAT_ID_BYTES);
        message.author = bounded_string(message.author, MAX_CHAT_AUTHOR_BYTES);
        message.text = bounded_string(message.text, MAX_CHAT_TEXT_BYTES);
        if let Some(reward) = message.reward.as_mut() {
            reward.title = bounded_string(std::mem::take(&mut reward.title), MAX_CHAT_TEXT_BYTES);
            reward.user_input = reward
                .user_input
                .take()
                .map(|value| bounded_string(value, MAX_CHAT_TEXT_BYTES));
            reward.background_color = reward
                .background_color
                .take()
                .map(|value| bounded_string(value, 16));
        }
        let message_bytes = payload_bytes(&message);
        if message_bytes > self.payload_budget {
            return;
        }

        while self.messages.len() >= self.capacity
            || self.retained_payload_bytes + message_bytes > self.payload_budget
        {
            let Some(oldest) = self.messages.pop_front() else {
                return;
            };
            self.retained_payload_bytes -= payload_bytes(&oldest);
        }

        self.retained_payload_bytes += message_bytes;
        self.messages.push_back(message);
    }

    pub fn messages(&self) -> &VecDeque<ChatMessage> {
        &self.messages
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn payload_budget(&self) -> usize {
        self.payload_budget
    }

    pub fn retained_payload_bytes(&self) -> usize {
        self.retained_payload_bytes
    }
}

fn bounded_string(value: String, maximum_bytes: usize) -> String {
    if value.len() <= maximum_bytes && value.capacity() <= maximum_bytes {
        return value;
    }

    let mut end = value.len().min(maximum_bytes);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn payload_bytes(message: &ChatMessage) -> usize {
    message.id.len()
        + message.author.len()
        + message.text.len()
        + message
            .reward
            .as_ref()
            .map(|reward| {
                reward.title.len()
                    + reward.user_input.as_ref().map_or(0, String::len)
                    + reward.background_color.as_ref().map_or(0, String::len)
            })
            .unwrap_or(0)
        + message
            .badges
            .iter()
            .map(|badge| badge.kind.len() + badge.text.len())
            .sum::<usize>()
}

#[derive(Clone, PartialEq, Eq)]
pub enum PlaybackState {
    Idle,
    Connecting,
    Playing,
    Reconnecting,
    Failed(String),
}

/// A runtime playback source; the URL is sensitive and must never appear in logs.
#[derive(Clone)]
pub struct PlaybackSource {
    pub url: String,
    pub quality_label: Option<String>,
}

impl fmt::Debug for PlaybackSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PlaybackSource")
            .field("url", &"<redacted>")
            .field("quality_label", &self.quality_label)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BoundedChat, ChatMessage, DEFAULT_CHAT_PAYLOAD_BUDGET, MAX_CHAT_AUTHOR_BYTES,
        MAX_CHAT_CAPACITY, MAX_CHAT_ID_BYTES, MAX_CHAT_TEXT_BYTES, PlaybackSource, StreamSummary,
    };

    fn message(id: &str, author: &str, text: &str) -> ChatMessage {
        ChatMessage {
            id: id.to_owned(),
            author: author.to_owned(),
            text: text.to_owned(),
            timestamp_ms: None,
            author_slug: None,
            badges: Vec::new(),
            reward: None,
        }
    }

    #[test]
    fn chat_keeps_only_the_newest_messages() {
        let mut chat = BoundedChat::new(2);
        chat.push(message("one", "viewer", "one"));
        chat.push(message("two", "viewer", "two"));
        chat.push(message("three", "viewer", "three"));

        let ids: Vec<_> = chat
            .messages()
            .iter()
            .map(|item| item.id.as_str())
            .collect();
        assert_eq!(ids, ["two", "three"]);
    }

    #[test]
    fn zero_capacity_discards_every_message() {
        let mut chat = BoundedChat::new(0);
        chat.push(message("discarded", "viewer", "discarded"));

        assert!(chat.messages().is_empty());
        assert_eq!(chat.retained_payload_bytes(), 0);
    }

    #[test]
    fn requested_limits_are_clamped() {
        let chat = BoundedChat::with_limits(usize::MAX, usize::MAX);

        assert_eq!(chat.capacity(), MAX_CHAT_CAPACITY);
        assert_eq!(chat.payload_budget(), DEFAULT_CHAT_PAYLOAD_BUDGET);
    }

    #[test]
    fn payload_budget_evicts_oldest_messages() {
        let mut chat = BoundedChat::with_limits(10, 12);
        chat.push(message("1", "a", "123456"));
        chat.push(message("2", "b", "abcdef"));

        assert_eq!(chat.messages().len(), 1);
        assert_eq!(chat.messages()[0].id, "2");
        assert!(chat.retained_payload_bytes() <= chat.payload_budget());
    }

    #[test]
    fn message_fields_are_truncated_at_utf8_boundaries() {
        let mut chat = BoundedChat::new(1);
        chat.push(message(
            &"識".repeat(MAX_CHAT_ID_BYTES),
            &"名".repeat(MAX_CHAT_AUTHOR_BYTES),
            &"字".repeat(MAX_CHAT_TEXT_BYTES),
        ));
        let stored = &chat.messages()[0];

        assert!(stored.id.len() <= MAX_CHAT_ID_BYTES);
        assert!(stored.author.len() <= MAX_CHAT_AUTHOR_BYTES);
        assert!(stored.text.len() <= MAX_CHAT_TEXT_BYTES);
        assert_eq!(stored.id, "識".repeat(MAX_CHAT_ID_BYTES / 3));
        assert_eq!(stored.author, "名".repeat(MAX_CHAT_AUTHOR_BYTES / 3));
        assert_eq!(stored.text, "字".repeat(MAX_CHAT_TEXT_BYTES / 3));
        assert!(chat.retained_payload_bytes() <= chat.payload_budget());
    }

    #[test]
    fn debug_output_redacts_runtime_urls() {
        let secret_url = "https://cdn.example/live.m3u8?token=private-value";
        let playback = PlaybackSource {
            url: secret_url.to_owned(),
            quality_label: Some("720p".to_owned()),
        };
        let stream = StreamSummary {
            channel_slug: "sample".to_owned(),
            channel_name: "Sample".to_owned(),
            title: "Live".to_owned(),
            category: None,
            viewer_count: Some(12),
            thumbnail_url: Some(secret_url.to_owned()),
            profile_picture_url: Some(secret_url.to_owned()),
        };

        assert!(!format!("{playback:?}").contains(secret_url));
        assert!(!format!("{stream:?}").contains(secret_url));
        assert!(!format!("{playback:?}").contains("private-value"));
        assert!(!format!("{stream:?}").contains("private-value"));
    }
}
