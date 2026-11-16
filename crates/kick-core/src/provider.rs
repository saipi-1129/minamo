//! Small, transport-independent read interfaces for stream catalog and chat data.
//!
//! These contracts describe application data only. They make no assumptions
//! about Kick endpoints, authentication, or chat transport. [`MockProvider`]
//! serves synthetic fixtures and never accesses the network.

use std::convert::Infallible;

use crate::{BoundedChat, ChatMessage, MAX_CHAT_CAPACITY, StreamSummary};

/// Number of deterministic synthetic chat messages available per mock stream.
pub const MOCK_CHAT_MESSAGES_PER_CHANNEL: usize = 500;

const MOCK_CHAT_START_TIMESTAMP_MS: u64 = 1_735_689_600_000; // Fixed fixture epoch, not wall-clock time.
const MOCK_CHAT_INTERVAL_MS: u64 = 1_000;
const MOCK_CHAT_AUTHORS: [&str; 8] = [
    "DemoAster",
    "DemoByte",
    "DemoComet",
    "DemoDelta",
    "DemoEcho",
    "DemoFern",
    "DemoGlitch",
    "DemoJun",
];
const MOCK_CHAT_TEXTS: [&str; 12] = [
    "[MOCK] GG!",
    "[MOCK] Nice run.",
    "[MOCK] Great question.",
    "[MOCK] Fixture chat.",
    "[MOCK] Hello!",
    "[MOCK] Let's go!",
    "[MOCK] Cool scene.",
    "[MOCK] Next level!",
    "[MOCK] Demo viewer here.",
    "[MOCK] Thanks for the stream.",
    "[MOCK] Great track.",
    "[MOCK] See you next time.",
];

/// Read-only access to a stream catalog.
pub trait CatalogProvider {
    /// Provider-specific failure type. The mock implementation is infallible.
    type Error;

    /// Returns at most `limit` featured streams in provider-defined stable order.
    fn featured_streams(&self, limit: usize) -> Result<Vec<StreamSummary>, Self::Error>;

    /// Searches stream metadata and returns at most `limit` results.
    ///
    /// The provider decides which metadata fields are searchable. An empty
    /// query may be treated as a request for the full catalog.
    fn search_streams(&self, query: &str, limit: usize) -> Result<Vec<StreamSummary>, Self::Error>;
}

/// Read-only access to a bounded snapshot of a channel's chat history.
pub trait ChatProvider {
    /// Provider-specific failure type. The mock implementation is infallible.
    type Error;

    /// Returns up to `limit` messages, oldest first, for the supplied channel.
    ///
    /// Implementations should cap `limit` to a reasonable value. An unknown
    /// channel may return an empty result.
    fn recent_messages(
        &self,
        channel_slug: &str,
        limit: usize,
    ) -> Result<Vec<ChatMessage>, Self::Error>;
}

/// Appends a recent provider snapshot to the caller's bounded chat history.
///
/// The history's entry limit is passed to the provider, and [`BoundedChat`]
/// enforces its entry, per-field, and total payload limits when retaining the
/// returned messages. Existing entries remain subject to the same eviction
/// rules as new messages.
pub fn append_recent_chat<P: ChatProvider>(
    provider: &P,
    channel_slug: &str,
    history: &mut BoundedChat,
) -> Result<(), P::Error> {
    if history.capacity() == 0 {
        return Ok(());
    }

    for message in provider.recent_messages(channel_slug, history.capacity())? {
        history.push(message);
    }
    Ok(())
}

/// A deterministic catalog and chat fixture for UI development.
///
/// Every item is synthetic and visibly labeled as mock data. Each stream has
/// [`MOCK_CHAT_MESSAGES_PER_CHANNEL`] reproducible chat messages. This provider
/// never contacts Kick or represents a verified Kick protocol behavior.
#[derive(Clone)]
pub struct MockProvider {
    streams: Vec<StreamSummary>,
}

impl MockProvider {
    /// Creates the same three synthetic streams each time.
    pub fn new() -> Self {
        let streams = vec![
            StreamSummary {
                channel_slug: "mock-pixel-pilot".to_owned(),
                channel_name: "Mock • PixelPilot".to_owned(),
                title: "[MOCK] Weekend speedruns".to_owned(),
                category: Some("Games".to_owned()),
                viewer_count: Some(872),
                thumbnail_url: None,
                profile_picture_url: None,
            },
            StreamSummary {
                channel_slug: "mock-night-signals".to_owned(),
                channel_name: "Mock • Night Signals".to_owned(),
                title: "[MOCK] Ambient coding session".to_owned(),
                category: Some("Music".to_owned()),
                viewer_count: Some(314),
                thumbnail_url: None,
                profile_picture_url: None,
            },
            StreamSummary {
                channel_slug: "mock-orbit-lab".to_owned(),
                channel_name: "Mock • Orbit Lab".to_owned(),
                title: "[MOCK] Deep space science Q&A".to_owned(),
                category: Some("Science & Technology".to_owned()),
                viewer_count: Some(1_284),
                thumbnail_url: None,
                profile_picture_url: None,
            },
        ];

        Self { streams }
    }
}

impl Default for MockProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl CatalogProvider for MockProvider {
    type Error = Infallible;

    fn featured_streams(&self, limit: usize) -> Result<Vec<StreamSummary>, Self::Error> {
        Ok(self.streams.iter().take(limit).cloned().collect())
    }

    fn search_streams(&self, query: &str, limit: usize) -> Result<Vec<StreamSummary>, Self::Error> {
        let query = query.trim().to_lowercase();
        Ok(self
            .streams
            .iter()
            .filter(|stream| {
                query.is_empty()
                    || stream.channel_slug.to_lowercase().contains(&query)
                    || stream.channel_name.to_lowercase().contains(&query)
                    || stream.title.to_lowercase().contains(&query)
                    || stream
                        .category
                        .as_deref()
                        .is_some_and(|category| category.to_lowercase().contains(&query))
            })
            .take(limit)
            .cloned()
            .collect())
    }
}

impl ChatProvider for MockProvider {
    type Error = Infallible;

    fn recent_messages(
        &self,
        channel_slug: &str,
        limit: usize,
    ) -> Result<Vec<ChatMessage>, Self::Error> {
        let channel_offset = match channel_slug {
            "mock-pixel-pilot" => 0,
            "mock-night-signals" => 1,
            "mock-orbit-lab" => 2,
            _ => return Ok(Vec::new()),
        };

        let count = limit
            .min(MAX_CHAT_CAPACITY)
            .min(MOCK_CHAT_MESSAGES_PER_CHANNEL);
        if count == 0 {
            return Ok(Vec::new());
        }

        let first_sequence = MOCK_CHAT_MESSAGES_PER_CHANNEL - count + 1;
        let messages = (first_sequence..=MOCK_CHAT_MESSAGES_PER_CHANNEL)
            .map(|sequence| mock_message(channel_slug, channel_offset, sequence))
            .collect();
        Ok(messages)
    }
}

fn mock_message(channel_slug: &str, channel_offset: usize, sequence: usize) -> ChatMessage {
    let author_index = (sequence - 1 + channel_offset) % MOCK_CHAT_AUTHORS.len();
    let text_index = (sequence + channel_offset * 3) % MOCK_CHAT_TEXTS.len();

    ChatMessage {
        id: format!("{channel_slug}-chat-{sequence:04}"),
        author: MOCK_CHAT_AUTHORS[author_index].to_owned(),
        text: MOCK_CHAT_TEXTS[text_index].to_owned(),
        timestamp_ms: Some(
            MOCK_CHAT_START_TIMESTAMP_MS + (sequence as u64 - 1) * MOCK_CHAT_INTERVAL_MS,
        ),
        author_slug: None,
        badges: Vec::new(),
        reward: None,
    }
}

#[cfg(test)]
mod tests {
    use super::MOCK_CHAT_MESSAGES_PER_CHANNEL;
    use super::{CatalogProvider, ChatProvider, MockProvider, append_recent_chat};
    use crate::{BoundedChat, MAX_CHAT_CAPACITY};
    use std::convert::Infallible;

    #[test]
    fn mock_catalog_is_deterministic_and_labeled() {
        let first = MockProvider::new();
        let second = MockProvider::default();

        let first_streams = first.featured_streams(usize::MAX).unwrap();
        assert_eq!(first_streams, second.featured_streams(usize::MAX).unwrap());
        assert_eq!(first_streams.len(), 3);
        assert!(
            first_streams
                .iter()
                .all(|stream| stream.title.starts_with("[MOCK]"))
        );
        assert!(
            first_streams
                .iter()
                .all(|stream| stream.thumbnail_url.is_none())
        );
    }

    #[test]
    fn catalog_search_matches_metadata_case_insensitively_and_respects_limit() {
        let provider = MockProvider::new();

        let games = provider.search_streams("  gAmEs  ", 1).unwrap();
        assert_eq!(games.len(), 1);
        assert_eq!(games[0].channel_slug, "mock-pixel-pilot");
        assert_eq!(provider.search_streams("", 2).unwrap().len(), 2);
        assert!(provider.search_streams("absent", 3).unwrap().is_empty());
        assert!(provider.featured_streams(0).unwrap().is_empty());
    }

    #[test]
    fn mock_chat_respects_requested_message_limit() {
        let provider = MockProvider::new();

        let messages = provider.recent_messages("mock-pixel-pilot", 17).unwrap();

        assert_eq!(messages.len(), 17);
        assert_eq!(messages.first().unwrap().id, "mock-pixel-pilot-chat-0484");
        assert_eq!(messages.last().unwrap().id, "mock-pixel-pilot-chat-0500");
        assert!(
            provider
                .recent_messages("mock-pixel-pilot", 0)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            provider
                .recent_messages("mock-pixel-pilot", usize::MAX)
                .unwrap()
                .len(),
            MOCK_CHAT_MESSAGES_PER_CHANNEL.min(MAX_CHAT_CAPACITY)
        );
    }

    #[test]
    fn mock_chat_fixture_is_deterministic_and_oldest_first() {
        let provider = MockProvider::new();
        let first = provider
            .recent_messages("mock-pixel-pilot", MOCK_CHAT_MESSAGES_PER_CHANNEL)
            .unwrap();
        let second = provider
            .recent_messages("mock-pixel-pilot", MOCK_CHAT_MESSAGES_PER_CHANNEL)
            .unwrap();

        assert_eq!(first.len(), MOCK_CHAT_MESSAGES_PER_CHANNEL);
        assert!(first.iter().zip(&second).all(|(first, second)| {
            first.id == second.id
                && first.author == second.author
                && first.text == second.text
                && first.timestamp_ms == second.timestamp_ms
        }));
        assert_eq!(first.first().unwrap().id, "mock-pixel-pilot-chat-0001");
        assert_eq!(first.last().unwrap().id, "mock-pixel-pilot-chat-0500");
        assert_eq!(first.first().unwrap().timestamp_ms, Some(1_735_689_600_000));
        assert!(
            first
                .windows(2)
                .all(|pair| pair[0].timestamp_ms < pair[1].timestamp_ms)
        );
        assert!(
            first
                .iter()
                .all(|message| message.text.starts_with("[MOCK]"))
        );
    }

    #[test]
    fn mock_chat_requires_an_exact_channel_slug() {
        let provider = MockProvider::new();

        for (slug, expected_id) in [
            ("mock-pixel-pilot", "mock-pixel-pilot-chat-0500"),
            ("mock-night-signals", "mock-night-signals-chat-0500"),
            ("mock-orbit-lab", "mock-orbit-lab-chat-0500"),
        ] {
            let messages = provider.recent_messages(slug, 1).unwrap();
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].id, expected_id);
        }

        for unknown_slug in [
            "mock-pixel",
            "MOCK-PIXEL-PILOT",
            "mock-pixel-pilot/",
            "not-a-mock-channel",
        ] {
            assert!(
                provider
                    .recent_messages(unknown_slug, MOCK_CHAT_MESSAGES_PER_CHANNEL)
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[test]
    fn mock_chat_uses_distinct_stable_content_for_each_stream() {
        let provider = MockProvider::new();
        let pixel = provider.recent_messages("mock-pixel-pilot", 2).unwrap();
        let night = provider.recent_messages("mock-night-signals", 2).unwrap();
        let orbit = provider.recent_messages("mock-orbit-lab", 2).unwrap();

        assert_ne!(pixel[0].author, night[0].author);
        assert_ne!(night[0].author, orbit[0].author);
        assert_ne!(pixel[0].text, night[0].text);
        assert_ne!(night[0].text, orbit[0].text);
    }

    #[test]
    fn bounded_chat_retains_only_its_capacity_from_the_mock_fixture() {
        let provider = MockProvider::new();
        let mut history = BoundedChat::new(2);

        append_recent_chat(&provider, "mock-pixel-pilot", &mut history).unwrap();

        assert_eq!(history.messages().len(), 2);
        assert_eq!(history.messages()[0].id, "mock-pixel-pilot-chat-0499");
        assert_eq!(history.messages()[1].id, "mock-pixel-pilot-chat-0500");
        assert!(history.retained_payload_bytes() <= history.payload_budget());
    }

    #[test]
    fn zero_capacity_history_does_not_request_messages() {
        let provider = MockProvider::new();
        let mut history = BoundedChat::new(0);

        append_recent_chat(&provider, "mock-pixel-pilot", &mut history).unwrap();

        assert!(history.messages().is_empty());
        assert_eq!(history.retained_payload_bytes(), 0);
    }

    #[test]
    fn mock_provider_interfaces_are_infallible() {
        fn assert_infallible<
            P: CatalogProvider<Error = Infallible> + ChatProvider<Error = Infallible>,
        >(
            _: &P,
        ) {
        }

        let provider = MockProvider::new();
        assert_infallible(&provider);
    }
}
