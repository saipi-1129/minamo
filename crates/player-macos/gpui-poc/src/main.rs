use std::{
    fs::{self, OpenOptions},
    io::Write,
    ptr::NonNull,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use gpui::{
    App, Bounds, Context, KeyDownEvent, Render, Window, WindowBounds, WindowOptions, div,
    prelude::*, px, rgb, size,
};
use gpui_platform::application;
use kick_player_macos::{Player, PlayerItemStatus, PlayerTransportStatus};
use objc2_app_kit::NSView;
use objc2_foundation::{NSPoint, NSRect, NSSize};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

const APPLE_HLS_SAMPLE: &str = "https://devstreaming-cdn.apple.com/videos/streaming/examples/bipbop_16x9/bipbop_16x9_variant.m3u8";
const LOG_PATH: &str = "/tmp/kick-gpui-avplayer-poc.log";
const PLAYBACK_TIMEOUT: Duration = Duration::from_secs(30);

fn write_log(message: impl AsRef<str>) {
    let elapsed_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(LOG_PATH) {
        let _ = writeln!(file, "{elapsed_ms} {}", message.as_ref());
        let _ = file.flush();
    }
}

struct PlayerHost {
    player: Option<Player>,
    attached: bool,
    monitor_started: bool,
    started_at: Instant,
    status: String,
    did_log_ready: bool,
    did_log_playing: bool,
    did_log_video_ready: bool,
    did_log_timeout: bool,
    previous_frame: Option<[f64; 4]>,
}

impl PlayerHost {
    fn new() -> Self {
        match Player::new() {
            Ok(player) => {
                write_log("PLAYER HANDLE PASS main-thread C ABI created");
                Self {
                    player: Some(player),
                    attached: false,
                    monitor_started: false,
                    started_at: Instant::now(),
                    status: "Waiting for GPUI native window".to_owned(),
                    did_log_ready: false,
                    did_log_playing: false,
                    did_log_video_ready: false,
                    did_log_timeout: false,
                    previous_frame: None,
                }
            }
            Err(error) => {
                write_log(format!("PLAYER HANDLE FAIL {error:?}"));
                Self {
                    player: None,
                    attached: false,
                    monitor_started: false,
                    started_at: Instant::now(),
                    status: format!("Player creation failed: {error:?}"),
                    did_log_ready: false,
                    did_log_playing: false,
                    did_log_video_ready: false,
                    did_log_timeout: false,
                    previous_frame: None,
                }
            }
        }
    }

    fn sync_native_view(&mut self, window: &Window) {
        let Some(player) = self.player.as_ref() else {
            return;
        };

        let handle = match HasWindowHandle::window_handle(window) {
            Ok(handle) => handle,
            Err(error) => {
                self.status = format!("GPUI window handle unavailable: {error}");
                write_log(format!("WINDOW HANDLE FAIL {error}"));
                return;
            }
        };
        let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
            self.status = "GPUI window is not backed by AppKit".to_owned();
            write_log("WINDOW HANDLE FAIL backend is not AppKit");
            return;
        };

        let native_view = match player.ns_view() {
            Ok(view) => view,
            Err(error) => {
                self.status = format!("Player surface unavailable: {error:?}");
                write_log(format!("PLAYER SURFACE FAIL {error:?}"));
                return;
            }
        };
        let child_pointer = unsafe { native_view.as_ptr() };
        let Some(child_pointer) = NonNull::new(child_pointer) else {
            self.status = "Player returned a null NSView".to_owned();
            write_log("PLAYER SURFACE FAIL null NSView");
            return;
        };

        let parent = unsafe { &*appkit.ns_view.as_ptr().cast::<NSView>() };
        let child = unsafe { &*child_pointer.as_ptr().cast::<NSView>() };
        let parent_bounds = parent.bounds();
        let frame = video_frame(parent_bounds, parent.isFlipped());
        child.setFrame(frame);

        if !self.attached {
            parent.addSubview(child);
            self.attached = true;

            let same_window = match (parent.window(), child.window()) {
                (Some(parent_window), Some(child_window)) => {
                    std::ptr::eq::<objc2_app_kit::NSWindow>(
                        &*parent_window,
                        &*child_window,
                    )
                }
                _ => false,
            };
            write_log(format!(
                "ATTACH {} parent_flipped={} parent_bounds={}x{} frame=({},{} {}x{})",
                if same_window { "PASS same_window=true" } else { "FAIL same_window=false" },
                parent.isFlipped(),
                parent_bounds.size.width,
                parent_bounds.size.height,
                frame.origin.x,
                frame.origin.y,
                frame.size.width,
                frame.size.height
            ));

            match player.play_hls(APPLE_HLS_SAMPLE, false) {
                Ok(()) => {
                    self.status = "Apple HLS sample requested".to_owned();
                    write_log("PLAY REQUEST PASS fixture=Apple HLS Bip Bop; URL omitted");
                }
                Err(error) => {
                    self.status = format!("Sample play request failed: {error:?}");
                    write_log(format!("PLAY REQUEST FAIL {error:?}"));
                }
            }
        }

        let frame_values = [frame.origin.x, frame.origin.y, frame.size.width, frame.size.height];
        if self.previous_frame.is_none_or(|previous| {
            previous
                .iter()
                .zip(frame_values)
                .any(|(old, new)| (old - new).abs() > 0.5)
        }) {
            write_log(format!(
                "FRAME ({:.1},{:.1} {:.1}x{:.1})",
                frame.origin.x, frame.origin.y, frame.size.width, frame.size.height
            ));
            self.previous_frame = Some(frame_values);
        }
    }

    fn start_monitor(&mut self, cx: &mut Context<Self>) {
        if self.monitor_started || self.player.is_none() {
            return;
        }
        self.monitor_started = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                let update = this.update(cx, |view, cx| view.record_snapshot(cx));
                if update.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn record_snapshot(&mut self, cx: &mut Context<Self>) {
        let Some(player) = self.player.as_ref() else {
            return;
        };
        let snapshot = match player.snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.status = format!("Player snapshot failed: {error:?}");
                write_log(format!("SNAPSHOT FAIL {error:?}"));
                cx.notify();
                return;
            }
        };

        let mut changed = false;
        let elapsed = self.started_at.elapsed();
        if !self.did_log_ready && snapshot.item_status == PlayerItemStatus::ReadyToPlay {
            self.did_log_ready = true;
            changed = true;
            write_log("READY PASS item=readyToPlay");
        }
        if !self.did_log_playing && snapshot.transport_status == PlayerTransportStatus::Playing {
            self.did_log_playing = true;
            changed = true;
            write_log(format!(
                "PLAYING PASS rate={:.2} time={:.2}s",
                snapshot.rate,
                snapshot.current_time_seconds.unwrap_or_default()
            ));
        }
        if !self.did_log_video_ready && snapshot.video_ready_for_display {
            self.did_log_video_ready = true;
            changed = true;
            write_log("VIDEO READY PASS AVPlayerLayer.isReadyForDisplay=true");
        }
        if snapshot.item_status == PlayerItemStatus::Failed {
            self.status = "AVPlayer reported a failed item".to_owned();
            write_log("PLAYBACK FAIL item=failed");
            changed = true;
        }
        if !self.did_log_timeout && elapsed >= PLAYBACK_TIMEOUT && !self.did_log_video_ready {
            self.did_log_timeout = true;
            changed = true;
            write_log("VIDEO READY FAIL timeout after 30s");
        }

        self.status = format!(
            "GPUI child view attached · item {:?} · transport {:?} · {:.2}s · first frame {} · press Q to detach",
            snapshot.item_status,
            snapshot.transport_status,
            snapshot.current_time_seconds.unwrap_or_default(),
            if snapshot.video_ready_for_display { "ready" } else { "waiting" }
        );
        if changed {
            cx.notify();
        }
    }
}

impl Drop for PlayerHost {
    fn drop(&mut self) {
        if self.attached {
            if let Some(player) = self.player.as_ref() {
                if let Ok(native_view) = player.ns_view() {
                    if let Some(pointer) = NonNull::new(unsafe { native_view.as_ptr() }) {
                        let child = unsafe { &*pointer.as_ptr().cast::<NSView>() };
                        child.removeFromSuperview();
                        write_log("DETACH PASS on GPUI view drop, before Player drop");
                    }
                }
            }
        }
        write_log("PLAYER DROP follows; Rust wrapper destroys Swift session");
    }
}

impl Render for PlayerHost {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_native_view(window);
        self.start_monitor(cx);

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x111214))
            .text_color(rgb(0xE6E7E9))
            .child(
                div()
                    .h(px(44.0))
                    .px_4()
                    .flex()
                    .items_center()
                    .text_size(px(13.0))
                    .child("GPUI window + AVPlayerLayer child NSView · Apple HLS sample"),
            )
            .child(div().flex_1())
            .child(
                div()
                    .h(px(48.0))
                    .px_4()
                    .flex()
                    .items_center()
                    .text_size(px(12.0))
                    .text_color(rgb(0x53FC18))
                    .child(self.status.clone()),
            )
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                if event.is_held || !event.keystroke.key.eq_ignore_ascii_case("q") {
                    return;
                }
                this.detach_and_shutdown(cx);
            }))
    }
}

impl PlayerHost {
    fn detach_and_shutdown(&mut self, cx: &mut Context<Self>) {
        if self.attached {
            if let Some(player) = self.player.as_ref() {
                if let Ok(native_view) = player.ns_view() {
                    if let Some(pointer) = NonNull::new(unsafe { native_view.as_ptr() }) {
                        let child = unsafe { &*pointer.as_ptr().cast::<NSView>() };
                        child.removeFromSuperview();
                    }
                }
            }
            self.attached = false;
            write_log("DETACH PASS key=Q");
        }
        if let Some(player) = self.player.take() {
            drop(player);
            write_log("PLAYER SHUTDOWN PASS key=Q; Swift session destroyed");
        }
        self.status = "Detached and shut down · close this window to finish".to_owned();
        cx.notify();
    }
}

fn video_frame(bounds: NSRect, is_flipped: bool) -> NSRect {
    let horizontal_margin = 32.0;
    let vertical_margin = 32.0;
    let footer_space = 84.0;
    let available_width = (bounds.size.width - horizontal_margin * 2.0).max(1.0);
    let available_height = (bounds.size.height - vertical_margin - footer_space).max(1.0);
    let width = available_width.min(available_height * 16.0 / 9.0);
    let height = width * 9.0 / 16.0;
    let x = bounds.origin.x + (bounds.size.width - width) / 2.0;
    let y = if is_flipped {
        bounds.origin.y + vertical_margin
    } else {
        bounds.origin.y + bounds.size.height - height - vertical_margin
    };
    NSRect::new(NSPoint::new(x, y), NSSize::new(width, height))
}

fn main() {
    let _ = fs::remove_file(LOG_PATH);
    write_log("START fixture=Apple HLS Bip Bop; no Kick cookies or user session");

    application().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1060.0), px(680.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_window, cx| cx.new(|_| PlayerHost::new()),
        )
        .expect("open GPUI AVPlayer child-view PoC window");
        cx.activate(true);
    });
}
