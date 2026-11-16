//! Parsing for the text placeholders used by the chat presentation layer.
//!
//! The parser accepts Kick's `[emote:<decimal-id>:<name>]` markers and the
//! `:name:` form observed in chat text. Input is bounded by the same UTF-8 byte
//! limit as `ChatMessage.text`; overlong input is clipped at a character
//! boundary before parsing.

use kick_core::MAX_CHAT_TEXT_BYTES;
use serde::Deserialize;
use std::fmt;

const EMOTE_PREFIX: &str = "[emote:";
const MAX_EMOTE_NAME_BYTES: usize = 96;

/// One emote record from the observed `/emotes/{slug}` catalogue response.
///
/// The public response exposes identity and access metadata, but no image URL.
#[derive(Clone, Deserialize, PartialEq, Eq)]
pub struct ChannelEmote {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub channel_id: Option<u64>,
    pub subscribers_only: bool,
}

impl fmt::Debug for ChannelEmote {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ChannelEmote(<redacted>)")
    }
}

/// A group in Kick's observed channel emote catalogue response.
#[derive(Clone, Deserialize, PartialEq, Eq)]
pub struct ChannelEmoteGroup {
    pub emotes: Vec<ChannelEmote>,
}

impl fmt::Debug for ChannelEmoteGroup {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ChannelEmoteGroup")
            .field("emote_count", &self.emotes.len())
            .finish()
    }
}

/// Decodes the observed root-array catalogue shape without retaining unrelated
/// group metadata such as playback URLs or profile fields.
pub fn decode_channel_emote_catalog(
    json: &[u8],
) -> Result<Vec<ChannelEmoteGroup>, serde_json::Error> {
    serde_json::from_slice(json)
}

/// An ordered portion of a chat message after parsing emote placeholders.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChatSegment {
    /// Ordinary text, including malformed emote-like placeholders.
    Text(String),
    /// A parsed emote token. Name-only markers need a channel catalogue lookup
    /// before an image ID can be resolved.
    Emote { id: Option<String>, name: String },
}

/// Splits chat text into ordinary text and recognized emote placeholders.
///
/// ID markers require a non-empty decimal ID. Name-only markers are syntactic
/// candidates and must still match the selected channel's emote catalogue
/// before being rendered as images. Invalid or incomplete placeholders remain
/// verbatim text. Only the first `MAX_CHAT_TEXT_BYTES` bytes are considered.
pub fn parse_chat_segments(text: &str) -> Vec<ChatSegment> {
    let text = bounded_prefix(text, MAX_CHAT_TEXT_BYTES);
    let mut segments = Vec::new();
    let mut text_start = 0;
    let mut search_from = 0;

    while search_from < text.len() {
        let bracket_start = text[search_from..]
            .find(EMOTE_PREFIX)
            .map(|offset| search_from + offset);
        let shortcode_start = text[search_from..]
            .find(':')
            .map(|offset| search_from + offset);

        match (bracket_start, shortcode_start) {
            (None, None) => break,
            (Some(marker_start), Some(colon_start)) if marker_start <= colon_start => {
                let marker_content_start = marker_start + EMOTE_PREFIX.len();
                let Some(relative_close) = text[marker_content_start..].find(']') else {
                    // Without a closing bracket there is no safe boundary for
                    // nested shortcode recognition. Preserve the rest as text.
                    search_from = text.len();
                    continue;
                };
                let marker_close = marker_content_start + relative_close;
                let marker_end = marker_close + 1;

                if let Some((id, name)) =
                    parse_marker_body(&text[marker_content_start..marker_close])
                {
                    if text_start < marker_start {
                        segments.push(ChatSegment::Text(text[text_start..marker_start].to_owned()));
                    }
                    segments.push(ChatSegment::Emote {
                        id: Some(id.to_owned()),
                        name: name.to_owned(),
                    });
                    text_start = marker_end;
                    search_from = marker_end;
                } else {
                    search_from = marker_end;
                }
            }
            (Some(marker_start), None) => {
                let marker_content_start = marker_start + EMOTE_PREFIX.len();
                let Some(relative_close) = text[marker_content_start..].find(']') else {
                    search_from = text.len();
                    continue;
                };
                let marker_close = marker_content_start + relative_close;
                let marker_end = marker_close + 1;
                if let Some((id, name)) =
                    parse_marker_body(&text[marker_content_start..marker_close])
                {
                    if text_start < marker_start {
                        segments.push(ChatSegment::Text(text[text_start..marker_start].to_owned()));
                    }
                    segments.push(ChatSegment::Emote {
                        id: Some(id.to_owned()),
                        name: name.to_owned(),
                    });
                    text_start = marker_end;
                    search_from = marker_end;
                } else {
                    search_from = marker_end;
                }
            }
            (_, Some(colon_start)) => {
                if let Some((name, end)) = parse_name_shortcode(text, colon_start) {
                    if text_start < colon_start {
                        segments.push(ChatSegment::Text(text[text_start..colon_start].to_owned()));
                    }
                    segments.push(ChatSegment::Emote {
                        id: None,
                        name: name.to_owned(),
                    });
                    text_start = end;
                    search_from = end;
                } else {
                    // The colon may be punctuation or part of a URL. Preserve
                    // it and continue looking for a later complete shortcode.
                    search_from = colon_start + 1;
                }
            }
        }
    }

    if text_start < text.len() || segments.is_empty() {
        segments.push(ChatSegment::Text(text[text_start..].to_owned()));
    }

    segments
}

fn bounded_prefix(text: &str, max_bytes: usize) -> &str {
    let mut end = text.len().min(max_bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn parse_marker_body(body: &str) -> Option<(&str, &str)> {
    let separator = body.find(':')?;
    let id = &body[..separator];
    let name = &body[separator + 1..];

    if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit()) || name.is_empty() {
        return None;
    }
    if name.len() > MAX_EMOTE_NAME_BYTES
        || name.chars().any(|character| {
            character.is_whitespace()
                || character.is_control()
                || matches!(character, ':' | '[' | ']')
        })
    {
        return None;
    }

    Some((id, name))
}

fn parse_name_shortcode(text: &str, start: usize) -> Option<(&str, usize)> {
    let after_open = start.checked_add(1)?;
    let relative_close = text[after_open..].find(':')?;
    let close = after_open + relative_close;
    let name = &text[after_open..close];
    if name.is_empty() || name.len() > MAX_EMOTE_NAME_BYTES {
        return None;
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return None;
    }
    Some((name, close + 1))
}

#[cfg(test)]
mod tests {
    use super::{ChannelEmote, ChatSegment, decode_channel_emote_catalog, parse_chat_segments};
    use kick_core::MAX_CHAT_TEXT_BYTES;

    #[test]
    fn decodes_observed_catalog_groups_and_emote_fields() {
        let payload = br#"[
          {
            "playback_url": "not-retained",
            "emotes": [
              {"channel_id": 123, "id": 456, "name": "sample", "subscribers_only": false},
              {"channel_id": 123, "id": 789, "name": "member", "subscribers_only": true}
            ]
          },
          {
            "id": "group",
            "name": "catalog group",
            "emotes": [
              {"channel_id": null, "id": 321, "name": "shared", "subscribers_only": false}
            ]
          }
        ]"#;

        let groups = decode_channel_emote_catalog(payload).unwrap();

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].emotes.len(), 2);
        assert_eq!(
            groups[0].emotes[0],
            ChannelEmote {
                channel_id: Some(123),
                id: 456,
                name: "sample".into(),
                subscribers_only: false,
            }
        );
        assert_eq!(groups[1].emotes[0].channel_id, None);
        assert_eq!(groups[1].emotes[0].id, 321);
        assert_eq!(groups[1].emotes[0].name, "shared");

        let debug = format!("{:?}", groups);
        assert!(!debug.contains("not-retained"));
        assert!(!debug.contains("sample"));
        assert!(!debug.contains("456"));
    }

    #[test]
    fn catalog_decoder_rejects_non_array_and_missing_emote_collection() {
        assert!(decode_channel_emote_catalog(br#"{}"#).is_err());
        assert!(decode_channel_emote_catalog(br#"[{"id":"group"}]"#).is_err());
    }

    #[test]
    fn parses_emote_between_text() {
        assert_eq!(
            parse_chat_segments("before [emote:5748088:collectiblesPepeLaugh] after"),
            vec![
                ChatSegment::Text("before ".into()),
                ChatSegment::Emote {
                    id: Some("5748088".into()),
                    name: "collectiblesPepeLaugh".into(),
                },
                ChatSegment::Text(" after".into()),
            ]
        );
    }

    #[test]
    fn parses_adjacent_and_multiple_emotes_in_order() {
        assert_eq!(
            parse_chat_segments("[emote:1:first][emote:2:second] ! [emote:3:third]"),
            vec![
                ChatSegment::Emote {
                    id: Some("1".into()),
                    name: "first".into(),
                },
                ChatSegment::Emote {
                    id: Some("2".into()),
                    name: "second".into(),
                },
                ChatSegment::Text(" ! ".into()),
                ChatSegment::Emote {
                    id: Some("3".into()),
                    name: "third".into(),
                },
            ]
        );
    }

    #[test]
    fn preserves_unicode_and_multilingual_text() {
        assert_eq!(
            parse_chat_segments("こんにちは 🌸 [emote:7:笑顔] مرحبا"),
            vec![
                ChatSegment::Text("こんにちは 🌸 ".into()),
                ChatSegment::Emote {
                    id: Some("7".into()),
                    name: "笑顔".into(),
                },
                ChatSegment::Text(" مرحبا".into()),
            ]
        );
    }

    #[test]
    fn leaves_malformed_placeholders_as_text() {
        let malformed = "x [emote:nope:name] y [emote:2:] z [emote:3:two:parts] [emote:4:open";
        assert_eq!(
            parse_chat_segments(malformed),
            vec![ChatSegment::Text(malformed.into())]
        );
    }

    #[test]
    fn keeps_malformed_text_and_parses_a_later_valid_marker() {
        assert_eq!(
            parse_chat_segments("[emote:bad:name] [emote:5:valid]"),
            vec![
                ChatSegment::Text("[emote:bad:name] ".into()),
                ChatSegment::Emote {
                    id: Some("5".into()),
                    name: "valid".into(),
                },
            ]
        );
    }

    #[test]
    fn clips_oversized_input_at_a_utf8_boundary() {
        let oversized = format!("{}[emote:9:after-limit]", "あ".repeat(MAX_CHAT_TEXT_BYTES));
        let segments = parse_chat_segments(&oversized);

        assert_eq!(segments.len(), 1);
        let ChatSegment::Text(text) = &segments[0] else {
            panic!("oversized suffix past the byte limit must not be parsed");
        };
        assert_eq!(text, &"あ".repeat(MAX_CHAT_TEXT_BYTES / "あ".len()));
        assert!(text.len() <= MAX_CHAT_TEXT_BYTES);
    }

    #[test]
    fn wraps_text_without_markers_in_one_segment() {
        assert_eq!(
            parse_chat_segments("plain text 文字 🙂"),
            vec![ChatSegment::Text("plain text 文字 🙂".into())]
        );
    }

    #[test]
    fn parses_name_only_kick_shortcodes_and_adjacent_tokens() {
        assert_eq!(
            parse_chat_segments("hi :collectiblesCarRetro::Wave-1: !"),
            vec![
                ChatSegment::Text("hi ".into()),
                ChatSegment::Emote {
                    id: None,
                    name: "collectiblesCarRetro".into(),
                },
                ChatSegment::Emote {
                    id: None,
                    name: "Wave-1".into(),
                },
                ChatSegment::Text(" !".into()),
            ]
        );
    }

    #[test]
    fn shortcode_parser_keeps_urls_and_invalid_tokens_as_text() {
        let text = "https://example.com/a:b :two words: :bad/name:";
        assert_eq!(
            parse_chat_segments(text),
            vec![ChatSegment::Text(text.into())]
        );
    }
}
