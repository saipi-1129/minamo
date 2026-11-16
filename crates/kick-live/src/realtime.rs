//! Bounded Centrifugo connection and chat subscription.
//!
//! Publication bodies and connection URLs are never formatted or logged.

use crate::{KickApiError, KickClient, classify_transport, limited_body};
use futures_util::{SinkExt, StreamExt};
use kick_core::{
    ChatBadge, ChatMessage, ChatReward, MAX_CHAT_AUTHOR_BYTES, MAX_CHAT_ID_BYTES,
    MAX_CHAT_TEXT_BYTES,
};
use serde_json::{Value, json};
use std::{
    collections::{HashSet, VecDeque},
    fmt, future,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{mpsc, watch},
    task::JoinHandle,
    time::{self, Instant, MissedTickBehavior},
};
use tokio_tungstenite::{
    WebSocketStream, connect_async_with_config,
    tungstenite::{Message, protocol::WebSocketConfig},
};
use zeroize::{Zeroize, Zeroizing};

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

const EVENT_CAPACITY: usize = 128;
const MAX_WIRE_FRAME_BYTES: usize = 1024 * 1024;
const MAX_DEDUPLICATED_IDS: usize = 2_048;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const ACK_TIMEOUT: Duration = Duration::from_secs(15);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(25);
const PONG_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(60);
const CHAT_MESSAGE_EVENT: &str = r"App\Events\ChatMessageEvent";
const STREAM_HOSTED_EVENT: &str = r"App\Events\StreamHostedEvent";
const MOVE_TO_SUPPORTED_CHANNEL_EVENT: &str = r"App\Events\ChatMoveToSupportedChannelEvent";
const REWARD_REDEEMED_EVENT: &str = "RewardRedeemedEvent";
const STREAMER_IS_LIVE_EVENT: &str = r"App\Events\StreamerIsLive";
const STOP_STREAM_BROADCAST_EVENT: &str = r"App\Events\StopStreamBroadcast";

/// A parsed chat publication for one room. `message.text` contains the body.
///
/// This type intentionally has no `Debug` implementation so message text and
/// usernames are not accidentally written to diagnostic logs.
pub enum ChatEvent {
    Message(ChatMessage),
    Raid(RaidEvent),
    MoveToChannel(MoveToChannelEvent),
}

/// A stream lifecycle push from the channel-level `channel.{id}` subscription.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamNotificationEvent {
    StreamerIsLive { slug: String, channel_id: u64 },
    StreamEnded { slug: String, channel_id: u64 },
}

/// A `StreamHostedEvent` from the chatroom's v1 channel.
///
/// The event is emitted when a channel hosts/raids another channel. `target`
/// is the hosted channel slug when the payload includes it; it is `None` for
/// informational host banners that do not identify a redirect target.
pub struct RaidEvent {
    pub chatroom_id: u64,
    pub hoster: String,
    pub hoster_id: Option<u64>,
    pub target: Option<String>,
    pub viewer_count: u64,
}

/// A `ChatMoveToSupportedChannelEvent` from the channel-level subscription.
/// This is the event the official web client uses to move viewers to the
/// hosted/raid target; `StreamHostedEvent` is only an informational banner.
pub struct MoveToChannelEvent {
    pub chatroom_id: u64,
    pub target_slug: String,
    pub target_username: Option<String>,
    pub target_viewers: u64,
}

/// Latest state of a [`ChatSession`]. Reconnect attempts use a capped delay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatStatus {
    Connecting { attempt: u32 },
    Connected,
    Reconnecting { attempt: u32 },
    Stopped,
}

/// Synchronous validation failures when starting a chat worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatStartError {
    InvalidClientId,
    InvalidChatroomId,
    AlreadyRunning,
    RuntimeUnavailable,
}

impl fmt::Display for ChatStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidClientId => "invalid realtime client ID",
            Self::InvalidChatroomId => "invalid chatroom ID",
            Self::AlreadyRunning => "a chat session is already active for this room",
            Self::RuntimeUnavailable => "a Tokio runtime is required to start chat",
        })
    }
}

impl std::error::Error for ChatStartError {}

/// A live chat task with bounded delivery and explicit cancellation.
pub struct ChatSession {
    events: mpsc::Receiver<ChatEvent>,
    status: watch::Receiver<ChatStatus>,
    cancel: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}

impl ChatSession {
    /// Waits for the next chat message. Returns `None` after shutdown.
    pub async fn recv(&mut self) -> Option<ChatEvent> {
        self.events.recv().await
    }

    /// Returns the most recently published connection state.
    pub fn status(&self) -> ChatStatus {
        *self.status.borrow()
    }

    /// Clones a watch receiver for selecting on state changes in a worker.
    pub fn status_receiver(&self) -> watch::Receiver<ChatStatus> {
        self.status.clone()
    }

    /// Requests cancellation. A blocked event send is also interrupted.
    pub fn cancel(&self) {
        self.cancel.send_replace(true);
    }

    /// Cancels the task and waits until its socket and room reservation drop.
    pub async fn stop(&mut self) {
        self.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for ChatSession {
    fn drop(&mut self) {
        self.cancel.send_replace(true);
    }
}

/// Result counters from the earlier bounded room subscription probe.
#[derive(Default, Debug)]
pub struct RoomProbe {
    pub subscribed: bool,
    pub publications: usize,
    pub messages: usize,
    pub error_code: Option<u64>,
}

/// Result counters for the `channel_{id}` auto-move subscription.
#[derive(Default, Debug)]
pub struct MoveProbe {
    pub subscribed: bool,
    pub publications: usize,
    pub move_events: usize,
    pub with_hosted_slug: usize,
    pub with_hosted_username: usize,
    pub with_viewers_count: usize,
    pub target_slugs: Vec<String>,
    pub event_names: Vec<String>,
}

/// Field-presence counters for the `chatroom_{id}` reward subscription.
/// Payload values are never retained or printed.
#[derive(Default, Debug)]
pub struct RewardProbe {
    pub subscribed: bool,
    pub publications: usize,
    pub reward_events: usize,
    pub with_reward_title: usize,
    pub with_username: usize,
    pub with_user_input: usize,
    pub with_background_color: usize,
    pub with_id: usize,
    pub with_created_at: usize,
    pub event_names: Vec<String>,
}

impl KickClient {
    /// Starts one live chat task for `chatroom_id`.
    ///
    /// The task fetches a fresh realtime configuration and connection token on
    /// every reconnect. Its event queue is capped at 128 messages; a slow
    /// consumer applies backpressure, and cancellation interrupts that wait.
    /// A room can have only one active session per process. Reconnect replays
    /// are deduplicated by message ID in a bounded 2,048-ID window; messages
    /// without an ID cannot be deduplicated.
    pub async fn run_chat(
        &self,
        client_id: &str,
        chatroom_id: u64,
        channel_id: Option<u64>,
    ) -> Result<ChatSession, ChatStartError> {
        if !valid_client_id(client_id) {
            return Err(ChatStartError::InvalidClientId);
        }
        if chatroom_id == 0 {
            return Err(ChatStartError::InvalidChatroomId);
        }
        if tokio::runtime::Handle::try_current().is_err() {
            return Err(ChatStartError::RuntimeUnavailable);
        }
        let reservation =
            RoomReservation::try_new(chatroom_id).ok_or(ChatStartError::AlreadyRunning)?;
        let client = KickClient {
            http: self.http.clone(),
            access_token: None,
            session_cookie: None,
        };
        let owned_client_id = client_id.to_owned();
        let (event_tx, events) = mpsc::channel(EVENT_CAPACITY);
        let (status_tx, status) = watch::channel(ChatStatus::Connecting { attempt: 0 });
        let (cancel, cancel_rx) = watch::channel(false);
        let task = tokio::spawn(async move {
            run_chat_task(
                client,
                owned_client_id,
                chatroom_id,
                channel_id,
                event_tx,
                status_tx,
                cancel_rx,
                reservation,
            )
            .await;
        });

        Ok(ChatSession {
            events,
            status,
            cancel,
            task: Some(task),
        })
    }

    /// Opens and validates the observed anonymous Centrifugo connection.
    pub async fn anonymous_realtime_handshake(
        &self,
        client_id: &str,
    ) -> Result<bool, KickApiError> {
        let mut socket = self.anonymous_realtime_socket(client_id).await?;
        let _ = time::timeout(Duration::from_secs(1), socket.close(None)).await;
        Ok(true)
    }

    async fn anonymous_realtime_socket(&self, client_id: &str) -> Result<Socket, KickApiError> {
        let config = self.anonymous_realtime_config(client_id).await?;
        let connection = config
            .connections
            .into_iter()
            .find(|connection| connection.provider == "centrifugo")
            .ok_or(KickApiError::InvalidJson)?;
        let url = reqwest::Url::parse(&connection.credentials.url)
            .map_err(|_| KickApiError::InvalidJson)?;
        if url.scheme() != "wss" || !url.username().is_empty() || url.password().is_some() {
            return Err(KickApiError::InvalidJson);
        }

        let response = self
            .http
            .post("https://mobile.kick.com/api/v1/realtime/auth/connection")
            .header("Content-Type", "application/json")
            .body(json!({"client_id":client_id}).to_string())
            .send()
            .await
            .map_err(|error| KickApiError::Transport(classify_transport(&error)))?;
        let mut bytes = limited_body(response).await?;
        let mut value: Value =
            serde_json::from_slice(&bytes).map_err(|_| KickApiError::InvalidJson)?;
        bytes.zeroize();
        let mut token = value
            .get_mut("data")
            .and_then(|data| data.get_mut("token"))
            .map(Value::take)
            .and_then(|value| match value {
                Value::String(token) => Some(Zeroizing::new(token)),
                _ => None,
            })
            .ok_or(KickApiError::InvalidJson)?;

        let (mut socket, _) = time::timeout(
            CONNECT_TIMEOUT,
            connect_async_with_config(
                url.as_str(),
                Some(
                    WebSocketConfig::default()
                        .max_message_size(Some(MAX_WIRE_FRAME_BYTES))
                        .max_frame_size(Some(MAX_WIRE_FRAME_BYTES)),
                ),
                false,
            ),
        )
        .await
        .map_err(|_| KickApiError::InvalidJson)?
        .map_err(|_| KickApiError::InvalidJson)?;

        let mut command_value =
            json!({"id":1,"connect":{"token":token.as_str(),"name":"kick-native","version":"0.1"}});
        let mut command = Zeroizing::new(command_value.to_string());
        token.zeroize();
        if let Some(Value::String(wire_token)) = command_value.pointer_mut("/connect/token") {
            wire_token.zeroize();
        }
        drop(command_value);
        let send_result = time::timeout(
            ACK_TIMEOUT,
            socket.send(Message::Text(command.to_string().into())),
        )
        .await;
        command.zeroize();
        send_result
            .map_err(|_| KickApiError::InvalidJson)?
            .map_err(|_| KickApiError::InvalidJson)?;

        time::timeout(ACK_TIMEOUT, wait_for_connect_ack(&mut socket))
            .await
            .map_err(|_| KickApiError::InvalidJson)?
            .map_err(|_| KickApiError::InvalidJson)?;
        Ok(socket)
    }

    /// Subscribes to `channel.{channel_id}` and counts
    /// `ChatMoveToSupportedChannelEvent` publications without retaining values.
    pub async fn anonymous_move_probe(
        &self,
        client_id: &str,
        channel_id: u64,
        seconds: u64,
    ) -> Result<MoveProbe, KickApiError> {
        let mut socket = self.anonymous_realtime_socket(client_id).await?;
        let channel = format!("channel.{channel_id}");
        socket
            .send(Message::Text(
                json!({"id":2,"subscribe":{"channel":channel}})
                    .to_string()
                    .into(),
            ))
            .await
            .map_err(|_| KickApiError::InvalidJson)?;
        let mut stats = MoveProbe::default();
        let deadline = Instant::now() + Duration::from_secs(seconds.clamp(1, 300));
        while let Ok(Some(frame)) = time::timeout_at(deadline, socket.next()).await {
            let frame = frame.map_err(|_| KickApiError::InvalidJson)?;
            if let Message::Text(text) = frame {
                if text.len() > MAX_WIRE_FRAME_BYTES {
                    return Err(KickApiError::ResponseTooLarge);
                }
                for line in text.lines() {
                    let reply: Value =
                        serde_json::from_str(line).map_err(|_| KickApiError::InvalidJson)?;
                    if reply.as_object().is_some_and(|object| object.is_empty()) {
                        socket
                            .send(Message::Text("{}".into()))
                            .await
                            .map_err(|_| KickApiError::InvalidJson)?;
                    }
                    if reply.get("id").and_then(Value::as_u64) == Some(2) {
                        stats.subscribed = reply.get("subscribe").is_some_and(Value::is_object)
                            && reply.get("error").is_none();
                        if !stats.subscribed {
                            break;
                        }
                    }
                    if reply.pointer("/push/channel").and_then(Value::as_str)
                        != Some(channel.as_str())
                    {
                        continue;
                    }
                    let Some(publication) = reply.pointer("/push/pub/data") else {
                        continue;
                    };
                    stats.publications += 1;
                    let Some(event) = publication.get("event").and_then(Value::as_str) else {
                        continue;
                    };
                    if !stats.event_names.iter().any(|name| name == event) {
                        if stats.event_names.len() < 20 {
                            stats.event_names.push(event.to_owned());
                        }
                    }
                    if event == MOVE_TO_SUPPORTED_CHANNEL_EVENT {
                        stats.move_events += 1;
                        let Some(raw_payload) = publication.get("data") else {
                            continue;
                        };
                        let payload = match raw_payload {
                            Value::String(raw) => match serde_json::from_str::<Value>(raw) {
                                Ok(value) => value,
                                Err(_) => continue,
                            },
                            Value::Object(_) => raw_payload.clone(),
                            _ => continue,
                        };
                        let Some(hosted) = payload.get("hosted") else {
                            continue;
                        };
                        if let Some(slug) = hosted
                            .get("slug")
                            .and_then(Value::as_str)
                            .filter(|slug| !slug.trim().is_empty())
                        {
                            stats.with_hosted_slug += 1;
                            if stats.target_slugs.len() < 5
                                && !stats.target_slugs.iter().any(|known| known == slug)
                            {
                                stats.target_slugs.push(slug.to_owned());
                            }
                        }
                        if hosted
                            .get("username")
                            .and_then(Value::as_str)
                            .is_some_and(|value| !value.trim().is_empty())
                        {
                            stats.with_hosted_username += 1;
                        }
                        if hosted.get("viewers_count").and_then(value_as_u64).is_some() {
                            stats.with_viewers_count += 1;
                        }
                    }
                }
            } else if let Message::Ping(payload) = frame {
                socket
                    .send(Message::Pong(payload))
                    .await
                    .map_err(|_| KickApiError::InvalidJson)?;
            }
        }
        let _ = time::timeout(Duration::from_secs(1), socket.close(None)).await;
        Ok(stats)
    }

    /// Subscribes to `chatroom_{room_id}` and counts `RewardRedeemedEvent`
    /// publications and their field presence. Values are not retained.
    pub async fn anonymous_reward_probe(
        &self,
        client_id: &str,
        room_id: u64,
        seconds: u64,
    ) -> Result<RewardProbe, KickApiError> {
        let mut socket = self.anonymous_realtime_socket(client_id).await?;
        let channel = format!("chatroom_{room_id}");
        socket
            .send(Message::Text(
                json!({"id":2,"subscribe":{"channel":channel}})
                    .to_string()
                    .into(),
            ))
            .await
            .map_err(|_| KickApiError::InvalidJson)?;
        let mut stats = RewardProbe::default();
        let deadline = Instant::now() + Duration::from_secs(seconds.clamp(1, 300));
        while let Ok(Some(frame)) = time::timeout_at(deadline, socket.next()).await {
            let frame = frame.map_err(|_| KickApiError::InvalidJson)?;
            if let Message::Text(text) = frame {
                if text.len() > MAX_WIRE_FRAME_BYTES {
                    return Err(KickApiError::ResponseTooLarge);
                }
                for line in text.lines() {
                    let reply: Value =
                        serde_json::from_str(line).map_err(|_| KickApiError::InvalidJson)?;
                    if reply.as_object().is_some_and(|object| object.is_empty()) {
                        socket
                            .send(Message::Text("{}".into()))
                            .await
                            .map_err(|_| KickApiError::InvalidJson)?;
                    }
                    if reply.get("id").and_then(Value::as_u64) == Some(2) {
                        stats.subscribed = reply.get("subscribe").is_some_and(Value::is_object)
                            && reply.get("error").is_none();
                        if !stats.subscribed {
                            break;
                        }
                    }
                    if reply.pointer("/push/channel").and_then(Value::as_str)
                        != Some(channel.as_str())
                    {
                        continue;
                    }
                    let Some(publication) = reply.pointer("/push/pub/data") else {
                        continue;
                    };
                    stats.publications += 1;
                    let Some(event) = publication.get("event").and_then(Value::as_str) else {
                        continue;
                    };
                    if !stats.event_names.iter().any(|name| name == event) {
                        if stats.event_names.len() < 20 {
                            stats.event_names.push(event.to_owned());
                        }
                    }
                    if event != "RewardRedeemedEvent" {
                        continue;
                    }
                    stats.reward_events += 1;
                    let Some(raw_payload) = publication.get("data") else {
                        continue;
                    };
                    let payload = match raw_payload {
                        Value::String(raw) => match serde_json::from_str::<Value>(raw) {
                            Ok(value) => value,
                            Err(_) => continue,
                        },
                        Value::Object(_) => raw_payload.clone(),
                        _ => continue,
                    };
                    if payload
                        .get("reward_title")
                        .and_then(Value::as_str)
                        .is_some_and(|value| !value.trim().is_empty())
                    {
                        stats.with_reward_title += 1;
                    }
                    if payload
                        .get("username")
                        .and_then(Value::as_str)
                        .is_some_and(|value| !value.trim().is_empty())
                    {
                        stats.with_username += 1;
                    }
                    if payload.get("user_input").is_some_and(Value::is_string) {
                        stats.with_user_input += 1;
                    }
                    if payload
                        .get("reward_background_color")
                        .is_some_and(Value::is_string)
                    {
                        stats.with_background_color += 1;
                    }
                    if payload.get("id").is_some_and(Value::is_string) {
                        stats.with_id += 1;
                    }
                    if payload.get("created_at").is_some_and(Value::is_string) {
                        stats.with_created_at += 1;
                    }
                }
            } else if let Message::Ping(payload) = frame {
                socket
                    .send(Message::Pong(payload))
                    .await
                    .map_err(|_| KickApiError::InvalidJson)?;
            }
        }
        let _ = time::timeout(Duration::from_secs(1), socket.close(None)).await;
        Ok(stats)
    }

    /// Opens the cookie-authenticated web realtime connection and counts
    /// `RewardRedeemedEvent` publications on `chatroom_{room_id}`.
    /// The connection token is zeroized and never printed.
    pub async fn authenticated_web_reward_probe(
        &self,
        client_id: &str,
        room_id: u64,
        seconds: u64,
    ) -> Result<RewardProbe, KickApiError> {
        let cookie = self
            .session_cookie
            .as_ref()
            .ok_or(KickApiError::SessionRequired)?;
        let config_url = reqwest::Url::parse("https://web.kick.com/api/v1/realtime/connection")
            .map_err(|_| KickApiError::InvalidJson)?;
        let config_body = json!({
            "client": {"id": client_id, "type": "web"},
            "capabilities": {"accepted_providers": [{"provider": "pusher"}, {"provider": "centrifugo"}]}
        });
        let request = cookie
            .apply_auth_headers(self.http.post(config_url))
            .map_err(|_| KickApiError::SessionInvalid)?;
        let response = request
            .header("Content-Type", "application/json")
            .body(config_body.to_string())
            .send()
            .await
            .map_err(|error| KickApiError::Transport(classify_transport(&error)))?;
        if !response.status().is_success() {
            return Err(KickApiError::HttpStatus(response.status().as_u16()));
        }
        let mut config_bytes = limited_body(response).await?;
        let config: Value =
            serde_json::from_slice(&config_bytes).map_err(|_| KickApiError::InvalidJson)?;
        config_bytes.zeroize();
        let url = config
            .pointer("/data/connections")
            .and_then(Value::as_array)
            .and_then(|connections| {
                connections.iter().find(|connection| {
                    connection.get("provider").and_then(Value::as_str) == Some("centrifugo")
                })
            })
            .and_then(|connection| connection.pointer("/credentials/url"))
            .and_then(Value::as_str)
            .ok_or(KickApiError::InvalidJson)?;
        let url = reqwest::Url::parse(url).map_err(|_| KickApiError::InvalidJson)?;
        if url.scheme() != "wss" {
            return Err(KickApiError::InvalidJson);
        }

        let auth_url = reqwest::Url::parse("https://web.kick.com/api/v1/realtime/auth/connection")
            .map_err(|_| KickApiError::InvalidJson)?;
        let request = cookie
            .apply_auth_headers(self.http.post(auth_url))
            .map_err(|_| KickApiError::SessionInvalid)?;
        let response = request
            .header("Content-Type", "application/json")
            .body(json!({"client_id": client_id}).to_string())
            .send()
            .await
            .map_err(|error| KickApiError::Transport(classify_transport(&error)))?;
        if !response.status().is_success() {
            return Err(KickApiError::HttpStatus(response.status().as_u16()));
        }
        let mut auth_bytes = limited_body(response).await?;
        let auth: Value =
            serde_json::from_slice(&auth_bytes).map_err(|_| KickApiError::InvalidJson)?;
        auth_bytes.zeroize();
        let mut token = auth
            .pointer("/data/data/token")
            .and_then(Value::as_str)
            .or_else(|| auth.pointer("/data/token").and_then(Value::as_str))
            .map(|token| Zeroizing::new(token.to_owned()))
            .ok_or(KickApiError::InvalidJson)?;

        let (mut socket, _) = time::timeout(
            CONNECT_TIMEOUT,
            connect_async_with_config(
                url.as_str(),
                Some(
                    WebSocketConfig::default()
                        .max_message_size(Some(MAX_WIRE_FRAME_BYTES))
                        .max_frame_size(Some(MAX_WIRE_FRAME_BYTES)),
                ),
                false,
            ),
        )
        .await
        .map_err(|_| KickApiError::InvalidJson)?
        .map_err(|_| KickApiError::InvalidJson)?;
        let mut command_value =
            json!({"id":1,"connect":{"token":token.as_str(),"name":"kick-native","version":"0.1"}});
        let mut command = Zeroizing::new(command_value.to_string());
        if let Some(Value::String(wire_token)) = command_value.pointer_mut("/connect/token") {
            wire_token.zeroize();
        }
        drop(command_value);
        socket
            .send(Message::Text(command.to_string().into()))
            .await
            .map_err(|_| KickApiError::InvalidJson)?;
        command.zeroize();
        token.zeroize();
        time::timeout(ACK_TIMEOUT, wait_for_connect_ack(&mut socket))
            .await
            .map_err(|_| KickApiError::InvalidJson)?
            .map_err(|_| KickApiError::InvalidJson)?;

        let channel = format!("chatroom_{room_id}");
        socket
            .send(Message::Text(
                json!({"id":2,"subscribe":{"channel":channel}})
                    .to_string()
                    .into(),
            ))
            .await
            .map_err(|_| KickApiError::InvalidJson)?;
        let mut stats = RewardProbe::default();
        let deadline = Instant::now() + Duration::from_secs(seconds.clamp(1, 300));
        while let Ok(Some(frame)) = time::timeout_at(deadline, socket.next()).await {
            let frame = frame.map_err(|_| KickApiError::InvalidJson)?;
            if let Message::Text(text) = frame {
                if text.len() > MAX_WIRE_FRAME_BYTES {
                    return Err(KickApiError::ResponseTooLarge);
                }
                for line in text.lines() {
                    let reply: Value =
                        serde_json::from_str(line).map_err(|_| KickApiError::InvalidJson)?;
                    if reply.as_object().is_some_and(|object| object.is_empty()) {
                        socket
                            .send(Message::Text("{}".into()))
                            .await
                            .map_err(|_| KickApiError::InvalidJson)?;
                    }
                    if reply.get("id").and_then(Value::as_u64) == Some(2) {
                        stats.subscribed = reply.get("subscribe").is_some_and(Value::is_object)
                            && reply.get("error").is_none();
                        if !stats.subscribed {
                            break;
                        }
                    }
                    if reply.pointer("/push/channel").and_then(Value::as_str)
                        != Some(channel.as_str())
                    {
                        continue;
                    }
                    let Some(publication) = reply.pointer("/push/pub/data") else {
                        continue;
                    };
                    stats.publications += 1;
                    let Some(event) = publication.get("event").and_then(Value::as_str) else {
                        continue;
                    };
                    if !stats.event_names.iter().any(|name| name == event) {
                        if stats.event_names.len() < 20 {
                            stats.event_names.push(event.to_owned());
                        }
                    }
                    if event != "RewardRedeemedEvent" {
                        continue;
                    }
                    stats.reward_events += 1;
                    let Some(raw_payload) = publication.get("data") else {
                        continue;
                    };
                    let payload = match raw_payload {
                        Value::String(raw) => match serde_json::from_str::<Value>(raw) {
                            Ok(value) => value,
                            Err(_) => continue,
                        },
                        Value::Object(_) => raw_payload.clone(),
                        _ => continue,
                    };
                    if payload
                        .get("reward_title")
                        .and_then(Value::as_str)
                        .is_some_and(|value| !value.trim().is_empty())
                    {
                        stats.with_reward_title += 1;
                    }
                    if payload
                        .get("username")
                        .and_then(Value::as_str)
                        .is_some_and(|value| !value.trim().is_empty())
                    {
                        stats.with_username += 1;
                    }
                    if payload.get("user_input").is_some_and(Value::is_string) {
                        stats.with_user_input += 1;
                    }
                    if payload
                        .get("reward_background_color")
                        .is_some_and(Value::is_string)
                    {
                        stats.with_background_color += 1;
                    }
                    if payload.get("id").is_some_and(Value::is_string) {
                        stats.with_id += 1;
                    }
                    if payload.get("created_at").is_some_and(Value::is_string) {
                        stats.with_created_at += 1;
                    }
                }
            } else if let Message::Ping(payload) = frame {
                socket
                    .send(Message::Pong(payload))
                    .await
                    .map_err(|_| KickApiError::InvalidJson)?;
            }
        }
        let _ = time::timeout(Duration::from_secs(1), socket.close(None)).await;
        Ok(stats)
    }

    /// Tests the observed public room template; publication bodies are not retained.
    pub async fn anonymous_room_probe(
        &self,
        client_id: &str,
        room_id: u64,
    ) -> Result<RoomProbe, KickApiError> {
        let mut socket = self.anonymous_realtime_socket(client_id).await?;
        let channel = format!("chatrooms.{room_id}.v2");
        socket
            .send(Message::Text(
                json!({"id":2,"subscribe":{"channel":channel}})
                    .to_string()
                    .into(),
            ))
            .await
            .map_err(|_| KickApiError::InvalidJson)?;
        let mut stats = RoomProbe::default();
        let deadline = Instant::now() + Duration::from_secs(30);
        while let Ok(Some(frame)) = time::timeout_at(deadline, socket.next()).await {
            let frame = frame.map_err(|_| KickApiError::InvalidJson)?;
            if let Message::Text(text) = frame {
                if text.len() > MAX_WIRE_FRAME_BYTES {
                    return Err(KickApiError::ResponseTooLarge);
                }
                for line in text.lines() {
                    let reply: Value =
                        serde_json::from_str(line).map_err(|_| KickApiError::InvalidJson)?;
                    if reply.as_object().is_some_and(|object| object.is_empty()) {
                        socket
                            .send(Message::Text("{}".into()))
                            .await
                            .map_err(|_| KickApiError::InvalidJson)?;
                    }
                    if reply.get("id").and_then(Value::as_u64) == Some(2) {
                        stats.error_code = reply.pointer("/error/code").and_then(Value::as_u64);
                        stats.subscribed = reply.get("subscribe").is_some_and(Value::is_object)
                            && stats.error_code.is_none();
                        if !stats.subscribed {
                            break;
                        }
                    }
                    if reply.pointer("/push/channel").and_then(Value::as_str)
                        == Some(channel.as_str())
                        && reply.pointer("/push/pub/data").is_some()
                    {
                        stats.publications += 1;
                        if parse_chat_publication(&reply, &channel, room_id).is_some() {
                            stats.messages += 1;
                        }
                    }
                }
            } else if let Message::Ping(payload) = frame {
                socket
                    .send(Message::Pong(payload))
                    .await
                    .map_err(|_| KickApiError::InvalidJson)?;
            }
            if stats.error_code.is_some() || stats.messages >= 3 {
                break;
            }
        }
        let _ = time::timeout(Duration::from_secs(1), socket.close(None)).await;
        Ok(stats)
    }
}

fn valid_client_id(client_id: &str) -> bool {
    !client_id.is_empty()
        && client_id.len() <= 128
        && client_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

static ACTIVE_CHATROOMS: OnceLock<Mutex<HashSet<u64>>> = OnceLock::new();

struct RoomReservation(u64);

impl RoomReservation {
    fn try_new(chatroom_id: u64) -> Option<Self> {
        let active = ACTIVE_CHATROOMS.get_or_init(|| Mutex::new(HashSet::new()));
        let mut active = active.lock().unwrap_or_else(|poison| poison.into_inner());
        if active.contains(&chatroom_id) {
            return None;
        }
        active.insert(chatroom_id);
        Some(Self(chatroom_id))
    }
}

impl Drop for RoomReservation {
    fn drop(&mut self) {
        if let Some(active) = ACTIVE_CHATROOMS.get() {
            active
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .remove(&self.0);
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum RunFailure {
    Cancelled,
    ReceiverClosed,
    Transport,
    Protocol,
    Rejected,
    Timeout,
}

/// Subscribes to `channel.{id}` for every notify channel on one Centrifugo
/// connection and emits stream-start/end events. This is push-based: the
/// client does not poll the stream status.
pub async fn run_stream_notifications(
    client: KickClient,
    client_id: String,
    slugs: Vec<String>,
    events: mpsc::Sender<StreamNotificationEvent>,
    cancel: Arc<AtomicBool>,
) {
    let mut subscriptions = Vec::new();
    for slug in slugs {
        if cancel.load(Ordering::Acquire) {
            return;
        }
        match client.anonymous_channel(&slug).await {
            Ok(channel) => {
                if let Some(channel_id) = channel.id {
                    subscriptions.push((channel_id, slug));
                }
            }
            Err(_) => {}
        }
    }
    if subscriptions.is_empty() {
        return;
    }

    let mut attempt = 0_u32;
    loop {
        if cancel.load(Ordering::Acquire) || events.is_closed() {
            break;
        }
        match run_stream_notification_socket(&client, &client_id, &subscriptions, &events, &cancel)
            .await
        {
            Ok(()) => break,
            Err(_) => {}
        }
        if cancel.load(Ordering::Acquire) || events.is_closed() {
            break;
        }
        attempt = attempt.saturating_add(1);
        let delay = reconnect_delay(attempt);
        let mut waited = Duration::ZERO;
        while waited < delay && !cancel.load(Ordering::Acquire) {
            time::sleep(Duration::from_millis(250)).await;
            waited += Duration::from_millis(250);
        }
    }
}

async fn run_stream_notification_socket(
    client: &KickClient,
    client_id: &str,
    subscriptions: &[(u64, String)],
    events: &mpsc::Sender<StreamNotificationEvent>,
    cancel: &Arc<AtomicBool>,
) -> Result<(), RunFailure> {
    let mut socket = client
        .anonymous_realtime_socket(client_id)
        .await
        .map_err(|_| RunFailure::Transport)?;
    wait_for_connect_ack(&mut socket).await?;

    for (index, (channel_id, _)) in subscriptions.iter().enumerate() {
        let id = 2 + index as u64;
        let channel = format!("channel.{channel_id}");
        socket
            .send(Message::Text(
                json!({"id": id, "subscribe": {"channel": channel}})
                    .to_string()
                    .into(),
            ))
            .await
            .map_err(|_| RunFailure::Transport)?;
    }

    let deadline = Instant::now() + ACK_TIMEOUT;
    let mut subscribed = vec![false; subscriptions.len()];
    while subscribed.iter().any(|value| !value) {
        if cancel.load(Ordering::Acquire) {
            return Ok(());
        }
        let frame = time::timeout_at(deadline, socket.next())
            .await
            .map_err(|_| RunFailure::Timeout)?
            .ok_or(RunFailure::Transport)?
            .map_err(|_| RunFailure::Transport)?;
        match frame {
            Message::Text(text) => {
                if text.len() > MAX_WIRE_FRAME_BYTES {
                    return Err(RunFailure::Protocol);
                }
                for line in text.lines() {
                    let reply: Value =
                        serde_json::from_str(line).map_err(|_| RunFailure::Protocol)?;
                    if reply.as_object().is_some_and(|object| object.is_empty()) {
                        socket
                            .send(Message::Text("{}".into()))
                            .await
                            .map_err(|_| RunFailure::Transport)?;
                    }
                    if let Some(id) = reply.get("id").and_then(Value::as_u64) {
                        if id >= 2 && (id as usize) < 2 + subscriptions.len() {
                            subscribed[(id - 2) as usize] =
                                reply.get("subscribe").is_some_and(Value::is_object)
                                    && reply.get("error").is_none();
                        }
                        continue;
                    }
                    if let Some(event) =
                        parse_stream_notification_publication(&reply, subscriptions)
                        && events.send(event).await.is_err()
                    {
                        return Err(RunFailure::ReceiverClosed);
                    }
                }
            }
            Message::Ping(payload) => socket
                .send(Message::Pong(payload))
                .await
                .map_err(|_| RunFailure::Transport)?,
            Message::Close(_) => return Err(RunFailure::Transport),
            _ => {}
        }
    }

    let mut heartbeat = time::interval_at(Instant::now() + HEARTBEAT_INTERVAL, HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut ping_sequence = 0_u64;
    loop {
        if cancel.load(Ordering::Acquire) {
            return Ok(());
        }
        tokio::select! {
            biased;
            _ = heartbeat.tick() => {
                ping_sequence = ping_sequence.wrapping_add(1);
                socket
                    .send(Message::Ping(ping_sequence.to_be_bytes().to_vec().into()))
                    .await
                    .map_err(|_| RunFailure::Transport)?;
            }
            result = time::timeout(Duration::from_secs(1), socket.next()) => {
                match result {
                    Ok(Some(Ok(Message::Text(text)))) => {
                        if text.len() > MAX_WIRE_FRAME_BYTES {
                            return Err(RunFailure::Protocol);
                        }
                        for line in text.lines() {
                            let reply: Value =
                                serde_json::from_str(line).map_err(|_| RunFailure::Protocol)?;
                            if reply.as_object().is_some_and(|object| object.is_empty()) {
                                socket
                                    .send(Message::Text("{}".into()))
                                    .await
                                    .map_err(|_| RunFailure::Transport)?;
                            }
                            if let Some(event) =
                                parse_stream_notification_publication(&reply, subscriptions)
                                && events.send(event).await.is_err()
                            {
                                return Err(RunFailure::ReceiverClosed);
                            }
                        }
                    }
                    Ok(Some(Ok(Message::Ping(payload)))) => {
                        socket
                            .send(Message::Pong(payload))
                            .await
                            .map_err(|_| RunFailure::Transport)?;
                    }
                    Ok(Some(Ok(Message::Close(_)))) => return Err(RunFailure::Transport),
                    Ok(Some(Err(_))) | Ok(None) => return Err(RunFailure::Transport),
                    Err(_) => {}
                    _ => {}
                }
            }
        }
    }
}

fn parse_stream_notification_publication(
    reply: &Value,
    subscriptions: &[(u64, String)],
) -> Option<StreamNotificationEvent> {
    let channel = reply.pointer("/push/channel")?.as_str()?;
    let (channel_id, slug) = subscriptions
        .iter()
        .find(|(id, _)| channel == format!("channel.{id}"))?;
    let event = reply.pointer("/push/pub/data/event")?.as_str()?;
    match event {
        STREAMER_IS_LIVE_EVENT => Some(StreamNotificationEvent::StreamerIsLive {
            slug: slug.clone(),
            channel_id: *channel_id,
        }),
        STOP_STREAM_BROADCAST_EVENT => Some(StreamNotificationEvent::StreamEnded {
            slug: slug.clone(),
            channel_id: *channel_id,
        }),
        _ => None,
    }
}

async fn run_chat_task(
    client: KickClient,
    client_id: String,
    chatroom_id: u64,
    channel_id: Option<u64>,
    events: mpsc::Sender<ChatEvent>,
    status: watch::Sender<ChatStatus>,
    mut cancel: watch::Receiver<bool>,
    _reservation: RoomReservation,
) {
    let chat_channel = format!("chatrooms.{chatroom_id}.v2");
    let raid_channel = format!("chatrooms.{chatroom_id}");
    let reward_channel = format!("chatroom_{chatroom_id}");
    let move_channel = channel_id.map(|id| format!("channel.{id}"));
    let mut deduper = MessageDeduper::default();
    let mut attempt = 0_u32;

    loop {
        if is_cancelled(&cancel) || events.is_closed() {
            break;
        }
        status.send_replace(ChatStatus::Connecting { attempt });
        let result = open_subscribed_socket(
            &client,
            &client_id,
            chatroom_id,
            &chat_channel,
            &raid_channel,
            &reward_channel,
            move_channel.as_deref(),
            &events,
            &mut cancel,
            &mut deduper,
        )
        .await;

        match result {
            Ok(mut socket) => {
                attempt = 0;
                status.send_replace(ChatStatus::Connected);
                match run_publication_loop(
                    &mut socket,
                    chatroom_id,
                    &chat_channel,
                    &raid_channel,
                    &reward_channel,
                    move_channel.as_deref(),
                    &events,
                    &mut cancel,
                    &mut deduper,
                )
                .await
                {
                    Err(RunFailure::Cancelled | RunFailure::ReceiverClosed) => break,
                    _ => {}
                }
            }
            Err(RunFailure::Cancelled | RunFailure::ReceiverClosed) => break,
            Err(_) => {}
        }

        if is_cancelled(&cancel) || events.is_closed() {
            break;
        }
        attempt = attempt.saturating_add(1);
        status.send_replace(ChatStatus::Reconnecting { attempt });
        let delay = reconnect_delay(attempt);
        if !wait_for_retry(delay, &mut cancel).await {
            break;
        }
    }
    status.send_replace(ChatStatus::Stopped);
}

fn reconnect_delay(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(6);
    Duration::from_secs((1_u64 << shift).min(MAX_RECONNECT_DELAY.as_secs()))
}

async fn wait_for_retry(delay: Duration, cancel: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        biased;
        _ = wait_for_cancel(cancel) => false,
        _ = time::sleep(delay) => true,
    }
}

async fn open_subscribed_socket(
    client: &KickClient,
    client_id: &str,
    chatroom_id: u64,
    chat_channel: &str,
    raid_channel: &str,
    reward_channel: &str,
    move_channel: Option<&str>,
    events: &mpsc::Sender<ChatEvent>,
    cancel: &mut watch::Receiver<bool>,
    deduper: &mut MessageDeduper,
) -> Result<Socket, RunFailure> {
    let mut socket = tokio::select! {
        biased;
        _ = wait_for_cancel(cancel) => return Err(RunFailure::Cancelled),
        result = client.anonymous_realtime_socket(client_id) => {
            result.map_err(|_| RunFailure::Transport)?
        }
    };
    let mut subscriptions = vec![
        (2_u64, chat_channel),
        (3_u64, raid_channel),
        (4_u64, reward_channel),
    ];
    if let Some(move_channel) = move_channel {
        subscriptions.push((5_u64, move_channel));
    }
    for (id, channel) in subscriptions {
        tokio::select! {
            biased;
            _ = wait_for_cancel(cancel) => return Err(RunFailure::Cancelled),
            result = socket.send(Message::Text(
                json!({"id":id,"subscribe":{"channel":channel}}).to_string().into()
            )) => result.map_err(|_| RunFailure::Transport)?,
        }
    }
    wait_for_subscribe_acks(
        &mut socket,
        chatroom_id,
        chat_channel,
        raid_channel,
        reward_channel,
        move_channel,
        events,
        cancel,
        deduper,
    )
    .await?;
    Ok(socket)
}

async fn wait_for_connect_ack<S>(socket: &mut WebSocketStream<S>) -> Result<(), RunFailure>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        let frame = time::timeout(ACK_TIMEOUT, socket.next())
            .await
            .map_err(|_| RunFailure::Timeout)?
            .ok_or(RunFailure::Transport)?
            .map_err(|_| RunFailure::Transport)?;
        match frame {
            Message::Text(text) => {
                if text.len() > MAX_WIRE_FRAME_BYTES {
                    return Err(RunFailure::Protocol);
                }
                for line in text.lines() {
                    let reply: Value =
                        serde_json::from_str(line).map_err(|_| RunFailure::Protocol)?;
                    if reply.as_object().is_some_and(|object| object.is_empty()) {
                        socket
                            .send(Message::Text("{}".into()))
                            .await
                            .map_err(|_| RunFailure::Transport)?;
                    }
                    if reply.get("id").and_then(Value::as_u64) == Some(1) {
                        if reply.get("connect").is_some_and(Value::is_object)
                            && reply.get("error").is_none()
                        {
                            return Ok(());
                        }
                        return Err(RunFailure::Rejected);
                    }
                }
            }
            Message::Ping(payload) => socket
                .send(Message::Pong(payload))
                .await
                .map_err(|_| RunFailure::Transport)?,
            Message::Close(_) => return Err(RunFailure::Transport),
            _ => {}
        }
    }
}

async fn wait_for_subscribe_acks<S>(
    socket: &mut WebSocketStream<S>,
    chatroom_id: u64,
    chat_channel: &str,
    raid_channel: &str,
    reward_channel: &str,
    move_channel: Option<&str>,
    events: &mpsc::Sender<ChatEvent>,
    cancel: &mut watch::Receiver<bool>,
    deduper: &mut MessageDeduper,
) -> Result<(), RunFailure>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let deadline = Instant::now() + ACK_TIMEOUT;
    let mut chat_subscribed = false;
    let mut raid_subscribed = false;
    let mut reward_subscribed = false;
    let mut move_subscribed = move_channel.is_none();
    loop {
        let frame = tokio::select! {
            biased;
            _ = wait_for_cancel(cancel) => return Err(RunFailure::Cancelled),
            result = time::timeout_at(deadline, socket.next()) => {
                match result {
                    Ok(frame) => frame.ok_or(RunFailure::Transport)?.map_err(|_| RunFailure::Transport)?,
                    Err(_) if chat_subscribed => return Ok(()),
                    Err(_) => return Err(RunFailure::Timeout),
                }
            }
        };
        match frame {
            Message::Text(text) => {
                if text.len() > MAX_WIRE_FRAME_BYTES {
                    return Err(RunFailure::Protocol);
                }
                for line in text.lines() {
                    let reply: Value =
                        serde_json::from_str(line).map_err(|_| RunFailure::Protocol)?;
                    if reply.as_object().is_some_and(|object| object.is_empty()) {
                        send_text_cancelable(socket, "{}", cancel).await?;
                    }
                    if let Some(id) = reply.get("id").and_then(Value::as_u64) {
                        let accepted = reply.get("subscribe").is_some_and(Value::is_object)
                            && reply.get("error").is_none();
                        match id {
                            2 => {
                                if !accepted {
                                    return Err(RunFailure::Rejected);
                                }
                                chat_subscribed = true;
                            }
                            3 => {
                                if accepted {
                                    raid_subscribed = true;
                                }
                            }
                            4 => {
                                // A missing reward subscription must not break
                                // chat; the channel is optional for channels
                                // that do not use channel points.
                                reward_subscribed = true;
                            }
                            5 => {
                                // The move-to-hosted event is optional too; a
                                // rejected channel subscription must not break
                                // normal chat.
                                move_subscribed = true;
                            }
                            _ => {}
                        }
                        if chat_subscribed
                            && raid_subscribed
                            && reward_subscribed
                            && move_subscribed
                        {
                            return Ok(());
                        }
                        continue;
                    }
                    dispatch_publication(
                        &reply,
                        chatroom_id,
                        chat_channel,
                        raid_channel,
                        reward_channel,
                        move_channel,
                        events,
                        cancel,
                        deduper,
                    )
                    .await?;
                }
            }
            Message::Ping(payload) => send_pong_cancelable(socket, payload, cancel).await?,
            Message::Close(_) => return Err(RunFailure::Transport),
            _ => {}
        }
    }
}

async fn run_publication_loop<S>(
    socket: &mut WebSocketStream<S>,
    chatroom_id: u64,
    chat_channel: &str,
    raid_channel: &str,
    reward_channel: &str,
    move_channel: Option<&str>,
    events: &mpsc::Sender<ChatEvent>,
    cancel: &mut watch::Receiver<bool>,
    deduper: &mut MessageDeduper,
) -> Result<(), RunFailure>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut heartbeat = time::interval_at(Instant::now() + HEARTBEAT_INTERVAL, HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut ping_sequence = 0_u64;
    let mut pending_pong: Option<(Vec<u8>, Instant)> = None;

    loop {
        let pong_deadline = pending_pong.as_ref().map(|(_, deadline)| *deadline);
        tokio::select! {
            biased;
            _ = wait_for_cancel(cancel) => return Err(RunFailure::Cancelled),
            _ = heartbeat.tick(), if pending_pong.is_none() => {
                ping_sequence = ping_sequence.wrapping_add(1);
                let payload = ping_sequence.to_be_bytes().to_vec();
                send_ping_cancelable(socket, payload.clone(), cancel).await?;
                pending_pong = Some((payload, Instant::now() + PONG_TIMEOUT));
            }
            _ = async {
                match pong_deadline {
                    Some(deadline) => time::sleep_until(deadline).await,
                    None => future::pending().await,
                }
            } => return Err(RunFailure::Timeout),
            frame = socket.next() => {
                let Some(frame) = frame else { return Err(RunFailure::Transport); };
                let frame = frame.map_err(|_| RunFailure::Transport)?;
                match frame {
                    Message::Text(text) => {
                        if text.len() > MAX_WIRE_FRAME_BYTES {
                            return Err(RunFailure::Protocol);
                        }
                        dispatch_text(
                            socket,
                            &text,
                            chatroom_id,
                            chat_channel,
                            raid_channel,
                            reward_channel,
                            move_channel,
                            events,
                            cancel,
                            deduper,
                        ).await?;
                    }
                    Message::Ping(payload) => send_pong_cancelable(socket, payload, cancel).await?,
                    Message::Pong(payload) => {
                        if pending_pong.as_ref().is_some_and(|(expected, _)| expected.as_slice() == payload.as_ref()) {
                            pending_pong = None;
                        }
                    }
                    Message::Close(_) => return Err(RunFailure::Transport),
                    _ => {}
                }
            }
        }
    }
}

async fn dispatch_text<S>(
    socket: &mut WebSocketStream<S>,
    text: &str,
    chatroom_id: u64,
    chat_channel: &str,
    raid_channel: &str,
    reward_channel: &str,
    move_channel: Option<&str>,
    events: &mpsc::Sender<ChatEvent>,
    cancel: &mut watch::Receiver<bool>,
    deduper: &mut MessageDeduper,
) -> Result<(), RunFailure>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    for line in text.lines() {
        let reply: Value = serde_json::from_str(line).map_err(|_| RunFailure::Protocol)?;
        if reply.as_object().is_some_and(|object| object.is_empty()) {
            send_text_cancelable(socket, "{}", cancel).await?;
            continue;
        }
        dispatch_publication(
            &reply,
            chatroom_id,
            chat_channel,
            raid_channel,
            reward_channel,
            move_channel,
            events,
            cancel,
            deduper,
        )
        .await?;
    }
    Ok(())
}

async fn dispatch_publication(
    reply: &Value,
    chatroom_id: u64,
    chat_channel: &str,
    raid_channel: &str,
    reward_channel: &str,
    move_channel: Option<&str>,
    events: &mpsc::Sender<ChatEvent>,
    cancel: &mut watch::Receiver<bool>,
    deduper: &mut MessageDeduper,
) -> Result<(), RunFailure> {
    let channel = reply
        .pointer("/push/channel")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let event = if channel == chat_channel {
        parse_chat_publication(reply, chat_channel, chatroom_id)
            .map(ChatEvent::Message)
            .or_else(|| {
                parse_raid_publication(reply, chat_channel, chatroom_id).map(ChatEvent::Raid)
            })
    } else if channel == raid_channel {
        parse_raid_publication(reply, raid_channel, chatroom_id).map(ChatEvent::Raid)
    } else if channel == reward_channel {
        parse_reward_publication(reply, reward_channel, chatroom_id).map(ChatEvent::Message)
    } else if move_channel.is_some_and(|move_channel| channel == move_channel) {
        parse_move_to_channel_publication(reply, channel, chatroom_id).map(ChatEvent::MoveToChannel)
    } else {
        None
    };
    let Some(event) = event else {
        return Ok(());
    };
    if let ChatEvent::Message(message) = &event
        && !deduper.should_emit(&message.id)
    {
        return Ok(());
    }
    tokio::select! {
        biased;
        _ = wait_for_cancel(cancel) => Err(RunFailure::Cancelled),
        result = events.send(event) => result.map_err(|_| RunFailure::ReceiverClosed),
    }
}

fn parse_raid_publication(reply: &Value, channel: &str, chatroom_id: u64) -> Option<RaidEvent> {
    if reply.pointer("/push/channel")?.as_str()? != channel {
        return None;
    }
    let publication = reply.pointer("/push/pub/data")?;
    if publication.get("event")?.as_str()? != STREAM_HOSTED_EVENT {
        return None;
    }
    let raw_payload = publication.get("data")?;
    let payload: Value = match raw_payload {
        Value::String(raw) => serde_json::from_str(raw).ok()?,
        Value::Object(_) => raw_payload.clone(),
        _ => return None,
    };
    let hoster = payload
        .pointer("/user/username")?
        .as_str()?
        .trim()
        .to_owned();
    if hoster.is_empty() {
        return None;
    }
    let hoster_id = payload.pointer("/user/id").and_then(value_as_u64);
    let viewer_count = payload
        .pointer("/message/numberOfViewers")
        .and_then(value_as_u64)
        .unwrap_or(0);
    let target = payload
        .pointer("/message/host_username")
        .and_then(Value::as_str)
        .or_else(|| {
            payload
                .pointer("/message/hosted_channel/slug")
                .and_then(Value::as_str)
        })
        .or_else(|| {
            payload
                .pointer("/message/hostedChannel/slug")
                .and_then(Value::as_str)
        })
        .map(str::trim)
        .filter(|target| !target.is_empty())
        .map(str::to_owned);
    Some(RaidEvent {
        chatroom_id,
        hoster,
        hoster_id,
        target,
        viewer_count,
    })
}

/// Parses the official auto-move event. The payload's `hosted` object is the
/// target channel; the web client redirects to `hosted.slug` after a short
/// countdown. `StreamHostedEvent` is informational and is not used for this.
fn parse_move_to_channel_publication(
    reply: &Value,
    channel: &str,
    chatroom_id: u64,
) -> Option<MoveToChannelEvent> {
    if reply.pointer("/push/channel")?.as_str()? != channel {
        return None;
    }
    let publication = reply.pointer("/push/pub/data")?;
    if publication.get("event")?.as_str()? != MOVE_TO_SUPPORTED_CHANNEL_EVENT {
        return None;
    }
    let raw_payload = publication.get("data")?;
    let payload: Value = match raw_payload {
        Value::String(raw) => serde_json::from_str(raw).ok()?,
        Value::Object(_) => raw_payload.clone(),
        _ => return None,
    };
    let hosted = payload.get("hosted")?;
    let target_slug = hosted.get("slug")?.as_str()?.trim();
    if target_slug.is_empty() {
        return None;
    }
    let target_username = hosted
        .get("username")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let target_viewers = hosted
        .get("viewers_count")
        .and_then(value_as_u64)
        .unwrap_or(0);
    Some(MoveToChannelEvent {
        chatroom_id,
        target_slug: truncate(target_slug, MAX_CHAT_AUTHOR_BYTES),
        target_username,
        target_viewers,
    })
}

fn parse_chat_publication(reply: &Value, channel: &str, chatroom_id: u64) -> Option<ChatMessage> {
    if reply.pointer("/push/channel")?.as_str()? != channel {
        return None;
    }
    let publication = reply.pointer("/push/pub/data")?;
    if publication.get("event")?.as_str()? != CHAT_MESSAGE_EVENT {
        return None;
    }
    let raw_payload = publication.get("data")?;
    let payload: Value = match raw_payload {
        Value::String(raw) => serde_json::from_str(raw).ok()?,
        Value::Object(_) => raw_payload.clone(),
        _ => return None,
    };
    if value_as_u64(payload.get("chatroom_id")?)? != chatroom_id {
        return None;
    }
    let content = payload.get("content")?.as_str()?;
    let author = payload.pointer("/sender/username")?.as_str()?;
    if content.trim().is_empty() || author.trim().is_empty() {
        return None;
    }
    let id = payload
        .get("id")
        .and_then(value_as_string)
        .map(|value| truncate(&value, MAX_CHAT_ID_BYTES))
        .unwrap_or_default();
    let author_slug = payload
        .pointer("/sender/slug")
        .and_then(Value::as_str)
        .map(|value| truncate(value, MAX_CHAT_AUTHOR_BYTES));
    let badges = parse_badges(payload.pointer("/sender/identity/badges"));
    Some(ChatMessage {
        id,
        author: truncate(author, MAX_CHAT_AUTHOR_BYTES),
        text: truncate(content, MAX_CHAT_TEXT_BYTES),
        timestamp_ms: None,
        author_slug,
        badges,
        reward: None,
    })
}

/// Parses a `RewardRedeemedEvent` from the `chatroom_{id}` subscription.
/// The official web client renders these as `rewards` chat entries with the
/// redeemer, reward title, optional input, and reward background color.
fn parse_reward_publication(reply: &Value, channel: &str, chatroom_id: u64) -> Option<ChatMessage> {
    if reply.pointer("/push/channel")?.as_str()? != channel {
        return None;
    }
    let publication = reply.pointer("/push/pub/data")?;
    if publication.get("event")?.as_str()? != REWARD_REDEEMED_EVENT {
        return None;
    }
    let raw_payload = publication.get("data")?;
    let payload: Value = match raw_payload {
        Value::String(raw) => serde_json::from_str(raw).ok()?,
        Value::Object(_) => raw_payload.clone(),
        _ => return None,
    };
    let title = payload.get("reward_title")?.as_str()?.trim();
    let username = payload.get("username")?.as_str()?.trim();
    if title.is_empty() || username.is_empty() {
        return None;
    }
    let user_input = payload
        .get("user_input")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| truncate(value, MAX_CHAT_TEXT_BYTES));
    let background_color = payload
        .get("reward_background_color")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| valid_hex_color(value))
        .map(str::to_owned);
    let id = payload
        .get("id")
        .and_then(value_as_string)
        .map(|value| truncate(&value, MAX_CHAT_ID_BYTES))
        .unwrap_or_else(|| reward_fallback_id(chatroom_id));
    Some(ChatMessage {
        id,
        author: truncate(username, MAX_CHAT_AUTHOR_BYTES),
        text: user_input.clone().unwrap_or_default(),
        timestamp_ms: None,
        author_slug: None,
        badges: Vec::new(),
        reward: Some(ChatReward {
            title: truncate(title, MAX_CHAT_TEXT_BYTES),
            user_input,
            background_color,
        }),
    })
}

fn valid_hex_color(value: &str) -> bool {
    let bytes = value.as_bytes();
    (bytes.len() == 7 || bytes.len() == 4)
        && bytes[0] == b'#'
        && bytes[1..].iter().all(|byte| byte.is_ascii_hexdigit())
}

fn reward_fallback_id(chatroom_id: u64) -> String {
    static REWARD_SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let sequence = REWARD_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("reward-{chatroom_id}-{sequence}")
}

/// Reads up to four chat badges (`type` + short `text`) from the sender identity.
fn parse_badges(value: Option<&Value>) -> Vec<ChatBadge> {
    let Some(items) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .take(4)
        .filter_map(|item| {
            let kind = item.get("type")?.as_str()?;
            if kind.is_empty() {
                return None;
            }
            let text = item.get("text").and_then(Value::as_str).unwrap_or("");
            let count = item.get("count").and_then(Value::as_u64);
            // Subscriber badges show the month count in the official client.
            let label = if kind == "subscriber" {
                count
                    .map(|count| count.to_string())
                    .unwrap_or_else(|| text.to_owned())
            } else {
                text.to_owned()
            };
            Some(ChatBadge {
                kind: truncate(kind, 32),
                text: truncate(&label, 16),
            })
        })
        .collect()
}

fn value_as_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str()?.parse::<u64>().ok())
}

fn value_as_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn truncate(value: &str, max_bytes: usize) -> String {
    let mut end = value.len().min(max_bytes);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

#[derive(Default)]
struct MessageDeduper {
    ids: HashSet<String>,
    order: VecDeque<String>,
}

impl MessageDeduper {
    fn should_emit(&mut self, id: &str) -> bool {
        if id.is_empty() {
            return true;
        }
        if self.ids.contains(id) {
            return false;
        }
        if self.order.len() >= MAX_DEDUPLICATED_IDS {
            if let Some(oldest) = self.order.pop_front() {
                self.ids.remove(&oldest);
            }
        }
        let id = id.to_owned();
        self.ids.insert(id.clone());
        self.order.push_back(id);
        true
    }
}

async fn send_text_cancelable<S>(
    socket: &mut WebSocketStream<S>,
    text: &str,
    cancel: &mut watch::Receiver<bool>,
) -> Result<(), RunFailure>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    tokio::select! {
        biased;
        _ = wait_for_cancel(cancel) => Err(RunFailure::Cancelled),
        result = socket.send(Message::Text(text.to_owned().into())) => {
            result.map_err(|_| RunFailure::Transport)
        }
    }
}

async fn send_pong_cancelable<S>(
    socket: &mut WebSocketStream<S>,
    payload: tokio_tungstenite::tungstenite::Bytes,
    cancel: &mut watch::Receiver<bool>,
) -> Result<(), RunFailure>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    tokio::select! {
        biased;
        _ = wait_for_cancel(cancel) => Err(RunFailure::Cancelled),
        result = socket.send(Message::Pong(payload)) => {
            result.map_err(|_| RunFailure::Transport)
        }
    }
}

async fn send_ping_cancelable<S>(
    socket: &mut WebSocketStream<S>,
    payload: Vec<u8>,
    cancel: &mut watch::Receiver<bool>,
) -> Result<(), RunFailure>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    tokio::select! {
        biased;
        _ = wait_for_cancel(cancel) => Err(RunFailure::Cancelled),
        result = socket.send(Message::Ping(payload.into())) => {
            result.map_err(|_| RunFailure::Transport)
        }
    }
}

async fn wait_for_cancel(cancel: &mut watch::Receiver<bool>) {
    loop {
        if *cancel.borrow() {
            return;
        }
        if cancel.changed().await.is_err() {
            return;
        }
    }
}

fn is_cancelled(cancel: &watch::Receiver<bool>) -> bool {
    *cancel.borrow()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{io::duplex, sync::watch};
    use tokio_tungstenite::{accept_async, client_async};

    #[test]
    fn parses_only_the_observed_room_chat_event() {
        let payload = json!({
            "chatroom_id": 42,
            "content": "fixture body",
            "created_at": "2026-01-01T00:00:00Z",
            "id": "fixture-message-id",
            "metadata": {},
            "sender": {"id": 7, "identity": {}, "slug": "fixture", "username": "tester"},
            "type": "message"
        });
        let publication = json!({
            "push": {
                "channel": "chatrooms.42.v2",
                "pub": {"data": {"event": CHAT_MESSAGE_EVENT, "data": payload.to_string()}}
            }
        });
        let message = parse_chat_publication(&publication, "chatrooms.42.v2", 42).unwrap();
        assert_eq!(message.id, "fixture-message-id");
        assert_eq!(message.author, "tester");
        assert_eq!(message.text, "fixture body");
        assert_eq!(message.timestamp_ms, None);

        let mut unknown = publication.clone();
        unknown["push"]["pub"]["data"]["event"] = json!(r"App\Events\UnknownEvent");
        assert!(parse_chat_publication(&unknown, "chatrooms.42.v2", 42).is_none());
        assert!(parse_chat_publication(&publication, "chatrooms.99.v2", 99).is_none());
    }

    #[test]
    fn parses_stream_hosted_event_with_target_and_viewer_count() {
        let publication = json!({
            "push": {
                "channel": "chatrooms.42",
                "pub": {"data": {"event": STREAM_HOSTED_EVENT, "data": json!({
                    "user": {"id": 7, "username": "raider"},
                    "message": {"numberOfViewers": 123, "host_username": "target"}
                }).to_string()}}
            }
        });
        let raid = parse_raid_publication(&publication, "chatrooms.42", 42).unwrap();
        assert_eq!(raid.chatroom_id, 42);
        assert_eq!(raid.hoster, "raider");
        assert_eq!(raid.hoster_id, Some(7));
        assert_eq!(raid.target.as_deref(), Some("target"));
        assert_eq!(raid.viewer_count, 123);
        assert!(parse_raid_publication(&publication, "chatrooms.42.v2", 42).is_none());
    }

    #[test]
    fn deduper_is_bounded_and_skips_reconnect_duplicates() {
        let mut deduper = MessageDeduper::default();
        assert!(deduper.should_emit("same-id"));
        assert!(!deduper.should_emit("same-id"));
        assert!(deduper.should_emit(""));
        for index in 0..=MAX_DEDUPLICATED_IDS {
            assert!(deduper.should_emit(&format!("id-{index}")));
        }
        assert!(deduper.order.len() <= MAX_DEDUPLICATED_IDS);
    }

    #[test]
    fn reconnect_delay_is_exponential_with_a_sixty_second_ceiling() {
        assert_eq!(reconnect_delay(1), Duration::from_secs(1));
        assert_eq!(reconnect_delay(2), Duration::from_secs(2));
        assert_eq!(reconnect_delay(3), Duration::from_secs(4));
        assert_eq!(reconnect_delay(100), MAX_RECONNECT_DELAY);
        assert_eq!(reconnect_delay(u32::MAX), MAX_RECONNECT_DELAY);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_interrupts_reconnect_backoff() {
        let (cancel_sender, mut cancel) = watch::channel(false);
        let (started_sender, started_receiver) = tokio::sync::oneshot::channel();
        let retry = tokio::spawn(async move {
            let _ = started_sender.send(());
            wait_for_retry(Duration::from_secs(60), &mut cancel).await
        });
        started_receiver.await.unwrap();
        cancel_sender.send_replace(true);

        let should_retry = time::timeout(Duration::from_secs(1), retry)
            .await
            .expect("cancel should end backoff promptly")
            .expect("retry task should not panic");
        assert!(!should_retry);
    }

    #[test]
    fn stream_notification_publication_parses_live_and_end_events() {
        let subscriptions = vec![(8448056, "220ninimaru".to_owned())];
        let live = json!({
            "push": {
                "channel": "channel.8448056",
                "pub": {"data": {"event": STREAMER_IS_LIVE_EVENT}}
            }
        });
        assert_eq!(
            parse_stream_notification_publication(&live, &subscriptions),
            Some(StreamNotificationEvent::StreamerIsLive {
                slug: "220ninimaru".to_owned(),
                channel_id: 8448056
            })
        );
        let ended = json!({
            "push": {
                "channel": "channel.8448056",
                "pub": {"data": {"event": STOP_STREAM_BROADCAST_EVENT}}
            }
        });
        assert_eq!(
            parse_stream_notification_publication(&ended, &subscriptions),
            Some(StreamNotificationEvent::StreamEnded {
                slug: "220ninimaru".to_owned(),
                channel_id: 8448056
            })
        );
        assert!(parse_stream_notification_publication(&live, &[]).is_none());
    }

    #[test]
    fn move_to_channel_publication_parses_the_hosted_target() {
        let reply = json!({
            "push": {
                "channel": "channel.8448056",
                "pub": {
                    "data": {
                        "event": MOVE_TO_SUPPORTED_CHANNEL_EVENT,
                        "data": {
                            "hosted": {
                                "slug": "kurosawa2525",
                                "username": "kurosawa2525",
                                "viewers_count": 1234
                            }
                        }
                    }
                }
            }
        });
        let event = parse_move_to_channel_publication(&reply, "channel.8448056", 8331642).unwrap();
        assert_eq!(event.chatroom_id, 8331642);
        assert_eq!(event.target_slug, "kurosawa2525");
        assert_eq!(event.target_username.as_deref(), Some("kurosawa2525"));
        assert_eq!(event.target_viewers, 1234);
        assert!(parse_move_to_channel_publication(&reply, "chatrooms.42", 42).is_none());
    }

    #[test]
    fn reward_publication_parses_the_documented_fields() {
        let reply = json!({
            "push": {
                "channel": "chatroom_42",
                "pub": {
                    "data": {
                        "event": "RewardRedeemedEvent",
                        "data": {
                            "id": "redemption-1",
                            "reward_title": "Hydrate",
                            "username": "viewer",
                            "user_input": "drink water",
                            "reward_background_color": "#53FC18"
                        }
                    }
                }
            }
        });
        let message = parse_reward_publication(&reply, "chatroom_42", 42).unwrap();
        assert_eq!(message.id, "redemption-1");
        assert_eq!(message.author, "viewer");
        assert_eq!(message.text, "drink water");
        let reward = message.reward.unwrap();
        assert_eq!(reward.title, "Hydrate");
        assert_eq!(reward.user_input.as_deref(), Some("drink water"));
        assert_eq!(reward.background_color.as_deref(), Some("#53FC18"));

        let invalid_color = json!({
            "push": {
                "channel": "chatroom_42",
                "pub": {
                    "data": {
                        "event": "RewardRedeemedEvent",
                        "data": {
                            "reward_title": "Hydrate",
                            "username": "viewer",
                            "reward_background_color": "green"
                        }
                    }
                }
            }
        });
        let message = parse_reward_publication(&invalid_color, "chatroom_42", 42).unwrap();
        assert!(message.reward.unwrap().background_color.is_none());
        assert!(message.id.starts_with("reward-42-"));

        let missing_title = json!({
            "push": {
                "channel": "chatroom_42",
                "pub": {
                    "data": {
                        "event": "RewardRedeemedEvent",
                        "data": {"username": "viewer"}
                    }
                }
            }
        });
        assert!(parse_reward_publication(&missing_title, "chatroom_42", 42).is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_unblocks_publication_when_event_queue_is_full() {
        let (events_tx, mut events_rx) = mpsc::channel(1);
        events_tx
            .send(ChatEvent::Message(ChatMessage {
                id: "already-queued".to_owned(),
                author: "fixture".to_owned(),
                text: "kept".to_owned(),
                timestamp_ms: None,
                author_slug: None,
                badges: Vec::new(),
                reward: None,
            }))
            .await
            .unwrap();
        let publication = json!({
            "push": {
                "channel": "chatrooms.42.v2",
                "pub": {
                    "data": {
                        "event": CHAT_MESSAGE_EVENT,
                        "data": json!({
                            "chatroom_id": 42,
                            "content": "blocked fixture",
                            "id": "blocked-message",
                            "sender": {"username": "fixture"}
                        }).to_string()
                    }
                }
            }
        });
        let (cancel_sender, mut cancel) = watch::channel(false);
        let mut deduper = MessageDeduper::default();
        let (started_sender, started_receiver) = tokio::sync::oneshot::channel();
        let blocked_send = tokio::spawn(async move {
            let _ = started_sender.send(());
            dispatch_publication(
                &publication,
                42,
                "chatrooms.42.v2",
                "chatrooms.42",
                "chatroom_42",
                None,
                &events_tx,
                &mut cancel,
                &mut deduper,
            )
            .await
        });

        started_receiver.await.unwrap();
        assert!(
            !blocked_send.is_finished(),
            "full bounded queue should hold the publication send"
        );
        cancel_sender.send_replace(true);
        let result = time::timeout(Duration::from_secs(1), blocked_send)
            .await
            .expect("blocked send should observe cancellation")
            .expect("publication task should not panic");
        assert!(matches!(result, Err(RunFailure::Cancelled)));
        assert_eq!(events_rx.len(), 1, "the queued event must remain intact");
        let ChatEvent::Message(message) = events_rx.recv().await.unwrap() else {
            panic!("expected a chat message");
        };
        assert_eq!(message.id, "already-queued");
        assert!(
            events_rx.try_recv().is_err(),
            "cancelled event must not leak in"
        );
    }

    #[test]
    fn room_reservation_rejects_duplicates_until_session_finishes() {
        let first = RoomReservation::try_new(9_876_543).unwrap();
        assert!(RoomReservation::try_new(9_876_543).is_none());
        drop(first);
        assert!(RoomReservation::try_new(9_876_543).is_some());
    }

    #[tokio::test]
    async fn local_websocket_checks_connect_subscribe_heartbeat_and_publication() {
        let (client_stream, server_stream) = duplex(64 * 1024);
        let server = tokio::spawn(async move {
            let mut socket = accept_async(server_stream).await.unwrap();

            let connect = socket.next().await.unwrap().unwrap();
            let Message::Text(connect) = connect else {
                panic!("expected connect command");
            };
            let connect: Value = serde_json::from_str(&connect).unwrap();
            assert_eq!(connect["id"], 1);
            assert_eq!(connect["connect"]["token"], "local-fixture-token");

            socket
                .send(Message::Ping(vec![1, 2, 3].into()))
                .await
                .unwrap();
            let pong = socket.next().await.unwrap().unwrap();
            assert!(matches!(pong, Message::Pong(payload) if payload.as_ref() == [1, 2, 3]));
            socket
                .send(Message::Text(
                    json!({"id":1,"connect":{}}).to_string().into(),
                ))
                .await
                .unwrap();

            for (expected_id, expected_channel) in [
                (2_u64, "chatrooms.42.v2"),
                (3_u64, "chatrooms.42"),
                (4_u64, "chatroom_42"),
            ] {
                let subscribe = socket.next().await.unwrap().unwrap();
                let Message::Text(subscribe) = subscribe else {
                    panic!("expected subscribe command");
                };
                let subscribe: Value = serde_json::from_str(&subscribe).unwrap();
                assert_eq!(subscribe["id"], expected_id);
                assert_eq!(subscribe["subscribe"]["channel"], expected_channel);
                socket
                    .send(Message::Text(
                        json!({"id":expected_id,"subscribe":{}}).to_string().into(),
                    ))
                    .await
                    .unwrap();
            }

            let payload = json!({
                "chatroom_id": 42,
                "content": "local fixture body",
                "created_at": "2026-01-01T00:00:00Z",
                "id": "local-fixture-message",
                "metadata": {},
                "sender": {"id": 7, "identity": {}, "slug": "fixture", "username": "local-user"},
                "type": "message"
            });
            let publication = json!({
                "push": {
                    "channel": "chatrooms.42.v2",
                    "pub": {"data": {"event": CHAT_MESSAGE_EVENT, "data": payload.to_string()}}
                }
            });
            socket
                .send(Message::Text(publication.to_string().into()))
                .await
                .unwrap();
        });

        let (mut socket, _) = client_async("ws://localhost/ws", client_stream)
            .await
            .unwrap();
        let token = Zeroizing::new(String::from("local-fixture-token"));
        send_connect_command(&mut socket, &token).await.unwrap();
        wait_for_connect_ack(&mut socket).await.unwrap();

        let (cancel_sender, mut cancel) = watch::channel(false);
        let (sender, mut events) = mpsc::channel(1);
        let mut deduper = MessageDeduper::default();
        for (id, channel) in [
            (2_u64, "chatrooms.42.v2"),
            (3_u64, "chatrooms.42"),
            (4_u64, "chatroom_42"),
        ] {
            socket
                .send(Message::Text(
                    json!({"id":id,"subscribe":{"channel":channel}})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
        }
        wait_for_subscribe_acks(
            &mut socket,
            42,
            "chatrooms.42.v2",
            "chatrooms.42",
            "chatroom_42",
            None,
            &sender,
            &mut cancel,
            &mut deduper,
        )
        .await
        .unwrap();
        let frame = socket.next().await.unwrap().unwrap();
        let Message::Text(text) = frame else {
            panic!("expected publication frame");
        };
        dispatch_text(
            &mut socket,
            &text,
            42,
            "chatrooms.42.v2",
            "chatrooms.42",
            "chatroom_42",
            None,
            &sender,
            &mut cancel,
            &mut deduper,
        )
        .await
        .unwrap();
        let ChatEvent::Message(message) = events.recv().await.unwrap() else {
            panic!("expected a chat message");
        };
        assert_eq!(message.id, "local-fixture-message");
        assert_eq!(message.author, "local-user");
        assert_eq!(message.text, "local fixture body");
        drop(cancel_sender);
        server.await.unwrap();
    }

    async fn send_connect_command<S>(
        socket: &mut WebSocketStream<S>,
        token: &str,
    ) -> Result<(), RunFailure>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let command = json!({
            "id": 1,
            "connect": {"token": token, "name": "kick-native", "version": "0.1"}
        });
        socket
            .send(Message::Text(command.to_string().into()))
            .await
            .map_err(|_| RunFailure::Transport)
    }
}
