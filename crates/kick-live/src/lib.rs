//! Async transports for documented Kick reads and explicitly scoped anonymous
//! website endpoints verified by this project.
//!
//! Official API methods require the documented bearer credential. Anonymous
//! catalog, channel, history, playback-source, search, and realtime methods
//! remain separate and are grounded in the evidence ledger; their internal
//! routes may change. Never log credential values, chat bodies, or signed media
//! URLs, and do not treat a successful route probe as a stable API contract.

#[cfg(feature = "realtime")]
pub mod realtime;

pub mod emotes;
pub mod hls;
pub mod search;
pub mod session;

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kick_core::{PlaybackSource, StreamSummary};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use reqwest::{Client, RequestBuilder, Response, Url};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use zeroize::Zeroize;

use crate::session::SessionCookie;

const API_BASE: &str = "https://api.kick.com";
const LIVE_STREAMS_PATH: &str = "/public/v2/livestreams";
const INTERNAL_API_BASE: &str = "https://api.kick.com";
const INTERNAL_LIVESTREAMS_PATH: &str = "/private/v1/livestreams";
const CHANNEL_LOOKUP_BASE: &str = "https://kick.com/api/v2/channels";
const CHANNEL_EMOTES_BASE: &str = "https://kick.com/emotes";
const WEB_API_BASE: &str = "https://web.kick.com/api/v1";
const CHAT_SEND_BASE: &str = "https://kick.com/api/v2/messages/send";
const CHAT_SEND_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_CHAT_SEND_BYTES: usize = 1024;
const PRODUCT_USER_AGENT: &str = "kick-native/0.1";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_CURSOR_BYTES: usize = 4 * 1024;
const MAX_ACCESS_TOKEN_BYTES: usize = 16 * 1024;
const MAX_FILTER_VALUES: usize = 25;
const MAX_PAGE_SIZE: usize = 1_000;
const DEFAULT_PAGE_SIZE: usize = 100;
const MAX_CHANNEL_SLUG_BYTES: usize = 256;

/// An access token kept out of `Debug` output and zeroized when dropped.
pub struct AccessToken(String);

impl AccessToken {
    /// Creates an access token from a value supplied by the auth layer.
    pub fn new(value: impl Into<String>) -> Result<Self, KickApiError> {
        let mut value = value.into();
        if value.trim().is_empty() || value.len() > MAX_ACCESS_TOKEN_BYTES {
            value.zeroize();
            return Err(KickApiError::InvalidAccessToken);
        }
        Ok(Self(value))
    }

    fn authorization_header(&self) -> Result<HeaderValue, KickApiError> {
        let mut bytes = Vec::with_capacity(b"Bearer ".len() + self.0.len());
        bytes.extend_from_slice(b"Bearer ");
        bytes.extend_from_slice(self.0.as_bytes());

        let header = HeaderValue::from_bytes(&bytes).map_err(|_| KickApiError::InvalidAccessToken);
        bytes.zeroize();

        let mut header = header?;
        header.set_sensitive(true);
        Ok(header)
    }
}

impl Drop for AccessToken {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl fmt::Debug for AccessToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AccessToken(<redacted>)")
    }
}

/// Failure class for an HTTP request, without response bodies or credentials.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportFailure {
    Timeout,
    Connection,
    Request,
    ResponseBody,
    Other,
}

/// A safe-to-display Kick API error.
///
/// Error responses are not buffered or parsed, so the error cannot expose
/// response content, request headers, or token values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KickApiError {
    InvalidAccessToken,
    InvalidQuery(&'static str),
    InvalidChannelSlug,
    InvalidChannelId,
    Transport(TransportFailure),
    HttpStatus(u16),
    ResponseTooLarge,
    InvalidJson,
    SessionRequired,
    SessionInvalid,
    /// A chat send was rejected with a known server error code.
    ChatSendRejected {
        status: u16,
        code: &'static str,
    },
}

impl fmt::Display for KickApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidAccessToken => formatter.write_str("invalid Kick access token"),
            Self::InvalidQuery(reason) => write!(formatter, "invalid Kick query: {reason}"),
            Self::InvalidChannelSlug => formatter.write_str("invalid Kick channel slug"),
            Self::InvalidChannelId => formatter.write_str("invalid Kick channel ID"),
            Self::Transport(TransportFailure::Timeout) => {
                formatter.write_str("Kick request timed out")
            }
            Self::Transport(TransportFailure::Connection) => {
                formatter.write_str("could not connect to Kick API")
            }
            Self::Transport(TransportFailure::Request) => {
                formatter.write_str("Kick request could not be sent")
            }
            Self::Transport(TransportFailure::ResponseBody) => {
                formatter.write_str("Kick response body could not be read")
            }
            Self::Transport(TransportFailure::Other) => {
                formatter.write_str("Kick network request failed")
            }
            Self::HttpStatus(status) => write!(formatter, "Kick API returned HTTP {status}"),
            Self::ResponseTooLarge => {
                formatter.write_str("Kick API response exceeded the local size limit")
            }
            Self::InvalidJson => formatter.write_str("Kick API returned an invalid response"),
            Self::SessionRequired => {
                formatter.write_str("Kick sign-in is required for this request")
            }
            Self::ChatSendRejected { status, code } => {
                write!(
                    formatter,
                    "Kick rejected the chat message (HTTP {status}): {code}"
                )
            }
            Self::SessionInvalid => {
                formatter.write_str("Kick sign-in headers could not be constructed")
            }
        }
    }
}

impl std::error::Error for KickApiError {}

/// Query supported by Kick's documented `GET /public/v2/livestreams` route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveStreamsQuery {
    /// Maximum number of results per page, clamped to the API's 1–1000 range.
    pub limit: usize,
    /// Opaque cursor returned by Kick for the next page.
    pub cursor: Option<String>,
    /// Optional Kick category IDs. The API accepts at most 25 values.
    pub category_ids: Vec<u64>,
    /// Optional BCP 47 language codes. The API accepts at most 25 values.
    pub language_codes: Vec<String>,
}

impl Default for LiveStreamsQuery {
    fn default() -> Self {
        Self {
            limit: DEFAULT_PAGE_SIZE,
            cursor: None,
            category_ids: Vec::new(),
            language_codes: Vec::new(),
        }
    }
}

/// One page from the active-livestream API.
#[derive(Clone, PartialEq, Eq)]
pub struct LiveStreamsPage {
    pub items: Vec<LiveStream>,
    /// Empty/absent means the API supplied no next cursor.
    pub next_cursor: Option<String>,
    pub message: String,
}

impl fmt::Debug for LiveStreamsPage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LiveStreamsPage")
            .field("items", &self.items)
            .field("has_next_cursor", &self.next_cursor.is_some())
            .field("message", &self.message)
            .finish()
    }
}

/// A public active livestream returned by Kick's documented v2 API.
#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct LiveStream {
    #[serde(default)]
    pub broadcaster_user: LiveUser,
    #[serde(default)]
    pub category: Option<LiveCategory>,
    #[serde(default)]
    pub channel: LiveChannel,
    #[serde(default)]
    pub has_mature_content: bool,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub language_code: String,
    #[serde(default)]
    pub started_at: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub thumbnail: Option<String>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub viewer_count: u64,
}

impl LiveStream {
    /// Converts the documented public fields to the app's transport-free model.
    pub fn to_summary(&self) -> StreamSummary {
        StreamSummary {
            channel_slug: self.channel.slug.clone(),
            channel_name: self.broadcaster_user.username.clone(),
            title: self.title.clone(),
            category: self.category.as_ref().map(|category| category.name.clone()),
            viewer_count: Some(self.viewer_count),
            thumbnail_url: self.thumbnail.clone(),
            profile_picture_url: self.broadcaster_user.profile_picture.clone(),
        }
    }
}

impl fmt::Debug for LiveStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LiveStream")
            .field("channel", &self.channel)
            .field("broadcaster_user", &self.broadcaster_user)
            .field("title", &self.title)
            .field("category", &self.category)
            .field("viewer_count", &self.viewer_count)
            .field("started_at", &self.started_at)
            .field("language_code", &self.language_code)
            .field("has_mature_content", &self.has_mature_content)
            .field("tags", &self.tags)
            .field("thumbnail", &self.thumbnail.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

#[derive(Clone, Default, PartialEq, Eq, Deserialize)]
pub struct LiveUser {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub profile_picture: Option<String>,
    #[serde(default)]
    pub username: String,
}

impl fmt::Debug for LiveUser {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LiveUser")
            .field("id", &self.id)
            .field(
                "profile_picture",
                &self.profile_picture.as_ref().map(|_| "<redacted>"),
            )
            .field("username", &self.username)
            .finish()
    }
}

#[derive(Clone, Default, PartialEq, Eq, Deserialize)]
pub struct LiveCategory {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub thumbnail: Option<String>,
}

impl fmt::Debug for LiveCategory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LiveCategory")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("thumbnail", &self.thumbnail.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

#[derive(Clone, Default, PartialEq, Eq, Deserialize)]
pub struct LiveChannel {
    #[serde(default)]
    pub slug: String,
}

impl fmt::Debug for LiveChannel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LiveChannel")
            .field("slug", &self.slug)
            .finish()
    }
}

/// One catalog record decoded from an observed anonymous Kick internal API.
///
/// This DTO retains only fields needed by the app. The observed `playback_url`
/// is handed to [`PlaybackSource`], whose `Debug` implementation redacts it.
/// The endpoint's relationship to the Kick APK is not established.
#[derive(Clone)]
pub struct AnonymousCatalogStream {
    pub channel_id: Option<String>,
    pub channel_slug: Option<String>,
    pub channel_name: Option<String>,
    pub title: Option<String>,
    pub category: Option<String>,
    pub viewer_count: Option<u64>,
    pub thumbnail_url: Option<String>,
    pub profile_picture_url: Option<String>,
    pub playback_source: Option<PlaybackSource>,
}

impl AnonymousCatalogStream {
    /// Converts this record when the identifying display fields are present.
    pub fn to_summary(&self) -> Option<StreamSummary> {
        Some(StreamSummary {
            channel_slug: self.channel_slug.clone()?,
            channel_name: self.channel_name.clone()?,
            title: self.title.clone()?,
            category: self.category.clone(),
            viewer_count: self.viewer_count,
            thumbnail_url: self.thumbnail_url.clone(),
            profile_picture_url: self.profile_picture_url.clone(),
        })
    }
}

impl fmt::Debug for AnonymousCatalogStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AnonymousCatalogStream")
            .field("channel_slug", &self.channel_slug)
            .field("channel_name", &self.channel_name)
            .field("title", &self.title)
            .field("category", &self.category)
            .field("viewer_count", &self.viewer_count)
            .field(
                "thumbnail_url",
                &self.thumbnail_url.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "profile_picture_url",
                &self.profile_picture_url.as_ref().map(|_| "<redacted>"),
            )
            .field("playback_source", &self.playback_source)
            .finish()
    }
}

/// One observed page from the anonymous internal livestream catalog.
#[derive(Clone)]
pub struct AnonymousCatalogPage {
    pub items: Vec<AnonymousCatalogStream>,
    pub next_cursor: Option<String>,
    pub version: Option<String>,
}

impl fmt::Debug for AnonymousCatalogPage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AnonymousCatalogPage")
            .field("items", &self.items)
            .field("has_next_cursor", &self.next_cursor.is_some())
            .field("version", &self.version)
            .finish()
    }
}

/// Runtime connection settings; URLs may contain private access material.
#[derive(Deserialize)]
pub struct RealtimeConfiguration {
    pub connections: Vec<RealtimeConnection>,
}
#[derive(Deserialize)]
pub struct RealtimeConnection {
    pub provider: String,
    pub credentials: RealtimeCredentials,
}
#[derive(Deserialize)]
pub struct RealtimeCredentials {
    pub url: String,
}
impl fmt::Debug for RealtimeConfiguration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RealtimeConfiguration")
            .field("connections", &self.connections.len())
            .finish()
    }
}
#[derive(Deserialize)]
struct RealtimeEnvelope {
    data: RealtimeConfiguration,
}

/// Sanitized fields from an observed anonymous channel response.
#[derive(Clone)]
pub struct AnonymousChannelSnapshot {
    pub chatroom_id: Option<u64>,
    /// Canonical numeric channel ID from the channel lookup response.
    pub id: Option<u64>,
    pub slug: Option<String>,
    /// The API returns this field as a string in the observed response.
    pub followers_count: Option<String>,
    pub username: Option<String>,
    pub profile_picture_url: Option<String>,
    pub bio: Option<String>,
    pub twitter: Option<String>,
    pub instagram: Option<String>,
    pub youtube: Option<String>,
    pub discord: Option<String>,
    pub tiktok: Option<String>,
    pub facebook: Option<String>,
    pub club: Option<String>,
    pub verified: bool,
    pub banner_url: Option<String>,
    pub offline_banner_url: Option<String>,
    pub language: Option<String>,
    pub tags: Vec<String>,
    pub livestream: Option<AnonymousChannelLivestream>,
    pub playback_source: Option<PlaybackSource>,
}

impl AnonymousChannelSnapshot {
    pub fn is_live(&self) -> bool {
        self.livestream
            .as_ref()
            .and_then(|livestream| livestream.is_live)
            .unwrap_or(false)
    }
}

impl fmt::Debug for AnonymousChannelSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AnonymousChannelSnapshot")
            .field("has_id", &self.id.is_some())
            .field("slug", &self.slug)
            .field("followers_count", &self.followers_count)
            .field("username", &self.username)
            .field(
                "profile_picture_url",
                &self.profile_picture_url.as_ref().map(|_| "<redacted>"),
            )
            .field("has_bio", &self.bio.is_some())
            .field("verified", &self.verified)
            .field("language", &self.language)
            .field("tags", &self.tags)
            .field(
                "banner_url",
                &self.banner_url.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "offline_banner_url",
                &self.offline_banner_url.as_ref().map(|_| "<redacted>"),
            )
            .field("livestream", &self.livestream)
            .field("playback_source", &self.playback_source)
            .finish()
    }
}

/// One followed channel from the observed authenticated sidebar route.
#[derive(Clone, PartialEq, Eq)]
pub struct FollowedChannel {
    pub channel_slug: String,
    pub channel_name: String,
    pub is_live: bool,
    pub category_name: Option<String>,
    pub session_title: Option<String>,
    pub viewer_count: Option<u64>,
    pub profile_picture_url: Option<String>,
}

impl FollowedChannel {
    /// Converts a followed-channel record into the shared stream summary model.
    pub fn to_summary(&self) -> StreamSummary {
        StreamSummary {
            channel_slug: self.channel_slug.clone(),
            channel_name: self.channel_name.clone(),
            title: self.session_title.clone().unwrap_or_default(),
            category: self.category_name.clone(),
            viewer_count: self.viewer_count,
            thumbnail_url: None,
            profile_picture_url: self.profile_picture_url.clone(),
        }
    }
}

impl fmt::Debug for FollowedChannel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FollowedChannel")
            .field("channel_slug", &self.channel_slug)
            .field("channel_name", &self.channel_name)
            .field("is_live", &self.is_live)
            .field("category_name", &self.category_name)
            .field("session_title", &self.session_title)
            .field("viewer_count", &self.viewer_count)
            .field(
                "profile_picture_url",
                &self.profile_picture_url.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// One page of followed channels from `GET /api/v2/channels/followed`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FollowedChannelsPage {
    pub channels: Vec<FollowedChannel>,
    pub next_cursor: Option<String>,
}

/// Live metadata exposed by the observed channel response.
#[derive(Clone, Debug)]
pub struct AnonymousChannelLivestream {
    pub is_live: Option<bool>,
    pub title: Option<String>,
    pub viewer_count: Option<u64>,
    pub started_at: Option<String>,
    pub language: Option<String>,
    pub tags: Vec<String>,
}

/// One channel-point reward offered by a channel.
#[derive(Clone, Debug)]
pub struct ChannelReward {
    pub id: String,
    pub title: String,
    pub cost: u64,
    pub description: Option<String>,
}

/// One past or active broadcast from the observed channel videos route.
#[derive(Clone)]
pub struct ChannelVideo {
    pub id: u64,
    pub uuid: String,
    pub title: String,
    pub created_at: Option<String>,
    pub duration_seconds: u64,
    pub views: Option<u64>,
    pub thumbnail_url: Option<String>,
    pub category: Option<String>,
    pub is_live: bool,
}

/// One clip from the observed channel clips route.
#[derive(Clone)]
pub struct ChannelClip {
    pub id: String,
    pub clip_url: Option<String>,
    pub title: String,
    pub duration_seconds: u64,
    pub views: Option<u64>,
    pub thumbnail_url: Option<String>,
    pub created_at: Option<String>,
}

/// One page of channel clips plus the opaque cursor for the next page.
#[derive(Clone, Default)]
pub struct ChannelClipPage {
    pub clips: Vec<ChannelClip>,
    pub next_cursor: Option<String>,
}

/// One recently used category from the observed recent-categories route.
#[derive(Clone)]
pub struct ChannelCategory {
    pub id: u64,
    pub name: String,
    pub slug: String,
    pub viewers: Option<u64>,
    pub banner_url: Option<String>,
    pub tags: Vec<String>,
}

/// The active follower goal configured by a channel.
#[derive(Clone)]
pub struct FollowerGoal {
    pub target: u64,
    pub current: u64,
    pub emoji_id: Option<String>,
}

/// Bounded channel-page extras used by the native account page.
#[derive(Clone, Default)]
pub struct ChannelPageData {
    pub videos: Vec<ChannelVideo>,
    pub clips: Vec<ChannelClip>,
    /// Cursor for the next clips page; `None` means the first page is complete.
    pub clips_cursor: Option<String>,
    pub categories: Vec<ChannelCategory>,
    pub follower_goal: Option<FollowerGoal>,
}

/// One message from an observed anonymous HTTP history snapshot.
///
/// This is a finite history page, not a realtime chat subscription. Message
/// content and sender identity are retained for display, but always redacted
/// from `Debug` output.
#[derive(Clone, PartialEq, Eq)]
pub struct AnonymousChatMessage {
    pub id: Option<String>,
    pub content: Option<String>,
    pub created_at: Option<String>,
    pub username: Option<String>,
    pub author_slug: Option<String>,
    pub badges: Vec<kick_core::ChatBadge>,
}

impl fmt::Debug for AnonymousChatMessage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AnonymousChatMessage")
            .field("has_id", &self.id.is_some())
            .field("has_content", &self.content.is_some())
            .field("created_at", &self.created_at)
            .field("has_sender", &self.username.is_some())
            .finish()
    }
}

/// One finite page of anonymous recent chat history.
#[derive(Clone)]
pub struct AnonymousRecentMessages {
    pub messages: Vec<AnonymousChatMessage>,
}

impl fmt::Debug for AnonymousRecentMessages {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AnonymousRecentMessages")
            .field("message_count", &self.messages.len())
            .finish()
    }
}

impl AnonymousRecentMessages {
    /// Counts messages with both non-empty sender names and body text.
    pub fn count_with_nonempty_sender_and_content(&self) -> usize {
        self.messages
            .iter()
            .filter(|message| {
                is_nonempty(message.username.as_deref()) && is_nonempty(message.content.as_deref())
            })
            .count()
    }
}

fn is_nonempty(value: Option<&str>) -> bool {
    value.is_some_and(|value| !value.trim().is_empty())
}

#[derive(Deserialize)]
struct AnonymousCatalogEnvelope {
    data: AnonymousCatalogData,
}

#[derive(Deserialize)]
struct AnonymousCatalogData {
    #[serde(default)]
    livestreams: Vec<AnonymousCatalogStreamWire>,
    #[serde(default)]
    next_cursor: Option<String>,
    #[serde(default)]
    version: Option<String>,
}

#[derive(Deserialize)]
struct AnonymousCatalogStreamWire {
    #[serde(default)]
    streamer: Option<AnonymousCatalogStreamerWire>,
    #[serde(default)]
    metadata: Option<AnonymousCatalogMetadataWire>,
    #[serde(default)]
    viewers_count: Option<u64>,
    #[serde(default)]
    thumbnail_url: Option<String>,
    #[serde(default)]
    playback_url: Option<String>,
}

#[derive(Deserialize)]
struct AnonymousCatalogStreamerWire {
    #[serde(default)]
    user: Option<AnonymousCatalogUserWire>,
    #[serde(default)]
    channel: Option<AnonymousCatalogChannelWire>,
}

#[derive(Deserialize)]
struct AnonymousCatalogUserWire {
    #[serde(default)]
    username: Option<String>,
    #[serde(default, alias = "profile_pic")]
    profile_picture: Option<String>,
}

#[derive(Deserialize)]
struct AnonymousCatalogChannelWire {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    slug: Option<String>,
}

#[derive(Deserialize)]
struct AnonymousCatalogMetadataWire {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    category: Option<AnonymousCatalogCategoryWire>,
}

#[derive(Deserialize)]
struct AnonymousCatalogCategoryWire {
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
struct AnonymousChannelWire {
    #[serde(default)]
    chatroom: Option<AnonymousChatroomWire>,
    #[serde(default)]
    id: Option<u64>,
    #[serde(default)]
    slug: Option<String>,
    #[serde(default)]
    followers_count: Option<String>,
    #[serde(default)]
    verified: Option<bool>,
    #[serde(default)]
    banner_image: Option<AnonymousBannerWire>,
    #[serde(default)]
    offline_banner_image: Option<AnonymousBannerWire>,
    #[serde(default)]
    user: Option<AnonymousChannelUserWire>,
    #[serde(default)]
    livestream: Option<AnonymousChannelLivestreamWire>,
    #[serde(default)]
    playback_url: Option<String>,
}

#[derive(Deserialize)]
struct AnonymousBannerWire {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    src: Option<String>,
    #[serde(default)]
    srcset: Option<String>,
}

#[derive(Deserialize)]
struct AnonymousChatroomWire {
    id: Option<u64>,
}

#[derive(Deserialize)]
struct AnonymousChannelUserWire {
    #[serde(default)]
    username: Option<String>,
    #[serde(default, alias = "profile_picture")]
    profile_pic: Option<String>,
    #[serde(default)]
    bio: Option<String>,
    #[serde(default)]
    twitter: Option<String>,
    #[serde(default)]
    instagram: Option<String>,
    #[serde(default)]
    youtube: Option<String>,
    #[serde(default)]
    discord: Option<String>,
    #[serde(default)]
    tiktok: Option<String>,
    #[serde(default)]
    facebook: Option<String>,
    #[serde(default)]
    club: Option<String>,
}

#[derive(Deserialize)]
struct AnonymousChannelLivestreamWire {
    #[serde(default)]
    is_live: Option<bool>,
    #[serde(default)]
    session_title: Option<String>,
    #[serde(default)]
    viewer_count: Option<u64>,
    #[serde(default)]
    start_time: Option<String>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    categories: Vec<AnonymousCategoryWire>,
    #[serde(default)]
    video: Option<ChannelVideoRefWire>,
}

#[derive(Default, Deserialize)]
struct ChannelVideoRefWire {
    #[serde(default)]
    uuid: Option<String>,
}

#[derive(Deserialize, Default)]
struct AnonymousCategoryWire {
    #[serde(default)]
    id: Option<u64>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    slug: Option<String>,
    #[serde(default)]
    viewers: Option<u64>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    banner: Option<AnonymousBannerWire>,
}

#[derive(Deserialize)]
struct ChannelVideoWire {
    #[serde(default)]
    id: u64,
    #[serde(default)]
    session_title: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    duration: Option<u64>,
    #[serde(default)]
    viewer_count: Option<u64>,
    #[serde(default)]
    views: Option<u64>,
    #[serde(default)]
    is_live: Option<bool>,
    #[serde(default)]
    thumbnail: Option<AnonymousBannerWire>,
    #[serde(default)]
    categories: Vec<AnonymousCategoryWire>,
    #[serde(default)]
    video: Option<ChannelVideoRefWire>,
}

#[derive(Deserialize)]
struct ChannelClipsEnvelope {
    #[serde(default)]
    clips: Vec<ChannelClipWire>,
    #[serde(
        default,
        rename = "nextCursor",
        deserialize_with = "deserialize_cursor"
    )]
    next_cursor: Option<String>,
}

#[derive(Deserialize)]
struct ChannelClipWire {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    duration: Option<u64>,
    #[serde(default)]
    view_count: Option<u64>,
    #[serde(default)]
    thumbnail_url: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    clip_url: Option<String>,
}

#[derive(Deserialize)]
struct ChannelGoalWire {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    target_value: Option<u64>,
    #[serde(default)]
    current_value: Option<u64>,
    #[serde(default)]
    progress_bar_emoji_id: Option<String>,
}

#[derive(Deserialize)]
struct WebLivestreamEnvelope {
    #[serde(default)]
    data: Option<WebLivestreamData>,
}

#[derive(Deserialize)]
struct WebLivestreamData {
    #[serde(default)]
    livestream: Option<WebLivestreamWire>,
}

#[derive(Deserialize)]
struct WebLivestreamWire {
    #[serde(default)]
    id: Option<String>,
}

#[derive(Deserialize)]
struct PlaybackInitResponse {
    #[serde(default)]
    playback_url: Option<PlaybackInitUrls>,
}

#[derive(Deserialize)]
struct PlaybackInitUrls {
    #[serde(default)]
    dvr: Option<String>,
    #[serde(default)]
    vod: Option<String>,
}

#[derive(Deserialize)]
struct FollowedChannelsWire {
    #[serde(default)]
    channels: Vec<FollowedChannelWire>,
    #[serde(
        default,
        rename = "nextCursor",
        deserialize_with = "deserialize_cursor"
    )]
    next_cursor: Option<String>,
}

fn deserialize_cursor<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|value| match value {
        serde_json::Value::String(cursor) => Some(cursor),
        serde_json::Value::Number(cursor) => Some(cursor.to_string()),
        _ => None,
    }))
}

#[derive(Deserialize)]
struct FollowedChannelWire {
    #[serde(default)]
    channel_slug: Option<String>,
    #[serde(default)]
    user_username: Option<String>,
    #[serde(default)]
    is_live: Option<bool>,
    #[serde(default)]
    category_name: Option<String>,
    #[serde(default)]
    session_title: Option<String>,
    #[serde(default)]
    viewer_count: Option<u64>,
    #[serde(default, alias = "profile_pic")]
    profile_picture: Option<String>,
}

#[derive(Deserialize)]
struct AnonymousRecentMessagesEnvelope {
    data: AnonymousRecentMessagesData,
}

#[derive(Deserialize)]
struct AnonymousRecentMessagesData {
    #[serde(default)]
    messages: Vec<AnonymousChatMessageWire>,
}

#[derive(Deserialize)]
struct AnonymousChatMessageWire {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    sender: Option<AnonymousChatSenderWire>,
}

#[derive(Deserialize)]
struct AnonymousChatSenderWire {
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    slug: Option<String>,
    #[serde(default)]
    identity: Option<AnonymousChatIdentityWire>,
}

#[derive(Default, Deserialize)]
struct AnonymousChatIdentityWire {
    #[serde(default)]
    badges: Vec<AnonymousChatBadgeWire>,
}

#[derive(Deserialize)]
struct AnonymousChatBadgeWire {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    count: Option<u64>,
}

impl From<AnonymousCatalogStreamWire> for AnonymousCatalogStream {
    fn from(stream: AnonymousCatalogStreamWire) -> Self {
        let (channel_name, profile_picture_url, (channel_slug, channel_id)) = stream
            .streamer
            .map(|streamer| {
                let (channel_name, profile_picture_url) = streamer
                    .user
                    .map(|user| (user.username, user.profile_picture))
                    .unwrap_or_default();
                (
                    channel_name,
                    profile_picture_url,
                    streamer
                        .channel
                        .map(|channel| (channel.slug, channel.id))
                        .unwrap_or_default(),
                )
            })
            .unwrap_or_default();
        let (title, category) = stream
            .metadata
            .map(|metadata| {
                (
                    metadata.title,
                    metadata.category.and_then(|category| category.name),
                )
            })
            .unwrap_or_default();

        Self {
            channel_id,
            channel_slug,
            channel_name,
            title,
            category,
            viewer_count: stream.viewers_count,
            thumbnail_url: stream.thumbnail_url,
            profile_picture_url,
            playback_source: playback_source(stream.playback_url),
        }
    }
}

impl From<AnonymousCatalogEnvelope> for AnonymousCatalogPage {
    fn from(response: AnonymousCatalogEnvelope) -> Self {
        Self {
            items: response
                .data
                .livestreams
                .into_iter()
                .map(AnonymousCatalogStream::from)
                .collect(),
            next_cursor: response
                .data
                .next_cursor
                .filter(|cursor| !cursor.is_empty()),
            version: response.data.version,
        }
    }
}

impl From<AnonymousChannelWire> for AnonymousChannelSnapshot {
    fn from(channel: AnonymousChannelWire) -> Self {
        let user = channel.user;
        let username = user.as_ref().and_then(|user| user.username.clone());
        let profile_picture_url = user.as_ref().and_then(|user| user.profile_pic.clone());
        let bio = user.as_ref().and_then(|user| user.bio.clone());
        let twitter = user.as_ref().and_then(|user| user.twitter.clone());
        let instagram = user.as_ref().and_then(|user| user.instagram.clone());
        let youtube = user.as_ref().and_then(|user| user.youtube.clone());
        let discord = user.as_ref().and_then(|user| user.discord.clone());
        let tiktok = user.as_ref().and_then(|user| user.tiktok.clone());
        let facebook = user.as_ref().and_then(|user| user.facebook.clone());
        let club = user.as_ref().and_then(|user| user.club.clone());
        let verified = channel.verified.unwrap_or(false);
        let banner_url = channel
            .banner_image
            .and_then(|banner| banner.url.or(banner.src));
        let offline_banner_url = channel
            .offline_banner_image
            .and_then(|banner| banner.url.or(banner.src));
        let has_active_livestream = channel
            .livestream
            .as_ref()
            .is_some_and(|livestream| livestream.is_live == Some(true));
        let language = channel
            .livestream
            .as_ref()
            .and_then(|livestream| livestream.language.clone());
        let tags: Vec<String> = channel
            .livestream
            .as_ref()
            .map(|livestream| {
                livestream
                    .categories
                    .iter()
                    .flat_map(|category| category.tags.iter().cloned())
                    .collect()
            })
            .unwrap_or_default();
        let livestream = channel
            .livestream
            .map(|livestream| AnonymousChannelLivestream {
                is_live: livestream.is_live,
                title: livestream.session_title,
                viewer_count: livestream.viewer_count,
                started_at: livestream.start_time,
                language: livestream.language,
                tags: livestream
                    .categories
                    .into_iter()
                    .flat_map(|category| category.tags)
                    .collect(),
            });
        let playback_source = if has_active_livestream {
            playback_source(channel.playback_url)
        } else {
            None
        };

        Self {
            chatroom_id: channel.chatroom.and_then(|r| r.id),
            id: channel.id,
            slug: channel.slug,
            followers_count: channel.followers_count,
            username,
            profile_picture_url,
            bio,
            twitter,
            instagram,
            youtube,
            discord,
            tiktok,
            facebook,
            club,
            verified,
            banner_url,
            offline_banner_url,
            language,
            tags,
            livestream,
            playback_source,
        }
    }
}

impl From<AnonymousRecentMessagesEnvelope> for AnonymousRecentMessages {
    fn from(response: AnonymousRecentMessagesEnvelope) -> Self {
        Self {
            messages: response
                .data
                .messages
                .into_iter()
                .map(|message| {
                    let (username, author_slug, badges) = message
                        .sender
                        .map(|sender| {
                            let badges = sender
                                .identity
                                .map(|identity| {
                                    identity
                                        .badges
                                        .into_iter()
                                        .take(4)
                                        .filter_map(|badge| {
                                            let kind = badge.kind?;
                                            if kind.is_empty() {
                                                return None;
                                            }
                                            let text = badge.text.unwrap_or_default();
                                            let label = if kind == "subscriber" {
                                                badge
                                                    .count
                                                    .map(|count| count.to_string())
                                                    .unwrap_or(text)
                                            } else {
                                                text
                                            };
                                            Some(kick_core::ChatBadge { kind, text: label })
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            (sender.username, sender.slug, badges)
                        })
                        .unwrap_or_default();
                    AnonymousChatMessage {
                        id: message.id,
                        content: message.content,
                        created_at: message.created_at,
                        username,
                        author_slug,
                        badges,
                    }
                })
                .collect(),
        }
    }
}

impl From<ChannelVideoWire> for ChannelVideo {
    fn from(video: ChannelVideoWire) -> Self {
        Self {
            id: video.id,
            uuid: video.video.and_then(|video| video.uuid).unwrap_or_default(),
            title: video
                .session_title
                .filter(|title| !title.trim().is_empty())
                .unwrap_or_else(|| "配信タイトルなし".to_owned()),
            created_at: video.created_at,
            // The videos route reports duration in milliseconds; clips report seconds.
            duration_seconds: video.duration.unwrap_or(0) / 1000,
            views: video.views.or(video.viewer_count),
            thumbnail_url: video
                .thumbnail
                .and_then(|thumbnail| thumbnail.src.or(thumbnail.url)),
            category: video
                .categories
                .into_iter()
                .find_map(|category| category.name),
            is_live: video.is_live.unwrap_or(false),
        }
    }
}

impl From<ChannelClipWire> for ChannelClip {
    fn from(clip: ChannelClipWire) -> Self {
        Self {
            id: clip.id.unwrap_or_default(),
            clip_url: clip.clip_url,
            title: clip
                .title
                .filter(|title| !title.trim().is_empty())
                .unwrap_or_else(|| "クリップ".to_owned()),
            duration_seconds: clip.duration.unwrap_or(0),
            views: clip.view_count,
            thumbnail_url: clip.thumbnail_url,
            created_at: clip.created_at,
        }
    }
}

impl From<AnonymousCategoryWire> for ChannelCategory {
    fn from(category: AnonymousCategoryWire) -> Self {
        Self {
            id: category.id.unwrap_or(0),
            name: category
                .name
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| "カテゴリ".to_owned()),
            slug: category.slug.unwrap_or_default(),
            viewers: category.viewers,
            banner_url: category.banner.and_then(|banner| banner.src.or(banner.url)),
            tags: category.tags,
        }
    }
}

fn follower_goal_from_wire(goals: Vec<ChannelGoalWire>) -> Option<FollowerGoal> {
    goals.into_iter().find_map(|goal| {
        if goal.kind.as_deref() != Some("followers") || goal.status.as_deref() != Some("active") {
            return None;
        }
        let target = goal.target_value?;
        if target == 0 {
            return None;
        }
        Some(FollowerGoal {
            target,
            current: goal.current_value.unwrap_or(0).min(target),
            emoji_id: goal.progress_bar_emoji_id,
        })
    })
}

fn playback_source(raw_url: Option<String>) -> Option<PlaybackSource> {
    let raw_url = raw_url?;
    if raw_url.len() > MAX_RESPONSE_BYTES / 2 {
        return None;
    }
    let parsed = Url::parse(&raw_url).ok()?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return None;
    }

    Some(PlaybackSource {
        url: raw_url,
        quality_label: None,
    })
}

fn channel_lookup_url(slug: &str) -> Result<Url, KickApiError> {
    if slug.is_empty()
        || slug.len() > MAX_CHANNEL_SLUG_BYTES
        || slug.trim() != slug
        || slug == "."
        || slug == ".."
        || slug.contains('/')
        || slug.contains('\\')
        || slug.chars().any(char::is_control)
    {
        return Err(KickApiError::InvalidChannelSlug);
    }

    let mut url = Url::parse(CHANNEL_LOOKUP_BASE).map_err(|_| KickApiError::InvalidChannelSlug)?;
    url.path_segments_mut()
        .map_err(|_| KickApiError::InvalidChannelSlug)?
        .push(slug);
    Ok(url)
}

fn channel_emotes_url(slug: &str) -> Result<Url, KickApiError> {
    if slug.is_empty()
        || slug.len() > MAX_CHANNEL_SLUG_BYTES
        || slug.trim() != slug
        || slug == "."
        || slug == ".."
        || slug.contains('/')
        || slug.contains('\\')
        || slug.chars().any(char::is_control)
    {
        return Err(KickApiError::InvalidChannelSlug);
    }

    let mut url = Url::parse(CHANNEL_EMOTES_BASE).map_err(|_| KickApiError::InvalidChannelSlug)?;
    url.path_segments_mut()
        .map_err(|_| KickApiError::InvalidChannelSlug)?
        .push(slug);
    Ok(url)
}

fn channel_subresource_url(slug: &str, resource: &str) -> Result<Url, KickApiError> {
    let mut url = channel_lookup_url(slug)?;
    url.path_segments_mut()
        .map_err(|_| KickApiError::InvalidChannelSlug)?
        .push(resource);
    Ok(url)
}

/// Builds the clips URL. Kick's `nextCursor` is a JSON object encoded as a
/// string, so it is passed through as one opaque query value.
fn channel_clips_url(slug: &str, cursor: Option<&str>) -> Result<Url, KickApiError> {
    let mut url = channel_subresource_url(slug, "clips")?;
    if let Some(cursor) = cursor {
        if cursor.is_empty() || cursor.len() > MAX_CURSOR_BYTES {
            return Err(KickApiError::InvalidQuery("invalid clips cursor"));
        }
        url.query_pairs_mut().append_pair("cursor", cursor);
    }
    Ok(url)
}

fn web_channel_livestream_url(slug: &str) -> Result<Url, KickApiError> {
    if slug.is_empty()
        || slug.len() > MAX_CHANNEL_SLUG_BYTES
        || slug.trim() != slug
        || slug == "."
        || slug == ".."
        || slug.contains('/')
        || slug.contains('\\')
        || slug.chars().any(char::is_control)
    {
        return Err(KickApiError::InvalidChannelSlug);
    }

    let mut url = Url::parse(WEB_API_BASE).map_err(|_| KickApiError::InvalidChannelSlug)?;
    url.path_segments_mut()
        .map_err(|_| KickApiError::InvalidChannelSlug)?
        .push("channels")
        .push(slug)
        .push("livestream");
    Ok(url)
}

fn playback_init_url(video_id: &str) -> Result<Url, KickApiError> {
    if video_id.is_empty()
        || video_id.len() > 128
        || video_id.trim() != video_id
        || video_id == "."
        || video_id == ".."
        || video_id.contains('/')
        || video_id.contains('\\')
        || video_id.chars().any(char::is_control)
    {
        return Err(KickApiError::InvalidQuery("invalid playback video ID"));
    }

    let mut url = Url::parse(WEB_API_BASE)
        .map_err(|_| KickApiError::InvalidQuery("invalid playback video ID"))?;
    url.path_segments_mut()
        .map_err(|_| KickApiError::InvalidQuery("invalid playback video ID"))?
        .push("stream")
        .push(video_id)
        .push("playback");
    Ok(url)
}

fn recent_messages_url(channel_id: u64) -> Result<Url, KickApiError> {
    if channel_id == 0 {
        return Err(KickApiError::InvalidChannelId);
    }

    let mut url = Url::parse(CHANNEL_LOOKUP_BASE).map_err(|_| KickApiError::InvalidChannelId)?;
    url.path_segments_mut()
        .map_err(|_| KickApiError::InvalidChannelId)?
        .push(&channel_id.to_string())
        .push("messages");
    url.query_pairs_mut().append_pair("limit", "1");
    Ok(url)
}

/// Read-only client for Kick's documented Public API.
pub struct KickClient {
    http: Client,
    access_token: Option<AccessToken>,
    session_cookie: Option<SessionCookie>,
}

impl KickClient {
    /// Creates an authenticated client from an app- or user-access token.
    pub fn new(access_token: AccessToken) -> Result<Self, KickApiError> {
        Self::build(Some(access_token))
    }

    /// Creates an unauthenticated client for checking the API auth boundary.
    ///
    /// The official OpenAPI specification lists access-token auth for this
    /// route. This constructor is intended for a narrow diagnostic request.
    pub fn unauthenticated_for_probe() -> Result<Self, KickApiError> {
        Self::build(None)
    }

    fn build(access_token: Option<AccessToken>) -> Result<Self, KickApiError> {
        let http = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(PRODUCT_USER_AGENT)
            .build()
            .map_err(|error| KickApiError::Transport(classify_transport(&error)))?;
        Ok(Self {
            http,
            access_token,
            session_cookie: None,
        })
    }

    /// Stores the validated browser-session Cookie for authenticated website reads.
    ///
    /// The Cookie is never attached to anonymous catalog, search, playback, or
    /// realtime requests. It is used only by methods that explicitly require a
    /// signed-in session.
    pub fn set_session_cookie(&mut self, cookie: SessionCookie) {
        self.session_cookie = Some(cookie);
    }

    /// Drops the in-memory session Cookie without touching Keychain.
    pub fn clear_session_cookie(&mut self) {
        self.session_cookie = None;
    }

    /// Returns whether a validated session Cookie is available.
    pub fn has_session_cookie(&self) -> bool {
        self.session_cookie.is_some()
    }

    /// Sends a chat message through the observed web-session route.
    ///
    /// The route and JSON body are static observations from the public web
    /// client; this project has not sent a real message. Callers must invoke
    /// this only from an explicit user action, never from tests or startup.
    pub async fn send_chat_message(
        &self,
        chatroom_id: u64,
        content: &str,
    ) -> Result<(), KickApiError> {
        let content = content.trim();
        if content.is_empty()
            || content.len() > MAX_CHAT_SEND_BYTES
            || content.chars().any(char::is_control)
        {
            return Err(KickApiError::InvalidQuery("chat content"));
        }
        let cookie = self
            .session_cookie
            .as_ref()
            .ok_or(KickApiError::SessionRequired)?;

        let url = chat_send_url(chatroom_id)?;
        let payload = chat_send_payload(content);
        let body = serde_json::to_vec(&payload).map_err(|_| KickApiError::InvalidJson)?;
        let request = cookie
            .apply_auth_headers(self.http.post(url))
            .map_err(|_| KickApiError::SessionInvalid)?;
        let response = request
            .header(CONTENT_TYPE, "application/json")
            .body(body)
            .timeout(CHAT_SEND_TIMEOUT)
            .send()
            .await
            .map_err(|error| KickApiError::Transport(classify_transport(&error)))?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let code = chat_send_error_code(response).await;
            return Err(KickApiError::ChatSendRejected { status, code });
        }
        Ok(())
    }

    /// Lists the channel's enabled channel-point rewards (session required).
    pub async fn channel_point_rewards(
        &self,
        slug: &str,
    ) -> Result<Vec<ChannelReward>, KickApiError> {
        let cookie = self
            .session_cookie
            .as_ref()
            .ok_or(KickApiError::SessionRequired)?;
        let url = Url::parse(&format!(
            "https://kick.com/api/v2/channels/{slug}/rewards?is_enabled=true"
        ))
        .map_err(|_| KickApiError::InvalidQuery("rewards endpoint"))?;
        let request = cookie
            .apply_auth_headers(self.http.get(url))
            .map_err(|_| KickApiError::SessionInvalid)?;
        let response = request
            .timeout(CHAT_SEND_TIMEOUT)
            .send()
            .await
            .map_err(|error| KickApiError::Transport(classify_transport(&error)))?;
        if !response.status().is_success() {
            return Err(KickApiError::HttpStatus(response.status().as_u16()));
        }
        let body = limited_body(response).await?;
        let value: serde_json::Value =
            serde_json::from_slice(&body).map_err(|_| KickApiError::InvalidJson)?;
        let items = value
            .get("data")
            .and_then(|data| data.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(items
            .into_iter()
            .filter_map(|item| {
                let id = item.get("id")?.as_str()?.to_owned();
                let title = item.get("title")?.as_str()?.to_owned();
                let cost = item.get("cost").and_then(|cost| cost.as_u64()).unwrap_or(0);
                let description = item
                    .get("description")
                    .and_then(|description| description.as_str())
                    .map(str::to_owned);
                Some(ChannelReward {
                    id,
                    title,
                    cost,
                    description,
                })
            })
            .take(50)
            .collect())
    }

    /// Redeems one channel-point reward (session required).
    pub async fn redeem_channel_point_reward(
        &self,
        slug: &str,
        reward_id: &str,
    ) -> Result<(), KickApiError> {
        let cookie = self
            .session_cookie
            .as_ref()
            .ok_or(KickApiError::SessionRequired)?;
        let url = Url::parse(&format!(
            "https://kick.com/api/v2/channels/{slug}/rewards/{reward_id}/redeem"
        ))
        .map_err(|_| KickApiError::InvalidQuery("redeem endpoint"))?;
        let request = cookie
            .apply_auth_headers(self.http.post(url))
            .map_err(|_| KickApiError::SessionInvalid)?;
        let response = request
            .header(CONTENT_TYPE, "application/json")
            .body("{}")
            .timeout(CHAT_SEND_TIMEOUT)
            .send()
            .await
            .map_err(|error| KickApiError::Transport(classify_transport(&error)))?;
        if !response.status().is_success() {
            return Err(KickApiError::HttpStatus(response.status().as_u16()));
        }
        Ok(())
    }

    /// Replaces the token after the app's auth layer refreshes it.
    pub fn replace_access_token(&mut self, access_token: AccessToken) {
        self.access_token = Some(access_token);
    }

    /// Fetches the observed anonymous internal livestream catalog.
    ///
    /// The request uses the captured `limit=1` query, which returned 20 rows
    /// during observation; the server's limit semantics remain unknown. The
    /// internal route's relationship to the APK is also unverified. No bearer
    /// token is sent, even when this client also has a public API token.
    pub async fn anonymous_catalog(&self) -> Result<AnonymousCatalogPage, KickApiError> {
        let url = Url::parse(&format!("{INTERNAL_API_BASE}{INTERNAL_LIVESTREAMS_PATH}"))
            .map_err(|_| KickApiError::InvalidQuery("catalog endpoint URL is invalid"))?;
        let mut url = url;
        url.query_pairs_mut().append_pair("limit", "1");
        let response: AnonymousCatalogEnvelope = self.anonymous_get_json(url).await?;
        Ok(response.into())
    }

    /// Fetches one channel from the observed anonymous channel route.
    ///
    /// The slug is encoded as one URL path segment. `playback_url` is retained
    /// only in the redacted [`PlaybackSource`] type and is never sent in an
    /// Authorization header or included in this method's error output.
    pub async fn anonymous_channel(
        &self,
        slug: &str,
    ) -> Result<AnonymousChannelSnapshot, KickApiError> {
        let url = channel_lookup_url(slug)?;
        let response: AnonymousChannelWire = self.anonymous_get_json(url).await?;
        Ok(response.into())
    }

    /// Fetches the channel's past broadcasts from the observed anonymous route.
    pub async fn anonymous_channel_videos(
        &self,
        slug: &str,
    ) -> Result<Vec<ChannelVideo>, KickApiError> {
        let url = channel_subresource_url(slug, "videos")?;
        let response: Vec<ChannelVideoWire> = self.anonymous_get_json(url).await?;
        Ok(response.into_iter().map(ChannelVideo::from).collect())
    }

    /// Resolves a past broadcast's HLS source from its video UUID.
    pub async fn anonymous_vod_source(&self, uuid: &str) -> Result<String, KickApiError> {
        if uuid.is_empty()
            || uuid.len() > 64
            || !uuid
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(KickApiError::InvalidQuery("video uuid"));
        }
        let url = Url::parse(&format!("https://kick.com/api/v1/video/{uuid}"))
            .map_err(|_| KickApiError::InvalidQuery("video uuid"))?;
        let value: serde_json::Value = self.anonymous_get_json(url).await?;
        value
            .get("source")
            .and_then(|value| value.as_str())
            .filter(|source| source.starts_with("https://"))
            .map(str::to_owned)
            .ok_or_else(|| KickApiError::InvalidJson)
    }

    /// Fetches one page of the channel's clips from the observed anonymous
    /// route. Pass the previous page's `next_cursor` to continue.
    pub async fn anonymous_channel_clips(
        &self,
        slug: &str,
        cursor: Option<&str>,
    ) -> Result<ChannelClipPage, KickApiError> {
        let url = channel_clips_url(slug, cursor)?;
        let response: ChannelClipsEnvelope = self.anonymous_get_json(url).await?;
        Ok(ChannelClipPage {
            clips: response
                .clips
                .into_iter()
                .map(ChannelClip::from)
                .filter(|clip| !clip.id.is_empty())
                .collect(),
            next_cursor: response
                .next_cursor
                .filter(|cursor| !cursor.is_empty() && cursor.len() <= MAX_CURSOR_BYTES),
        })
    }

    /// Fetches the channel's recently used categories.
    pub async fn anonymous_channel_recent_categories(
        &self,
        slug: &str,
    ) -> Result<Vec<ChannelCategory>, KickApiError> {
        let url = channel_subresource_url(slug, "recent-categories")?;
        let response: Vec<AnonymousCategoryWire> = self.anonymous_get_json(url).await?;
        Ok(response.into_iter().map(ChannelCategory::from).collect())
    }

    /// Fetches the channel's active follower goal, if one is configured.
    pub async fn anonymous_channel_goals(
        &self,
        slug: &str,
    ) -> Result<Option<FollowerGoal>, KickApiError> {
        let url = channel_subresource_url(slug, "goals")?;
        let response: Vec<ChannelGoalWire> = self.anonymous_get_json(url).await?;
        Ok(follower_goal_from_wire(response))
    }

    /// Resolves the web livestream UUID used by the playback-init route.
    pub async fn anonymous_channel_livestream_id(
        &self,
        slug: &str,
    ) -> Result<Option<String>, KickApiError> {
        let url = web_channel_livestream_url(slug)?;
        let response: WebLivestreamEnvelope = self.anonymous_get_json(url).await?;
        Ok(response
            .data
            .and_then(|data| data.livestream)
            .and_then(|livestream| livestream.id)
            .filter(|id| !id.trim().is_empty()))
    }

    /// Requests the DVR playback source for a live broadcast.
    ///
    /// This is the observed playback-session init route. It is a read-only
    /// session initialization, not a social write. The returned URL is kept
    /// only in [`PlaybackSource`] and is never logged.
    pub async fn anonymous_playback_dvr(
        &self,
        slug: &str,
        video_id: &str,
    ) -> Result<Option<String>, KickApiError> {
        let url = playback_init_url(video_id)?;
        let body = serde_json::json!({
            "video_player": {
                "player": {
                    "player_name": "web",
                    "player_version": "1.0.0",
                    "player_software": "IVS Player",
                    "player_software_version": "1.0.0"
                },
                "mux_sdk": {"sdk_available": false},
                "pal_sdk": {"sdk_available": false, "nonce": ""},
                "datazoom_sdk": {"sdk_available": false},
                "google_ads_sdk": {"sdk_available": false},
                "ima_sdk": {"sdk_available": false}
            },
            "video_session": {
                "page_type": "channel",
                "player_remote_played": false,
                "enable_sampling": false,
                "url_path": slug,
                "autoplay_behaviour": "auto",
                "play_muted": true,
                "viewer_connection_type": ""
            },
            "user_session": {
                "session_id": "",
                "player_device_id": "unknown",
                "browser_lang": "ja-JP"
            }
        });
        let response = self
            .http
            .post(url)
            .header(CONTENT_TYPE, "application/json")
            .header("Origin", "https://kick.com")
            .header("Referer", "https://kick.com/")
            .body(body.to_string())
            .send()
            .await
            .map_err(|error| KickApiError::Transport(classify_transport(&error)))?;
        let body = limited_body(response).await?;
        let response: PlaybackInitResponse =
            serde_json::from_slice(&body).map_err(|_| KickApiError::InvalidJson)?;
        Ok(response
            .playback_url
            .and_then(|playback| playback.dvr.or(playback.vod))
            .and_then(|url| playback_source(Some(url)))
            .map(|source| source.url))
    }

    /// Fetches one observed anonymous HTTP recent-history snapshot by canonical numeric ID.
    ///
    /// The sampled `limit=1` request returned 25 records, so this method does
    /// not claim that the query controls page size. It does not open a socket
    /// or follow the response cursor.
    pub async fn anonymous_recent_messages(
        &self,
        channel_id: u64,
    ) -> Result<AnonymousRecentMessages, KickApiError> {
        let url = recent_messages_url(channel_id)?;
        let response: AnonymousRecentMessagesEnvelope = self.anonymous_get_json(url).await?;
        Ok(response.into())
    }

    /// Fetches one anonymous channel emote catalogue from the observed
    /// `GET /emotes/{slug}` route. The decoder keeps only the group emote
    /// records and discards unrelated profile and playback metadata. This
    /// response does not provide an emote image URL.
    pub async fn anonymous_channel_emotes(
        &self,
        slug: &str,
    ) -> Result<Vec<emotes::ChannelEmoteGroup>, KickApiError> {
        let url = channel_emotes_url(slug)?;
        let body = self.anonymous_get_bytes(url).await?;
        emotes::decode_channel_emote_catalog(&body).map_err(|_| KickApiError::InvalidJson)
    }

    /// Fetches one page of followed channels from the observed authenticated
    /// `GET /api/v2/channels/followed` route.
    ///
    /// The session Cookie is required and is never attached to anonymous
    /// requests. The caller supplies the opaque cursor returned by a previous
    /// page; values are bounded before they are added to the query string.
    pub async fn followed_channels(
        &self,
        cursor: Option<&str>,
    ) -> Result<FollowedChannelsPage, KickApiError> {
        let body = self.followed_channels_body(cursor).await?;
        decode_followed_channels(&body)
    }

    /// Follows or unfollows a channel using the authenticated website route.
    pub async fn set_channel_follow(&self, slug: &str, follow: bool) -> Result<(), KickApiError> {
        let cookie = self
            .session_cookie
            .as_ref()
            .ok_or(KickApiError::SessionRequired)?;
        let mut url = channel_lookup_url(slug)?;
        url.path_segments_mut()
            .map_err(|_| KickApiError::InvalidChannelSlug)?
            .push("follow");
        let method = if follow {
            reqwest::Method::POST
        } else {
            reqwest::Method::DELETE
        };
        let request = cookie
            .apply_auth_headers(self.http.request(method, url))
            .map_err(|_| KickApiError::SessionInvalid)?;
        let response = request
            .send()
            .await
            .map_err(|error| KickApiError::Transport(classify_transport(&error)))?;
        if !response.status().is_success() {
            return Err(KickApiError::HttpStatus(response.status().as_u16()));
        }
        Ok(())
    }

    async fn followed_channels_body(&self, cursor: Option<&str>) -> Result<Vec<u8>, KickApiError> {
        let cookie = self
            .session_cookie
            .as_ref()
            .ok_or(KickApiError::SessionRequired)?;
        let mut url = Url::parse(&format!("{CHANNEL_LOOKUP_BASE}/followed"))
            .map_err(|_| KickApiError::InvalidQuery("followed endpoint URL is invalid"))?;
        if let Some(cursor) = cursor {
            if cursor.is_empty() || cursor.len() > MAX_CURSOR_BYTES {
                return Err(KickApiError::InvalidQuery("invalid followed cursor"));
            }
            url.query_pairs_mut().append_pair("cursor", cursor);
        }
        let request = cookie
            .apply_auth_headers(self.http.get(url))
            .map_err(|_| KickApiError::SessionInvalid)?;
        let response = request
            .send()
            .await
            .map_err(|error| KickApiError::Transport(classify_transport(&error)))?;
        limited_body(response).await
    }

    fn anonymous_request(&self, url: Url) -> RequestBuilder {
        self.http.get(url)
    }

    /// Requests the observed mobile realtime configuration without account credentials.
    /// The caller supplies its own installation/client ID; this never reads a device ID.
    pub async fn anonymous_realtime_config(
        &self,
        client_id: &str,
    ) -> Result<RealtimeConfiguration, KickApiError> {
        if client_id.is_empty()
            || client_id.len() > 128
            || !client_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(KickApiError::InvalidQuery("invalid realtime client ID"));
        }
        let body = serde_json::json!({"client":{"id":client_id,"type":"android-streaming"},"capabilities":{"accepted_providers":[{"provider":"pusher"},{"provider":"centrifugo"}]}});
        let response = self
            .http
            .post("https://mobile.kick.com/api/v1/realtime/connection")
            .header("Content-Type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| KickApiError::Transport(classify_transport(&e)))?;
        let body = limited_body(response).await?;
        let envelope: RealtimeEnvelope =
            serde_json::from_slice(&body).map_err(|_| KickApiError::InvalidJson)?;
        Ok(envelope.data)
    }

    pub async fn anonymous_chat_realtime_config(
        &self,
        client_id: &str,
        channel_id: &str,
    ) -> Result<RealtimeConfiguration, KickApiError> {
        if channel_id.is_empty()
            || channel_id.len() > 256
            || channel_id.chars().any(char::is_control)
            || matches!(channel_id, "." | "..")
        {
            return Err(KickApiError::InvalidChannelId);
        }
        let mut endpoint = Url::parse("https://mobile.kick.com/api/v1/realtime/")
            .map_err(|_| KickApiError::InvalidChannelId)?;
        endpoint
            .path_segments_mut()
            .map_err(|_| KickApiError::InvalidChannelId)?
            .pop_if_empty()
            .push(channel_id)
            .push("chat")
            .push("connection");
        if client_id.is_empty()
            || client_id.len() > 128
            || !client_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(KickApiError::InvalidQuery("invalid realtime client ID"));
        }
        let body = serde_json::json!({"client":{"id":client_id,"type":"android-streaming"},"capabilities":{"accepted_providers":[{"provider":"pusher"},{"provider":"centrifugo"}]}});
        let response = self
            .http
            .post(endpoint)
            .header("Content-Type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| KickApiError::Transport(classify_transport(&e)))?;
        let body = limited_body(response).await?;
        let envelope: RealtimeEnvelope =
            serde_json::from_slice(&body).map_err(|_| KickApiError::InvalidJson)?;
        Ok(envelope.data)
    }

    async fn anonymous_get_json<T: DeserializeOwned>(&self, url: Url) -> Result<T, KickApiError> {
        let body = self.anonymous_get_bytes(url).await?;
        serde_json::from_slice(&body).map_err(|_| KickApiError::InvalidJson)
    }

    async fn anonymous_get_bytes(&self, url: Url) -> Result<Vec<u8>, KickApiError> {
        let response = self
            .anonymous_request(url)
            .send()
            .await
            .map_err(|error| KickApiError::Transport(classify_transport(&error)))?;
        limited_body(response).await
    }

    /// Requests one page of public active livestreams.
    ///
    /// Dropping this future cancels the in-flight request. The response is
    /// capped at 8 MiB before JSON decoding; HTTP error bodies are not retained.
    pub async fn live_streams(
        &self,
        query: &LiveStreamsQuery,
    ) -> Result<LiveStreamsPage, KickApiError> {
        let parameters = query_parameters(query)?;

        let mut request = self
            .http
            .get(format!("{API_BASE}{LIVE_STREAMS_PATH}"))
            .query(&parameters);
        if let Some(token) = self.access_token.as_ref() {
            request = request.header(AUTHORIZATION, token.authorization_header()?);
        }

        let response = request
            .send()
            .await
            .map_err(|error| KickApiError::Transport(classify_transport(&error)))?;
        let body = limited_body(response).await?;
        let response: ApiPage<LiveStream> =
            serde_json::from_slice(&body).map_err(|_| KickApiError::InvalidJson)?;

        Ok(LiveStreamsPage {
            items: response.data,
            next_cursor: if response.pagination.next_cursor.is_empty() {
                None
            } else {
                Some(response.pagination.next_cursor)
            },
            message: response.message,
        })
    }
}

fn decode_followed_channels(body: &[u8]) -> Result<FollowedChannelsPage, KickApiError> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| KickApiError::InvalidJson)?;
    if !value.is_object() {
        return Err(KickApiError::InvalidJson);
    }
    let wire: FollowedChannelsWire =
        serde_json::from_value(value).map_err(|_| KickApiError::InvalidJson)?;
    let channels = wire
        .channels
        .into_iter()
        .filter_map(|channel| {
            let channel_slug = channel.channel_slug.filter(|slug| !slug.is_empty())?;
            let channel_name = channel
                .user_username
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| channel_slug.clone());
            Some(FollowedChannel {
                channel_slug,
                channel_name,
                is_live: channel.is_live.unwrap_or(false),
                category_name: channel.category_name,
                session_title: channel.session_title,
                viewer_count: channel.viewer_count,
                profile_picture_url: channel.profile_picture,
            })
        })
        .collect();
    Ok(FollowedChannelsPage {
        channels,
        next_cursor: wire.next_cursor,
    })
}

fn query_parameters(query: &LiveStreamsQuery) -> Result<Vec<(String, String)>, KickApiError> {
    if query.limit == 0 {
        return Err(KickApiError::InvalidQuery("limit must be at least 1"));
    }
    if query.category_ids.len() > MAX_FILTER_VALUES {
        return Err(KickApiError::InvalidQuery(
            "at most 25 category IDs are supported",
        ));
    }
    if query.language_codes.len() > MAX_FILTER_VALUES {
        return Err(KickApiError::InvalidQuery(
            "at most 25 language codes are supported",
        ));
    }
    if query
        .cursor
        .as_ref()
        .is_some_and(|cursor| cursor.len() > MAX_CURSOR_BYTES)
    {
        return Err(KickApiError::InvalidQuery(
            "cursor exceeds the local size limit",
        ));
    }

    let limit = query.limit.min(MAX_PAGE_SIZE);
    let mut parameters = vec![("limit".to_owned(), limit.to_string())];
    if let Some(cursor) = query.cursor.as_deref().filter(|cursor| !cursor.is_empty()) {
        parameters.push(("cursor".to_owned(), cursor.to_owned()));
    }
    parameters.extend(
        query
            .category_ids
            .iter()
            .map(|category_id| ("category_id".to_owned(), category_id.to_string())),
    );
    parameters.extend(
        query
            .language_codes
            .iter()
            .map(|language| ("language_code".to_owned(), language.clone())),
    );
    Ok(parameters)
}

impl fmt::Debug for KickClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KickClient")
            .field("authenticated", &self.access_token.is_some())
            .finish_non_exhaustive()
    }
}

async fn limited_body(response: Response) -> Result<Vec<u8>, KickApiError> {
    let status = response.status();
    if !status.is_success() {
        return Err(KickApiError::HttpStatus(status.as_u16()));
    }

    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(KickApiError::ResponseTooLarge);
    }

    let initial_capacity = response
        .content_length()
        .unwrap_or(8 * 1024)
        .min(MAX_RESPONSE_BYTES as u64) as usize;
    let mut body = Vec::with_capacity(initial_capacity);
    let mut response = response;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| KickApiError::Transport(classify_transport(&error)))?
    {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(KickApiError::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn classify_transport(error: &reqwest::Error) -> TransportFailure {
    if error.is_timeout() {
        TransportFailure::Timeout
    } else if error.is_connect() {
        TransportFailure::Connection
    } else if error.is_body() {
        TransportFailure::ResponseBody
    } else if error.is_request() || error.is_builder() {
        TransportFailure::Request
    } else {
        TransportFailure::Other
    }
}

#[derive(Deserialize)]
struct ApiPage<T> {
    data: Vec<T>,
    message: String,
    pagination: ApiPagination,
}

#[derive(Deserialize)]
struct ApiPagination {
    next_cursor: String,
}

fn chat_send_url(chatroom_id: u64) -> Result<Url, KickApiError> {
    if chatroom_id == 0 {
        return Err(KickApiError::InvalidChannelId);
    }
    Url::parse(&format!("{CHAT_SEND_BASE}/{chatroom_id}"))
        .map_err(|_| KickApiError::InvalidChannelId)
}

/// Last server-provided chat rejection text, kept out of `KickApiError` so the
/// error enum stays `Copy`.
static CHAT_SEND_REJECTION: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Takes the last rejection text for display. The value is cleared on read.
pub fn take_chat_send_rejection() -> Option<String> {
    CHAT_SEND_REJECTION
        .lock()
        .ok()
        .and_then(|mut slot| slot.take())
}

/// Known chat-send rejection codes observed in the public web client.
const CHAT_SEND_ERROR_CODES: [&str; 14] = [
    "ACCOUNT_AGE_MODE_ERROR",
    "BANNED_ERROR",
    "EMOTES_ONLY_ERROR",
    "FOLLOWERS_ONLY_ERROR",
    "FOREIGN_CHANNEL_EMOTE_ERROR",
    "INVALID_EMOTE_ERROR",
    "MAX_SPECIAL_CHARS_ERROR",
    "MESSAGE_CONTENT_LENGTH_ERROR",
    "NO_LINKS_ERROR",
    "ORIGINAL_MESSAGE_NOT_FOUND_ERROR",
    "RATE_LIMIT_ERROR",
    "SLOW_MODE_ERROR",
    "SUBSCRIBERS_ONLY_EMOTE_ERROR",
    "SUBSCRIBERS_ONLY_ERROR",
];

/// Reads a bounded rejection body and returns the known error code it carries.
async fn chat_send_error_code(response: Response) -> &'static str {
    const MAX_DETAIL_BYTES: usize = 1024;
    let Ok(bytes) = response.bytes().await else {
        return "";
    };
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_DETAIL_BYTES)]).into_owned();
    let mut detail = text.trim().chars().take(160).collect::<String>();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text.trim()) {
        for pointer in [
            "/data/status/message",
            "/data/data/status/message",
            "/status/message",
            "/message",
            "/error",
        ] {
            if let Some(message) = value.pointer(pointer).and_then(|item| item.as_str()) {
                detail = message.chars().take(160).collect();
                break;
            }
        }
    }
    if let Ok(mut slot) = CHAT_SEND_REJECTION.lock() {
        *slot = Some(detail);
    }
    CHAT_SEND_ERROR_CODES
        .iter()
        .find(|code| text.contains(**code))
        .copied()
        .unwrap_or("")
}

fn chat_send_payload(content: &str) -> serde_json::Value {
    serde_json::json!({
        "content": content,
        "type": "message",
        "message_ref": chat_message_ref(),
    })
}

fn chat_message_ref() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().to_string())
        .unwrap_or_else(|_| "0".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emote_catalog_url_uses_the_observed_route_and_rejects_path_injection() {
        assert_eq!(
            channel_emotes_url("xqc").unwrap().as_str(),
            "https://kick.com/emotes/xqc"
        );
        assert_eq!(
            channel_emotes_url("../other").unwrap_err(),
            KickApiError::InvalidChannelSlug
        );
        assert_eq!(
            channel_emotes_url("bad/slug").unwrap_err(),
            KickApiError::InvalidChannelSlug
        );
    }

    #[test]
    fn channel_subresource_urls_use_the_observed_routes() {
        assert_eq!(
            channel_subresource_url("zokkunzokkun", "videos")
                .unwrap()
                .as_str(),
            "https://kick.com/api/v2/channels/zokkunzokkun/videos"
        );
        assert_eq!(
            channel_subresource_url("zokkunzokkun", "clips")
                .unwrap()
                .as_str(),
            "https://kick.com/api/v2/channels/zokkunzokkun/clips"
        );
        assert_eq!(
            channel_subresource_url("zokkunzokkun", "recent-categories")
                .unwrap()
                .as_str(),
            "https://kick.com/api/v2/channels/zokkunzokkun/recent-categories"
        );
        assert_eq!(
            channel_subresource_url("zokkunzokkun", "goals")
                .unwrap()
                .as_str(),
            "https://kick.com/api/v2/channels/zokkunzokkun/goals"
        );
        assert_eq!(
            channel_subresource_url("../other", "videos").unwrap_err(),
            KickApiError::InvalidChannelSlug
        );
    }

    #[test]
    fn web_playback_urls_use_the_uuid_route() {
        assert_eq!(
            web_channel_livestream_url("zokkunzokkun").unwrap().as_str(),
            "https://web.kick.com/api/v1/channels/zokkunzokkun/livestream"
        );
        assert_eq!(
            playback_init_url("01a1d939-27a0-7609-a716-6e449486b340")
                .unwrap()
                .as_str(),
            "https://web.kick.com/api/v1/stream/01a1d939-27a0-7609-a716-6e449486b340/playback"
        );
        assert_eq!(
            playback_init_url("../other").unwrap_err(),
            KickApiError::InvalidQuery("invalid playback video ID")
        );
    }

    #[test]
    fn video_duration_is_converted_from_milliseconds() {
        let wire: ChannelVideoWire = serde_json::from_str(
            r#"{"id":1,"session_title":"Test","duration":170907000,"views":10,"is_live":false,"thumbnail":{"src":"https://example.invalid/t.jpg"},"categories":[{"name":"IRL"}]}"#,
        )
        .unwrap();
        let video = ChannelVideo::from(wire);
        assert_eq!(video.duration_seconds, 170907);
        assert_eq!(video.category.as_deref(), Some("IRL"));
        assert_eq!(
            video.thumbnail_url.as_deref(),
            Some("https://example.invalid/t.jpg")
        );
    }

    #[test]
    fn follower_goal_uses_only_active_followers_goals() {
        let goals: Vec<ChannelGoalWire> = serde_json::from_str(
            r#"[{"type":"followers","status":"ended","target_value":100,"current_value":100},{"type":"subscribers","status":"active","target_value":50,"current_value":10},{"type":"followers","status":"active","target_value":20000,"current_value":18278,"progress_bar_emoji_id":"emotes/5804796"}]"#,
        )
        .unwrap();
        let goal = follower_goal_from_wire(goals).unwrap();
        assert_eq!(goal.target, 20000);
        assert_eq!(goal.current, 18278);
        assert_eq!(goal.emoji_id.as_deref(), Some("emotes/5804796"));
    }

    #[test]
    fn clips_url_encodes_the_opaque_cursor_as_one_query_value() {
        assert_eq!(
            channel_clips_url("zokkunzokkun", None).unwrap().as_str(),
            "https://kick.com/api/v2/channels/zokkunzokkun/clips"
        );
        let url = channel_clips_url(
            "zokkunzokkun",
            Some(r#"{"view":0,"id":"clip_01M7ET8AX01S1J22X6PS57YKD2"}"#),
        )
        .unwrap();
        let query = url.query().unwrap_or_default();
        assert!(query.starts_with("cursor=%7B%22view%22%3A0%2C%22id%22%3A%22clip_"));
        assert!(!query.contains('&'));
        assert_eq!(
            channel_clips_url("zokkunzokkun", Some("")).unwrap_err(),
            KickApiError::InvalidQuery("invalid clips cursor")
        );
    }

    #[test]
    fn clips_envelope_keeps_the_next_cursor() {
        let envelope: ChannelClipsEnvelope = serde_json::from_str(
            r#"{"clips":[{"id":"clip_1","title":"clip"}],"nextCursor":"cursor-1"}"#,
        )
        .unwrap();
        assert_eq!(envelope.clips.len(), 1);
        assert_eq!(envelope.next_cursor.as_deref(), Some("cursor-1"));

        let envelope: ChannelClipsEnvelope = serde_json::from_str(r#"{"clips":[]}"#).unwrap();
        assert!(envelope.next_cursor.is_none());
    }

    #[test]
    fn clip_and_category_wire_values_are_bounded() {
        let clip_wire: ChannelClipWire = serde_json::from_str(
            r#"{"id":"clip_1","title":"clip","duration":15,"view_count":8,"thumbnail_url":"https://example.invalid/c.webp"}"#,
        )
        .unwrap();
        let clip = ChannelClip::from(clip_wire);
        assert_eq!(clip.duration_seconds, 15);
        assert_eq!(clip.views, Some(8));
        let category_wire: AnonymousCategoryWire = serde_json::from_str(
            r#"{"id":8549,"name":"IRL","slug":"irl","viewers":93271,"tags":["Adventure"],"banner":{"src":"https://example.invalid/b.webp"}}"#,
        )
        .unwrap();
        let category = ChannelCategory::from(category_wire);
        assert_eq!(category.name, "IRL");
        assert_eq!(category.viewers, Some(93271));
        assert_eq!(category.tags, ["Adventure"]);
    }

    #[test]
    fn query_rejects_zero_limit_and_clamps_values_above_the_api_maximum() {
        let error = query_parameters(&LiveStreamsQuery {
            limit: 0,
            ..LiveStreamsQuery::default()
        })
        .unwrap_err();
        assert_eq!(
            error,
            KickApiError::InvalidQuery("limit must be at least 1")
        );

        let parameters = query_parameters(&LiveStreamsQuery {
            limit: MAX_PAGE_SIZE + 1,
            ..LiveStreamsQuery::default()
        })
        .unwrap();
        assert_eq!(parameters, [("limit".into(), MAX_PAGE_SIZE.to_string())]);
    }

    #[test]
    fn query_uses_the_documented_multi_value_parameter_names() {
        let query = LiveStreamsQuery {
            limit: 20,
            cursor: Some("cursor +/next".into()),
            category_ids: vec![42, 84],
            language_codes: vec!["en-US".into(), "ja".into()],
        };
        let parameters = query_parameters(&query).unwrap();
        let request = Client::new()
            .get(format!("{API_BASE}{LIVE_STREAMS_PATH}"))
            .query(&parameters)
            .build()
            .unwrap();
        let actual = request
            .url()
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect::<Vec<_>>();

        assert_eq!(
            actual,
            [
                ("limit".into(), "20".into()),
                ("cursor".into(), "cursor +/next".into()),
                ("category_id".into(), "42".into()),
                ("category_id".into(), "84".into()),
                ("language_code".into(), "en-US".into()),
                ("language_code".into(), "ja".into()),
            ]
        );
    }

    #[test]
    fn access_token_debug_is_redacted_and_authorization_header_is_sensitive() {
        let token = AccessToken::new("sensitive-test-token").unwrap();
        let debug = format!("{token:?}");
        let header = token.authorization_header().unwrap();

        assert_eq!(debug, "AccessToken(<redacted>)");
        assert!(header.is_sensitive());
    }

    #[test]
    fn documented_livestream_page_shape_deserializes_and_maps_to_summary() {
        let payload = r#"
        {
          "data": [{
            "broadcaster_user": {
              "id": 123,
              "profile_picture": "https://example.invalid/profile.jpg",
              "username": "example_channel"
            },
            "category": {
              "id": 456,
              "name": "Example category",
              "thumbnail": "https://example.invalid/category.jpg"
            },
            "channel": { "slug": "example-channel" },
            "has_mature_content": false,
            "id": "123e4567-e89b-12d3-a456-426614174000",
            "language_code": "en",
            "started_at": "2026-11-12T00:00:00Z",
            "tags": ["example"],
            "thumbnail": "https://example.invalid/stream.jpg",
            "title": "Example stream",
            "viewer_count": 17
          }],
          "message": "",
          "pagination": { "next_cursor": "next-page" }
        }
        "#;

        let response: ApiPage<LiveStream> = serde_json::from_str(payload).unwrap();
        let stream = response.data.into_iter().next().unwrap();
        let summary = stream.to_summary();

        assert_eq!(summary.channel_slug, "example-channel");
        assert_eq!(summary.channel_name, "example_channel");
        assert_eq!(summary.title, "Example stream");
        assert_eq!(summary.category.as_deref(), Some("Example category"));
        assert_eq!(summary.viewer_count, Some(17));
        assert_eq!(
            summary.thumbnail_url.as_deref(),
            Some("https://example.invalid/stream.jpg")
        );
    }

    #[test]
    fn anonymous_catalog_keeps_only_display_fields_and_redacts_runtime_urls() {
        let fixture = r#"
        {
          "data": {
            "livestreams": [{
              "id": "fixture-stream-id",
              "streamer": {
                "user": { "username": "fixture_user" },
                "channel": { "slug": "fixture-channel" }
              },
              "metadata": {
                "category": { "name": "Fixture category" },
                "has_mature_content": false,
                "language": "en",
                "title": "Fixture title"
              },
              "viewers_count": 14,
              "thumbnail_url": "https://example.invalid/thumbnail.jpg",
              "playback_url": "https://cdn.example.invalid/live.m3u8?token=fixture-secret"
            }],
            "next_cursor": "fixture-next-cursor",
            "version": "fixture-version"
          },
          "status": { "code": 200, "error": false, "message": "ok" }
        }
        "#;

        let wire: AnonymousCatalogEnvelope = serde_json::from_str(fixture).unwrap();
        let page = AnonymousCatalogPage::from(wire);
        let stream = page.items.first().unwrap();
        let summary = stream.to_summary().unwrap();
        let debug = format!("{page:?}");

        assert_eq!(page.items.len(), 1);
        assert!(page.next_cursor.is_some());
        assert_eq!(stream.channel_slug.as_deref(), Some("fixture-channel"));
        assert_eq!(stream.channel_name.as_deref(), Some("fixture_user"));
        assert_eq!(summary.title, "Fixture title");
        assert_eq!(summary.category.as_deref(), Some("Fixture category"));
        assert_eq!(summary.viewer_count, Some(14));
        assert!(stream.playback_source.is_some());
        assert!(!debug.contains("fixture-secret"));
        assert!(!debug.contains("live.m3u8"));
    }

    #[test]
    fn anonymous_channel_supports_offline_and_live_shapes_without_retaining_url_text() {
        let offline_fixture = r#"
        {
          "id": 12345,
          "slug": "fixture-channel",
          "followers_count": "123",
          "user": {
            "username": "fixture_user",
            "profile_picture": "https://example.invalid/profile.jpg"
          },
          "livestream": null,
          "playback_url": "https://cdn.example.invalid/stale.m3u8?token=offline-fixture"
        }
        "#;
        let offline: AnonymousChannelSnapshot =
            serde_json::from_str::<AnonymousChannelWire>(offline_fixture)
                .unwrap()
                .into();
        assert!(!offline.is_live());
        assert_eq!(offline.id, Some(12345));
        assert!(offline.livestream.is_none());
        assert!(offline.playback_source.is_none());
        assert!(!format!("{offline:?}").contains("offline-fixture"));

        let live_fixture = r#"
        {
          "slug": "fixture-channel",
          "followers_count": "123",
          "user": { "username": "fixture_user" },
          "livestream": {
            "is_live": true,
            "session_title": "Fixture live title",
            "viewer_count": 31,
            "start_time": "2026-11-12T00:00:00Z",
            "playback_url": "https://cdn.example.invalid/ignored.m3u8?token=nested-secret"
          },
          "playback_url": "https://cdn.example.invalid/live.m3u8?token=fixture-secret"
        }
        "#;
        let live: AnonymousChannelSnapshot =
            serde_json::from_str::<AnonymousChannelWire>(live_fixture)
                .unwrap()
                .into();
        let debug = format!("{live:?}");

        assert!(live.is_live());
        assert_eq!(live.username.as_deref(), Some("fixture_user"));
        assert_eq!(
            live.livestream
                .as_ref()
                .and_then(|stream| stream.title.as_deref()),
            Some("Fixture live title")
        );
        assert_eq!(
            live.livestream
                .as_ref()
                .and_then(|stream| stream.viewer_count),
            Some(31)
        );
        assert!(!debug.contains("fixture-secret"));
        assert!(!debug.contains("nested-secret"));
        assert!(!debug.contains("live.m3u8"));
    }

    #[test]
    fn followed_channels_decode_observed_fields_and_redact_profile_urls() {
        let fixture = br#"{
          "channels": [
            {
              "channel_slug": "fixture-live",
              "user_username": "FixtureLive",
              "is_live": true,
              "category_name": "Just Chatting",
              "session_title": "Fixture title",
              "viewer_count": 42,
              "profile_picture": "https://example.invalid/profile-live.jpg"
            },
            {
              "channel_slug": "fixture-offline",
              "user_username": "",
              "is_live": false,
              "category_name": null,
              "session_title": null,
              "viewer_count": null,
              "profile_picture": null
            },
            {
              "channel_slug": "",
              "user_username": "ignored"
            }
          ],
          "nextCursor": "fixture-cursor"
        }"#;

        let page = decode_followed_channels(fixture).unwrap();
        assert_eq!(page.channels.len(), 2);
        assert_eq!(page.next_cursor.as_deref(), Some("fixture-cursor"));
        assert_eq!(page.channels[0].channel_slug, "fixture-live");
        assert_eq!(page.channels[0].channel_name, "FixtureLive");
        assert!(page.channels[0].is_live);
        assert_eq!(
            page.channels[0].category_name.as_deref(),
            Some("Just Chatting")
        );
        assert_eq!(page.channels[0].viewer_count, Some(42));
        assert_eq!(page.channels[1].channel_slug, "fixture-offline");
        assert_eq!(page.channels[1].channel_name, "fixture-offline");
        assert!(!page.channels[1].is_live);

        let debug = format!("{page:?}");
        assert!(!debug.contains("profile-live.jpg"));
        assert!(!debug.contains("example.invalid"));
    }

    #[test]
    fn followed_channels_reject_malformed_payloads() {
        assert!(decode_followed_channels(br#"{}"#).is_ok());
        assert!(decode_followed_channels(br#"[]"#).is_err());
        assert!(decode_followed_channels(br#"{"channels": "not-an-array"}"#).is_err());
    }

    #[test]
    fn anonymous_requests_never_forward_a_stored_bearer_token() {
        let client = KickClient::new(AccessToken::new("test-only-token").unwrap()).unwrap();
        let url = Url::parse("https://api.kick.com/private/v1/livestreams?limit=1").unwrap();
        let request = client.anonymous_request(url).build().unwrap();

        assert!(!request.headers().contains_key(AUTHORIZATION));
    }

    #[test]
    fn anonymous_recent_messages_uses_canonical_numeric_id_and_redacts_content() {
        let url = recent_messages_url(12345).unwrap();
        assert_eq!(url.path(), "/api/v2/channels/12345/messages");
        assert_eq!(url.query(), Some("limit=1"));
        assert_eq!(recent_messages_url(0), Err(KickApiError::InvalidChannelId));

        let fixture = r#"
        {
          "data": {
            "cursor": "fixture-cursor",
            "messages": [{
              "chat_id": 12345,
              "content": "private message fixture",
              "created_at": "2026-11-12T00:00:00Z",
              "id": "fixture-message-id",
              "metadata": "{}",
              "sender": {
                "id": 321,
                "identity": {},
                "slug": "fixture-user",
                "username": "fixture-user"
              },
              "type": "message",
              "user_id": 321
            }, {
              "content": "",
              "sender": { "username": "fixture-user" }
            }, {
              "content": "body-only fixture",
              "sender": { "username": " " }
            }],
            "pinned_message": null
          },
          "status": { "code": 200, "error": false, "message": "ok" }
        }
        "#;
        let wire: AnonymousRecentMessagesEnvelope = serde_json::from_str(fixture).unwrap();
        let page = AnonymousRecentMessages::from(wire);
        let message = page.messages.first().unwrap();
        let debug = format!("{page:?} {:?}", message);

        assert_eq!(page.messages.len(), 3);
        assert_eq!(page.count_with_nonempty_sender_and_content(), 1);
        assert_eq!(message.content.as_deref(), Some("private message fixture"));
        assert_eq!(message.username.as_deref(), Some("fixture-user"));
        assert!(!debug.contains("private message fixture"));
        assert!(!debug.contains("fixture-user"));
    }

    #[test]
    fn channel_slug_is_encoded_as_one_path_segment_and_rejects_path_separators() {
        let url = channel_lookup_url("name?query#fragment").unwrap();
        assert_eq!(url.path(), "/api/v2/channels/name%3Fquery%23fragment");
        assert_eq!(
            channel_lookup_url("../chatroom"),
            Err(KickApiError::InvalidChannelSlug)
        );
        assert_eq!(
            channel_lookup_url(""),
            Err(KickApiError::InvalidChannelSlug)
        );
    }

    #[test]
    fn playback_sources_require_https_without_url_credentials_and_redact_debug() {
        let source = playback_source(Some(
            "https://cdn.example.invalid/live.m3u8?token=fixture-secret".into(),
        ))
        .unwrap();
        let debug = format!("{source:?}");

        assert!(playback_source(Some("http://cdn.example.invalid/live.m3u8".into())).is_none());
        assert!(
            playback_source(Some(
                "https://user:password@cdn.example.invalid/live.m3u8".into()
            ))
            .is_none()
        );
        assert!(!debug.contains("fixture-secret"));
        assert!(!debug.contains("live.m3u8"));
    }
}

#[cfg(test)]
mod realtime_tests {
    #[test]
    fn catalog_preserves_internal_identifier_separately_from_slug() {
        let wire: super::AnonymousCatalogStreamWire =
            serde_json::from_str(r#"{"streamer":{"channel":{"id":"opaque/id","slug":"sample"}}}"#)
                .unwrap();
        let stream: super::AnonymousCatalogStream = wire.into();
        assert_eq!(stream.channel_id.as_deref(), Some("opaque/id"));
        assert_eq!(stream.channel_slug.as_deref(), Some("sample"));
        assert!(!format!("{stream:?}").contains("opaque/id"));
    }

    #[test]
    fn configuration_decode_and_debug_do_not_expose_access_url() {
        let config: super::RealtimeConfiguration = serde_json::from_str(r#"{"connections":[{"provider":"centrifugo","credentials":{"url":"wss://example.test/ws?token=fixture-secret"}}]}"#).unwrap();
        assert_eq!(config.connections[0].provider, "centrifugo");
        assert!(!format!("{config:?}").contains("fixture-secret"));
        assert!(!format!("{config:?}").contains("example.test"));
    }
}

#[cfg(test)]
mod chat_send_tests {
    use super::*;

    #[test]
    fn chat_send_url_is_fixed_to_the_observed_route() {
        let url = chat_send_url(123).expect("valid chatroom id");
        assert_eq!(url.as_str(), "https://kick.com/api/v2/messages/send/123");
        assert_eq!(chat_send_url(0), Err(KickApiError::InvalidChannelId));
    }

    #[test]
    fn chat_send_payload_has_the_observed_shape() {
        let payload = chat_send_payload("hello :KEKW:");
        assert_eq!(payload["content"], "hello :KEKW:");
        assert_eq!(payload["type"], "message");
        assert!(
            payload["message_ref"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
        );
    }
}
