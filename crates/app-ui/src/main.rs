use gpui::{
    AnyElement, App, Bounds, ClickEvent, Context, Div, Entity, FocusHandle, FollowMode, ImageCache,
    ImageCacheError, ImageCacheItem, KeyBinding, KeyDownEvent, ListAlignment, ListState, Menu,
    MenuItem, Modifiers, MouseButton, MouseDownEvent, NavigationDirection, Render, Resource,
    Stateful, TitlebarOptions, Window, WindowBounds, WindowHandle, WindowOptions, accesskit::Role,
    actions, div, img, list, point, prelude::*, px, rgb, rgba, size,
};
use kick_core::{BoundedChat, ChatBadge, ChatMessage, ChatReward, PlaybackSource, StreamSummary};
use kick_live::{
    AnonymousRecentMessages, FollowedChannel, FollowedChannelsPage, KickApiError, KickClient,
    emotes::{ChatSegment, parse_chat_segments},
    session::{SessionAuthError, SessionCookie, SessionUser},
};
use kick_player_macos::{
    PlaybackSnapshot, Player, PlayerError, PlayerItemStatus, PlayerQualityCap, PlayerState,
    PlayerTransportStatus,
    chat_input::ChatInputField,
    credentials::{self, CredentialError, SecretCookieInput},
};
use objc2_app_kit::NSView;
use objc2_foundation::{NSPoint, NSRect, NSSize};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    fs,
    future::Future,
    io::Read,
    ops::Range,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tokio::{runtime::Builder as RuntimeBuilder, sync::oneshot, time::timeout};

thread_local! {
    /// The single embedded chat field owned by the shell. The composer renders a
    /// canvas that positions this native view inside the window.
    static CHAT_INPUT_HOST: RefCell<Option<Rc<RefCell<ChatInputField>>>> = const { RefCell::new(None) };
}

fn chat_input_host() -> Option<Rc<RefCell<ChatInputField>>> {
    CHAT_INPUT_HOST.with(|slot| slot.borrow().clone())
}

const CHAT_OVERDRAW: f32 = 144.0;
/// How long the chat stays connected after a live stream ends so a raid
/// published after the end can still arrive.
const RAID_AFTER_STREAM_END_GRACE_SECONDS: u64 = 120;
const CHAT_HISTORY_MAX_MESSAGES: usize = 100;
// kick-live separately caps raw HTTP bodies at 8 MiB; JSON DTO expansion can exceed that.
// This limit only bounds the retained message string payload, not process memory or RSS.
const CHAT_HISTORY_MAX_BYTES: usize = 256 * 1024;
const CHAT_WORKER_EVENT_CAPACITY: usize = 128;
const CHAT_WORKER_DRAIN_MAX: usize = 32;
const CHAT_UI_BATCH_INTERVAL: Duration = Duration::from_millis(100);
const CHAT_MIN_WIDTH: f32 = 300.0;
const CHAT_MAX_WIDTH: f32 = 420.0;
const CHAT_RESIZE_STEP: f32 = 24.0;
const CHAT_DEFAULT_WIDTH: f32 = 352.0;
const CHAT_DELAY_DEFAULT_SECONDS: u64 = 5;
const CHAT_DELAY_MAX_SECONDS: u64 = 60;
const CHAT_DELAY_OPTIONS: [u64; 6] = [0, 3, 5, 8, 10, 15];
const RAID_DISPLAY_OPTIONS: [u64; 4] = [5, 10, 15, 30];
const CHAT_PENDING_MAX_MESSAGES: usize = 512;
const CHAT_DELAY_TICK: Duration = Duration::from_millis(100);
const EMOTE_IMAGE_CACHE_MAX: usize = 256;
const EMOTE_DIAGNOSTIC_MAX: usize = 32;
const NAV_MIN_WIDTH: f32 = 152.0;
const NAV_MAX_WIDTH: f32 = 300.0;
const NAV_DEFAULT_WIDTH: f32 = 240.0;
const SIDEBAR_COLLAPSED_WIDTH: f32 = 48.0;
const FOLLOWED_LIMIT_OPTIONS: [usize; 6] = [5, 8, 12, 20, 30, 50];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SettingsTab {
    General,
    Video,
    Notifications,
    App,
}
const PANEL_DIVIDER_WIDTH: f32 = 6.0;
const MAIN_MIN_WIDTH: f32 = 300.0;

// Shared visual tokens from DESIGN.md.
const APP_BG: u32 = 0x111214;
const RAIL_BG: u32 = 0x18191C;
const VIDEO_BG: u32 = 0x050607;
const RAISED: u32 = 0x202226;
const SELECTED_ROW: u32 = 0x25272B;
const LINE: u32 = 0x2B2D31;
const TEXT: u32 = 0xE6E7E9;
const SUBTLE: u32 = 0xA2A5AA;
const MUTED: u32 = 0x7D8289;
const ACCENT: u32 = 0x53FC18;
const DANGER: u32 = 0xFF625A;

/// UI language. The choice is stored in the settings file and applied to the
/// whole app; `tr` maps the Japanese source strings to English at render time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Language {
    #[default]
    Japanese,
    English,
}

static LANGUAGE: AtomicU8 = AtomicU8::new(0);

fn current_language() -> Language {
    if LANGUAGE.load(Ordering::Relaxed) == 1 {
        Language::English
    } else {
        Language::Japanese
    }
}

fn set_language(language: Language) {
    LANGUAGE.store(
        if language == Language::English { 1 } else { 0 },
        Ordering::Relaxed,
    );
}

/// Picks a Japanese or English value without allocating for the unused branch.
fn localized(ja: impl FnOnce() -> String, en: impl FnOnce() -> String) -> String {
    match current_language() {
        Language::Japanese => ja(),
        Language::English => en(),
    }
}

/// Translates a Japanese UI string. Unknown strings fall back to the source.
fn tr(ja: &'static str) -> &'static str {
    if current_language() == Language::Japanese {
        return ja;
    }
    match ja {
        "{} が {} 人を連れてレイドしました" => "{} raided with {} viewers",
        "{} に移動します ({}秒)" => "Moving to {} in {}s",
        "{} 件" => "{}",
        "{} 視聴中 · {} 配信" => "{} watching · {} live",
        "{}、{}、{}" => "{}、{}、{}",
        "{}、{}、カテゴリ {}、{}、匿名カタログから取得した実データ" => {
            "{}、{}、category {}、{}、real data from the anonymous catalog"
        }
        "← 配信に戻る" => "← Back to stream",
        "↓ 新着メッセージ {unread_messages}件" => "↓ {unread_messages} new messages",
        "○ オフライン" => "○ Offline",
        "● ライブ中" => "● Live",
        "☆ サブスクを贈る" => "☆ Gift a sub",
        "あ" => "a",
        "あい" => "ai",
        "あなたへのカテゴリー" => "Categories for you",
        "あなたへの配信" => "Streams for you",
        "い" => "i",
        "おすすめ" => "Recommended",
        "おすすめの配信はありません" => "No recommended streams",
        "おすすめをもっと見る" => "See more recommendations",
        "おすすめを取得しています…" => "Loading recommendations…",
        "おすすめを取得できません: {}" => "Could not load recommendations: {}",
        "おすすめを少なく表示" => "Show fewer recommendations",
        "おすすめ配信を取得しています…" => "Loading recommended streams…",
        "おすすめ配信を取得できません: {}" => {
            "Could not load recommended streams: {}"
        }
        "このアプリについて" => "About this app",
        "このクライアントではフォロー情報をまだ取得できません" => {
            "This client cannot load follow data yet"
        }
        "さらに読み込んでいます…" => "Loading more…",
        "すべてを表示" => "Show all",
        "ほかを隠す" => "Hide others",
        "もう一度送信すると確定します" => "Press send again to confirm",
        "もっと見る" => "See more",
        "アカウント" => "Account",
        "アカウント: ログイン入力ウィンドウを表示中" => {
            "Account: login window open"
        }
        "アカウント: fixture_user。押してメニューを開く" => {
            "Account: fixture_user. Press to open the menu"
        }
        "アカウント: {username}。押してメニューを開く" => {
            "Account: {username}. Press to open the menu"
        }
        "アカウント: 未ログイン。押してログイン" => {
            "Account: signed out. Press to sign in"
        }
        "アカウント: 認証中" => "Account: signing in",
        "アカウントを確認中です。フォロー情報は未取得です" => {
            "Checking the account. Follow data is not loaded"
        }
        "アカウント状態を確認できません · {reason}" => {
            "Could not verify the account · {reason}"
        }
        "アカウント状態を確認できません。押して再試行" => {
            "Could not verify the account. Press to retry"
        }
        "イベント" => "Events",
        "イベント一覧は未対応です" => "The event list is not supported yet",
        "エモート" => "Emotes",
        "エモート {}件" => "Emotes: {}",
        "エモートID未解決 {unresolved_emotes}件" => {
            "{unresolved_emotes} unresolved emote IDs"
        }
        "エモートを再読み込み" => "Reload emotes",
        "エモートを取得中…" => "Loading emotes…",
        "エモートを選択" => "Choose an emote",
        "エモート取得失敗: {}" => "Could not load emotes: {}",
        "エモート画像取得失敗 {failed_emotes}件" => {
            "{failed_emotes} emote images failed"
        }
        "エモート選択を閉じる" => "Close the emote picker",
        "オフ" => "Off",
        "オフライン" => "Offline",
        "オン" => "On",
        "カタログ" => "Catalog",
        "カタログから配信を選択してください" => "Choose a stream from the catalog",
        "カタログ取得失敗: {}" => "Could not load the catalog: {}",
        "カテゴリ" => "Category",
        "カテゴリなし" => "No category",
        "カテゴリー" => "Categories",
        "キャンセル" => "Cancel",
        "クリップ" => "Clips",
        "{} が {} を交換" => "{} redeemed {}",
        "メッセージ: {}" => "Message: {}",
        "クリップはありません" => "No clips",
        "クリップをさらに読み込む" => "Load more clips",
        "クリップを読み込み中…" => "Loading more clips…",
        "さらに読み込む" => "Load more",
        "クリップは未対応" => "Clips are not supported yet",
        "ゲーム" => "Games",
        "コメントはリアルタイムで表示されます。" => {
            "Comments appear in real time."
        }
        "コメントをコピー" => "Copy comment",
        "コメントをコピーしました" => "Comment copied",
        "コメント遅延" => "Comment delay",
        "コメント遅延 {label}" => "Comment delay {label}",
        "コメント遅延を 5 秒にすると、映像とコメントのタイミングがおおよそ揃います。" => {
            "A 5-second comment delay roughly lines chat up with the video."
        }
        "サイドバーを展開" => "Expand the sidebar",
        "サイドバーを折りたたむ" => "Collapse the sidebar",
        "サイドバー幅を変更" => "Resize the sidebar",
        "サインアップ" => "Sign up",
        "サインアップは未対応" => "Sign-up is not supported yet",
        "サインイン状態は未確認のためフォロー情報を表示できません" => {
            "Follow data is hidden until sign-in is confirmed"
        }
        "サブスクを贈るは未対応" => "Gifting a sub is not supported yet",
        "セッションを確認できません" => "Could not verify the session",
        "チャット" => "Chat",
        "チャットを表示" => "Show chat",
        "チャットを送信" => "Send chat",
        "チャットを開始できません: {error}" => "Could not start chat: {error}",
        "チャットを隠す" => "Hide chat",
        "チャットメッセージ" => "Chat messages",
        "チャットルームが未接続です" => "The chat room is not connected",
        "チャット保持" => "Chat retained",
        "チャット幅を変更" => "Resize the chat panel",
        "チャット接続" => "Chat connection",
        "チャット接続失敗: {}" => "Chat connection failed: {}",
        "チャンネル" => "Channel",
        "チャンネルについて" => "About the channel",
        "チャンネルの再生ソースを取得できません" => {
            "Could not get the playback source"
        }
        "チャンネルの再生ソースを取得中…" => "Loading the playback source…",
        "チャンネルを再試行" => "Retry the channel",
        "チャンネルページを開く" => "Open the channel page",
        "チャンネル取得失敗: HTTP 403（アクセス拒否）" => {
            "Channel load failed: HTTP 403 (access denied)"
        }
        "チャンネル取得失敗: {}" => "Could not load the channel: {}",
        "チャンネル応答に数値IDがありません" => {
            "The channel response has no numeric ID"
        }
        "チャンネル情報を取得しています…" => "Loading channel info…",
        "チャンネル情報を取得できません: {error}" => {
            "Could not load channel info: {error}"
        }
        "デコード失敗" => "Decode failed",
        "デバッグ" => "Debug",
        "交換イベント" => "Reward events",
        "レイド移動" => "Raid moves",
        "ネイティブ macOS クライアント" => "Native macOS client",
        "ネイティブウィンドウを取得できません" => {
            "Could not get the native window"
        }
        "ピクチャ・イン・ピクチャは未対応" => {
            "Picture in picture is not supported yet"
        }
        "ピークRSS" => "Peak RSS",
        "フォロワー {followers}" => "{followers} followers",
        "フォロワーゴール" => "Follower goal",
        "フォロー" => "Follow",
        "フォローする" => "Follow",
        "フォロー中" => "Following",
        "フォロー中のチャンネル" => "Followed channels",
        "フォロー中のチャンネルはありません" => "No followed channels",
        "フォロー中のチャンネルはまだありません" => "No followed channels yet",
        "フォロー中のチャンネルを取得しています…" => {
            "Loading followed channels…"
        }
        "フォロー中のチャンネルを見るにはログインしてください" => {
            "Sign in to see followed channels"
        }
        "フォロー中の一覧を開く" => "Open the followed list",
        "フォロー中をさらに読み込む" => "Load more followed channels",
        "フォロー中を再試行" => "Retry followed channels",
        "フォロー中を取得しています…" => "Loading followed channels…",
        "フォロー中を取得できません: {}" => "Could not load followed channels: {}",
        "フォロー中を取得中…" => "Loading followed channels…",
        "ブラウズ" => "Browse",
        "ブロック（ローカルのみ）" => "Block (local only)",
        "プレイヤー" => "Player",
        "ホーム" => "Home",
        "ミュート" => "Mute",
        "ミュート解除" => "Unmute",
        "メッセージの形式を確認してください" => "Check the message format",
        "メッセージを入力してください" => "Enter a message",
        "メッセージを送信する" => "Send the message",
        "ユーザー情報" => "User info",
        "ユーザー情報は未取得" => "User info is not loaded",
        "ライブチャンネル" => "Live channels",
        "ライブ中" => "Live",
        "ライブ中のフォロー中チャンネルはありません" => {
            "No followed channels are live"
        }
        "ライブ再生ソースがありません" => "No live playback source",
        "ライブ動画領域" => "Live video area",
        "ライブ映像のミュートを解除しました" => "Unmuted the live video",
        "ライブ映像の一時停止を要求しました" => "Requested a live video pause",
        "ライブ映像の再開を要求しました" => "Requested a live video resume",
        "ライブ映像は一時停止中" => "Live video is paused",
        "ライブ映像をバッファ中…" => "Buffering live video…",
        "ライブ映像をミュートしました" => "Muted the live video",
        "ライブ映像を再生していません" => "Live video is not playing",
        "ライブ映像を再生中" => "Playing live video",
        "ライブ映像を準備しました" => "Live video is ready",
        "リンク" => "Links",
        "ルームなし" => "No chat room",
        "レイド先へ今すぐ移動" => "Move to the raid target now",
        "レイド移動をキャンセル" => "Cancel the raid move",
        "レイド通知を閉じる" => "Dismiss the raid notice",
        "ログアウト" => "Sign out",
        "コメントを入力" => "Enter a comment",
        "送信" => "Send",
        "キャンセル" => "Cancel",
        "メッセージを送信する" => "Send a message",
        "メッセージが長すぎます（500文字まで）" => {
            "The message is too long (500 characters max)"
        }
        "Enter で送信 / Ctrl+A で全選択 / 日本語・英語は入力メニューで切り替え" => {
            "Enter to send / Ctrl+A to select all / switch Japanese and English from the input menu"
        }
        "5秒" => "5s",
        "10秒" => "10s",
        "15秒" => "15s",
        "30秒" => "30s",
        "ログイン" => "Sign in",
        "ログインする" => "Sign in",
        "Kick にログイン" => "Sign in to Kick",
        "Kick の session_token の値（DevTools → Application → Cookies → kick.com）を貼り付けてください。" => {
            "Paste the session_token value from DevTools → Application → Cookies → kick.com."
        }
        "session_token の値" => "session_token value",
        "session_token を入力してください" => "Enter a session_token.",
        "クリップボードから貼り付け" => "Paste from clipboard",
        "コピー方法" => "How to copy",
        "JSONファイルを読み込む…" => "Load JSON file…",
        "ログインできません: {error}" => "Could not sign in: {error}",
        "ログインできません: {reason}" => "Could not sign in: {reason}",
        "ポイント表示を閉じる" => "Close points panel",
        "エモート選択を閉じる" => "Close emote picker",
        "ログイン済み" => "Signed in",
        "一時停止" => "Pause",
        "予定されている配信" => "Scheduled streams",
        "予定されている配信はありません" => "No scheduled streams",
        "今すぐ移動" => "Move now",
        "AVPlayer のバッファリングにより、ライブ端から約 5 秒遅れます。" => {
            "Low-latency playback runs about 5 seconds behind live because of AVPlayer."
        }
        "全画面" => "Full screen",
        "全画面を終了" => "Exit full screen",
        "全画面表示を終了" => "Exit full screen",
        "再接続中（試行 {}）" => "Reconnecting (attempt {})",
        "再生" => "Playback",
        "再試行" => "Retry",
        "再読み込み" => "Reload",
        "処理中…" => "Working…",
        "劇場モード" => "Theater mode",
        "匿名カタログworkerを利用できません" => {
            "The anonymous catalog worker is unavailable"
        }
        "匿名カタログに配信がありません" => "The anonymous catalog has no streams",
        "匿名カタログの実データ" => "Anonymous catalog data",
        "匿名カタログの配信を検索" => "Search anonymous catalog streams",
        "匿名カタログを取得しています…" => "Loading the anonymous catalog…",
        "匿名カタログを取得できません: {}" => {
            "Could not load the anonymous catalog: {}"
        }
        "匿名カタログ取得中" => "Loading the anonymous catalog",
        "匿名チャットIDを生成できません" => "Could not create an anonymous chat ID",
        "匿名チャンネル詳細: オフライン" => "Channel details: offline",
        "匿名チャンネル詳細: ライブ中" => "Channel details: live",
        "匿名チャンネル詳細: 未取得" => "Channel details: not loaded",
        "匿名チャンネル詳細を取得中…" => "Loading channel details…",
        "取得できません" => "Unavailable",
        "取得をキャンセルしました" => "Cancelled the request",
        "取得中…" => "Loading…",
        "取得失敗" => "Load failed",
        "この配信には字幕がありません" => "This stream has no subtitles",
        "字幕をオフにする" => "Turn subtitles off",
        "字幕をオンにする" => "Turn subtitles on",
        "ピクチャ・イン・ピクチャ" => "Picture in picture",
        "ピクチャ・イン・ピクチャを終了" => "Exit picture in picture",
        "字幕は未対応" => "Subtitles are not supported yet",
        "少なく表示" => "Show fewer",
        "履歴" => "History",
        "履歴 25件" => "History: 25",
        "履歴 {message_count}件" => "History: {message_count}",
        "履歴なし" => "No history",
        "履歴を再試行" => "Retry history",
        "履歴を取得できません: {}" => "Could not load history: {}",
        "履歴スナップショットを取得しています…" => {
            "Loading a history snapshot…"
        }
        "履歴取得中…" => "Loading history…",
        "履歴失敗: {}" => "History failed: {}",
        "接続中（試行 {}）" => "Connecting (attempt {})",
        "接続停止" => "Disconnected",
        "接続済み" => "Connected",
        "新しいコメントは {chat_delay_seconds} 秒遅れて表示されます。" => {
            "New comments appear {chat_delay_seconds} seconds later."
        }
        "日本語ユーザー" => "Japanese user",
        "映像の遅れに合わせてコメントを遅らせ、ネタバレを防ぎます。" => {
            "Delay comments to match the video and avoid spoilers."
        }
        "最近のチャット履歴はありません" => "No recent chat history",
        "未ログイン" => "Signed out",
        "未ログインのためフォロー情報を表示できません" => {
            "Sign in to see follow data"
        }
        "未取得" => "Not loaded",
        "未選択" => "Not selected",
        "検索" => "Search",
        "検索できません: {error}" => "Search failed: {error}",
        "検索に一致する配信はありません" => "No streams match the search",
        "検索をクリア" => "Clear the search",
        "検索を実行" => "Run the search",
        "検索中…" => "Searching…",
        "概要" => "Overview",
        "無効" => "Disabled",
        "状態未確認" => "State unknown",
        "現在おすすめできる配信がありません" => "No recommendations right now",
        "現在のユーザー確認の応答が空でした。サインイン状態は未確認です" => {
            "The user check returned an empty response. Sign-in is unconfirmed"
        }
        "画質" => "Quality",
        "自動" => "Auto",
        "デフォルト画質" => "Default quality",
        "新しい再生に適用するデフォルトの画質上限を選択します。" => {
            "Choose the default quality cap for new playback."
        }
        "再生中の画質はプレイヤーの画質ボタンで変更できます。" => {
            "You can change the current quality with the player's quality button."
        }
        "画質設定は未対応" => "Quality settings are not supported yet",
        "登録" => "Subscribe",
        "登録は未対応" => "Subscribing is not supported yet",
        "破棄" => "Dropped",
        "科学・技術" => "Science & Technology",
        "統計情報を別ウィンドウで表示します。" => {
            "Show statistics in a separate window."
        }
        "自己紹介" => "About",
        "視聴回数未取得" => "View count not loaded",
        "視聴者 {viewers}" => "{viewers} viewers",
        "視聴者数未提供" => "Viewer count unavailable",
        "流れるコメント" => "Scrolling comments",
        "映像の上を流れるコメントの表示を設定します。" => {
            "Configure comments that scroll over the video."
        }
        "文字サイズ" => "Text size",
        "不透明度" => "Opacity",
        "小" => "Small",
        "中" => "Medium",
        "大" => "Large",
        "チャットは別ウィンドウで表示中" => "Chat is shown in a separate window",
        "チャットを別ウィンドウで開く" => "Open chat in a separate window",
        "元に戻す" => "Restore",
        "元のウィンドウに戻す" => "Restore to the main window",
        "過去配信を再生中: {title}" => "Playing past broadcast: {title}",
        "再生終了: {title}" => "Playback ended: {title}",
        "もう一度再生" => "Play again",
        "過去配信を再生できません" => "Could not play the past broadcast",
        "ライブに戻る" => "Back to live",
        "通知" => "Notifications",
        "配信開始の通知をオンにしたチャンネルです。" => {
            "Channels with live-start notifications enabled."
        }
        "通知をオンにしたチャンネルはありません" => {
            "No channels have notifications enabled"
        }
        "解除" => "Remove",
        "チャンネルポイント" => "Channel points",
        "ポイント" => "Points",
        "交換" => "Redeem",
        "交換しました" => "Redeemed",
        "交換できる報酬がありません" => "No rewards available",
        "が視聴中" => "watching",
        "通知をオンにする" => "Turn notifications on",
        "通知の許可" => "Notification permission",
        "一般" => "General",
        "フォロー中の表示" => "Followed channels",
        "フォロー中の表示数" => "Followed channels shown",
        "左のサイドバーに表示するフォロー中チャンネル数です。" => {
            "Number of followed channels shown in the left sidebar."
        }
        "「もっと見る」から全件表示できます。" => {
            "Use More to see all followed channels."
        }
        "5件" => "5",
        "8件" => "8",
        "12件" => "12",
        "20件" => "20",
        "映像" => "Video",
        "アプリ" => "App",
        "通知を許可" => "Allow notifications",
        "テスト通知" => "Test notification",
        "テスト通知です" => "This is a test notification.",
        "システム設定を開く" => "Open System Settings",
        "許可" => "Allowed",
        "拒否" => "Denied",
        "未確認" => "Not determined",
        "仮許可" => "Provisional",
        "一時許可" => "Ephemeral",
        "バックグラウンド通知: 有効" => "Background notifications: on",
        "バックグラウンド通知: 無効" => "Background notifications: off",
        "有効にする" => "Enable",
        "無効にする" => "Disable",
        "バックグラウンド通知を有効にしました。初回は「Kick Native Notifier」の通知許可を求めるダイアログが出ます" => {
            "Background notifications enabled. The first run asks for permission for Kick Native Notifier."
        }
        "バックグラウンド通知を無効にしました" => {
            "Background notifications disabled"
        }
        "バックグラウンド通知ヘルパーが見つかりません。アプリを再インストールしてください" => {
            "The background notification helper is missing. Reinstall the app."
        }
        "LaunchAgentsフォルダを特定できません" => {
            "Could not locate the LaunchAgents folder"
        }
        "LaunchAgentを書き込めません" => "Could not write the LaunchAgent",
        "通知をオフにする" => "Turn notifications off",
        "チャンネルを共有" => "Share the channel",
        "リンクをコピーしました" => "Link copied",
        "言語" => "Language",
        "アプリの表示言語を選択します。" => "Choose the app display language.",
        "設定" => "Settings",
        "設定…" => "Settings…",
        "設定を閉じる" => "Close settings",
        "詳細取得中" => "Loading details",
        "認証中…" => "Signing in…",
        "認証確認中" => "Checking credentials",
        "読み込み中…" => "Loading…",
        "読込失敗" => "Load failed",
        "送信" => "Send",
        "送信が制限されています（レート制限）" => "Sending is rate limited",
        "送信しました" => "Sent",
        "送信できません" => "Could not send",
        "送信できません (HTTP {status})" => "Could not send (HTTP {status})",
        "送信できません（通信エラー）" => "Could not send (network error)",
        "送信にはログインが必要です" => "Sign in to send messages",
        "送信の確認" => "Confirm sending",
        "送信中" => "Sending",
        "送信中…" => "Sending…",
        "通信ワーカーを利用できません" => "The network worker is unavailable",
        "遅延中" => "Delayed",
        "過去の配信" => "Past streams",
        "過去の配信はありません" => "No past streams",
        "過去の配信やクリップはありません" => "No past streams or clips",
        "選択中のチャンネル情報" => "Selected channel info",
        "配信" => "Live",
        "配信が終了しました" => "The stream ended",
        "配信はオフラインです" => "The stream is offline",
        "配信を探す" => "Find streams",
        "配信を選択してください" => "Choose a stream",
        "配信タイトルなし" => "Untitled stream",
        "配信名、タイトル、カテゴリを検索" => {
            "Search streams, titles, and categories"
        }
        "配信情報" => "Stream info",
        "配信数" => "Streams",
        "配信画面に戻る" => "Back to the stream",
        "閉じる" => "Close",
        "開始 {started_at}" => "Started {started_at}",
        "雑談" => "Just Chatting",
        "音楽" => "Music",
        "AppKitウィンドウを利用できません" => "AppKit window unavailable",
        "AVPlayerでライブ映像を読み込み中…" => "Loading live video with AVPlayer…",
        "AVPlayerで配信を再生できません" => "AVPlayer cannot play this stream",
        "AVPlayerの再生要求に失敗: {error:?}" => "AVPlayer play request failed: {error:?}",
        "AVPlayerの表示領域を取得できません" => "Could not get the AVPlayer view",
        "AVPlayerを初期化できません: {error:?}" => {
            "Could not initialize AVPlayer: {error:?}"
        }
        "AVPlayerミュート操作に失敗: {error:?}" => "AVPlayer mute failed: {error:?}",
        "AVPlayer操作に失敗: {error:?}" => "AVPlayer operation failed: {error:?}",
        "AVPlayer状態の取得に失敗: {error:?}" => {
            "Could not read the AVPlayer state: {error:?}"
        }
        "session_token を入力するとフォロー情報を確認できます" => {
            "Enter a cookie to load follow data"
        }
        "ログイン情報を安全に取得できません" => {
            "Could not read the login details securely"
        }
        "ログイン入力ウィンドウを開けません" => "Could not open the login window",
        "ログイン情報を入力中" => "Entering login details",
        "ログイン情報を入力中…" => "Entering login details…",
        "ログイン情報の形式を確認してください" => "Check the login input format",
        "Enterで検索を実行してください" => "Press Enter to search",
        "HTTP 403（アクセス拒否）" => "HTTP 403 (access denied)",
        "Keychainから削除できません" => "Could not delete from the Keychain",
        "Keychainから読み込めません" => "Could not read from the Keychain",
        "Keychainに保存できません" => "Could not save to the Keychain",
        "Kick Native について" => "About Kick Native",
        "Kick Native の動作を変更します" => "Change how Kick Native behaves",
        "Kick Native を終了" => "Quit Kick Native",
        "Kick Native を隠す" => "Hide Kick Native",
        "Kick Native デバッグ" => "Kick Native Debug",
        "Kickを検索" => "Search Kick",
        "Kickチャットメッセージ" => "Kick chat messages",
        "Tokio runtimeを起動できません" => "Could not start the Tokio runtime",
        "ok あ" => "ok a",
        "{accessible_label}。現在の幅 {:.0} ポイント" => {
            "{accessible_label}. Current width {:.0} points"
        }
        "{author} をローカルで非表示にしました" => "Hid {author} locally",
        "{message_count} 件" => "{message_count}",
        "{pending_count} 件のコメントを保持中です。{chat_delay_seconds} 秒後に表示されます。" => {
            "Holding {pending_count} comments. They appear after {chat_delay_seconds} seconds."
        }
        "{seconds}秒" => "{seconds}s",
        "{unread_messages}件の新着メッセージ。最新へ移動" => {
            "{unread_messages} new messages. Jump to latest"
        }
        "{viewers} 視聴中" => "{viewers} watching",
        "{viewers}人" => "{viewers}",
        "{views} 回視聴" => "{views} views",
        "{} · {filtered_count}件" => "{} · {filtered_count}",
        "RSS" => "RSS",
        other => other,
    }
}

/// Formats a Japanese or English template with the same arguments.
macro_rules! t {
    ($ja:literal) => {
        tr($ja)
    };
    ($ja:literal, $en:literal $(, $arg:expr)* $(,)?) => {
        match current_language() {
            Language::Japanese => format!($ja $(, $arg)*),
            Language::English => format!($en $(, $arg)*),
        }
    };
}

actions!(
    kick_native,
    [
        OpenSettings,
        QuitApp,
        HideApp,
        HideOtherApps,
        ShowAllApps,
        ShowAbout,
    ]
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FollowingState {
    NotRequested,
    Loading,
    Ready,
    Failed(LiveError),
}

impl FollowingState {
    fn label(self) -> String {
        match self {
            Self::NotRequested => tr("未取得").to_owned(),
            Self::Loading => tr("フォロー中を取得中…").to_owned(),
            Self::Ready => tr("フォロー中のチャンネル").to_owned(),
            Self::Failed(error) => t!(
                "フォロー中を取得できません: {}",
                "Could not load followed channels: {}",
                error.user_message()
            ),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FollowingTab {
    Live,
    All,
    Categories,
    Upcoming,
}

impl FollowingTab {
    const ALL: [Self; 4] = [Self::Live, Self::All, Self::Categories, Self::Upcoming];

    fn label(self) -> &'static str {
        match self {
            Self::Live => tr("ライブチャンネル"),
            Self::All => tr("チャンネル"),
            Self::Categories => tr("カテゴリ"),
            Self::Upcoming => tr("予定されている配信"),
        }
    }
}

#[derive(Clone)]
struct ChatContextMenu {
    author: String,
    author_slug: Option<String>,
    text: String,
    position: gpui::Point<gpui::Pixels>,
}

/// A real-time chat message held back so the visible chat matches the delayed
/// video. `due` is when the message becomes visible.
struct PendingChatMessage {
    due: Instant,
    message: ChatMessage,
}

#[derive(Clone, Default)]
struct SelectedChannelDetails {
    profile_picture_url: Option<String>,
    avatar_path: Option<PathBuf>,
    followers_count: Option<String>,
    username: Option<String>,
    started_at: Option<String>,
    viewer_count: Option<u64>,
    title: Option<String>,
    is_live: bool,
    bio: Option<String>,
    twitter: Option<String>,
    instagram: Option<String>,
    youtube: Option<String>,
    discord: Option<String>,
    tiktok: Option<String>,
    facebook: Option<String>,
    club: Option<String>,
    verified: bool,
    banner_url: Option<String>,
    offline_banner_url: Option<String>,
    language: Option<String>,
    tags: Vec<String>,
    follower_goal: Option<kick_live::FollowerGoal>,
}

struct PendingRaid {
    hoster: String,
    target: String,
    viewer_count: u64,
    deadline: Instant,
}

/// The most recently created shell. Application menu actions are dispatched
/// without a focused element, so they reach this handle instead of the view tree.
struct ShellHandle(Mutex<Option<gpui::WeakEntity<KickShell>>>);

impl gpui::Global for ShellHandle {}

struct KickShell {
    live_worker: Option<LiveWorker>,
    emote_image_cache: Entity<BoundedEmoteImageCache>,
    emote_diagnostics: EmoteDiagnostics,
    player_host: Rc<RefCell<PlayerHost>>,
    player_monitor_started: bool,
    streams: Vec<StreamSummary>,
    chat_messages: Arc<[ChatMessage]>,
    chat_state: ListState,
    chat_unread_messages: usize,
    chat_scrolled_up: bool,
    chat_popped_out: bool,
    vod_playing: Option<String>,
    vod_url: Option<String>,
    vod_ended: bool,
    rewards_open: bool,
    rewards_loading: bool,
    rewards: Vec<kick_live::ChannelReward>,
    rewards_error: Option<String>,
    vod_positions: HashMap<String, f64>,
    vod_key: Option<String>,
    chat_popout: Option<WindowHandle<ChatPopout>>,
    chat_snapshot: Arc<Mutex<ChatSnapshot>>,
    navigation_width: f32,
    chat_width: f32,
    selected_nav: &'static str,
    selected_channel_slug: String,
    selected_channel_id: Option<u64>,
    selected_channel_details: SelectedChannelDetails,
    emote_ids_by_name: Arc<HashMap<String, u64>>,
    emote_order: Arc<Vec<(String, u64)>>,
    recent_streams: Vec<String>,
    search_query: String,
    search_state: SearchState,
    search_generation: u64,
    root_focus: FocusHandle,
    search_focus: FocusHandle,
    catalog_state: CatalogState,
    channel_state: ChannelLookupState,
    channel_page_state: ChannelPageState,
    channel_page_generation: u64,
    channel_page_tab: ChannelPageTab,
    clips_loading_more: bool,
    clips_error: Option<String>,
    chat_history_state: ChatHistoryState,
    chat_realtime_state: ChatRealtimeState,
    account_state: AccountState,
    login_panel_open: bool,
    login_input: String,
    login_error: Option<String>,
    login_input_focus: FocusHandle,
    chat_dropped_messages: u64,
    reward_events_received: u64,
    move_events_received: u64,
    selected_chatroom_id: Option<u64>,
    chat_send_pending: bool,
    chat_send_confirmation: bool,
    emote_picker_open: bool,
    blocked_authors: HashSet<String>,
    stats_refresh_started: bool,
    following_refreshing: bool,
    channel_page_visible: bool,
    channel_page_forward: bool,
    feed_forward: bool,
    catalog_generation: u64,
    channel_generation: u64,
    chat_generation: u64,
    chat_history_generation: u64,
    auth_generation: u64,
    chat_monitor_started: bool,
    chat_visible: bool,
    narrow_chat_visible: bool,
    sidebar_visible: bool,
    sidebar_collapsed: bool,
    feed_visible: bool,
    theater_mode: bool,
    following_state: FollowingState,
    following_tab: FollowingTab,
    followed_channels: Vec<FollowedChannel>,
    following_cursor: Option<String>,
    following_has_more: bool,
    following_loading_more: bool,
    following_pages_loaded: usize,
    following_generation: u64,
    show_all_recommended: bool,
    chat_input_host: Rc<RefCell<ChatInputField>>,
    chat_input_notice: Option<String>,
    chat_context_menu: Option<ChatContextMenu>,
    suppress_menu_close: bool,
    pending_raid: Option<PendingRaid>,
    raid_notice: Option<String>,
    raid_generation: u64,
    video_fullscreen: bool,
    follow_action_pending: bool,
    follow_error: Option<String>,
    settings_open: bool,
    settings_tab: SettingsTab,
    followed_limit: usize,
    raid_display_seconds: u64,
    account_menu_open: bool,
    debug_mode: bool,
    notify_channels: HashSet<String>,
    notification_worker: Option<NotificationWorker>,
    notification_generation: u64,
    notification_permission: i32,
    notification_notice: Option<String>,
    toast: Option<(String, Instant)>,
    danmaku_enabled: bool,
    danmaku_font_size: f32,
    danmaku_opacity: f32,
    default_quality: PlayerQualityCap,
    last_danmaku_at: Option<Instant>,
    debug_stats: Arc<Mutex<DebugStats>>,
    debug_window: Option<WindowHandle<DebugView>>,
    debug_stats_started: bool,
    started_at: Instant,
    root_focus_initialized: bool,
    chat_delay_seconds: u64,
    pending_chat: VecDeque<PendingChatMessage>,
    chat_delay_task_running: bool,
}

/// Why an emote image could not be shown. The chat fallback uses this to keep
/// a missing catalogue ID distinct from a failed image request or decode.
#[derive(Clone, Debug, PartialEq, Eq)]
enum EmoteImageFailure {
    HttpStatus(u16),
    Decode,
    Transport,
    Other,
}

impl EmoteImageFailure {
    fn from_error(error: &ImageCacheError) -> Self {
        match error {
            ImageCacheError::BadStatus { status, .. } => Self::HttpStatus(status.as_u16()),
            ImageCacheError::Image(_) | ImageCacheError::Usvg(_) => Self::Decode,
            ImageCacheError::Io(_) => Self::Transport,
            ImageCacheError::Other(_) | ImageCacheError::Asset(_) => Self::Other,
        }
    }

    fn label(&self) -> String {
        match self {
            Self::HttpStatus(status) => format!("HTTP {status}"),
            Self::Decode => tr("デコード失敗").to_owned(),
            Self::Transport => tr("取得失敗").to_owned(),
            Self::Other => tr("読込失敗").to_owned(),
        }
    }
}

fn emote_image_error_detail(error: &ImageCacheError) -> String {
    match error {
        ImageCacheError::Other(error) => format!("{error:#}"),
        ImageCacheError::Io(error) => error.to_string(),
        ImageCacheError::BadStatus { status, .. } => format!("HTTP {status}"),
        ImageCacheError::Image(error) => error.to_string(),
        ImageCacheError::Usvg(error) => error.to_string(),
        ImageCacheError::Asset(message) => message.to_string(),
    }
}

/// Bounded, UI-thread diagnostic state for emote rendering.
///
/// The image cache records load failures by URL; the chat renderer records
/// name-only tokens that the selected channel catalogue could not resolve.
/// Values are counts and classifications only; chat text is never retained.
#[derive(Clone, Default)]
struct EmoteDiagnostics {
    inner: Arc<Mutex<EmoteDiagnosticsInner>>,
}

#[derive(Default)]
struct EmoteDiagnosticsInner {
    unresolved_names: HashSet<String>,
    unresolved_order: VecDeque<String>,
    image_failures: HashMap<String, EmoteImageFailure>,
    image_failure_order: VecDeque<String>,
}

impl EmoteDiagnostics {
    fn lock(&self) -> std::sync::MutexGuard<'_, EmoteDiagnosticsInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn clear(&self) {
        let mut inner = self.lock();
        inner.unresolved_names.clear();
        inner.unresolved_order.clear();
        inner.image_failures.clear();
        inner.image_failure_order.clear();
    }

    fn record_unresolved(&self, name: &str) -> bool {
        let mut inner = self.lock();
        if inner.unresolved_names.insert(name.to_owned()) {
            inner.unresolved_order.push_back(name.to_owned());
            while inner.unresolved_order.len() > EMOTE_DIAGNOSTIC_MAX {
                if let Some(oldest) = inner.unresolved_order.pop_front() {
                    inner.unresolved_names.remove(&oldest);
                }
            }
            true
        } else {
            false
        }
    }

    fn clear_unresolved(&self, name: &str) {
        let mut inner = self.lock();
        if inner.unresolved_names.remove(name) {
            inner.unresolved_order.retain(|candidate| candidate != name);
        }
    }

    /// Returns true when this URL's recorded failure is new or changed.
    fn record_image_failure(&self, url: &str, failure: EmoteImageFailure) -> bool {
        let mut inner = self.lock();
        if inner.image_failures.get(url) == Some(&failure) {
            return false;
        }
        if inner
            .image_failures
            .insert(url.to_owned(), failure)
            .is_none()
        {
            inner.image_failure_order.push_back(url.to_owned());
            while inner.image_failure_order.len() > EMOTE_IMAGE_CACHE_MAX {
                if let Some(oldest) = inner.image_failure_order.pop_front() {
                    inner.image_failures.remove(&oldest);
                }
            }
        }
        true
    }

    fn clear_image_failure(&self, url: &str) -> bool {
        let mut inner = self.lock();
        let removed = inner.image_failures.remove(url).is_some();
        if removed {
            inner
                .image_failure_order
                .retain(|candidate| candidate != url);
        }
        removed
    }

    fn image_failure(&self, url: &str) -> Option<EmoteImageFailure> {
        self.lock().image_failures.get(url).cloned()
    }

    fn counts(&self) -> (usize, usize) {
        let inner = self.lock();
        (inner.unresolved_names.len(), inner.image_failures.len())
    }
}

#[derive(Default)]
struct BoundedEmoteImageCache {
    entries: HashMap<Resource, ImageCacheItem>,
    recency: VecDeque<Resource>,
    diagnostics: EmoteDiagnostics,
}

impl BoundedEmoteImageCache {
    fn new(cx: &mut Context<KickShell>, diagnostics: EmoteDiagnostics) -> Entity<Self> {
        let cache = cx.new(|_| Self {
            diagnostics,
            ..Self::default()
        });
        cx.observe_release(&cache, |_shell, cache, cx| {
            for (_, item) in std::mem::take(&mut cache.entries) {
                if let Some(Ok(image)) = item.get() {
                    cx.drop_image(image, None);
                }
            }
            cache.recency.clear();
        })
        .detach();
        cache
    }

    fn evict_oldest(&mut self, window: &mut Window, cx: &mut App) {
        let Some(resource) = self.recency.pop_front() else {
            return;
        };
        if let Some(item) = self.entries.remove(&resource)
            && let Some(Ok(image)) = item.get()
        {
            cx.drop_image(image, Some(window));
        }
    }
}

impl ImageCache for BoundedEmoteImageCache {
    fn load(
        &mut self,
        resource: &Resource,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Result<Arc<gpui::RenderImage>, gpui::ImageCacheError>> {
        if !self.entries.contains_key(resource) {
            while self.entries.len() >= EMOTE_IMAGE_CACHE_MAX {
                self.evict_oldest(window, cx);
            }
            self.entries
                .insert(resource.clone(), ImageCacheItem::new(resource, cx));
        }

        self.recency.retain(|candidate| candidate != resource);
        self.recency.push_back(resource.clone());
        let result = self.entries.get(resource)?.use_image(window);
        if let Resource::Uri(uri) = resource {
            let url = uri.to_string();
            match &result {
                Some(Ok(_)) => {
                    if self.diagnostics.clear_image_failure(&url) {
                        cx.notify(window.current_view());
                    }
                }
                Some(Err(error)) => {
                    let failure = EmoteImageFailure::from_error(error);
                    if self.diagnostics.record_image_failure(&url, failure.clone()) {
                        eprintln!(
                            "emote image failure: {} ({url}): {}",
                            failure.label(),
                            emote_image_error_detail(error)
                        );
                        cx.notify(window.current_view());
                    }
                }
                None => {}
            }
        }
        result
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum AccountState {
    SignedOut,
    EnteringCookie,
    Checking,
    SignedIn { username: String },
    Failed(String),
}

impl AccountState {
    fn label(&self, compact: bool) -> String {
        if compact {
            return match self {
                Self::SignedOut => tr("未ログイン").to_owned(),
                Self::EnteringCookie => tr("ログイン情報を入力中").to_owned(),
                Self::Checking => tr("認証確認中").to_owned(),
                Self::SignedIn { .. } => tr("ログイン済み").to_owned(),
                Self::Failed(_) => tr("状態未確認").to_owned(),
            };
        }
        match self {
            Self::SignedOut => tr("未ログイン").to_owned(),
            Self::EnteringCookie => tr("ログイン情報を入力中…").to_owned(),
            Self::Checking => tr("認証中…").to_owned(),
            Self::SignedIn { username } => format!("@{username}"),
            Self::Failed(reason) => t!(
                "アカウント状態を確認できません · {reason}",
                "Could not verify the account · {reason}"
            ),
        }
    }

    fn accessibility_label(&self) -> String {
        match self {
            Self::SignedOut => tr("アカウント: 未ログイン。押してログイン").to_owned(),
            Self::EnteringCookie => tr("アカウント: ログイン入力ウィンドウを表示中").to_owned(),
            Self::Checking => tr("アカウント: 認証中").to_owned(),
            Self::SignedIn { username } => t!(
                "アカウント: {username}。押してメニューを開く",
                "Account: {username}. Press to open the menu"
            ),
            Self::Failed(_) => tr("アカウント状態を確認できません。押して再試行").to_owned(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PanelDivider {
    Navigation,
    Chat,
}

fn clamp_navigation_width(width: f32) -> f32 {
    width.clamp(NAV_MIN_WIDTH, NAV_MAX_WIDTH)
}

fn clamp_chat_width(width: f32, width_limit: f32) -> f32 {
    width.clamp(CHAT_MIN_WIDTH, width_limit.max(CHAT_MIN_WIDTH))
}

fn chat_width_limit_for_window(
    window_width: f32,
    navigation_width: f32,
    show_navigation: bool,
) -> f32 {
    let navigation_space = if show_navigation {
        navigation_width + PANEL_DIVIDER_WIDTH
    } else {
        0.0
    };
    let available = window_width - navigation_space - MAIN_MIN_WIDTH - PANEL_DIVIDER_WIDTH;
    CHAT_MAX_WIDTH.min(available.max(CHAT_MIN_WIDTH))
}

fn navigation_width_limit_for_window(window_width: f32, show_chat: bool) -> f32 {
    let chat_space = if show_chat {
        CHAT_MIN_WIDTH + PANEL_DIVIDER_WIDTH
    } else {
        0.0
    };
    (window_width - chat_space - MAIN_MIN_WIDTH - PANEL_DIVIDER_WIDTH)
        .clamp(NAV_MIN_WIDTH, NAV_MAX_WIDTH)
}

fn panel_widths_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).map(|home| {
        home.join("Library")
            .join("Application Support")
            .join("KickRust")
            .join("ui-layout-v1.txt")
    })
}

fn load_panel_widths() -> (f32, f32) {
    let Some(path) = panel_widths_path() else {
        return (NAV_DEFAULT_WIDTH, CHAT_DEFAULT_WIDTH);
    };
    let Ok(contents) = fs::read_to_string(path) else {
        return (NAV_DEFAULT_WIDTH, CHAT_DEFAULT_WIDTH);
    };

    let mut navigation_width = NAV_DEFAULT_WIDTH;
    let mut chat_width = CHAT_DEFAULT_WIDTH;
    for line in contents.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let Ok(value) = value.parse::<f32>() else {
            continue;
        };
        match key {
            "navigation" if value.is_finite() => {
                navigation_width = clamp_navigation_width(value);
            }
            "chat" if value.is_finite() => chat_width = clamp_chat_width(value, CHAT_MAX_WIDTH),
            _ => {}
        }
    }
    (navigation_width, chat_width)
}

fn settings_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).map(|home| {
        home.join("Library")
            .join("Application Support")
            .join("KickRust")
            .join("settings-v1.txt")
    })
}

/// Accepts a bare `session_token` value (the minimal login input), a Cookie
/// header, or a Cookie header produced by the JSON importer. A bare value is
/// tried both as an encoded token and as an already-decoded token.
fn parse_login_candidates(input: &[u8]) -> Result<Vec<SessionCookie>, SessionAuthError> {
    let text = std::str::from_utf8(input).map_err(|_| SessionAuthError::InvalidCookie)?;
    let trimmed = text.trim();
    let trimmed = trimmed
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(trimmed);
    let trimmed = trimmed.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() {
        return Err(SessionAuthError::InvalidCookie);
    }
    // Cookie Editor and browser exports may include the whole cookie line
    // (`session_token=...; Path=/; Domain=.kick.com; ...`). Extract only the
    // session_token value and ignore attributes.
    if let Some(start) = trimmed.find("session_token=") {
        let rest = &trimmed[start + "session_token=".len()..];
        let value = rest.split(';').next().unwrap_or(rest).trim();
        let value = value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .unwrap_or(value);
        if !value.is_empty() {
            let mut candidates = vec![SessionCookie::from_session_token(value)?];
            if value.contains('%')
                && let Ok(raw) = SessionCookie::from_session_token_raw(value)
            {
                candidates.push(raw);
            }
            return Ok(candidates);
        }
    }
    let looks_like_header = trimmed.starts_with("Cookie:")
        || trimmed.contains(';')
        || trimmed.contains("kick_session=");
    if looks_like_header {
        return Ok(vec![SessionCookie::parse_bytes(input)?]);
    }
    if trimmed.starts_with('{') {
        return Err(SessionAuthError::InvalidCookie);
    }
    let mut candidates = vec![SessionCookie::from_session_token(trimmed)?];
    if trimmed.contains('%')
        && let Ok(raw) = SessionCookie::from_session_token_raw(trimmed)
    {
        candidates.push(raw);
    }
    Ok(candidates)
}

fn parse_login_cookie(input: &[u8]) -> Result<SessionCookie, SessionAuthError> {
    parse_login_candidates(input)?
        .into_iter()
        .next()
        .ok_or(SessionAuthError::InvalidCookie)
}

fn background_notifier_plist_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).map(|home| {
        home.join("Library")
            .join("LaunchAgents")
            .join("dev.codex.kick-native.notifier.plist")
    })
}

fn background_notifier_installed() -> bool {
    background_notifier_plist_path().is_some_and(|path| path.is_file())
}

fn load_chat_delay_seconds() -> u64 {
    let Some(path) = settings_path() else {
        return CHAT_DELAY_DEFAULT_SECONDS;
    };
    let Ok(contents) = fs::read_to_string(path) else {
        return CHAT_DELAY_DEFAULT_SECONDS;
    };
    for line in contents.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key != "chat_delay" {
            continue;
        }
        let Ok(seconds) = value.trim().parse::<u64>() else {
            continue;
        };
        return seconds.min(CHAT_DELAY_MAX_SECONDS);
    }
    CHAT_DELAY_DEFAULT_SECONDS
}

fn load_debug_mode() -> bool {
    let Some(path) = settings_path() else {
        return false;
    };
    let Ok(contents) = fs::read_to_string(path) else {
        return false;
    };
    contents.lines().any(|line| line.trim() == "debug=1")
}

fn load_language() -> Language {
    let Some(path) = settings_path() else {
        return Language::Japanese;
    };
    let Ok(contents) = fs::read_to_string(path) else {
        return Language::Japanese;
    };
    if contents.lines().any(|line| line.trim() == "language=en") {
        Language::English
    } else {
        Language::Japanese
    }
}

fn settings_value(key: &str) -> Option<String> {
    let path = settings_path()?;
    let contents = fs::read_to_string(path).ok()?;
    contents
        .lines()
        .find_map(|line| {
            line.strip_prefix(key)
                .and_then(|rest| rest.strip_prefix('='))
        })
        .map(str::to_owned)
}

fn parse_default_quality(value: Option<String>) -> PlayerQualityCap {
    value
        .and_then(|value| value.trim().parse::<i32>().ok())
        .and_then(PlayerQualityCap::from_raw)
        .unwrap_or(PlayerQualityCap::Automatic)
}

fn load_default_quality() -> PlayerQualityCap {
    parse_default_quality(settings_value("quality"))
}

fn load_danmaku_enabled() -> bool {
    settings_value("danmaku").as_deref() == Some("1")
}

fn load_danmaku_font_size() -> f32 {
    settings_value("danmaku_size")
        .and_then(|value| value.parse::<f32>().ok())
        .filter(|value| value.is_finite())
        .map(|value| value.clamp(12.0, 36.0))
        .unwrap_or(18.0)
}

fn load_danmaku_opacity() -> f32 {
    settings_value("danmaku_opacity")
        .and_then(|value| value.parse::<f32>().ok())
        .filter(|value| value.is_finite())
        .map(|value| value.clamp(0.2, 1.0))
        .unwrap_or(0.9)
}

fn vod_positions_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).map(|home| {
        home.join("Library")
            .join("Application Support")
            .join("KickRust")
            .join("vod-positions-v1.txt")
    })
}

/// VOD positions are stored one `uuid=seconds` pair per line. Parsing is
/// deliberately strict so a corrupted or hand-edited file cannot make the app
/// seek to a nonsensical position.
fn parse_vod_positions(contents: &str) -> HashMap<String, f64> {
    contents
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            let seconds = value.parse::<f64>().ok()?;
            if key.is_empty() || !seconds.is_finite() || seconds < 0.0 {
                return None;
            }
            Some((key.to_owned(), seconds))
        })
        .take(200)
        .collect()
}

fn format_vod_positions(positions: &HashMap<String, f64>) -> String {
    let mut body = String::new();
    for (key, seconds) in positions.iter().take(200) {
        body.push_str(key);
        body.push('=');
        body.push_str(&format!("{seconds:.1}"));
        body.push('\n');
    }
    body
}

fn load_vod_positions() -> HashMap<String, f64> {
    let Some(path) = vod_positions_path() else {
        return HashMap::new();
    };
    let Ok(contents) = fs::read_to_string(path) else {
        return HashMap::new();
    };
    parse_vod_positions(&contents)
}

fn save_vod_positions(positions: &HashMap<String, f64>) {
    let Some(path) = vod_positions_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, format_vod_positions(positions));
}

fn load_followed_limit() -> usize {
    settings_value("followed_limit")
        .and_then(|value| value.trim().parse::<usize>().ok())
        .map(|value| value.clamp(1, 50))
        .unwrap_or(5)
}

fn load_raid_display_seconds() -> u64 {
    settings_value("raid_display_seconds")
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|value| value.clamp(5, 60))
        .unwrap_or(10)
}

fn load_notify_channels() -> HashSet<String> {
    let Some(path) = settings_path() else {
        return HashSet::new();
    };
    let Ok(contents) = fs::read_to_string(path) else {
        return HashSet::new();
    };
    contents
        .lines()
        .find_map(|line| line.strip_prefix("notify="))
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|slug| !slug.is_empty() && slug.len() <= 96)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn save_settings(
    chat_delay_seconds: u64,
    debug_mode: bool,
    language: Language,
    notify_channels: &HashSet<String>,
    danmaku_enabled: bool,
    danmaku_font_size: f32,
    danmaku_opacity: f32,
    default_quality: PlayerQualityCap,
    followed_limit: usize,
    raid_display_seconds: u64,
) {
    let Some(path) = settings_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(
        path,
        format!(
            "chat_delay={}\ndebug={}\nlanguage={}\nnotify={}\ndanmaku={}\ndanmaku_size={}\ndanmaku_opacity={}\nquality={}\nfollowed_limit={}\nraid_display_seconds={}\n",
            chat_delay_seconds.min(CHAT_DELAY_MAX_SECONDS),
            u8::from(debug_mode),
            match language {
                Language::Japanese => "ja",
                Language::English => "en",
            },
            notify_channels
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(","),
            u8::from(danmaku_enabled),
            danmaku_font_size,
            danmaku_opacity,
            default_quality.raw_value(),
            followed_limit.clamp(1, 50),
            raid_display_seconds.clamp(5, 60)
        ),
    );
}

#[cfg(debug_assertions)]
fn debug_initial_channel() -> Option<String> {
    let slug = std::env::var("KICK_NATIVE_INITIAL_CHANNEL").ok()?;
    let slug = slug.trim();
    if slug.is_empty()
        || slug.len() > 96
        || !slug
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return None;
    }
    Some(slug.to_owned())
}

fn following_unavailable_message(account_state: &AccountState) -> String {
    match account_state {
        AccountState::SignedOut => tr("未ログインのためフォロー情報を表示できません").to_owned(),
        AccountState::EnteringCookie => {
            tr("session_token を入力するとフォロー情報を確認できます").to_owned()
        }
        AccountState::Checking => tr("アカウントを確認中です。フォロー情報は未取得です").to_owned(),
        AccountState::SignedIn { .. } => {
            tr("このクライアントではフォロー情報をまだ取得できません").to_owned()
        }
        AccountState::Failed(_) => {
            tr("サインイン状態は未確認のためフォロー情報を表示できません").to_owned()
        }
    }
}

fn session_auth_failure_message(error: &SessionAuthError) -> String {
    match error {
        SessionAuthError::UnrecognizedResponse(Some(keys)) if keys.is_empty() => {
            tr("現在のユーザー確認の応答が空でした。サインイン状態は未確認です").to_owned()
        }
        error => error.to_string(),
    }
}

/// Strips emote tokens so the scrolling comment shows readable text only.
fn danmaku_text(text: &str) -> String {
    parse_chat_segments(text)
        .into_iter()
        .filter_map(|segment| match segment {
            ChatSegment::Text(text) => Some(text),
            ChatSegment::Emote { .. } => None,
        })
        .collect::<String>()
        .trim()
        .to_owned()
}

fn chat_text_for_display(text: &str) -> String {
    parse_chat_segments(text)
        .into_iter()
        .map(|segment| match segment {
            ChatSegment::Text(text) => text,
            ChatSegment::Emote { name, .. } => format!(":{name}:"),
        })
        .collect()
}

#[derive(Default)]
struct PlayerHost {
    player: Option<Player>,
    attached: bool,
    status: String,
    had_current_item: bool,
    viewer_visible: bool,
    resume_on_return: bool,
    language: Language,
    quality_cap: kick_player_macos::PlayerQualityCap,
    subtitle_count: i32,
    subtitle_index: i32,
    pip_active: bool,
    danmaku_font_size: f32,
    danmaku_opacity: f32,
}

impl PlayerHost {
    fn new(quality_cap: PlayerQualityCap) -> Self {
        Self {
            player: None,
            attached: false,
            status: tr("ライブ映像を再生していません").to_owned(),
            had_current_item: false,
            viewer_visible: true,
            resume_on_return: false,
            language: current_language(),
            quality_cap,
            subtitle_count: 0,
            subtitle_index: 0,
            pip_active: false,
            danmaku_font_size: 18.0,
            danmaku_opacity: 0.9,
        }
    }

    fn set_language(&mut self, language: Language) {
        self.language = language;
        if let Some(player) = self.player.as_ref() {
            let _ = player.set_language(language == Language::English);
        }
    }

    fn set_danmaku_config(&mut self, font_size: f32, opacity: f32) {
        self.danmaku_font_size = font_size.clamp(12.0, 36.0);
        self.danmaku_opacity = opacity.clamp(0.2, 1.0);
        if let Some(player) = self.player.as_ref() {
            let _ = player.set_danmaku_config(self.danmaku_font_size, self.danmaku_opacity);
        }
    }

    /// Cycles the quality cap and applies it to the running player.
    fn cycle_quality(&mut self) {
        self.quality_cap = self.quality_cap.next();
        if let Some(player) = self.player.as_ref() {
            let _ = player.set_quality(self.quality_cap);
        }
    }

    /// Advances the legible-track selection: off -> 1 -> ... -> count -> off.
    /// The native surface owns the visible CC menu; this path is the accessible
    /// mirror and the keyboard/automation hook.
    fn cycle_subtitle(&mut self) {
        let Some(player) = self.player.as_ref() else {
            return;
        };
        let (count, selected) = player.subtitle_state().unwrap_or((0, 0));
        self.subtitle_count = count;
        self.subtitle_index = selected;
        if count <= 0 {
            return;
        }
        let next = (selected + 1) % (count + 1);
        if player.set_subtitle(next).is_ok() {
            self.subtitle_index = next;
        }
    }

    /// Toggles native picture in picture and records the observed state.
    fn toggle_picture_in_picture(&mut self) {
        let Some(player) = self.player.as_ref() else {
            return;
        };
        let active = player.picture_in_picture_state().unwrap_or(self.pip_active);
        if player.set_picture_in_picture(!active).is_ok() {
            self.pip_active = !active;
        }
    }

    /// Copies subtitle/PiP state from the native session so the GPUI mirror and
    /// the debug window can show it. Returns true when either value changed.
    fn refresh_track_state(&mut self) -> bool {
        let Some(player) = self.player.as_ref() else {
            let changed = self.subtitle_count != 0 || self.subtitle_index != 0 || self.pip_active;
            self.subtitle_count = 0;
            self.subtitle_index = 0;
            self.pip_active = false;
            return changed;
        };
        let (count, selected) = player.subtitle_state().unwrap_or((0, 0));
        let pip = player.picture_in_picture_state().unwrap_or(self.pip_active);
        let quality = player.quality_state().unwrap_or(self.quality_cap);
        let changed = count != self.subtitle_count
            || selected != self.subtitle_index
            || pip != self.pip_active
            || quality != self.quality_cap;
        self.subtitle_count = count;
        self.subtitle_index = selected;
        self.pip_active = pip;
        self.quality_cap = quality;
        changed
    }

    fn set_channel_loading(&mut self) {
        self.shutdown_player();
        self.status = tr("チャンネルの再生ソースを取得中…").to_owned();
    }

    fn set_playback_source(
        &mut self,
        source: Option<PlaybackSource>,
        dvr_url: Option<String>,
        is_live: bool,
    ) {
        self.shutdown_player();
        if !is_live {
            self.status = tr("配信はオフラインです").to_owned();
            return;
        }
        let Some(source) = source else {
            self.status = tr("ライブ再生ソースがありません").to_owned();
            return;
        };
        let dvr_url = dvr_url.filter(|url| !url.trim().is_empty());

        match Player::new() {
            Ok(player) => {
                let _ = player.set_vod_mode(false);
                let _ = player.set_language(self.language == Language::English);
                let _ = player.set_quality(self.quality_cap);
                let result =
                    player.play_hls_pair(&source.url, dvr_url.as_deref().unwrap_or_default(), true);
                match result {
                    Ok(()) => {
                        if !self.viewer_visible {
                            let _ = player.pause();
                            self.resume_on_return = true;
                        }
                        let _ =
                            player.set_danmaku_config(self.danmaku_font_size, self.danmaku_opacity);
                        self.player = Some(player);
                        self.status = tr("AVPlayerでライブ映像を読み込み中…").to_owned();
                    }
                    Err(error) => {
                        self.status = t!(
                            "AVPlayerの再生要求に失敗: {error:?}",
                            "AVPlayer play request failed: {error:?}"
                        );
                    }
                }
            }
            Err(error) => {
                self.status = t!(
                    "AVPlayerを初期化できません: {error:?}",
                    "Could not initialize AVPlayer: {error:?}"
                );
            }
        }
    }

    /// Plays a past broadcast or clip URL (VOD, not live).
    fn play_vod(&mut self, url: &str) {
        self.shutdown_player();
        match Player::new() {
            Ok(player) => match player.play_hls(url, false) {
                Ok(()) => {
                    let _ = player.set_vod_mode(true);
                    let _ = player.set_quality(self.quality_cap);
                    if !self.viewer_visible {
                        let _ = player.pause();
                        self.resume_on_return = true;
                    }
                    let _ = player.set_danmaku_config(self.danmaku_font_size, self.danmaku_opacity);
                    self.player = Some(player);
                    self.status = tr("AVPlayerでライブ映像を読み込み中…").to_owned();
                }
                Err(error) => {
                    self.status = t!(
                        "AVPlayerの再生要求に失敗: {error:?}",
                        "AVPlayer play request failed: {error:?}"
                    );
                }
            },
            Err(error) => {
                self.status = t!(
                    "AVPlayerを初期化できません: {error:?}",
                    "Could not initialize AVPlayer: {error:?}"
                );
            }
        }
    }

    fn set_channel_failure(&mut self) {
        self.shutdown_player();
        self.status = tr("チャンネルの再生ソースを取得できません").to_owned();
    }

    fn set_video_hidden(&mut self, hidden: bool) {
        if let Some(player) = self.player.as_ref()
            && let Ok(native_view) = player.ns_view()
        {
            let view = unsafe { &*native_view.as_ptr().cast::<NSView>() };
            view.setHidden(hidden);
        }
    }

    fn detach_view(&mut self) {
        if let Some(player) = self.player.as_ref()
            && let Ok(native_view) = player.ns_view()
        {
            let view = unsafe { &*native_view.as_ptr().cast::<NSView>() };
            view.removeFromSuperview();
        }
        self.attached = false;
    }

    /// Removes the native video view and pauses playback when the viewer is
    /// replaced by discovery/following content. Resumes only if playback was
    /// active before the switch, so a manual pause is preserved.
    fn suspend_view(&mut self) {
        if !self.viewer_visible {
            return;
        }
        self.viewer_visible = false;
        self.detach_view();
        self.resume_on_return = self
            .player
            .as_ref()
            .and_then(|player| player.snapshot().ok())
            .is_some_and(|snapshot| should_resume_after_viewer_return(snapshot.transport_status));
        if self.resume_on_return
            && let Some(player) = self.player.as_ref()
        {
            let _ = player.pause();
        }
    }

    fn resume_view(&mut self) {
        if self.viewer_visible {
            return;
        }
        self.viewer_visible = true;
        if self.resume_on_return
            && let Some(player) = self.player.as_ref()
        {
            let _ = player.resume();
        }
        self.resume_on_return = false;
    }

    fn shutdown_player(&mut self) {
        self.detach_view();
        self.player.take();
        self.had_current_item = false;
        self.resume_on_return = false;
    }

    fn controls_snapshot(&self) -> Option<PlaybackSnapshot> {
        self.player.as_ref()?.snapshot().ok()
    }

    fn toggle_play_pause(&mut self) -> Result<(), PlayerError> {
        let player = self.player.as_ref().ok_or(PlayerError::NoCurrentItem)?;
        let snapshot = player.snapshot()?;
        if !snapshot.has_current_item {
            return Err(PlayerError::NoCurrentItem);
        }
        let paused = snapshot.transport_status == PlayerTransportStatus::Paused
            || snapshot.state == PlayerState::Paused;
        if paused {
            player.resume()?;
            self.status = tr("ライブ映像の再開を要求しました").to_owned();
        } else {
            player.pause()?;
            self.status = tr("ライブ映像の一時停止を要求しました").to_owned();
        }
        Ok(())
    }

    fn toggle_mute(&mut self) -> Result<(), PlayerError> {
        let player = self.player.as_ref().ok_or(PlayerError::NoCurrentItem)?;
        let snapshot = player.snapshot()?;
        player.set_muted(!snapshot.is_muted)?;
        self.status = if snapshot.is_muted {
            tr("ライブ映像のミュートを解除しました").to_owned()
        } else {
            tr("ライブ映像をミュートしました").to_owned()
        };
        Ok(())
    }

    fn sync_view(&mut self, window: &Window, bounds: Bounds<gpui::Pixels>) {
        let Some(player) = self.player.as_ref() else {
            return;
        };
        if bounds.is_empty() {
            self.detach_view();
            return;
        }

        let handle = match HasWindowHandle::window_handle(window) {
            Ok(handle) => handle,
            Err(_) => {
                self.status = tr("ネイティブウィンドウを取得できません").to_owned();
                return;
            }
        };
        let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
            self.status = tr("AppKitウィンドウを利用できません").to_owned();
            return;
        };
        let Ok(native_view) = player.ns_view() else {
            self.status = tr("AVPlayerの表示領域を取得できません").to_owned();
            return;
        };

        let parent = unsafe { &*appkit.ns_view.as_ptr().cast::<NSView>() };
        let child = unsafe { &*native_view.as_ptr().cast::<NSView>() };
        let parent_bounds = parent.bounds();
        let x = parent_bounds.origin.x + f64::from(bounds.origin.x.as_f32());
        let width = f64::from(bounds.size.width.as_f32());
        let height = f64::from(bounds.size.height.as_f32());
        let y_offset = f64::from(bounds.origin.y.as_f32());
        let y = if parent.isFlipped() {
            parent_bounds.origin.y + y_offset
        } else {
            parent_bounds.origin.y + parent_bounds.size.height - y_offset - height
        };
        child.setFrame(NSRect {
            origin: NSPoint { x, y },
            size: NSSize { width, height },
        });
        let attached_to_parent =
            unsafe { child.superview() }.is_some_and(|superview| std::ptr::eq(&*superview, parent));
        if !attached_to_parent {
            parent.addSubview(child);
        }
        self.attached = true;
    }

    fn refresh_snapshot(&mut self) -> PlayerMonitorUpdate {
        let Some(snapshot_result) = self.player.as_ref().map(Player::snapshot) else {
            self.refresh_track_state();
            return PlayerMonitorUpdate::Unchanged;
        };
        let mut ended = false;
        let status = match snapshot_result {
            Ok(snapshot) => {
                ended = playback_has_ended(snapshot, self.had_current_item);
                self.had_current_item |= snapshot.has_current_item;
                if ended {
                    let status = snapshot_status(snapshot);
                    self.shutdown_player();
                    status
                } else if snapshot.state == PlayerState::Idle && !snapshot.has_current_item {
                    tr("AVPlayerでライブ映像を読み込み中…").to_owned()
                } else {
                    snapshot_status(snapshot)
                }
            }
            Err(error) => t!(
                "AVPlayer状態の取得に失敗: {error:?}",
                "Could not read the AVPlayer state: {error:?}"
            ),
        };
        if self.status == status {
            let track_changed = self.refresh_track_state();
            return if ended {
                PlayerMonitorUpdate::Ended
            } else if track_changed {
                PlayerMonitorUpdate::Changed
            } else {
                PlayerMonitorUpdate::Unchanged
            };
        }
        self.status = status;
        self.refresh_track_state();
        if ended {
            PlayerMonitorUpdate::Ended
        } else {
            PlayerMonitorUpdate::Changed
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlayerMonitorUpdate {
    Unchanged,
    Changed,
    Ended,
}

fn should_resume_after_viewer_return(status: PlayerTransportStatus) -> bool {
    matches!(
        status,
        PlayerTransportStatus::Playing | PlayerTransportStatus::Waiting
    )
}

fn playback_has_ended(snapshot: PlaybackSnapshot, previously_had_item: bool) -> bool {
    previously_had_item && snapshot.state == PlayerState::Idle && !snapshot.has_current_item
}

fn snapshot_status(snapshot: PlaybackSnapshot) -> String {
    if snapshot.item_status == PlayerItemStatus::Failed || snapshot.state == PlayerState::Failed {
        tr("AVPlayerで配信を再生できません").to_owned()
    } else if snapshot.state == PlayerState::Idle && !snapshot.has_current_item {
        tr("配信が終了しました").to_owned()
    } else if snapshot.video_ready_for_display {
        match snapshot.transport_status {
            PlayerTransportStatus::Playing => tr("ライブ映像を再生中").to_owned(),
            PlayerTransportStatus::Paused => tr("ライブ映像は一時停止中").to_owned(),
            PlayerTransportStatus::Waiting => tr("ライブ映像をバッファ中…").to_owned(),
            PlayerTransportStatus::Unknown => tr("ライブ映像を準備しました").to_owned(),
        }
    } else {
        tr("AVPlayerでライブ映像を読み込み中…").to_owned()
    }
}

impl Drop for KickShell {
    fn drop(&mut self) {
        self.player_host.borrow_mut().shutdown_player();
    }
}

impl KickShell {
    fn new(cx: &mut Context<Self>) -> Self {
        let streams = Vec::new();
        let chat_messages: Arc<[ChatMessage]> = Arc::from(Vec::new());
        let (navigation_width, chat_width) = load_panel_widths();
        let chat_state = new_chat_list_state(chat_messages.len());
        let emote_diagnostics = EmoteDiagnostics::default();
        let emote_image_cache = BoundedEmoteImageCache::new(cx, emote_diagnostics.clone());

        let default_quality = load_default_quality();
        let mut this = Self {
            live_worker: LiveWorker::new().ok(),
            emote_image_cache,
            emote_diagnostics,
            player_host: Rc::new(RefCell::new(PlayerHost::new(default_quality))),
            player_monitor_started: false,
            streams,
            chat_messages,
            chat_state: chat_state.clone(),
            chat_unread_messages: 0,
            chat_scrolled_up: false,
            chat_popped_out: false,
            vod_playing: None,
            vod_url: None,
            vod_ended: false,
            rewards_open: false,
            rewards_loading: false,
            rewards: Vec::new(),
            rewards_error: None,
            vod_positions: load_vod_positions(),
            vod_key: None,
            chat_popout: None,
            chat_snapshot: Arc::new(Mutex::new(ChatSnapshot::default())),
            navigation_width,
            chat_width,
            selected_nav: "ホーム",
            recent_streams: Vec::new(),
            selected_channel_slug: String::new(),
            selected_channel_id: None,
            selected_channel_details: SelectedChannelDetails::default(),
            emote_ids_by_name: Arc::new(HashMap::new()),
            emote_order: Arc::new(Vec::new()),
            search_query: String::new(),
            search_state: SearchState::Idle,
            search_generation: 0,
            root_focus: cx.focus_handle(),
            search_focus: cx.focus_handle().tab_index(0),
            catalog_state: CatalogState::Loading,
            channel_state: ChannelLookupState::NotRequested,
            channel_page_state: ChannelPageState::Idle,
            channel_page_generation: 0,
            channel_page_tab: ChannelPageTab::Home,
            clips_loading_more: false,
            clips_error: None,
            chat_history_state: ChatHistoryState::NoChannel,
            chat_realtime_state: ChatRealtimeState::NoChannel,
            account_state: AccountState::SignedOut,
            login_panel_open: false,
            login_input: String::new(),
            login_error: None,
            login_input_focus: cx.focus_handle().tab_index(0),
            chat_dropped_messages: 0,
            reward_events_received: 0,
            move_events_received: 0,
            selected_chatroom_id: None,
            chat_send_pending: false,
            chat_send_confirmation: false,
            emote_picker_open: false,
            blocked_authors: HashSet::new(),
            stats_refresh_started: false,
            following_refreshing: false,
            channel_page_visible: false,
            channel_page_forward: false,
            feed_forward: false,
            catalog_generation: 1,
            channel_generation: 0,
            chat_generation: 0,
            chat_history_generation: 0,
            auth_generation: 0,
            chat_monitor_started: false,
            chat_visible: true,
            narrow_chat_visible: false,
            sidebar_visible: true,
            sidebar_collapsed: false,
            feed_visible: true,
            theater_mode: false,
            following_state: FollowingState::NotRequested,
            following_tab: FollowingTab::Live,
            followed_channels: Vec::new(),
            following_cursor: None,
            following_has_more: false,
            following_loading_more: false,
            following_pages_loaded: 0,
            following_generation: 0,
            show_all_recommended: false,
            chat_input_host: Rc::new(RefCell::new(
                ChatInputField::new().expect("native chat input field"),
            )),
            chat_input_notice: None,
            chat_context_menu: None,
            suppress_menu_close: false,
            pending_raid: None,
            raid_notice: None,
            raid_generation: 0,
            video_fullscreen: false,
            follow_action_pending: false,
            follow_error: None,
            settings_open: false,
            settings_tab: SettingsTab::General,
            followed_limit: load_followed_limit(),
            raid_display_seconds: load_raid_display_seconds(),
            account_menu_open: false,
            debug_mode: load_debug_mode(),
            notify_channels: load_notify_channels(),
            notification_worker: None,
            notification_generation: 0,
            notification_permission: kick_player_macos::notification_status(),
            notification_notice: None,
            toast: None,
            danmaku_enabled: load_danmaku_enabled(),
            danmaku_font_size: load_danmaku_font_size(),
            danmaku_opacity: load_danmaku_opacity(),
            default_quality,
            last_danmaku_at: None,
            debug_stats: Arc::new(Mutex::new(DebugStats::default())),
            debug_window: None,
            debug_stats_started: false,
            started_at: Instant::now(),
            root_focus_initialized: false,
            chat_delay_seconds: load_chat_delay_seconds(),
            pending_chat: VecDeque::new(),
            chat_delay_task_running: false,
        };

        let chat_state_for_scroll = chat_state;
        let shell_for_scroll = cx.entity().downgrade();
        this.chat_state
            .set_scroll_handler(move |event, _window, cx| {
                let following = event.is_following_tail;
                let shell = shell_for_scroll.clone();
                // The list calls this handler while its state is mutably borrowed.
                // Defer the state update until the list finishes its scroll/layout pass.
                cx.defer(move |cx| {
                    let _ = shell.update(cx, |this, cx| {
                        let scrolled_up = !following;
                        if this.chat_scrolled_up != scrolled_up {
                            this.chat_scrolled_up = scrolled_up;
                            cx.notify();
                        }
                        if following && this.chat_unread_messages > 0 {
                            this.chat_unread_messages = 0;
                            cx.notify();
                        }
                    });
                });
            });

        if this.live_worker.is_some() {
            this.start_catalog_load(cx);
        } else {
            this.catalog_state = CatalogState::Failed(LiveError::WorkerUnavailable);
        }
        match credentials::load_cookie() {
            Ok(Some(input)) => this.validate_cookie_input(input, false, cx),
            Ok(None) => {}
            Err(_) => {
                this.account_state =
                    AccountState::Failed(tr("Keychainから読み込めません").to_owned());
            }
        }
        this.player_host
            .borrow_mut()
            .set_danmaku_config(this.danmaku_font_size, this.danmaku_opacity);
        cx.set_global(ShellHandle(Mutex::new(Some(cx.entity().downgrade()))));
        if this.debug_mode {
            this.ensure_debug_stats_task(cx);
            cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(400))
                    .await;
                let _ = this.update(cx, |this, cx| this.open_debug_window(cx));
            })
            .detach();
        }
        this.start_notifications(cx);
        this.refresh_notification_permission(cx);
        CHAT_INPUT_HOST.with(|slot| {
            *slot.borrow_mut() = Some(this.chat_input_host.clone());
        });
        this.chat_input_host
            .borrow()
            .set_placeholder(tr("メッセージを送信する"));
        this
    }

    fn start_notifications(&mut self, cx: &mut Context<Self>) {
        self.stop_notifications();
        self.notification_generation = self.notification_generation.wrapping_add(1);
        let generation = self.notification_generation;
        let slugs: Vec<String> = self.notify_channels.iter().cloned().collect();
        if slugs.is_empty() {
            return;
        }
        let Some((worker, mut receiver)) = NotificationWorker::start(slugs) else {
            return;
        };
        self.notification_worker = Some(worker);
        cx.spawn(async move |this, cx| {
            while let Some(event) = receiver.recv().await {
                let _ = this.update(cx, |this, cx| {
                    if generation != this.notification_generation {
                        return;
                    }
                    this.handle_stream_notification(event, cx);
                });
            }
        })
        .detach();
        self.restart_background_notifier();
    }

    fn stop_notifications(&mut self) {
        if let Some(worker) = self.notification_worker.take() {
            worker.cancel();
        }
    }

    fn install_background_notifier(&mut self, cx: &mut Context<Self>) {
        let helper = std::path::Path::new(
            "/Applications/KickNativeNotifier.app/Contents/MacOS/kick-gpui-poc",
        );
        if !helper.is_file() {
            self.notification_notice = Some(tr(
                "バックグラウンド通知ヘルパーが見つかりません。アプリを再インストールしてください",
            )
            .to_owned());
            cx.notify();
            return;
        }
        let Some(path) = background_notifier_plist_path() else {
            self.notification_notice = Some(tr("LaunchAgentsフォルダを特定できません").to_owned());
            cx.notify();
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let home = std::env::var("HOME").unwrap_or_default();
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>dev.codex.kick-native.notifier</string>
  <key>ProgramArguments</key>
  <array>
    <string>/Applications/KickNativeNotifier.app/Contents/MacOS/kick-gpui-poc</string>
    <string>--notify-helper</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>HOME</key>
    <string>{home}</string>
  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>ProcessType</key>
  <string>Background</string>
  <key>ThrottleInterval</key>
  <integer>30</integer>
  <key>StandardOutPath</key>
  <string>/private/tmp/kick-notifier.out</string>
  <key>StandardErrorPath</key>
  <string>/private/tmp/kick-notifier.err</string>
</dict>
</plist>
"#
        );
        if fs::write(&path, plist).is_err() {
            self.notification_notice = Some(tr("LaunchAgentを書き込めません").to_owned());
            cx.notify();
            return;
        }
        let domain = format!("gui/{}", unsafe { libc::getuid() });
        let service = format!("{domain}/dev.codex.kick-native.notifier");
        let path_text = path.to_string_lossy().into_owned();
        let _ = std::process::Command::new("launchctl")
            .args(["bootout", &service])
            .status();
        let _ = std::process::Command::new("launchctl")
            .args(["bootout", &domain, &path_text])
            .status();
        let bootstrapped = std::process::Command::new("launchctl")
            .args(["bootstrap", &domain, &path_text])
            .status()
            .is_ok_and(|status| status.success());
        if !bootstrapped {
            let _ = std::process::Command::new("launchctl")
                .args(["load", "-w", &path_text])
                .status();
        }
        let _ = std::process::Command::new("launchctl")
            .args(["kickstart", "-k", &service])
            .status();
        self.notification_notice = Some(tr(
            "バックグラウンド通知を有効にしました。初回は「Kick Native Notifier」の通知許可を求めるダイアログが出ます",
        )
        .to_owned());
        cx.notify();
    }

    fn uninstall_background_notifier(&mut self, cx: &mut Context<Self>) {
        let domain = format!("gui/{}", unsafe { libc::getuid() });
        let service = format!("{domain}/dev.codex.kick-native.notifier");
        let _ = std::process::Command::new("launchctl")
            .args(["bootout", &service])
            .status();
        if let Some(path) = background_notifier_plist_path() {
            let _ = fs::remove_file(path);
        }
        self.notification_notice = Some(tr("バックグラウンド通知を無効にしました").to_owned());
        cx.notify();
    }

    fn restart_background_notifier(&self) {
        if !background_notifier_installed() {
            return;
        }
        let domain = format!("gui/{}", unsafe { libc::getuid() });
        let service = format!("{domain}/dev.codex.kick-native.notifier");
        // `launchctl kickstart` can block until the helper finishes launching, so never
        // wait for it on the UI thread.
        std::thread::spawn(move || {
            let _ = std::process::Command::new("launchctl")
                .args(["kickstart", "-k", &service])
                .status();
        });
    }

    fn handle_stream_notification(
        &mut self,
        event: kick_live::realtime::StreamNotificationEvent,
        cx: &mut Context<Self>,
    ) {
        let kick_live::realtime::StreamNotificationEvent::StreamerIsLive { slug, .. } = event
        else {
            return;
        };
        if !self.notify_channels.contains(&slug) {
            return;
        }
        let title = self
            .streams
            .iter()
            .find(|stream| stream.channel_slug == slug)
            .map(|stream| stream.channel_name.clone())
            .or_else(|| {
                self.followed_channels
                    .iter()
                    .find(|channel| channel.channel_slug == slug)
                    .map(|channel| channel.channel_name.clone())
            })
            .unwrap_or_else(|| slug.clone());
        let body = t!("{title} が配信を開始しました", "{title} is live");
        let _ = kick_player_macos::post_notification(&title, &body, &slug);
        cx.notify();
    }

    fn request_notifications(&mut self, cx: &mut Context<Self>) {
        let _ = kick_player_macos::request_notification_authorization();
        self.refresh_notification_permission(cx);
        cx.notify();
    }

    fn refresh_notification_permission(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            let status = kick_player_macos::notification_status();
            let _ = this.update(cx, |this, cx| {
                this.notification_permission = status;
                cx.notify();
            });
        })
        .detach();
    }

    fn show_toast(&mut self, text: String, cx: &mut Context<Self>) {
        self.toast = Some((text, Instant::now()));
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(2)).await;
            let _ = this.update(cx, |this, cx| {
                if this
                    .toast
                    .as_ref()
                    .is_some_and(|(_, at)| at.elapsed() >= Duration::from_secs(2))
                {
                    this.toast = None;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn test_notification(&mut self, cx: &mut Context<Self>) {
        let _ = kick_player_macos::post_notification("Kick Native", tr("テスト通知です"), "");
        cx.notify();
    }

    fn persist_panel_widths(&self) {
        let Some(path) = panel_widths_path() else {
            return;
        };
        let Some(parent) = path.parent() else {
            return;
        };
        if fs::create_dir_all(parent).is_err() {
            return;
        }
        let temporary_path = path.with_extension("tmp");
        let contents = format!(
            "navigation={}\nchat={}\n",
            self.navigation_width, self.chat_width
        );
        if fs::write(&temporary_path, contents).is_ok() {
            let _ = fs::rename(temporary_path, path);
        }
    }

    fn resize_panel_from_pointer(
        &mut self,
        divider: PanelDivider,
        pointer_x: f32,
        window_width: f32,
    ) {
        match divider {
            PanelDivider::Navigation => {
                let show_chat = window_width >= 780.0 && self.chat_visible;
                let width_limit = navigation_width_limit_for_window(window_width, show_chat);
                self.navigation_width =
                    (pointer_x - PANEL_DIVIDER_WIDTH / 2.0).clamp(NAV_MIN_WIDTH, width_limit);
            }
            PanelDivider::Chat => {
                let show_navigation =
                    self.sidebar_visible && !self.theater_mode && window_width >= 780.0;
                let navigation_width_limit = navigation_width_limit_for_window(
                    window_width,
                    window_width >= 780.0 && self.chat_visible,
                );
                let width_limit = chat_width_limit_for_window(
                    window_width,
                    self.navigation_width.min(navigation_width_limit),
                    show_navigation,
                );
                self.chat_width = clamp_chat_width(
                    window_width - pointer_x - PANEL_DIVIDER_WIDTH / 2.0,
                    width_limit,
                );
            }
        }
    }

    fn set_feed_visible(&mut self, visible: bool) {
        if visible {
            self.player_host.borrow_mut().detach_view();
        }
        self.feed_visible = visible;
    }

    fn set_theater_mode(&mut self, enabled: bool) {
        self.theater_mode = enabled;
        if !enabled && self.feed_visible {
            self.player_host.borrow_mut().detach_view();
        }
    }

    fn start_player_monitor(&mut self, cx: &mut Context<Self>) {
        if self.player_monitor_started {
            return;
        }
        self.player_monitor_started = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                let update = this.update(cx, |view, cx| {
                    let monitor_update = view.player_host.borrow_mut().refresh_snapshot();
                    match monitor_update {
                        PlayerMonitorUpdate::Unchanged => {}
                        PlayerMonitorUpdate::Changed => cx.notify(),
                        PlayerMonitorUpdate::Ended => {
                            if should_keep_chat_after_end(view.vod_playing.is_some()) {
                                view.keep_chat_for_raid_after_stream_end(cx);
                            } else {
                                view.vod_ended = true;
                                view.stop_chat();
                            }
                            cx.notify();
                        }
                    }
                });
                if update.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn stop_chat(&mut self) {
        self.chat_generation = self.chat_generation.wrapping_add(1);
        if let Some(worker) = self.live_worker.as_ref() {
            worker.stop_chat();
        }
        self.chat_realtime_state = ChatRealtimeState::Stopped;
    }

    /// The streamer usually raids after the broadcast ends, and the
    /// `StreamHostedEvent` arrives on the still-open chat channel. Keep the
    /// chat session alive for a bounded grace period instead of stopping it
    /// the moment the player ends. A channel switch or a handled raid cancels
    /// the wait through the chat generation.
    fn keep_chat_for_raid_after_stream_end(&mut self, cx: &mut Context<Self>) {
        let generation = self.chat_generation;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_secs(RAID_AFTER_STREAM_END_GRACE_SECONDS))
                .await;
            let _ = this.update(cx, |this, cx| {
                if generation != this.chat_generation {
                    return;
                }
                if this.pending_raid.is_none() {
                    this.stop_chat();
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn sign_out(&mut self, cx: &mut Context<Self>) {
        if matches!(self.account_state, AccountState::SignedIn { .. }) {
            self.auth_generation = self.auth_generation.wrapping_add(1);
            self.following_generation = self.following_generation.wrapping_add(1);
            self.following_state = FollowingState::NotRequested;
            self.followed_channels.clear();
            self.following_cursor = None;
            self.following_has_more = false;
            self.following_loading_more = false;
            if let Some(worker) = self.live_worker.as_mut() {
                worker.clear_session();
            }
            match credentials::delete_cookie() {
                Ok(()) => self.account_state = AccountState::SignedOut,
                Err(_) => {
                    self.account_state =
                        AccountState::Failed(tr("Keychainから削除できません").to_owned());
                }
            }
            cx.notify();
        }
    }

    fn warm_avatar_cache(&self, urls: Vec<String>, cx: &mut Context<Self>) {
        let pending: Vec<String> = urls
            .into_iter()
            .filter(|url| !avatar_cache_path(url).exists())
            .take(16)
            .collect();
        if pending.is_empty() {
            return;
        }
        cx.spawn(async move |this, cx| {
            let done = cx
                .background_executor()
                .spawn(async move {
                    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    else {
                        return false;
                    };
                    let mut any = false;
                    for url in pending {
                        let path = avatar_cache_path(&url);
                        if runtime.block_on(fetch_and_crop_avatar(&url, &path)).is_ok() {
                            any = true;
                        }
                    }
                    any
                })
                .await;
            if done {
                let _ = this.update(cx, |_, cx| cx.notify());
            }
        })
        .detach();
    }

    fn prepare_channel_avatar(&mut self, url: String, cx: &mut Context<Self>) {
        let path = avatar_cache_path(&url);
        if path.exists() {
            self.selected_channel_details.avatar_path = Some(path);
            return;
        }
        cx.spawn(async move |this, cx| {
            // reqwest needs a Tokio reactor; the GPUI executor is not one.
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .ok()
                .map(|runtime| runtime.block_on(fetch_and_crop_avatar(&url, &path)));
            if let Some(Ok(path)) = result {
                let _ = this.update(cx, |this, cx| {
                    this.selected_channel_details.avatar_path = Some(path);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn open_login_panel(&mut self, cx: &mut Context<Self>) {
        self.login_panel_open = true;
        self.login_input.clear();
        self.login_error = None;
        self.player_host.borrow_mut().set_video_hidden(true);
        cx.notify();
    }

    fn open_login_panel_focused(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_login_panel(cx);
        self.login_input_focus.focus(window, cx);
    }

    fn close_login_panel(&mut self, cx: &mut Context<Self>) {
        self.login_panel_open = false;
        self.login_input.clear();
        self.login_error = None;
        self.player_host.borrow_mut().set_video_hidden(false);
        cx.notify();
    }

    fn paste_login_clipboard(&mut self, cx: &mut Context<Self>) {
        match credentials::read_clipboard() {
            Ok(input) => {
                let text = input.with_bytes(|bytes| String::from_utf8_lossy(bytes).to_string());
                self.login_input = text;
                self.login_error = None;
            }
            Err(error) => {
                self.login_error = Some(error.to_string());
            }
        }
        cx.notify();
    }

    fn submit_login(&mut self, cx: &mut Context<Self>) {
        let text = self.login_input.trim().to_owned();
        if text.is_empty() {
            self.login_error = Some(tr("session_token を入力してください").to_owned());
            cx.notify();
            return;
        }
        match credentials::secret_cookie_from_login_text(&text) {
            Ok(input) => {
                self.login_panel_open = false;
                self.login_input.clear();
                self.login_error = None;
                self.player_host.borrow_mut().set_video_hidden(false);
                self.validate_cookie_input(input, true, cx);
            }
            Err(error) => {
                self.login_error = Some(error.to_string());
                cx.notify();
            }
        }
    }

    fn account_button_action(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(
            self.account_state,
            AccountState::SignedOut | AccountState::Failed(_)
        ) {
            self.open_login_panel_focused(window, cx);
        } else {
            self.toggle_account_menu(cx);
        }
    }

    fn begin_login(&mut self, cx: &mut Context<Self>) {
        if matches!(
            self.account_state,
            AccountState::EnteringCookie | AccountState::Checking
        ) {
            return;
        }

        let (reply, receiver) = tokio::sync::oneshot::channel();
        if credentials::begin_prompt_cookie(move |result| {
            let _ = reply.send(result);
        })
        .is_err()
        {
            self.account_state =
                AccountState::Failed(tr("ログイン入力ウィンドウを開けません").to_owned());
            cx.notify();
            return;
        }

        self.account_state = AccountState::EnteringCookie;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = receiver
                .await
                .unwrap_or(Err(CredentialError::NativeFailure));
            let _ = this.update(cx, |this, cx| match result {
                Ok(Some(input)) => this.validate_cookie_input(input, true, cx),
                Ok(None) => {
                    this.account_state = AccountState::SignedOut;
                    cx.notify();
                }
                Err(CredentialError::InvalidCookie) => {
                    this.account_state =
                        AccountState::Failed(tr("ログイン情報の形式を確認してください").to_owned());
                    cx.notify();
                }
                Err(_) => {
                    this.account_state =
                        AccountState::Failed(tr("ログイン情報を安全に取得できません").to_owned());
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn validate_cookie_input(
        &mut self,
        input: SecretCookieInput,
        save_on_success: bool,
        cx: &mut Context<Self>,
    ) {
        let candidates = match input.with_bytes(parse_login_candidates) {
            Ok(candidates) => candidates,
            Err(error) => {
                self.account_state = AccountState::Failed(error.to_string());
                self.show_toast(format!("ログインできません: {error}"), cx);
                cx.notify();
                return;
            }
        };
        let Some(worker) = self.live_worker.as_ref() else {
            self.account_state =
                AccountState::Failed(tr("通信ワーカーを利用できません").to_owned());
            cx.notify();
            return;
        };

        self.auth_generation = self.auth_generation.wrapping_add(1);
        let generation = self.auth_generation;
        self.account_state = AccountState::Checking;
        let receivers: Vec<_> = candidates
            .into_iter()
            .map(|candidate| worker.validate_session(candidate))
            .collect();
        cx.spawn(async move |this, cx| {
            let mut result = Err(SessionAuthError::Transport(
                kick_live::TransportFailure::Other,
            ));
            for receiver in receivers {
                result = receiver.await.unwrap_or(Err(SessionAuthError::Transport(
                    kick_live::TransportFailure::Other,
                )));
                if result.is_ok() {
                    break;
                }
            }
            let storage_error = if result.is_ok() && save_on_success {
                credentials::save_cookie(&input).err()
            } else if !save_on_success
                && matches!(&result, Err(SessionAuthError::HttpStatus(401 | 403)))
            {
                let _ = credentials::delete_cookie();
                None
            } else {
                None
            };

            let _ = this.update(cx, |this, cx| {
                if this.auth_generation != generation {
                    return;
                }
                this.account_state = match (result, storage_error) {
                    (Ok(user), None) => AccountState::SignedIn {
                        username: user.username,
                    },
                    (Ok(_), Some(_)) => {
                        AccountState::Failed(tr("Keychainに保存できません").to_owned())
                    }
                    (Err(error), _) => {
                        let reason = session_auth_failure_message(&error);
                        this.show_toast(format!("ログインできません: {reason}"), cx);
                        AccountState::Failed(reason)
                    }
                };
                if matches!(this.account_state, AccountState::SignedIn { .. }) {
                    this.start_following_load(cx);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn toggle_playback(&mut self, cx: &mut Context<Self>) {
        let result = self.player_host.borrow_mut().toggle_play_pause();
        match result {
            Ok(()) => {}
            Err(PlayerError::NoCurrentItem) => {
                if !self.selected_channel_slug.is_empty() {
                    self.retry_selected_channel(cx);
                }
            }
            Err(error) => {
                self.player_host.borrow_mut().status = t!(
                    "AVPlayer操作に失敗: {error:?}",
                    "AVPlayer operation failed: {error:?}"
                );
            }
        }
    }

    fn toggle_mute(&mut self) {
        if let Err(error) = self.player_host.borrow_mut().toggle_mute() {
            self.player_host.borrow_mut().status = t!(
                "AVPlayerミュート操作に失敗: {error:?}",
                "AVPlayer mute failed: {error:?}"
            );
        }
    }

    fn toggle_video_fullscreen(&mut self, cx: &mut Context<Self>) {
        self.video_fullscreen = !self.video_fullscreen;
        cx.notify();
    }

    fn send_chat_message(&mut self, cx: &mut Context<Self>) {
        let raw = self.chat_input_host.borrow().text();
        // Match the web client: trim, normalise the heart shortcode, and fold newlines.
        let content = raw
            .trim()
            .replace("<3", "\u{1F49A}")
            .replace(['\r', '\n'], " ");
        if content.chars().count() > 500 {
            self.chat_input_notice = Some(tr("メッセージが長すぎます（500文字まで）").to_owned());
            cx.notify();
            return;
        }
        if content.is_empty() {
            self.chat_input_notice = Some(tr("メッセージを入力してください").to_owned());
            cx.notify();
            return;
        }
        if !matches!(self.account_state, AccountState::SignedIn { .. }) {
            self.chat_input_notice = Some(tr("送信にはログインが必要です").to_owned());
            cx.notify();
            return;
        }
        let Some(chatroom_id) = self.selected_chatroom_id else {
            self.chat_input_notice = Some(tr("チャットルームが未接続です").to_owned());
            cx.notify();
            return;
        };
        if self.chat_send_pending {
            return;
        }
        let Some(worker) = self.live_worker.as_mut() else {
            self.chat_input_notice = Some(tr("通信ワーカーを利用できません").to_owned());
            cx.notify();
            return;
        };
        self.chat_send_confirmation = false;
        self.chat_send_pending = true;
        self.chat_input_notice = Some(tr("送信中…").to_owned());
        let receiver = worker.send_chat(chatroom_id, content);
        cx.spawn(async move |this, cx| {
            let result = receiver.await.unwrap_or(Err(KickApiError::SessionRequired));
            let _ = this.update(cx, |this, cx| {
                this.chat_send_pending = false;
                match result {
                    Ok(()) => {
                        this.chat_input_host.borrow().set_text("");
                        this.chat_input_notice = Some(tr("送信しました").to_owned());
                    }
                    Err(error) => {
                        this.chat_input_notice = Some(chat_send_error_message(error));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn open_chat_context_menu(
        &mut self,
        message: &ChatMessage,
        position: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.chat_context_menu = Some(ChatContextMenu {
            author: message.author.clone(),
            author_slug: message.author_slug.clone(),
            text: message.text.clone(),
            position,
        });
        self.suppress_menu_close = true;
        cx.notify();
    }

    fn close_chat_context_menu(&mut self, cx: &mut Context<Self>) {
        self.chat_context_menu = None;
        cx.notify();
    }

    fn copy_chat_message(&mut self, text: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        self.chat_context_menu = None;
        self.chat_input_notice = Some(tr("コメントをコピーしました").to_owned());
        cx.notify();
    }

    fn open_author_page(&mut self, slug: String, author: String, cx: &mut Context<Self>) {
        let summary = StreamSummary {
            channel_slug: slug.clone(),
            channel_name: author,
            title: String::new(),
            category: None,
            viewer_count: None,
            thumbnail_url: None,
            profile_picture_url: None,
        };
        self.select_summary(summary, cx);
        self.open_channel_page(cx);
        self.chat_context_menu = None;
        cx.notify();
    }

    fn open_channel_page(&mut self, cx: &mut Context<Self>) {
        self.set_theater_mode(false);
        self.channel_page_forward = false;
        self.feed_forward = false;
        self.channel_page_visible = true;
        self.channel_page_tab = ChannelPageTab::Home;
        let slug = self.selected_channel_slug.clone();
        if !slug.is_empty() {
            self.start_channel_page_load(slug, cx);
        }
        cx.notify();
    }

    fn close_channel_page(&mut self, cx: &mut Context<Self>) {
        if !self.channel_page_visible {
            return;
        }
        self.channel_page_visible = false;
        self.channel_page_forward = true;
        self.feed_forward = false;
        cx.notify();
    }

    fn navigate_back(&mut self, cx: &mut Context<Self>) {
        if self.channel_page_visible {
            self.close_channel_page(cx);
            return;
        }
        if self.feed_visible {
            self.feed_visible = false;
            self.feed_forward = true;
            cx.notify();
            return;
        }
        if self.selected_nav != "ホーム" {
            self.selected_nav = "ホーム";
            self.feed_visible = false;
            self.feed_forward = true;
            cx.notify();
        }
    }

    fn navigate_forward(&mut self, cx: &mut Context<Self>) {
        if self.channel_page_forward && !self.channel_page_visible {
            self.channel_page_forward = false;
            self.open_channel_page(cx);
            return;
        }
        if self.feed_forward {
            self.feed_forward = false;
            self.selected_nav = "ホーム";
            self.feed_visible = true;
            cx.notify();
        }
    }

    fn start_channel_page_load(&mut self, slug: String, cx: &mut Context<Self>) {
        self.channel_page_generation = self.channel_page_generation.wrapping_add(1);
        let generation = self.channel_page_generation;
        self.channel_page_state = ChannelPageState::Loading;
        self.clips_loading_more = false;
        self.clips_error = None;
        if let Some(worker) = self.live_worker.as_mut() {
            worker.cancel_clips_page();
        }
        let Some(worker) = self.live_worker.as_mut() else {
            self.channel_page_state =
                ChannelPageState::Failed(tr("通信ワーカーを利用できません").to_owned());
            cx.notify();
            return;
        };
        let receiver = worker.request_channel_page(slug);
        cx.spawn(async move |this, cx| {
            let result = receiver.await.unwrap_or(Err(LiveError::WorkerUnavailable));
            let _ = this.update(cx, |this, cx| {
                if generation != this.channel_page_generation {
                    return;
                }
                this.channel_page_state = match result {
                    Ok(data) => ChannelPageState::Ready(data),
                    Err(error) => ChannelPageState::Failed(error.user_message()),
                };
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn load_more_clips(&mut self, cx: &mut Context<Self>) {
        if self.clips_loading_more {
            return;
        }
        let Some(cursor) = (match &self.channel_page_state {
            ChannelPageState::Ready(data) => data.clips_cursor.clone(),
            _ => None,
        }) else {
            return;
        };
        let slug = self.selected_channel_slug.clone();
        if slug.is_empty() {
            return;
        }
        let generation = self.channel_page_generation;
        let Some(worker) = self.live_worker.as_mut() else {
            self.clips_error = Some(tr("通信ワーカーを利用できません").to_owned());
            cx.notify();
            return;
        };
        let receiver = worker.request_channel_clips_page(slug, cursor);
        self.clips_loading_more = true;
        self.clips_error = None;
        cx.spawn(async move |this, cx| {
            let result = receiver.await.unwrap_or(Err(LiveError::WorkerUnavailable));
            let _ = this.update(cx, |this, cx| {
                if generation != this.channel_page_generation {
                    return;
                }
                this.clips_loading_more = false;
                match result {
                    Ok(page) => {
                        if let ChannelPageState::Ready(data) = &mut this.channel_page_state {
                            merge_clips(&mut data.clips, page.clips, 24);
                            data.clips_cursor = page.next_cursor;
                        }
                    }
                    Err(error) => {
                        this.clips_error = Some(error.user_message().to_owned());
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn toggle_follow_slug(&mut self, slug: String, cx: &mut Context<Self>) {
        if self.follow_action_pending {
            return;
        }
        if !matches!(self.account_state, AccountState::SignedIn { .. }) {
            self.begin_login(cx);
            return;
        }
        let following = self
            .followed_channels
            .iter()
            .any(|channel| channel.channel_slug == slug);
        let Some(worker) = self.live_worker.as_ref() else {
            self.follow_error = Some(tr("通信ワーカーを利用できません").to_owned());
            return;
        };
        let receiver = worker.set_channel_follow(slug, !following);
        self.follow_action_pending = true;
        self.follow_error = None;
        self.chat_context_menu = None;
        cx.spawn(async move |this, cx| {
            let result = receiver.await.unwrap_or(Err(LiveError::WorkerUnavailable));
            let _ = this.update(cx, |this, cx| {
                this.follow_action_pending = false;
                match result {
                    Ok(()) => this.start_following_load(cx),
                    Err(error) => this.follow_error = Some(error.user_message()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn block_author(&mut self, author: String, cx: &mut Context<Self>) {
        self.blocked_authors.insert(author.clone());
        let filtered: Vec<ChatMessage> = self
            .chat_messages
            .iter()
            .filter(|message| !self.blocked_authors.contains(&message.author))
            .cloned()
            .collect();
        self.chat_messages = Arc::from(filtered);
        self.chat_state.reset(self.chat_messages.len());
        self.chat_state.set_follow_mode(FollowMode::Tail);
        self.chat_state.scroll_to_end();
        self.chat_scrolled_up = false;
        self.chat_context_menu = None;
        self.chat_input_notice = Some(t!(
            "{author} をローカルで非表示にしました",
            "Hid {author} locally"
        ));
        cx.notify();
    }

    fn select_summary(&mut self, stream: StreamSummary, cx: &mut Context<Self>) {
        self.save_vod_position();
        self.vod_playing = None;
        self.vod_url = None;
        self.vod_ended = false;
        self.vod_key = None;
        self.selected_nav = "";
        self.feed_visible = false;
        self.channel_page_visible = false;
        self.channel_page_forward = false;
        self.feed_forward = false;
        if !self
            .streams
            .iter()
            .any(|existing| existing.channel_slug == stream.channel_slug)
        {
            self.streams.push(stream.clone());
        }
        self.select_stream(&stream.channel_slug, cx);
    }

    fn select_stream(&mut self, channel_slug: &str, cx: &mut Context<Self>) {
        if !self
            .streams
            .iter()
            .any(|stream| stream.channel_slug == channel_slug)
        {
            return;
        }
        if self.selected_channel_slug == channel_slug {
            if should_retry_channel_on_reselect(self.channel_state) {
                self.retry_selected_channel(cx);
            }
            return;
        }

        self.selected_channel_slug = channel_slug.to_owned();
        self.selected_channel_id = None;
        self.selected_channel_details = SelectedChannelDetails::default();
        self.channel_state = ChannelLookupState::Loading;
        self.recent_streams.retain(|recent| recent != channel_slug);
        self.recent_streams.insert(0, channel_slug.to_owned());
        self.recent_streams.truncate(6);

        self.reset_chat_history(ChatHistoryState::Loading);
        self.start_channel_lookup(channel_slug, cx);
    }

    fn reset_chat_history(&mut self, state: ChatHistoryState) {
        self.chat_history_generation = self.chat_history_generation.wrapping_add(1);
        self.chat_messages = Arc::from(Vec::new());
        self.chat_history_state = state;
        self.chat_state.reset(0);
        self.chat_state.set_follow_mode(FollowMode::Tail);
        self.chat_unread_messages = 0;
        self.chat_scrolled_up = false;
        self.pending_chat.clear();
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.streams.is_empty() {
            return;
        }
        let current = self
            .streams
            .iter()
            .position(|stream| stream.channel_slug == self.selected_channel_slug)
            .unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(self.streams.len() as isize) as usize;
        let channel_slug = self.streams[next].channel_slug.clone();
        self.selected_nav = "";
        self.feed_visible = false;
        self.channel_page_visible = false;
        self.select_stream(&channel_slug, cx);
    }

    fn start_search(&mut self, cx: &mut Context<Self>) {
        let query = self.search_query.trim().to_owned();
        if query.is_empty() {
            self.search_generation = self.search_generation.wrapping_add(1);
            self.search_state = SearchState::Idle;
            cx.notify();
            return;
        }
        self.feed_visible = true;
        let Some(worker) = self.live_worker.as_mut() else {
            self.search_state = SearchState::Failed(tr("通信ワーカーを利用できません").to_owned());
            cx.notify();
            return;
        };
        self.search_generation = self.search_generation.wrapping_add(1);
        let generation = self.search_generation;
        self.search_state = SearchState::Loading;
        let receiver = worker.request_search(query);
        cx.spawn(async move |this, cx| {
            let result = receiver.await.unwrap_or(Err(LiveError::WorkerUnavailable));
            let _ = this.update(cx, |this, cx| {
                if generation != this.search_generation {
                    return;
                }
                this.search_state = match result {
                    Ok(results) => SearchState::Ready(results),
                    Err(error) => SearchState::Failed(error.user_message()),
                };
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn refresh_stats(&mut self, cx: &mut Context<Self>) {
        if matches!(self.account_state, AccountState::SignedIn { .. }) {
            self.refresh_following(cx);
        }
        self.refresh_channel_stats(cx);
    }

    fn refresh_channel_stats(&mut self, cx: &mut Context<Self>) {
        let slug = self.selected_channel_slug.clone();
        if slug.is_empty() || !matches!(self.channel_state, ChannelLookupState::Ready { .. }) {
            return;
        }
        let Some(worker) = self.live_worker.as_mut() else {
            return;
        };
        let receiver = worker.request_channel_stats(slug.clone());
        cx.spawn(async move |this, cx| {
            let result = receiver.await.unwrap_or(Err(LiveError::WorkerUnavailable));
            let _ = this.update(cx, |this, cx| {
                if this.selected_channel_slug != slug {
                    return;
                }
                if let Ok(channel) = result {
                    this.apply_channel_stats(channel);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn apply_channel_stats(&mut self, channel: ChannelLookupResult) {
        let viewer_count = channel.viewer_count;
        let title = channel.title.clone();
        let profile_picture_url = channel.profile_picture_url.clone();
        self.selected_channel_details.viewer_count = viewer_count;
        self.selected_channel_details.followers_count = channel.followers_count;
        self.selected_channel_details.title = channel.title;
        self.selected_channel_details.is_live = channel.is_live;
        self.selected_channel_details.profile_picture_url = channel.profile_picture_url;
        self.selected_channel_details.bio = channel.bio;
        self.selected_channel_details.twitter = channel.twitter;
        self.selected_channel_details.instagram = channel.instagram;
        self.selected_channel_details.youtube = channel.youtube;
        self.selected_channel_details.discord = channel.discord;
        self.selected_channel_details.tiktok = channel.tiktok;
        self.selected_channel_details.facebook = channel.facebook;
        self.selected_channel_details.club = channel.club;
        self.selected_channel_details.verified = channel.verified;
        self.selected_channel_details.banner_url = channel.banner_url;
        self.selected_channel_details.offline_banner_url = channel.offline_banner_url;
        self.selected_channel_details.language = channel.language;
        self.selected_channel_details.tags = channel.tags;
        if channel.follower_goal.is_some() {
            self.selected_channel_details.follower_goal = channel.follower_goal;
        }
        self.channel_state = ChannelLookupState::Ready {
            is_live: channel.is_live,
        };
        if let Some(stream) = self
            .streams
            .iter_mut()
            .find(|stream| stream.channel_slug == self.selected_channel_slug)
        {
            stream.viewer_count = viewer_count;
            if let Some(title) = title {
                stream.title = title;
            }
            stream.profile_picture_url = profile_picture_url;
        }
    }

    fn refresh_following(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.account_state, AccountState::SignedIn { .. }) || self.following_refreshing
        {
            return;
        }
        let Some(worker) = self.live_worker.as_mut() else {
            return;
        };
        let generation = self.following_generation;
        let receiver = worker.request_followed_channels(None);
        self.following_refreshing = true;
        cx.spawn(async move |this, cx| {
            let result = receiver.await.unwrap_or(Err(LiveError::WorkerUnavailable));
            let _ = this.update(cx, |this, cx| {
                this.following_refreshing = false;
                if generation != this.following_generation {
                    return;
                }
                if let Ok(page) = result {
                    if this.following_pages_loaded > 1 {
                        // Keep the pages already loaded and merge fresh live/viewer
                        // data from the first page instead of shrinking the sidebar.
                        for updated in page.channels {
                            if let Some(existing) = this
                                .followed_channels
                                .iter_mut()
                                .find(|channel| channel.channel_slug == updated.channel_slug)
                            {
                                existing.is_live = updated.is_live;
                                existing.viewer_count = updated.viewer_count;
                                existing.session_title = updated.session_title;
                                existing.category_name = updated.category_name;
                                existing.profile_picture_url = updated.profile_picture_url;
                            } else {
                                this.followed_channels.push(updated);
                            }
                        }
                    } else {
                        this.followed_channels = page.channels;
                        this.following_cursor = page.next_cursor;
                        this.following_has_more = this.following_cursor.is_some();
                    }
                    this.warm_avatar_cache(
                        this.followed_channels
                            .iter()
                            .filter_map(|channel| channel.profile_picture_url.clone())
                            .collect(),
                        cx,
                    );
                    this.following_state = FollowingState::Ready;
                    this.ensure_followed_visible(cx);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn start_catalog_load(&mut self, cx: &mut Context<Self>) {
        let Some(worker) = self.live_worker.as_mut() else {
            self.catalog_state = CatalogState::Failed(LiveError::WorkerUnavailable);
            return;
        };

        let generation = self.catalog_generation;
        let receiver = worker.request_catalog();
        cx.spawn(async move |this, cx| {
            let result = receiver.await.unwrap_or(Err(LiveError::WorkerUnavailable));
            let _ = this.update(cx, |this, cx| {
                this.apply_catalog_result(generation, result, cx);
            });
        })
        .detach();
    }

    fn retry_catalog(&mut self, cx: &mut Context<Self>) {
        self.player_host.borrow_mut().shutdown_player();
        self.catalog_generation = self.catalog_generation.wrapping_add(1);
        self.catalog_state = CatalogState::Loading;
        self.streams.clear();
        self.selected_channel_slug.clear();
        self.selected_channel_id = None;
        self.recent_streams.clear();
        self.channel_state = ChannelLookupState::NotRequested;
        self.chat_realtime_state = ChatRealtimeState::NoChannel;
        self.reset_chat_history(ChatHistoryState::NoChannel);

        if !self.restart_live_worker() {
            self.catalog_state = CatalogState::Failed(LiveError::WorkerUnavailable);
            cx.notify();
            return;
        }

        self.start_catalog_load(cx);
        cx.notify();
    }

    fn retry_selected_channel(&mut self, cx: &mut Context<Self>) {
        let channel_slug = self.selected_channel_slug.clone();
        if channel_slug.is_empty() {
            return;
        }

        if matches!(self.channel_state, ChannelLookupState::Failed(error) if retry_requires_worker_restart(error))
            && !self.restart_live_worker()
        {
            self.channel_state = ChannelLookupState::Failed(LiveError::WorkerUnavailable);
            self.reset_chat_history(ChatHistoryState::Failed(LiveError::WorkerUnavailable));
            cx.notify();
            return;
        }

        self.start_channel_lookup(&channel_slug, cx);
        cx.notify();
    }

    fn restart_live_worker(&mut self) -> bool {
        self.channel_generation = self.channel_generation.wrapping_add(1);
        self.chat_generation = self.chat_generation.wrapping_add(1);
        self.chat_history_generation = self.chat_history_generation.wrapping_add(1);
        if let Some(worker) = self.live_worker.take() {
            drop(worker);
        }
        self.live_worker = LiveWorker::new().ok();
        self.live_worker.is_some()
    }

    fn apply_catalog_result(
        &mut self,
        generation: u64,
        result: Result<Vec<StreamSummary>, LiveError>,
        cx: &mut Context<Self>,
    ) {
        if generation != self.catalog_generation {
            return;
        }

        match result {
            Ok(streams) => {
                self.catalog_state = CatalogState::Ready;
                self.streams = streams;
                self.warm_avatar_cache(
                    self.streams
                        .iter()
                        .filter_map(|stream| stream.profile_picture_url.clone())
                        .collect(),
                    cx,
                );
                #[cfg(debug_assertions)]
                if let Some(slug) = debug_initial_channel() {
                    if !self
                        .streams
                        .iter()
                        .any(|stream| stream.channel_slug == slug)
                    {
                        self.streams.insert(
                            0,
                            StreamSummary {
                                channel_slug: slug.clone(),
                                channel_name: slug.clone(),
                                title: "debug override".to_owned(),
                                category: None,
                                viewer_count: None,
                                thumbnail_url: None,
                                profile_picture_url: None,
                            },
                        );
                    }
                    self.selected_channel_slug = slug.clone();
                    self.selected_channel_id = None;
                    self.recent_streams = vec![slug.clone()];
                    self.start_channel_lookup(&slug, cx);
                    cx.notify();
                    return;
                }
                // Start on the home feed without selecting a recommended
                // stream. The user chooses a channel explicitly.
                self.selected_channel_slug.clear();
                self.selected_channel_id = None;
                self.channel_state = ChannelLookupState::NotRequested;
                self.chat_realtime_state = ChatRealtimeState::NoChannel;
                self.player_host.borrow_mut().set_channel_failure();
                self.reset_chat_history(ChatHistoryState::NoChannel);
            }
            Err(error) => {
                self.catalog_state = CatalogState::Failed(error);
                self.streams.clear();
                self.selected_channel_slug.clear();
                self.selected_channel_id = None;
                self.channel_state = ChannelLookupState::NotRequested;
                self.chat_realtime_state = ChatRealtimeState::NoChannel;
                self.reset_chat_history(ChatHistoryState::NoChannel);
                self.player_host.borrow_mut().set_channel_failure();
            }
        }
        cx.notify();
    }

    fn start_channel_lookup(&mut self, channel_slug: &str, cx: &mut Context<Self>) {
        self.clear_raid_state();
        self.channel_generation = self.channel_generation.wrapping_add(1);
        self.chat_generation = self.chat_generation.wrapping_add(1);
        let generation = self.channel_generation;
        self.emote_ids_by_name = Arc::new(HashMap::new());
        self.emote_diagnostics.clear();
        self.channel_state = ChannelLookupState::Loading;
        self.selected_channel_id = None;
        self.selected_chatroom_id = None;
        self.chat_send_confirmation = false;
        self.chat_send_pending = false;
        self.emote_picker_open = false;
        self.channel_page_visible = false;
        self.chat_realtime_state = ChatRealtimeState::WaitingForChannel;
        self.chat_dropped_messages = 0;
        self.reward_events_received = 0;
        self.move_events_received = 0;
        self.player_host.borrow_mut().set_channel_loading();
        self.reset_chat_history(ChatHistoryState::Loading);

        let Some(worker) = self.live_worker.as_mut() else {
            self.channel_state = ChannelLookupState::Failed(LiveError::WorkerUnavailable);
            self.chat_realtime_state = ChatRealtimeState::Unavailable;
            self.player_host.borrow_mut().set_channel_failure();
            self.reset_chat_history(ChatHistoryState::Failed(LiveError::WorkerUnavailable));
            return;
        };

        worker.cancel_emotes();
        // Queue stop before the new lookup. The worker awaits ChatSession::stop
        // before it handles the channel request, releasing the old room first.
        worker.stop_chat();
        let receiver = worker.request_channel(channel_slug.to_owned());
        cx.spawn(async move |this, cx| {
            let result = receiver.await.unwrap_or(Err(LiveError::WorkerUnavailable));
            let _ = this.update(cx, |this, cx| {
                if generation != this.channel_generation {
                    return;
                }
                this.apply_channel_result(generation, result, cx);
            });
        })
        .detach();
    }

    fn apply_channel_result(
        &mut self,
        generation: u64,
        result: Result<ChannelLookupResult, LiveError>,
        cx: &mut Context<Self>,
    ) {
        if generation != self.channel_generation {
            return;
        }

        match result {
            Ok(channel) => {
                let channel_id = channel.channel_id;
                let chatroom_id = channel.chatroom_id;
                self.selected_chatroom_id = chatroom_id;
                let is_live = channel.is_live;
                let avatar_url = channel.profile_picture_url.clone();
                self.selected_channel_details = SelectedChannelDetails {
                    profile_picture_url: channel.profile_picture_url,
                    avatar_path: None,
                    followers_count: channel.followers_count,
                    username: channel.username,
                    started_at: channel.started_at,
                    viewer_count: channel.viewer_count,
                    title: channel.title,
                    is_live,
                    bio: channel.bio,
                    twitter: channel.twitter,
                    instagram: channel.instagram,
                    youtube: channel.youtube,
                    discord: channel.discord,
                    tiktok: channel.tiktok,
                    facebook: channel.facebook,
                    club: channel.club,
                    verified: channel.verified,
                    banner_url: channel.banner_url,
                    offline_banner_url: channel.offline_banner_url,
                    language: channel.language,
                    tags: channel.tags,
                    follower_goal: channel.follower_goal,
                };
                if let Some(url) = avatar_url {
                    self.prepare_channel_avatar(url, cx);
                }
                self.player_host.borrow_mut().set_playback_source(
                    channel.playback_source,
                    channel.dvr_url,
                    is_live,
                );
                if self.player_host.borrow().player.is_some() {
                    self.start_player_monitor(cx);
                }
                self.channel_state = ChannelLookupState::Ready { is_live };
                self.selected_channel_id = channel_id;
                let channel_slug = self.selected_channel_slug.clone();
                self.start_emote_lookup(&channel_slug, generation, cx);
                if !is_live {
                    self.chat_realtime_state = ChatRealtimeState::Offline;
                } else if let Some(chatroom_id) = chatroom_id {
                    self.start_chat_session(chatroom_id, generation, cx);
                } else {
                    self.chat_realtime_state = ChatRealtimeState::Unavailable;
                }
                if let Some(channel_id) = channel_id {
                    self.start_history_lookup(channel_id, generation, cx);
                } else {
                    self.reset_chat_history(ChatHistoryState::Failed(LiveError::MissingChannelId));
                }
            }
            Err(error) => {
                self.channel_state = ChannelLookupState::Failed(error);
                self.selected_channel_id = None;
                self.selected_channel_details = SelectedChannelDetails::default();
                self.chat_realtime_state = ChatRealtimeState::Unavailable;
                self.player_host.borrow_mut().set_channel_failure();
                self.reset_chat_history(ChatHistoryState::Failed(error));
            }
        }
        cx.notify();
    }

    fn start_emote_lookup(&mut self, channel_slug: &str, generation: u64, cx: &mut Context<Self>) {
        if generation != self.channel_generation {
            return;
        }
        let Some(worker) = self.live_worker.as_mut() else {
            return;
        };
        let receiver = worker.request_emotes(channel_slug.to_owned());
        cx.spawn(async move |this, cx| {
            let result = receiver.await;
            let _ = this.update(cx, |this, cx| {
                if generation != this.channel_generation {
                    return;
                }
                match result {
                    Ok(Ok(emotes)) => {
                        this.emote_ids_by_name = Arc::new(
                            emotes
                                .iter()
                                .map(|(name, id)| (name.to_ascii_lowercase(), *id))
                                .collect::<HashMap<_, _>>(),
                        );
                        this.emote_order = Arc::new(emotes);
                        this.chat_state.reset(this.chat_messages.len());
                        this.chat_state.set_follow_mode(FollowMode::Tail);
                        this.chat_state.scroll_to_end();
                        this.chat_scrolled_up = false;
                        cx.notify();
                    }
                    Ok(Err(error)) => {
                        this.chat_input_notice = Some(t!(
                            "エモート取得失敗: {}",
                            "Could not load emotes: {}",
                            error.user_message()
                        ));
                        cx.notify();
                    }
                    Err(_) => {}
                }
            });
        })
        .detach();
    }

    fn retry_emotes(&mut self, cx: &mut Context<Self>) {
        let slug = self.selected_channel_slug.clone();
        if slug.is_empty() {
            return;
        }
        let generation = self.channel_generation;
        self.start_emote_lookup(&slug, generation, cx);
    }

    fn start_following_load(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.account_state, AccountState::SignedIn { .. }) {
            self.following_generation = self.following_generation.wrapping_add(1);
            self.following_state = FollowingState::NotRequested;
            self.followed_channels.clear();
            self.following_cursor = None;
            self.following_has_more = false;
            self.following_loading_more = false;
            return;
        }
        self.following_generation = self.following_generation.wrapping_add(1);
        let generation = self.following_generation;
        self.following_state = FollowingState::Loading;
        self.followed_channels.clear();
        self.following_cursor = None;
        self.following_has_more = false;
        self.following_loading_more = false;
        self.following_pages_loaded = 1;
        let Some(worker) = self.live_worker.as_mut() else {
            self.following_state = FollowingState::Failed(LiveError::WorkerUnavailable);
            return;
        };
        let receiver = worker.request_followed_channels(None);
        cx.spawn(async move |this, cx| {
            let result = receiver.await.unwrap_or(Err(LiveError::WorkerUnavailable));
            let _ = this.update(cx, |this, cx| {
                if generation != this.following_generation {
                    return;
                }
                match result {
                    Ok(page) => {
                        this.followed_channels = page.channels;
                        this.warm_avatar_cache(
                            this.followed_channels
                                .iter()
                                .filter_map(|channel| channel.profile_picture_url.clone())
                                .collect(),
                            cx,
                        );
                        this.following_cursor = page.next_cursor;
                        this.following_has_more = this.following_cursor.is_some();
                        this.following_state = FollowingState::Ready;
                        this.ensure_followed_visible(cx);
                    }
                    Err(error) => {
                        this.following_state = FollowingState::Failed(error);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn load_more_following(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.following_state, FollowingState::Ready)
            || !self.following_has_more
            || self.following_loading_more
        {
            return;
        }
        let Some(cursor) = self.following_cursor.clone() else {
            return;
        };
        let generation = self.following_generation;
        let Some(worker) = self.live_worker.as_mut() else {
            return;
        };
        let receiver = worker.request_followed_channels(Some(cursor.clone()));
        self.following_loading_more = true;
        self.following_pages_loaded = self.following_pages_loaded.saturating_add(1);
        cx.spawn(async move |this, cx| {
            let result = receiver.await.unwrap_or(Err(LiveError::WorkerUnavailable));
            let _ = this.update(cx, |this, cx| {
                if generation != this.following_generation {
                    return;
                }
                this.following_loading_more = false;
                if let Ok(page) = result {
                    let mut seen = this
                        .followed_channels
                        .iter()
                        .map(|channel| channel.channel_slug.clone())
                        .collect::<HashSet<_>>();
                    for channel in page.channels {
                        if seen.insert(channel.channel_slug.clone()) {
                            this.followed_channels.push(channel);
                        }
                    }
                    let next = page.next_cursor;
                    // Stop if the server repeats a cursor instead of advancing.
                    this.following_has_more = next.is_some() && next != Some(cursor);
                    this.following_cursor = next;
                    this.ensure_followed_visible(cx);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn select_following_tab(&mut self, tab: FollowingTab, cx: &mut Context<Self>) {
        self.following_tab = tab;
        if matches!(self.following_state, FollowingState::NotRequested) {
            self.start_following_load(cx);
        }
        cx.notify();
    }

    fn toggle_selected_channel_follow(&mut self, cx: &mut Context<Self>) {
        if self.follow_action_pending {
            return;
        }
        if !matches!(self.account_state, AccountState::SignedIn { .. }) {
            self.begin_login(cx);
            return;
        }
        let slug = self.selected_channel_slug.clone();
        if slug.is_empty() {
            return;
        }
        let following = self
            .followed_channels
            .iter()
            .any(|channel| channel.channel_slug == slug);
        let Some(worker) = self.live_worker.as_ref() else {
            self.follow_error = Some(tr("通信ワーカーを利用できません").to_owned());
            return;
        };
        let receiver = worker.set_channel_follow(slug, !following);
        self.follow_action_pending = true;
        self.follow_error = None;
        cx.spawn(async move |this, cx| {
            let result = receiver.await.unwrap_or(Err(LiveError::WorkerUnavailable));
            let _ = this.update(cx, |this, cx| {
                this.follow_action_pending = false;
                match result {
                    Ok(()) => this.start_following_load(cx),
                    Err(error) => this.follow_error = Some(error.user_message()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn start_chat_session(&mut self, chatroom_id: u64, generation: u64, cx: &mut Context<Self>) {
        if generation != self.channel_generation {
            return;
        }
        self.chat_realtime_state = ChatRealtimeState::Connecting { attempt: 0 };
        self.chat_dropped_messages = 0;
        let Some(worker) = self.live_worker.as_mut() else {
            self.chat_realtime_state = ChatRealtimeState::Failed(LiveError::WorkerUnavailable);
            return;
        };
        worker.start_chat(chatroom_id, self.selected_channel_id, self.chat_generation);
        self.start_chat_monitor(cx);
    }

    fn start_chat_monitor(&mut self, cx: &mut Context<Self>) {
        if self.chat_monitor_started {
            return;
        }
        self.chat_monitor_started = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(CHAT_UI_BATCH_INTERVAL).await;
                let update = this.update(cx, |view, cx| {
                    let input_dirty = view.chat_input_host.borrow().take_dirty();
                    let input_submitted = view.chat_input_host.borrow().take_submit();
                    if input_dirty {
                        cx.notify();
                    }
                    if input_submitted {
                        view.send_chat_message(cx);
                    }
                    let events = view
                        .live_worker
                        .as_mut()
                        .map(|worker| worker.drain_chat_events())
                        .unwrap_or_default();
                    let dropped_messages = view
                        .live_worker
                        .as_ref()
                        .map(LiveWorker::dropped_chat_messages)
                        .unwrap_or(view.chat_dropped_messages);
                    if (!events.is_empty() || dropped_messages != view.chat_dropped_messages)
                        && view.apply_chat_events(events, dropped_messages, cx)
                    {
                        cx.notify();
                    }
                });
                if update.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn apply_chat_events(
        &mut self,
        events: Vec<ChatWorkerEvent>,
        dropped_messages: u64,
        cx: &mut Context<Self>,
    ) -> bool {
        let mut state = self.chat_realtime_state;
        let mut state_changed = false;
        let mut incoming_messages = Vec::new();
        let generation = self.chat_generation;
        for event in events {
            match event {
                ChatWorkerEvent::Status {
                    generation: event_generation,
                    status,
                } if event_generation == generation => {
                    let next_state = ChatRealtimeState::from(status);
                    state_changed |= state != next_state;
                    state = next_state;
                }
                ChatWorkerEvent::Failed {
                    generation: event_generation,
                    error,
                } if event_generation == generation => {
                    let next_state = ChatRealtimeState::Failed(error);
                    state_changed |= state != next_state;
                    state = next_state;
                }
                ChatWorkerEvent::Message {
                    generation: event_generation,
                    message,
                } if event_generation == generation => {
                    if message.reward.is_some() {
                        self.reward_events_received = self.reward_events_received.saturating_add(1);
                    }
                    incoming_messages.push(message);
                }
                ChatWorkerEvent::Raid {
                    generation: event_generation,
                    raid,
                } if event_generation == generation => {
                    self.handle_raid_event(raid, cx);
                    state_changed = true;
                }
                ChatWorkerEvent::MoveToChannel {
                    generation: event_generation,
                    event,
                } if event_generation == generation => {
                    self.move_events_received = self.move_events_received.saturating_add(1);
                    self.handle_move_to_channel(event, cx);
                    state_changed = true;
                }
                _ => {}
            }
        }
        let mut changed = state_changed;
        if !incoming_messages.is_empty() {
            if self.chat_delay_seconds > 0 {
                self.enqueue_delayed_chat(incoming_messages, cx);
            } else {
                self.append_chat_messages(incoming_messages);
            }
            changed = true;
        }
        if state_changed {
            self.chat_realtime_state = state;
        }
        if self.chat_dropped_messages != dropped_messages {
            self.chat_dropped_messages = dropped_messages;
            changed = true;
        }
        if changed {
            self.sync_chat_snapshot(cx);
        }
        changed
    }

    /// Appends messages to the visible chat list and keeps the scroll position
    /// and unread counter consistent with the previous tail state.
    fn append_chat_messages(&mut self, incoming: Vec<ChatMessage>) {
        if incoming.is_empty() {
            return;
        }
        if self.danmaku_enabled {
            let now = Instant::now();
            let ready = self
                .last_danmaku_at
                .is_none_or(|last| now.duration_since(last) >= Duration::from_millis(180));
            if ready {
                let text = incoming.iter().rev().find_map(|message| {
                    let text = danmaku_text(&message.text);
                    (!text.is_empty() && text.chars().count() <= 80).then_some(text)
                });
                if let Some(text) = text {
                    if let Some(player) = self.player_host.borrow().player.as_ref() {
                        let _ = player.push_danmaku(&text);
                        self.last_danmaku_at = Some(now);
                    }
                }
            }
        }
        let was_following_tail = self.chat_state.is_following_tail() && !self.chat_scrolled_up;
        let new_message_count = count_new_chat_messages(&self.chat_messages, &incoming);
        let messages = merge_chat_messages(&self.chat_messages, incoming);
        sync_chat_list_state(
            &self.chat_state,
            &self.chat_messages,
            &messages,
            was_following_tail,
        );
        self.chat_messages = Arc::from(messages);
        if was_following_tail {
            self.chat_unread_messages = 0;
        } else {
            self.chat_unread_messages = self.chat_unread_messages.saturating_add(new_message_count);
        }
    }

    /// Holds real-time messages until `chat_delay_seconds` has elapsed so the
    /// visible chat matches the delayed video instead of spoiling it.
    fn enqueue_delayed_chat(&mut self, messages: Vec<ChatMessage>, cx: &mut Context<Self>) {
        if messages.is_empty() {
            return;
        }
        let delay = Duration::from_secs(self.chat_delay_seconds);
        let now = Instant::now();
        for message in messages {
            self.pending_chat.push_back(PendingChatMessage {
                due: now + delay,
                message,
            });
        }
        while self.pending_chat.len() > CHAT_PENDING_MAX_MESSAGES {
            self.pending_chat.pop_front();
        }
        self.ensure_chat_delay_task(cx);
    }

    fn ensure_chat_delay_task(&mut self, cx: &mut Context<Self>) {
        if self.chat_delay_task_running {
            return;
        }
        self.chat_delay_task_running = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(CHAT_DELAY_TICK).await;
                let Ok(keep_running) = this.update(cx, |this, cx| {
                    if this.release_due_chat() {
                        cx.notify();
                    }
                    !this.pending_chat.is_empty()
                }) else {
                    return;
                };
                if !keep_running {
                    break;
                }
            }
            let _ = this.update(cx, |this, _cx| {
                this.chat_delay_task_running = false;
            });
        })
        .detach();
    }

    /// Releases every held message whose delay has elapsed.
    fn release_due_chat(&mut self) -> bool {
        let due = take_due_chat(&mut self.pending_chat, Instant::now());
        if due.is_empty() {
            return false;
        }
        self.append_chat_messages(due);
        true
    }

    fn flush_pending_chat(&mut self) -> bool {
        if self.pending_chat.is_empty() {
            return false;
        }
        let messages: Vec<ChatMessage> = self
            .pending_chat
            .drain(..)
            .map(|pending| pending.message)
            .collect();
        self.append_chat_messages(messages);
        true
    }

    fn save_current_settings(&self) {
        save_settings(
            self.chat_delay_seconds,
            self.debug_mode,
            current_language(),
            &self.notify_channels,
            self.danmaku_enabled,
            self.danmaku_font_size,
            self.danmaku_opacity,
            self.default_quality,
            self.followed_limit,
            self.raid_display_seconds,
        );
    }

    fn set_raid_display_seconds(&mut self, seconds: u64, cx: &mut Context<Self>) {
        let seconds = seconds.clamp(5, 60);
        if self.raid_display_seconds == seconds {
            return;
        }
        self.raid_display_seconds = seconds;
        self.save_current_settings();
        cx.notify();
    }

    fn set_default_quality(&mut self, quality: PlayerQualityCap, cx: &mut Context<Self>) {
        if self.default_quality == quality {
            return;
        }
        self.default_quality = quality;
        {
            let mut host = self.player_host.borrow_mut();
            host.quality_cap = quality;
            if let Some(player) = host.player.as_ref() {
                let _ = player.set_quality(quality);
            }
        }
        self.save_current_settings();
        cx.notify();
    }

    fn set_settings_tab(&mut self, tab: SettingsTab, cx: &mut Context<Self>) {
        if self.settings_tab == tab {
            return;
        }
        self.settings_tab = tab;
        cx.notify();
    }

    fn set_followed_limit(&mut self, limit: usize, cx: &mut Context<Self>) {
        let limit = limit.clamp(1, 50);
        if self.followed_limit == limit {
            return;
        }
        self.followed_limit = limit;
        self.save_current_settings();
        self.ensure_followed_visible(cx);
        cx.notify();
    }

    /// Loads further followed pages until the sidebar can fill its configured
    /// row count, so raising the limit actually shows more channels.
    fn live_followed_count(&self) -> usize {
        self.followed_channels
            .iter()
            .filter(|channel| channel.is_live)
            .count()
    }

    fn ensure_followed_visible(&mut self, cx: &mut Context<Self>) {
        // The sidebar lists live channels only, so keep paging until enough live
        // channels are loaded. The page cap bounds requests for users who follow
        // many offline channels.
        const MAX_AUTO_PAGES: usize = 12;
        if !matches!(self.following_state, FollowingState::Ready)
            || self.live_followed_count() >= self.followed_limit
            || !self.following_has_more
            || self.following_loading_more
            || self.following_pages_loaded >= MAX_AUTO_PAGES
        {
            return;
        }
        self.load_more_following(cx);
    }

    fn set_chat_delay(&mut self, seconds: u64, cx: &mut Context<Self>) {
        let seconds = seconds.min(CHAT_DELAY_MAX_SECONDS);
        if self.chat_delay_seconds == seconds {
            return;
        }
        self.chat_delay_seconds = seconds;
        self.save_current_settings();
        if seconds == 0 {
            self.flush_pending_chat();
        }
        cx.notify();
    }

    fn toggle_account_menu(&mut self, cx: &mut Context<Self>) {
        self.account_menu_open = !self.account_menu_open;
        self.chat_context_menu = None;
        self.suppress_menu_close = true;
        cx.notify();
    }

    fn set_danmaku(&mut self, enabled: bool, font_size: f32, opacity: f32, cx: &mut Context<Self>) {
        self.danmaku_enabled = enabled;
        self.danmaku_font_size = font_size.clamp(12.0, 36.0);
        self.danmaku_opacity = opacity.clamp(0.2, 1.0);
        self.player_host
            .borrow_mut()
            .set_danmaku_config(self.danmaku_font_size, self.danmaku_opacity);
        self.save_current_settings();
        cx.notify();
    }

    fn remove_notify_channel(&mut self, slug: String, cx: &mut Context<Self>) {
        if self.notify_channels.remove(&slug) {
            self.save_current_settings();
            self.start_notifications(cx);
            cx.notify();
        }
    }

    fn toggle_channel_notifications(&mut self, slug: String, cx: &mut Context<Self>) {
        if slug.is_empty() {
            return;
        }
        let enabling = if self.notify_channels.contains(&slug) {
            self.notify_channels.remove(&slug);
            false
        } else {
            self.notify_channels.insert(slug);
            true
        };
        if enabling && self.notification_permission == 0 {
            let _ = kick_player_macos::request_notification_authorization();
            self.refresh_notification_permission(cx);
        }
        self.save_current_settings();
        self.start_notifications(cx);
        cx.notify();
    }

    fn set_language_setting(&mut self, language: Language, cx: &mut Context<Self>) {
        if current_language() == language {
            return;
        }
        set_language(language);
        self.player_host.borrow_mut().set_language(language);
        self.save_current_settings();
        cx.set_menus(app_menus());
        if let Some(window) = self.debug_window {
            let title = tr("Kick Native デバッグ");
            let _ = window.update(cx, |_, window, _| window.set_window_title(title));
        }
        cx.notify();
    }

    fn set_debug_mode(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.debug_mode == enabled {
            return;
        }
        self.debug_mode = enabled;
        self.save_current_settings();
        if enabled {
            self.open_debug_window(cx);
            self.ensure_debug_stats_task(cx);
        } else if let Some(window) = self.debug_window.take() {
            let _ = window.update(cx, |_, window, _| window.remove_window());
            self.debug_stats_started = false;
        }
        cx.notify();
    }

    fn open_debug_window(&mut self, cx: &mut Context<Self>) {
        if let Some(window) = self.debug_window {
            if window
                .update(cx, |_, window, _| window.activate_window())
                .is_ok()
            {
                return;
            }
            self.debug_window = None;
        }
        let stats = self.debug_stats.clone();
        let bounds = Bounds::centered(None, size(px(460.0), px(420.0)), cx);
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(tr("Kick Native デバッグ").into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            move |_window, cx| cx.new(|cx| DebugView::new(stats.clone(), cx)),
        );
        match opened {
            Ok(window) => self.debug_window = Some(window),
            Err(_) => {
                self.debug_mode = false;
                self.save_current_settings();
            }
        }
    }

    fn ensure_debug_stats_task(&mut self, cx: &mut Context<Self>) {
        if self.debug_stats_started {
            return;
        }
        self.debug_stats_started = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let Ok(keep_running) = this.update(cx, |this, cx| {
                    if !this.debug_mode {
                        return false;
                    }
                    this.refresh_debug_stats();
                    cx.notify();
                    true
                }) else {
                    return;
                };
                if !keep_running {
                    break;
                }
            }
        })
        .detach();
    }

    fn refresh_debug_stats(&mut self) {
        let (resident_bytes, peak_bytes) = process_memory_bytes().unwrap_or((0, 0));
        let player_status = self.player_host.borrow().status.clone();
        let stats = DebugStats {
            resident_bytes,
            peak_bytes,
            chat_messages: self.chat_messages.len(),
            chat_pending: self.pending_chat.len(),
            chat_dropped: self.chat_dropped_messages,
            reward_events: self.reward_events_received,
            move_events: self.move_events_received,
            emote_count: self.emote_ids_by_name.len(),
            streams: self.streams.len(),
            channel: self.selected_channel_slug.clone(),
            player_status,
            catalog: self.catalog_state.label(),
            realtime: self.chat_realtime_state.label(),
            history: match self.chat_history_state {
                ChatHistoryState::Ready { message_count } => {
                    t!("{message_count} 件", "{message_count}")
                }
                state => state.label(),
            },
        };
        if let Ok(mut guard) = self.debug_stats.lock() {
            *guard = stats;
        }
    }

    fn open_rewards(&mut self, cx: &mut Context<Self>) {
        if self.rewards_open {
            self.rewards_open = false;
            cx.notify();
            return;
        }
        let slug = self.selected_channel_slug.clone();
        if slug.is_empty() {
            return;
        }
        self.rewards_open = true;
        self.rewards_loading = true;
        self.rewards_error = None;
        let Some(worker) = self.live_worker.as_mut() else {
            self.rewards_loading = false;
            self.rewards_error = Some(tr("通信ワーカーを利用できません").to_owned());
            cx.notify();
            return;
        };
        let receiver = worker.request_rewards(slug);
        cx.spawn(async move |this, cx| {
            let result = receiver.await.unwrap_or(Err(LiveError::WorkerUnavailable));
            let _ = this.update(cx, |this, cx| {
                this.rewards_loading = false;
                match result {
                    Ok(rewards) => this.rewards = rewards,
                    Err(error) => {
                        this.rewards_error = Some(error.user_message().to_owned());
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn redeem_reward(&mut self, reward_id: String, cx: &mut Context<Self>) {
        let slug = self.selected_channel_slug.clone();
        if slug.is_empty() {
            return;
        }
        let Some(worker) = self.live_worker.as_mut() else {
            return;
        };
        let receiver = worker.request_redeem_reward(slug, reward_id);
        cx.spawn(async move |this, cx| {
            let result = receiver.await.unwrap_or(Err(LiveError::WorkerUnavailable));
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.chat_input_notice = Some(tr("交換しました").to_owned());
                        this.rewards_open = false;
                    }
                    Err(error) => {
                        this.rewards_error = Some(error.user_message().to_owned());
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn play_vod_uuid(&mut self, uuid: String, title: String, cx: &mut Context<Self>) {
        if uuid.is_empty() {
            return;
        }
        let uuid_for_resume = uuid.clone();
        let Some(worker) = self.live_worker.as_mut() else {
            return;
        };
        let receiver = worker.request_vod_source(uuid);
        cx.spawn(async move |this, cx| {
            let result = receiver.await.unwrap_or(Err(LiveError::WorkerUnavailable));
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(url) => {
                        this.vod_url = Some(url.clone());
                        this.vod_ended = false;
                        this.player_host.borrow_mut().play_vod(&url);
                        this.vod_key = Some(uuid_for_resume.clone());
                        if let Some(seconds) = this.vod_positions.get(&uuid_for_resume).copied()
                            && seconds > 5.0
                            && let Some(player) = this.player_host.borrow().player.as_ref()
                        {
                            let _ = player.seek(seconds);
                        }
                        this.vod_playing = Some(title);
                    }
                    Err(_) => {
                        this.chat_input_notice = Some(tr("過去配信を再生できません").to_owned());
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn play_clip_url(&mut self, url: String, title: String, cx: &mut Context<Self>) {
        if url.is_empty() {
            return;
        }
        // A clip is opened from the channel page. Leave that page and the
        // feed/following views so the player surface is resumed and visible;
        // otherwise the clip's audio can start behind a suspended view.
        self.channel_page_visible = false;
        self.selected_nav = "ホーム";
        self.feed_visible = false;
        self.vod_url = Some(url.clone());
        self.vod_ended = false;
        self.player_host.borrow_mut().play_vod(&url);
        self.vod_playing = Some(title);
        cx.notify();
    }

    fn replay_vod(&mut self, cx: &mut Context<Self>) {
        let (Some(url), Some(title)) = (self.vod_url.clone(), self.vod_playing.clone()) else {
            return;
        };
        self.vod_ended = false;
        self.channel_page_visible = false;
        self.selected_nav = "ホーム";
        self.feed_visible = false;
        self.player_host.borrow_mut().play_vod(&url);
        self.vod_playing = Some(title);
        cx.notify();
    }

    fn save_vod_position(&mut self) {
        if self.vod_ended {
            return;
        }
        let Some(key) = self.vod_key.clone() else {
            return;
        };
        let Some(seconds) = self
            .player_host
            .borrow()
            .controls_snapshot()
            .and_then(|snapshot| snapshot.current_time_seconds)
        else {
            return;
        };
        if seconds.is_finite() && seconds > 5.0 {
            self.vod_positions.insert(key, seconds);
            save_vod_positions(&self.vod_positions);
        }
    }

    fn return_to_live(&mut self, cx: &mut Context<Self>) {
        self.save_vod_position();
        self.vod_url = None;
        self.vod_ended = false;
        if self.vod_playing.take().is_none() {
            return;
        }
        let slug = self.selected_channel_slug.clone();
        self.start_channel_lookup(&slug, cx);
        cx.notify();
    }

    fn pop_out_chat(&mut self, cx: &mut Context<Self>) {
        if self.chat_popped_out {
            return;
        }
        let snapshot = self.chat_snapshot.clone();
        let shell = cx.entity().downgrade();
        let bounds = Bounds::centered(None, size(px(380.0), px(640.0)), cx);
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(tr("チャット").into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            move |_window, cx| cx.new(|cx| ChatPopout::new(snapshot.clone(), shell.clone(), cx)),
        );
        if let Ok(window) = opened {
            self.chat_popout = Some(window);
            self.chat_popped_out = true;
            self.sync_chat_snapshot(cx);
            self.start_popout_watch(cx);
            cx.notify();
        }
    }

    fn restore_chat(&mut self, cx: &mut Context<Self>) {
        if let Some(window) = self.chat_popout.take() {
            let _ = window.update(cx, |_, window, _| window.remove_window());
        }
        if self.chat_popped_out {
            self.chat_popped_out = false;
            cx.notify();
        }
    }

    fn sync_chat_snapshot(&mut self, cx: &mut Context<Self>) {
        if !self.chat_popped_out {
            return;
        }
        if let Ok(mut guard) = self.chat_snapshot.lock() {
            *guard = ChatSnapshot {
                messages: self.chat_messages.clone(),
                emotes: self.emote_ids_by_name.clone(),
                history: self.chat_history_state,
                unread: self.chat_unread_messages,
            };
        }
        if let Some(window) = self.chat_popout {
            let _ = window.update(cx, |view, _, cx| view.sync(cx));
        }
    }

    fn start_popout_watch(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(400))
                    .await;
                let Ok(keep) = this.update(cx, |this, cx| {
                    let Some(window) = this.chat_popout else {
                        return false;
                    };
                    if window.update(cx, |_, _, _| ()).is_err() {
                        this.chat_popout = None;
                        this.chat_popped_out = false;
                        cx.notify();
                        return false;
                    }
                    true
                }) else {
                    return;
                };
                if !keep {
                    break;
                }
            }
        })
        .detach();
    }

    fn open_settings(&mut self, cx: &mut Context<Self>) {
        self.settings_open = true;
        self.chat_context_menu = None;
        self.emote_picker_open = false;
        cx.notify();
    }

    fn close_settings(&mut self, cx: &mut Context<Self>) {
        if !self.settings_open {
            return;
        }
        self.settings_open = false;
        cx.notify();
    }

    fn handle_raid_event(&mut self, raid: kick_live::realtime::RaidEvent, cx: &mut Context<Self>) {
        if !raid_is_for_current_channel(raid.chatroom_id, self.selected_chatroom_id) {
            return;
        }
        let target = raid
            .target
            .clone()
            .filter(|target| !target.is_empty() && *target != self.selected_channel_slug);
        let total_seconds = self.raid_display_seconds;
        if let Some(target) = target {
            self.pending_raid = Some(PendingRaid {
                hoster: raid.hoster.clone(),
                target,
                viewer_count: raid.viewer_count,
                deadline: Instant::now() + Duration::from_secs(total_seconds),
            });
            self.raid_notice = None;
            self.start_raid_countdown(cx);
        } else {
            self.pending_raid = None;
            self.raid_notice = Some(t!(
                "{} が {} 人を連れてレイドしました",
                "{} raided with {} viewers",
                raid.hoster,
                raid.viewer_count
            ));
        }
        cx.notify();
    }

    /// Clears a pending or informational raid banner and stops its countdown.
    /// Called when the selected channel changes so a raid from the previous
    /// stream cannot remain at the top of the new one.
    fn clear_raid_state(&mut self) {
        self.raid_generation = self.raid_generation.wrapping_add(1);
        self.pending_raid = None;
        self.raid_notice = None;
    }

    /// Handles the official auto-move event. Unlike `StreamHostedEvent`, this
    /// carries the target channel (`hosted.slug`), so the app can switch.
    fn handle_move_to_channel(
        &mut self,
        event: kick_live::realtime::MoveToChannelEvent,
        cx: &mut Context<Self>,
    ) {
        if !raid_is_for_current_channel(event.chatroom_id, self.selected_chatroom_id) {
            return;
        }
        if event.target_slug.is_empty() || event.target_slug == self.selected_channel_slug {
            return;
        }
        let hoster = self
            .selected_channel_details
            .username
            .clone()
            .unwrap_or_else(|| self.selected_channel_slug.clone());
        let viewer_count = self
            .selected_channel_details
            .viewer_count
            .unwrap_or(event.target_viewers);
        let total_seconds = self.raid_display_seconds;
        self.pending_raid = Some(PendingRaid {
            hoster,
            target: event.target_slug,
            viewer_count,
            deadline: Instant::now() + Duration::from_secs(total_seconds),
        });
        self.raid_notice = None;
        self.start_raid_countdown(cx);
        cx.notify();
    }

    fn start_raid_countdown(&mut self, cx: &mut Context<Self>) {
        self.raid_generation = self.raid_generation.wrapping_add(1);
        let generation = self.raid_generation;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let keep_running = this.update(cx, |this, cx| {
                    if generation != this.raid_generation {
                        return false;
                    }
                    let Some(raid) = this.pending_raid.as_ref() else {
                        return false;
                    };
                    if Instant::now() < raid.deadline {
                        cx.notify();
                        return true;
                    }
                    let target = raid.target.clone();
                    this.pending_raid = None;
                    this.raid_notice = None;
                    this.switch_to_raid_target(target, cx);
                    false
                });
                if !matches!(keep_running, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
    }

    fn switch_to_raid_target(&mut self, target: String, cx: &mut Context<Self>) {
        let summary = StreamSummary {
            channel_slug: target.clone(),
            channel_name: target,
            title: String::new(),
            category: None,
            viewer_count: None,
            thumbnail_url: None,
            profile_picture_url: None,
        };
        self.select_summary(summary, cx);
        cx.notify();
    }

    fn accept_raid(&mut self, cx: &mut Context<Self>) {
        self.raid_generation = self.raid_generation.wrapping_add(1);
        let Some(raid) = self.pending_raid.take() else {
            return;
        };
        self.raid_notice = None;
        self.switch_to_raid_target(raid.target, cx);
    }

    fn cancel_raid(&mut self, cx: &mut Context<Self>) {
        self.raid_generation = self.raid_generation.wrapping_add(1);
        self.pending_raid = None;
        self.raid_notice = None;
        cx.notify();
    }

    fn dismiss_raid_notice(&mut self, cx: &mut Context<Self>) {
        self.raid_notice = None;
        cx.notify();
    }

    fn start_history_lookup(&mut self, channel_id: u64, generation: u64, cx: &mut Context<Self>) {
        if generation != self.channel_generation {
            return;
        }
        self.chat_history_generation = self.chat_history_generation.wrapping_add(1);
        self.chat_history_state = ChatHistoryState::Loading;
        let history_generation = self.chat_history_generation;

        let Some(worker) = self.live_worker.as_mut() else {
            self.reset_chat_history(ChatHistoryState::Failed(LiveError::WorkerUnavailable));
            return;
        };

        let receiver = worker.request_history(channel_id);
        cx.spawn(async move |this, cx| {
            let result = receiver.await.unwrap_or(Err(LiveError::WorkerUnavailable));
            let _ = this.update(cx, |this, cx| {
                if !history_result_is_current(
                    generation,
                    this.channel_generation,
                    history_generation,
                    this.chat_history_generation,
                ) {
                    return;
                }
                match result {
                    Ok(messages) => {
                        let count = messages.len();
                        let merged =
                            merge_chat_messages(&messages, this.chat_messages.iter().cloned());
                        let retained_count = merged.len();
                        sync_chat_list_state(
                            &this.chat_state,
                            &this.chat_messages,
                            &merged,
                            this.chat_state.is_following_tail(),
                        );
                        this.chat_messages = Arc::from(merged);
                        this.chat_unread_messages = 0;
                        this.chat_scrolled_up = false;
                        this.chat_history_state = ChatHistoryState::Ready {
                            message_count: count,
                        };
                        this.sync_chat_snapshot(cx);
                        debug_assert_eq!(this.chat_state.item_count(), retained_count);
                    }
                    Err(error) => {
                        this.chat_history_state = ChatHistoryState::Failed(error);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn retry_chat_history(&mut self, cx: &mut Context<Self>) {
        if !chat_history_retry_available(self.chat_history_state) {
            return;
        }

        let mut restarted_worker = false;
        if let Some(error) = self.chat_history_state.failure()
            && retry_requires_worker_restart(error)
        {
            if !self.restart_live_worker() {
                self.reset_chat_history(ChatHistoryState::Failed(LiveError::WorkerUnavailable));
                cx.notify();
                return;
            }
            restarted_worker = true;
        }

        if restarted_worker && !self.selected_channel_slug.is_empty() {
            let channel_slug = self.selected_channel_slug.clone();
            self.start_channel_lookup(&channel_slug, cx);
            cx.notify();
            return;
        }

        if let Some(channel_id) = self.selected_channel_id {
            self.start_history_lookup(channel_id, self.channel_generation, cx);
        } else if !self.selected_channel_slug.is_empty() {
            let channel_slug = self.selected_channel_slug.clone();
            self.start_channel_lookup(&channel_slug, cx);
        }
        cx.notify();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LiveError {
    WorkerUnavailable,
    RuntimeUnavailable,
    Cancelled,
    MissingChannelId,
    ClientIdUnavailable,
    ChatStart(kick_live::realtime::ChatStartError),
    Api(KickApiError),
}

fn retry_requires_worker_restart(error: LiveError) -> bool {
    matches!(
        error,
        LiveError::WorkerUnavailable | LiveError::RuntimeUnavailable
    )
}

fn should_retry_channel_on_reselect(state: ChannelLookupState) -> bool {
    matches!(state, ChannelLookupState::Failed(_))
}

struct ChannelLookupResult {
    channel_id: Option<u64>,
    chatroom_id: Option<u64>,
    is_live: bool,
    playback_source: Option<PlaybackSource>,
    profile_picture_url: Option<String>,
    followers_count: Option<String>,
    username: Option<String>,
    started_at: Option<String>,
    viewer_count: Option<u64>,
    title: Option<String>,
    bio: Option<String>,
    twitter: Option<String>,
    instagram: Option<String>,
    youtube: Option<String>,
    discord: Option<String>,
    tiktok: Option<String>,
    facebook: Option<String>,
    club: Option<String>,
    verified: bool,
    banner_url: Option<String>,
    offline_banner_url: Option<String>,
    language: Option<String>,
    tags: Vec<String>,
    follower_goal: Option<kick_live::FollowerGoal>,
    dvr_url: Option<String>,
}

fn channel_lookup_result(snapshot: kick_live::AnonymousChannelSnapshot) -> ChannelLookupResult {
    let is_live = snapshot.is_live();
    let started_at = snapshot
        .livestream
        .as_ref()
        .and_then(|stream| stream.started_at.clone());
    let viewer_count = snapshot
        .livestream
        .as_ref()
        .and_then(|stream| stream.viewer_count);
    let title = snapshot
        .livestream
        .as_ref()
        .and_then(|stream| stream.title.clone());
    ChannelLookupResult {
        channel_id: snapshot.id,
        chatroom_id: snapshot.chatroom_id,
        is_live,
        playback_source: snapshot.playback_source,
        profile_picture_url: snapshot.profile_picture_url,
        followers_count: snapshot.followers_count,
        username: snapshot.username,
        started_at,
        viewer_count,
        title,
        bio: snapshot.bio,
        twitter: snapshot.twitter,
        instagram: snapshot.instagram,
        youtube: snapshot.youtube,
        discord: snapshot.discord,
        tiktok: snapshot.tiktok,
        facebook: snapshot.facebook,
        club: snapshot.club,
        verified: snapshot.verified,
        banner_url: snapshot.banner_url,
        offline_banner_url: snapshot.offline_banner_url,
        language: snapshot.language,
        tags: snapshot.tags,
        follower_goal: None,
        dvr_url: None,
    }
}

impl LiveError {
    fn user_message(self) -> String {
        match self {
            Self::WorkerUnavailable => tr("匿名カタログworkerを利用できません").to_owned(),
            Self::RuntimeUnavailable => tr("Tokio runtimeを起動できません").to_owned(),
            Self::Cancelled => tr("取得をキャンセルしました").to_owned(),
            Self::MissingChannelId => tr("チャンネル応答に数値IDがありません").to_owned(),
            Self::ClientIdUnavailable => tr("匿名チャットIDを生成できません").to_owned(),
            Self::ChatStart(error) => t!(
                "チャットを開始できません: {error}",
                "Could not start chat: {error}"
            ),
            Self::Api(KickApiError::HttpStatus(403)) => tr("HTTP 403（アクセス拒否）").to_owned(),
            Self::Api(error) => error.to_string(),
        }
    }
}

fn chat_send_rejection_reason(code: &str) -> String {
    match code {
        "ACCOUNT_AGE_MODE_ERROR" => t!(
            "アカウントの年齢設定により送信できません",
            "Your account age setting blocks sending"
        ),
        "BANNED_ERROR" => t!(
            "このチャンネルでは送信が禁止されています",
            "You are banned from chatting in this channel"
        ),
        "EMOTES_ONLY_ERROR" => t!(
            "このチャンネルはエモートのみ送信できます",
            "This channel only allows emote messages"
        ),
        "FOLLOWERS_ONLY_ERROR" => t!(
            "フォロワーのみ送信できます",
            "Only followers can send messages"
        ),
        "FOREIGN_CHANNEL_EMOTE_ERROR" => t!(
            "このチャンネルでは使えないエモートが含まれています",
            "The message contains an emote from another channel"
        ),
        "INVALID_EMOTE_ERROR" => t!(
            "無効なエモートが含まれています",
            "The message contains an invalid emote"
        ),
        "MAX_SPECIAL_CHARS_ERROR" => t!(
            "特殊文字が多すぎます",
            "The message has too many special characters"
        ),
        "MESSAGE_CONTENT_LENGTH_ERROR" => t!("メッセージが長すぎます", "The message is too long"),
        "NO_LINKS_ERROR" => t!("リンクは送信できません", "Links are not allowed"),
        "ORIGINAL_MESSAGE_NOT_FOUND_ERROR" => t!(
            "返信先のメッセージが見つかりません",
            "The message being replied to was not found"
        ),
        "RATE_LIMIT_ERROR" => t!("送信が速すぎます", "You are sending too fast"),
        "SLOW_MODE_ERROR" => t!("スローモード中です", "Slow mode is enabled"),
        "SUBSCRIBERS_ONLY_EMOTE_ERROR" => t!(
            "サブスクライバー限定のエモートです",
            "That emote is for subscribers only"
        ),
        "SUBSCRIBERS_ONLY_ERROR" => t!(
            "サブスクライバーのみ送信できます",
            "Only subscribers can send messages"
        ),
        _ => String::new(),
    }
}

fn chat_send_error_message(error: KickApiError) -> String {
    match error {
        KickApiError::SessionRequired => tr("送信にはログインが必要です").to_owned(),
        KickApiError::SessionInvalid => tr("セッションを確認できません").to_owned(),
        KickApiError::HttpStatus(429) => tr("送信が制限されています（レート制限）").to_owned(),
        KickApiError::ChatSendRejected { status, code } => {
            let mut reason = chat_send_rejection_reason(code);
            if reason.is_empty()
                && let Some(detail) = kick_live::take_chat_send_rejection()
            {
                reason = detail;
            }
            if reason.is_empty() {
                t!(
                    "送信できません (HTTP {status})",
                    "Could not send (HTTP {status})"
                )
            } else {
                t!(
                    "送信できません (HTTP {status}): {reason}",
                    "Could not send (HTTP {status}): {reason}"
                )
            }
        }
        KickApiError::HttpStatus(status) => t!(
            "送信できません (HTTP {status})",
            "Could not send (HTTP {status})"
        ),
        KickApiError::InvalidQuery(_) => tr("メッセージの形式を確認してください").to_owned(),
        KickApiError::Transport(_) => tr("送信できません（通信エラー）").to_owned(),
        _ => tr("送信できません").to_owned(),
    }
}

#[derive(Clone, Default)]
enum SearchState {
    #[default]
    Idle,
    Loading,
    Ready(Vec<StreamSummary>),
    Failed(String),
}

#[derive(Clone, Copy)]
enum CatalogState {
    Loading,
    Ready,
    Failed(LiveError),
}

impl CatalogState {
    fn label(self) -> String {
        match self {
            Self::Loading => tr("匿名カタログ取得中").to_owned(),
            Self::Ready => tr("匿名カタログの実データ").to_owned(),
            Self::Failed(error) => t!(
                "カタログ取得失敗: {}",
                "Could not load the catalog: {}",
                error.user_message()
            ),
        }
    }
}

fn catalog_retry_available(state: CatalogState) -> bool {
    matches!(state, CatalogState::Failed(_))
}

#[derive(Clone, Copy)]
enum ChannelLookupState {
    NotRequested,
    Loading,
    Ready { is_live: bool },
    Failed(LiveError),
}

impl ChannelLookupState {
    fn label(self) -> String {
        match self {
            Self::NotRequested => tr("匿名チャンネル詳細: 未取得").to_owned(),
            Self::Loading => tr("匿名チャンネル詳細を取得中…").to_owned(),
            Self::Ready { is_live: true } => tr("匿名チャンネル詳細: ライブ中").to_owned(),
            Self::Ready { is_live: false } => tr("匿名チャンネル詳細: オフライン").to_owned(),
            Self::Failed(error) => {
                t!(
                    "チャンネル取得失敗: {}",
                    "Could not load the channel: {}",
                    error.user_message()
                )
            }
        }
    }
}

#[derive(Clone)]
enum ChannelPageState {
    Idle,
    Loading,
    Ready(kick_live::ChannelPageData),
    Failed(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ChannelPageTab {
    Home,
    Videos,
    Clips,
    About,
}

impl ChannelPageTab {
    const ALL: [Self; 4] = [Self::Home, Self::Videos, Self::Clips, Self::About];

    fn label(self) -> &'static str {
        match self {
            Self::Home => tr("ホーム"),
            Self::Videos => tr("配信"),
            Self::Clips => tr("クリップ"),
            Self::About => tr("概要"),
        }
    }
}

#[derive(Clone, Copy)]
enum ChatHistoryState {
    NoChannel,
    Loading,
    Ready { message_count: usize },
    Failed(LiveError),
}

impl ChatHistoryState {
    fn label(self) -> String {
        match self {
            Self::NoChannel => tr("履歴なし").to_owned(),
            Self::Loading => tr("履歴取得中…").to_owned(),
            Self::Ready { message_count } => {
                t!("履歴 {message_count}件", "History: {message_count}")
            }
            Self::Failed(error) => t!("履歴失敗: {}", "History failed: {}", error.user_message()),
        }
    }

    fn empty_message(self) -> String {
        match self {
            Self::NoChannel => tr("カタログから配信を選択してください").to_owned(),
            Self::Loading => tr("履歴スナップショットを取得しています…").to_owned(),
            Self::Ready { message_count: 0 } => tr("最近のチャット履歴はありません").to_owned(),
            Self::Ready { .. } => String::new(),
            Self::Failed(error) => t!(
                "履歴を取得できません: {}",
                "Could not load history: {}",
                error.user_message()
            ),
        }
    }

    fn failure(self) -> Option<LiveError> {
        match self {
            Self::Failed(error) => Some(error),
            _ => None,
        }
    }
}

fn chat_history_retry_available(state: ChatHistoryState) -> bool {
    state.failure().is_some()
}

fn history_result_is_current(
    request_channel_generation: u64,
    current_channel_generation: u64,
    request_history_generation: u64,
    current_history_generation: u64,
) -> bool {
    request_channel_generation == current_channel_generation
        && request_history_generation == current_history_generation
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChatRealtimeState {
    NoChannel,
    WaitingForChannel,
    Offline,
    Unavailable,
    Connecting { attempt: u32 },
    Connected,
    Reconnecting { attempt: u32 },
    Stopped,
    Failed(LiveError),
}

impl ChatRealtimeState {
    fn label(self) -> String {
        match self {
            Self::NoChannel => tr("未選択").to_owned(),
            Self::WaitingForChannel => tr("詳細取得中").to_owned(),
            Self::Offline => tr("オフライン").to_owned(),
            Self::Unavailable => tr("ルームなし").to_owned(),
            Self::Connecting { attempt } => {
                t!("接続中（試行 {}）", "Connecting (attempt {})", attempt + 1)
            }
            Self::Connected => tr("接続済み").to_owned(),
            Self::Reconnecting { attempt } => t!(
                "再接続中（試行 {}）",
                "Reconnecting (attempt {})",
                attempt + 1
            ),
            Self::Stopped => tr("接続停止").to_owned(),
            Self::Failed(error) => t!(
                "チャット接続失敗: {}",
                "Chat connection failed: {}",
                error.user_message()
            ),
        }
    }
}

impl From<kick_live::realtime::ChatStatus> for ChatRealtimeState {
    fn from(status: kick_live::realtime::ChatStatus) -> Self {
        use kick_live::realtime::ChatStatus;
        match status {
            ChatStatus::Connecting { attempt } => Self::Connecting { attempt },
            ChatStatus::Connected => Self::Connected,
            ChatStatus::Reconnecting { attempt } => Self::Reconnecting { attempt },
            ChatStatus::Stopped => Self::Stopped,
        }
    }
}

fn merge_chat_messages(
    first: &[ChatMessage],
    later: impl IntoIterator<Item = ChatMessage>,
) -> Vec<ChatMessage> {
    let mut retained = BoundedChat::with_limits(CHAT_HISTORY_MAX_MESSAGES, CHAT_HISTORY_MAX_BYTES);
    let mut seen = HashSet::with_capacity(CHAT_HISTORY_MAX_MESSAGES);
    for message in first.iter().cloned().chain(later) {
        if message.id.is_empty() || seen.insert(message.id.clone()) {
            retained.push(message);
        }
    }
    retained.messages().iter().cloned().collect()
}

/// Removes and returns the held chat messages whose due time has passed.
/// Messages are ordered by arrival, so the scan stops at the first pending one.
fn take_due_chat(pending: &mut VecDeque<PendingChatMessage>, now: Instant) -> Vec<ChatMessage> {
    let mut due = Vec::new();
    while let Some(front) = pending.front() {
        if front.due > now {
            break;
        }
        if let Some(message) = pending.pop_front() {
            due.push(message.message);
        }
    }
    due
}

fn new_chat_list_state(item_count: usize) -> ListState {
    let state = ListState::new(item_count, ListAlignment::Bottom, px(CHAT_OVERDRAW));
    state.set_follow_mode(FollowMode::Tail);
    state
}

fn count_new_chat_messages(existing: &[ChatMessage], incoming: &[ChatMessage]) -> usize {
    let mut seen = existing
        .iter()
        .filter(|message| !message.id.is_empty())
        .map(|message| message.id.clone())
        .collect::<HashSet<_>>();

    incoming
        .iter()
        .filter(|message| message.id.is_empty() || seen.insert(message.id.clone()))
        .count()
}

fn chat_splice_plan<T: PartialEq>(old: &[T], new: &[T]) -> Vec<(Range<usize>, usize)> {
    if old == new {
        return Vec::new();
    }

    let common_prefix = old
        .iter()
        .zip(new)
        .take_while(|(old, new)| old == new)
        .count();
    if common_prefix == old.len() {
        return vec![(old.len()..old.len(), new.len() - old.len())];
    }
    if common_prefix == new.len() {
        return vec![(new.len()..old.len(), 0)];
    }

    // When bounded history evicts old rows as live rows arrive, remove that
    // prefix and append the novel suffix separately so the visible anchor stays put.
    for removed_prefix in 1..old.len().min(new.len()) {
        let retained_count = old.len() - removed_prefix;
        if old[removed_prefix..] == new[..retained_count] {
            let appended_count = new.len() - retained_count;
            let mut splices = vec![(0..removed_prefix, 0)];
            if appended_count > 0 {
                splices.push((retained_count..retained_count, appended_count));
            }
            return splices;
        }
    }

    let common_suffix = old[common_prefix..]
        .iter()
        .rev()
        .zip(new[common_prefix..].iter().rev())
        .take_while(|(old, new)| old == new)
        .count();
    let old_end = old.len() - common_suffix;
    let new_end = new.len() - common_suffix;
    vec![(common_prefix..old_end, new_end - common_prefix)]
}

fn sync_chat_list_state(
    state: &ListState,
    old: &[ChatMessage],
    new: &[ChatMessage],
    was_following_tail: bool,
) {
    for (old_range, new_count) in chat_splice_plan(old, new) {
        state.splice(old_range, new_count);
    }
    if was_following_tail {
        state.scroll_to_end();
    }
}

fn inline_chat_height(window_height: f32) -> f32 {
    (window_height * 0.34).clamp(180.0, 280.0)
}

fn chat_visibility_for_layout(
    narrow_mode: bool,
    wide_chat_visible: bool,
    narrow_chat_visible: bool,
) -> bool {
    if narrow_mode {
        narrow_chat_visible
    } else {
        wide_chat_visible
    }
}

enum ChatWorkerEvent {
    Status {
        generation: u64,
        status: kick_live::realtime::ChatStatus,
    },
    Message {
        generation: u64,
        message: ChatMessage,
    },
    Raid {
        generation: u64,
        raid: kick_live::realtime::RaidEvent,
    },
    MoveToChannel {
        generation: u64,
        event: kick_live::realtime::MoveToChannelEvent,
    },
    Failed {
        generation: u64,
        error: LiveError,
    },
}

enum WorkerCommand {
    Catalog {
        cancel: Arc<AtomicBool>,
        reply: oneshot::Sender<Result<Vec<StreamSummary>, LiveError>>,
    },
    Channel {
        slug: String,
        cancel: Arc<AtomicBool>,
        reply: oneshot::Sender<Result<ChannelLookupResult, LiveError>>,
    },
    ChannelStats {
        slug: String,
        cancel: Arc<AtomicBool>,
        reply: oneshot::Sender<Result<ChannelLookupResult, LiveError>>,
    },
    ChannelPage {
        slug: String,
        cancel: Arc<AtomicBool>,
        reply: oneshot::Sender<Result<kick_live::ChannelPageData, LiveError>>,
    },
    ChannelClipsPage {
        slug: String,
        cursor: String,
        cancel: Arc<AtomicBool>,
        reply: oneshot::Sender<Result<kick_live::ChannelClipPage, LiveError>>,
    },
    Emotes {
        slug: String,
        cancel: Arc<AtomicBool>,
        reply: oneshot::Sender<Result<Vec<(String, u64)>, LiveError>>,
    },
    VodSource {
        uuid: String,
        reply: oneshot::Sender<Result<String, LiveError>>,
    },
    Rewards {
        slug: String,
        reply: oneshot::Sender<Result<Vec<kick_live::ChannelReward>, LiveError>>,
    },
    RedeemReward {
        slug: String,
        reward_id: String,
        reply: oneshot::Sender<Result<(), LiveError>>,
    },
    History {
        channel_id: u64,
        cancel: Arc<AtomicBool>,
        reply: oneshot::Sender<Result<Vec<ChatMessage>, LiveError>>,
    },
    Search {
        query: String,
        cancel: Arc<AtomicBool>,
        reply: oneshot::Sender<Result<Vec<StreamSummary>, LiveError>>,
    },
    SendChat {
        chatroom_id: u64,
        content: String,
        reply: oneshot::Sender<Result<(), KickApiError>>,
    },
    StartChat {
        chatroom_id: u64,
        channel_id: Option<u64>,
        generation: u64,
    },
    StopChat,
    ValidateSession {
        cookie: SessionCookie,
        reply: oneshot::Sender<Result<SessionUser, SessionAuthError>>,
    },
    FollowedChannels {
        cursor: Option<String>,
        cancel: Arc<AtomicBool>,
        reply: oneshot::Sender<Result<FollowedChannelsPage, LiveError>>,
    },
    FollowChannel {
        slug: String,
        follow: bool,
        reply: oneshot::Sender<Result<(), LiveError>>,
    },
    ClearSession,
    Shutdown,
}

/// Owns the long-lived Centrifugo notification subscription. The thread is
/// detached on cancellation; the socket read has a one-second timeout so it
/// exits promptly when the notify set changes.
struct NotificationWorker {
    cancel: Arc<AtomicBool>,
}

impl NotificationWorker {
    fn start(
        slugs: Vec<String>,
    ) -> Option<(
        Self,
        tokio::sync::mpsc::Receiver<kick_live::realtime::StreamNotificationEvent>,
    )> {
        let (sender, receiver) = tokio::sync::mpsc::channel(64);
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let _thread = std::thread::Builder::new()
            .name("kick-notifications".to_owned())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                let Ok(client) = KickClient::unauthenticated_for_probe() else {
                    return;
                };
                let client_id = format!("kick-native-notify-{}", std::process::id());
                runtime.block_on(kick_live::realtime::run_stream_notifications(
                    client,
                    client_id,
                    slugs,
                    sender,
                    worker_cancel,
                ));
            })
            .ok()?;
        Some((Self { cancel }, receiver))
    }

    fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }
}

impl Drop for NotificationWorker {
    fn drop(&mut self) {
        self.cancel();
    }
}

struct LiveWorker {
    commands: Sender<WorkerCommand>,
    thread: Option<JoinHandle<()>>,
    catalog_cancel: Option<Arc<AtomicBool>>,
    channel_cancel: Option<Arc<AtomicBool>>,
    channel_page_cancel: Option<Arc<AtomicBool>>,
    clips_cancel: Option<Arc<AtomicBool>>,
    emotes_cancel: Option<Arc<AtomicBool>>,
    following_cancel: Option<Arc<AtomicBool>>,
    search_cancel: Option<Arc<AtomicBool>>,
    stats_cancel: Option<Arc<AtomicBool>>,
    chat_events: Receiver<ChatWorkerEvent>,
    dropped_chat_messages: Arc<AtomicU64>,
}

impl LiveWorker {
    fn new() -> std::io::Result<Self> {
        let (commands, receiver) = mpsc::channel();
        let (chat_event_sender, chat_events) = mpsc::sync_channel(CHAT_WORKER_EVENT_CAPACITY);
        let dropped_chat_messages = Arc::new(AtomicU64::new(0));
        let worker_dropped_chat_messages = dropped_chat_messages.clone();
        let thread = thread::Builder::new()
            .name("kick-live-api".to_owned())
            .spawn(move || {
                live_worker_loop(receiver, chat_event_sender, worker_dropped_chat_messages)
            })?;
        Ok(Self {
            commands,
            thread: Some(thread),
            catalog_cancel: None,
            channel_cancel: None,
            channel_page_cancel: None,
            clips_cancel: None,
            emotes_cancel: None,
            following_cancel: None,
            search_cancel: None,
            stats_cancel: None,
            chat_events,
            dropped_chat_messages,
        })
    }

    fn request_catalog(&mut self) -> oneshot::Receiver<Result<Vec<StreamSummary>, LiveError>> {
        if let Some(cancel) = self.catalog_cancel.take() {
            cancel.store(true, Ordering::Release);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let (reply, receiver) = oneshot::channel();
        self.catalog_cancel = Some(cancel.clone());
        let _ = self.commands.send(WorkerCommand::Catalog { cancel, reply });
        receiver
    }

    fn request_channel(
        &mut self,
        slug: String,
    ) -> oneshot::Receiver<Result<ChannelLookupResult, LiveError>> {
        if let Some(cancel) = self.channel_cancel.take() {
            cancel.store(true, Ordering::Release);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let (reply, receiver) = oneshot::channel();
        self.channel_cancel = Some(cancel.clone());
        let _ = self.commands.send(WorkerCommand::Channel {
            slug,
            cancel,
            reply,
        });
        receiver
    }

    fn request_channel_stats(
        &mut self,
        slug: String,
    ) -> oneshot::Receiver<Result<ChannelLookupResult, LiveError>> {
        if let Some(cancel) = self.stats_cancel.take() {
            cancel.store(true, Ordering::Release);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let (reply, receiver) = oneshot::channel();
        self.stats_cancel = Some(cancel.clone());
        let _ = self.commands.send(WorkerCommand::ChannelStats {
            slug,
            cancel,
            reply,
        });
        receiver
    }

    fn request_channel_page(
        &mut self,
        slug: String,
    ) -> oneshot::Receiver<Result<kick_live::ChannelPageData, LiveError>> {
        if let Some(cancel) = self.channel_page_cancel.take() {
            cancel.store(true, Ordering::Release);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let (reply, receiver) = oneshot::channel();
        self.channel_page_cancel = Some(cancel.clone());
        let _ = self.commands.send(WorkerCommand::ChannelPage {
            slug,
            cancel,
            reply,
        });
        receiver
    }

    fn cancel_clips_page(&mut self) {
        if let Some(cancel) = self.clips_cancel.take() {
            cancel.store(true, Ordering::Release);
        }
    }

    fn request_channel_clips_page(
        &mut self,
        slug: String,
        cursor: String,
    ) -> oneshot::Receiver<Result<kick_live::ChannelClipPage, LiveError>> {
        if let Some(cancel) = self.clips_cancel.take() {
            cancel.store(true, Ordering::Release);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let (reply, receiver) = oneshot::channel();
        self.clips_cancel = Some(cancel.clone());
        let _ = self.commands.send(WorkerCommand::ChannelClipsPage {
            slug,
            cursor,
            cancel,
            reply,
        });
        receiver
    }

    fn request_history(
        &mut self,
        channel_id: u64,
    ) -> oneshot::Receiver<Result<Vec<ChatMessage>, LiveError>> {
        if let Some(cancel) = self.channel_cancel.take() {
            cancel.store(true, Ordering::Release);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let (reply, receiver) = oneshot::channel();
        self.channel_cancel = Some(cancel.clone());
        let _ = self.commands.send(WorkerCommand::History {
            channel_id,
            cancel,
            reply,
        });
        receiver
    }

    fn request_search(
        &mut self,
        query: String,
    ) -> oneshot::Receiver<Result<Vec<StreamSummary>, LiveError>> {
        if let Some(cancel) = self.search_cancel.take() {
            cancel.store(true, Ordering::Release);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let (reply, receiver) = oneshot::channel();
        self.search_cancel = Some(cancel.clone());
        let _ = self.commands.send(WorkerCommand::Search {
            query,
            cancel,
            reply,
        });
        receiver
    }

    fn send_chat(
        &mut self,
        chatroom_id: u64,
        content: String,
    ) -> oneshot::Receiver<Result<(), KickApiError>> {
        let (reply, receiver) = oneshot::channel();
        let _ = self.commands.send(WorkerCommand::SendChat {
            chatroom_id,
            content,
            reply,
        });
        receiver
    }

    fn request_emotes(
        &mut self,
        slug: String,
    ) -> oneshot::Receiver<Result<Vec<(String, u64)>, LiveError>> {
        self.cancel_emotes();
        let cancel = Arc::new(AtomicBool::new(false));
        let (reply, receiver) = oneshot::channel();
        self.emotes_cancel = Some(cancel.clone());
        let _ = self.commands.send(WorkerCommand::Emotes {
            slug,
            cancel,
            reply,
        });
        receiver
    }

    fn request_rewards(
        &mut self,
        slug: String,
    ) -> oneshot::Receiver<Result<Vec<kick_live::ChannelReward>, LiveError>> {
        let (reply, receiver) = oneshot::channel();
        let _ = self.commands.send(WorkerCommand::Rewards { slug, reply });
        receiver
    }

    fn request_redeem_reward(
        &mut self,
        slug: String,
        reward_id: String,
    ) -> oneshot::Receiver<Result<(), LiveError>> {
        let (reply, receiver) = oneshot::channel();
        let _ = self.commands.send(WorkerCommand::RedeemReward {
            slug,
            reward_id,
            reply,
        });
        receiver
    }

    fn request_vod_source(&mut self, uuid: String) -> oneshot::Receiver<Result<String, LiveError>> {
        let (reply, receiver) = oneshot::channel();
        let _ = self.commands.send(WorkerCommand::VodSource { uuid, reply });
        receiver
    }

    fn cancel_emotes(&mut self) {
        if let Some(cancel) = self.emotes_cancel.take() {
            cancel.store(true, Ordering::Release);
        }
    }

    fn request_followed_channels(
        &mut self,
        cursor: Option<String>,
    ) -> oneshot::Receiver<Result<FollowedChannelsPage, LiveError>> {
        if let Some(cancel) = self.following_cancel.take() {
            cancel.store(true, Ordering::Release);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let (reply, receiver) = oneshot::channel();
        self.following_cancel = Some(cancel.clone());
        let _ = self.commands.send(WorkerCommand::FollowedChannels {
            cursor,
            cancel,
            reply,
        });
        receiver
    }

    fn clear_session(&mut self) {
        if let Some(cancel) = self.following_cancel.take() {
            cancel.store(true, Ordering::Release);
        }
        let _ = self.commands.send(WorkerCommand::ClearSession);
    }

    fn set_channel_follow(
        &self,
        slug: String,
        follow: bool,
    ) -> oneshot::Receiver<Result<(), LiveError>> {
        let (reply, receiver) = oneshot::channel();
        let _ = self.commands.send(WorkerCommand::FollowChannel {
            slug,
            follow,
            reply,
        });
        receiver
    }

    fn start_chat(&self, chatroom_id: u64, channel_id: Option<u64>, generation: u64) {
        self.dropped_chat_messages.store(0, Ordering::Release);
        let _ = self.commands.send(WorkerCommand::StartChat {
            chatroom_id,
            channel_id,
            generation,
        });
    }

    fn stop_chat(&self) {
        let _ = self.commands.send(WorkerCommand::StopChat);
    }

    fn validate_session(
        &self,
        cookie: SessionCookie,
    ) -> oneshot::Receiver<Result<SessionUser, SessionAuthError>> {
        let (reply, receiver) = oneshot::channel();
        let _ = self
            .commands
            .send(WorkerCommand::ValidateSession { cookie, reply });
        receiver
    }

    fn drain_chat_events(&self) -> Vec<ChatWorkerEvent> {
        self.chat_events.try_iter().collect()
    }

    fn dropped_chat_messages(&self) -> u64 {
        self.dropped_chat_messages.load(Ordering::Acquire)
    }
}

impl Drop for LiveWorker {
    fn drop(&mut self) {
        if let Some(cancel) = &self.catalog_cancel {
            cancel.store(true, Ordering::Release);
        }
        if let Some(cancel) = &self.channel_cancel {
            cancel.store(true, Ordering::Release);
        }
        if let Some(cancel) = &self.emotes_cancel {
            cancel.store(true, Ordering::Release);
        }
        let _ = self.commands.send(WorkerCommand::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn live_worker_loop(
    receiver: mpsc::Receiver<WorkerCommand>,
    chat_events: SyncSender<ChatWorkerEvent>,
    dropped_chat_messages: Arc<AtomicU64>,
) {
    let runtime = RuntimeBuilder::new_current_thread()
        .enable_all()
        .build()
        .ok();
    let client = runtime.as_ref().map(|runtime| {
        let _entered = runtime.enter();
        KickClient::unauthenticated_for_probe()
    });
    let mut client = client;
    let client_id = new_realtime_client_id();
    let mut chat_session = None;
    let mut chat_generation = 0;
    let mut last_chat_status = None;
    let mut pending_chat_state_event = None;

    loop {
        flush_pending_chat_state_event(&chat_events, &mut pending_chat_state_event);
        poll_chat_session(
            &runtime,
            &mut chat_session,
            chat_generation,
            &mut last_chat_status,
            &chat_events,
            &mut pending_chat_state_event,
            &dropped_chat_messages,
        );
        let command = if chat_session.is_some() || pending_chat_state_event.is_some() {
            match receiver.recv_timeout(Duration::from_millis(25)) {
                Ok(command) => command,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match receiver.recv() {
                Ok(command) => command,
                Err(_) => break,
            }
        };
        let should_shutdown = match command {
            WorkerCommand::Catalog { cancel, reply } => {
                let result = match (&runtime, &client) {
                    (Some(runtime), Some(Ok(client))) => runtime.block_on(async {
                        let page = cancellable_request(client.anonymous_catalog(), cancel).await?;
                        Ok(page
                            .items
                            .iter()
                            .filter_map(|item| item.to_summary())
                            .collect())
                    }),
                    (None, _) => Err(LiveError::RuntimeUnavailable),
                    (_, Some(Err(error))) => Err(LiveError::Api(*error)),
                    _ => Err(LiveError::RuntimeUnavailable),
                };
                let _ = reply.send(result);
                false
            }
            WorkerCommand::Channel {
                slug,
                cancel,
                reply,
            } => {
                let result = match (&runtime, &client) {
                    (Some(runtime), Some(Ok(client))) => runtime.block_on(async {
                        let snapshot =
                            cancellable_request(client.anonymous_channel(&slug), cancel.clone())
                                .await?;
                        let mut result = channel_lookup_result(snapshot);
                        result.follower_goal =
                            client.anonymous_channel_goals(&slug).await.ok().flatten();
                        if result.is_live
                            && let Ok(Some(video_id)) =
                                client.anonymous_channel_livestream_id(&slug).await
                        {
                            result.dvr_url = client
                                .anonymous_playback_dvr(&slug, &video_id)
                                .await
                                .ok()
                                .flatten();
                        }
                        Ok(result)
                    }),
                    (None, _) => Err(LiveError::RuntimeUnavailable),
                    (_, Some(Err(error))) => Err(LiveError::Api(*error)),
                    _ => Err(LiveError::RuntimeUnavailable),
                };
                let _ = reply.send(result);
                false
            }
            WorkerCommand::ChannelStats {
                slug,
                cancel,
                reply,
            } => {
                let result = match (&runtime, &client) {
                    (Some(runtime), Some(Ok(client))) => runtime.block_on(async {
                        cancellable_request(client.anonymous_channel(&slug), cancel)
                            .await
                            .map(channel_lookup_result)
                    }),
                    (None, _) => Err(LiveError::RuntimeUnavailable),
                    (_, Some(Err(error))) => Err(LiveError::Api(*error)),
                    _ => Err(LiveError::RuntimeUnavailable),
                };
                let _ = reply.send(result);
                false
            }
            WorkerCommand::ChannelClipsPage {
                slug,
                cursor,
                cancel,
                reply,
            } => {
                let result = match (&runtime, &client) {
                    (Some(runtime), Some(Ok(client))) => runtime.block_on(async {
                        cancellable_request(
                            client.anonymous_channel_clips(&slug, Some(cursor.as_str())),
                            cancel,
                        )
                        .await
                    }),
                    (None, _) => Err(LiveError::RuntimeUnavailable),
                    (_, Some(Err(error))) => Err(LiveError::Api(*error)),
                    _ => Err(LiveError::RuntimeUnavailable),
                };
                let _ = reply.send(result);
                false
            }
            WorkerCommand::ChannelPage {
                slug,
                cancel,
                reply,
            } => {
                let result = match (&runtime, &client) {
                    (Some(runtime), Some(Ok(client))) => runtime.block_on(async {
                        let mut data = kick_live::ChannelPageData::default();
                        let mut first_error = None;
                        let mut any_success = false;

                        match cancellable_request(
                            client.anonymous_channel_videos(&slug),
                            cancel.clone(),
                        )
                        .await
                        {
                            Ok(videos) => {
                                any_success = true;
                                data.videos = videos.into_iter().take(24).collect();
                            }
                            Err(error) => first_error = Some(error),
                        }
                        match cancellable_request(
                            client.anonymous_channel_clips(&slug, None),
                            cancel.clone(),
                        )
                        .await
                        {
                            Ok(page) => {
                                any_success = true;
                                data.clips = page.clips.into_iter().take(24).collect();
                                data.clips_cursor = page.next_cursor;
                            }
                            Err(error) => {
                                first_error.get_or_insert(error);
                            }
                        };
                        match cancellable_request(
                            client.anonymous_channel_recent_categories(&slug),
                            cancel.clone(),
                        )
                        .await
                        {
                            Ok(categories) => {
                                any_success = true;
                                data.categories = categories.into_iter().take(12).collect();
                            }
                            Err(error) => {
                                first_error.get_or_insert(error);
                            }
                        };
                        match cancellable_request(client.anonymous_channel_goals(&slug), cancel)
                            .await
                        {
                            Ok(goal) => {
                                any_success = true;
                                data.follower_goal = goal;
                            }
                            Err(error) => {
                                first_error.get_or_insert(error);
                            }
                        };

                        if any_success {
                            Ok(data)
                        } else {
                            Err(first_error.unwrap_or(LiveError::WorkerUnavailable))
                        }
                    }),
                    (None, _) => Err(LiveError::RuntimeUnavailable),
                    (_, Some(Err(error))) => Err(LiveError::Api(*error)),
                    _ => Err(LiveError::RuntimeUnavailable),
                };
                let _ = reply.send(result);
                false
            }
            WorkerCommand::Emotes {
                slug,
                cancel,
                reply,
            } => {
                let result = match (&runtime, &client) {
                    (Some(runtime), Some(Ok(client))) => runtime.block_on(async {
                        let groups =
                            cancellable_request(client.anonymous_channel_emotes(&slug), cancel)
                                .await?;
                        let mut seen = HashSet::new();
                        let mut emotes = Vec::new();
                        // Groups arrive channel-first, then global, then emoji.
                        for emote in groups.into_iter().flat_map(|group| group.emotes) {
                            if seen.insert(emote.name.to_ascii_lowercase()) {
                                emotes.push((emote.name, emote.id));
                            }
                        }
                        Ok(emotes)
                    }),
                    (None, _) => Err(LiveError::RuntimeUnavailable),
                    (_, Some(Err(error))) => Err(LiveError::Api(*error)),
                    _ => Err(LiveError::RuntimeUnavailable),
                };
                let _ = reply.send(result);
                false
            }
            WorkerCommand::Rewards { slug, reply } => {
                let result = match (&runtime, &client) {
                    (Some(runtime), Some(Ok(client))) => runtime
                        .block_on(client.channel_point_rewards(&slug))
                        .map_err(LiveError::Api),
                    (None, _) => Err(LiveError::RuntimeUnavailable),
                    (_, Some(Err(error))) => Err(LiveError::Api(*error)),
                    _ => Err(LiveError::RuntimeUnavailable),
                };
                let _ = reply.send(result);
                false
            }
            WorkerCommand::RedeemReward {
                slug,
                reward_id,
                reply,
            } => {
                let result = match (&runtime, &client) {
                    (Some(runtime), Some(Ok(client))) => runtime
                        .block_on(client.redeem_channel_point_reward(&slug, &reward_id))
                        .map_err(LiveError::Api),
                    (None, _) => Err(LiveError::RuntimeUnavailable),
                    (_, Some(Err(error))) => Err(LiveError::Api(*error)),
                    _ => Err(LiveError::RuntimeUnavailable),
                };
                let _ = reply.send(result);
                false
            }
            WorkerCommand::VodSource { uuid, reply } => {
                let result = match (&runtime, &client) {
                    (Some(runtime), Some(Ok(client))) => runtime
                        .block_on(client.anonymous_vod_source(&uuid))
                        .map_err(LiveError::Api),
                    (None, _) => Err(LiveError::RuntimeUnavailable),
                    (_, Some(Err(error))) => Err(LiveError::Api(*error)),
                    _ => Err(LiveError::RuntimeUnavailable),
                };
                let _ = reply.send(result);
                false
            }
            WorkerCommand::History {
                channel_id,
                cancel,
                reply,
            } => {
                let result = match (&runtime, &client) {
                    (Some(runtime), Some(Ok(client))) => runtime.block_on(async {
                        cancellable_request(client.anonymous_recent_messages(channel_id), cancel)
                            .await
                            .map(bounded_recent_messages)
                    }),
                    (None, _) => Err(LiveError::RuntimeUnavailable),
                    (_, Some(Err(error))) => Err(LiveError::Api(*error)),
                    _ => Err(LiveError::RuntimeUnavailable),
                };
                let _ = reply.send(result);
                false
            }
            WorkerCommand::Search {
                query,
                cancel,
                reply,
            } => {
                let result = match (&runtime, &client) {
                    (Some(runtime), Some(Ok(client))) => runtime.block_on(async {
                        cancellable_request(client.anonymous_search(&query), cancel).await
                    }),
                    (None, _) => Err(LiveError::RuntimeUnavailable),
                    (_, Some(Err(error))) => Err(LiveError::Api(*error)),
                    _ => Err(LiveError::RuntimeUnavailable),
                };
                let _ = reply.send(result);
                false
            }
            WorkerCommand::SendChat {
                chatroom_id,
                content,
                reply,
            } => {
                let result = match (&runtime, &client) {
                    (Some(runtime), Some(Ok(client))) => {
                        runtime.block_on(client.send_chat_message(chatroom_id, &content))
                    }
                    (None, _) => Err(KickApiError::SessionRequired),
                    (_, Some(Err(error))) => Err(*error),
                    _ => Err(KickApiError::SessionRequired),
                };
                let _ = reply.send(result);
                false
            }
            WorkerCommand::ValidateSession { cookie, reply } => {
                let result = match (&runtime, client.as_mut()) {
                    (Some(runtime), Some(Ok(client))) => {
                        let result = runtime.block_on(client.validate_session(&cookie));
                        if result.is_ok() {
                            client.set_session_cookie(cookie);
                        }
                        result
                    }
                    (None, _) => Err(SessionAuthError::Transport(
                        kick_live::TransportFailure::Other,
                    )),
                    (_, Some(Err(_))) | (_, None) => Err(SessionAuthError::Transport(
                        kick_live::TransportFailure::Other,
                    )),
                };
                let _ = reply.send(result);
                false
            }
            WorkerCommand::FollowedChannels {
                cursor,
                cancel,
                reply,
            } => {
                let result = match (&runtime, &client) {
                    (Some(runtime), Some(Ok(client))) => runtime.block_on(cancellable_request(
                        client.followed_channels(cursor.as_deref()),
                        cancel,
                    )),
                    (None, _) => Err(LiveError::RuntimeUnavailable),
                    (_, Some(Err(error))) => Err(LiveError::Api(*error)),
                    _ => Err(LiveError::RuntimeUnavailable),
                };
                let _ = reply.send(result);
                false
            }
            WorkerCommand::FollowChannel {
                slug,
                follow,
                reply,
            } => {
                let result: Result<(), LiveError> = match (&runtime, &client) {
                    (Some(runtime), Some(Ok(client))) => runtime
                        .block_on(client.set_channel_follow(&slug, follow))
                        .map_err(LiveError::Api),
                    (None, _) => Err(LiveError::RuntimeUnavailable),
                    (_, Some(Err(error))) => Err(LiveError::Api(*error)),
                    _ => Err(LiveError::RuntimeUnavailable),
                };
                let _ = reply.send(result);
                false
            }
            WorkerCommand::ClearSession => {
                if let Some(Ok(client)) = client.as_mut() {
                    client.clear_session_cookie();
                }
                false
            }
            WorkerCommand::StartChat {
                chatroom_id,
                channel_id,
                generation,
            } => {
                stop_chat_session(&runtime, &mut chat_session);
                last_chat_status = None;
                chat_generation = generation;
                match (&runtime, &client, &client_id) {
                    (Some(runtime), Some(Ok(client)), Some(client_id)) => {
                        match runtime.block_on(client.run_chat(client_id, chatroom_id, channel_id))
                        {
                            Ok(session) => {
                                let status = session.status();
                                pending_chat_state_event =
                                    Some(ChatWorkerEvent::Status { generation, status });
                                last_chat_status = Some(status);
                                chat_session = Some(session);
                            }
                            Err(error) => {
                                pending_chat_state_event = Some(ChatWorkerEvent::Failed {
                                    generation,
                                    error: LiveError::ChatStart(error),
                                });
                            }
                        }
                    }
                    (None, _, _) => {
                        pending_chat_state_event = Some(ChatWorkerEvent::Failed {
                            generation,
                            error: LiveError::RuntimeUnavailable,
                        });
                    }
                    (_, Some(Err(error)), _) => {
                        pending_chat_state_event = Some(ChatWorkerEvent::Failed {
                            generation,
                            error: LiveError::Api(*error),
                        });
                    }
                    (_, _, None) => {
                        pending_chat_state_event = Some(ChatWorkerEvent::Failed {
                            generation,
                            error: LiveError::ClientIdUnavailable,
                        });
                    }
                    (_, None, _) => {
                        pending_chat_state_event = Some(ChatWorkerEvent::Failed {
                            generation,
                            error: LiveError::RuntimeUnavailable,
                        });
                    }
                }
                false
            }
            WorkerCommand::StopChat => {
                stop_chat_session(&runtime, &mut chat_session);
                dropped_chat_messages.store(0, Ordering::Release);
                pending_chat_state_event = None;
                last_chat_status = None;
                false
            }
            WorkerCommand::Shutdown => true,
        };
        if should_shutdown {
            break;
        }
    }
    stop_chat_session(&runtime, &mut chat_session);
}

fn new_realtime_client_id() -> Option<String> {
    let mut random = [0_u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut random))
        .ok()?;
    let id = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Some(id)
}

fn poll_chat_session(
    runtime: &Option<tokio::runtime::Runtime>,
    session: &mut Option<kick_live::realtime::ChatSession>,
    generation: u64,
    last_status: &mut Option<kick_live::realtime::ChatStatus>,
    events: &SyncSender<ChatWorkerEvent>,
    pending_state_event: &mut Option<ChatWorkerEvent>,
    dropped_messages: &AtomicU64,
) {
    let (Some(runtime), Some(chat_session)) = (runtime.as_ref(), session.as_mut()) else {
        return;
    };
    let status = chat_session.status();
    if *last_status != Some(status) {
        *pending_state_event = Some(ChatWorkerEvent::Status { generation, status });
        *last_status = Some(status);
    }
    let mut session_ended = false;
    for _ in 0..CHAT_WORKER_DRAIN_MAX {
        match runtime
            .block_on(async { timeout(Duration::from_millis(1), chat_session.recv()).await })
        {
            Ok(Some(event)) => match event {
                kick_live::realtime::ChatEvent::Message(message) => {
                    try_send_chat_message(events, generation, message, dropped_messages);
                }
                kick_live::realtime::ChatEvent::Raid(raid) => {
                    try_send_chat_raid(events, generation, raid, dropped_messages);
                }
                kick_live::realtime::ChatEvent::MoveToChannel(event) => {
                    try_send_chat_move(events, generation, event, dropped_messages);
                }
            },
            Ok(None) => {
                let status = chat_session.status();
                if *last_status != Some(status) {
                    *pending_state_event = Some(ChatWorkerEvent::Status { generation, status });
                    *last_status = Some(status);
                }
                session_ended = true;
                break;
            }
            Err(_) => break,
        }
    }
    if session_ended {
        if let Some(mut ended_session) = session.take() {
            runtime.block_on(ended_session.stop());
        }
    }
}

fn stop_chat_session(
    runtime: &Option<tokio::runtime::Runtime>,
    session: &mut Option<kick_live::realtime::ChatSession>,
) {
    if let Some(mut session) = session.take() {
        session.cancel();
        if let Some(runtime) = runtime.as_ref() {
            runtime.block_on(session.stop());
        }
    }
}

fn flush_pending_chat_state_event(
    events: &SyncSender<ChatWorkerEvent>,
    pending: &mut Option<ChatWorkerEvent>,
) {
    let Some(event) = pending.take() else {
        return;
    };
    match events.try_send(event) {
        Ok(()) => {}
        Err(TrySendError::Full(event)) => *pending = Some(event),
        Err(TrySendError::Disconnected(_)) => {}
    }
}

fn try_send_chat_message(
    events: &SyncSender<ChatWorkerEvent>,
    generation: u64,
    message: ChatMessage,
    dropped_messages: &AtomicU64,
) {
    let event = ChatWorkerEvent::Message {
        generation,
        message,
    };
    match events.try_send(event) {
        Ok(()) | Err(TrySendError::Disconnected(_)) => {}
        Err(TrySendError::Full(ChatWorkerEvent::Message { .. })) => {
            dropped_messages.fetch_add(1, Ordering::AcqRel);
        }
        Err(TrySendError::Full(_)) => {}
    }
}

fn try_send_chat_raid(
    events: &SyncSender<ChatWorkerEvent>,
    generation: u64,
    raid: kick_live::realtime::RaidEvent,
    dropped_messages: &AtomicU64,
) {
    let event = ChatWorkerEvent::Raid { generation, raid };
    match events.try_send(event) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            dropped_messages.fetch_add(1, Ordering::Relaxed);
        }
        Err(TrySendError::Disconnected(_)) => {}
    }
}

fn try_send_chat_move(
    events: &SyncSender<ChatWorkerEvent>,
    generation: u64,
    event: kick_live::realtime::MoveToChannelEvent,
    dropped_messages: &AtomicU64,
) {
    let event = ChatWorkerEvent::MoveToChannel { generation, event };
    match events.try_send(event) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            dropped_messages.fetch_add(1, Ordering::Relaxed);
        }
        Err(TrySendError::Disconnected(_)) => {}
    }
}

async fn cancellable_request<T>(
    future: impl Future<Output = Result<T, KickApiError>>,
    cancel: Arc<AtomicBool>,
) -> Result<T, LiveError> {
    tokio::pin!(future);
    loop {
        if cancel.load(Ordering::Acquire) {
            return Err(LiveError::Cancelled);
        }
        tokio::select! {
            result = &mut future => return result.map_err(LiveError::Api),
            _ = tokio::time::sleep(Duration::from_millis(25)) => {}
        }
    }
}

fn bounded_recent_messages(page: AnonymousRecentMessages) -> Vec<ChatMessage> {
    let mut history = BoundedChat::with_limits(CHAT_HISTORY_MAX_MESSAGES, CHAT_HISTORY_MAX_BYTES);
    for (index, message) in page
        .messages
        .into_iter()
        .take(CHAT_HISTORY_MAX_MESSAGES)
        .enumerate()
    {
        let Some(text) = message.content.filter(|text| !text.trim().is_empty()) else {
            continue;
        };
        let author = message
            .username
            .filter(|username| !username.trim().is_empty())
            .unwrap_or_else(|| "Unknown sender".to_owned());
        history.push(ChatMessage {
            id: message.id.unwrap_or_else(|| format!("history-{index}")),
            author,
            text,
            timestamp_ms: None,
            author_slug: message.author_slug,
            badges: message.badges,
            reward: None,
        });
    }

    history.messages().iter().cloned().collect()
}

impl Render for KickShell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        {
            let field = self.chat_input_host.borrow();
            if !field.painted_this_frame() {
                field.set_hidden(true);
            }
            field.begin_frame();
        }
        // Keyboard events only reach the shell's `on_key_down` while a node in
        // its subtree has focus, so give the root focus once after the first frame.
        if !self.root_focus_initialized {
            self.root_focus_initialized = true;
            let focus = self.root_focus.clone();
            window.defer(cx, move |window, cx| {
                focus.focus(window, cx);
            });
        }
        if !self.stats_refresh_started {
            self.stats_refresh_started = true;
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_secs(60))
                        .await;
                    if this.update(cx, |this, cx| this.refresh_stats(cx)).is_err() {
                        break;
                    }
                }
            })
            .detach();
        }
        let window_width = window.bounds().size.width;
        let narrow = window_width < px(780.0);
        let width_points = window_width.as_f32();
        let narrow_chat_height = inline_chat_height(window.bounds().size.height.as_f32());
        let show_navigation =
            !narrow && (self.channel_page_visible || (!self.theater_mode && self.sidebar_visible));
        let show_chat = !narrow && self.chat_visible;
        let sidebar_collapsed = self.sidebar_collapsed && !narrow;
        let navigation_width_limit = navigation_width_limit_for_window(width_points, show_chat);
        let navigation_width = if sidebar_collapsed {
            SIDEBAR_COLLAPSED_WIDTH
        } else {
            self.navigation_width.min(navigation_width_limit)
        };
        let chat_width_limit =
            chat_width_limit_for_window(width_points, navigation_width, show_navigation);
        let rendered_chat_width = clamp_chat_width(self.chat_width, chat_width_limit);
        let streams = self.streams.clone();
        let chat_messages = self.chat_messages.clone();
        let emote_ids_by_name = self.emote_ids_by_name.clone();
        let emote_image_cache = self.emote_image_cache.clone();

        let mut top_strip = div()
            .id("window-top-strip")
            .h(px(28.0))
            .min_h(px(28.0))
            .w_full()
            .bg(rgb(APP_BG));
        if self.video_fullscreen {
            top_strip = top_strip.flex().items_center().justify_end().px_3().child(
                div()
                    .id("exit-video-fullscreen")
                    .role(Role::Button)
                    .aria_label(tr("全画面表示を終了"))
                    .tab_index(0)
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .text_size(px(11.0))
                    .text_color(rgb(SUBTLE))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(RAISED)))
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.toggle_video_fullscreen(cx);
                    }))
                    .child(tr("全画面を終了")),
            );
        } else {
            // macOS convention: double-clicking the title bar zooms the window.
            // The top strip is the title bar area, so no separate button is drawn.
            top_strip = top_strip
                .flex()
                .items_center()
                .justify_end()
                .px_3()
                .w_full()
                .on_click(cx.listener(|_this, event: &ClickEvent, window, cx| {
                    let is_double_click = matches!(
                        event,
                        ClickEvent::Mouse(mouse) if mouse.up.click_count == 2
                    );
                    if is_double_click {
                        window.zoom_window();
                        cx.notify();
                    }
                }))
                .child(
                    div()
                        .id("open-settings")
                        .role(Role::Button)
                        .aria_label(tr("設定"))
                        .tab_index(0)
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .text_size(px(11.0))
                        .text_color(rgb(SUBTLE))
                        .cursor_pointer()
                        .hover(|style| style.bg(rgb(RAISED)))
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.open_settings(cx);
                        }))
                        .child(tr("設定")),
                );
        }

        let mut shell = div()
            .id("kick-shell")
            .track_focus(&self.root_focus)
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(rgb(APP_BG))
            .font_family("Inter")
            .text_color(rgb(TEXT))
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Back),
                cx.listener(|this, _event: &MouseDownEvent, _window, cx| {
                    this.navigate_back(cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Forward),
                cx.listener(|this, _event: &MouseDownEvent, _window, cx| {
                    this.navigate_forward(cx);
                }),
            )
            .child(top_strip);

        if let Some(title) = &self.vod_playing {
            shell = shell.child(
                div()
                    .h(px(32.0))
                    .min_h(px(32.0))
                    .px_4()
                    .flex()
                    .items_center()
                    .justify_between()
                    .bg(rgb(RAISED))
                    .text_size(px(11.0))
                    .text_color(rgb(SUBTLE))
                    .child(if self.vod_ended {
                        t!("再生終了: {title}", "Playback ended: {title}")
                    } else {
                        t!(
                            "過去配信を再生中: {title}",
                            "Playing past broadcast: {title}"
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .when(self.vod_ended, |bar| {
                                bar.child(
                                    div()
                                        .id("replay-vod")
                                        .role(Role::Button)
                                        .aria_label(tr("もう一度再生"))
                                        .px_2()
                                        .py_1()
                                        .rounded_md()
                                        .bg(rgb(RAISED))
                                        .text_size(px(11.0))
                                        .text_color(rgb(TEXT))
                                        .cursor_pointer()
                                        .on_click(cx.listener(|this, _event, _window, cx| {
                                            this.replay_vod(cx);
                                        }))
                                        .child(tr("もう一度再生")),
                                )
                            })
                            .child(
                                div()
                                    .id("return-to-live")
                                    .role(Role::Button)
                                    .aria_label(tr("ライブに戻る"))
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .bg(rgb(ACCENT))
                                    .text_size(px(11.0))
                                    .text_color(rgb(0x0B0B0F))
                                    .cursor_pointer()
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        this.return_to_live(cx);
                                    }))
                                    .child(tr("ライブに戻る")),
                            ),
                    ),
            );
        }
        if let Some(raid) = &self.pending_raid {
            shell = shell.child(raid_banner(raid, cx));
        } else if let Some(notice) = &self.raid_notice {
            shell = shell.child(raid_notice_banner(notice, cx));
        }

        if self.settings_open && !self.video_fullscreen {
            self.player_host.borrow_mut().suspend_view();
            shell = shell.child(settings_page(
                self.chat_delay_seconds,
                self.pending_chat.len(),
                self.debug_mode,
                &self.notify_channels,
                self.danmaku_enabled,
                self.danmaku_font_size,
                self.danmaku_opacity,
                self.default_quality,
                self.notification_permission,
                self.notification_notice.as_deref(),
                self.settings_tab,
                self.followed_limit,
                self.raid_display_seconds,
                cx,
            ));
        } else if self.video_fullscreen {
            shell = shell.child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .w_full()
                    .child(video_surface(self.player_host.clone())),
            );
        } else {
            let mut body = div().flex_1().min_h(px(0.0)).w_full().flex();

            if show_navigation {
                body = body.child(navigation_rail(
                    sidebar_collapsed,
                    navigation_width,
                    self.selected_nav,
                    self.catalog_state,
                    streams.clone(),
                    self.selected_channel_slug.clone(),
                    self.show_all_recommended,
                    self.account_state.clone(),
                    &self.search_query,
                    &self.search_focus,
                    self.following_state,
                    self.followed_channels.clone(),
                    self.following_has_more,
                    self.following_loading_more,
                    self.followed_limit,
                    cx,
                ));
                if !sidebar_collapsed {
                    body = body.child(panel_divider(
                        "navigation-divider",
                        tr("サイドバー幅を変更"),
                        PanelDivider::Navigation,
                        self.navigation_width,
                        NAV_MIN_WIDTH,
                        navigation_width_limit,
                        cx,
                    ));
                }
            }

            body = body.child(main_column(
                self.player_host.clone(),
                narrow,
                self.chat_visible,
                self.narrow_chat_visible,
                self.theater_mode,
                self.chat_state.clone(),
                streams,
                self.catalog_state,
                self.channel_state,
                self.account_state.clone(),
                self.selected_nav,
                self.selected_channel_slug.clone(),
                self.selected_channel_details.clone(),
                self.channel_page_visible,
                self.channel_page_state.clone(),
                self.clips_loading_more,
                self.clips_error.as_deref(),
                self.channel_page_tab,
                self.recent_streams.clone(),
                self.search_query.clone(),
                &self.search_state,
                self.search_focus.clone(),
                self.feed_visible,
                chat_messages.clone(),
                emote_ids_by_name.clone(),
                self.emote_order.clone(),
                emote_image_cache.clone(),
                self.chat_history_state,
                self.chat_unread_messages,
                narrow_chat_height,
                self.chat_input_notice.clone(),
                self.chat_send_pending,
                self.chat_send_confirmation,
                self.emote_picker_open,
                self.following_state,
                self.following_tab,
                self.followed_channels.clone(),
                self.following_has_more,
                self.following_loading_more,
                self.follow_action_pending,
                self.follow_error.clone(),
                self.notify_channels.contains(&self.selected_channel_slug),
                window,
                cx,
            ));

            let show_chat_column = !narrow
                && self.chat_visible
                && !self.channel_page_visible
                && !self.feed_visible
                && self.selected_nav != "フォロー中"
                && self.selected_nav != "イベント";
            if show_chat_column && self.chat_popped_out {
                body = body.child(panel_divider(
                    "chat-divider",
                    tr("チャット幅を変更"),
                    PanelDivider::Chat,
                    self.chat_width,
                    CHAT_MIN_WIDTH,
                    chat_width_limit,
                    cx,
                ));
                body = body.child(chat_popout_placeholder(rendered_chat_width, cx));
            } else if show_chat_column {
                body = body.child(panel_divider(
                    "chat-divider",
                    tr("チャット幅を変更"),
                    PanelDivider::Chat,
                    self.chat_width,
                    CHAT_MIN_WIDTH,
                    chat_width_limit,
                    cx,
                ));
                body = body.child(chat_column(
                    self.chat_state.clone(),
                    rendered_chat_width,
                    chat_messages,
                    emote_ids_by_name,
                    self.emote_order.clone(),
                    emote_image_cache,
                    self.chat_history_state,
                    self.chat_unread_messages,
                    self.chat_input_notice.clone(),
                    self.chat_send_pending,
                    self.chat_send_confirmation,
                    self.emote_picker_open,
                    window,
                    cx,
                ));
            }

            shell = shell.child(body);
        }

        if !self.video_fullscreen
            && let Some(menu) = &self.chat_context_menu
        {
            let window_size = window.bounds().size;
            let x = menu
                .position
                .x
                .min(window_size.width - px(232.0))
                .max(px(8.0));
            let y = menu
                .position
                .y
                .min(window_size.height - px(240.0))
                .max(px(8.0));
            shell = shell.child(
                div()
                    .id("chat-context-menu-layer")
                    .absolute()
                    .left(x)
                    .top(y)
                    .child(chat_context_popover(menu, cx)),
            );
        }

        if self.rewards_open && !self.video_fullscreen {
            shell = shell.child(
                div()
                    .id("rewards-layer")
                    .absolute()
                    .right(px(24.0))
                    .top(px(150.0))
                    .child(rewards_panel(
                        &self.rewards,
                        self.rewards_loading,
                        self.rewards_error.as_deref(),
                        cx,
                    )),
            );
        }

        if self.account_menu_open && !self.video_fullscreen {
            shell = shell.child(
                div()
                    .id("account-menu-layer")
                    .absolute()
                    .left(px(8.0))
                    .bottom(px(56.0))
                    .child(account_popover(self.account_state.clone(), cx)),
            );
        }

        if let Some((text, at)) = &self.toast
            && at.elapsed() < Duration::from_secs(2)
        {
            shell = shell.child(
                div()
                    .absolute()
                    .bottom(px(24.0))
                    .left(px(0.0))
                    .right(px(0.0))
                    .flex()
                    .justify_center()
                    .child(
                        div()
                            .px_4()
                            .py_2()
                            .rounded_md()
                            .bg(rgba(0x000000DD))
                            .text_size(px(12.0))
                            .text_color(rgb(TEXT))
                            .child(text.clone()),
                    ),
            );
        }
        shell
            .on_click(cx.listener(|this, event: &gpui::ClickEvent, window, cx| {
                if this.suppress_menu_close {
                    this.suppress_menu_close = false;
                    return;
                }
                if this.account_menu_open {
                    this.account_menu_open = false;
                    cx.notify();
                }
                if this.chat_context_menu.is_none() {
                    return;
                }
                let Some(menu) = &this.chat_context_menu else {
                    return;
                };
                let window_size = window.bounds().size;
                let menu_x = menu
                    .position
                    .x
                    .min(window_size.width - px(232.0))
                    .max(px(8.0));
                let menu_y = menu
                    .position
                    .y
                    .min(window_size.height - px(260.0))
                    .max(px(8.0));
                let position = event.position();
                let inside = position.x >= menu_x
                    && position.x <= menu_x + px(232.0)
                    && position.y >= menu_y
                    && position.y <= menu_y + px(260.0);
                if !inside {
                    this.close_chat_context_menu(cx);
                }
            }))
            .on_drag_move::<PanelDivider>(cx.listener(
                |this, event: &gpui::DragMoveEvent<PanelDivider>, window, cx| {
                    let divider = *event.drag(cx);
                    let window_width = window.bounds().size.width.as_f32();
                    this.resize_panel_from_pointer(
                        divider,
                        event.event.position.x.as_f32(),
                        window_width,
                    );
                    cx.notify();
                },
            ))
            .on_drop::<PanelDivider>(cx.listener(|this, _divider, _window, cx| {
                this.persist_panel_widths();
                cx.notify();
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.is_held {
                    return;
                }

                let key = event.keystroke.key.as_str();
                let modifiers = event.keystroke.modifiers;
                if key == "escape" && this.settings_open {
                    this.close_settings(cx);
                    return;
                }
                if key == "escape" && window.is_fullscreen() {
                    window.toggle_fullscreen();
                    cx.notify();
                    return;
                }
                if this.video_fullscreen {
                    if key == "escape" || (key == "f" && modifiers == Modifiers::none()) {
                        this.toggle_video_fullscreen(cx);
                    }
                    return;
                }
                let search_has_focus = this.search_focus.is_focused(window);

                if key == "escape" {
                    if search_has_focus {
                        if !this.search_query.is_empty() {
                            this.search_query.clear();
                            cx.notify();
                        } else {
                            window.blur(cx);
                        }
                    } else if this.theater_mode {
                        this.set_theater_mode(false);
                        cx.notify();
                    }
                    return;
                }

                if modifiers.platform
                    && !modifiers.control
                    && !modifiers.alt
                    && !modifiers.shift
                    && !modifiers.function
                    && key == ","
                {
                    if this.settings_open {
                        this.close_settings(cx);
                    } else {
                        this.open_settings(cx);
                    }
                    return;
                }

                if search_has_focus {
                    return;
                }

                if modifiers.platform
                    && !modifiers.control
                    && !modifiers.alt
                    && !modifiers.shift
                    && !modifiers.function
                    && key == "k"
                {
                    this.set_theater_mode(false);
                    this.set_feed_visible(true);
                    this.search_focus.focus(window, cx);
                    cx.notify();
                    return;
                }

                let has_interactive_focus = this.root_focus.contains_focused(window, cx);
                let text_input_has_focus =
                    search_has_focus || this.chat_input_host.borrow().is_focused();
                if let Some(shortcut) =
                    player_shortcut(key, search_has_focus, has_interactive_focus, modifiers)
                {
                    match shortcut {
                        PlayerShortcut::TogglePlayback => this.toggle_playback(cx),
                        PlayerShortcut::ToggleMute => this.toggle_mute(),
                        PlayerShortcut::ToggleFullscreen => this.toggle_video_fullscreen(cx),
                    }
                    cx.notify();
                    return;
                }

                if !navigation_shortcut_allowed(search_has_focus, text_input_has_focus) {
                    return;
                }

                if modifiers != Modifiers::none() {
                    return;
                }

                if key == "i" {
                    if !this.selected_channel_slug.is_empty() {
                        this.open_channel_page(cx);
                    }
                    return;
                }

                match key {
                    "j" | "arrowdown" => {
                        this.move_selection(1, cx);
                        cx.notify();
                    }
                    "k" | "arrowup" => {
                        this.move_selection(-1, cx);
                        cx.notify();
                    }
                    "t" => {
                        this.set_theater_mode(!this.theater_mode);
                        cx.notify();
                    }
                    "c" => {
                        this.chat_visible = !this.chat_visible;
                        cx.notify();
                    }
                    _ => {}
                }
            }))
            .when(self.login_panel_open, |root| {
                root.child(login_panel(
                    &self.login_input,
                    self.login_error.as_deref(),
                    &self.login_input_focus,
                    cx,
                ))
            })
    }
}

fn collapsed_rail_avatar(
    id: String,
    name: String,
    url: Option<String>,
    is_live: bool,
    summary: StreamSummary,
    cx: &mut Context<KickShell>,
) -> Stateful<Div> {
    let summary_for_click = summary.clone();
    div()
        .id(id)
        .role(Role::Button)
        .aria_label(name.clone())
        .tab_index(0)
        .w(px(32.0))
        .h(px(32.0))
        .flex_shrink_0()
        .rounded_full()
        .overflow_hidden()
        .bg(rgb(RAISED))
        .when(is_live, |avatar| {
            avatar.border_1().border_color(rgb(ACCENT))
        })
        .cursor_pointer()
        .hover(|style| style.opacity(0.85))
        .on_click(cx.listener(move |this, _event, _window, cx| {
            this.select_summary(summary_for_click.clone(), cx);
            cx.notify();
        }))
        .child(
            div()
                .size_full()
                .rounded_full()
                .overflow_hidden()
                .child(if let Some(url) = url {
                    cached_circular_avatar(&url, 32.0).into_any()
                } else {
                    avatar_placeholder(&name, 30.0).into_any()
                }),
        )
}

/// A raid publication is only for the chatroom the app is currently
/// subscribed to. The worker already scopes events by generation, but this
/// check also protects against a stale room ID after a channel switch.
/// A live broadcast can be followed by a raid, which is published after the
/// player ends. A VOD has no such follow-up, so its chat is stopped normally.
fn should_keep_chat_after_end(vod_playing: bool) -> bool {
    !vod_playing
}

fn raid_is_for_current_channel(raid_chatroom_id: u64, current_chatroom_id: Option<u64>) -> bool {
    current_chatroom_id.is_none_or(|current| current == raid_chatroom_id)
}

fn raid_countdown_seconds(viewer_count: u64) -> u64 {
    if viewer_count < 100 {
        15
    } else if viewer_count < 500 {
        30
    } else {
        60
    }
}

fn raid_banner(raid: &PendingRaid, cx: &mut Context<KickShell>) -> Stateful<Div> {
    let remaining = raid
        .deadline
        .saturating_duration_since(Instant::now())
        .as_secs();
    div()
        .id("raid-banner")
        .w_full()
        .px_4()
        .py_2()
        .bg(rgb(ACCENT))
        .text_color(rgb(0x0B0B0F))
        .flex()
        .items_center()
        .justify_between()
        .gap_3()
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_size(px(13.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child(t!(
                            "{} が {} 人を連れてレイドしました",
                            "{} raided with {} viewers",
                            raid.hoster,
                            raid.viewer_count
                        )),
                )
                .child(div().text_size(px(11.0)).child(t!(
                    "{} に移動します ({}秒)",
                    "Moving to {} in {}s",
                    raid.target,
                    remaining
                ))),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .id("raid-now")
                        .role(Role::Button)
                        .aria_label(tr("レイド先へ今すぐ移動"))
                        .tab_index(0)
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .bg(rgba(0x00000022))
                        .text_size(px(11.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .cursor_pointer()
                        .hover(|style| style.bg(rgba(0x00000033)))
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.accept_raid(cx);
                        }))
                        .child(tr("今すぐ移動")),
                )
                .child(
                    div()
                        .id("raid-cancel")
                        .role(Role::Button)
                        .aria_label(tr("レイド移動をキャンセル"))
                        .tab_index(0)
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .bg(rgba(0x00000022))
                        .text_size(px(11.0))
                        .cursor_pointer()
                        .hover(|style| style.bg(rgba(0x00000033)))
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.cancel_raid(cx);
                        }))
                        .child(tr("キャンセル")),
                ),
        )
}

fn raid_notice_banner(notice: &str, cx: &mut Context<KickShell>) -> Stateful<Div> {
    div()
        .id("raid-notice")
        .w_full()
        .px_4()
        .py_2()
        .bg(rgb(RAISED))
        .text_color(rgb(TEXT))
        .flex()
        .items_center()
        .justify_between()
        .gap_3()
        .child(div().text_size(px(12.0)).child(notice.to_owned()))
        .child(
            div()
                .id("raid-notice-close")
                .role(Role::Button)
                .aria_label(tr("レイド通知を閉じる"))
                .tab_index(0)
                .px_3()
                .py_1()
                .rounded_md()
                .text_size(px(11.0))
                .text_color(rgb(SUBTLE))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(SELECTED_ROW)))
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.dismiss_raid_notice(cx);
                }))
                .child(tr("閉じる")),
        )
}

fn navigation_rail(
    collapsed: bool,
    rail_width: f32,
    selected_nav: &'static str,
    catalog_state: CatalogState,
    streams: Vec<StreamSummary>,
    selected_channel_slug: String,
    show_all_recommended: bool,
    account_state: AccountState,
    search_query: &str,
    search_focus: &FocusHandle,
    following_state: FollowingState,
    followed_channels: Vec<FollowedChannel>,
    following_has_more: bool,
    following_loading_more: bool,
    followed_limit: usize,
    cx: &mut Context<KickShell>,
) -> Stateful<Div> {
    let header = sidebar_header(collapsed, cx);
    let mut nav_items = div()
        .id("primary-nav-tabs")
        .role(Role::TabList)
        .flex()
        .flex_col()
        .gap_1()
        .px_2()
        .pb_2()
        .tab_group();

    for label in ["ホーム", "ブラウズ", "フォロー中", "イベント"] {
        nav_items = nav_items.child(nav_item(label, selected_nav == label, collapsed, cx));
    }

    let mut rail = div()
        .id("navigation-rail")
        .role(Role::Navigation)
        .w(px(rail_width))
        .min_w(px(rail_width))
        .h_full()
        .flex()
        .flex_col()
        .bg(rgb(APP_BG))
        .child(header);

    if collapsed {
        let rail = rail.child(nav_items);
        let mut avatars = div()
            .id("collapsed-rail-avatars")
            .flex()
            .flex_col()
            .items_center()
            .gap_2()
            .pt_2()
            .pb_3()
            .overflow_y_scroll();
        for channel in followed_channels
            .iter()
            .filter(|channel| channel.is_live)
            .take(followed_limit.min(8))
        {
            avatars = avatars.child(collapsed_rail_avatar(
                format!("collapsed-followed-{}", channel.channel_slug),
                channel.channel_name.clone(),
                channel.profile_picture_url.clone(),
                channel.is_live,
                channel.to_summary(),
                cx,
            ));
        }
        for stream in streams.iter().take(8) {
            if followed_channels
                .iter()
                .any(|channel| channel.channel_slug == stream.channel_slug)
            {
                continue;
            }
            avatars = avatars.child(collapsed_rail_avatar(
                format!("collapsed-stream-{}", stream.channel_slug),
                stream.channel_name.clone(),
                stream.profile_picture_url.clone(),
                true,
                stream.clone(),
                cx,
            ));
        }
        return rail.child(avatars);
    }

    rail = rail.child(div().px_2().pb_2().child(sidebar_search_field(
        search_query,
        search_focus,
        cx,
    )));
    rail = rail.child(nav_items);

    let mut sections = div()
        .id("sidebar-sections")
        .flex_1()
        .min_h(px(0.0))
        .overflow_y_scroll()
        .flex()
        .flex_col();

    if matches!(account_state, AccountState::SignedIn { .. }) {
        let live_count = followed_channels
            .iter()
            .filter(|channel| channel.is_live)
            .count();
        let show_more = following_has_more || live_count > 5;
        let mut followed_header = div()
            .px_3()
            .pt_2()
            .pb_1()
            .flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(rgb(MUTED))
                    .child(tr("フォロー中")),
            );
        if show_more {
            followed_header = followed_header.child(
                div()
                    .id("sidebar-followed-more")
                    .role(Role::Button)
                    .aria_label(tr("フォロー中の一覧を開く"))
                    .tab_index(0)
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .text_size(px(10.0))
                    .text_color(rgb(SUBTLE))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(RAISED)))
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.selected_nav = "フォロー中";
                        this.set_feed_visible(false);
                        this.start_following_load(cx);
                        cx.notify();
                    }))
                    .child(tr("もっと見る")),
            );
        }

        let mut followed_section = div()
            .id("sidebar-followed-section")
            .flex()
            .flex_col()
            .gap_1()
            .pl_1()
            .pb_2()
            .child(followed_header);

        match following_state {
            FollowingState::Loading => {
                followed_section = followed_section
                    .child(sidebar_section_status(tr("フォロー中を取得しています…")));
            }
            FollowingState::Failed(error) => {
                followed_section = followed_section.child(sidebar_section_status(&t!(
                    "フォロー中を取得できません: {}",
                    "Could not load followed channels: {}",
                    error.user_message()
                )));
            }
            FollowingState::Ready if followed_channels.is_empty() => {
                followed_section = followed_section.child(sidebar_section_status(tr(
                    "フォロー中のチャンネルはありません",
                )));
            }
            FollowingState::Ready if live_count == 0 => {
                followed_section = followed_section.child(sidebar_section_status(tr(
                    "配信中のフォロー中チャンネルはありません",
                )));
            }
            _ => {
                for channel in followed_channels
                    .iter()
                    .filter(|channel| channel.is_live)
                    .take(followed_limit)
                {
                    let selected = channel.channel_slug == selected_channel_slug;
                    followed_section = followed_section.child(sidebar_stream_row(
                        channel.to_summary(),
                        selected,
                        cx,
                    ));
                }
                if following_loading_more {
                    followed_section = followed_section
                        .child(sidebar_section_status(tr("さらに読み込んでいます…")));
                }
            }
        }
        sections = sections.child(followed_section);
    }

    let recommended_limit = if show_all_recommended { 20 } else { 10 };
    let recommended_count = streams.len();
    let mut recommended_rows = div().id("sidebar-recommended-streams").flex().flex_col();
    if streams.is_empty() {
        let empty_message = match catalog_state {
            CatalogState::Loading => tr("おすすめを取得しています…").to_owned(),
            CatalogState::Ready => tr("おすすめの配信はありません").to_owned(),
            CatalogState::Failed(error) => {
                t!(
                    "おすすめを取得できません: {}",
                    "Could not load recommendations: {}",
                    error.user_message()
                )
            }
        };
        recommended_rows = recommended_rows.child({
            let mut empty = div()
                .px_3()
                .py_2()
                .flex()
                .flex_col()
                .gap_1()
                .text_size(px(11.0))
                .text_color(rgb(MUTED))
                .child(empty_message);
            if catalog_retry_available(catalog_state) {
                empty = empty.child(header_action_button(
                    "catalog-retry-sidebar",
                    tr("再試行"),
                    None,
                    HeaderAction::RetryCatalog,
                    cx,
                ));
            }
            empty
        });
    } else {
        for stream in streams.into_iter().take(recommended_limit) {
            let selected = stream.channel_slug == selected_channel_slug;
            recommended_rows = recommended_rows.child(sidebar_stream_row(stream, selected, cx));
        }
    }

    let mut recommended_section = div()
        .id("sidebar-recommended-section")
        .flex()
        .flex_col()
        .child(
            div()
                .px_3()
                .pt_2()
                .pb_1()
                .text_size(px(11.0))
                .text_color(rgb(MUTED))
                .child(tr("おすすめ")),
        )
        .child(recommended_rows);
    if recommended_count > 10 {
        recommended_section = recommended_section.child(
            div()
                .id("toggle-recommended-count")
                .role(Role::Button)
                .aria_label(if show_all_recommended {
                    tr("おすすめを少なく表示")
                } else {
                    tr("おすすめをもっと見る")
                })
                .tab_index(0)
                .px_3()
                .py_2()
                .text_size(px(11.0))
                .text_color(rgb(SUBTLE))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(RAISED)))
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.show_all_recommended = !this.show_all_recommended;
                    cx.notify();
                }))
                .child(if show_all_recommended {
                    tr("少なく表示")
                } else {
                    tr("もっと見る")
                }),
        );
    }
    sections = sections.child(recommended_section);

    rail.child(sections)
        .child(sidebar_account_area(account_state, cx))
}

fn sidebar_stream_row(
    stream: StreamSummary,
    selected: bool,
    cx: &mut Context<KickShell>,
) -> Stateful<Div> {
    let stream_for_click = stream.clone();
    let stream_for_key = stream.clone();
    let category = stream
        .category
        .clone()
        .unwrap_or_else(|| tr("カテゴリなし").to_owned());
    let viewers = stream
        .viewer_count
        .map(|viewers| t!("{viewers}人", "{viewers}"))
        .unwrap_or_default();
    let avatar = if let Some(url) = &stream.profile_picture_url {
        cached_circular_avatar(url, 32.0).into_any()
    } else {
        avatar_placeholder(&stream.channel_name, 32.0).into_any()
    };

    let mut row = div()
        .id(format!("sidebar-stream-{}", stream.channel_slug))
        .role(Role::Button)
        .aria_label(format!(
            "{}、{}、{}",
            stream.channel_name, category, viewers
        ))
        .aria_selected(selected)
        .tab_index(0)
        .w_full()
        .flex_shrink_0()
        .min_h(px(48.0))
        .px_2()
        .py_1()
        .flex()
        .items_center()
        .gap_2()
        .rounded_md()
        .cursor_pointer()
        .on_click(cx.listener(move |this, _event, _window, cx| {
            this.select_summary(stream_for_click.clone(), cx);
            cx.notify();
        }))
        .on_key_down(cx.listener(move |this, event, _window, cx| {
            if is_activation_key(event) {
                this.select_summary(stream_for_key.clone(), cx);
                cx.notify();
            }
        }))
        .child(avatar)
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(rgb(TEXT))
                        .truncate()
                        .child(stream.channel_name.clone()),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(rgb(MUTED))
                        .truncate()
                        .child(category),
                ),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .text_size(px(11.0))
                .text_color(rgb(SUBTLE))
                .child(div().w(px(6.0)).h(px(6.0)).rounded_full().bg(rgb(ACCENT)))
                .child(viewers),
        );
    row = if selected {
        row.bg(rgb(SELECTED_ROW))
    } else {
        row.hover(|style| style.bg(rgb(RAISED)))
    };
    row.focus_visible(|style| style.bg(rgb(RAISED)))
}

fn panel_divider(
    id: &'static str,
    accessible_label: &'static str,
    divider: PanelDivider,
    current_width: f32,
    min_width: f32,
    max_width: f32,
    cx: &mut Context<KickShell>,
) -> Stateful<Div> {
    let accessible_label = t!(
        "{accessible_label}。現在の幅 {:.0} ポイント",
        "{accessible_label}. Current width {:.0} points",
        current_width
    );
    div()
        .id(id)
        .role(Role::Slider)
        .aria_label(accessible_label)
        .aria_numeric_value(f64::from(current_width))
        .aria_min_numeric_value(f64::from(min_width))
        .aria_max_numeric_value(f64::from(max_width))
        .aria_orientation(gpui::accesskit::Orientation::Horizontal)
        .tab_index(0)
        .w(px(PANEL_DIVIDER_WIDTH))
        .min_w(px(PANEL_DIVIDER_WIDTH))
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .cursor_col_resize()
        .hover(|style| style.bg(rgb(RAISED)))
        .focus_visible(|style| style.bg(rgb(RAISED)))
        .on_drag(divider, |_, _, _, cx| cx.new(|_| gpui::Empty))
        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
            if event.is_held || event.keystroke.modifiers != Modifiers::none() {
                return;
            }
            let delta = match (divider, event.keystroke.key.as_str()) {
                (PanelDivider::Navigation, "arrowleft") => -CHAT_RESIZE_STEP,
                (PanelDivider::Navigation, "arrowright") => CHAT_RESIZE_STEP,
                (PanelDivider::Chat, "arrowleft") => CHAT_RESIZE_STEP,
                (PanelDivider::Chat, "arrowright") => -CHAT_RESIZE_STEP,
                _ => return,
            };
            match divider {
                PanelDivider::Navigation => {
                    let show_chat = window.bounds().size.width >= px(780.0) && this.chat_visible;
                    let width_limit = navigation_width_limit_for_window(
                        window.bounds().size.width.as_f32(),
                        show_chat,
                    );
                    this.navigation_width = (this.navigation_width + delta)
                        .clamp(min_width, max_width.min(width_limit));
                }
                PanelDivider::Chat => {
                    let window_width = window.bounds().size.width.as_f32();
                    let show_navigation =
                        this.sidebar_visible && !this.theater_mode && window_width >= 780.0;
                    let navigation_limit = navigation_width_limit_for_window(
                        window_width,
                        window_width >= 780.0 && this.chat_visible,
                    );
                    let navigation_width = this.navigation_width.min(navigation_limit);
                    let width_limit = chat_width_limit_for_window(
                        window_width,
                        navigation_width,
                        show_navigation,
                    );
                    this.chat_width =
                        (this.chat_width + delta).clamp(min_width, max_width.min(width_limit));
                }
            }
            this.persist_panel_widths();
            cx.notify();
        }))
}

fn nav_item(
    label: &'static str,
    selected: bool,
    compact: bool,
    cx: &mut Context<KickShell>,
) -> Stateful<Div> {
    let content = if compact {
        gpui::svg()
            .data(nav_icon(label, selected))
            .w(px(16.0))
            .h(px(16.0))
            .text_color(rgb(if selected { TEXT } else { SUBTLE }))
            .into_any()
    } else {
        div().child(tr(label)).into_any()
    };
    let mut item = div()
        .id(label)
        .role(Role::Tab)
        .aria_label(tr(label))
        .aria_selected(selected)
        .tab_index(0)
        .on_click(cx.listener(move |this, _event, _window, cx| {
            this.selected_nav = label;
            this.channel_page_visible = false;
            this.channel_page_forward = false;
            this.feed_forward = false;
            this.set_theater_mode(false);
            match label {
                "ホーム" => this.set_feed_visible(true),
                "ブラウズ" => this.set_feed_visible(true),
                "フォロー中" => {
                    this.set_feed_visible(false);
                    this.start_following_load(cx);
                }
                "イベント" => this.set_feed_visible(false),
                _ => {}
            }
            cx.notify();
        }))
        .on_key_down(cx.listener(move |this, event, _window, cx| {
            if is_activation_key(event) {
                this.selected_nav = label;
                this.channel_page_visible = false;
                this.channel_page_forward = false;
                this.feed_forward = false;
                this.set_theater_mode(false);
                match label {
                    "ホーム" => this.set_feed_visible(true),
                    "ブラウズ" => this.set_feed_visible(true),
                    "フォロー中" => {
                        this.set_feed_visible(false);
                        this.start_following_load(cx);
                    }
                    "イベント" => this.set_feed_visible(false),
                    _ => {}
                }
                cx.notify();
            }
        }))
        .h(px(34.0))
        .w_full()
        .flex()
        .items_center()
        .rounded_md()
        .cursor_pointer()
        .child(content);

    item = if compact {
        item.justify_center().text_size(px(16.0))
    } else {
        item.px_3().text_size(px(13.0))
    };

    item = if selected {
        item.bg(rgb(SELECTED_ROW)).text_color(rgb(TEXT))
    } else {
        item.text_color(rgb(SUBTLE))
            .hover(|style| style.bg(rgb(RAISED)))
    };

    item.focus_visible(|style| style.bg(rgb(RAISED)))
}

fn nav_icon(label: &str, selected: bool) -> &'static [u8] {
    match (label, selected) {
        ("ホーム", true) => include_bytes!("../assets/home-filled.svg"),
        ("ホーム", false) => include_bytes!("../assets/home.svg"),
        ("ブラウズ", true) => include_bytes!("../assets/browse-filled.svg"),
        ("ブラウズ", false) => include_bytes!("../assets/browse.svg"),
        ("フォロー中", _) => include_bytes!("../assets/following.svg"),
        ("イベント", _) => include_bytes!("../assets/events.svg"),
        _ => include_bytes!("../assets/browse.svg"),
    }
}

fn sidebar_header(collapsed: bool, cx: &mut Context<KickShell>) -> Div {
    let toggle_size = if collapsed { 28.0 } else { 32.0 };
    let mut header = div().flex().items_center().gap_1().px_2().pt_3().pb_2();
    if collapsed {
        header = header.justify_center();
    }
    header = header.child(
        div()
            .id("sidebar-toggle")
            .role(Role::Button)
            .aria_label(if collapsed {
                tr("サイドバーを展開")
            } else {
                tr("サイドバーを折りたたむ")
            })
            .tab_index(0)
            .w(px(toggle_size))
            .h(px(toggle_size))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .text_color(rgb(SUBTLE))
            .cursor_pointer()
            .hover(|style| style.bg(rgb(RAISED)))
            .focus_visible(|style| style.bg(rgb(RAISED)))
            .on_click(cx.listener(|this, _event, _window, cx| {
                this.sidebar_collapsed = !this.sidebar_collapsed;
                cx.notify();
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                if is_activation_key(event) {
                    this.sidebar_collapsed = !this.sidebar_collapsed;
                    cx.notify();
                }
            }))
            .child(
                gpui::svg()
                    .data(include_bytes!("../assets/menu.svg"))
                    .w(px(if collapsed { 16.0 } else { 18.0 }))
                    .h(px(if collapsed { 16.0 } else { 18.0 }))
                    .text_color(rgb(SUBTLE)),
            ),
    );
    if !collapsed {
        header = header.child(
            gpui::svg()
                .data(include_bytes!("../assets/kick-logo.svg"))
                .w(px(72.0))
                .h(px(24.0))
                .text_color(rgb(ACCENT)),
        );
    }
    header
}

fn login_action_button(
    id: &'static str,
    label: impl Into<String>,
    primary: bool,
    cx: &mut Context<KickShell>,
    action: impl Fn(&mut KickShell, &mut Context<KickShell>) + 'static,
) -> Stateful<Div> {
    let label = label.into();
    let mut button = div()
        .id(id)
        .role(Role::Button)
        .aria_label(label.clone())
        .tab_index(0)
        .px_3()
        .py_2()
        .rounded_md()
        .text_size(px(12.0))
        .cursor_pointer()
        .hover(|style| style.opacity(0.9))
        .on_click(cx.listener(move |this, _event, _window, cx| action(this, cx)))
        .child(label);
    if primary {
        button = button.bg(rgb(ACCENT)).text_color(rgb(0x0B0B0F));
    } else {
        button = button.bg(rgb(RAISED)).text_color(rgb(TEXT));
    }
    button
}

fn login_panel(
    input: &str,
    error: Option<&str>,
    focus: &FocusHandle,
    cx: &mut Context<KickShell>,
) -> Div {
    let mut card = div()
        .w(px(540.0))
        .rounded(px(14.0))
        .bg(rgb(RAIL_BG))
        .p_6()
        .flex()
        .flex_col()
        .gap_3()
        .child(
            div()
                .text_size(px(18.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(tr("Kick にログイン")),
        )
        .child(
            div()
                .text_size(px(12.0))
                .text_color(rgb(SUBTLE))
                .child(tr("Kick の session_token の値（DevTools → Application → Cookies → kick.com）を貼り付けてください。")),
        )
        .child(
            div()
                .id("login-input")
                .track_focus(focus)
                .role(Role::TextInput)
                .aria_label(tr("session_token"))
                .tab_index(0)
                .h(px(42.0))
                .px_3()
                .rounded_md()
                .bg(rgb(APP_BG))
                .flex()
                .items_center()
                .text_size(px(13.0))
                .text_color(rgb(TEXT))
                .cursor_text()
                .on_click(cx.listener(|this, _event, window, cx| {
                    this.login_input_focus.focus(window, cx);
                }))
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                    if event.is_held {
                        return;
                    }
                    if event.keystroke.modifiers.platform || event.keystroke.modifiers.control {
                        return;
                    }
                    match event.keystroke.key.as_str() {
                        "backspace" => {
                            this.login_input.pop();
                            this.login_error = None;
                            cx.notify();
                        }
                        "enter" => this.submit_login(cx),
                        _ => {
                            if let Some(character) = event.keystroke.key_char.as_deref() {
                                this.login_input.push_str(character);
                                this.login_error = None;
                                cx.notify();
                            }
                        }
                    }
                }))
                .child(if input.is_empty() {
                    div().text_color(rgb(MUTED)).child(tr("session_token の値"))
                } else {
                    div().child("•".repeat(input.chars().count().min(64)))
                }),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(login_action_button(
                    "login-paste",
                    tr("クリップボードから貼り付け"),
                    false,
                    cx,
                    |this, cx| this.paste_login_clipboard(cx),
                ))
                .child(login_action_button(
                    "login-help",
                    tr("コピー方法"),
                    false,
                    cx,
                    |_this, _cx| credentials::show_login_help(),
                ))
                .child(login_action_button(
                    "login-json",
                    tr("JSONファイルを読み込む…"),
                    false,
                    cx,
                    |this, cx| {
                        this.login_panel_open = false;
                        this.player_host.borrow_mut().set_video_hidden(false);
                        this.begin_login(cx);
                    },
                )),
        );
    if let Some(error) = error {
        card = card.child(
            div()
                .text_size(px(11.0))
                .text_color(rgb(DANGER))
                .child(error.to_owned()),
        );
    }
    let card = card.child(
        div()
            .flex()
            .justify_end()
            .gap_2()
            .child(login_action_button(
                "login-cancel",
                tr("キャンセル"),
                false,
                cx,
                |this, cx| this.close_login_panel(cx),
            ))
            .child(login_action_button(
                "login-submit",
                tr("ログインする"),
                true,
                cx,
                |this, cx| this.submit_login(cx),
            )),
    );
    div()
        .absolute()
        .inset_0()
        .bg(rgba(0x000000AA))
        .flex()
        .items_center()
        .justify_center()
        .child(card)
}

fn sidebar_account_area(account_state: AccountState, cx: &mut Context<KickShell>) -> Div {
    let failure_reason = match &account_state {
        AccountState::Failed(reason) => Some(reason.clone()),
        _ => None,
    };
    let account_label = match &account_state {
        AccountState::SignedOut => tr("ログイン").to_owned(),
        AccountState::EnteringCookie => tr("ログイン情報を入力中…").to_owned(),
        AccountState::Checking => tr("認証中…").to_owned(),
        AccountState::SignedIn { username } => format!("@{username}"),
        AccountState::Failed(_) => tr("状態未確認").to_owned(),
    };
    let signed_in = matches!(account_state, AccountState::SignedIn { .. });
    let display_name = match &account_state {
        AccountState::SignedIn { username } => username.as_str(),
        _ => "?",
    };
    let mut row = div()
        .flex()
        .items_center()
        .gap_2()
        .child(avatar_placeholder(display_name, 24.0))
        .child(
            div()
                .id("sidebar-account")
                .role(Role::Button)
                .aria_label(account_state.accessibility_label())
                .tab_index(0)
                .px_2()
                .py_1()
                .rounded_md()
                .text_size(px(12.0))
                .text_color(rgb(if signed_in { TEXT } else { SUBTLE }))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(RAISED)))
                .focus_visible(|style| style.bg(rgb(RAISED)))
                .on_click(cx.listener(|this, _event, window, cx| {
                    this.account_button_action(window, cx);
                }))
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                    if is_activation_key(event) {
                        this.account_button_action(window, cx);
                    }
                }))
                .child(account_label),
        );
    let mut area = div().flex().flex_col().gap_1().px_2().py_2().child(row);
    if let Some(reason) = failure_reason {
        area = area.child(
            div()
                .px_2()
                .text_size(px(10.0))
                .text_color(rgb(SUBTLE))
                .truncate()
                .child(reason),
        );
    }
    area
}

fn sidebar_section_status(message: &str) -> Div {
    div()
        .px_3()
        .py_2()
        .text_size(px(11.0))
        .text_color(rgb(MUTED))
        .child(message.to_owned())
}

fn sidebar_search_field(
    search_query: &str,
    search_focus: &FocusHandle,
    cx: &mut Context<KickShell>,
) -> Stateful<Div> {
    let value = if search_query.is_empty() {
        tr("検索")
    } else {
        search_query
    };
    let color = if search_query.is_empty() { MUTED } else { TEXT };
    div()
        .id("sidebar-search")
        .track_focus(search_focus)
        .tab_index(0)
        .aria_label(tr("Kickを検索"))
        .w_full()
        .h(px(36.0))
        .px_3()
        .flex()
        .items_center()
        .gap_2()
        .rounded_full()
        .bg(rgb(RAISED))
        .text_size(px(13.0))
        .text_color(rgb(color))
        .cursor_text()
        .focus_visible(|style| style.bg(rgb(SELECTED_ROW)))
        .on_click(cx.listener(|this, _event, window, cx| {
            this.search_focus.focus(window, cx);
        }))
        .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
            if event.is_held {
                return;
            }
            let key = event.keystroke.key.as_str();
            if key == "enter" {
                this.start_search(cx);
                return;
            }
            if key == "backspace" {
                this.search_query.pop();
                this.search_state = SearchState::Idle;
                this.search_generation = this.search_generation.wrapping_add(1);
                cx.notify();
            } else if !event.keystroke.modifiers.modified()
                && let Some(character) = event.keystroke.key_char.as_deref()
                && !character.chars().any(char::is_control)
            {
                this.search_query.push_str(character);
                this.search_state = SearchState::Idle;
                this.search_generation = this.search_generation.wrapping_add(1);
                cx.notify();
            }
        }))
        .child(
            div()
                .id("sidebar-search-submit")
                .role(Role::Button)
                .aria_label(tr("検索を実行"))
                .tab_index(0)
                .w(px(16.0))
                .h(px(16.0))
                .flex()
                .items_center()
                .justify_center()
                .text_color(rgb(MUTED))
                .cursor_pointer()
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.start_search(cx);
                }))
                .child(
                    gpui::svg()
                        .data(include_bytes!("../assets/search.svg"))
                        .w(px(14.0))
                        .h(px(14.0))
                        .text_color(rgb(MUTED)),
                ),
        )
        .child(value.to_owned())
        .when(!search_query.is_empty(), |field| {
            field.child(
                div()
                    .id("clear-sidebar-search")
                    .role(Role::Button)
                    .aria_label(tr("検索をクリア"))
                    .tab_index(0)
                    .px_1()
                    .text_color(rgb(SUBTLE))
                    .cursor_pointer()
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                        if is_activation_key(event) {
                            this.search_query.clear();
                            this.start_search(cx);
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.search_query.clear();
                        this.start_search(cx);
                        cx.notify();
                    }))
                    .child("×"),
            )
        })
}

/// Renders one of the bundled Kick SVG icons.
fn kick_icon(data: &'static [u8], size: f32, color: u32) -> gpui::Svg {
    gpui::svg()
        .data(data)
        .w(px(size))
        .h(px(size))
        .flex_shrink_0()
        .text_color(rgb(color))
}

fn avatar_cache_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join("Library/Caches/KickRust/avatars")
}

fn avatar_cache_path(url: &str) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    url.hash(&mut hasher);
    avatar_cache_dir().join(format!("{:016x}.png", hasher.finish()))
}

async fn fetch_and_crop_avatar(url: &str, path: &Path) -> Result<PathBuf, String> {
    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
        .build()
        .map_err(|error| format!("client: {error}"))?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|error| format!("send: {error}"))?;
    let status = response.status();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_owned();
    if !status.is_success() {
        return Err(format!("status {status} content-type {content_type}"));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|error| format!("body: {error}"))?;
    let image = image::load_from_memory(&bytes).map_err(|error| format!("decode: {error}"))?;
    let (width, height) = (image.width(), image.height());
    let side = width.min(height);
    if side == 0 {
        return Err("empty image".to_owned());
    }
    let x = (width - side) / 2;
    let y = (height - side) / 2;
    let cropped =
        image
            .crop_imm(x, y, side, side)
            .resize(192, 192, image::imageops::FilterType::Lanczos3);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("mkdir: {error}"))?;
    }
    cropped
        .save(path)
        .map_err(|error| format!("save: {error}"))?;
    Ok(path.to_path_buf())
}

/// Renders a circular avatar from a pre-cropped square file. The image is
/// square, so ObjectFit::Contain fills it and the face stays centered.
fn circular_avatar_path(path: PathBuf, size: f32) -> Div {
    div()
        .w(px(size))
        .h(px(size))
        .flex_shrink_0()
        .rounded_full()
        .overflow_hidden()
        .bg(rgb(RAISED))
        .child(
            img(path)
                .w(px(size))
                .h(px(size))
                .flex_shrink_0()
                .rounded_full()
                .overflow_hidden()
                .object_fit(gpui::ObjectFit::Contain),
        )
}

/// Uses the pre-cropped square avatar when available, falling back to the
/// URL image until the background crop finishes.
fn cached_circular_avatar(url: &str, size: f32) -> Div {
    let path = avatar_cache_path(url);
    if path.exists() {
        circular_avatar_path(path, size)
    } else {
        circular_avatar(url.to_owned(), size)
    }
}

/// Renders a circular avatar. The image element is explicitly square so
/// ObjectFit::Contain centers portrait sources; the image's own rounded clip
/// keeps the avatar circular even when the container's clip does not apply to
/// image paint.
fn circular_avatar(url: String, size: f32) -> Div {
    div()
        .w(px(size))
        .h(px(size))
        .flex_shrink_0()
        .rounded_full()
        .overflow_hidden()
        .bg(rgb(RAISED))
        .child(
            img(url)
                .w(px(size))
                .h(px(size))
                .flex_shrink_0()
                .rounded_full()
                .object_fit(gpui::ObjectFit::Contain),
        )
}

fn avatar_placeholder(name: &str, size: f32) -> Div {
    let initial = name
        .chars()
        .next()
        .unwrap_or('?')
        .to_uppercase()
        .to_string();
    div()
        .w(px(size))
        .h(px(size))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(rgb(RAISED))
        .text_size(px(size * 0.4))
        .text_color(rgb(SUBTLE))
        .child(initial)
}

fn following_page(
    state: FollowingState,
    tab: FollowingTab,
    channels: &[FollowedChannel],
    has_more: bool,
    loading_more: bool,
    account_state: &AccountState,
    selected_channel_slug: &str,
    emote_image_cache: Entity<BoundedEmoteImageCache>,
    cx: &mut Context<KickShell>,
) -> Stateful<Div> {
    let mut page = div()
        .image_cache(emote_image_cache)
        .id("following-page")
        .flex_1()
        .min_h(px(0.0))
        .w_full()
        .flex()
        .flex_col()
        .gap_3()
        .p_4()
        .overflow_y_scroll();
    page = page.child(
        div()
            .text_size(px(20.0))
            .text_color(rgb(TEXT))
            .child(tr("フォロー中")),
    );
    let mut tabs = div().flex().items_center().gap_4();
    for candidate in FollowingTab::ALL {
        tabs = tabs.child(following_tab_button(candidate, candidate == tab, cx));
    }
    page = page.child(tabs);

    if !matches!(account_state, AccountState::SignedIn { .. }) {
        return page
            .child(
                div()
                    .text_size(px(13.0))
                    .text_color(rgb(MUTED))
                    .child(tr("フォロー中のチャンネルを見るにはログインしてください")),
            )
            .child(
                div()
                    .id("following-login")
                    .role(Role::Button)
                    .aria_label(tr("ログイン"))
                    .tab_index(0)
                    .px_3()
                    .py_2()
                    .w(px(120.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(rgb(ACCENT))
                    .text_size(px(12.0))
                    .text_color(rgb(0x0B0B0F))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.begin_login(cx);
                    }))
                    .child(tr("ログイン")),
            );
    }

    match state {
        FollowingState::NotRequested | FollowingState::Loading => page.child(
            div()
                .text_size(px(13.0))
                .text_color(rgb(MUTED))
                .child(tr("フォロー中のチャンネルを取得しています…")),
        ),
        FollowingState::Failed(error) => page
            .child(
                div()
                    .text_size(px(13.0))
                    .text_color(rgb(DANGER))
                    .child(error.user_message()),
            )
            .child(
                div()
                    .id("retry-following")
                    .role(Role::Button)
                    .aria_label(tr("フォロー中を再試行"))
                    .tab_index(0)
                    .px_3()
                    .py_2()
                    .w(px(150.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .bg(rgb(RAISED))
                    .text_size(px(12.0))
                    .text_color(rgb(TEXT))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.start_following_load(cx);
                    }))
                    .child(tr("再試行")),
            ),
        FollowingState::Ready => {
            let visible: Vec<&FollowedChannel> = match tab {
                FollowingTab::Live => channels.iter().filter(|channel| channel.is_live).collect(),
                FollowingTab::All => channels.iter().collect(),
                FollowingTab::Categories => channels.iter().collect(),
                FollowingTab::Upcoming => Vec::new(),
            };
            if tab == FollowingTab::Upcoming {
                page = page.child(
                    div()
                        .text_size(px(13.0))
                        .text_color(rgb(MUTED))
                        .child(tr("予定されている配信はありません")),
                );
            } else if visible.is_empty() {
                let message = if tab == FollowingTab::Live {
                    tr("ライブ中のフォロー中チャンネルはありません")
                } else {
                    tr("フォロー中のチャンネルはありません")
                };
                page = page.child(
                    div()
                        .text_size(px(13.0))
                        .text_color(rgb(MUTED))
                        .child(message),
                );
            } else if tab == FollowingTab::Categories {
                let mut groups: Vec<(&str, Vec<&FollowedChannel>)> = Vec::new();
                for channel in visible {
                    let category = channel
                        .category_name
                        .as_deref()
                        .unwrap_or(tr("カテゴリなし"));
                    if let Some((_, group)) = groups.iter_mut().find(|(name, _)| *name == category)
                    {
                        group.push(channel);
                    } else {
                        groups.push((category, vec![channel]));
                    }
                }
                for (category, group) in groups {
                    page = page.child(
                        div()
                            .text_size(px(13.0))
                            .text_color(rgb(SUBTLE))
                            .child(category.to_owned()),
                    );
                    for channel in group {
                        page = page.child(following_channel_row(
                            channel,
                            channel.channel_slug == selected_channel_slug,
                            cx,
                        ));
                    }
                }
            } else {
                for channel in visible {
                    page = page.child(following_channel_row(
                        channel,
                        channel.channel_slug == selected_channel_slug,
                        cx,
                    ));
                }
            }
            if has_more && tab != FollowingTab::Upcoming {
                page = page.child(
                    div()
                        .id("following-more")
                        .role(Role::Button)
                        .aria_label(tr("フォロー中をさらに読み込む"))
                        .tab_index(0)
                        .px_3()
                        .py_2()
                        .w(px(180.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .bg(rgb(RAISED))
                        .text_size(px(12.0))
                        .text_color(rgb(if loading_more { MUTED } else { TEXT }))
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.load_more_following(cx);
                        }))
                        .child(if loading_more {
                            tr("読み込み中…")
                        } else {
                            tr("もっと見る")
                        }),
                );
            }
            page
        }
    }
}

fn following_tab_button(
    tab: FollowingTab,
    selected: bool,
    cx: &mut Context<KickShell>,
) -> Stateful<Div> {
    div()
        .id(format!("following-tab-{}", tab.label()))
        .role(Role::Tab)
        .aria_label(tab.label())
        .aria_selected(selected)
        .tab_index(0)
        .pb_2()
        .text_size(px(13.0))
        .text_color(rgb(if selected { TEXT } else { MUTED }))
        .border_b_2()
        .border_color(rgb(if selected { ACCENT } else { LINE }))
        .cursor_pointer()
        .hover(|style| style.text_color(rgb(TEXT)))
        .focus_visible(|style| style.text_color(rgb(TEXT)))
        .on_click(cx.listener(move |this, _event, _window, cx| {
            this.select_following_tab(tab, cx);
        }))
        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _window, cx| {
            if is_activation_key(event) {
                this.select_following_tab(tab, cx);
            }
        }))
        .child(tab.label())
}

fn following_channel_row(
    channel: &FollowedChannel,
    selected: bool,
    cx: &mut Context<KickShell>,
) -> Stateful<Div> {
    let summary_for_click = channel.to_summary();
    let summary_for_key = summary_for_click.clone();
    let avatar = if let Some(url) = &channel.profile_picture_url {
        img(url.clone())
            .w(px(40.0))
            .h(px(40.0))
            .flex_shrink_0()
            .rounded_full()
            .overflow_hidden()
            .into_any()
    } else {
        avatar_placeholder(&channel.channel_name, 40.0).into_any()
    };
    let viewers = channel
        .viewer_count
        .map(|viewers| t!("{viewers}人", "{viewers}"))
        .unwrap_or_default();
    let mut row = div()
        .id(format!("following-channel-{}", channel.channel_slug))
        .role(Role::Button)
        .aria_label(format!(
            "{}、{}、{}",
            channel.channel_name,
            channel
                .session_title
                .as_deref()
                .unwrap_or(tr("配信タイトルなし")),
            if channel.is_live {
                tr("ライブ中")
            } else {
                tr("オフライン")
            }
        ))
        .aria_selected(selected)
        .tab_index(0)
        .w_full()
        .min_h(px(64.0))
        .px_2()
        .py_2()
        .flex()
        .items_center()
        .gap_3()
        .rounded_md()
        .cursor_pointer()
        .on_click(cx.listener(move |this, _event, _window, cx| {
            this.select_summary(summary_for_click.clone(), cx);
            cx.notify();
        }))
        .on_key_down(cx.listener(move |this, event, _window, cx| {
            if is_activation_key(event) {
                this.select_summary(summary_for_key.clone(), cx);
                cx.notify();
            }
        }))
        .child(avatar)
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_size(px(13.0))
                        .text_color(rgb(TEXT))
                        .truncate()
                        .child(channel.channel_name.clone()),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(rgb(MUTED))
                        .truncate()
                        .child(
                            channel
                                .session_title
                                .clone()
                                .unwrap_or_else(|| tr("配信タイトルなし").to_owned()),
                        ),
                ),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .text_size(px(11.0))
                .text_color(rgb(if channel.is_live { ACCENT } else { MUTED }))
                .child(if channel.is_live { "●" } else { "○" })
                .child(viewers),
        );
    row = if selected {
        row.bg(rgb(SELECTED_ROW))
    } else {
        row.hover(|style| style.bg(rgb(RAISED)))
    };
    row.focus_visible(|style| style.bg(rgb(RAISED)))
}

fn settings_section(title: &'static str, description: &'static str) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_3()
        .p_4()
        .rounded_md()
        .bg(rgb(RAIL_BG))
        .border_1()
        .border_color(rgb(LINE))
        .child(div().text_size(px(15.0)).text_color(rgb(TEXT)).child(title))
        .child(
            div()
                .text_size(px(12.0))
                .text_color(rgb(MUTED))
                .child(description),
        )
}

fn settings_toggle(
    id: &'static str,
    label: &'static str,
    selected: bool,
    cx: &mut Context<KickShell>,
    action: impl Fn(&mut KickShell, &mut Context<KickShell>) + 'static,
) -> Stateful<Div> {
    let action = Rc::new(action);
    let click_action = action.clone();
    let key_action = action;
    div()
        .id(id)
        .role(Role::Button)
        .aria_label(label)
        .tab_index(0)
        .px_4()
        .py_2()
        .rounded_md()
        .border_1()
        .border_color(rgb(if selected { ACCENT } else { LINE }))
        .bg(rgb(if selected { SELECTED_ROW } else { RAISED }))
        .text_size(px(12.0))
        .text_color(rgb(if selected { TEXT } else { SUBTLE }))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(SELECTED_ROW)))
        .on_click(cx.listener(move |this, _event, _window, cx| {
            click_action(this, cx);
        }))
        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _window, cx| {
            if is_activation_key(event) {
                key_action(this, cx);
            }
        }))
        .child(label)
}

fn settings_tab_button(
    tab: SettingsTab,
    label: &'static str,
    selected: bool,
    cx: &mut Context<KickShell>,
) -> Stateful<Div> {
    let id = match tab {
        SettingsTab::General => "settings-tab-general",
        SettingsTab::Video => "settings-tab-video",
        SettingsTab::Notifications => "settings-tab-notifications",
        SettingsTab::App => "settings-tab-app",
    };
    div()
        .id(id)
        .role(Role::Tab)
        .aria_label(label)
        .aria_selected(selected)
        .tab_index(0)
        .px_3()
        .py_2()
        .rounded_md()
        .border_1()
        .border_color(rgb(if selected { ACCENT } else { LINE }))
        .bg(rgb(if selected { SELECTED_ROW } else { RAISED }))
        .text_size(px(12.0))
        .text_color(rgb(if selected { TEXT } else { SUBTLE }))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(SELECTED_ROW)))
        .on_click(cx.listener(move |this, _event, _window, cx| {
            this.set_settings_tab(tab, cx);
        }))
        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _window, cx| {
            if is_activation_key(event) {
                this.set_settings_tab(tab, cx);
            }
        }))
        .child(label)
}

fn settings_tabs(selected: SettingsTab, cx: &mut Context<KickShell>) -> Stateful<Div> {
    div()
        .id("settings-tabs")
        .role(Role::TabList)
        .flex()
        .items_center()
        .gap_1()
        .px_4()
        .py_2()
        .border_b_1()
        .border_color(rgb(LINE))
        .child(settings_tab_button(
            SettingsTab::General,
            tr("一般"),
            selected == SettingsTab::General,
            cx,
        ))
        .child(settings_tab_button(
            SettingsTab::Video,
            tr("映像"),
            selected == SettingsTab::Video,
            cx,
        ))
        .child(settings_tab_button(
            SettingsTab::Notifications,
            tr("通知"),
            selected == SettingsTab::Notifications,
            cx,
        ))
        .child(settings_tab_button(
            SettingsTab::App,
            tr("アプリ"),
            selected == SettingsTab::App,
            cx,
        ))
}

fn settings_page(
    chat_delay_seconds: u64,
    pending_count: usize,
    debug_mode: bool,
    notify_channels: &HashSet<String>,
    danmaku_enabled: bool,
    danmaku_font_size: f32,
    danmaku_opacity: f32,
    default_quality: PlayerQualityCap,
    notification_permission: i32,
    notification_notice: Option<&str>,
    settings_tab: SettingsTab,
    followed_limit: usize,
    raid_display_seconds: u64,
    cx: &mut Context<KickShell>,
) -> impl IntoElement {
    let mut raid_options = div().flex().flex_wrap().items_center().gap_2();
    for seconds in RAID_DISPLAY_OPTIONS {
        let selected = seconds == raid_display_seconds;
        raid_options = raid_options.child(settings_toggle(
            match seconds {
                5 => "raid-display-5",
                10 => "raid-display-10",
                15 => "raid-display-15",
                _ => "raid-display-30",
            },
            match seconds {
                5 => tr("5秒"),
                10 => tr("10秒"),
                15 => tr("15秒"),
                _ => tr("30秒"),
            },
            selected,
            cx,
            move |this, cx| this.set_raid_display_seconds(seconds, cx),
        ));
    }

    let mut delay_options = div().flex().flex_wrap().items_center().gap_2();
    for seconds in CHAT_DELAY_OPTIONS {
        let selected = seconds == chat_delay_seconds;
        let label = if seconds == 0 {
            tr("オフ").to_owned()
        } else {
            t!("{seconds}秒", "{seconds}s")
        };
        delay_options = delay_options.child(
            div()
                .id(gpui::ElementId::named_usize(
                    "chat-delay-option",
                    seconds as usize,
                ))
                .role(Role::Button)
                .aria_label(t!("コメント遅延 {label}", "Comment delay {label}"))
                .tab_index(0)
                .px_4()
                .py_2()
                .rounded_md()
                .border_1()
                .border_color(rgb(if selected { ACCENT } else { LINE }))
                .bg(rgb(if selected { SELECTED_ROW } else { RAISED }))
                .text_size(px(12.0))
                .text_color(rgb(if selected { TEXT } else { SUBTLE }))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(SELECTED_ROW)))
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.set_chat_delay(seconds, cx);
                }))
                .child(label),
        );
    }

    let status = if chat_delay_seconds == 0 {
        tr("コメントはリアルタイムで表示されます。").to_owned()
    } else if pending_count > 0 {
        t!(
            "{pending_count} 件のコメントを保持中です。{chat_delay_seconds} 秒後に表示されます。",
            "Holding {pending_count} comments. They appear after {chat_delay_seconds} seconds."
        )
    } else {
        t!(
            "新しいコメントは {chat_delay_seconds} 秒遅れて表示されます。",
            "New comments appear {chat_delay_seconds} seconds later."
        )
    };

    div()
        .id("settings-page")
        .flex_1()
        .min_h(px(0.0))
        .w_full()
        .flex()
        .flex_col()
        .bg(rgb(APP_BG))
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .px_4()
                .py_3()
                .border_b_1()
                .border_color(rgb(LINE))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(div().text_size(px(20.0)).text_color(rgb(TEXT)).child(tr("設定")))
                        .child(
                            div()
                                .text_size(px(12.0))
                                .text_color(rgb(MUTED))
                                .child(tr("Kick Native の動作を変更します")),
                        ),
                )
                .child(
                    div()
                        .id("close-settings")
                        .role(Role::Button)
                        .aria_label(tr("設定を閉じる"))
                        .tab_index(0)
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .text_size(px(12.0))
                        .text_color(rgb(SUBTLE))
                        .cursor_pointer()
                        .hover(|style| style.bg(rgb(RAISED)))
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.close_settings(cx);
                        }))
                        .child(tr("閉じる")),
                ),
        )
        .child(settings_tabs(settings_tab, cx))
        .child(
            div()
                .id("settings-scroll")
                .flex_1()
                .min_h(px(0.0))
                .overflow_y_scroll()
                .p_4()
                .flex()
                .flex_col()
                .gap_4()
                .when(settings_tab == SettingsTab::General, |page| page.child(
                    settings_section(
                        tr("フォロー中の表示"),
                        tr("左のサイドバーに表示するフォロー中チャンネル数です。"),
                    )
                    .child(
                        div()
                            .text_size(px(13.0))
                            .text_color(rgb(TEXT))
                            .child(tr("フォロー中の表示数")),
                    )
                    .child({
                        let mut options = div().flex().items_center().gap_2();
                        for limit in FOLLOWED_LIMIT_OPTIONS {
                            let id = match limit {
                                5 => "followed-limit-5",
                                8 => "followed-limit-8",
                                12 => "followed-limit-12",
                                20 => "followed-limit-20",
                                30 => "followed-limit-30",
                                _ => "followed-limit-50",
                            };
                            let label = match limit {
                                5 => "5件",
                                8 => "8件",
                                12 => "12件",
                                20 => "20件",
                                30 => "30件",
                                _ => "50件",
                            };
                            options = options.child(settings_toggle(
                                id,
                                tr(label),
                                followed_limit == limit,
                                cx,
                                move |this, cx| this.set_followed_limit(limit, cx),
                            ));
                        }
                        options
                    })
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(rgb(MUTED))
                            .child(tr("「もっと見る」から全件表示できます。")),
                    ),
                ))
                .when(settings_tab == SettingsTab::General, |page| page.child(
                    settings_section(
                        tr("レイド表示"),
                        tr("レイド通知を表示する時間を選択します。時間が経つと自動で消えます。"),
                    )
                    .child(raid_options),
                ))
                .when(settings_tab == SettingsTab::General, |page| page.child(
                    settings_section(
                        tr("チャット"),
                        tr("映像の遅れに合わせてコメントを遅らせ、ネタバレを防ぎます。"),
                    )
                    .child(
                        div()
                            .text_size(px(13.0))
                            .text_color(rgb(TEXT))
                            .child(tr("コメント遅延")),
                    )
                    .child(delay_options)
                    .child(div().text_size(px(12.0)).text_color(rgb(MUTED)).child(status)),
                ))
                .when(settings_tab == SettingsTab::General, |page| page.child(
                    settings_section(
                        tr("流れるコメント"),
                        tr("映像の上を流れるコメントの表示を設定します。"),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(settings_toggle(
                                "danmaku-off",
                                tr("オフ"),
                                !danmaku_enabled,
                                cx,
                                |this, cx| {
                                    this.set_danmaku(
                                        false,
                                        this.danmaku_font_size,
                                        this.danmaku_opacity,
                                        cx,
                                    )
                                },
                            ))
                            .child(settings_toggle(
                                "danmaku-on",
                                tr("オン"),
                                danmaku_enabled,
                                cx,
                                |this, cx| {
                                    this.set_danmaku(
                                        true,
                                        this.danmaku_font_size,
                                        this.danmaku_opacity,
                                        cx,
                                    )
                                },
                            )),
                    )
                    .child(
                        div()
                            .text_size(px(13.0))
                            .text_color(rgb(TEXT))
                            .child(tr("文字サイズ")),
                    )
                    .child({
                        let mut sizes = div().flex().items_center().gap_2();
                        for (id, label, size) in [
                            ("danmaku-size-s", "小", 14.0_f32),
                            ("danmaku-size-m", "中", 18.0_f32),
                            ("danmaku-size-l", "大", 24.0_f32),
                        ] {
                            sizes = sizes.child(settings_toggle(
                                id,
                                tr(label),
                                (danmaku_font_size - size).abs() < 0.5,
                                cx,
                                move |this, cx| {
                                    this.set_danmaku(
                                        this.danmaku_enabled,
                                        size,
                                        this.danmaku_opacity,
                                        cx,
                                    )
                                },
                            ));
                        }
                        sizes
                    })
                    .child(
                        div()
                            .text_size(px(13.0))
                            .text_color(rgb(TEXT))
                            .child(tr("不透明度")),
                    )
                    .child({
                        let mut opacities = div().flex().items_center().gap_2();
                        for (id, label, value) in [
                            ("danmaku-opacity-50", "50%", 0.5_f32),
                            ("danmaku-opacity-75", "75%", 0.75_f32),
                            ("danmaku-opacity-100", "100%", 1.0_f32),
                        ] {
                            opacities = opacities.child(settings_toggle(
                                id,
                                label,
                                (danmaku_opacity - value).abs() < 0.01,
                                cx,
                                move |this, cx| {
                                    this.set_danmaku(
                                        this.danmaku_enabled,
                                        this.danmaku_font_size,
                                        value,
                                        cx,
                                    )
                                },
                            ));
                        }
                        opacities
                    }),
                ))
                .when(settings_tab == SettingsTab::Video, |page| page.child(
                    settings_section(
                        tr("画質"),
                        tr("新しい再生に適用するデフォルトの画質上限を選択します。"),
                    )
                    .child(
                        div()
                            .text_size(px(13.0))
                            .text_color(rgb(TEXT))
                            .child(tr("デフォルト画質")),
                    )
                    .child({
                        let mut qualities = div().flex().items_center().gap_2();
                        for (id, label, quality) in [
                            ("quality-auto", "自動", PlayerQualityCap::Automatic),
                            ("quality-1080", "1080p", PlayerQualityCap::P1080),
                            ("quality-720", "720p", PlayerQualityCap::P720),
                            ("quality-480", "480p", PlayerQualityCap::P480),
                        ] {
                            qualities = qualities.child(settings_toggle(
                                id,
                                tr(label),
                                default_quality == quality,
                                cx,
                                move |this, cx| this.set_default_quality(quality, cx),
                            ));
                        }
                        qualities
                    })
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(rgb(MUTED))
                            .child(tr("再生中の画質はプレイヤーの画質ボタンで変更できます。")),
                    ),
                ))
                .when(settings_tab == SettingsTab::Video, |page| page.child(
                    settings_section(
                        tr("再生"),
                        tr("AVPlayer のバッファリングにより、ライブ端から約 5 秒遅れます。"),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(rgb(SUBTLE))
                            .child(tr("コメント遅延を 5 秒にすると、映像とコメントのタイミングがおおよそ揃います。")),
                    ),
                ))
                .when(settings_tab == SettingsTab::Notifications, |page| page.child({
                    let mut section = settings_section(
                        tr("通知"),
                        tr("配信開始の通知をオンにしたチャンネルです。"),
                    );
                    let permission_label = match notification_permission {
                        1 => tr("拒否"),
                        2 => tr("許可"),
                        3 => tr("仮許可"),
                        4 => tr("一時許可"),
                        _ => tr("未確認"),
                    };
                    section = section.child(
                        div()
                            .flex()
                            .items_center()
                            .flex_wrap()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(rgb(MUTED))
                                    .child(format!("{}: {permission_label}", tr("通知の許可"))),
                            )
                            .child(settings_toggle(
                                "notification-request",
                                tr("通知を許可"),
                                false,
                                cx,
                                |this, cx| this.request_notifications(cx),
                            ))
                            .child(settings_toggle(
                                "notification-test",
                                tr("テスト通知"),
                                false,
                                cx,
                                |this, cx| this.test_notification(cx),
                            ))
                            .child(settings_toggle(
                                "notification-settings",
                                tr("システム設定を開く"),
                                false,
                                cx,
                                |this, cx| {
                                    kick_player_macos::open_notification_settings();
                                    this.refresh_notification_permission(cx);
                                },
                            )),
                    );
                    let background_installed = background_notifier_installed();
                    section = section.child(
                        div()
                            .flex()
                            .items_center()
                            .flex_wrap()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(rgb(MUTED))
                                    .child(if background_installed {
                                        tr("バックグラウンド通知: 有効").to_owned()
                                    } else {
                                        tr("バックグラウンド通知: 無効").to_owned()
                                    }),
                            )
                            .child(settings_toggle(
                                "background-notifier-install",
                                tr("有効にする"),
                                false,
                                cx,
                                |this, cx| this.install_background_notifier(cx),
                            ))
                            .child(settings_toggle(
                                "background-notifier-remove",
                                tr("無効にする"),
                                false,
                                cx,
                                |this, cx| this.uninstall_background_notifier(cx),
                            )),
                    );
                    if let Some(notice) = notification_notice {
                        section = section.child(
                            div()
                                .text_size(px(11.0))
                                .text_color(rgb(ACCENT))
                                .child(notice.to_owned()),
                        );
                    }
                    if notify_channels.is_empty() {
                        section = section.child(
                            div()
                                .text_size(px(12.0))
                                .text_color(rgb(MUTED))
                                .child(tr("通知をオンにしたチャンネルはありません")),
                        );
                    } else {
                        let mut list = div().flex().flex_col().gap_1();
                        for slug in notify_channels.iter().take(12) {
                            let slug_for_remove = slug.clone();
                            list = list.child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_size(px(12.0))
                                            .text_color(rgb(TEXT))
                                            .truncate()
                                            .child(slug.clone()),
                                    )
                                    .child(
                                        div()
                                            .id(format!("notify-remove-{slug}"))
                                            .role(Role::Button)
                                            .aria_label(tr("通知をオフにする"))
                                            .px_2()
                                            .py_1()
                                            .rounded_md()
                                            .bg(rgb(RAISED))
                                            .text_size(px(10.0))
                                            .text_color(rgb(SUBTLE))
                                            .cursor_pointer()
                                            .hover(|style| style.bg(rgb(SELECTED_ROW)))
                                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                                this.remove_notify_channel(
                                                    slug_for_remove.clone(),
                                                    cx,
                                                );
                                            }))
                                            .child(tr("解除")),
                                    ),
                            );
                        }
                        section = section.child(list);
                    }
                    section
                }))
                .when(settings_tab == SettingsTab::General, |page| page.child(
                    settings_section(
                        tr("言語"),
                        tr("アプリの表示言語を選択します。"),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(settings_toggle(
                                "language-ja",
                                "日本語",
                                current_language() == Language::Japanese,
                                cx,
                                |this, cx| this.set_language_setting(Language::Japanese, cx),
                            ))
                            .child(settings_toggle(
                                "language-en",
                                "English",
                                current_language() == Language::English,
                                cx,
                                |this, cx| this.set_language_setting(Language::English, cx),
                            )),
                    ),
                ))
                .when(settings_tab == SettingsTab::App, |page| page.child(
                    settings_section(
                        tr("デバッグ"),
                        tr("統計情報を別ウィンドウで表示します。"),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(settings_toggle(
                                "debug-off",
                                tr("オフ"),
                                !debug_mode,
                                cx,
                                |this, cx| this.set_debug_mode(false, cx),
                            ))
                            .child(settings_toggle(
                                "debug-on",
                                tr("オン"),
                                debug_mode,
                                cx,
                                |this, cx| this.set_debug_mode(true, cx),
                            )),
                    ),
                ))
                .when(settings_tab == SettingsTab::App, |page| page.child(
                    settings_section(tr("このアプリについて"), "Kick Native 0.1.0")
                        .child(
                            div()
                                .text_size(px(12.0))
                                .text_color(rgb(MUTED))
                                .child(tr("ネイティブ macOS クライアント")),
                        ),
                )),
        )
}

/// Developer check: prints only the number of enabled rewards for a channel.
/// The session cookie is loaded from the app's Keychain item and never printed.
fn rewards_check_slug() -> Option<String> {
    let mut arguments = std::env::args().skip(1);
    if arguments.next().as_deref() != Some("--check-rewards") {
        return None;
    }
    arguments.next().filter(|slug| !slug.is_empty())
}

fn web_reward_probe_args() -> Option<(String, u64)> {
    let mut arguments = std::env::args().skip(1);
    if arguments.next().as_deref() != Some("--reward-probe-web") {
        return None;
    }
    let slug = arguments.next().filter(|slug| !slug.is_empty())?;
    let seconds = arguments
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| (1..=300).contains(seconds))
        .unwrap_or(60);
    Some((slug, seconds))
}

fn run_web_reward_probe(slug: &str, seconds: u64) {
    let Some(input) = credentials::load_cookie().ok().flatten() else {
        println!("error=no_session");
        return;
    };
    let cookie = match input.with_bytes(SessionCookie::parse_bytes) {
        Ok(cookie) => cookie,
        Err(error) => {
            println!("error={error}");
            return;
        }
    };
    let Ok(mut client) = KickClient::unauthenticated_for_probe() else {
        println!("error=client");
        return;
    };
    client.set_session_cookie(cookie);
    let Ok(runtime) = RuntimeBuilder::new_current_thread().enable_all().build() else {
        println!("error=runtime");
        return;
    };
    let channel = match runtime.block_on(client.anonymous_channel(slug)) {
        Ok(channel) => channel,
        Err(error) => {
            println!("error=channel:{error}");
            return;
        }
    };
    let Some(chatroom_id) = channel.chatroom_id else {
        println!("error=no_chatroom");
        return;
    };
    let client_id = format!("kick-native-web-{}", std::process::id());
    match runtime.block_on(client.authenticated_web_reward_probe(&client_id, chatroom_id, seconds))
    {
        Ok(stats) => {
            println!("subscribed={}", stats.subscribed);
            println!("publications={}", stats.publications);
            println!("reward_events={}", stats.reward_events);
            println!("with_reward_title={}", stats.with_reward_title);
            println!("with_username={}", stats.with_username);
            println!("with_user_input={}", stats.with_user_input);
            println!("with_background_color={}", stats.with_background_color);
            println!("with_id={}", stats.with_id);
            println!("with_created_at={}", stats.with_created_at);
            println!("event_names={}", stats.event_names.join(","));
        }
        Err(error) => println!("error={error}"),
    }
}

fn run_rewards_check(slug: &str) {
    let Some(input) = credentials::load_cookie().ok().flatten() else {
        println!("error=no_session");
        return;
    };
    let cookie = match input.with_bytes(SessionCookie::parse_bytes) {
        Ok(cookie) => cookie,
        Err(error) => {
            println!("error={error}");
            return;
        }
    };
    let Ok(mut client) = KickClient::unauthenticated_for_probe() else {
        println!("error=client");
        return;
    };
    client.set_session_cookie(cookie);
    let Ok(runtime) = RuntimeBuilder::new_current_thread().enable_all().build() else {
        println!("error=runtime");
        return;
    };
    match runtime.block_on(client.channel_point_rewards(slug)) {
        Ok(rewards) => println!("rewards={}", rewards.len()),
        Err(error) => println!("error={error}"),
    }
}

fn main_column(
    player_host: Rc<RefCell<PlayerHost>>,
    narrow: bool,
    chat_visible: bool,
    narrow_chat_visible: bool,
    theater_mode: bool,
    chat_state: ListState,
    streams: Vec<StreamSummary>,
    catalog_state: CatalogState,
    channel_state: ChannelLookupState,
    account_state: AccountState,
    selected_nav: &'static str,
    selected_channel_slug: String,
    selected_channel_details: SelectedChannelDetails,
    channel_page_visible: bool,
    channel_page_state: ChannelPageState,
    clips_loading_more: bool,
    clips_error: Option<&str>,
    channel_page_tab: ChannelPageTab,
    recent_streams: Vec<String>,
    search_query: String,
    search_state: &SearchState,
    search_focus: FocusHandle,
    feed_visible: bool,
    chat_messages: Arc<[ChatMessage]>,
    emote_ids_by_name: Arc<HashMap<String, u64>>,
    emote_order: Arc<Vec<(String, u64)>>,
    emote_image_cache: Entity<BoundedEmoteImageCache>,
    chat_history_state: ChatHistoryState,
    chat_unread_messages: usize,
    narrow_chat_height: f32,
    chat_input_notice: Option<String>,
    chat_send_pending: bool,
    chat_send_confirmation: bool,
    emote_picker_open: bool,
    following_state: FollowingState,
    following_tab: FollowingTab,
    followed_channels: Vec<FollowedChannel>,
    following_has_more: bool,
    following_loading_more: bool,
    follow_action_pending: bool,
    follow_error: Option<String>,
    notify_enabled: bool,
    window: &Window,
    cx: &mut Context<KickShell>,
) -> Div {
    let chat_visible_for_layout =
        chat_visibility_for_layout(narrow, chat_visible, narrow_chat_visible);

    let browser = stream_browser(
        &streams,
        catalog_state,
        selected_nav,
        &selected_channel_slug,
        &recent_streams,
        &account_state,
        &search_query,
        search_state,
        &search_focus,
        cx,
    );
    let selected = streams
        .iter()
        .find(|stream| stream.channel_slug == selected_channel_slug)
        .cloned();
    let selected_is_followed = followed_channels
        .iter()
        .any(|channel| channel.channel_slug == selected_channel_slug);
    let showing_following = selected_nav == "フォロー中" && !theater_mode;
    let showing_browser = !showing_following
        && (feed_visible || selected_nav == "ブラウズ" || selected_nav == "イベント");
    if showing_following || showing_browser || channel_page_visible {
        player_host.borrow_mut().suspend_view();
    } else {
        player_host.borrow_mut().resume_view();
    }

    let chat_toggle_action = chat_toggle_action(narrow, chat_visible_for_layout);
    let viewer = if channel_page_visible {
        match selected.as_ref() {
            Some(stream) => channel_page(
                stream,
                &selected_channel_details,
                selected_is_followed,
                follow_action_pending,
                follow_error.as_deref(),
                &channel_page_state,
                clips_loading_more,
                clips_error,
                channel_page_tab,
                cx,
            ),
            None => viewer_column(
                None,
                channel_state,
                &selected_channel_details,
                selected_is_followed,
                follow_action_pending,
                follow_error.as_deref(),
                chat_toggle_action,
                notify_enabled,
                player_host,
                cx,
            ),
        }
    } else {
        viewer_column(
            selected,
            channel_state,
            &selected_channel_details,
            selected_is_followed,
            follow_action_pending,
            follow_error.as_deref(),
            chat_toggle_action,
            notify_enabled,
            player_host,
            cx,
        )
    };

    let mut content = div()
        .id("main-content")
        .flex_1()
        .min_h(px(0.0))
        .flex()
        .flex_col();
    if showing_following {
        content = content.child(following_page(
            following_state,
            following_tab,
            &followed_channels,
            following_has_more,
            following_loading_more,
            &account_state,
            &selected_channel_slug,
            emote_image_cache.clone(),
            cx,
        ));
    } else if showing_browser {
        if selected_nav == "イベント" {
            content = content.child(
                div()
                    .flex_1()
                    .p_4()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(20.0))
                            .text_color(rgb(TEXT))
                            .child(tr("イベント")),
                    )
                    .child(
                        div()
                            .text_size(px(13.0))
                            .text_color(rgb(MUTED))
                            .child(tr("イベント一覧は未対応です")),
                    ),
            );
        } else {
            if selected_nav == "ホーム" && search_query.trim().is_empty() {
                content = content.child(home_feed(
                    &streams,
                    catalog_state,
                    &followed_channels,
                    &account_state,
                    &selected_channel_slug,
                    cx,
                ));
            } else {
                content = content.child(browser);
            }
        }
    } else {
        content = content.child(viewer);
    }

    if narrow && chat_visible_for_layout {
        content = content.child(inline_chat_panel(
            chat_state,
            chat_messages,
            emote_ids_by_name,
            emote_order,
            emote_image_cache,
            chat_history_state,
            chat_unread_messages,
            chat_input_notice,
            narrow_chat_height,
            chat_send_pending,
            chat_send_confirmation,
            emote_picker_open,
            window,
            cx,
        ));
    }

    div()
        .flex_1()
        .min_w(px(0.0))
        .min_h(px(0.0))
        .h_full()
        .flex()
        .flex_col()
        .tab_group()
        .child(content)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HeaderAction {
    ToggleFullscreen,
    ToggleChat,
    ToggleNarrowChat,
    ToggleTheater,
    RetryCatalog,
    RetryChannel,
    RetryHistory,
    RetrySearch,
}

fn chat_toggle_action(narrow: bool, chat_visible: bool) -> Option<HeaderAction> {
    if chat_visible {
        None
    } else if narrow {
        Some(HeaderAction::ToggleNarrowChat)
    } else {
        Some(HeaderAction::ToggleChat)
    }
}

fn header_action_button(
    id: &'static str,
    label: &'static str,
    expanded: Option<bool>,
    action: HeaderAction,
    cx: &mut Context<KickShell>,
) -> Stateful<Div> {
    let mut button = div()
        .id(id)
        .role(Role::Button)
        .aria_label(label)
        .tab_index(0)
        .rounded_md()
        .px_2()
        .py_1()
        .text_size(px(12.0))
        .text_color(rgb(SUBTLE))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(RAISED)))
        .focus_visible(|style| style.bg(rgb(RAISED)));
    if let Some(expanded) = expanded {
        button = button.aria_expanded(expanded);
    }
    button
        .on_key_down(cx.listener(move |this, event, window, cx| {
            if is_activation_key(event) {
                apply_header_action(this, action, window, cx);
                cx.notify();
            }
        }))
        .on_click(cx.listener(move |this, _event, window, cx| {
            apply_header_action(this, action, window, cx);
            cx.notify();
        }))
        .child(label)
}

fn chat_collapse_button(
    id: &'static str,
    label: &'static str,
    action: HeaderAction,
    cx: &mut Context<KickShell>,
) -> Stateful<Div> {
    div()
        .id(id)
        .role(Role::Button)
        .aria_label(label)
        .tab_index(0)
        .w(px(32.0))
        .h(px(32.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .text_size(px(22.0))
        .text_color(rgb(SUBTLE))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(RAISED)))
        .focus_visible(|style| style.bg(rgb(RAISED)))
        .on_click(cx.listener(move |this, _event, window, cx| {
            apply_header_action(this, action, window, cx);
            cx.notify();
        }))
        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
            if is_activation_key(event) {
                apply_header_action(this, action, window, cx);
                cx.notify();
            }
        }))
        .child("‹")
}

fn apply_header_action(
    this: &mut KickShell,
    action: HeaderAction,
    _window: &mut Window,
    cx: &mut Context<KickShell>,
) {
    match action {
        HeaderAction::ToggleFullscreen => this.toggle_video_fullscreen(cx),
        HeaderAction::ToggleChat => this.chat_visible = !this.chat_visible,
        HeaderAction::ToggleNarrowChat => {
            this.narrow_chat_visible = !this.narrow_chat_visible;
        }
        HeaderAction::ToggleTheater => this.set_theater_mode(!this.theater_mode),
        HeaderAction::RetryCatalog => this.retry_catalog(cx),
        HeaderAction::RetryChannel => this.retry_selected_channel(cx),
        HeaderAction::RetryHistory => this.retry_chat_history(cx),
        HeaderAction::RetrySearch => this.start_search(cx),
    }
}

#[derive(Clone, Copy)]
enum PlayerAction {
    TogglePlayback,
    ToggleMute,
    CycleSubtitles,
    TogglePictureInPicture,
}

fn apply_player_action(this: &mut KickShell, action: PlayerAction, cx: &mut Context<KickShell>) {
    match action {
        PlayerAction::TogglePlayback => this.toggle_playback(cx),
        PlayerAction::ToggleMute => this.toggle_mute(),
        PlayerAction::CycleSubtitles => {
            this.player_host.borrow_mut().cycle_subtitle();
            cx.notify();
        }
        PlayerAction::TogglePictureInPicture => {
            this.player_host.borrow_mut().toggle_picture_in_picture();
            cx.notify();
        }
    }
}

fn home_feed(
    streams: &[StreamSummary],
    catalog_state: CatalogState,
    followed_channels: &[FollowedChannel],
    account_state: &AccountState,
    selected_channel_slug: &str,
    cx: &mut Context<KickShell>,
) -> Stateful<Div> {
    let mut page = div()
        .id("home-feed")
        .flex_1()
        .min_h(px(0.0))
        .w_full()
        .overflow_y_scroll()
        .p_4()
        .flex()
        .flex_col()
        .gap_5();

    if streams.is_empty() {
        let message = match catalog_state {
            CatalogState::Loading => tr("おすすめ配信を取得しています…"),
            CatalogState::Failed(error) => {
                return page.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(
                            div()
                                .text_size(px(14.0))
                                .text_color(rgb(TEXT))
                                .child(tr("ホーム")),
                        )
                        .child(div().text_size(px(12.0)).text_color(rgb(MUTED)).child(t!(
                            "おすすめ配信を取得できません: {}",
                            "Could not load recommended streams: {}",
                            error.user_message()
                        ))),
                );
            }
            CatalogState::Ready => tr("現在おすすめできる配信がありません"),
        };
        return page.child(
            div()
                .text_size(px(13.0))
                .text_color(rgb(MUTED))
                .child(message),
        );
    }

    let hero = streams
        .iter()
        .max_by_key(|stream| stream.viewer_count.unwrap_or(0))
        .or_else(|| streams.first());
    if let Some(hero) = hero {
        page = page.child(home_hero_card(hero, cx));
    }

    page = page.child(home_section_header(tr("あなたへの配信")));
    page = page.child(home_stream_row(streams, selected_channel_slug, cx));

    let categories = home_category_summary(streams);
    if !categories.is_empty() {
        page = page.child(home_section_header(tr("あなたへのカテゴリー")));
        page = page.child(home_category_row(&categories));
    }

    if !followed_channels.is_empty() {
        page = page.child(home_section_header(tr("フォロー中のチャンネル")));
        page = page.child(home_followed_row(followed_channels, cx));
    } else if matches!(account_state, AccountState::SignedIn { .. }) {
        page = page.child(home_section_header(tr("フォロー中のチャンネル")));
        page = page.child(
            div()
                .text_size(px(12.0))
                .text_color(rgb(MUTED))
                .child(tr("フォロー中のチャンネルはまだありません")),
        );
    }

    page
}

fn home_section_header(title: &str) -> Div {
    div()
        .w_full()
        .flex()
        .items_center()
        .justify_between()
        .child(
            div()
                .text_size(px(18.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(rgb(TEXT))
                .child(title.to_owned()),
        )
}

fn home_hero_card(stream: &StreamSummary, cx: &mut Context<KickShell>) -> Stateful<Div> {
    let title = if stream.title.trim().is_empty() {
        stream.channel_name.clone()
    } else {
        stream.title.clone()
    };
    let category = stream
        .category
        .clone()
        .unwrap_or_else(|| tr("カテゴリなし").to_owned());
    let viewers = stream
        .viewer_count
        .map(|viewers| t!("{viewers} 視聴中", "{viewers} watching"))
        .unwrap_or_else(|| tr("視聴者数未提供").to_owned());
    let stream_for_click = stream.clone();
    let mut card = div()
        .id("home-hero")
        .relative()
        .w_full()
        .h(px(340.0))
        .flex_shrink_0()
        .rounded(px(14.0))
        .overflow_hidden()
        .bg(rgb(RAISED))
        .cursor_pointer()
        .on_click(cx.listener(move |this, _event, _window, cx| {
            this.select_summary(stream_for_click.clone(), cx);
            cx.notify();
        }));
    if let Some(url) = &stream.thumbnail_url {
        card = card.child(
            img(url.clone())
                .size_full()
                .object_fit(gpui::ObjectFit::Cover),
        );
    }
    let overlay = div()
        .absolute()
        .left_0()
        .right_0()
        .bottom_0()
        .p_4()
        .flex()
        .flex_col()
        .gap_2()
        .bg(rgba(0x000000CC))
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .bg(rgb(ACCENT))
                        .text_size(px(10.0))
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(rgb(0x0B0B0F))
                        .child("LIVE"),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(rgb(TEXT))
                        .child(viewers),
                ),
        )
        .child(
            div()
                .max_w(px(760.0))
                .text_size(px(24.0))
                .line_height(px(30.0))
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(rgb(TEXT))
                .child(title),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(avatar_placeholder(&stream.channel_name, 28.0))
                .child(
                    div()
                        .text_size(px(13.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(rgb(TEXT))
                        .child(stream.channel_name.clone()),
                )
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(rgb(SUBTLE))
                        .child(format!("· {category}")),
                ),
        );
    card.child(overlay)
}

fn home_stream_row(
    streams: &[StreamSummary],
    selected_channel_slug: &str,
    cx: &mut Context<KickShell>,
) -> Stateful<Div> {
    let mut row = div()
        .id("home-stream-row")
        .w_full()
        .flex()
        .gap_3()
        .overflow_x_scroll()
        .pb_2();
    for stream in streams.iter().take(12) {
        row = row.child(home_stream_card(
            stream,
            stream.channel_slug == selected_channel_slug,
            cx,
        ));
    }
    row
}

fn home_stream_card(
    stream: &StreamSummary,
    selected: bool,
    cx: &mut Context<KickShell>,
) -> Stateful<Div> {
    let title = if stream.title.trim().is_empty() {
        stream.channel_name.clone()
    } else {
        stream.title.clone()
    };
    let category = stream
        .category
        .clone()
        .unwrap_or_else(|| tr("カテゴリなし").to_owned());
    let viewers = stream
        .viewer_count
        .map(|viewers| t!("{viewers} 視聴中", "{viewers} watching"))
        .unwrap_or_else(|| tr("視聴者数未提供").to_owned());
    let stream_for_click = stream.clone();
    let mut thumbnail = div()
        .relative()
        .w_full()
        .aspect_ratio(16.0 / 9.0)
        .rounded(px(10.0))
        .overflow_hidden()
        .bg(rgb(RAISED));
    if let Some(url) = &stream.thumbnail_url {
        thumbnail = thumbnail.child(
            img(url.clone())
                .size_full()
                .object_fit(gpui::ObjectFit::Cover),
        );
    }
    thumbnail = thumbnail
        .child(
            div()
                .absolute()
                .left(px(8.0))
                .top(px(8.0))
                .px_2()
                .py_1()
                .rounded_md()
                .bg(rgb(ACCENT))
                .text_size(px(9.0))
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(rgb(0x0B0B0F))
                .child("LIVE"),
        )
        .child(
            div()
                .absolute()
                .right(px(8.0))
                .bottom(px(8.0))
                .px_2()
                .py_1()
                .rounded_md()
                .bg(rgba(0x000000CC))
                .text_size(px(9.0))
                .text_color(rgb(TEXT))
                .child(viewers),
        );

    let card = div()
        .id(format!("home-stream-{}", stream.channel_slug))
        .w(px(268.0))
        .flex_shrink_0()
        .flex()
        .flex_col()
        .gap_2()
        .cursor_pointer()
        .rounded(px(10.0))
        .p_1()
        .when(selected, |card| card.bg(rgb(SELECTED_ROW)))
        .hover(|style| style.bg(rgb(RAISED)))
        .on_click(cx.listener(move |this, _event, _window, cx| {
            this.select_summary(stream_for_click.clone(), cx);
            cx.notify();
        }))
        .child(thumbnail)
        .child(
            div()
                .flex()
                .items_start()
                .gap_2()
                .child(if let Some(url) = &stream.profile_picture_url {
                    img(url.clone())
                        .w(px(32.0))
                        .h(px(32.0))
                        .flex_shrink_0()
                        .rounded_full()
                        .overflow_hidden()
                        .into_any()
                } else {
                    avatar_placeholder(&stream.channel_name, 32.0).into_any()
                })
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_size(px(13.0))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(rgb(TEXT))
                                .truncate()
                                .child(title),
                        )
                        .child(
                            div()
                                .text_size(px(12.0))
                                .text_color(rgb(SUBTLE))
                                .truncate()
                                .child(stream.channel_name.clone()),
                        )
                        .child(
                            div()
                                .text_size(px(11.0))
                                .text_color(rgb(MUTED))
                                .truncate()
                                .child(category),
                        ),
                ),
        );
    card
}

struct HomeCategory {
    name: String,
    viewers: u64,
    stream_count: usize,
}

fn home_category_summary(streams: &[StreamSummary]) -> Vec<HomeCategory> {
    let mut categories: Vec<HomeCategory> = Vec::new();
    for stream in streams {
        let Some(name) = stream.category.as_deref().filter(|name| !name.is_empty()) else {
            continue;
        };
        if let Some(existing) = categories.iter_mut().find(|category| category.name == name) {
            existing.viewers = existing
                .viewers
                .saturating_add(stream.viewer_count.unwrap_or(0));
            existing.stream_count += 1;
        } else {
            categories.push(HomeCategory {
                name: name.to_owned(),
                viewers: stream.viewer_count.unwrap_or(0),
                stream_count: 1,
            });
        }
    }
    categories.sort_by(|a, b| b.viewers.cmp(&a.viewers));
    categories.truncate(10);
    categories
}

fn home_category_row(categories: &[HomeCategory]) -> Stateful<Div> {
    let mut row = div()
        .id("home-category-row")
        .w_full()
        .flex()
        .gap_3()
        .overflow_x_scroll()
        .pb_2();
    for category in categories {
        row = row.child(
            div()
                .id(format!("home-category-{}", category.name))
                .w(px(190.0))
                .h(px(104.0))
                .flex_shrink_0()
                .flex()
                .flex_col()
                .justify_end()
                .gap_1()
                .p_3()
                .rounded(px(10.0))
                .bg(rgb(RAISED))
                .child(
                    div()
                        .text_size(px(22.0))
                        .child(category_icon(&category.name).to_owned()),
                )
                .child(
                    div()
                        .text_size(px(14.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(rgb(TEXT))
                        .truncate()
                        .child(category.name.clone()),
                )
                .child(div().text_size(px(11.0)).text_color(rgb(MUTED)).child(t!(
                    "{} 視聴中 · {} 配信",
                    "{} watching · {} live",
                    category.viewers,
                    category.stream_count
                ))),
        );
    }
    row
}

fn category_icon(name: &str) -> &'static str {
    match name {
        "Games" => "🎮",
        "IRL" => "🎙️",
        "Music" => "🎵",
        "Gambling" => "🎰",
        "Creative" => "🎨",
        "Esports" => "🏆",
        "Just Chatting" => "💬",
        "Science & Technology" => "🔬",
        "Sports" => "⚽",
        "Food & Drink" => "🍔",
        _ => "▦",
    }
}

fn home_followed_row(channels: &[FollowedChannel], cx: &mut Context<KickShell>) -> Stateful<Div> {
    let mut row = div()
        .id("home-followed-row")
        .w_full()
        .flex()
        .gap_3()
        .overflow_x_scroll()
        .pb_2();
    for channel in channels.iter().take(12) {
        let summary = channel.to_summary();
        let summary_for_click = summary.clone();
        row = row.child(
            div()
                .id(format!("home-followed-{}", channel.channel_slug))
                .w(px(190.0))
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap_2()
                .p_2()
                .rounded(px(10.0))
                .bg(rgb(RAISED))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(SELECTED_ROW)))
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.select_summary(summary_for_click.clone(), cx);
                    cx.notify();
                }))
                .child(if let Some(url) = &channel.profile_picture_url {
                    cached_circular_avatar(url, 40.0).into_any()
                } else {
                    avatar_placeholder(&channel.channel_name, 40.0).into_any()
                })
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_size(px(12.0))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(rgb(TEXT))
                                .truncate()
                                .child(channel.channel_name.clone()),
                        )
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(rgb(if channel.is_live { ACCENT } else { MUTED }))
                                .child(if channel.is_live {
                                    tr("● ライブ中")
                                } else {
                                    tr("○ オフライン")
                                }),
                        ),
                ),
        );
    }
    row
}

fn stream_browser(
    streams: &[StreamSummary],
    catalog_state: CatalogState,
    selected_nav: &'static str,
    selected_channel_slug: &str,
    recent_streams: &[String],
    account_state: &AccountState,
    search_query: &str,
    search_state: &SearchState,
    search_focus: &FocusHandle,
    cx: &mut Context<KickShell>,
) -> Stateful<Div> {
    let query = search_query.trim().to_lowercase();
    let search_active = !query.is_empty();
    let (filtered, empty_message) = if search_active {
        match search_state {
            SearchState::Idle => (Vec::new(), tr("Enterで検索を実行してください").to_owned()),
            SearchState::Loading => (Vec::new(), tr("検索中…").to_owned()),
            SearchState::Ready(results) => {
                let results = results.clone();
                let message = if results.is_empty() {
                    tr("検索に一致する配信はありません").to_owned()
                } else {
                    String::new()
                };
                (results, message)
            }
            SearchState::Failed(error) => (
                Vec::new(),
                t!("検索できません: {error}", "Search failed: {error}"),
            ),
        }
    } else {
        let mut filtered: Vec<_> = streams
            .iter()
            .filter(|stream| {
                query.is_empty()
                    || stream.channel_name.to_lowercase().contains(&query)
                    || stream.title.to_lowercase().contains(&query)
                    || stream
                        .category
                        .as_deref()
                        .is_some_and(|category| category.to_lowercase().contains(&query))
            })
            .cloned()
            .collect();
        filtered.retain(|stream| match selected_nav {
            "フォロー中" => false,
            "雑談" => stream.category.as_deref() == Some("Just Chatting"),
            "ゲーム" => stream.category.as_deref() == Some("Games"),
            "音楽" => stream.category.as_deref() == Some("Music"),
            "科学・技術" => stream.category.as_deref() == Some("Science & Technology"),
            "履歴" => recent_streams.contains(&stream.channel_slug),
            _ => true,
        });
        let message = if filtered.is_empty() {
            match catalog_state {
                CatalogState::Loading => tr("匿名カタログを取得しています…").to_owned(),
                CatalogState::Failed(error) => {
                    t!(
                        "匿名カタログを取得できません: {}",
                        "Could not load the anonymous catalog: {}",
                        error.user_message()
                    )
                }
                CatalogState::Ready if selected_nav == "フォロー中" => {
                    following_unavailable_message(account_state)
                }
                CatalogState::Ready => tr("匿名カタログに配信がありません").to_owned(),
            }
        } else {
            String::new()
        };
        (filtered, message)
    };
    let filtered_count = filtered.len();

    let mut rows = div().flex().flex_col();
    for stream in filtered {
        let selected = stream.channel_slug == selected_channel_slug;
        rows = rows.child(stream_row(stream, selected, cx));
    }
    if filtered_count == 0 {
        rows = rows.child({
            let mut empty = div()
                .px_3()
                .py_3()
                .flex()
                .flex_col()
                .gap_2()
                .text_size(px(12.0))
                .text_color(rgb(MUTED))
                .child(empty_message);
            if search_active && matches!(search_state, SearchState::Failed(_)) {
                empty = empty.child(header_action_button(
                    "search-retry-browser",
                    tr("再試行"),
                    None,
                    HeaderAction::RetrySearch,
                    cx,
                ));
            } else if !search_active && catalog_retry_available(catalog_state) {
                empty = empty.child(header_action_button(
                    "catalog-retry-browser",
                    tr("再試行"),
                    None,
                    HeaderAction::RetryCatalog,
                    cx,
                ));
            }
            empty
        });
    }

    let result_list = div()
        .id("stream-results")
        .flex_1()
        .min_h(px(0.0))
        .overflow_y_scroll()
        .child(rows);

    let browser = div()
        .id("stream-browser")
        .flex_1()
        .min_h(px(0.0))
        .w_full()
        .h_full()
        .flex()
        .flex_col()
        .overflow_hidden()
        .bg(rgb(APP_BG))
        .child(
            div()
                .px_3()
                .pt_3()
                .pb_2()
                .flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_size(px(14.0))
                        .text_color(rgb(TEXT))
                        .child(tr("配信を探す")),
                )
                .child(div().text_size(px(11.0)).text_color(rgb(MUTED)).child(t!(
                    "{} · {filtered_count}件",
                    "{} · {filtered_count}",
                    catalog_state.label()
                ))),
        )
        .child(search_field(search_query, search_focus, cx))
        .child(result_list);

    browser
}

fn search_field(
    search_query: &str,
    search_focus: &FocusHandle,
    cx: &mut Context<KickShell>,
) -> Stateful<Div> {
    let value = if search_query.is_empty() {
        tr("配信名、タイトル、カテゴリを検索")
    } else {
        search_query
    };
    let color = if search_query.is_empty() { MUTED } else { TEXT };
    div()
        .id("local-stream-search")
        .track_focus(search_focus)
        .tab_index(0)
        .aria_label(tr("匿名カタログの配信を検索"))
        .h(px(36.0))
        .min_h(px(36.0))
        .mx_3()
        .my_2()
        .px_3()
        .flex()
        .items_center()
        .justify_between()
        .rounded_full()
        .bg(rgb(RAISED))
        .text_size(px(13.0))
        .text_color(rgb(color))
        .cursor_text()
        .focus_visible(|style| style.bg(rgb(SELECTED_ROW)))
        .on_click(cx.listener(|this, _event, window, cx| {
            this.search_focus.focus(window, cx);
        }))
        .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
            if event.is_held {
                return;
            }
            let key = event.keystroke.key.as_str();
            if key == "enter" {
                this.start_search(cx);
                return;
            }
            if key == "backspace" {
                this.search_query.pop();
                this.search_state = SearchState::Idle;
                this.search_generation = this.search_generation.wrapping_add(1);
                cx.notify();
            } else if !event.keystroke.modifiers.modified()
                && let Some(character) = event.keystroke.key_char.as_deref()
                && !character.chars().any(char::is_control)
            {
                this.search_query.push_str(character);
                this.search_state = SearchState::Idle;
                this.search_generation = this.search_generation.wrapping_add(1);
                cx.notify();
            }
        }))
        .child(value.to_owned())
        .when(!search_query.is_empty(), |field| {
            field.child(
                div()
                    .id("clear-stream-search")
                    .role(Role::Button)
                    .aria_label(tr("検索をクリア"))
                    .tab_index(0)
                    .px_1()
                    .text_color(rgb(SUBTLE))
                    .cursor_pointer()
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                        if is_activation_key(event) {
                            this.search_query.clear();
                            this.start_search(cx);
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.search_query.clear();
                        this.start_search(cx);
                        cx.notify();
                    }))
                    .child("×"),
            )
        })
}

fn stream_row(stream: StreamSummary, selected: bool, cx: &mut Context<KickShell>) -> Stateful<Div> {
    let viewers = stream
        .viewer_count
        .map(|viewers| t!("{viewers}人", "{viewers}"))
        .unwrap_or_else(|| tr("視聴者数未提供").to_owned());
    let category = stream.category.as_deref().unwrap_or(tr("カテゴリなし"));
    let title = if stream.title.trim().is_empty() {
        stream.channel_name.clone()
    } else {
        stream.title.clone()
    };
    let metadata = if stream.title.trim().is_empty() {
        format!("{category} · {viewers}")
    } else {
        format!("{} · {category} · {viewers}", stream.channel_name)
    };
    let accessibility_label = t!(
        "{}、{}、カテゴリ {}、{}、匿名カタログから取得した実データ",
        "{}、{}、category {}、{}、real data from the anonymous catalog",
        stream.channel_name,
        stream.title,
        category,
        viewers
    );
    let avatar = if let Some(url) = &stream.profile_picture_url {
        cached_circular_avatar(url, 36.0).into_any()
    } else {
        avatar_placeholder(&stream.channel_name, 36.0).into_any()
    };
    let stream_for_click = stream.clone();
    let stream_for_key = stream.clone();
    let mut row = div()
        .id(format!("stream-row-{}", stream.channel_slug))
        .role(Role::Button)
        .aria_label(accessibility_label)
        .aria_selected(selected)
        .tab_index(0)
        .w_full()
        .h(px(76.0))
        .min_h(px(76.0))
        .flex_shrink_0()
        .px_3()
        .py_2()
        .flex()
        .items_center()
        .gap_3()
        .rounded_md()
        .text_size(px(12.0))
        .cursor_pointer()
        .on_click(cx.listener(move |this, _event, _window, cx| {
            this.select_summary(stream_for_click.clone(), cx);
            this.set_feed_visible(false);
            cx.notify();
        }))
        .on_key_down(cx.listener(move |this, event, _window, cx| {
            if is_activation_key(event) {
                this.select_summary(stream_for_key.clone(), cx);
                this.set_feed_visible(false);
                cx.notify();
            }
        }))
        .child(avatar)
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_size(px(13.0))
                        .text_color(rgb(TEXT))
                        .truncate()
                        .child(title),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(rgb(SUBTLE))
                        .truncate()
                        .child(metadata),
                ),
        );

    row = if selected {
        row.bg(rgb(SELECTED_ROW))
    } else {
        row.hover(|style| style.bg(rgb(RAISED)))
    };
    row.focus_visible(|style| style.bg(rgb(RAISED)))
}

fn channel_page(
    stream: &StreamSummary,
    details: &SelectedChannelDetails,
    is_followed: bool,
    follow_pending: bool,
    follow_error: Option<&str>,
    page_state: &ChannelPageState,
    clips_loading_more: bool,
    clips_error: Option<&str>,
    page_tab: ChannelPageTab,
    cx: &mut Context<KickShell>,
) -> Stateful<Div> {
    let channel_name = details
        .username
        .clone()
        .unwrap_or_else(|| stream.channel_name.clone());
    let followers = details
        .followers_count
        .clone()
        .unwrap_or_else(|| tr("未取得").to_owned());
    let avatar = if let Some(path) = &details.avatar_path {
        circular_avatar_path(path.clone(), 96.0).into_any()
    } else if let Some(url) = &details.profile_picture_url {
        circular_avatar(url.clone(), 96.0).into_any()
    } else {
        avatar_placeholder(&channel_name, 96.0).into_any()
    };
    let mut page = div()
        .id("channel-page")
        .flex_1()
        .min_w(px(0.0))
        .flex()
        .flex_col()
        .gap_4()
        .p_4()
        .overflow_y_scroll()
        .child(
            div()
                .id("channel-page-back")
                .role(Role::Button)
                .aria_label(tr("配信画面に戻る"))
                .tab_index(0)
                .px_2()
                .py_1()
                .rounded_md()
                .text_size(px(12.0))
                .text_color(rgb(SUBTLE))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(RAISED)))
                .focus_visible(|style| style.bg(rgb(RAISED)))
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.close_channel_page(cx);
                }))
                .child(tr("← 配信に戻る")),
        );

    if let Some(url) = details
        .banner_url
        .clone()
        .or_else(|| details.offline_banner_url.clone())
    {
        page = page.child(
            div()
                .w_full()
                .h(px(180.0))
                .rounded(px(12.0))
                .overflow_hidden()
                .bg(rgb(RAISED))
                .child(
                    img(url)
                        .w_full()
                        .h_full()
                        .object_fit(gpui::ObjectFit::Cover),
                ),
        );
    }

    page = page.child(
        div()
            .w_full()
            .flex()
            .items_end()
            .gap_3()
            .child(avatar)
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(26.0))
                                    .font_weight(gpui::FontWeight::BOLD)
                                    .text_color(rgb(TEXT))
                                    .truncate()
                                    .child(channel_name),
                            )
                            .when(details.verified, |row| {
                                row.child(
                                    div().text_size(px(14.0)).text_color(rgb(ACCENT)).child("✓"),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_size(px(12.0))
                            .text_color(rgb(MUTED))
                            .child(t!("フォロワー {followers}", "{followers} followers"))
                            .child(
                                div()
                                    .text_color(rgb(if details.is_live { ACCENT } else { MUTED }))
                                    .child(if details.is_live {
                                        tr("● ライブ中")
                                    } else {
                                        tr("○ オフライン")
                                    }),
                            ),
                    ),
            )
            .child(follow_button(is_followed, follow_pending, cx)),
    );

    if let Some(bio) = details.bio.clone().filter(|bio| !bio.trim().is_empty()) {
        let first_line = bio.lines().next().unwrap_or_default().trim().to_owned();
        if !first_line.is_empty() {
            page = page.child(
                div()
                    .text_size(px(13.0))
                    .text_color(rgb(SUBTLE))
                    .child(first_line),
            );
        }
    }

    page = page.child(channel_page_tabs(page_tab, cx));

    match page_tab {
        ChannelPageTab::Home => {
            page = page.child(channel_page_home_sections(page_state, cx));
        }
        ChannelPageTab::Videos => {
            page = page.child(channel_page_videos_section(page_state, cx));
        }
        ChannelPageTab::Clips => {
            page = page.child(channel_page_clips_section(
                page_state,
                clips_loading_more,
                clips_error,
                cx,
            ));
        }
        ChannelPageTab::About => {
            if let Some(bio) = details.bio.clone().filter(|bio| !bio.trim().is_empty()) {
                page = page.child(
                    div()
                        .w_full()
                        .p_4()
                        .rounded(px(12.0))
                        .bg(rgb(RAISED))
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(
                            div()
                                .text_size(px(13.0))
                                .text_color(rgb(TEXT))
                                .child(tr("自己紹介")),
                        )
                        .child(div().text_size(px(12.0)).text_color(rgb(SUBTLE)).child(bio)),
                );
            }

            let socials = [
                ("X", details.twitter.clone()),
                ("Instagram", details.instagram.clone()),
                ("YouTube", details.youtube.clone()),
                ("Discord", details.discord.clone()),
                ("TikTok", details.tiktok.clone()),
                ("Facebook", details.facebook.clone()),
                ("Club", details.club.clone()),
            ];
            let mut social_rows = div().flex().flex_col().gap_1();
            let mut has_social = false;
            for (label, value) in socials {
                if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
                    has_social = true;
                    social_rows = social_rows.child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .text_size(px(12.0))
                            .text_color(rgb(SUBTLE))
                            .child(label)
                            .child(div().text_color(rgb(TEXT)).child(value)),
                    );
                }
            }
            if has_social {
                page = page.child(
                    div()
                        .w_full()
                        .p_4()
                        .rounded(px(12.0))
                        .bg(rgb(RAISED))
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(
                            div()
                                .text_size(px(13.0))
                                .text_color(rgb(TEXT))
                                .child(tr("リンク")),
                        )
                        .child(social_rows),
                );
            }

            page = page.child(channel_about_cards(
                details,
                ChannelLookupState::Ready {
                    is_live: details.is_live,
                },
            ));
        }
    }

    if let Some(error) = follow_error {
        page = page.child(
            div()
                .text_size(px(11.0))
                .text_color(rgb(DANGER))
                .child(error.to_owned()),
        );
    }
    page
}

fn channel_page_tabs(active: ChannelPageTab, cx: &mut Context<KickShell>) -> Stateful<Div> {
    let mut tabs = div()
        .id("channel-page-tabs")
        .role(Role::TabList)
        .w_full()
        .flex()
        .items_center()
        .gap_1()
        .border_b_2()
        .border_color(rgb(LINE))
        .pb_0();
    for tab in ChannelPageTab::ALL {
        let selected = tab == active;
        tabs = tabs.child(
            div()
                .id(format!("channel-tab-{}", tab.label()))
                .role(Role::Tab)
                .aria_label(tab.label())
                .aria_selected(selected)
                .tab_index(0)
                .min_w(px(64.0))
                .justify_center()
                .px_3()
                .py_2()
                .text_size(px(13.0))
                .font_weight(if selected {
                    gpui::FontWeight::SEMIBOLD
                } else {
                    gpui::FontWeight::NORMAL
                })
                .text_color(rgb(if selected { TEXT } else { MUTED }))
                .cursor_pointer()
                .when(selected, |tab| tab.border_b_2().border_color(rgb(ACCENT)))
                .hover(|style| style.bg(rgb(RAISED)))
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.channel_page_tab = tab;
                    cx.notify();
                }))
                .child(tab.label()),
        );
    }
    tabs
}

fn channel_page_loading() -> Div {
    div()
        .w_full()
        .py_6()
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(12.0))
        .text_color(rgb(MUTED))
        .child(tr("チャンネル情報を取得しています…"))
}

fn channel_page_error(error: &str) -> Div {
    div()
        .w_full()
        .py_6()
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(12.0))
        .text_color(rgb(DANGER))
        .child(t!(
            "チャンネル情報を取得できません: {error}",
            "Could not load channel info: {error}"
        ))
}

fn channel_page_home_sections(state: &ChannelPageState, cx: &mut Context<KickShell>) -> Div {
    let mut sections = div().w_full().flex().flex_col().gap_4();
    match state {
        ChannelPageState::Idle | ChannelPageState::Loading => {
            sections = sections.child(channel_page_loading());
        }
        ChannelPageState::Failed(error) => {
            sections = sections.child(channel_page_error(error));
        }
        ChannelPageState::Ready(data) => {
            if !data.videos.is_empty() {
                sections = sections.child(channel_section_header(tr("過去の配信")));
                sections = sections.child(channel_video_grid(&data.videos, 6, cx));
            }
            if !data.clips.is_empty() {
                sections = sections.child(channel_section_header(tr("クリップ")));
                sections = sections.child(channel_clip_row(&data.clips, 6, cx));
            }
            if !data.categories.is_empty() {
                sections = sections.child(channel_section_header(tr("カテゴリー")));
                sections = sections.child(channel_category_row(&data.categories));
            }
            if data.videos.is_empty() && data.clips.is_empty() && data.categories.is_empty() {
                sections = sections.child(
                    div()
                        .text_size(px(12.0))
                        .text_color(rgb(MUTED))
                        .child(tr("過去の配信やクリップはありません")),
                );
            }
        }
    }
    sections
}

fn channel_page_videos_section(
    state: &ChannelPageState,
    cx: &mut Context<KickShell>,
) -> AnyElement {
    match state {
        ChannelPageState::Idle | ChannelPageState::Loading => channel_page_loading().into_any(),
        ChannelPageState::Failed(error) => channel_page_error(error).into_any(),
        ChannelPageState::Ready(data) if data.videos.is_empty() => div()
            .text_size(px(12.0))
            .text_color(rgb(MUTED))
            .child(tr("過去の配信はありません"))
            .into_any(),
        ChannelPageState::Ready(data) => channel_video_grid(&data.videos, 24, cx).into_any(),
    }
}

/// Appends only clips whose IDs are not already present, capped at `limit`
/// new rows. Kick can repeat a clip across page boundaries, so the merge is
/// ID-based rather than positional.
fn merge_clips(
    existing: &mut Vec<kick_live::ChannelClip>,
    incoming: Vec<kick_live::ChannelClip>,
    limit: usize,
) {
    let mut seen: HashSet<String> = existing.iter().map(|clip| clip.id.clone()).collect();
    let mut added = Vec::new();
    for clip in incoming {
        if seen.insert(clip.id.clone()) {
            added.push(clip);
        }
        if added.len() >= limit {
            break;
        }
    }
    existing.extend(added);
}

fn channel_page_clips_section(
    state: &ChannelPageState,
    loading_more: bool,
    error: Option<&str>,
    cx: &mut Context<KickShell>,
) -> AnyElement {
    let mut section = div().w_full().flex().flex_col().gap_2();
    match state {
        ChannelPageState::Idle | ChannelPageState::Loading => {
            return channel_page_loading().into_any();
        }
        ChannelPageState::Failed(error) => return channel_page_error(error).into_any(),
        ChannelPageState::Ready(data) if data.clips.is_empty() => {
            section = section.child(
                div()
                    .text_size(px(12.0))
                    .text_color(rgb(MUTED))
                    .child(tr("クリップはありません")),
            );
        }
        ChannelPageState::Ready(data) => {
            section = section.child(channel_clip_row(&data.clips, 24, cx));
            if data.clips_cursor.is_some() || loading_more || error.is_some() {
                let mut footer = div().flex().items_center().gap_2();
                if loading_more {
                    footer = footer.child(
                        div()
                            .text_size(px(12.0))
                            .text_color(rgb(MUTED))
                            .child(tr("クリップを読み込み中…")),
                    );
                } else if let Some(error) = error {
                    footer = footer.child(
                        div()
                            .text_size(px(12.0))
                            .text_color(rgb(DANGER))
                            .child(error.to_owned()),
                    );
                }
                if data.clips_cursor.is_some() && !loading_more {
                    footer = footer.child(
                        div()
                            .id("clips-load-more")
                            .role(Role::Button)
                            .aria_label(tr("クリップをさらに読み込む"))
                            .tab_index(0)
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .bg(rgb(RAISED))
                            .text_size(px(12.0))
                            .text_color(rgb(SUBTLE))
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(SELECTED_ROW)))
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.load_more_clips(cx);
                            }))
                            .child(tr("さらに読み込む")),
                    );
                }
                section = section.child(footer);
            }
        }
    }
    section.into_any()
}

fn channel_section_header(title: &str) -> Div {
    div()
        .w_full()
        .flex()
        .items_center()
        .justify_between()
        .child(
            div()
                .text_size(px(17.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(rgb(TEXT))
                .child(title.to_owned()),
        )
}

fn channel_video_grid(
    videos: &[kick_live::ChannelVideo],
    limit: usize,
    cx: &mut Context<KickShell>,
) -> Div {
    let mut grid = div().w_full().flex().flex_wrap().gap_3();
    for video in videos.iter().take(limit) {
        grid = grid.child(channel_video_card(video, cx));
    }
    grid
}

fn channel_video_card(
    video: &kick_live::ChannelVideo,
    cx: &mut Context<KickShell>,
) -> Stateful<Div> {
    let uuid = video.uuid.clone();
    let title = video.title.clone();
    let duration = format_duration(video.duration_seconds);
    let views = video
        .views
        .map(|views| t!("{views} 回視聴", "{views} views"))
        .unwrap_or_else(|| tr("視聴回数未取得").to_owned());
    let date = video
        .created_at
        .as_deref()
        .map(|date| date.chars().take(10).collect::<String>())
        .unwrap_or_default();
    let mut thumbnail = div()
        .relative()
        .w_full()
        .aspect_ratio(16.0 / 9.0)
        .rounded(px(10.0))
        .overflow_hidden()
        .bg(rgb(RAISED));
    if let Some(url) = &video.thumbnail_url {
        thumbnail = thumbnail.child(
            img(url.clone())
                .size_full()
                .object_fit(gpui::ObjectFit::Cover),
        );
    }
    if !duration.is_empty() {
        thumbnail = thumbnail.child(
            div()
                .absolute()
                .right(px(6.0))
                .bottom(px(6.0))
                .px_2()
                .py_1()
                .rounded_md()
                .bg(rgba(0x000000CC))
                .text_size(px(9.0))
                .text_color(rgb(TEXT))
                .child(duration),
        );
    }
    div()
        .id(format!("channel-video-{}", video.id))
        .role(Role::Button)
        .aria_label(title.clone())
        .cursor_pointer()
        .on_click(cx.listener(move |this, _event, _window, cx| {
            this.play_vod_uuid(uuid.clone(), title.clone(), cx);
        }))
        .w(px(248.0))
        .flex_shrink_0()
        .flex()
        .flex_col()
        .gap_2()
        .child(thumbnail)
        .child(
            div()
                .text_size(px(13.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(rgb(TEXT))
                .truncate()
                .child(video.title.clone()),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .text_size(px(11.0))
                .text_color(rgb(MUTED))
                .child(views)
                .when(!date.is_empty(), |row| row.child(format!("· {date}"))),
        )
}

fn channel_clip_row(
    clips: &[kick_live::ChannelClip],
    limit: usize,
    cx: &mut Context<KickShell>,
) -> Stateful<Div> {
    let mut row = div()
        .id("channel-clip-row")
        .w_full()
        .flex()
        .gap_3()
        .overflow_x_scroll()
        .pb_2();
    for clip in clips.iter().take(limit) {
        let mut thumbnail = div()
            .relative()
            .w_full()
            .aspect_ratio(9.0 / 16.0)
            .rounded(px(10.0))
            .overflow_hidden()
            .bg(rgb(RAISED));
        if let Some(url) = &clip.thumbnail_url {
            thumbnail = thumbnail.child(
                img(url.clone())
                    .size_full()
                    .object_fit(gpui::ObjectFit::Cover),
            );
        }
        let views = clip
            .views
            .map(|views| t!("{views} 回視聴", "{views} views"))
            .unwrap_or_else(|| tr("視聴回数未取得").to_owned());
        row = row.child(
            div()
                .id(format!("channel-clip-{}", clip.id))
                .role(Role::Button)
                .aria_label(clip.title.clone())
                .cursor_pointer()
                .on_click({
                    let url = clip.clip_url.clone().unwrap_or_default();
                    let title = clip.title.clone();
                    cx.listener(move |this, _event, _window, cx| {
                        this.play_clip_url(url.clone(), title.clone(), cx);
                    })
                })
                .w(px(164.0))
                .flex_shrink_0()
                .flex()
                .flex_col()
                .gap_2()
                .child(thumbnail)
                .child(
                    div()
                        .text_size(px(12.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(rgb(TEXT))
                        .truncate()
                        .child(clip.title.clone()),
                )
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(rgb(MUTED))
                        .child(views),
                ),
        );
    }
    row
}

fn channel_category_row(categories: &[kick_live::ChannelCategory]) -> Stateful<Div> {
    let mut row = div()
        .id("channel-category-row")
        .w_full()
        .flex()
        .gap_3()
        .overflow_x_scroll()
        .pb_2();
    for category in categories {
        let mut card = div()
            .id(format!("channel-category-{}", category.id))
            .w(px(190.0))
            .h(px(108.0))
            .flex_shrink_0()
            .relative()
            .rounded(px(10.0))
            .overflow_hidden()
            .bg(rgb(RAISED));
        if let Some(url) = &category.banner_url {
            card = card.child(
                img(url.clone())
                    .size_full()
                    .object_fit(gpui::ObjectFit::Cover),
            );
        }
        let viewers = category
            .viewers
            .map(|viewers| t!("{viewers} 視聴中", "{viewers} watching"))
            .unwrap_or_else(|| tr("視聴者数未提供").to_owned());
        card = card.child(
            div()
                .absolute()
                .left_0()
                .right_0()
                .bottom_0()
                .p_3()
                .flex()
                .flex_col()
                .gap_1()
                .bg(rgba(0x000000CC))
                .child(
                    div()
                        .text_size(px(14.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(rgb(TEXT))
                        .truncate()
                        .child(category.name.clone()),
                )
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(rgb(SUBTLE))
                        .child(viewers),
                ),
        );
        row = row.child(card);
    }
    row
}

fn format_duration(seconds: u64) -> String {
    if seconds == 0 {
        return String::new();
    }
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

fn viewer_column(
    stream: Option<StreamSummary>,
    channel_state: ChannelLookupState,
    details: &SelectedChannelDetails,
    is_followed: bool,
    follow_pending: bool,
    follow_error: Option<&str>,
    chat_toggle_action: Option<HeaderAction>,
    notify_enabled: bool,
    player_host: Rc<RefCell<PlayerHost>>,
    cx: &mut Context<KickShell>,
) -> Stateful<Div> {
    let mut viewer = div()
        .id("selected-channel-viewer")
        .flex_1()
        .min_w(px(0.0))
        .flex()
        .flex_col()
        .gap_3()
        .p_4()
        .overflow_y_scroll()
        .child(video_stage(player_host.clone(), cx));
    if let Some(stream) = stream {
        viewer = viewer.child(channel_info_row(
            stream,
            channel_state,
            details,
            is_followed,
            follow_pending,
            follow_error,
            chat_toggle_action,
            notify_enabled,
            cx,
        ));
        viewer = viewer.child(channel_about_cards(details, channel_state));
    } else {
        viewer = viewer.child(
            div()
                .text_size(px(14.0))
                .text_color(rgb(SUBTLE))
                .child(tr("配信を選択してください")),
        );
    }

    viewer
}

fn video_stage(player_host: Rc<RefCell<PlayerHost>>, cx: &mut Context<KickShell>) -> Div {
    div()
        .relative()
        .w_full()
        .flex_shrink_0()
        .aspect_ratio(16.0 / 9.0)
        .rounded(px(12.0))
        .bg(rgb(VIDEO_BG))
        .overflow_hidden()
        .child(video_surface(player_host.clone()))
        .child(player_overlay(player_host, cx))
}

// The native AppKit player surface draws the visible hover controls above GPUI's
// Metal view. GPUI's accesskit adapter does not expose AppKit subviews, so this
// GPUI layer remains as the accessible control mirror and is visually covered by
// the native surface whenever a player is attached. Keep its labels descriptive.
fn player_overlay(
    player_host: Rc<RefCell<PlayerHost>>,
    cx: &mut Context<KickShell>,
) -> Stateful<Div> {
    let snapshot = player_host.borrow().controls_snapshot();
    let paused = snapshot.as_ref().is_some_and(|snapshot| {
        snapshot.transport_status == PlayerTransportStatus::Paused
            || snapshot.state == PlayerState::Paused
    });
    let muted = snapshot.as_ref().is_some_and(|snapshot| snapshot.is_muted);
    let time = snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.current_time_seconds);
    let (play_symbol, play_label, mute_symbol, mute_label) = player_overlay_labels(paused, muted);
    let (subtitle_active, subtitle_count, pip_active) = {
        let host = player_host.borrow();
        (
            host.subtitle_index > 0,
            host.subtitle_count,
            host.pip_active,
        )
    };
    let subtitle_aria = if subtitle_count == 0 {
        tr("この配信には字幕がありません")
    } else if subtitle_active {
        tr("字幕をオフにする")
    } else {
        tr("字幕をオンにする")
    };

    div()
        .id("player-overlay")
        .absolute()
        .bottom_0()
        .left_0()
        .right_0()
        .px_3()
        .py_2()
        .flex()
        .items_center()
        .gap_2()
        .bg(rgba(0x000000CC))
        .text_color(rgb(0xFFFFFF))
        .child(overlay_player_button(
            "overlay-play",
            play_symbol,
            play_label,
            PlayerAction::TogglePlayback,
            cx,
        ))
        .child(overlay_player_button(
            "overlay-mute",
            mute_symbol,
            mute_label,
            PlayerAction::ToggleMute,
            cx,
        ))
        .child(div().text_size(px(11.0)).child(format_playback_time(time)))
        .child(
            div()
                .text_size(px(10.0))
                .text_color(rgb(ACCENT))
                .child("LIVE"),
        )
        .child(div().flex_1())
        .child(
            div()
                .id("overlay-quality")
                .role(Role::Button)
                .aria_label(format!(
                    "{}: {}",
                    tr("画質"),
                    player_host.borrow().quality_cap.label()
                ))
                .w(px(46.0))
                .h(px(28.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded_md()
                .text_size(px(10.0))
                .text_color(rgb(0xFFFFFF))
                .cursor_pointer()
                .hover(|style| style.bg(rgba(0xFFFFFF22)))
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.player_host.borrow_mut().cycle_quality();
                    cx.notify();
                }))
                .child(player_host.borrow().quality_cap.label()),
        )
        .child(overlay_player_toggle(
            "overlay-captions",
            "CC",
            subtitle_aria,
            subtitle_active,
            PlayerAction::CycleSubtitles,
            cx,
        ))
        .child(overlay_disabled(
            "overlay-clip",
            "✂",
            tr("クリップは未対応"),
        ))
        .child(overlay_player_toggle(
            "overlay-pip",
            "▣",
            if pip_active {
                tr("ピクチャ・イン・ピクチャを終了")
            } else {
                tr("ピクチャ・イン・ピクチャ")
            },
            pip_active,
            PlayerAction::TogglePictureInPicture,
            cx,
        ))
        .child(overlay_header_button(
            "overlay-fullscreen",
            "⛶",
            tr("全画面"),
            HeaderAction::ToggleFullscreen,
            cx,
        ))
        .child(overlay_header_button(
            "overlay-theater",
            "▭",
            tr("劇場モード"),
            HeaderAction::ToggleTheater,
            cx,
        ))
}

fn player_overlay_labels(
    paused: bool,
    muted: bool,
) -> (&'static str, &'static str, &'static str, &'static str) {
    let play_symbol = if paused { "▶" } else { "⏸" };
    let play_label = if paused {
        tr("再生")
    } else {
        tr("一時停止")
    };
    let mute_symbol = if muted { "🔇" } else { "🔊" };
    let mute_label = if muted {
        tr("ミュート解除")
    } else {
        tr("ミュート")
    };
    (play_symbol, play_label, mute_symbol, mute_label)
}

fn overlay_player_button(
    id: &'static str,
    symbol: &'static str,
    aria_label: &'static str,
    action: PlayerAction,
    cx: &mut Context<KickShell>,
) -> Stateful<Div> {
    div()
        .id(id)
        .role(Role::Button)
        .aria_label(aria_label)
        .w(px(28.0))
        .h(px(28.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .text_size(px(14.0))
        .text_color(rgb(0xFFFFFF))
        .cursor_pointer()
        .hover(|style| style.bg(rgba(0xFFFFFF22)))
        .on_click(cx.listener(move |this, _event, _window, cx| {
            apply_player_action(this, action, cx);
            cx.notify();
        }))
        .child(symbol)
}

fn overlay_header_button(
    id: &'static str,
    symbol: &'static str,
    aria_label: &'static str,
    action: HeaderAction,
    cx: &mut Context<KickShell>,
) -> Stateful<Div> {
    div()
        .id(id)
        .role(Role::Button)
        .aria_label(aria_label)
        .w(px(28.0))
        .h(px(28.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .text_size(px(14.0))
        .text_color(rgb(0xFFFFFF))
        .cursor_pointer()
        .hover(|style| style.bg(rgba(0xFFFFFF22)))
        .on_click(cx.listener(move |this, _event, window, cx| {
            apply_header_action(this, action, window, cx);
            cx.notify();
        }))
        .child(symbol)
}

fn overlay_player_toggle(
    id: &'static str,
    symbol: &'static str,
    aria_label: &'static str,
    active: bool,
    action: PlayerAction,
    cx: &mut Context<KickShell>,
) -> Stateful<Div> {
    div()
        .id(id)
        .role(Role::Button)
        .aria_label(aria_label)
        .tab_index(0)
        .w(px(28.0))
        .h(px(28.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .text_size(px(12.0))
        .text_color(rgb(if active { ACCENT } else { 0xFFFFFF }))
        .cursor_pointer()
        .hover(|style| style.bg(rgba(0xFFFFFF22)))
        .on_click(cx.listener(move |this, _event, _window, cx| {
            apply_player_action(this, action, cx);
            cx.notify();
        }))
        .child(symbol)
}

fn overlay_disabled(id: &'static str, label: &'static str, aria: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .role(Role::Button)
        .aria_label(aria)
        .w(px(28.0))
        .h(px(28.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .text_size(px(12.0))
        .text_color(rgb(0x777777))
        .cursor_default()
        .child(label)
}

fn format_playback_time(seconds: Option<f64>) -> String {
    let Some(seconds) = seconds.filter(|seconds| seconds.is_finite() && *seconds >= 0.0) else {
        return "LIVE".to_owned();
    };
    let total = seconds as u64;
    format!(
        "{:02}:{:02}:{:02}",
        total / 3600,
        (total % 3600) / 60,
        total % 60
    )
}

fn notification_bell(enabled: bool, cx: &mut Context<KickShell>) -> Stateful<Div> {
    div()
        .id("channel-notify")
        .role(Role::Button)
        .aria_label(if enabled {
            tr("通知をオフにする")
        } else {
            tr("通知をオンにする")
        })
        .tab_index(0)
        .w(px(36.0))
        .h(px(36.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(rgb(RAISED))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(SELECTED_ROW)))
        .on_click(cx.listener(|this, _event, _window, cx| {
            let slug = this.selected_channel_slug.clone();
            this.toggle_channel_notifications(slug, cx);
        }))
        .child(kick_icon(
            include_bytes!("../assets/bell.svg"),
            16.0,
            if enabled { ACCENT } else { SUBTLE },
        ))
}

fn channel_share_button(cx: &mut Context<KickShell>) -> Stateful<Div> {
    div()
        .id("channel-share")
        .role(Role::Button)
        .aria_label(tr("チャンネルを共有"))
        .tab_index(0)
        .w(px(28.0))
        .h(px(28.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .text_size(px(13.0))
        .text_color(rgb(SUBTLE))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(RAISED)))
        .on_click(cx.listener(|this, _event, _window, cx| {
            let slug = this.selected_channel_slug.clone();
            if slug.is_empty() {
                return;
            }
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(format!(
                "https://kick.com/{slug}"
            )));
            this.show_toast(tr("リンクをコピーしました").to_owned(), cx);
        }))
        .child(kick_icon(
            include_bytes!("../assets/share.svg"),
            14.0,
            SUBTLE,
        ))
}

fn channel_info_button(cx: &mut Context<KickShell>) -> Stateful<Div> {
    div()
        .id("channel-info")
        .role(Role::Button)
        .aria_label(tr("チャンネルページを開く"))
        .tab_index(0)
        .w(px(28.0))
        .h(px(28.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .text_size(px(13.0))
        .text_color(rgb(SUBTLE))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(RAISED)))
        .on_click(cx.listener(|this, _event, _window, cx| {
            this.open_channel_page(cx);
        }))
        .child("ⓘ")
}

fn channel_info_row(
    stream: StreamSummary,
    channel_state: ChannelLookupState,
    details: &SelectedChannelDetails,
    is_followed: bool,
    follow_pending: bool,
    follow_error: Option<&str>,
    chat_toggle_action: Option<HeaderAction>,
    notify_enabled: bool,
    cx: &mut Context<KickShell>,
) -> Stateful<Div> {
    let channel_name = details
        .username
        .clone()
        .unwrap_or_else(|| stream.channel_name.clone());
    let title = details
        .title
        .clone()
        .unwrap_or_else(|| stream.title.clone());
    let category = stream
        .category
        .clone()
        .unwrap_or_else(|| tr("カテゴリなし").to_owned());
    let language = details
        .language
        .clone()
        .unwrap_or_else(|| "Japanese".to_owned());
    let viewers = details
        .viewer_count
        .or(stream.viewer_count)
        .map(|viewers| format!("{viewers}"))
        .unwrap_or_else(|| "—".to_owned());
    // The live ring is the outer 96 px background with 1 px padding; the
    // inner avatar must be 94 px so it cannot cover the ring.
    let avatar_size = if details.is_live { 94.0 } else { 96.0 };
    // Explicitly center the crop so portrait sources do not shift left.
    let avatar_inner = if let Some(path) = &details.avatar_path {
        circular_avatar_path(path.clone(), avatar_size).into_any()
    } else if let Some(url) = &details.profile_picture_url {
        circular_avatar(url.clone(), avatar_size).into_any()
    } else {
        avatar_placeholder(&channel_name, avatar_size).into_any()
    };
    // The ring is the outer background with 1 pt padding, so the image can
    // never cover or overflow it.
    let avatar = div()
        .relative()
        .w(px(96.0))
        .h(px(96.0))
        .flex_shrink_0()
        .rounded_full()
        .when(details.is_live, |frame| frame.bg(rgb(ACCENT)).p(px(1.0)))
        .child(
            div()
                .id("channel-avatar-open")
                .role(Role::Button)
                .aria_label(tr("チャンネルページを開く"))
                .tab_index(0)
                .w(px(avatar_size))
                .h(px(avatar_size))
                .rounded_full()
                .overflow_hidden()
                .bg(rgb(APP_BG))
                .cursor_pointer()
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.open_channel_page(cx);
                }))
                .child(avatar_inner),
        )
        .when(details.is_live, |column| {
            column.child(
                div()
                    .absolute()
                    .bottom(px(-2.0))
                    .left(px(31.0))
                    .px_2()
                    .py(px(1.0))
                    .rounded_full()
                    .bg(rgb(ACCENT))
                    .text_size(px(9.0))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(rgb(0x0B0B0F))
                    .child("LIVE"),
            )
        });

    let mut meta = div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_2()
        .text_size(px(11.0))
        .text_color(rgb(MUTED))
        .child(
            div()
                .px_2()
                .py_1()
                .rounded_full()
                .bg(rgb(RAISED))
                .text_color(rgb(SUBTLE))
                .child(category),
        )
        .child(language);
    for tag in details.tags.iter().take(3) {
        meta = meta.child(
            div()
                .px_2()
                .py_1()
                .rounded_full()
                .bg(rgb(RAISED))
                .text_color(rgb(SUBTLE))
                .child(tag.clone()),
        );
    }

    let mut details_column = div()
        .flex_1()
        .min_w(px(0.0))
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .id("channel-name-open")
                .role(Role::Button)
                .aria_label(tr("チャンネルページを開く"))
                .tab_index(0)
                .cursor_pointer()
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.open_channel_page(cx);
                }))
                .flex()
                .self_start()
                .items_center()
                .gap_2()
                .py(px(2.0))
                .child(
                    div()
                        .text_size(px(20.0))
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(rgb(TEXT))
                        .truncate()
                        .child(channel_name),
                )
                .when(details.verified, |row| {
                    row.child(div().text_size(px(12.0)).text_color(rgb(ACCENT)).child("✓"))
                }),
        )
        .child(
            div()
                .text_size(px(14.0))
                .text_color(rgb(SUBTLE))
                .truncate()
                .child(title),
        )
        .child(meta);
    if let Some(bio) = details
        .bio
        .as_deref()
        .and_then(|bio| bio.lines().next())
        .map(str::trim)
        .filter(|bio| !bio.is_empty())
    {
        details_column = details_column.child(
            div()
                .text_size(px(12.0))
                .text_color(rgb(MUTED))
                .truncate()
                .child(bio.to_owned()),
        );
    }

    let mut info = div()
        .id("selected-channel-details")
        .role(Role::Group)
        .aria_label(tr("選択中のチャンネル情報"))
        .w_full()
        .flex()
        .flex_col()
        .gap_3()
        .pt_6()
        .child(
            div()
                .w_full()
                .flex()
                .items_start()
                .gap_3()
                .child(avatar)
                .child(details_column)
                .child({
                    let mut actions = div()
                        .flex()
                        .flex_col()
                        .items_end()
                        .gap_2()
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .items_center()
                                .justify_end()
                                .gap_2()
                                .child(notification_bell(notify_enabled, cx))
                                .child(follow_button(is_followed, follow_pending, cx))
                                .child(
                                    div()
                                        .id("channel-points")
                                        .role(Role::Button)
                                        .aria_label(tr("チャンネルポイント"))
                                        .px_3()
                                        .py_2()
                                        .rounded_full()
                                        .bg(rgb(RAISED))
                                        .text_size(px(12.0))
                                        .text_color(rgb(SUBTLE))
                                        .cursor_pointer()
                                        .hover(|style| style.bg(rgb(SELECTED_ROW)))
                                        .on_click(cx.listener(|this, _event, _window, cx| {
                                            this.open_rewards(cx);
                                        }))
                                        .child(tr("ポイント")),
                                )
                                .child(
                                    div()
                                        .id("channel-gift-sub")
                                        .role(Role::Button)
                                        .aria_label(tr("サブスクを贈るは未対応"))
                                        .px_3()
                                        .py_2()
                                        .rounded_full()
                                        .bg(rgb(RAISED))
                                        .text_size(px(12.0))
                                        .text_color(rgb(MUTED))
                                        .cursor_default()
                                        .child(tr("☆ サブスクを贈る")),
                                )
                                .child(
                                    div()
                                        .id("channel-subscribe")
                                        .role(Role::Button)
                                        .aria_label(tr("登録は未対応"))
                                        .px_4()
                                        .py_2()
                                        .rounded_full()
                                        .bg(rgb(ACCENT))
                                        .text_size(px(12.0))
                                        .text_color(rgb(0x0B0B0F))
                                        .opacity(0.6)
                                        .cursor_default()
                                        .child(tr("登録")),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_3()
                                .text_size(px(12.0))
                                .text_color(rgb(MUTED))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_1()
                                        .child(kick_icon(
                                            include_bytes!("../assets/people.svg"),
                                            14.0,
                                            SUBTLE,
                                        ))
                                        .child(div().text_color(rgb(ACCENT)).child(viewers.clone()))
                                        .child(div().child(tr("が視聴中"))),
                                )
                                .child(channel_share_button(cx))
                                .child(channel_info_button(cx)),
                        );
                    if matches!(channel_state, ChannelLookupState::Failed(_)) {
                        actions = actions.child(header_action_button(
                            "channel-retry",
                            tr("チャンネルを再試行"),
                            None,
                            HeaderAction::RetryChannel,
                            cx,
                        ));
                    }
                    if let Some(action) = chat_toggle_action {
                        actions = actions.child(header_action_button(
                            "channel-chat-toggle",
                            tr("チャットを表示"),
                            Some(false),
                            action,
                            cx,
                        ));
                    }
                    actions
                }),
        );

    if let Some(goal) = &details.follower_goal {
        let progress = if goal.target > 0 {
            (goal.current as f32 / goal.target as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };
        info = info.child(
            div()
                .w_full()
                .p_3()
                .rounded(px(10.0))
                .bg(rgb(RAISED))
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_size(px(12.0))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(rgb(TEXT))
                                .child(tr("フォロワーゴール")),
                        )
                        .child(
                            div()
                                .text_size(px(11.0))
                                .text_color(rgb(MUTED))
                                .child(format!("{} / {}", goal.current, goal.target)),
                        ),
                )
                .child(
                    div()
                        .w(px(260.0))
                        .h(px(8.0))
                        .rounded_full()
                        .bg(rgb(APP_BG))
                        .child(
                            div()
                                .w(px(260.0 * progress))
                                .h_full()
                                .rounded_full()
                                .bg(rgb(ACCENT)),
                        ),
                ),
        );
    }

    if let Some(error) = follow_error {
        info = info.child(
            div()
                .text_size(px(11.0))
                .text_color(rgb(DANGER))
                .child(error.to_owned()),
        );
    }
    info
}

fn follow_button(is_followed: bool, pending: bool, cx: &mut Context<KickShell>) -> Stateful<Div> {
    let label = if pending {
        tr("処理中…")
    } else if is_followed {
        "フォロー中"
    } else {
        tr("フォローする")
    };
    div()
        .id("channel-follow")
        .role(Role::Button)
        .aria_label(label)
        .tab_index(0)
        .w(px(36.0))
        .h(px(36.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(rgb(if is_followed { RAISED } else { RAIL_BG }))
        .text_size(px(16.0))
        .text_color(rgb(if is_followed { ACCENT } else { TEXT }))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(SELECTED_ROW)))
        .focus_visible(|style| style.bg(rgb(SELECTED_ROW)))
        .on_click(cx.listener(|this, _event, _window, cx| {
            this.toggle_selected_channel_follow(cx);
        }))
        .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
            if is_activation_key(event) {
                this.toggle_selected_channel_follow(cx);
            }
        }))
        .child(if pending {
            div().text_size(px(14.0)).child("…").into_any()
        } else {
            kick_icon(
                include_bytes!("../assets/heart.svg"),
                16.0,
                if is_followed { ACCENT } else { TEXT },
            )
            .into_any()
        })
}

fn channel_about_cards(details: &SelectedChannelDetails, channel_state: ChannelLookupState) -> Div {
    let followers = details
        .followers_count
        .clone()
        .unwrap_or_else(|| tr("未取得").to_owned());
    let username = details
        .username
        .clone()
        .unwrap_or_else(|| tr("未取得").to_owned());
    let viewers = details
        .viewer_count
        .map(|viewers| t!("{viewers}人", "{viewers}"))
        .unwrap_or_else(|| tr("未取得").to_owned());
    let started_at = details
        .started_at
        .clone()
        .unwrap_or_else(|| tr("未取得").to_owned());
    let (live_label, live_color) = match channel_state {
        ChannelLookupState::Ready { is_live: true } => (tr("ライブ中"), ACCENT),
        ChannelLookupState::Ready { is_live: false } => (tr("オフライン"), MUTED),
        ChannelLookupState::Loading => (tr("取得中…"), MUTED),
        ChannelLookupState::Failed(_) => (tr("取得できません"), DANGER),
        ChannelLookupState::NotRequested => (tr("未取得"), MUTED),
    };
    div()
        .w_full()
        .flex()
        .gap_3()
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .p_4()
                .rounded(px(12.0))
                .bg(rgb(RAISED))
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .text_size(px(13.0))
                        .text_color(rgb(TEXT))
                        .child(tr("チャンネルについて")),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(rgb(MUTED))
                        .child(format!("@{username}")),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(rgb(SUBTLE))
                        .child(t!("フォロワー {followers}", "{followers} followers")),
                ),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .p_4()
                .rounded(px(12.0))
                .bg(rgb(RAISED))
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .text_size(px(13.0))
                        .text_color(rgb(TEXT))
                        .child(tr("配信情報")),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(rgb(SUBTLE))
                        .child(t!("視聴者 {viewers}", "{viewers} viewers")),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(rgb(MUTED))
                        .child(t!("開始 {started_at}", "Started {started_at}")),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .text_size(px(11.0))
                        .text_color(rgb(live_color))
                        .child("●")
                        .child(live_label),
                ),
        )
}

fn video_surface(player_host: Rc<RefCell<PlayerHost>>) -> Stateful<Div> {
    let fallback_status = player_host.borrow().status.clone();
    div()
        .on_children_prepainted(move |children, window, _cx| {
            if let Some(bounds) = children.first() {
                player_host.borrow_mut().sync_view(window, *bounds);
            } else {
                player_host.borrow_mut().detach_view();
            }
        })
        .id("video-surface")
        .role(Role::Group)
        .aria_label(tr("ライブ動画領域"))
        .w_full()
        .h_full()
        .bg(rgb(VIDEO_BG))
        .text_color(rgb(0xFFFFFF))
        .child(
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_1()
                .text_size(px(13.0))
                .text_color(rgb(MUTED))
                .child(fallback_status),
        )
}

fn chat_popout_placeholder(width: f32, cx: &mut Context<KickShell>) -> Div {
    div()
        .w(px(width))
        .min_w(px(CHAT_MIN_WIDTH))
        .max_w(px(CHAT_MAX_WIDTH))
        .h_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_2()
        .px_4()
        .bg(rgb(APP_BG))
        .child(
            div()
                .text_size(px(12.0))
                .text_color(rgb(MUTED))
                .child(tr("チャットは別ウィンドウで表示中")),
        )
        .child(
            div()
                .id("chat-restore")
                .role(Role::Button)
                .aria_label(tr("元のウィンドウに戻す"))
                .px_3()
                .py_1()
                .rounded_md()
                .bg(rgb(RAISED))
                .text_size(px(11.0))
                .text_color(rgb(SUBTLE))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(SELECTED_ROW)))
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.restore_chat(cx);
                }))
                .child(tr("元に戻す")),
        )
}

fn chat_popout_button(cx: &mut Context<KickShell>) -> Stateful<Div> {
    div()
        .id("chat-popout")
        .role(Role::Button)
        .aria_label(tr("チャットを別ウィンドウで開く"))
        .tab_index(0)
        .w(px(28.0))
        .h(px(28.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .text_size(px(15.0))
        .text_color(rgb(SUBTLE))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(RAISED)))
        .on_click(cx.listener(|this, _event, _window, cx| {
            this.pop_out_chat(cx);
        }))
        .child("⧉")
}

fn chat_column(
    chat_state: ListState,
    width: f32,
    chat_messages: Arc<[ChatMessage]>,
    emote_ids_by_name: Arc<HashMap<String, u64>>,
    emote_order: Arc<Vec<(String, u64)>>,
    emote_image_cache: Entity<BoundedEmoteImageCache>,
    history_state: ChatHistoryState,
    unread_messages: usize,
    chat_input_notice: Option<String>,
    chat_send_pending: bool,
    chat_send_confirmation: bool,
    emote_picker_open: bool,
    window: &Window,
    cx: &mut Context<KickShell>,
) -> Div {
    let (unresolved_emotes, failed_emotes) = emote_image_cache.read(cx).diagnostics.counts();
    let mut title = div().flex().items_center().gap_2().child(
        div()
            .text_size(px(13.0))
            .text_color(rgb(TEXT))
            .child(tr("チャット")),
    );
    if unresolved_emotes > 0 {
        title = title.child(
            div()
                .id("emote-unresolved-count")
                .text_size(px(10.0))
                .text_color(rgb(MUTED))
                .aria_label(t!(
                    "エモートID未解決 {unresolved_emotes}件",
                    "{unresolved_emotes} unresolved emote IDs"
                ))
                .child(format!("?{unresolved_emotes}")),
        );
    }
    if failed_emotes > 0 {
        title = title.child(
            div()
                .id("emote-image-failure-count")
                .text_size(px(10.0))
                .text_color(rgb(DANGER))
                .aria_label(t!(
                    "エモート画像取得失敗 {failed_emotes}件",
                    "{failed_emotes} emote images failed"
                ))
                .child(format!("⚠{failed_emotes}")),
        );
    }
    if chat_history_retry_available(history_state) {
        title = title.child(header_action_button(
            "retry-history",
            tr("履歴を再試行"),
            None,
            HeaderAction::RetryHistory,
            cx,
        ));
    }

    let header = div()
        .h(px(52.0))
        .min_h(px(52.0))
        .px_3()
        .flex()
        .items_center()
        .gap_2()
        .child(chat_collapse_button(
            "chat-collapse",
            tr("チャットを隠す"),
            HeaderAction::ToggleChat,
            cx,
        ))
        .child(title)
        .child(div().flex_1())
        .child(chat_popout_button(cx));

    div()
        .w(px(width))
        .min_w(px(CHAT_MIN_WIDTH))
        .max_w(px(CHAT_MAX_WIDTH))
        .h_full()
        .flex()
        .flex_col()
        .bg(rgb(APP_BG))
        .child(header)
        .child(chat_feed(
            chat_state,
            chat_messages,
            emote_ids_by_name.clone(),
            emote_image_cache.clone(),
            history_state,
            unread_messages,
            cx,
        ))
        .child(chat_composer(
            chat_input_notice,
            chat_send_pending,
            chat_send_confirmation,
            &emote_ids_by_name,
            &emote_order,
            emote_picker_open,
            emote_image_cache,
            window,
            cx,
        ))
}

fn is_activation_key(event: &KeyDownEvent) -> bool {
    !event.is_held
        && event.keystroke.modifiers == Modifiers::none()
        && matches!(event.keystroke.key.as_str(), "enter" | "space")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlayerShortcut {
    TogglePlayback,
    ToggleMute,
    ToggleFullscreen,
}

fn navigation_shortcut_allowed(search_has_focus: bool, text_input_has_focus: bool) -> bool {
    !search_has_focus && !text_input_has_focus
}

fn player_shortcut(
    key: &str,
    search_has_focus: bool,
    interactive_focus: bool,
    modifiers: Modifiers,
) -> Option<PlayerShortcut> {
    if search_has_focus || interactive_focus || modifiers != Modifiers::none() {
        return None;
    }
    match key {
        "space" => Some(PlayerShortcut::TogglePlayback),
        "m" => Some(PlayerShortcut::ToggleMute),
        "f" => Some(PlayerShortcut::ToggleFullscreen),
        _ => None,
    }
}

fn inline_chat_panel(
    chat_state: ListState,
    chat_messages: Arc<[ChatMessage]>,
    emote_ids_by_name: Arc<HashMap<String, u64>>,
    emote_order: Arc<Vec<(String, u64)>>,
    emote_image_cache: Entity<BoundedEmoteImageCache>,
    history_state: ChatHistoryState,
    unread_messages: usize,
    chat_input_notice: Option<String>,
    panel_height: f32,
    chat_send_pending: bool,
    chat_send_confirmation: bool,
    emote_picker_open: bool,
    window: &Window,
    cx: &mut Context<KickShell>,
) -> Div {
    let (unresolved_emotes, failed_emotes) = emote_image_cache.read(cx).diagnostics.counts();
    let mut title = div()
        .flex()
        .items_center()
        .gap_2()
        .child(div().text_color(rgb(TEXT)).child(tr("チャット")));
    if unresolved_emotes > 0 {
        title = title.child(
            div()
                .id("emote-unresolved-count")
                .text_size(px(10.0))
                .text_color(rgb(MUTED))
                .aria_label(t!(
                    "エモートID未解決 {unresolved_emotes}件",
                    "{unresolved_emotes} unresolved emote IDs"
                ))
                .child(format!("?{unresolved_emotes}")),
        );
    }
    if failed_emotes > 0 {
        title = title.child(
            div()
                .id("emote-image-failure-count")
                .text_size(px(10.0))
                .text_color(rgb(DANGER))
                .aria_label(t!(
                    "エモート画像取得失敗 {failed_emotes}件",
                    "{failed_emotes} emote images failed"
                ))
                .child(format!("⚠{failed_emotes}")),
        );
    }
    let mut header = div()
        .h(px(44.0))
        .min_h(px(44.0))
        .px_3()
        .flex()
        .items_center()
        .justify_between()
        .text_size(px(13.0))
        .child(title)
        .child(header_action_button(
            "chat-collapse-inline",
            tr("チャットを隠す"),
            None,
            HeaderAction::ToggleNarrowChat,
            cx,
        ));
    if chat_history_retry_available(history_state) {
        header = header.child(header_action_button(
            "retry-history-inline",
            tr("履歴を再試行"),
            None,
            HeaderAction::RetryHistory,
            cx,
        ));
    }
    div()
        .h(px(panel_height))
        .min_h(px(panel_height))
        .flex()
        .flex_col()
        .child(header)
        .child(chat_feed(
            chat_state,
            chat_messages,
            emote_ids_by_name.clone(),
            emote_image_cache.clone(),
            history_state,
            unread_messages,
            cx,
        ))
        .child(chat_composer(
            chat_input_notice,
            chat_send_pending,
            chat_send_confirmation,
            &emote_ids_by_name,
            &emote_order,
            emote_picker_open,
            emote_image_cache,
            window,
            cx,
        ))
}

fn attach_chat_input(
    host: &Rc<RefCell<ChatInputField>>,
    window: &mut Window,
    bounds: Bounds<gpui::Pixels>,
) {
    if bounds.is_empty() {
        return;
    }
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return;
    };
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        return;
    };
    let field = host.borrow();
    field.attach(
        appkit.ns_view.as_ptr(),
        f64::from(bounds.origin.x.as_f32()),
        f64::from(bounds.origin.y.as_f32()),
        f64::from(bounds.size.width.as_f32()),
        f64::from(bounds.size.height.as_f32()),
    );
    field.mark_painted();
}

fn chat_composer(
    notice: Option<String>,
    chat_send_pending: bool,
    chat_send_confirmation: bool,
    emote_ids_by_name: &HashMap<String, u64>,
    emote_order: &[(String, u64)],
    emote_picker_open: bool,
    emote_image_cache: Entity<BoundedEmoteImageCache>,
    _window: &Window,
    cx: &mut Context<KickShell>,
) -> Div {
    let input_empty = chat_input_host()
        .map(|host| host.borrow().text().trim().is_empty())
        .unwrap_or(true);
    if let Some(host) = chat_input_host() {
        host.borrow().set_placeholder(tr("メッセージを送信する"));
    }
    let mut composer = div()
        .image_cache(emote_image_cache)
        .min_h(px(64.0))
        .px_3()
        .py_2()
        .flex()
        .flex_col()
        .justify_end()
        .gap_1();
    if let Some(notice) = notice {
        composer = composer.child(
            div()
                .text_size(px(10.0))
                .text_color(rgb(MUTED))
                .child(notice),
        );
    }
    if emote_picker_open {
        let mut picker = div()
            .id("emote-picker")
            .max_h(px(200.0))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .rounded(px(12.0))
            .bg(rgb(RAISED))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_size(px(10.0)).text_color(rgb(MUTED)).child(t!(
                        "エモート {}件",
                        "Emotes: {}",
                        emote_ids_by_name.len()
                    )))
                    .child(
                        div()
                            .id("emote-close")
                            .role(Role::Button)
                            .aria_label(tr("エモート選択を閉じる"))
                            .tab_index(0)
                            .w(px(24.0))
                            .h(px(24.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .text_size(px(14.0))
                            .text_color(rgb(SUBTLE))
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(SELECTED_ROW)))
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.emote_picker_open = false;
                                cx.notify();
                            }))
                            .child("×"),
                    ),
            );
        if emote_ids_by_name.is_empty() {
            picker = picker.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(rgb(MUTED))
                            .child(tr("エモートを取得中…")),
                    )
                    .child(
                        div()
                            .id("emote-retry")
                            .role(Role::Button)
                            .aria_label(tr("エモートを再読み込み"))
                            .tab_index(0)
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .text_size(px(11.0))
                            .text_color(rgb(SUBTLE))
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(SELECTED_ROW)))
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.retry_emotes(cx);
                            }))
                            .child(tr("再読み込み")),
                    ),
            );
        } else {
            // Channel emotes come first so they are never pushed out by the cap.
            let mut fallback: Vec<(&String, &u64)> = emote_ids_by_name.iter().collect();
            fallback.sort_by(|left, right| left.0.cmp(right.0));
            let ordered: Vec<(&String, &u64)> = if emote_order.is_empty() {
                fallback
            } else {
                emote_order.iter().map(|(name, id)| (name, id)).collect()
            };
            let mut grid = div().flex().flex_wrap().gap_1();
            for (name, id) in ordered.into_iter().take(96) {
                let token = format!("[emote:{id}:{name}]");
                grid = grid.child(
                    div()
                        .id(format!("emote-pick-{id}"))
                        .role(Role::Button)
                        .aria_label(name.clone())
                        .tab_index(0)
                        .w(px(32.0))
                        .h(px(32.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_md()
                        .cursor_pointer()
                        .hover(|style| style.bg(rgb(SELECTED_ROW)))
                        .on_click(cx.listener(move |this, _event, _window, cx| {
                            this.chat_input_host.borrow().append_text(&token);
                            this.chat_input_host.borrow().focus();
                            this.chat_send_confirmation = false;
                            cx.notify();
                        }))
                        .child(img(emote_image_url(*id)).w(px(24.0)).h(px(24.0))),
                );
            }
            picker = picker.child(grid);
        }
        composer = composer.child(picker);
    }
    composer.child(
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .on_children_prepainted(move |children, window, _cx| {
                        if let Some(bounds) = children.first()
                            && let Some(host) = chat_input_host()
                        {
                            attach_chat_input(&host, window, *bounds);
                        }
                    })
                    .id("chat-input")
                    .role(Role::TextInput)
                    .aria_label(tr("チャットメッセージ"))
                    .tab_index(0)
                    .flex_1()
                    .min_w(px(0.0))
                    .h(px(36.0))
                    .rounded_full()
                    .bg(rgb(RAIL_BG))
                    .cursor_text()
                    .focus_visible(|style| style.bg(rgb(SELECTED_ROW)))
                    .on_click(cx.listener(|_this, _event, _window, _cx| {
                        if let Some(host) = chat_input_host() {
                            host.borrow().focus();
                        }
                    }))
                    .child(div().absolute().inset_0()),
            )
            .child(
                div()
                    .id("chat-emote")
                    .role(Role::Button)
                    .aria_label(if emote_picker_open {
                        tr("エモート選択を閉じる")
                    } else {
                        tr("エモートを選択")
                    })
                    .tab_index(0)
                    .w(px(32.0))
                    .h(px(32.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .text_size(px(14.0))
                    .text_color(rgb(if emote_picker_open { ACCENT } else { MUTED }))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(RAISED)))
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.emote_picker_open = !this.emote_picker_open;
                        cx.notify();
                    }))
                    .child("☺"),
            )
            .child(
                div()
                    .id("chat-send")
                    .role(Role::Button)
                    .aria_label(tr("チャットを送信"))
                    .tab_index(0)
                    .px_3()
                    .h(px(32.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .bg(rgb(ACCENT))
                    .text_size(px(12.0))
                    .text_color(rgb(0x0B0B0F))
                    .opacity(if input_empty || chat_send_pending {
                        0.5
                    } else {
                        1.0
                    })
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.send_chat_message(cx);
                    }))
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                        if is_activation_key(event) {
                            this.send_chat_message(cx);
                        }
                    }))
                    .child(if chat_send_pending {
                        tr("送信中")
                    } else {
                        tr("送信")
                    }),
            ),
    )
}

fn chat_feed(
    chat_state: ListState,
    chat_messages: Arc<[ChatMessage]>,
    emote_ids_by_name: Arc<HashMap<String, u64>>,
    emote_image_cache: Entity<BoundedEmoteImageCache>,
    history_state: ChatHistoryState,
    unread_messages: usize,
    cx: &mut Context<KickShell>,
) -> Stateful<Div> {
    let emote_diagnostics = emote_image_cache.read(cx).diagnostics.clone();
    let shell = cx.entity().downgrade();
    let mut feed = div()
        .image_cache(emote_image_cache)
        .id("chat-message-list")
        .role(Role::List)
        .aria_label(tr("Kickチャットメッセージ"))
        .flex_1()
        .min_h(px(0.0))
        .w_full()
        .flex()
        .flex_col()
        .overflow_hidden();

    if chat_messages.is_empty() {
        let empty_message = history_state.empty_message();
        feed = feed.child(
            div()
                .id("chat-history-status")
                .role(Role::Status)
                .aria_label(empty_message.clone())
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .px_3()
                .text_size(px(12.0))
                .text_color(rgb(MUTED))
                .child(empty_message),
        );
    } else {
        feed = feed.child(
            list(chat_state, move |index, _window, _cx| {
                chat_messages
                    .get(index)
                    .map(|message| {
                        chat_row(
                            index,
                            message,
                            &emote_ids_by_name,
                            &emote_diagnostics,
                            shell.clone(),
                        )
                        .into_any()
                    })
                    .unwrap_or_else(|| div().into_any())
            })
            .w_full()
            .flex_1()
            .min_h(px(0.0)),
        );
    }

    if unread_messages > 0 {
        feed = feed.child(new_chat_messages_button(unread_messages, cx));
    }

    feed
}

fn new_chat_messages_button(unread_messages: usize, cx: &mut Context<KickShell>) -> Stateful<Div> {
    let label = t!(
        "↓ 新着メッセージ {unread_messages}件",
        "↓ {unread_messages} new messages"
    );
    div()
        .id("chat-new-messages")
        .role(Role::Button)
        .aria_label(t!(
            "{unread_messages}件の新着メッセージ。最新へ移動",
            "{unread_messages} new messages. Jump to latest"
        ))
        .tab_index(0)
        .h(px(36.0))
        .min_h(px(36.0))
        .mx_2()
        .my_1()
        .px_3()
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(rgb(RAISED))
        .text_size(px(12.0))
        .text_color(rgb(ACCENT))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(SELECTED_ROW)))
        .focus_visible(|style| style.bg(rgb(SELECTED_ROW)))
        .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
            if is_activation_key(event) {
                this.chat_state.set_follow_mode(FollowMode::Tail);
                this.chat_state.scroll_to_end();
                this.chat_unread_messages = 0;
                this.chat_scrolled_up = false;
                cx.notify();
            }
        }))
        .on_click(cx.listener(|this, _event, _window, cx| {
            this.chat_state.set_follow_mode(FollowMode::Tail);
            this.chat_state.scroll_to_end();
            this.chat_unread_messages = 0;
            this.chat_scrolled_up = false;
            cx.notify();
        }))
        .child(label)
}

const CHAT_AUTHOR_COLORS: [u32; 10] = [
    0x8AB4F8, // blue
    0x7EE787, // green
    0xF78FB3, // pink
    0xF2CC60, // yellow
    0xFFA657, // orange
    0xC792EA, // purple
    0x7DD3FC, // cyan
    0xE6E7E9, // white
    0xA5D6A7, // mint
    0xEF9A9A, // coral
];

fn chat_author_color(author: &str) -> u32 {
    let hash = author.bytes().fold(0u64, |hash, byte| {
        hash.wrapping_mul(31).wrapping_add(u64::from(byte))
    });
    CHAT_AUTHOR_COLORS[(hash as usize) % CHAT_AUTHOR_COLORS.len()]
}

fn rewards_panel(
    rewards: &[kick_live::ChannelReward],
    loading: bool,
    error: Option<&str>,
    cx: &mut Context<KickShell>,
) -> Div {
    let mut panel = div()
        .w(px(280.0))
        .rounded(px(12.0))
        .bg(rgb(RAISED))
        .p_3()
        .flex()
        .flex_col()
        .gap_2()
        .text_color(rgb(TEXT))
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_size(px(13.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child(tr("チャンネルポイント")),
                )
                .child(
                    div()
                        .id("rewards-close")
                        .role(Role::Button)
                        .aria_label(tr("ポイント表示を閉じる"))
                        .tab_index(0)
                        .w(px(24.0))
                        .h(px(24.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .text_size(px(14.0))
                        .text_color(rgb(SUBTLE))
                        .cursor_pointer()
                        .hover(|style| style.bg(rgb(SELECTED_ROW)))
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.rewards_open = false;
                            cx.notify();
                        }))
                        .child("×"),
                ),
        );
    if loading {
        panel = panel.child(
            div()
                .text_size(px(11.0))
                .text_color(rgb(MUTED))
                .child(tr("読み込み中…")),
        );
    } else if let Some(error) = error {
        panel = panel.child(
            div()
                .text_size(px(11.0))
                .text_color(rgb(DANGER))
                .child(error.to_owned()),
        );
    } else if rewards.is_empty() {
        panel = panel.child(
            div()
                .text_size(px(11.0))
                .text_color(rgb(MUTED))
                .child(tr("交換できる報酬がありません")),
        );
    } else {
        for reward in rewards.iter().take(8) {
            let id = reward.id.clone();
            panel = panel.child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .py_1()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .min_w(px(0.0))
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .truncate()
                                    .child(reward.title.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(10.0))
                                    .text_color(rgb(MUTED))
                                    .child(format!("{} pt", reward.cost)),
                            ),
                    )
                    .child(
                        div()
                            .id(format!("reward-{id}"))
                            .role(Role::Button)
                            .aria_label(reward.title.clone())
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .bg(rgb(ACCENT))
                            .text_size(px(10.0))
                            .text_color(rgb(0x0B0B0F))
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.redeem_reward(id.clone(), cx);
                            }))
                            .child(tr("交換")),
                    ),
            );
        }
    }
    panel
}

fn account_popover(account_state: AccountState, cx: &mut Context<KickShell>) -> Div {
    let mut popover = div()
        .w(px(220.0))
        .rounded(px(12.0))
        .bg(rgb(RAISED))
        .p_2()
        .flex()
        .flex_col()
        .gap_1()
        .text_size(px(12.0))
        .text_color(rgb(TEXT));
    match &account_state {
        AccountState::SignedIn { username } => {
            popover = popover
                .child(
                    div()
                        .px_2()
                        .py_1()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child(format!("@{username}")),
                )
                .child(context_menu_action(
                    "account-menu-logout",
                    tr("ログアウト"),
                    cx,
                    |this, cx| this.sign_out(cx),
                ));
        }
        _ => {
            popover = popover
                .child(
                    div()
                        .px_2()
                        .py_1()
                        .text_color(rgb(MUTED))
                        .child(tr("アカウント")),
                )
                .child(context_menu_action(
                    "account-menu-login",
                    tr("ログイン"),
                    cx,
                    |this, cx| this.open_login_panel(cx),
                ));
        }
    }
    popover
}

fn chat_context_popover(menu: &ChatContextMenu, cx: &mut Context<KickShell>) -> Div {
    let author = menu.author.clone();
    let author_slug = menu.author_slug.clone();
    let text = menu.text.clone();
    let mut popover = div()
        .w(px(220.0))
        .rounded(px(12.0))
        .bg(rgb(RAISED))
        .p_2()
        .flex()
        .flex_col()
        .gap_1()
        .text_size(px(12.0))
        .text_color(rgb(TEXT))
        .child(
            div()
                .px_2()
                .py_1()
                .text_size(px(12.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(rgb(chat_author_color(&menu.author)))
                .child(menu.author.clone()),
        );
    if let Some(slug) = author_slug.clone() {
        let author_for_page = author.clone();
        popover = popover.child(context_menu_action(
            "chat-menu-user",
            tr("ユーザー情報"),
            cx,
            move |this, cx| this.open_author_page(slug.clone(), author_for_page.clone(), cx),
        ));
    } else {
        popover = popover.child(context_menu_disabled(tr("ユーザー情報は未取得")));
    }
    popover = popover.child(context_menu_action(
        "chat-menu-copy",
        tr("コメントをコピー"),
        cx,
        move |this, cx| this.copy_chat_message(text.clone(), cx),
    ));
    if let Some(slug) = author_slug.clone() {
        popover = popover.child(context_menu_action(
            "chat-menu-follow",
            tr("フォロー"),
            cx,
            move |this, cx| this.toggle_follow_slug(slug.clone(), cx),
        ));
    }
    popover = popover.child(context_menu_action(
        "chat-menu-block",
        tr("ブロック（ローカルのみ）"),
        cx,
        move |this, cx| this.block_author(author.clone(), cx),
    ));
    popover = popover.child(context_menu_action(
        "chat-menu-close",
        tr("閉じる"),
        cx,
        move |this, cx| this.close_chat_context_menu(cx),
    ));
    popover.occlude()
}

fn context_menu_action(
    id: &'static str,
    label: &'static str,
    cx: &mut Context<KickShell>,
    action: impl Fn(&mut KickShell, &mut Context<KickShell>) + 'static,
) -> Stateful<Div> {
    let action = Rc::new(action);
    let click_action = action.clone();
    let key_action = action;
    div()
        .id(id)
        .role(Role::Button)
        .aria_label(label)
        .tab_index(0)
        .px_2()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .hover(|style| style.bg(rgb(SELECTED_ROW)))
        .focus_visible(|style| style.bg(rgb(SELECTED_ROW)))
        .on_click(cx.listener(move |this, _event, _window, cx| {
            click_action(this, cx);
        }))
        .on_key_down(cx.listener(move |this, event, _window, cx| {
            if is_activation_key(event) {
                key_action(this, cx);
            }
        }))
        .child(label)
}

fn context_menu_disabled(label: &'static str) -> Div {
    div()
        .px_2()
        .py_1()
        .text_size(px(12.0))
        .text_color(rgb(MUTED))
        .child(label)
}

fn chat_row(
    index: usize,
    message: &ChatMessage,
    emote_ids_by_name: &HashMap<String, u64>,
    emote_diagnostics: &EmoteDiagnostics,
    shell: gpui::WeakEntity<KickShell>,
) -> Stateful<Div> {
    if let Some(reward) = message.reward.as_ref() {
        return chat_reward_row(index, message, reward, shell);
    }
    let displayed_text = chat_text_for_display(&message.text);
    let message_for_click = message.clone();
    let shell_for_click = shell;
    let has_emote = parse_chat_segments(&message.text)
        .iter()
        .any(|segment| matches!(segment, ChatSegment::Emote { .. }));
    let mut row = div()
        .id(format!("chat-message-{index}-{}", message.id))
        .role(Role::ListItem)
        .aria_label(format!("{}: {displayed_text}", message.author))
        .w_full()
        .px_3()
        .py_1()
        .flex()
        .items_start()
        .gap_1()
        .cursor_pointer()
        .on_click(move |event, _window, cx| {
            let position = event.position();
            let _ = shell_for_click.update(cx, |this, cx| {
                this.open_chat_context_menu(&message_for_click, position, cx);
            });
        });

    if has_emote {
        row = row.child(
            chat_inline_message(message, emote_ids_by_name, emote_diagnostics)
                .flex_1()
                .min_w(px(0.0)),
        );
    } else {
        for badge in message.badges.iter().take(4) {
            row = row.child(chat_badge_element(badge));
        }
        let full = format!("{}: {}", message.author, displayed_text);
        let author_end = message.author.len() + 1;
        let highlight = gpui::HighlightStyle {
            color: Some(rgb(chat_author_color(&message.author)).into()),
            font_weight: Some(gpui::FontWeight::SEMIBOLD),
            ..Default::default()
        };
        row = row.child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .text_size(px(14.0))
                .line_height(px(20.0))
                .text_color(rgb(TEXT))
                .child(gpui::StyledText::new(full).with_highlights([(0..author_end, highlight)])),
        );
    }
    row
}

/// Renders a channel-point redemption as a chat event row, matching the
/// official client's left color bar and "redeemed" wording.
fn chat_reward_row(
    index: usize,
    message: &ChatMessage,
    reward: &ChatReward,
    shell: gpui::WeakEntity<KickShell>,
) -> Stateful<Div> {
    let color = reward
        .background_color
        .as_deref()
        .and_then(parse_hex_color)
        .unwrap_or(ACCENT);
    let full = t!(
        "{} が {} を交換",
        "{} redeemed {}",
        message.author,
        reward.title
    );
    let author_end = message.author.len();
    let highlight = gpui::HighlightStyle {
        color: Some(rgb(color).into()),
        font_weight: Some(gpui::FontWeight::SEMIBOLD),
        ..Default::default()
    };
    let message_for_click = message.clone();
    let shell_for_click = shell;
    let mut body = div()
        .flex_1()
        .min_w(px(0.0))
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_size(px(13.0))
                .line_height(px(18.0))
                .text_color(rgb(TEXT))
                .child(gpui::StyledText::new(full).with_highlights([(0..author_end, highlight)])),
        );
    if let Some(input) = reward.user_input.as_ref() {
        body = body.child(div().text_size(px(12.0)).text_color(rgb(SUBTLE)).child(t!(
            "メッセージ: {}",
            "Message: {}",
            input
        )));
    }
    div()
        .id(format!("chat-reward-{index}-{}", message.id))
        .role(Role::ListItem)
        .aria_label(format!("{}: {}", message.author, reward.title))
        .w_full()
        .px_3()
        .py_1()
        .flex()
        .items_start()
        .gap_2()
        .border_l_4()
        .border_color(rgb(color))
        .cursor_pointer()
        .on_click(move |event, _window, cx| {
            let position = event.position();
            let _ = shell_for_click.update(cx, |this, cx| {
                this.open_chat_context_menu(&message_for_click, position, cx);
            });
        })
        .child(div().text_size(px(14.0)).text_color(rgb(color)).child("✦"))
        .child(body)
}

fn parse_hex_color(value: &str) -> Option<u32> {
    let hex = value.strip_prefix('#')?;
    match hex.len() {
        6 => u32::from_str_radix(hex, 16).ok(),
        3 => {
            let mut expanded = String::with_capacity(6);
            for ch in hex.chars() {
                expanded.push(ch);
                expanded.push(ch);
            }
            u32::from_str_radix(&expanded, 16).ok()
        }
        _ => None,
    }
}

/// Renders one chat badge with the official Kick badge artwork.
fn chat_badge_element(badge: &ChatBadge) -> AnyElement {
    let icon: Option<&'static [u8]> = match badge.kind.as_str() {
        "subscriber" | "sub_gifter" => Some(include_bytes!("../assets/badge-subscriber.svg")),
        "moderator" => Some(include_bytes!("../assets/badge-moderator.svg")),
        "vip" => Some(include_bytes!("../assets/badge-vip.svg")),
        "verified" => Some(include_bytes!("../assets/badge-verified.svg")),
        "staff" => Some(include_bytes!("../assets/badge-staff.svg")),
        "og" => Some(include_bytes!("../assets/badge-og.svg")),
        "bot" => Some(include_bytes!("../assets/badge-bot.svg")),
        "host" => Some(include_bytes!("../assets/badge-host.svg")),
        "sidekick" => Some(include_bytes!("../assets/badge-sidekick.svg")),
        _ => None,
    };
    if let Some(data) = icon {
        let show_count = matches!(badge.kind.as_str(), "subscriber" | "sub_gifter")
            && !badge.text.is_empty()
            && badge.text.chars().count() <= 3;
        let mut container = div()
            .relative()
            .w(px(18.0))
            .h(px(18.0))
            .flex_shrink_0()
            .child(kick_icon(data, 18.0, TEXT));
        if show_count {
            container = container.child(
                div()
                    .absolute()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(8.0))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(rgb(0x0B0B0F))
                    .child(badge.text.clone()),
            );
        }
        return container.into_any();
    }
    let (background, foreground, label) = match badge.kind.as_str() {
        "founder" => (0xFF625A, 0xFFFFFF, "F".to_owned()),
        _ => (
            0x3A3D42,
            0xE6E7E9,
            if badge.text.is_empty() {
                badge.kind.chars().take(3).collect()
            } else {
                badge.text.clone()
            },
        ),
    };
    div()
        .flex_shrink_0()
        .h(px(16.0))
        .min_w(px(16.0))
        .px_1()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.0))
        .bg(rgb(background))
        .text_size(px(10.0))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(rgb(foreground))
        .child(label)
        .into_any()
}

fn chat_emote_element(
    id: Option<&str>,
    name: &str,
    emote_ids_by_name: &HashMap<String, u64>,
    emote_diagnostics: &EmoteDiagnostics,
) -> AnyElement {
    if let Some(emote_id) = resolve_emote_id(id, name, emote_ids_by_name) {
        emote_diagnostics.clear_unresolved(name);
        let url = emote_image_url(emote_id);
        let fallback_diagnostics = emote_diagnostics.clone();
        let fallback_url = url.clone();
        let fallback_name = name.to_owned();
        img(url)
            .w(px(24.0))
            .h(px(24.0))
            .flex_shrink_0()
            .overflow_hidden()
            .object_fit(gpui::ObjectFit::Contain)
            .with_fallback(move || {
                emote_image_fallback(
                    &fallback_name,
                    fallback_diagnostics.image_failure(&fallback_url),
                )
            })
            .into_any()
    } else {
        if emote_diagnostics.record_unresolved(name) {
            eprintln!("emote unresolved: {name}");
        }
        unresolved_emote_token(name).into_any()
    }
}

/// Renders an emote-bearing message as one wrapping flow so continuation
/// lines start at the left edge instead of indenting under the username.
fn chat_inline_message(
    message: &ChatMessage,
    emote_ids_by_name: &HashMap<String, u64>,
    emote_diagnostics: &EmoteDiagnostics,
) -> Div {
    let mut row = div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .w_full()
        .min_w(px(0.0));
    for badge in message.badges.iter().take(4) {
        row = row.child(div().mr(px(2.0)).child(chat_badge_element(badge)));
    }
    row = row.child(
        div()
            .max_w(px(160.0))
            .flex_shrink_0()
            .truncate()
            .text_size(px(14.0))
            .line_height(px(20.0))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(rgb(chat_author_color(&message.author)))
            .child(format!("{}:\u{00A0}", message.author)),
    );
    for segment in parse_chat_segments(&message.text) {
        match segment {
            ChatSegment::Text(text) => {
                for chunk in chat_wrap_chunks(&text) {
                    row = row.child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(14.0))
                            .line_height(px(20.0))
                            .text_color(rgb(TEXT))
                            .child(chunk),
                    );
                }
            }
            ChatSegment::Emote { id, name } => {
                row = row.child(div().mx(px(1.0)).child(chat_emote_element(
                    id.as_deref(),
                    &name,
                    emote_ids_by_name,
                    emote_diagnostics,
                )));
            }
        }
    }
    row
}

fn chat_wrap_chunks(text: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_whitespace() {
            if current.is_empty() {
                chunks.push("\u{00A0}".to_owned());
            } else {
                current.push('\u{00A0}');
                chunks.push(std::mem::take(&mut current));
            }
        } else if is_cjk(ch) {
            if !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
            }
            chunks.push(ch.to_string());
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn is_cjk(ch: char) -> bool {
    matches!(
        ch as u32,
        0x3000..=0x30FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xF900..=0xFAFF
            | 0xFF00..=0xFFEF
            | 0xAC00..=0xD7AF
    )
}

fn emote_image_fallback(name: &str, failure: Option<EmoteImageFailure>) -> AnyElement {
    match failure {
        Some(failure) => {
            let glyph = match failure {
                EmoteImageFailure::HttpStatus(status) => status.to_string(),
                EmoteImageFailure::Decode => "⚠".to_owned(),
                EmoteImageFailure::Transport => "⚠".to_owned(),
                EmoteImageFailure::Other => "⚠".to_owned(),
            };
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(9.0))
                .line_height(px(12.0))
                .text_color(rgb(DANGER))
                .child(glyph)
                .into_any()
        }
        None => div()
            .size_full()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .text_size(px(8.0))
            .line_height(px(12.0))
            .child(format!(":{name}:"))
            .into_any(),
    }
}

fn unresolved_emote_token(name: &str) -> Div {
    div()
        .flex()
        .items_center()
        .gap_1()
        .child(
            div()
                .flex_shrink_0()
                .w(px(12.0))
                .h(px(12.0))
                .flex()
                .items_center()
                .justify_center()
                .bg(rgb(LINE))
                .text_size(px(8.0))
                .line_height(px(10.0))
                .text_color(rgb(MUTED))
                .child("?"),
        )
        .child(
            div()
                .min_w(px(0.0))
                .max_w(px(128.0))
                .truncate()
                .text_color(rgb(MUTED))
                .child(format!(":{name}:")),
        )
}

fn resolve_emote_id(
    marker_id: Option<&str>,
    name: &str,
    emote_ids_by_name: &HashMap<String, u64>,
) -> Option<u64> {
    marker_id
        .and_then(|id| id.parse::<u64>().ok())
        .or_else(|| emote_ids_by_name.get(&name.to_ascii_lowercase()).copied())
}

fn emote_image_url(id: u64) -> String {
    format!("https://files.kick.com/emotes/{id}/fullsize")
}

/// Shared chat state rendered by the popout window.
#[derive(Clone)]
struct ChatSnapshot {
    messages: Arc<[ChatMessage]>,
    emotes: Arc<HashMap<String, u64>>,
    history: ChatHistoryState,
    unread: usize,
}

impl Default for ChatSnapshot {
    fn default() -> Self {
        Self {
            messages: Arc::from(Vec::new()),
            emotes: Arc::new(HashMap::new()),
            history: ChatHistoryState::NoChannel,
            unread: 0,
        }
    }
}

/// A separate window that mirrors the chat while the main window shows a
/// placeholder. Closing the window restores the in-window chat.
struct ChatPopout {
    snapshot: Arc<Mutex<ChatSnapshot>>,
    shell: gpui::WeakEntity<KickShell>,
    list: ListState,
    last_len: usize,
}

impl ChatPopout {
    fn new(
        snapshot: Arc<Mutex<ChatSnapshot>>,
        shell: gpui::WeakEntity<KickShell>,
        cx: &mut Context<Self>,
    ) -> Self {
        let _ = cx;
        let len = snapshot
            .lock()
            .map(|snapshot| snapshot.messages.len())
            .unwrap_or(0);
        let list = new_chat_list_state(len);
        list.scroll_to_end();
        Self {
            snapshot,
            shell,
            list,
            last_len: len,
        }
    }

    /// Updates the list outside render so the new item count is laid out.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let len = self
            .snapshot
            .lock()
            .map(|snapshot| snapshot.messages.len())
            .unwrap_or(0);
        if len != self.last_len {
            self.list.reset(len);
            self.list.set_follow_mode(FollowMode::Tail);
            self.list.scroll_to_end();
            self.last_len = len;
        }
        cx.notify();
    }
}

impl Render for ChatPopout {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let snapshot = self
            .snapshot
            .lock()
            .map(|snapshot| snapshot.clone())
            .unwrap_or_default();

        let diagnostics = EmoteDiagnostics::default();
        let shell = self.shell.clone();
        let messages = snapshot.messages.clone();
        let emotes = snapshot.emotes.clone();
        div()
            .size_full()
            .bg(rgb(APP_BG))
            .flex()
            .flex_col()
            .text_color(rgb(TEXT))
            .child(
                div()
                    .h(px(44.0))
                    .min_h(px(44.0))
                    .px_3()
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(rgb(LINE))
                    .child(div().text_size(px(13.0)).child(tr("チャット")))
                    .child(
                        div()
                            .id("popout-restore")
                            .role(Role::Button)
                            .aria_label(tr("元のウィンドウに戻す"))
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .text_size(px(11.0))
                            .text_color(rgb(SUBTLE))
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(RAISED)))
                            .on_click(|_event, window, _cx| {
                                window.remove_window();
                            })
                            .child(tr("元に戻す")),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .w_full()
                    .flex()
                    .flex_col()
                    .child(
                        list(self.list.clone(), move |index, _window, _cx| {
                            messages
                                .get(index)
                                .map(|message| {
                                    chat_row(index, message, &emotes, &diagnostics, shell.clone())
                                        .into_any()
                                })
                                .unwrap_or_else(|| div().into_any())
                        })
                        .w_full()
                        .flex_1()
                        .min_h(px(0.0)),
                    ),
            )
    }
}

/// Runtime counters shown in the debug window. The values are refreshed by
/// the shell once per second while debug mode is on.
#[derive(Clone, Default)]
struct DebugStats {
    resident_bytes: u64,
    peak_bytes: u64,
    chat_messages: usize,
    chat_pending: usize,
    chat_dropped: u64,
    reward_events: u64,
    move_events: u64,
    emote_count: usize,
    streams: usize,
    channel: String,
    player_status: String,
    catalog: String,
    realtime: String,
    history: String,
}

/// A separate window that renders the shared debug counters once per second.
struct DebugView {
    stats: Arc<Mutex<DebugStats>>,
}

impl DebugView {
    fn new(stats: Arc<Mutex<DebugStats>>, cx: &mut Context<Self>) -> Self {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        })
        .detach();
        Self { stats }
    }
}

impl Render for DebugView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let stats = self
            .stats
            .lock()
            .map(|stats| stats.clone())
            .unwrap_or_default();
        let mut rows = div().flex().flex_col().gap_2().child(
            div()
                .text_size(px(16.0))
                .text_color(rgb(TEXT))
                .child(tr("デバッグ")),
        );
        rows = rows
            .child(debug_row(tr("RSS"), format_bytes(stats.resident_bytes)))
            .child(debug_row(tr("ピークRSS"), format_bytes(stats.peak_bytes)))
            .child(debug_row(tr("チャンネル"), stats.channel.clone()))
            .child(debug_row(tr("プレイヤー"), stats.player_status.clone()))
            .child(debug_row(
                tr("チャット保持"),
                t!("{} 件", "{}", stats.chat_messages),
            ))
            .child(debug_row(
                tr("遅延中"),
                t!("{} 件", "{}", stats.chat_pending),
            ))
            .child(debug_row(tr("破棄"), t!("{} 件", "{}", stats.chat_dropped)))
            .child(debug_row(
                tr("交換イベント"),
                t!("{} 件", "{}", stats.reward_events),
            ))
            .child(debug_row(
                tr("レイド移動"),
                t!("{} 件", "{}", stats.move_events),
            ))
            .child(debug_row(
                tr("エモート"),
                t!("{} 件", "{}", stats.emote_count),
            ))
            .child(debug_row(tr("配信数"), t!("{} 件", "{}", stats.streams)))
            .child(debug_row(tr("カタログ"), stats.catalog.clone()))
            .child(debug_row(tr("チャット接続"), stats.realtime.clone()))
            .child(debug_row(tr("履歴"), stats.history.clone()));
        div()
            .size_full()
            .bg(rgb(APP_BG))
            .p_4()
            .flex()
            .flex_col()
            .gap_2()
            .text_size(px(12.0))
            .text_color(rgb(SUBTLE))
            .child(rows)
    }
}

fn debug_row(label: &'static str, value: String) -> Div {
    div()
        .flex()
        .items_start()
        .gap_3()
        .child(
            div()
                .w(px(96.0))
                .flex_shrink_0()
                .text_color(rgb(MUTED))
                .child(label),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .text_color(rgb(TEXT))
                .child(value),
        )
}

fn format_bytes(bytes: u64) -> String {
    if bytes == 0 {
        return "—".to_owned();
    }
    let megabytes = bytes as f64 / (1024.0 * 1024.0);
    format!("{megabytes:.1} MB")
}

/// Reads the current and peak resident size of this process.
fn process_memory_bytes() -> Option<(u64, u64)> {
    unsafe {
        let mut info: libc::mach_task_basic_info = std::mem::zeroed();
        let mut count = libc::MACH_TASK_BASIC_INFO_COUNT;
        let result = libc::task_info(
            libc::mach_task_self(),
            libc::MACH_TASK_BASIC_INFO,
            &mut info as *mut libc::mach_task_basic_info as libc::task_info_t,
            &mut count,
        );
        if result != libc::KERN_SUCCESS {
            return None;
        }
        Some((info.resident_size, info.resident_size_max))
    }
}

fn main() {
    if std::env::args().any(|arg| arg == "--notify-helper-status") {
        let _ = kick_player_macos::request_notification_authorization();
        std::thread::sleep(Duration::from_secs(1));
        println!("status={}", kick_player_macos::notification_status());
        return;
    }
    if std::env::args().any(|arg| arg == "--notify-helper-test") {
        let _ = kick_player_macos::request_notification_authorization();
        std::thread::sleep(Duration::from_secs(1));
        println!("status={}", kick_player_macos::notification_status());
        let _ = kick_player_macos::post_notification("Kick Native Notifier", "テスト通知です", "");
        std::thread::sleep(Duration::from_secs(1));
        return;
    }
    if std::env::args().any(|arg| arg == "--notify-helper") {
        run_notify_helper();
    }
    if std::env::args().any(|arg| arg == "--auth-cookie-probe") {
        run_auth_cookie_probe();
        return;
    }
    if std::env::args().any(|arg| arg == "--auth-token-probe") {
        run_auth_token_probe();
        return;
    }
    if std::env::args().any(|arg| arg == "--auth-token-stdin") {
        run_auth_token_stdin_probe();
        return;
    }
    if std::env::args().any(|arg| arg == "--keychain-save-probe") {
        run_keychain_save_probe();
        return;
    }
    if std::env::args().any(|arg| arg == "--auth-token-save-stdin") {
        run_auth_token_save_stdin_probe();
        return;
    }
    if let Some(position) = std::env::args().position(|arg| arg == "--avatar-crop-probe") {
        let url = std::env::args().nth(position + 1).unwrap_or_default();
        run_avatar_crop_probe(&url);
        return;
    }
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    if let Some(slug) = rewards_check_slug() {
        run_rewards_check(&slug);
        return;
    }
    if let Some((slug, seconds)) = web_reward_probe_args() {
        run_web_reward_probe(&slug, seconds);
        return;
    }
    // Both reqwest clients in this process share rustls. The Zed HTTP client
    // and the Kick API client can otherwise leave the process-level provider
    // ambiguous when both ring and aws-lc-rs are compiled in.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let application = gpui_platform::application()
        .with_http_client(Arc::new(reqwest_client::ReqwestClient::new()));
    application.on_reopen(|cx| {
        // The platform calls this only when no visible window remains. A
        // minimized window can still be present in the window list but cannot
        // be reliably restored through GPUI, so close any leftovers and open
        // a fresh main window.
        for handle in cx.windows() {
            let _ = handle.update(cx, |_, window, _| {
                window.remove_window();
            });
        }
        open_main_window(cx);
        cx.activate(true);
    });
    application.run(|cx: &mut App| {
        let _ = cx.text_system().add_fonts(vec![std::borrow::Cow::Borrowed(
            &include_bytes!("../assets/InterVariable.ttf")[..],
        )]);
        set_language(load_language());
        cx.bind_keys([
            KeyBinding::new("cmd-,", OpenSettings, None),
            KeyBinding::new("cmd-q", QuitApp, None),
        ]);
        cx.set_menus(app_menus());
        cx.on_action(|_: &QuitApp, cx: &mut App| cx.quit());
        cx.on_action(|_: &HideApp, cx: &mut App| cx.hide());
        cx.on_action(|_: &HideOtherApps, cx: &mut App| cx.hide_other_apps());
        cx.on_action(|_: &ShowAllApps, cx: &mut App| cx.unhide_other_apps());
        cx.on_action(|_: &ShowAbout, _cx: &mut App| show_about_panel());
        cx.on_action(|_: &OpenSettings, cx: &mut App| {
            let shell = cx
                .try_global::<ShellHandle>()
                .and_then(|handle| handle.0.lock().ok().and_then(|guard| guard.clone()));
            if let Some(shell) = shell {
                let _ = shell.update(cx, |this, cx| {
                    if this.settings_open {
                        this.close_settings(cx);
                    } else {
                        this.open_settings(cx);
                    }
                });
            }
        });
        install_activation_observer();
        std::mem::forget(cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        }));
        open_main_window(cx);
        cx.activate(true);
    });
}

/// Headless LaunchAgent mode. It keeps the channel subscription alive after
/// the main app quits, requests notification permission, and posts local
/// notifications on `StreamerIsLive`. It has no window or menu-bar icon.
fn run_notify_helper() -> ! {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let _ = kick_player_macos::request_notification_authorization();
    let slugs: Vec<String> = load_notify_channels().into_iter().collect();
    if slugs.is_empty() {
        std::process::exit(0);
    }
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        std::process::exit(1);
    };
    let (sender, mut receiver) = tokio::sync::mpsc::channel(64);
    let cancel = Arc::new(AtomicBool::new(false));
    let Ok(client) = KickClient::unauthenticated_for_probe() else {
        std::process::exit(1);
    };
    let client_id = format!("kick-native-notify-helper-{}", std::process::id());
    runtime.spawn(async move {
        kick_live::realtime::run_stream_notifications(client, client_id, slugs, sender, cancel)
            .await;
    });
    loop {
        match runtime.block_on(receiver.recv()) {
            Some(kick_live::realtime::StreamNotificationEvent::StreamerIsLive { slug, .. }) => {
                let _ = kick_player_macos::post_notification(&slug, "配信が始まりました", &slug);
            }
            Some(kick_live::realtime::StreamNotificationEvent::StreamEnded { .. }) => {}
            None => break,
        }
    }
    std::process::exit(0);
}

fn install_activation_observer() {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    unsafe {
        let center: *mut AnyObject = msg_send![class!(NSNotificationCenter), defaultCenter];
        let name = objc2_foundation::NSString::from_str("NSApplicationDidBecomeActiveNotification");
        let block = block2::RcBlock::new(move |_notification: *mut AnyObject| {
            let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
            let _: () = msg_send![app, unhide: std::ptr::null::<AnyObject>()];
            let windows: *mut AnyObject = msg_send![app, windows];
            let count: usize = msg_send![windows, count];
            for index in 0..count {
                let window: *mut AnyObject = msg_send![windows, objectAtIndex: index];
                let _: () = msg_send![
                    window,
                    makeKeyAndOrderFront: std::ptr::null::<AnyObject>()
                ];
            }
        });
        let observer: *mut AnyObject = msg_send![
            center,
            addObserverForName: &*name as *const _ as *mut AnyObject,
            object: std::ptr::null::<AnyObject>(),
            queue: std::ptr::null::<AnyObject>(),
            usingBlock: &*block as *const _ as *mut AnyObject
        ];
        // Keep the block and observer alive for the process lifetime.
        std::mem::forget(block);
        std::mem::forget(name);
        std::mem::forget(observer);
    }
}

fn run_auth_cookie_probe() {
    let Some(input) = credentials::load_cookie().ok().flatten() else {
        println!("no_session");
        return;
    };
    let cookie = match input.with_bytes(SessionCookie::parse_bytes) {
        Ok(cookie) => cookie,
        Err(error) => {
            println!("parse_error={error:?}");
            return;
        }
    };
    println!("cookie_names={}", cookie.cookie_names().join(","));
    let Ok(runtime) = RuntimeBuilder::new_current_thread().enable_all().build() else {
        println!("runtime_error");
        return;
    };
    let Ok(client) = KickClient::unauthenticated_for_probe() else {
        println!("client_error");
        return;
    };
    print_auth_probe("all", runtime.block_on(client.validate_session(&cookie)));
    for (label, names) in [
        ("session_token", vec!["session_token"]),
        ("kick_session", vec!["kick_session"]),
        (
            "session_token+kick_session",
            vec!["session_token", "kick_session"],
        ),
    ] {
        match cookie.select_cookies(&names) {
            Some(subset) => {
                print_auth_probe(label, runtime.block_on(client.validate_session(&subset)))
            }
            None => println!("{label}=missing"),
        }
    }
    print_auth_probe(
        "bearer_only",
        runtime.block_on(client.validate_session_bearer_only(&cookie)),
    );
}

fn run_avatar_crop_probe(url: &str) {
    if url.is_empty() {
        println!("missing_url");
        return;
    }
    let path = avatar_cache_path(url);
    let Ok(runtime) = RuntimeBuilder::new_current_thread().enable_all().build() else {
        println!("runtime_error");
        return;
    };
    match runtime.block_on(fetch_and_crop_avatar(url, &path)) {
        Ok(path) => println!("ok {}", path.display()),
        Err(error) => println!("failed {error}"),
    }
}

fn run_auth_token_save_stdin_probe() {
    let mut token = String::new();
    let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut token);
    let token = token.trim();
    if token.is_empty() {
        println!("empty");
        return;
    }
    let input = match credentials::secret_cookie_from_login_text(token) {
        Ok(input) => input,
        Err(error) => {
            println!("construct=err {error:?}");
            return;
        }
    };
    match credentials::save_cookie(&input) {
        Ok(()) => println!("save=ok"),
        Err(error) => println!("save=err {error:?}"),
    }
    match credentials::load_cookie() {
        Ok(Some(_)) => println!("load=ok"),
        Ok(None) => println!("load=none"),
        Err(error) => println!("load=err {error:?}"),
    }
    match credentials::delete_cookie() {
        Ok(()) => println!("delete=ok"),
        Err(error) => println!("delete=err {error:?}"),
    }
}

fn run_keychain_save_probe() {
    let input = match credentials::secret_cookie_from_login_text("session_token=probe") {
        Ok(input) => input,
        Err(error) => {
            println!("construct=err {error:?}");
            return;
        }
    };
    match credentials::save_cookie(&input) {
        Ok(()) => println!("save=ok"),
        Err(error) => println!("save=err {error:?}"),
    }
    match credentials::load_cookie() {
        Ok(Some(_)) => println!("load=ok"),
        Ok(None) => println!("load=none"),
        Err(error) => println!("load=err {error:?}"),
    }
    match credentials::delete_cookie() {
        Ok(()) => println!("delete=ok"),
        Err(error) => println!("delete=err {error:?}"),
    }
}

fn run_auth_token_stdin_probe() {
    let mut token = String::new();
    let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut token);
    let token = token.trim();
    if token.is_empty() {
        println!("empty");
        return;
    }
    let cookie = match SessionCookie::from_session_token(token) {
        Ok(cookie) => cookie,
        Err(error) => {
            println!("construct_error={error:?}");
            return;
        }
    };
    let Ok(runtime) = RuntimeBuilder::new_current_thread().enable_all().build() else {
        println!("runtime_error");
        return;
    };
    let Ok(client) = KickClient::unauthenticated_for_probe() else {
        println!("client_error");
        return;
    };
    print_auth_probe(
        "stdin_from_session_token",
        runtime.block_on(client.validate_session(&cookie)),
    );
}

fn run_auth_token_probe() {
    let Some(input) = credentials::load_cookie().ok().flatten() else {
        println!("no_session");
        return;
    };
    let cookie = match input.with_bytes(SessionCookie::parse_bytes) {
        Ok(cookie) => cookie,
        Err(error) => {
            println!("parse_error={error:?}");
            return;
        }
    };
    let Some(token) = cookie.session_token_value() else {
        println!("no_session_token");
        return;
    };
    let Ok(runtime) = RuntimeBuilder::new_current_thread().enable_all().build() else {
        println!("runtime_error");
        return;
    };
    let Ok(client) = KickClient::unauthenticated_for_probe() else {
        println!("client_error");
        return;
    };
    match SessionCookie::from_session_token(token.as_str()) {
        Ok(from_token) => print_auth_probe(
            "from_session_token",
            runtime.block_on(client.validate_session(&from_token)),
        ),
        Err(error) => println!("from_session_token=construct_err {error:?}"),
    }
}

fn print_auth_probe(label: &str, result: Result<SessionUser, SessionAuthError>) {
    match result {
        Ok(_) => println!("{label}=ok user=1"),
        Err(error) => println!("{label}=err {error:?}"),
    }
}

fn open_main_window(cx: &mut App) {
    let bounds = Bounds::centered(None, size(px(1360.0), px(820.0)), cx);
    let handle = cx
        .open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: None,
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(12.0), px(12.0))),
                }),
                ..Default::default()
            },
            |_window, cx| cx.new(|cx| KickShell::new(cx)),
        )
        .expect("open Kick GPUI PoC window");
    // Move the window to the active Space when the app is activated from the
    // Dock. Without this, a window left on another Space stays there and the
    // Dock icon appears to do nothing.
    let _ = handle.update(cx, |_, window, _| {
        if let Ok(handle) = HasWindowHandle::window_handle(window)
            && let RawWindowHandle::AppKit(appkit) = handle.as_raw()
        {
            let view = unsafe { &*appkit.ns_view.as_ptr().cast::<NSView>() };
            unsafe {
                let ns_window: *mut objc2::runtime::AnyObject = objc2::msg_send![view, window];
                if !ns_window.is_null() {
                    // NSWindowCollectionBehaviorMoveToActiveSpace = 1 << 1
                    let _: () = objc2::msg_send![ns_window, setCollectionBehavior: 2u64];
                }
            }
        }
    });
}

fn app_menus() -> Vec<Menu> {
    vec![Menu {
        name: "Kick Native".into(),
        items: vec![
            MenuItem::action(tr("Kick Native について"), ShowAbout),
            MenuItem::separator(),
            MenuItem::action(tr("設定…"), OpenSettings),
            MenuItem::separator(),
            MenuItem::action(tr("Kick Native を隠す"), HideApp),
            MenuItem::action(tr("ほかを隠す"), HideOtherApps),
            MenuItem::action(tr("すべてを表示"), ShowAllApps),
            MenuItem::separator(),
            MenuItem::action(tr("Kick Native を終了"), QuitApp),
        ],
        disabled: false,
    }]
}

fn show_about_panel() {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    unsafe {
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        if app.is_null() {
            return;
        }
        let _: () = msg_send![app, orderFrontStandardAboutPanel: std::ptr::null::<AnyObject>()];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delayed_chat_releases_messages_only_after_their_due_time() {
        let start = Instant::now();
        let mut pending = VecDeque::new();
        for (index, seconds) in [5_u64, 6].into_iter().enumerate() {
            pending.push_back(PendingChatMessage {
                due: start + Duration::from_secs(seconds),
                message: ChatMessage {
                    id: format!("m{index}"),
                    author: "viewer".to_owned(),
                    text: format!("message {index}"),
                    timestamp_ms: None,
                    author_slug: None,
                    badges: Vec::new(),
                    reward: None,
                },
            });
        }

        assert!(take_due_chat(&mut pending, start).is_empty());
        assert!(take_due_chat(&mut pending, start + Duration::from_secs(4)).is_empty());
        assert_eq!(pending.len(), 2);

        let first = take_due_chat(&mut pending, start + Duration::from_secs(5));
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].id, "m0");
        assert_eq!(pending.len(), 1);

        let second = take_due_chat(&mut pending, start + Duration::from_secs(6));
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].id, "m1");
        assert!(pending.is_empty());
        assert!(take_due_chat(&mut pending, start + Duration::from_secs(60)).is_empty());
    }

    #[test]
    fn account_control_reports_state_without_exposing_cookie_material() {
        let signed_out = AccountState::SignedOut;
        assert_eq!(signed_out.label(false), tr("未ログイン"));
        assert_eq!(
            signed_out.accessibility_label(),
            tr("アカウント: 未ログイン。押してログイン")
        );

        let entering_cookie = AccountState::EnteringCookie;
        assert_eq!(entering_cookie.label(false), tr("ログイン情報を入力中…"));
        assert_eq!(
            entering_cookie.accessibility_label(),
            tr("アカウント: ログイン入力ウィンドウを表示中")
        );

        let signed_in = AccountState::SignedIn {
            username: "fixture_user".to_owned(),
        };
        assert_eq!(signed_in.label(false), "@fixture_user");
        assert_eq!(
            signed_in.accessibility_label(),
            tr("アカウント: fixture_user。押してメニューを開く")
        );
        assert_eq!(signed_in.label(true), tr("ログイン済み"));
    }

    #[test]
    fn panel_widths_stay_within_layout_bounds() {
        assert_eq!(clamp_navigation_width(20.0), NAV_MIN_WIDTH);
        assert_eq!(clamp_navigation_width(900.0), NAV_MAX_WIDTH);
        assert_eq!(clamp_chat_width(200.0, 340.0), CHAT_MIN_WIDTH);
        assert_eq!(clamp_chat_width(500.0, 340.0), 340.0);
        assert_eq!(
            chat_width_limit_for_window(1_360.0, NAV_DEFAULT_WIDTH, true),
            CHAT_MAX_WIDTH
        );
    }

    #[test]
    fn unavailable_following_state_does_not_claim_follow_data() {
        assert_eq!(
            following_unavailable_message(&AccountState::SignedOut),
            tr("未ログインのためフォロー情報を表示できません")
        );
        assert_eq!(
            following_unavailable_message(&AccountState::SignedIn {
                username: "viewer".to_owned(),
            }),
            tr("このクライアントではフォロー情報をまだ取得できません")
        );
        assert_eq!(
            following_unavailable_message(&AccountState::Failed("unverified".to_owned())),
            tr("サインイン状態は未確認のためフォロー情報を表示できません")
        );
    }

    #[test]
    fn empty_current_user_response_does_not_mark_cookie_invalid() {
        let message =
            session_auth_failure_message(&SessionAuthError::UnrecognizedResponse(Some(Vec::new())));

        assert_eq!(
            message,
            tr("現在のユーザー確認の応答が空でした。サインイン状態は未確認です")
        );
        assert!(!message.contains(tr("無効")));
    }

    #[test]
    fn narrow_chat_height_scales_with_window_and_stays_bounded() {
        assert_eq!(inline_chat_height(400.0), 180.0);
        assert_eq!(inline_chat_height(820.0), 278.8);
        assert_eq!(inline_chat_height(1_200.0), 280.0);
    }

    #[test]
    fn narrow_chat_defaults_collapsed_without_changing_wide_visibility() {
        assert!(!chat_visibility_for_layout(true, true, false));
        assert!(chat_visibility_for_layout(false, true, false));
        assert!(!chat_visibility_for_layout(false, false, true));
        assert!(chat_visibility_for_layout(true, false, true));
    }

    #[test]
    fn chat_list_splices_keep_append_eviction_and_history_prefix_bounded() {
        assert_eq!(
            chat_splice_plan(&[1, 2, 3], &[1, 2, 3, 4, 5]),
            vec![(3..3, 2)]
        );
        assert_eq!(
            chat_splice_plan(&[1, 2, 3], &[2, 3, 4]),
            vec![(0..1, 0), (2..2, 1)]
        );
        assert_eq!(chat_splice_plan(&[2, 3], &[1, 2, 3]), vec![(0..0, 1)]);
        assert_eq!(chat_splice_plan(&[1, 2], &[1, 2]), Vec::new());
    }

    #[test]
    fn unread_count_ignores_replayed_ids_but_counts_idless_messages() {
        let existing = vec![ChatMessage {
            id: "known".to_owned(),
            author: "viewer".to_owned(),
            text: "older".to_owned(),
            timestamp_ms: None,
            author_slug: None,
            badges: Vec::new(),
            reward: None,
        }];
        let incoming = vec![
            ChatMessage {
                id: "known".to_owned(),
                author: "viewer".to_owned(),
                text: "replay".to_owned(),
                timestamp_ms: None,
                author_slug: None,
                badges: Vec::new(),
                reward: None,
            },
            ChatMessage {
                id: "new".to_owned(),
                author: "viewer".to_owned(),
                text: "new".to_owned(),
                timestamp_ms: None,
                author_slug: None,
                badges: Vec::new(),
                reward: None,
            },
            ChatMessage {
                id: "new".to_owned(),
                author: "viewer".to_owned(),
                text: "duplicate in batch".to_owned(),
                timestamp_ms: None,
                author_slug: None,
                badges: Vec::new(),
                reward: None,
            },
            ChatMessage {
                id: String::new(),
                author: "viewer".to_owned(),
                text: "without id".to_owned(),
                timestamp_ms: None,
                author_slug: None,
                badges: Vec::new(),
                reward: None,
            },
        ];

        assert_eq!(count_new_chat_messages(&existing, &incoming), 2);
    }

    #[test]
    fn reward_colors_accept_hex_and_reject_other_values() {
        assert_eq!(parse_hex_color("#53FC18"), Some(0x53FC18));
        assert_eq!(parse_hex_color("#fff"), Some(0xFFFFFF));
        assert_eq!(parse_hex_color("53FC18"), None);
        assert_eq!(parse_hex_color("#12345"), None);
        assert_eq!(parse_hex_color("#gggggg"), None);
    }

    #[test]
    fn chat_author_colors_are_deterministic_and_from_the_palette() {
        assert_eq!(chat_author_color("alice"), chat_author_color("alice"));
        assert!(CHAT_AUTHOR_COLORS.contains(&chat_author_color("alice")));
        assert!(CHAT_AUTHOR_COLORS.contains(&chat_author_color(tr("日本語ユーザー"))));
    }

    #[test]
    fn emote_placeholders_render_as_text_without_an_image_source() {
        assert_eq!(
            chat_text_for_display("hello [emote:5748088:collectiblesPepeLaugh]!"),
            "hello :collectiblesPepeLaugh:!"
        );
    }

    #[test]
    fn channel_catalogue_resolves_name_tokens_to_the_verified_image_route() {
        let catalogue = HashMap::from([("testemote".to_owned(), 1234)]);
        assert_eq!(resolve_emote_id(None, "TestEmote", &catalogue), Some(1234));
        assert_eq!(
            resolve_emote_id(Some("5748088"), "ignored", &catalogue),
            Some(5748088)
        );
        assert_eq!(
            emote_image_url(37226),
            "https://files.kick.com/emotes/37226/fullsize"
        );
        assert_eq!(resolve_emote_id(None, "unknown", &catalogue), None);
    }

    #[test]
    fn emote_diagnostics_separate_unresolved_names_from_image_failures() {
        let diagnostics = EmoteDiagnostics::default();
        assert_eq!(diagnostics.counts(), (0, 0));

        diagnostics.record_unresolved("collectiblesCarRetro");
        diagnostics.record_unresolved("collectiblesCarRetro");
        assert_eq!(diagnostics.counts(), (1, 0));

        let url = emote_image_url(37226);
        assert!(diagnostics.record_image_failure(&url, EmoteImageFailure::HttpStatus(404)));
        assert!(!diagnostics.record_image_failure(&url, EmoteImageFailure::HttpStatus(404)));
        assert_eq!(diagnostics.counts(), (1, 1));
        assert_eq!(
            diagnostics.image_failure(&url),
            Some(EmoteImageFailure::HttpStatus(404))
        );

        assert!(diagnostics.record_image_failure(&url, EmoteImageFailure::Decode));
        assert_eq!(
            diagnostics.image_failure(&url),
            Some(EmoteImageFailure::Decode)
        );

        diagnostics.clear_unresolved("collectiblesCarRetro");
        assert!(diagnostics.clear_image_failure(&url));
        assert_eq!(diagnostics.counts(), (0, 0));

        diagnostics.record_unresolved("one");
        diagnostics.clear();
        assert_eq!(diagnostics.counts(), (0, 0));
    }

    #[test]
    fn emote_diagnostics_stay_bounded() {
        let diagnostics = EmoteDiagnostics::default();
        for index in 0..(EMOTE_DIAGNOSTIC_MAX + 8) {
            diagnostics.record_unresolved(&format!("name-{index}"));
        }
        let (unresolved, _) = diagnostics.counts();
        assert_eq!(unresolved, EMOTE_DIAGNOSTIC_MAX);

        for index in 0..(EMOTE_IMAGE_CACHE_MAX + 8) {
            diagnostics.record_image_failure(
                &format!("https://files.kick.com/emotes/{index}/fullsize"),
                EmoteImageFailure::Transport,
            );
        }
        let (_, failures) = diagnostics.counts();
        assert_eq!(failures, EMOTE_IMAGE_CACHE_MAX);
    }

    #[test]
    fn emote_failure_labels_are_specific_enough_to_diagnose() {
        assert_eq!(EmoteImageFailure::HttpStatus(404).label(), "HTTP 404");
        assert_eq!(EmoteImageFailure::Decode.label(), tr("デコード失敗"));
        assert_eq!(EmoteImageFailure::Transport.label(), tr("取得失敗"));
        assert_eq!(EmoteImageFailure::Other.label(), tr("読込失敗"));
    }

    #[test]
    fn channel_forbidden_status_is_distinct_from_catalog_success() {
        let catalog = CatalogState::Ready;
        let channel = ChannelLookupState::Failed(LiveError::Api(KickApiError::HttpStatus(403)));

        assert_eq!(catalog.label(), tr("匿名カタログの実データ"));
        assert_eq!(
            channel.label(),
            tr("チャンネル取得失敗: HTTP 403（アクセス拒否）")
        );
    }

    #[test]
    fn failed_channel_can_be_retried_without_restarting_a_healthy_worker() {
        let forbidden = LiveError::Api(KickApiError::HttpStatus(403));
        let state = ChannelLookupState::Failed(forbidden);

        assert!(should_retry_channel_on_reselect(state));
        assert!(!retry_requires_worker_restart(forbidden));
        assert!(retry_requires_worker_restart(LiveError::RuntimeUnavailable));
        assert!(!should_retry_channel_on_reselect(
            ChannelLookupState::Ready { is_live: true }
        ));
    }

    #[test]
    fn catalog_retry_is_offered_after_a_failed_load() {
        assert!(catalog_retry_available(CatalogState::Failed(
            LiveError::Api(KickApiError::Transport(
                kick_live::TransportFailure::Connection
            ))
        )));
        assert!(!catalog_retry_available(CatalogState::Loading));
        assert!(!catalog_retry_available(CatalogState::Ready));
    }

    #[test]
    fn recent_history_is_bounded_before_it_reaches_the_ui() {
        let page = AnonymousRecentMessages {
            messages: (0..CHAT_HISTORY_MAX_MESSAGES + 20)
                .map(|index| kick_live::AnonymousChatMessage {
                    id: Some(format!("message-{index}")),
                    content: Some("x".repeat(4 * 1024)),
                    created_at: None,
                    username: Some("viewer".to_owned()),
                    author_slug: None,
                    badges: Vec::new(),
                })
                .collect(),
        };

        let messages = bounded_recent_messages(page);
        let retained_bytes: usize = messages
            .iter()
            .map(|message| message.id.len() + message.author.len() + message.text.len())
            .sum();

        assert!(messages.len() <= CHAT_HISTORY_MAX_MESSAGES);
        assert!(retained_bytes <= CHAT_HISTORY_MAX_BYTES);
    }

    #[test]
    fn history_and_realtime_labels_are_reported_separately() {
        let label = ChatHistoryState::Ready { message_count: 25 }.label();

        assert_eq!(label, tr("履歴 25件"));
        assert_eq!(ChatRealtimeState::NoChannel.label(), tr("未選択"));
    }

    #[test]
    fn history_and_live_messages_merge_once_with_id_deduplication() {
        let history = vec![
            ChatMessage {
                id: "history-1".to_owned(),
                author: "viewer".to_owned(),
                text: "older".to_owned(),
                timestamp_ms: None,
                author_slug: None,
                badges: Vec::new(),
                reward: None,
            },
            ChatMessage {
                id: "duplicate".to_owned(),
                author: "viewer".to_owned(),
                text: "same message".to_owned(),
                timestamp_ms: None,
                author_slug: None,
                badges: Vec::new(),
                reward: None,
            },
            ChatMessage {
                id: String::new(),
                author: "viewer".to_owned(),
                text: "history without id".to_owned(),
                timestamp_ms: None,
                author_slug: None,
                badges: Vec::new(),
                reward: None,
            },
        ];
        let live = vec![
            ChatMessage {
                id: "duplicate".to_owned(),
                author: "viewer".to_owned(),
                text: "same message".to_owned(),
                timestamp_ms: None,
                author_slug: None,
                badges: Vec::new(),
                reward: None,
            },
            ChatMessage {
                id: "live-1".to_owned(),
                author: "viewer".to_owned(),
                text: "new".to_owned(),
                timestamp_ms: None,
                author_slug: None,
                badges: Vec::new(),
                reward: None,
            },
            ChatMessage {
                id: String::new(),
                author: "viewer".to_owned(),
                text: "live without id".to_owned(),
                timestamp_ms: None,
                author_slug: None,
                badges: Vec::new(),
                reward: None,
            },
        ];

        let merged = merge_chat_messages(&history, live);

        assert_eq!(merged.len(), 5);
        assert_eq!(merged[1].id, "duplicate");
        assert_eq!(merged[2].text, "history without id");
        assert_eq!(merged[3].id, "live-1");
        assert_eq!(merged[4].text, "live without id");
        assert!(merged.len() <= CHAT_HISTORY_MAX_MESSAGES);
        assert!(
            merged
                .iter()
                .map(|message| message.id.len() + message.author.len() + message.text.len())
                .sum::<usize>()
                <= CHAT_HISTORY_MAX_BYTES
        );
    }

    #[test]
    fn full_chat_event_queue_counts_dropped_message() {
        let (events, _event_receiver) = mpsc::sync_channel(1);
        assert!(
            events
                .try_send(ChatWorkerEvent::Status {
                    generation: 1,
                    status: kick_live::realtime::ChatStatus::Connected,
                })
                .is_ok()
        );

        let dropped = AtomicU64::new(0);
        try_send_chat_message(
            &events,
            1,
            ChatMessage {
                id: "live-1".to_owned(),
                author: "viewer".to_owned(),
                text: "bounded".to_owned(),
                timestamp_ms: None,
                author_slug: None,
                badges: Vec::new(),
                reward: None,
            },
            &dropped,
        );

        assert_eq!(dropped.load(Ordering::Acquire), 1);
    }

    #[test]
    fn idle_live_worker_stops_without_a_network_request() {
        let (finished, receiver) = mpsc::channel();
        let worker_thread = thread::spawn(move || {
            let worker = LiveWorker::new().expect("worker thread starts");
            drop(worker);
            let _ = finished.send(());
        });
        receiver
            .recv_timeout(Duration::from_secs(3))
            .expect("idle worker must process shutdown and join");
        worker_thread.join().unwrap();
    }

    #[test]
    fn history_retry_is_available_only_after_failure() {
        assert!(chat_history_retry_available(ChatHistoryState::Failed(
            LiveError::Api(KickApiError::HttpStatus(403))
        )));
        assert!(!chat_history_retry_available(ChatHistoryState::Loading));
        assert!(!chat_history_retry_available(ChatHistoryState::NoChannel));
    }

    #[test]
    fn cancelled_history_retry_cannot_replace_a_newer_loading_state() {
        let channel_generation = 7;
        let first_request_generation = 12;
        let second_request_generation = 13;

        assert!(!history_result_is_current(
            channel_generation,
            channel_generation,
            first_request_generation,
            second_request_generation,
        ));
        assert!(history_result_is_current(
            channel_generation,
            channel_generation,
            second_request_generation,
            second_request_generation,
        ));
        assert!(!history_result_is_current(
            channel_generation,
            channel_generation + 1,
            second_request_generation,
            second_request_generation,
        ));
    }

    #[test]
    fn player_shortcuts_route_only_without_text_or_control_focus() {
        let no_modifiers = Modifiers::none();
        assert_eq!(
            player_shortcut("space", false, false, no_modifiers),
            Some(PlayerShortcut::TogglePlayback)
        );
        assert_eq!(
            player_shortcut("m", false, false, no_modifiers),
            Some(PlayerShortcut::ToggleMute)
        );
        assert_eq!(
            player_shortcut("f", false, false, no_modifiers),
            Some(PlayerShortcut::ToggleFullscreen)
        );
        assert_eq!(player_shortcut("space", true, false, no_modifiers), None);
        assert_eq!(player_shortcut("space", false, true, no_modifiers), None);
        assert_eq!(
            player_shortcut(
                "space",
                false,
                false,
                Modifiers {
                    platform: true,
                    ..no_modifiers
                },
            ),
            None
        );
    }

    #[test]
    fn navigation_shortcuts_are_disabled_while_text_input_has_focus() {
        assert!(navigation_shortcut_allowed(false, false));
        assert!(!navigation_shortcut_allowed(true, false));
        assert!(!navigation_shortcut_allowed(false, true));
    }

    #[test]
    fn viewer_suspend_resumes_only_active_playback() {
        assert!(should_resume_after_viewer_return(
            PlayerTransportStatus::Playing
        ));
        assert!(should_resume_after_viewer_return(
            PlayerTransportStatus::Waiting
        ));
        assert!(!should_resume_after_viewer_return(
            PlayerTransportStatus::Paused
        ));
        assert!(!should_resume_after_viewer_return(
            PlayerTransportStatus::Unknown
        ));
    }

    #[test]
    fn chat_wrap_chunks_split_cjk_and_keep_latin_words() {
        assert_eq!(chat_wrap_chunks("abc def"), vec!["abc\u{00A0}", "def"]);
        assert_eq!(chat_wrap_chunks(tr("あい")), vec![tr("あ"), tr("い")]);
        assert_eq!(chat_wrap_chunks(tr("ok あ")), vec!["ok\u{00A0}", tr("あ")]);
    }

    #[test]
    fn live_end_keeps_chat_for_a_raid_but_vod_end_stops_it() {
        assert!(should_keep_chat_after_end(false));
        assert!(!should_keep_chat_after_end(true));
    }

    #[test]
    fn raid_events_are_scoped_to_the_current_chatroom() {
        assert!(raid_is_for_current_channel(42, Some(42)));
        assert!(!raid_is_for_current_channel(42, Some(99)));
        assert!(raid_is_for_current_channel(42, None));
    }

    #[test]
    fn raid_countdown_matches_official_viewer_thresholds() {
        assert_eq!(raid_countdown_seconds(0), 15);
        assert_eq!(raid_countdown_seconds(99), 15);
        assert_eq!(raid_countdown_seconds(100), 30);
        assert_eq!(raid_countdown_seconds(499), 30);
        assert_eq!(raid_countdown_seconds(500), 60);
    }

    #[test]
    fn idle_without_an_item_is_only_a_playback_end_after_an_item_loaded() {
        let idle = PlaybackSnapshot {
            state: PlayerState::Idle,
            item_status: PlayerItemStatus::Unknown,
            transport_status: PlayerTransportStatus::Unknown,
            current_time_seconds: None,
            rate: 0.0,
            volume: 1.0,
            is_muted: false,
            has_current_item: false,
            video_ready_for_display: false,
        };
        let loading = PlaybackSnapshot {
            state: PlayerState::Loading,
            ..idle
        };

        assert!(!playback_has_ended(idle, false));
        assert!(playback_has_ended(idle, true));
        assert!(!playback_has_ended(loading, true));
        assert_eq!(snapshot_status(idle), tr("配信が終了しました"));
    }

    #[test]
    fn chat_toggle_is_offered_only_when_chat_is_hidden() {
        assert_eq!(chat_toggle_action(false, true), None);
        assert_eq!(chat_toggle_action(true, true), None);
        assert_eq!(
            chat_toggle_action(false, false),
            Some(HeaderAction::ToggleChat)
        );
        assert_eq!(
            chat_toggle_action(true, false),
            Some(HeaderAction::ToggleNarrowChat)
        );
    }

    #[test]
    fn player_overlay_labels_describe_the_current_action() {
        assert_eq!(
            player_overlay_labels(false, false),
            ("⏸", tr("一時停止"), "🔊", tr("ミュート"))
        );
        assert_eq!(
            player_overlay_labels(true, true),
            ("▶", tr("再生"), "🔇", tr("ミュート解除"))
        );
    }

    #[test]
    fn clip_merge_deduplicates_by_id_and_caps_new_rows() {
        let clip = |id: &str| kick_live::ChannelClip {
            id: id.to_owned(),
            clip_url: None,
            title: id.to_owned(),
            duration_seconds: 0,
            views: None,
            thumbnail_url: None,
            created_at: None,
        };
        let mut existing = vec![clip("a"), clip("b")];
        merge_clips(
            &mut existing,
            vec![clip("b"), clip("c"), clip("d"), clip("e")],
            2,
        );
        let ids: Vec<&str> = existing.iter().map(|clip| clip.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c", "d"]);
    }

    #[test]
    fn login_input_accepts_a_bare_session_token_and_cookie_headers() {
        let token = parse_login_cookie(b"abc123").unwrap();
        assert_eq!(token.cookie_names(), vec!["session_token".to_owned()]);
        assert_eq!(parse_login_candidates(b"abc123").unwrap().len(), 1);
        assert_eq!(parse_login_candidates(b"a%2Fb").unwrap().len(), 2);

        let header = parse_login_cookie(b"session_token=abc; kick_session=def").unwrap();
        assert_eq!(header.cookie_names(), vec!["session_token".to_owned()]);
        let full_cookie =
            parse_login_cookie(b"session_token=abc; Path=/; Domain=.kick.com; Secure").unwrap();
        assert_eq!(full_cookie.cookie_names(), vec!["session_token".to_owned()]);

        let prefixed = parse_login_cookie(b"Cookie: session_token=abc").unwrap();
        assert_eq!(prefixed.cookie_names(), vec!["session_token".to_owned()]);

        assert!(parse_login_cookie(b"").is_err());
        assert!(parse_login_cookie(b"{\"session_token\":\"abc\"}").is_err());
    }

    #[test]
    fn default_quality_parses_known_caps_and_falls_back_to_automatic() {
        assert_eq!(
            parse_default_quality(Some("1080".to_owned())),
            PlayerQualityCap::P1080
        );
        assert_eq!(
            parse_default_quality(Some("720".to_owned())),
            PlayerQualityCap::P720
        );
        assert_eq!(
            parse_default_quality(Some("480".to_owned())),
            PlayerQualityCap::P480
        );
        assert_eq!(
            parse_default_quality(Some("0".to_owned())),
            PlayerQualityCap::Automatic
        );
        assert_eq!(
            parse_default_quality(Some("999".to_owned())),
            PlayerQualityCap::Automatic
        );
        assert_eq!(parse_default_quality(None), PlayerQualityCap::Automatic);
    }

    #[test]
    fn vod_positions_path_uses_the_documented_location() {
        let path = vod_positions_path().expect("HOME is set in the test environment");
        assert!(
            path.ends_with("Library/Application Support/KickRust/vod-positions-v1.txt"),
            "unexpected VOD positions path: {}",
            path.display()
        );
    }

    #[test]
    fn vod_positions_round_trip_and_reject_invalid_lines() {
        let mut positions = HashMap::new();
        positions.insert("uuid-a".to_owned(), 123.45);
        positions.insert("uuid-b".to_owned(), 0.0);
        let body = format_vod_positions(&positions);
        assert_eq!(body.lines().count(), 2);
        assert!(body.contains("uuid-a=123.5") || body.contains("uuid-a=123.4"));

        let parsed = parse_vod_positions(&body);
        assert_eq!(parsed.len(), 2);
        assert!(parsed.contains_key("uuid-a"));
        assert!(parsed.contains_key("uuid-b"));

        let parsed = parse_vod_positions(
            "good=42.0\n\nmissing-value=\nnegative=-1\nnot-a-number=abc\nempty-key=\n",
        );
        assert_eq!(parsed.get("good"), Some(&42.0));
        assert_eq!(parsed.len(), 1);
    }

    #[test]
    fn vod_positions_are_capped_at_two_hundred_entries() {
        let body = (0..250)
            .map(|index| format!("uuid-{index}={index}.0"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(parse_vod_positions(&body).len(), 200);
    }

    #[test]
    fn cancellation_interrupts_a_pending_request() {
        let runtime = RuntimeBuilder::new_current_thread()
            .enable_all()
            .build()
            .expect("test Tokio runtime");

        let result = runtime.block_on(async {
            let cancel = Arc::new(AtomicBool::new(false));
            let cancel_request = cancel.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(5)).await;
                cancel_request.store(true, Ordering::Release);
            });

            cancellable_request(
                async {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    Ok::<_, KickApiError>(())
                },
                cancel,
            )
            .await
        });

        assert_eq!(result, Err(LiveError::Cancelled));
    }
}
