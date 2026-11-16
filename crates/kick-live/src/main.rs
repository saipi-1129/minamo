#![cfg(feature = "cli")]

use std::collections::HashSet;
use std::process::ExitCode;
use std::{env, io::Read};

use kick_live::{
    AccessToken, KickApiError, KickClient, LiveStreamsQuery,
    emotes::{ChatSegment, parse_chat_segments},
};

const DEFAULT_LIMIT: usize = 5;
const MAX_LIMIT: usize = 25;

fn main() -> ExitCode {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    if arguments
        .first()
        .is_some_and(|value| value == "emote-catalog-check")
    {
        if !(2..=3).contains(&arguments.len()) {
            eprintln!("usage: kick-live emote-catalog-check <public-slug> [emote-name]");
            return ExitCode::FAILURE;
        }
        return run_emote_catalog_check(&arguments[1], arguments.get(2).map(String::as_str));
    }
    if arguments
        .first()
        .is_some_and(|value| value == "emote-chat-check")
    {
        if !(3..=4).contains(&arguments.len()) {
            eprintln!(
                "usage: kick-live emote-chat-check <public-slug> <emote-name> [listen-seconds]"
            );
            return ExitCode::FAILURE;
        }
        let listen_seconds = arguments
            .get(3)
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|seconds| (1..=300).contains(seconds))
            .unwrap_or(15);
        return run_emote_chat_check(&arguments[1], &arguments[2], listen_seconds);
    }
    if arguments.first().is_some_and(|value| value == "move-probe") {
        if !(2..=3).contains(&arguments.len()) {
            eprintln!("usage: kick-live move-probe <public-slug> [seconds]");
            return ExitCode::FAILURE;
        }
        let seconds = arguments
            .get(2)
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|seconds| (1..=300).contains(seconds))
            .unwrap_or(60);
        return run_move_probe(&arguments[1], seconds);
    }
    if arguments
        .first()
        .is_some_and(|value| value == "reward-probe")
    {
        if !(2..=3).contains(&arguments.len()) {
            eprintln!("usage: kick-live reward-probe <public-slug> [seconds]");
            return ExitCode::FAILURE;
        }
        let seconds = arguments
            .get(2)
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|seconds| (1..=300).contains(seconds))
            .unwrap_or(60);
        return run_reward_probe(&arguments[1], seconds);
    }
    if arguments
        .first()
        .is_some_and(|value| value == "clips-page-check")
    {
        if arguments.len() != 2 {
            eprintln!("usage: kick-live clips-page-check <public-slug>");
            return ExitCode::FAILURE;
        }
        return run_clips_page_check(&arguments[1]);
    }
    if arguments
        .first()
        .is_some_and(|value| value == "internal-search")
    {
        if arguments.len() != 2 {
            eprintln!("usage: kick-live internal-search <query>");
            return ExitCode::FAILURE;
        }
        return run_internal_search(&arguments[1]);
    }
    if arguments
        .first()
        .is_some_and(|argument| argument == "internal-probe")
    {
        return run_internal_probe(&arguments[1..]);
    }
    if arguments
        .first()
        .is_some_and(|argument| argument == "live-history-check")
    {
        return run_live_history_check(&arguments[1..]);
    }

    if arguments
        .first()
        .is_some_and(|v| v == "live-playlist-check")
    {
        if arguments.len() != 1 {
            return ExitCode::FAILURE;
        }
        return run_live_playlist_check(false);
    }

    if arguments.len() == 1 && arguments[0] == "native-playback-check" {
        return run_live_playlist_check(true);
    }
    if arguments.len() == 1 && arguments[0] == "realtime-config-check" {
        return run_realtime_config_check(0);
    }
    if arguments.len() == 1 && arguments[0] == "realtime-handshake-check" {
        return run_realtime_config_check(1);
    }
    if arguments.len() == 1 && arguments[0] == "chat-config-check" {
        return run_realtime_config_check(2);
    }
    if arguments.len() == 1 && arguments[0] == "chat-catalog-config-check" {
        return run_realtime_config_check(3);
    }
    if arguments.len() == 1 && arguments[0] == "realtime-room-check" {
        return run_realtime_config_check(4);
    }
    if arguments.len() == 1 && arguments[0] == "live-session-check" {
        return run_realtime_config_check(5);
    }
    match run_public_api(&arguments) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("kick-live: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run_emote_catalog_check(slug: &str, requested_name: Option<&str>) -> ExitCode {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        eprintln!("emote catalog check: runtime initialization failed");
        return ExitCode::FAILURE;
    };
    let Ok(client) = KickClient::unauthenticated_for_probe() else {
        eprintln!("emote catalog check: client initialization failed");
        return ExitCode::FAILURE;
    };

    match runtime.block_on(client.anonymous_channel_emotes(slug)) {
        Ok(groups) => {
            let emote_count = groups.iter().fold(0usize, |total, group| {
                total.saturating_add(group.emotes.len())
            });
            println!("emote catalog groups: {}", groups.len());
            println!("emote records: {emote_count}");
            if let Some(requested_name) = requested_name {
                let matches = groups
                    .iter()
                    .flat_map(|group| &group.emotes)
                    .filter(|emote| emote.name.eq_ignore_ascii_case(requested_name))
                    .collect::<Vec<_>>();
                println!("exact-name matches: {}", matches.len());
                for emote in matches {
                    println!("emote ID: {}", emote.id);
                }
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("emote catalog check failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run_emote_chat_check(slug: &str, requested_name: &str, listen_seconds: u64) -> ExitCode {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        eprintln!("emote chat check: runtime initialization failed");
        return ExitCode::FAILURE;
    };
    let Ok(client) = KickClient::unauthenticated_for_probe() else {
        eprintln!("emote chat check: client initialization failed");
        return ExitCode::FAILURE;
    };

    let mut random = [0u8; 16];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut random))
        .is_err()
    {
        eprintln!("emote chat check: could not generate a local client ID");
        return ExitCode::FAILURE;
    }
    let client_id = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();

    let result = runtime.block_on(async {
        let channel = client.anonymous_channel(slug).await?;
        let Some(channel_id) = channel.id else {
            return Err(KickApiError::InvalidJson);
        };
        if !channel.is_live() {
            return Err(KickApiError::InvalidJson);
        }
        let Some(chatroom_id) = channel.chatroom_id else {
            return Err(KickApiError::InvalidJson);
        };
        let messages = client.anonymous_recent_messages(channel_id).await?;
        let mut history_ids = Vec::new();
        let mut history_name_only = 0usize;
        let mut history_marker_ids = 0usize;
        let mut history_marker_name_only = 0usize;
        let mut history_sample_id = None;
        let collect_matches = |content: &str,
                               ids: &mut Vec<String>,
                               name_only: &mut usize,
                               marker_ids: &mut usize,
                               marker_name_only: &mut usize,
                               sample_id: &mut Option<String>| {
            for segment in parse_chat_segments(content) {
                if let ChatSegment::Emote { id, name } = segment {
                    if let Some(id) = id {
                        *marker_ids = marker_ids.saturating_add(1);
                        sample_id.get_or_insert_with(|| id.clone());
                        if name.eq_ignore_ascii_case(requested_name) {
                            ids.push(id);
                        }
                    } else {
                        *marker_name_only = marker_name_only.saturating_add(1);
                        if name.eq_ignore_ascii_case(requested_name) {
                            *name_only = name_only.saturating_add(1);
                        }
                    }
                }
            }
        };
        for content in messages
            .messages
            .iter()
            .filter_map(|message| message.content.as_deref())
        {
            collect_matches(
                content,
                &mut history_ids,
                &mut history_name_only,
                &mut history_marker_ids,
                &mut history_marker_name_only,
                &mut history_sample_id,
            );
        }

        let mut session = client
            .run_chat(&client_id, chatroom_id, None)
            .await
            .map_err(|_| KickApiError::Transport(kick_live::TransportFailure::Other))?;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(listen_seconds);
        let mut realtime_messages = 0usize;
        let mut realtime_ids = Vec::new();
        let mut realtime_name_only = 0usize;
        let mut realtime_marker_ids = 0usize;
        let mut realtime_marker_name_only = 0usize;
        let mut realtime_sample_id = None;
        while let Ok(Some(event)) = tokio::time::timeout_at(deadline, session.recv()).await {
            let kick_live::realtime::ChatEvent::Message(message) = event else {
                continue;
            };
            realtime_messages = realtime_messages.saturating_add(1);
            collect_matches(
                &message.text,
                &mut realtime_ids,
                &mut realtime_name_only,
                &mut realtime_marker_ids,
                &mut realtime_marker_name_only,
                &mut realtime_sample_id,
            );
        }
        let _ = tokio::time::timeout(std::time::Duration::from_secs(3), session.stop()).await;
        Ok((
            messages.messages.len(),
            history_ids,
            history_name_only,
            history_marker_ids,
            history_marker_name_only,
            history_sample_id,
            realtime_messages,
            realtime_ids,
            realtime_name_only,
            realtime_marker_ids,
            realtime_marker_name_only,
            realtime_sample_id,
        ))
    });

    match result {
        Ok((
            history_count,
            history_ids,
            history_name_only,
            history_marker_ids,
            history_marker_name_only,
            history_sample_id,
            realtime_count,
            realtime_ids,
            realtime_name_only,
            realtime_marker_ids,
            realtime_marker_name_only,
            realtime_sample_id,
        )) => {
            println!("recent history records: {history_count}");
            println!("history ID-bearing target markers: {}", history_ids.len());
            for id in history_ids {
                println!("emote ID: {id}");
            }
            println!("history name-only target tokens: {history_name_only}");
            println!("history ID-bearing emote markers: {history_marker_ids}");
            println!("history name-only emote markers: {history_marker_name_only}");
            if let Some(id) = history_sample_id {
                println!("sample history emote ID: {id}");
            }
            println!("realtime messages observed: {realtime_count}");
            println!("realtime ID-bearing target markers: {}", realtime_ids.len());
            for id in realtime_ids {
                println!("emote ID: {id}");
            }
            println!("realtime name-only target tokens: {realtime_name_only}");
            println!("realtime ID-bearing emote markers: {realtime_marker_ids}");
            println!("realtime name-only emote markers: {realtime_marker_name_only}");
            if let Some(id) = realtime_sample_id {
                println!("sample realtime emote ID: {id}");
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("emote chat check failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run_internal_search(query: &str) -> ExitCode {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return ExitCode::FAILURE;
    };
    let Ok(client) = KickClient::unauthenticated_for_probe() else {
        return ExitCode::FAILURE;
    };
    match runtime.block_on(client.anonymous_search(query)) {
        Ok(channels) => {
            println!(
                "{}",
                serde_json::json!({
                    "channels": channels.len(),
                    "channels_with_slug": channels.iter().filter(|item| !item.channel_slug.is_empty()).count(),
                    "channels_with_name": channels.iter().filter(|item| !item.channel_name.is_empty()).count(),
                })
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("internal search failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run_live_playlist_check(native: bool) -> ExitCode {
    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build();
    let Ok(runtime) = result else {
        return ExitCode::FAILURE;
    };
    let result = runtime.block_on(async {
        let client =
            KickClient::unauthenticated_for_probe().map_err(|_| "client initialization failed")?;
        let catalog = client
            .anonymous_catalog()
            .await
            .map_err(|_| "catalog request failed")?;
        let slug = catalog
            .items
            .iter()
            .find_map(|item| item.channel_slug.clone())
            .ok_or("no channel")?;
        drop(catalog);
        let channel = client
            .anonymous_channel(&slug)
            .await
            .map_err(|_| "channel request failed")?;
        if !channel.is_live() {
            return Err("selected channel is offline");
        }
        let source = channel.playback_source.ok_or("no active playback source")?;
        if native {
            use std::io::Write;
            use std::process::{Command, Stdio};
            let mut child = Command::new(
                env::var("KICK_NATIVE_PROBE")
                    .map_err(|_| "set KICK_NATIVE_PROBE to the local probe executable")?,
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::inherit())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| "native probe launch failed")?;
            let mut input = child.stdin.take().ok_or("native probe stdin unavailable")?;
            writeln!(input, "{}", source.url).map_err(|_| "native probe input failed")?;
            drop(input);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            loop {
                if let Some(status) = child.try_wait().map_err(|_| "native probe wait failed")? {
                    return if status.success() {
                        Ok(())
                    } else {
                        Err("native playback probe failed")
                    };
                }
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("native playback probe timed out");
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
        let http = reqwest::Client::builder()
            .user_agent("kick-native/0.1")
            .timeout(std::time::Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| "HTTP initialization failed")?;
        let mut response = http
            .get(&source.url)
            .send()
            .await
            .map_err(|_| "playlist request failed")?;
        println!("playlist HTTP status: {}", response.status().as_u16());
        if !response.status().is_success() {
            return Err("playlist rejected");
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| "playlist body failed")? {
            if bytes.len().saturating_add(chunk.len()) > 1024 * 1024 {
                return Err("playlist exceeds limit");
            }
            bytes.extend_from_slice(&chunk);
        }
        let valid = bytes.starts_with(b"#EXTM3U");
        println!("playlist bytes: {}", bytes.len());
        println!("HLS marker verified: {valid}");
        if valid {
            let qualities = kick_live::hls::parse_master(
                &source.url,
                std::str::from_utf8(&bytes).map_err(|_| "invalid UTF-8 playlist")?,
            )
            .map_err(|_| "quality inspection failed")?;
            println!("quality variants parsed: {}", qualities.len());
            for quality in qualities {
                println!(
                    "quality resolution: {:?}; bandwidth: {}",
                    quality.resolution, quality.bandwidth
                );
            }
        }
        println!("native playback verification: not performed");
        if valid {
            Ok(())
        } else {
            Err("not an HLS playlist")
        }
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("live-playlist-check: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run_realtime_config_check(mode: u8) -> ExitCode {
    use std::io::Read;
    eprintln!("realtime probe: generating local client ID");
    let mut random = [0u8; 8];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut random))
        .is_err()
    {
        return ExitCode::FAILURE;
    }
    let client_id = random
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return ExitCode::FAILURE;
    };
    let Ok(client) = KickClient::unauthenticated_for_probe() else {
        return ExitCode::FAILURE;
    };
    if mode == 5 {
        let result = runtime.block_on(async {
            eprintln!("realtime probe: fetching catalog");
            let catalog = client
                .anonymous_catalog()
                .await
                .map_err(|_| "catalog request failed")?;
            let slug = catalog
                .items
                .iter()
                .find_map(|i| i.channel_slug.as_deref())
                .ok_or("no channel")?;
            let channel = client
                .anonymous_channel(slug)
                .await
                .map_err(|_| "channel request failed")?;
            eprintln!("realtime probe: channel metadata received");
            if !channel.is_live() {
                return Err("selected channel offline");
            }
            let room = channel.chatroom_id.ok_or("no chatroom ID")?;
            let mut session = client
                .run_chat(&client_id, room, None)
                .await
                .map_err(|_| "session start failed")?;
            let duplicate_rejected = matches!(
                client.run_chat(&client_id, room, None).await,
                Err(kick_live::realtime::ChatStartError::AlreadyRunning)
            );
            println!("duplicate session rejected: {duplicate_rejected}");
            let mut counts = [0usize; 2];
            for phase in 0..2 {
                if phase == 1 {
                    session = client
                        .run_chat(&client_id, room, None)
                        .await
                        .map_err(|_| "session restart failed")?;
                }
                let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(25);
                while counts[phase] < 5 {
                    match tokio::time::timeout_at(deadline, session.recv()).await {
                        Ok(Some(kick_live::realtime::ChatEvent::Message(message)))
                            if !message.author.is_empty() && !message.text.is_empty() =>
                        {
                            counts[phase] += 1
                        }
                        Ok(Some(_)) => {}
                        _ => break,
                    }
                }
                println!("session {} parsed messages: {}", phase + 1, counts[phase]);
                tokio::time::timeout(std::time::Duration::from_secs(3), session.stop())
                    .await
                    .map_err(|_| "session stop timed out")?;
                let stopped = session.status() == kick_live::realtime::ChatStatus::Stopped;
                println!("session {} stopped: {stopped}", phase + 1);
                if !stopped {
                    return Err("session did not stop");
                }
            }
            if duplicate_rejected && counts.iter().all(|n| *n >= 2) {
                Ok(())
            } else {
                Err("insufficient session verification")
            }
        });
        return match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("live-session-check: {e}");
                ExitCode::FAILURE
            }
        };
    }
    if mode == 4 {
        let result = runtime.block_on(async {
            let catalog = client.anonymous_catalog().await?;
            let slug = catalog
                .items
                .iter()
                .find_map(|r| r.channel_slug.as_deref())
                .ok_or(KickApiError::InvalidChannelSlug)?;
            let channel = client.anonymous_channel(slug).await?;
            if !channel.is_live() {
                return Err(KickApiError::InvalidQuery("selected channel is offline"));
            }
            client
                .anonymous_room_probe(
                    &client_id,
                    channel.chatroom_id.ok_or(KickApiError::InvalidChannelId)?,
                )
                .await
        });
        return match result {
            Ok(stats) => {
                println!("candidate room subscribed: {}", stats.subscribed);
                println!("publications received: {}", stats.publications);
                println!("messages with username and body: {}", stats.messages);
                println!("subscription error code: {:?}", stats.error_code);
                if stats.subscribed && stats.messages >= 2 {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                }
            }
            Err(error) => {
                eprintln!("realtime room probe: {error}");
                ExitCode::FAILURE
            }
        };
    }
    if mode == 1 {
        return match runtime.block_on(client.anonymous_realtime_handshake(&client_id)) {
            Ok(connected) => {
                println!("Centrifugo handshake accepted: {connected}");
                if connected {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                }
            }
            Err(error) => {
                eprintln!("realtime handshake: {error}");
                ExitCode::FAILURE
            }
        };
    }
    let response = if mode == 2 || mode == 3 {
        runtime.block_on(async {
            let catalog = client.anonymous_catalog().await?;
            let candidate_id = catalog
                .items
                .iter()
                .find(|i| i.channel_slug.is_some())
                .and_then(|i| i.channel_id.clone());
            let slug = catalog
                .items
                .iter()
                .find_map(|i| i.channel_slug.as_deref())
                .ok_or(KickApiError::InvalidChannelSlug)?;
            let channel = client.anonymous_channel(slug).await?;
            if !channel.is_live() {
                return Err(KickApiError::InvalidQuery("selected channel is offline"));
            }
            let id = if mode == 3 {
                candidate_id.ok_or(KickApiError::InvalidChannelId)?
            } else {
                channel
                    .id
                    .ok_or(KickApiError::InvalidChannelId)?
                    .to_string()
            };
            client.anonymous_chat_realtime_config(&client_id, &id).await
        })
    } else {
        runtime.block_on(client.anonymous_realtime_config(&client_id))
    };
    match response {
        Ok(config) => {
            println!("realtime connections: {}", config.connections.len());
            println!(
                "centrifugo configuration present: {}",
                config
                    .connections
                    .iter()
                    .any(|c| c.provider == "centrifugo")
            );
            println!("WebSocket receive verification: not performed");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("realtime configuration: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run_public_api(arguments: &[String]) -> Result<(), KickApiError> {
    let limit = parse_limit(arguments.iter().cloned())?;
    let token = match env::var("KICK_ACCESS_TOKEN") {
        Ok(value) => Some(AccessToken::new(value)?),
        Err(env::VarError::NotPresent) => None,
        Err(env::VarError::NotUnicode(_)) => return Err(KickApiError::InvalidAccessToken),
    };
    eprintln!(
        "auth token: {}",
        if token.is_some() {
            "present"
        } else {
            "not set; probing auth requirement"
        }
    );

    let client = match token {
        Some(token) => KickClient::new(token)?,
        None => KickClient::unauthenticated_for_probe()?,
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| KickApiError::Transport(kick_live::TransportFailure::Other))?;
    let page = runtime.block_on(client.live_streams(&LiveStreamsQuery {
        limit,
        ..LiveStreamsQuery::default()
    }))?;

    println!("verified live stream records: {}", page.items.len());
    println!("next page: {}", page.next_cursor.is_some());
    for stream in &page.items {
        println!(
            "{:?} | {:?} | {:?} | {} viewers | {:?}",
            stream.channel.slug,
            stream.broadcaster_user.username,
            stream.title,
            stream.viewer_count,
            stream
                .category
                .as_ref()
                .map(|category| category.name.as_str())
                .unwrap_or("uncategorized")
        );
    }
    Ok(())
}

fn run_move_probe(slug: &str, seconds: u64) -> ExitCode {
    let client = match KickClient::unauthenticated_for_probe() {
        Ok(client) => client,
        Err(error) => {
            eprintln!("kick-live move-probe: {error}");
            return ExitCode::FAILURE;
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => {
            eprintln!("kick-live move-probe: could not initialize async runtime");
            return ExitCode::FAILURE;
        }
    };
    let channel = match runtime.block_on(client.anonymous_channel(slug)) {
        Ok(channel) => channel,
        Err(error) => {
            eprintln!("kick-live move-probe: channel lookup failed: {error}");
            return ExitCode::FAILURE;
        }
    };
    let Some(channel_id) = channel.id else {
        eprintln!("kick-live move-probe: channel has no canonical id");
        return ExitCode::FAILURE;
    };
    let client_id = format!("kick-native-move-probe-{}", std::process::id());
    let stats = match runtime.block_on(client.anonymous_move_probe(&client_id, channel_id, seconds))
    {
        Ok(stats) => stats,
        Err(error) => {
            eprintln!("kick-live move-probe: subscription failed: {error}");
            return ExitCode::FAILURE;
        }
    };
    println!("subscribed: {}", stats.subscribed);
    println!("publications: {}", stats.publications);
    println!("move events: {}", stats.move_events);
    println!("with hosted.slug: {}", stats.with_hosted_slug);
    println!("with hosted.username: {}", stats.with_hosted_username);
    println!("with hosted.viewers_count: {}", stats.with_viewers_count);
    println!("target slugs: {}", stats.target_slugs.join(", "));
    println!("event names: {}", stats.event_names.join(", "));
    ExitCode::SUCCESS
}

fn run_reward_probe(slug: &str, seconds: u64) -> ExitCode {
    let client = match KickClient::unauthenticated_for_probe() {
        Ok(client) => client,
        Err(error) => {
            eprintln!("kick-live reward-probe: {error}");
            return ExitCode::FAILURE;
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => {
            eprintln!("kick-live reward-probe: could not initialize async runtime");
            return ExitCode::FAILURE;
        }
    };
    let channel = match runtime.block_on(client.anonymous_channel(slug)) {
        Ok(channel) => channel,
        Err(error) => {
            eprintln!("kick-live reward-probe: channel lookup failed: {error}");
            return ExitCode::FAILURE;
        }
    };
    let Some(chatroom_id) = channel.chatroom_id else {
        eprintln!("kick-live reward-probe: channel has no chatroom id");
        return ExitCode::FAILURE;
    };
    let client_id = format!("kick-native-reward-probe-{}", std::process::id());
    let stats =
        match runtime.block_on(client.anonymous_reward_probe(&client_id, chatroom_id, seconds)) {
            Ok(stats) => stats,
            Err(error) => {
                eprintln!("kick-live reward-probe: subscription failed: {error}");
                return ExitCode::FAILURE;
            }
        };
    println!("subscribed: {}", stats.subscribed);
    println!("publications: {}", stats.publications);
    println!("reward events: {}", stats.reward_events);
    println!("with reward_title: {}", stats.with_reward_title);
    println!("with username: {}", stats.with_username);
    println!("with user_input: {}", stats.with_user_input);
    println!(
        "with reward_background_color: {}",
        stats.with_background_color
    );
    println!("with id: {}", stats.with_id);
    println!("with created_at: {}", stats.with_created_at);
    println!("event names: {}", stats.event_names.join(", "));
    ExitCode::SUCCESS
}

fn run_clips_page_check(slug: &str) -> ExitCode {
    let client = match KickClient::unauthenticated_for_probe() {
        Ok(client) => client,
        Err(error) => {
            eprintln!("kick-live clips-page-check: {error}");
            return ExitCode::FAILURE;
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => {
            eprintln!("kick-live clips-page-check: could not initialize async runtime");
            return ExitCode::FAILURE;
        }
    };

    let first = match runtime.block_on(client.anonymous_channel_clips(slug, None)) {
        Ok(page) => page,
        Err(error) => {
            eprintln!("kick-live clips-page-check: first page failed: {error}");
            return ExitCode::FAILURE;
        }
    };
    println!("first page clips: {}", first.clips.len());
    println!(
        "first page has next cursor: {}",
        first.next_cursor.is_some()
    );
    let Some(cursor) = first.next_cursor else {
        println!("second page: not requested (no cursor)");
        return ExitCode::SUCCESS;
    };

    let second = match runtime.block_on(client.anonymous_channel_clips(slug, Some(&cursor))) {
        Ok(page) => page,
        Err(error) => {
            eprintln!("kick-live clips-page-check: second page failed: {error}");
            return ExitCode::FAILURE;
        }
    };
    let first_ids: HashSet<&str> = first.clips.iter().map(|clip| clip.id.as_str()).collect();
    let overlap = second
        .clips
        .iter()
        .filter(|clip| first_ids.contains(clip.id.as_str()))
        .count();
    println!("second page clips: {}", second.clips.len());
    println!("second page overlap with first: {overlap}");
    println!(
        "second page has next cursor: {}",
        second.next_cursor.is_some()
    );
    if second.clips.is_empty() || overlap > 0 {
        eprintln!("kick-live clips-page-check: cursor did not advance the page");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn run_internal_probe(arguments: &[String]) -> ExitCode {
    let (slug, show_messages) = match parse_internal_probe_args(arguments) {
        Ok(values) => values,
        Err(error) => {
            eprintln!("kick-live internal-probe: {error}");
            return ExitCode::FAILURE;
        }
    };
    let client = match KickClient::unauthenticated_for_probe() {
        Ok(client) => client,
        Err(error) => {
            eprintln!("kick-live internal-probe: {error}");
            return ExitCode::FAILURE;
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => {
            eprintln!("kick-live internal-probe: could not initialize async runtime");
            return ExitCode::FAILURE;
        }
    };

    // Run both endpoints even if one fails so the probe reports each outcome.
    let catalog = runtime.block_on(client.anonymous_catalog());
    let channel = runtime.block_on(client.anonymous_channel(&slug));
    let mut failed = false;

    match catalog {
        Ok(page) => {
            println!("anonymous catalog records: {}", page.items.len());
            println!(
                "anonymous catalog has next cursor: {}",
                page.next_cursor.is_some()
            );
        }
        Err(error) => {
            eprintln!("anonymous catalog: {error}");
            failed = true;
        }
    }
    match channel {
        Ok(channel) => {
            println!("anonymous channel parsed: true");
            println!("anonymous channel is live: {}", channel.is_live());
            println!(
                "anonymous channel has playback source: {}",
                channel.playback_source.is_some()
            );
            match channel.id {
                Some(channel_id) => {
                    match runtime.block_on(client.anonymous_recent_messages(channel_id)) {
                        Ok(page) => {
                            println!("anonymous HTTP history records: {}", page.messages.len());
                            println!(
                                "messages with nonempty sender and body: {}",
                                page.count_with_nonempty_sender_and_content()
                            );
                            println!("realtime chat verification: not performed");
                            if show_messages {
                                for message in &page.messages {
                                    println!(
                                        "[{:?}] {:?}: {:?}",
                                        message.created_at.as_deref().unwrap_or("time unavailable"),
                                        message.username.as_deref().unwrap_or("sender unavailable"),
                                        message.content.as_deref().unwrap_or("")
                                    );
                                }
                            }
                        }
                        Err(error) => {
                            eprintln!("anonymous recent history: {error}");
                            failed = true;
                        }
                    }
                }
                None => {
                    eprintln!("anonymous recent history: channel response had no numeric ID");
                    failed = true;
                }
            }
        }
        Err(error) => {
            eprintln!("anonymous channel: {error}");
            failed = true;
        }
    }

    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn run_live_history_check(arguments: &[String]) -> ExitCode {
    if !arguments.is_empty() {
        eprintln!("kick-live live-history-check: this command takes no arguments");
        return ExitCode::FAILURE;
    }
    let client = match KickClient::unauthenticated_for_probe() {
        Ok(client) => client,
        Err(error) => {
            eprintln!("kick-live live-history-check: {error}");
            return ExitCode::FAILURE;
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => {
            eprintln!("kick-live live-history-check: could not initialize async runtime");
            return ExitCode::FAILURE;
        }
    };

    let catalog = match runtime.block_on(client.anonymous_catalog()) {
        Ok(catalog) => catalog,
        Err(error) => {
            eprintln!("anonymous catalog: {error}");
            return ExitCode::FAILURE;
        }
    };
    let channel_slug = catalog
        .items
        .iter()
        .filter_map(|stream| stream.channel_slug.as_deref())
        .find(|slug| !slug.trim().is_empty())
        .map(str::to_owned);
    drop(catalog);
    let Some(channel_slug) = channel_slug else {
        eprintln!("anonymous live-history check: catalog had no usable channel slug");
        return ExitCode::FAILURE;
    };

    let channel = match runtime.block_on(client.anonymous_channel(&channel_slug)) {
        Ok(channel) => channel,
        Err(error) => {
            eprintln!("anonymous channel: {error}");
            return ExitCode::FAILURE;
        }
    };
    drop(channel_slug);
    if !channel.is_live() {
        println!("anonymous live channel confirmed: false");
        return ExitCode::FAILURE;
    }
    let Some(channel_id) = channel.id else {
        eprintln!("anonymous live-history check: channel response had no numeric ID");
        return ExitCode::FAILURE;
    };
    drop(channel);

    let messages = match runtime.block_on(client.anonymous_recent_messages(channel_id)) {
        Ok(messages) => messages,
        Err(error) => {
            eprintln!("anonymous recent history: {error}");
            return ExitCode::FAILURE;
        }
    };
    let qualifying_messages = messages.count_with_nonempty_sender_and_content();
    println!("anonymous live channel confirmed: true");
    println!(
        "anonymous HTTP history records: {}",
        messages.messages.len()
    );
    println!("messages with nonempty sender and body: {qualifying_messages}");
    println!(
        "at least two qualifying history messages: {}",
        qualifying_messages >= 2
    );
    println!("realtime chat verification: not performed");

    if qualifying_messages >= 2 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn parse_internal_probe_args(arguments: &[String]) -> Result<(String, bool), KickApiError> {
    let mut arguments = arguments.iter().map(String::as_str);
    let mut slug = None;
    let mut show_messages = false;

    while let Some(argument) = arguments.next() {
        match argument {
            "--slug" if slug.is_none() => {
                slug = Some(
                    arguments
                        .next()
                        .ok_or(KickApiError::InvalidChannelSlug)?
                        .to_owned(),
                );
            }
            "--show-messages" if !show_messages => show_messages = true,
            _ => return Err(KickApiError::InvalidChannelSlug),
        }
    }

    Ok((slug.ok_or(KickApiError::InvalidChannelSlug)?, show_messages))
}

fn parse_limit(arguments: impl IntoIterator<Item = String>) -> Result<usize, KickApiError> {
    let mut args = arguments.into_iter();
    let mut limit = DEFAULT_LIMIT;
    while let Some(argument) = args.next() {
        if argument == "--limit" {
            let value = args
                .next()
                .ok_or(KickApiError::InvalidQuery("--limit requires a number"))?;
            limit = parse_limit_value(&value)?;
        } else if let Some(value) = argument.strip_prefix("--limit=") {
            limit = parse_limit_value(value)?;
        } else {
            return Err(KickApiError::InvalidQuery("unknown command-line option"));
        }
    }
    Ok(limit)
}

fn parse_limit_value(value: &str) -> Result<usize, KickApiError> {
    let parsed = value
        .parse::<usize>()
        .map_err(|_| KickApiError::InvalidQuery("--limit must be a positive integer"))?;
    if parsed == 0 || parsed > MAX_LIMIT {
        return Err(KickApiError::InvalidQuery(
            "--limit must be between 1 and 25",
        ));
    }
    Ok(parsed)
}
