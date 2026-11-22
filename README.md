> **Unofficial** — This is an unofficial client and is not affiliated with, endorsed by, or connected to Kick.
>
> **非公式** — このアプリは Kick 公式とは関係のない非公式クライアントです。

# Minamo for macOS

A lightweight native macOS client for [Kick](https://kick.com), built with Rust, GPUI, AppKit, and AVFoundation. It does **not** use Electron, Tauri, WKWebView, Chromium, or an embedded web view.

[English](#english) | [日本語](#日本語)

## Download

Apple Silicon DMG: see the [latest release](https://github.com/saipi-1129/minamo/releases/latest).

- macOS 13 or later, Apple Silicon
- The build is signed for local development but **not notarized**. If macOS blocks it, right-click the app and choose **Open**, or allow it in **System Settings → Privacy & Security**.

---

## English

### Sign in

1. Log in to [kick.com](https://kick.com) in your browser.
2. Open DevTools → **Application** → **Cookies** → `https://kick.com`.
3. Copy the **Value** of the `session_token` cookie.
4. In the app, open the account button, choose **Paste from clipboard** (クリップボードから貼り付け), then **Sign in** (ログインする).

The token is stored in the macOS Keychain. `kick_session` alone does not work. A JSON cookie export can also be imported from the same panel.

### Features

**Playback**

- Native AVPlayer video surface inside a GPUI shell; no embedded browser or web view.
- Live playback, DVR seeking, quality caps (Auto/1080p/720p/480p), subtitles (CC), picture in picture, video-only fullscreen, and theater modes.
- VOD playback with a resumed position, and clip playback.
- MetalFX Spatial upscaling while on AC power, toggled by the `FX ON` / `FX OFF` button above the seek bar.
- An experimental loopback relay refreshes the live playlist sooner and is on by default; turn it off in Settings → Video if playback is unstable.

**Chat**

- Real-time chat with emotes, badges, comment delay, channel points, and raid/host events.
- Inline native chat input in the composer (no popup): macOS input method (Japanese/English), Ctrl+A, copy/paste, undo, and Enter to send.
- The emote picker lists the channel's own emotes first and inserts the selected emote into the input.
- Failed sends show Kick's reason (slow mode, followers-only, invalid emote, rate limit, …).
- Chat popout window, message copy, unread counts, and local hiding of a user's messages.

**Browse**

- Home with recommended streams and a category overview, browse, and search.
- Followed page with live, channels, and categories tabs.
- Channel page with home, videos, clips, and about tabs.
- The sidebar followed list shows live channels only and fills up to the configured count (5/8/12/20/30/50).

**Notifications and settings**

- Stream-start notifications from Kick's realtime push, per channel; an optional LaunchAgent helper keeps notifying after the app quits.
- Settings: UI language (Japanese/English), default quality, comment delay, danmaku overlay (on/off, size, opacity), raid banner duration, and followed count.
- Keyboard shortcuts and mouse back/forward navigation.

**Sign-in**

- Sign in with the `session_token` value, a full Cookie header, or a JSON cookie export. The session is stored in the macOS Keychain and can be removed from the app.

**Updates**

- Checks GitHub Releases for a newer version and shows it in Settings → App.
- Downloads the DMG, verifies its code signature and team ID, then installs it after the app quits and relaunches.

### Not yet supported

- Subscriptions (subscribe, gift a sub, payments)
- Events tab and upcoming/scheduled streams
- Chat replies and threads
- Chat moderation (delete message, timeout, ban)
- Server-side mute and report (only local hiding is available)
- Predictions and polls
- Emote search, favorites, and recent emotes (the picker shows up to 96)
- Chat font size (only the danmaku overlay size is configurable)
- Category browse pages (the home screen shows a category overview only)
- Clip creation
- Search filters (category, language)
- Pre-send checks for slow mode and followers-only (the server rejects the send)
- Notarization
- Multiple accounts

### Keyboard and mouse

- `⌘K` — search
- `⌘,` — settings
- `Space` — play/pause
- `M` — mute
- `F` — fullscreen
- `⌘Q` — quit
- Mouse back/forward buttons — history navigation

### Requirements

- Apple Silicon Mac, macOS 13 or later.
- Rust 1.85+ with the 2024 edition.
- Xcode command-line tools, SwiftPM, and the Metal toolchain required by GPUI.

### Build

```sh
KICK_NATIVE_BUILD_ROOT=/private/tmp/kick-native-build tools/build-macos.sh check
KICK_NATIVE_BUILD_ROOT=/private/tmp/kick-native-build tools/build-macos.sh debug
```

The packaged app is written to `/private/tmp/KickNative.app`. To install it into `/Applications` and relaunch:

```sh
KICK_NATIVE_BUILD_ROOT=/private/tmp/kick-native-build tools/build-macos.sh install
```

The build uses `/private/tmp` for artifacts; the source tree stays clean.

### Architecture

- `crates/app-ui` — GPUI application shell, settings, channel pages, and player host.
- `crates/kick-live` — Kick API and realtime (Centrifugo) client.
- `crates/kick-core` — chat models and bounded buffers.
- `crates/player-macos` — Swift AVFoundation/AVPlayer bridge and native controls.

### Privacy and credentials

- No cookies, tokens, APKs, extracted APK files, or runtime traffic with secrets are committed to this repository.
- Sign in with the `session_token` value from your browser's DevTools (Application → Cookies → kick.com); a full Cookie header or JSON export also works. The session is stored in the macOS Keychain, not in the repository or settings file.
- Settings and VOD positions live under `~/Library/Application Support/KickRust/`.
- The app does not send chat messages automatically.

### Limitations

- Kick's web API and realtime protocol are not a stable public contract and may change without notice.
- Chat sending uses Kick's web session route (`POST /api/v2/messages/send/{chatroomId}`), which is not a documented public API. A message is sent only when you press Enter or the send button.
- The DMG is not notarized; Gatekeeper may require a manual Open.
- The in-app update installs into the current app location, so the app must live in a writable folder such as `/Applications`.
- Notifications require macOS permission. The background helper must be enabled in Settings and runs only while the user is logged in.
- This is a personal project and is not affiliated with or endorsed by Kick.

### License

Apache License 2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).

---

## 日本語

### 概要

Rust、GPUI、AppKit、AVFoundation で作った Kick のネイティブ macOS クライアントです。Electron、Tauri、WKWebView、Chromium、埋め込み WebView は使用していません。

### ダウンロード

Apple Silicon 用 DMG は [最新リリース](https://github.com/saipi-1129/minamo/releases/latest) から取得できます。

- macOS 13 以降、Apple Silicon
- ローカル開発用の署名のみで、**公証（notarization）は未実施**です。起動できない場合はアプリを右クリックして「開く」を選ぶか、システム設定 → プライバシーとセキュリティ で許可してください。

### ログイン

1. ブラウザで [kick.com](https://kick.com) にログイン
2. DevTools → **Application** → **Cookies** → `https://kick.com`
3. `session_token` の **Value** をコピー
4. アプリのアカウントボタン → **「クリップボードから貼り付け」** → **「ログインする」**

トークンは macOS の Keychain に保存されます。`kick_session` だけではログインできません。同じ画面から JSON の Cookie エクスポートも読み込めます。

### 主な機能

**再生**

- AVPlayer によるネイティブ映像表示。ブラウザや WebView の埋め込みなし。
- ライブ再生、DVR シーク、画質上限（自動/1080p/720p/480p）、字幕（CC）、ピクチャ・イン・ピクチャ、映像のみ全画面、劇場モード。
- VOD 再生（再生位置の復元つき）とクリップ再生。
- 充電中は MetalFX Spatial でアップスケール。シークバー上の `FX ON` / `FX OFF` で切り替え。
- 低遅延リレー（実験的）を既定で有効。プレイリストの更新を早めます。再生が不安定な場合は設定 → 映像 でオフにできます。

**チャット**

- リアルタイムチャット、エモート、バッジ、コメント遅延、チャンネルポイント、レイド/ホストイベント。
- チャット欄内のネイティブ入力欄（ポップアップなし）。日本語/英語入力、Ctrl+A、コピー/貼り付け、取り消し、Enter 送信に対応。
- エモート一覧は配信者固有のエモートを先頭に表示し、選ぶと入力欄に挿入されます。
- 送信に失敗した場合は Kick の拒否理由（スローモード、フォロワー限定、無効なエモート、レート制限など）を表示。
- チャットの別ウィンドウ表示、メッセージのコピー、未読数表示、特定ユーザーのローカル非表示。

**視聴・探索**

- ホーム（おすすめ配信、カテゴリ概要）、ブラウズ、検索。
- フォロー中ページ（ライブ / チャンネル / カテゴリ）。
- チャンネルページ（ホーム / 配信 / クリップ / 概要）。
- 左のフォロー中は配信中のチャンネルのみ表示し、設定した件数（5/8/12/20/30/50件）まで自動で読み込みます。

**通知・設定**

- Kick のリアルタイム push による配信開始通知（チャンネルごとにオン/オフ）。アプリ終了後も通知する場合は LaunchAgent ヘルパーを有効化。
- 設定: 表示言語（日本語/英語）、デフォルト画質、コメント遅延、流れるコメント（オン/オフ・サイズ・透明度）、レイド表示時間、フォロー表示数。
- キーボードショートカット、マウスの戻る/進む。

**ログイン**

- `session_token` の値、Cookie ヘッダー、JSON の Cookie エクスポートからログインできます。セッションは macOS の Keychain に保存し、アプリから削除できます。

**アップデート**

- GitHub Releases で新しいバージョンを確認し、設定 → アプリ に表示します。
- DMG をダウンロードし、コード署名と Team ID を検証してから、アプリ終了後にインストールして再起動します。

### 未対応の機能

- サブスクリプション（登録・ギフトサブ・支払い）
- イベント一覧、予定されている配信
- チャットの返信・スレッド
- チャットのモデレーション（メッセージ削除、タイムアウト、BAN）
- サーバー側のミュート・通報（ローカル非表示のみ）
- 予測（Predictions）・投票（Polls）
- エモートの検索・お気に入り・最近使った（一覧は最大96件）
- チャット本文の文字サイズ（流れるコメントのサイズのみ設定可能）
- カテゴリ別のブラウズページ（ホームのカテゴリ概要のみ）
- クリップの作成
- 検索の絞り込み（カテゴリ・言語）
- スローモード/フォロワー限定の送信前チェック（送信後にサーバーが拒否）
- 公証（notarization）
- 複数アカウントの切り替え

### キーボードとマウス

- `⌘K` — 検索
- `⌘,` — 設定
- `Space` — 再生/一時停止
- `M` — ミュート
- `F` — 全画面
- `⌘Q` — 終了
- マウスの戻る/進むボタン — 履歴の移動

### 必要環境

- Apple Silicon Mac、macOS 13 以降
- Rust 1.85 以降（2024 edition）
- Xcode コマンドラインツール、SwiftPM、GPUI が必要とする Metal ツールチェーン

### ビルド

```sh
KICK_NATIVE_BUILD_ROOT=/private/tmp/kick-native-build tools/build-macos.sh check
KICK_NATIVE_BUILD_ROOT=/private/tmp/kick-native-build tools/build-macos.sh debug
```

生成物は `/private/tmp/KickNative.app` です。`/Applications` にインストールして再起動する場合:

```sh
KICK_NATIVE_BUILD_ROOT=/private/tmp/kick-native-build tools/build-macos.sh install
```

### 構成

- `crates/app-ui` — GPUI のアプリシェル、設定、チャンネルページ、プレイヤーホスト
- `crates/kick-live` — Kick API とリアルタイム（Centrifugo）クライアント
- `crates/kick-core` — チャットモデルと上限付きバッファ
- `crates/player-macos` — Swift AVFoundation/AVPlayer ブリッジとネイティブコントロール

### 認証情報とプライバシー

- Cookie、トークン、APK、APK 展開物、秘密情報を含む通信ログはリポジトリに含めていません。
- ログインはブラウザの DevTools（Application → Cookies → kick.com）にある `session_token` の値だけで行えます。Cookie ヘッダーや JSON エクスポートも使用できます。セッションは macOS の Keychain に保存し、リポジトリや設定ファイルには書きません。
- 設定と VOD の再生位置は `~/Library/Application Support/KickRust/` に保存されます。
- チャットメッセージを自動送信することはありません。

### 制限事項

- Kick の Web API とリアルタイムプロトコルは安定した公開仕様ではなく、予告なく変更される可能性があります。
- チャット送信は Kick の Web セッション経路（`POST /api/v2/messages/send/{chatroomId}`）を使用しており、公開 API ではありません。Enter か送信ボタンを押したときだけ送信します。
- DMG は公証されていないため、Gatekeeper の警告が出る場合があります。
- アプリ内アップデートは現在のアプリの場所にインストールするため、`/Applications` など書き込み可能な場所に置く必要があります。
- 通知には macOS の許可が必要です。バックグラウンド通知ヘルパーは設定で有効化した場合のみ、ログイン中に動作します。
- 個人プロジェクトであり、Kick とは提携・承認関係にありません。

### ライセンス

Apache License 2.0。[LICENSE](LICENSE) と [NOTICE](NOTICE) を参照してください。
