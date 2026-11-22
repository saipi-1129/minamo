//! Thin Rust ownership wrapper for the Swift AVPlayer C ABI.
//!
//! Construct, use, and drop `Player` on the AppKit main thread. The native surface
//! pointer is borrowed and remains valid only while the `Player` handle is alive.

use std::ffi::{CString, NulError, c_char, c_void};
use std::marker::PhantomData;
use std::ptr::NonNull;
use std::rc::Rc;

pub mod chat_input;
pub mod credentials;

#[link(name = "KickPlayerCBridge", kind = "dylib")]
unsafe extern "C" {
    fn kick_player_create(out_handle: *mut *mut c_void) -> i32;
    fn kick_player_play_hls(
        handle: *mut c_void,
        url_utf8: *const c_char,
        start_at_live_edge: i32,
    ) -> i32;
    fn kick_player_play_hls_pair(
        handle: *mut c_void,
        live_url_utf8: *const c_char,
        dvr_url_utf8: *const c_char,
        start_at_live_edge: i32,
    ) -> i32;
    fn kick_player_pause(handle: *mut c_void) -> i32;
    fn kick_player_resume(handle: *mut c_void) -> i32;
    fn kick_player_set_volume(handle: *mut c_void, volume: f32) -> i32;
    fn kick_player_set_muted(handle: *mut c_void, muted: i32) -> i32;
    fn kick_player_set_quality(handle: *mut c_void, cap: i32) -> i32;
    fn kick_player_set_vod_mode(handle: *mut c_void, enabled: i32) -> i32;
    fn kick_player_set_language(handle: *mut c_void, language: i32) -> i32;
    fn kick_player_set_upscaling(handle: *mut c_void, enabled: i32) -> i32;
    fn kick_player_upscaling_active(handle: *mut c_void) -> i32;
    fn kick_upscaling_toggle_take() -> i32;
    fn kick_power_is_ac() -> i32;
    fn kick_apply_appearance_icon();
    fn kick_video_only_fullscreen_take() -> i32;
    fn kick_notifications_request_authorization() -> i32;
    fn kick_notifications_status() -> i32;
    fn kick_notifications_post(
        title_utf8: *const c_char,
        body_utf8: *const c_char,
        slug_utf8: *const c_char,
    ) -> i32;
    fn kick_notifications_open_settings();
    fn kick_player_quality_state(handle: *mut c_void) -> i32;
    fn kick_player_set_subtitle(handle: *mut c_void, index: i32) -> i32;
    fn kick_player_subtitle_state(
        handle: *mut c_void,
        out_count: *mut i32,
        out_selected: *mut i32,
    ) -> i32;
    fn kick_player_set_pip(handle: *mut c_void, enabled: i32) -> i32;
    fn kick_player_pip_state(handle: *mut c_void) -> i32;
    fn kick_player_seek(handle: *mut c_void, seconds: f64) -> i32;
    fn kick_player_push_danmaku(handle: *mut c_void, text_utf8: *const c_char) -> i32;
    fn kick_player_set_danmaku_config(handle: *mut c_void, font_size: f64, opacity: f64) -> i32;
    fn kick_player_state(handle: *mut c_void) -> i32;
    fn kick_player_snapshot(
        handle: *mut c_void,
        out_state: *mut i32,
        out_item_status: *mut i32,
        out_transport_status: *mut i32,
        out_current_time_seconds: *mut f64,
        out_has_current_time: *mut i32,
        out_rate: *mut f32,
        out_volume: *mut f32,
        out_is_muted: *mut i32,
        out_has_current_item: *mut i32,
        out_video_ready_for_display: *mut i32,
    ) -> i32;
    fn kick_player_nsview(handle: *mut c_void) -> *mut c_void;
    fn kick_player_destroy(handle: *mut c_void) -> i32;
}

#[derive(Debug, PartialEq, Eq)]
pub enum PlayerError {
    BridgeUnavailable,
    InvalidURL,
    SessionClosed,
    NoCurrentItem,
    WrongThread,
    InvalidArgument,
    InvalidSnapshot,
    Unsupported,
}

impl From<NulError> for PlayerError {
    fn from(_: NulError) -> Self {
        Self::InvalidArgument
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayerState {
    Idle,
    Loading,
    Ready,
    PlaybackRequested,
    Paused,
    Failed,
    Playing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayerItemStatus {
    Unknown,
    ReadyToPlay,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayerTransportStatus {
    Paused,
    Waiting,
    Playing,
    Unknown,
}

/// Requests macOS notification permission. The system prompt is asynchronous.
pub fn request_notification_authorization() -> Result<(), PlayerError> {
    match unsafe { kick_notifications_request_authorization() } {
        0 => Ok(()),
        -2 => Err(PlayerError::WrongThread),
        _ => Err(PlayerError::InvalidArgument),
    }
}

/// Returns the last observed authorization status: 0 not determined, 1 denied,
/// 2 authorized, 3 provisional, 4 ephemeral.
pub fn notification_status() -> i32 {
    unsafe { kick_notifications_status() }
}

/// Posts one local notification. The slug is retained for click-through.
pub fn post_notification(title: &str, body: &str, slug: &str) -> Result<(), PlayerError> {
    let title = CString::new(title).map_err(|_| PlayerError::InvalidArgument)?;
    let body = CString::new(body).map_err(|_| PlayerError::InvalidArgument)?;
    let slug = CString::new(slug).map_err(|_| PlayerError::InvalidArgument)?;
    match unsafe { kick_notifications_post(title.as_ptr(), body.as_ptr(), slug.as_ptr()) } {
        0 => Ok(()),
        -2 => Err(PlayerError::WrongThread),
        _ => Err(PlayerError::InvalidArgument),
    }
}

/// Opens System Settings > Notifications.
pub fn open_notification_settings() {
    unsafe { kick_notifications_open_settings() }
}

/// Resolution caps understood by the native player bridge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PlayerQualityCap {
    #[default]
    Automatic,
    P1080,
    P720,
    P480,
}

impl PlayerQualityCap {
    pub fn raw_value(self) -> i32 {
        match self {
            Self::Automatic => 0,
            Self::P1080 => 1080,
            Self::P720 => 720,
            Self::P480 => 480,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Automatic => "自動",
            Self::P1080 => "1080p",
            Self::P720 => "720p",
            Self::P480 => "480p",
        }
    }

    pub fn from_raw(value: i32) -> Option<Self> {
        match value {
            0 => Some(Self::Automatic),
            1080 => Some(Self::P1080),
            720 => Some(Self::P720),
            480 => Some(Self::P480),
            _ => None,
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Automatic => Self::P1080,
            Self::P1080 => Self::P720,
            Self::P720 => Self::P480,
            Self::P480 => Self::Automatic,
        }
    }
}

/// URL-free state copied from AVPlayer on the AppKit main thread.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlaybackSnapshot {
    pub state: PlayerState,
    pub item_status: PlayerItemStatus,
    pub transport_status: PlayerTransportStatus,
    pub current_time_seconds: Option<f64>,
    pub rate: f32,
    pub volume: f32,
    pub is_muted: bool,
    pub has_current_item: bool,
    pub video_ready_for_display: bool,
}

/// Borrowed native view; the lifetime prevents it outliving its owning `Player` in safe Rust.
pub struct NativeView<'a> {
    pointer: NonNull<c_void>,
    _owner: PhantomData<&'a Player>,
}

impl NativeView<'_> {
    /// Exposes the borrowed pointer for a native host-view integration.
    ///
    /// # Safety
    /// The caller must not retain or use this pointer after the `NativeView` or its `Player`
    /// has been dropped, and must use it only on the AppKit main thread.
    pub unsafe fn as_ptr(&self) -> *mut c_void {
        self.pointer.as_ptr()
    }
}

/// One native AVPlayer session. This type is deliberately `!Send` and `!Sync`.
pub struct Player {
    handle: NonNull<c_void>,
    _main_thread_affinity: PhantomData<Rc<()>>,
}

/// Returns true once after the native overlay asks for the video-only view.
pub fn take_video_only_fullscreen_request() -> bool {
    unsafe { kick_video_only_fullscreen_take() == 1 }
}

/// Applies the light or dark Dock icon for the current system appearance and
/// keeps it in sync when the appearance changes. Call from the AppKit main thread.
/// Returns the overlay toggle result: `Some(true)` on, `Some(false)` off,
/// `None` when the user did not touch the MetalFX button.
pub fn take_upscaling_toggle() -> Option<bool> {
    match unsafe { kick_upscaling_toggle_take() } {
        1 => Some(true),
        0 => Some(false),
        _ => None,
    }
}

/// Whether the Mac is currently running from the power adapter.
pub fn is_on_ac_power() -> bool {
    unsafe { kick_power_is_ac() == 1 }
}

pub fn apply_appearance_icon() {
    unsafe { kick_apply_appearance_icon() };
}

impl Player {
    /// Creates the native session. Must be called on the AppKit main thread.
    pub fn new() -> Result<Self, PlayerError> {
        let mut raw_handle = std::ptr::null_mut();
        match unsafe { kick_player_create(&mut raw_handle) } {
            0 => {}
            -2 => return Err(PlayerError::WrongThread),
            _ => return Err(PlayerError::BridgeUnavailable),
        }
        let handle = NonNull::new(raw_handle).ok_or(PlayerError::BridgeUnavailable)?;
        Ok(Self {
            handle,
            _main_thread_affinity: PhantomData,
        })
    }

    /// Queues an HTTP(S) HLS playlist for playback. When `start_at_live_edge` is true,
    /// the native player seeks to the end of the seekable range before starting playback.
    pub fn play_hls(&self, url: &str, start_at_live_edge: bool) -> Result<(), PlayerError> {
        let url = CString::new(url)?;
        match unsafe {
            kick_player_play_hls(
                self.handle.as_ptr(),
                url.as_ptr(),
                i32::from(start_at_live_edge),
            )
        } {
            0 => Ok(()),
            -2 => Err(PlayerError::WrongThread),
            -3 => Err(PlayerError::InvalidURL),
            -4 => Err(PlayerError::SessionClosed),
            _ => Err(PlayerError::InvalidArgument),
        }
    }

    /// Queues a live/DVR HLS pair. The live URL is the low-latency source; the DVR
    /// URL may be empty and is used when the user seeks backward.
    pub fn play_hls_pair(
        &self,
        live_url: &str,
        dvr_url: &str,
        start_at_live_edge: bool,
    ) -> Result<(), PlayerError> {
        let live_url = CString::new(live_url)?;
        let dvr_url = CString::new(dvr_url)?;
        match unsafe {
            kick_player_play_hls_pair(
                self.handle.as_ptr(),
                live_url.as_ptr(),
                dvr_url.as_ptr(),
                i32::from(start_at_live_edge),
            )
        } {
            0 => Ok(()),
            -2 => Err(PlayerError::WrongThread),
            -3 => Err(PlayerError::InvalidURL),
            -4 => Err(PlayerError::SessionClosed),
            _ => Err(PlayerError::InvalidArgument),
        }
    }

    pub fn pause(&self) -> Result<(), PlayerError> {
        match unsafe { kick_player_pause(self.handle.as_ptr()) } {
            0 => Ok(()),
            -2 => Err(PlayerError::WrongThread),
            _ => Err(PlayerError::InvalidArgument),
        }
    }

    /// Resumes the currently loaded item without reloading its URL.
    pub fn resume(&self) -> Result<(), PlayerError> {
        match unsafe { kick_player_resume(self.handle.as_ptr()) } {
            0 => Ok(()),
            -1 => Err(PlayerError::InvalidArgument),
            -2 => Err(PlayerError::WrongThread),
            -3 => Err(PlayerError::NoCurrentItem),
            -4 => Err(PlayerError::SessionClosed),
            _ => Err(PlayerError::InvalidArgument),
        }
    }

    /// Sets per-player volume; values outside 0...1 are clamped by AVPlayer bridge code.
    pub fn set_volume(&self, volume: f32) -> Result<(), PlayerError> {
        match unsafe { kick_player_set_volume(self.handle.as_ptr(), volume) } {
            0 => Ok(()),
            -2 => Err(PlayerError::WrongThread),
            -3 => Err(PlayerError::InvalidArgument),
            _ => Err(PlayerError::InvalidArgument),
        }
    }

    /// Seeks the current item to an absolute position in seconds.
    pub fn seek(&self, seconds: f64) -> Result<(), PlayerError> {
        if !seconds.is_finite() || seconds < 0.0 {
            return Err(PlayerError::InvalidArgument);
        }
        match unsafe { kick_player_seek(self.handle.as_ptr(), seconds) } {
            0 => Ok(()),
            -2 => Err(PlayerError::WrongThread),
            _ => Err(PlayerError::InvalidArgument),
        }
    }

    /// Adds a scrolling comment above the video.
    pub fn push_danmaku(&self, text: &str) -> Result<(), PlayerError> {
        let Ok(text) = std::ffi::CString::new(text) else {
            return Err(PlayerError::InvalidArgument);
        };
        match unsafe { kick_player_push_danmaku(self.handle.as_ptr(), text.as_ptr()) } {
            0 => Ok(()),
            -2 => Err(PlayerError::WrongThread),
            _ => Err(PlayerError::InvalidArgument),
        }
    }

    /// Sets the danmaku font size in points and opacity in 0...1.
    pub fn set_danmaku_config(&self, font_size: f32, opacity: f32) -> Result<(), PlayerError> {
        match unsafe {
            kick_player_set_danmaku_config(
                self.handle.as_ptr(),
                f64::from(font_size),
                f64::from(opacity),
            )
        } {
            0 => Ok(()),
            -2 => Err(PlayerError::WrongThread),
            _ => Err(PlayerError::InvalidArgument),
        }
    }

    /// Applies a quality cap. AVPlayer treats the resolution and bit-rate
    /// values as preferences, so the result depends on the stream's renditions.
    pub fn set_quality(&self, cap: PlayerQualityCap) -> Result<(), PlayerError> {
        match unsafe { kick_player_set_quality(self.handle.as_ptr(), cap.raw_value()) } {
            0 => Ok(()),
            -2 => Err(PlayerError::WrongThread),
            -3 => Err(PlayerError::InvalidArgument),
            _ => Err(PlayerError::InvalidArgument),
        }
    }

    /// Returns the active quality cap, including changes made in the native popup.
    pub fn quality_state(&self) -> Result<PlayerQualityCap, PlayerError> {
        match unsafe { kick_player_quality_state(self.handle.as_ptr()) } {
            -1 => Err(PlayerError::InvalidArgument),
            -2 => Err(PlayerError::WrongThread),
            value => PlayerQualityCap::from_raw(value).ok_or(PlayerError::InvalidSnapshot),
        }
    }

    /// Whether MetalFX upscaling is rendering right now.
    pub fn upscaling_active(&self) -> bool {
        unsafe { kick_player_upscaling_active(self.handle.as_ptr()) == 1 }
    }

    /// Enables MetalFX spatial upscaling while the Mac is on AC power.
    pub fn set_upscaling(&self, enabled: bool) -> Result<(), PlayerError> {
        match unsafe { kick_player_set_upscaling(self.handle.as_ptr(), i32::from(enabled)) } {
            0 => Ok(()),
            -2 => Err(PlayerError::WrongThread),
            _ => Err(PlayerError::InvalidArgument),
        }
    }

    /// Marks the current item as a VOD/clip (true) or live (false).
    pub fn set_language(&self, english: bool) -> Result<(), PlayerError> {
        match unsafe { kick_player_set_language(self.handle.as_ptr(), i32::from(english)) } {
            0 => Ok(()),
            -2 => Err(PlayerError::WrongThread),
            _ => Err(PlayerError::InvalidArgument),
        }
    }

    pub fn set_vod_mode(&self, enabled: bool) -> Result<(), PlayerError> {
        match unsafe { kick_player_set_vod_mode(self.handle.as_ptr(), i32::from(enabled)) } {
            0 => Ok(()),
            -2 => Err(PlayerError::WrongThread),
            _ => Err(PlayerError::InvalidArgument),
        }
    }

    /// Selects a legible track. 0 turns subtitles off; 1...count selects one.
    pub fn set_subtitle(&self, index: i32) -> Result<(), PlayerError> {
        match unsafe { kick_player_set_subtitle(self.handle.as_ptr(), index) } {
            0 => Ok(()),
            -2 => Err(PlayerError::WrongThread),
            -3 => Err(PlayerError::InvalidArgument),
            -4 => Err(PlayerError::NoCurrentItem),
            -5 => Err(PlayerError::SessionClosed),
            _ => Err(PlayerError::InvalidArgument),
        }
    }

    /// Returns the legible-track count and the selected index (0 = off).
    pub fn subtitle_state(&self) -> Result<(i32, i32), PlayerError> {
        let mut count = 0;
        let mut selected = 0;
        match unsafe { kick_player_subtitle_state(self.handle.as_ptr(), &mut count, &mut selected) }
        {
            0 => Ok((count, selected)),
            -2 => Err(PlayerError::WrongThread),
            _ => Err(PlayerError::InvalidArgument),
        }
    }

    /// Starts or stops native picture in picture. Starting is asynchronous.
    pub fn set_picture_in_picture(&self, enabled: bool) -> Result<(), PlayerError> {
        match unsafe { kick_player_set_pip(self.handle.as_ptr(), i32::from(enabled)) } {
            0 => Ok(()),
            -2 => Err(PlayerError::WrongThread),
            -3 => Err(PlayerError::Unsupported),
            -4 => Err(PlayerError::NoCurrentItem),
            -5 => Err(PlayerError::SessionClosed),
            _ => Err(PlayerError::InvalidArgument),
        }
    }

    /// Reports whether picture in picture is currently active.
    pub fn picture_in_picture_state(&self) -> Result<bool, PlayerError> {
        match unsafe { kick_player_pip_state(self.handle.as_ptr()) } {
            0 => Ok(false),
            1 => Ok(true),
            -2 => Err(PlayerError::WrongThread),
            _ => Err(PlayerError::InvalidArgument),
        }
    }

    pub fn set_muted(&self, muted: bool) -> Result<(), PlayerError> {
        match unsafe { kick_player_set_muted(self.handle.as_ptr(), i32::from(muted)) } {
            0 => Ok(()),
            -2 => Err(PlayerError::WrongThread),
            _ => Err(PlayerError::InvalidArgument),
        }
    }

    /// Returns idle 0, loading 1, ready 2, playback requested 3, paused 4, failed 5, playing 6.
    /// Call from the AppKit main thread.
    pub fn state(&self) -> Result<i32, PlayerError> {
        match unsafe { kick_player_state(self.handle.as_ptr()) } {
            -2 => Err(PlayerError::WrongThread),
            -1 => Err(PlayerError::InvalidArgument),
            state => Ok(state),
        }
    }

    /// Reads a typed, URL-free snapshot. Call on the AppKit main thread.
    pub fn snapshot(&self) -> Result<PlaybackSnapshot, PlayerError> {
        let mut state = 0;
        let mut item_status = 0;
        let mut transport_status = 0;
        let mut current_time_seconds = 0.0;
        let mut has_current_time = 0;
        let mut rate = 0.0;
        let mut volume = 0.0;
        let mut is_muted = 0;
        let mut has_current_item = 0;
        let mut video_ready_for_display = 0;

        let result = unsafe {
            kick_player_snapshot(
                self.handle.as_ptr(),
                &mut state,
                &mut item_status,
                &mut transport_status,
                &mut current_time_seconds,
                &mut has_current_time,
                &mut rate,
                &mut volume,
                &mut is_muted,
                &mut has_current_item,
                &mut video_ready_for_display,
            )
        };

        match result {
            -1 => return Err(PlayerError::InvalidArgument),
            -2 => return Err(PlayerError::WrongThread),
            0 => {}
            _ => return Err(PlayerError::InvalidSnapshot),
        }

        let state = match state {
            0 => PlayerState::Idle,
            1 => PlayerState::Loading,
            2 => PlayerState::Ready,
            3 => PlayerState::PlaybackRequested,
            4 => PlayerState::Paused,
            5 => PlayerState::Failed,
            6 => PlayerState::Playing,
            _ => return Err(PlayerError::InvalidSnapshot),
        };
        let item_status = match item_status {
            0 => PlayerItemStatus::Unknown,
            1 => PlayerItemStatus::ReadyToPlay,
            2 => PlayerItemStatus::Failed,
            _ => return Err(PlayerError::InvalidSnapshot),
        };
        let transport_status = match transport_status {
            0 => PlayerTransportStatus::Paused,
            1 => PlayerTransportStatus::Waiting,
            2 => PlayerTransportStatus::Playing,
            3 => PlayerTransportStatus::Unknown,
            _ => return Err(PlayerError::InvalidSnapshot),
        };
        let bool_value = |value| match value {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(PlayerError::InvalidSnapshot),
        };
        let current_time_seconds = match bool_value(has_current_time)? {
            true if current_time_seconds.is_finite() => Some(current_time_seconds),
            true => return Err(PlayerError::InvalidSnapshot),
            false => None,
        };
        if !rate.is_finite() || !volume.is_finite() || !(0.0..=1.0).contains(&volume) {
            return Err(PlayerError::InvalidSnapshot);
        }

        Ok(PlaybackSnapshot {
            state,
            item_status,
            transport_status,
            current_time_seconds,
            rate,
            volume,
            is_muted: bool_value(is_muted)?,
            has_current_item: bool_value(has_current_item)?,
            video_ready_for_display: bool_value(video_ready_for_display)?,
        })
    }

    /// Returns a borrowed `NSView *`; it must not be released by the caller.
    /// The returned wrapper is tied to this `Player`'s borrow lifetime.
    pub fn ns_view(&self) -> Result<NativeView<'_>, PlayerError> {
        let view = unsafe { kick_player_nsview(self.handle.as_ptr()) };
        let pointer = NonNull::new(view).ok_or(PlayerError::WrongThread)?;
        Ok(NativeView {
            pointer,
            _owner: PhantomData,
        })
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        // Drop on the same AppKit main thread that created the handle.
        unsafe {
            let _ = kick_player_destroy(self.handle.as_ptr());
        }
    }
}
