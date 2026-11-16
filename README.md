> **Unofficial** — This is an unofficial client and is not affiliated with, endorsed by, or connected to Kick.
>
> **非公式** — このアプリは Kick 公式とは関係のない非公式クライアントです。

# Kick Native for macOS

A lightweight native macOS client for [Kick](https://kick.com), built with Rust, GPUI, AppKit, and AVFoundation. It does **not** use Electron, Tauri, WKWebView, Chromium, or an embedded web view.

[English](#english) | [日本語](#日本語)

## Download

Apple Silicon DMG: see the [latest release](https://github.com/saipi-1129/kick-native/releases/latest).

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

- Native AVPlayer video surface with a GPUI shell; no embedded browser or web view.
- Live playback, DVR seeking, quality caps, subtitles (CC), picture in picture, fullscreen and theater modes.
- The latency policy stays enabled by default. The old "low latency" toggle was removed because the label was misleading.

**Chat**

- Real-time chat with emotes, badges, comment delay, channel points, and raid/host events.
- Inline native chat input inside the composer (no popup): macOS input method (Japanese/English), Ctrl+A, copy/paste, undo, and Enter to send.
- The emote picker lists the channel's own emotes first, and picking one inserts it into the input.
- Failed sends show Kick's reason (slow mode, followers-only, invalid emote, rate limit, …).
- Chat popout window, message copy, and unread counts.

**Browse**

- Channel pages, clips with pagination, VOD resume, search, and followed channels.
- The sidebar followed list shows live channels only and fills up to the configured count (5/8/12/20/30/50).

**Notifications and settings**

- Stream-start notifications through Kick's `channel.{id}` realtime push, with an optional headless LaunchAgent helper for notifications after the app quits.
- Settings tabs for general, video, notifications, and app preferences: UI language (Japanese/English), default quality, comment delay, danmaku overlay (on/off, size, opacity), raid banner duration, and followed count.
- Keyboard shortcuts, mouse back/forward navigation, and `⌘Q` to quit.

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
- The followed sidebar lists live channels only; offline followed channels are hidden.
- Subscription and payment APIs are not implemented.
- The DMG is not notarized; Gatekeeper may require a manual Open.
- Notifications require macOS permission. The background helper must be enabled in Settings and runs only while the user is logged in.
- This is a personal project and is not affiliated with or endorsed by Kick.

### License

Apache License 2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).

---

## 日本語

### 概要

Rust、GPUI、AppKit、AVFoundation で作った Kick のネイティブ macOS クライアントです。Electron、Tauri、WKWebView、Chromium、埋め込み WebView は使用していません。

### ダウンロード

Apple Silicon 用 DMG は [最新リリース](https://github.com/saipi-1129/kick-native/releases/latest) から取得できます。

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
- ライブ再生、DVR シーク、画質上限、字幕（CC）、ピクチャ・イン・ピクチャ、全画面・劇場モード。
- 遅延ポリシーは常時オン。誤解を招く「低遅延」表記と切り替えは削除しました。

**チャット**

- リアルタイムチャット、エモート、バッジ、コメント遅延、チャンネルポイント、レイド/ホストイベント。
- チャット欄内のネイティブ入力欄（ポップアップなし）。日本語/英語入力、Ctrl+A、コピー/貼り付け、取り消し、Enter 送信に対応。
- エモート一覧は配信者固有のエモートを先頭に表示し、選ぶと入力欄に挿入されます。
- 送信に失敗した場合は Kick の拒否理由（スローモード、フォロワー限定、無効なエモート、レート制限など）を表示。
- チャットの別ウィンドウ表示、メッセージのコピー、未読数表示。

**視聴**

- チャンネルページ、クリップの追加読み込み、VOD の再生位置復元、検索、フォロー中チャンネル。
- 左のフォロー中は配信中のチャンネルのみ表示し、設定した件数（5/8/12/20/30/50件）まで自動で読み込みます。

**通知と設定**

- Kick の `channel.{id}` リアルタイム push による配信開始通知。アプリ終了後も通知する場合は LaunchAgent ヘルパーを設定から有効化。
- 設定を「一般 / 映像 / 通知 / アプリ」のタブに分類。表示言語（日本語/英語）、デフォルト画質、コメント遅延、流れるコメント（オン/オフ・サイズ・透明度）、レイド表示時間、フォロー表示数を変更可能。
- キーボードショートカット、マウスの戻る/進む、`⌘Q` での終了。

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
- 左のフォロー中は配信中のチャンネルのみ表示し、オフラインのチャンネルは表示されません。
- サブスクリプション/支払い API は未実装です。
- DMG は公証されていないため、Gatekeeper の警告が出る場合があります。
- 通知には macOS の許可が必要です。バックグラウンド通知ヘルパーは設定で有効化した場合のみ、ログイン中に動作します。
- 個人プロジェクトであり、Kick とは提携・承認関係にありません。

### ライセンス

Apache License 2.0。[LICENSE](LICENSE) と [NOTICE](NOTICE) を参照してください。
