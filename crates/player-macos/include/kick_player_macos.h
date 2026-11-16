#ifndef KICK_PLAYER_MACOS_H
#define KICK_PLAYER_MACOS_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

enum kick_player_state {
    KICK_PLAYER_IDLE = 0,
    KICK_PLAYER_LOADING = 1,
    KICK_PLAYER_READY = 2,
    KICK_PLAYER_PLAYBACK_REQUESTED = 3,
    KICK_PLAYER_PAUSED = 4,
    KICK_PLAYER_FAILED = 5,
    KICK_PLAYER_PLAYING = 6
};

enum kick_player_item_status {
    KICK_PLAYER_ITEM_UNKNOWN = 0,
    KICK_PLAYER_ITEM_READY = 1,
    KICK_PLAYER_ITEM_FAILED = 2
};

enum kick_player_transport_status {
    KICK_PLAYER_TRANSPORT_PAUSED = 0,
    KICK_PLAYER_TRANSPORT_WAITING = 1,
    KICK_PLAYER_TRANSPORT_PLAYING = 2,
    KICK_PLAYER_TRANSPORT_UNKNOWN = 3
};

/* Seeks the current item to an absolute position in seconds. */
int32_t kick_player_seek(void *handle, double seconds);
/* Adds a scrolling comment above the video. */
int32_t kick_player_push_danmaku(void *handle, const char *text_utf8);
/* Sets the danmaku font size in points and opacity in 0...1. */
int32_t kick_player_set_danmaku_config(void *handle, double font_size, double opacity);
/* Applies a quality cap (0 automatic, 1080, 720, 480). */
int32_t kick_player_set_quality(void *handle, int32_t cap);
/* Marks the current item as a VOD/clip (1) or live (0) so the native overlay
 * labels the seek bar as a playback position instead of a live DVR window. */
int32_t kick_player_set_vod_mode(void *handle, int32_t enabled);
/* Returns the active quality cap (0, 1080, 720, 480), -1 for a null handle,
 * or -2 off the main thread. */
int32_t kick_player_quality_state(void *handle);
/* Selects a legible track. 0 turns subtitles off; 1...count selects one.
 * Returns 0 on acceptance, -1 for a null handle, -2 off the main thread,
 * -3 for an invalid index or no legible tracks, -4 without a current item,
 * or -5 after shutdown. */
int32_t kick_player_set_subtitle(void *handle, int32_t index);
/* Copies the legible-track count and the selected index (0 = off).
 * Returns 0, -1 for a null handle, -2 off-main, or -3 for a null output. */
int32_t kick_player_subtitle_state(void *handle, int32_t *out_count, int32_t *out_selected);
/* Starts (1) or stops (0) picture in picture. Returns 0 on acceptance,
 * -1 for a null handle, -2 off-main, -3 when unsupported, -4 without a
 * current item, or -5 after shutdown. Starting is asynchronous. */
int32_t kick_player_set_pip(void *handle, int32_t enabled);
/* Returns 1 while picture in picture is active, 0 otherwise,
 * -1 for a null handle, or -2 off-main. */
int32_t kick_player_pip_state(void *handle);
/* All functions must be called from the AppKit main thread. */
/* create returns 0 on success, -1 for a null output pointer, or -2 off the main thread. */
int32_t kick_player_create(void **out_handle);
/* play_hls returns 0 on acceptance, -1 for null input, -2 off-main, -3 invalid URL,
 * -4 after shutdown, or -5 for another bridge error. start_at_live_edge is 0 or 1;
 * 1 seeks to the end of the seekable range before starting playback (DVR EVENT). */
int32_t kick_player_play_hls(void *handle, const char *url_utf8, int32_t start_at_live_edge);
/* play_hls_pair starts a live/DVR pair. live_url_utf8 is required; dvr_url_utf8 may be
 * NULL or empty. The live URL is the low-latency source and the DVR URL is used when
 * seeking backward. Returns the same codes as kick_player_play_hls. */
int32_t kick_player_play_hls_pair(
    void *handle,
    const char *live_url_utf8,
    const char *dvr_url_utf8,
    int32_t start_at_live_edge
);
int32_t kick_player_pause(void *handle);
/* resume resumes the existing item without reloading a URL: 0 accepted, -1 null handle,
 * -2 off-main, -3 no current item, -4 after shutdown, or -5 for another bridge error. */
int32_t kick_player_resume(void *handle);
int32_t kick_player_set_volume(void *handle, float volume);
int32_t kick_player_set_muted(void *handle, int32_t muted);
int32_t kick_player_state(void *handle);
/* Snapshot copies URL-free fields. All output pointers are required. Returns 0, -1 null
 * handle, -2 off-main, or -3 for a null output pointer. has_current_time indicates whether
 * current_time_seconds is meaningful. Boolean outputs are 0 or 1. */
int32_t kick_player_snapshot(
    void *handle,
    int32_t *out_state,
    int32_t *out_item_status,
    int32_t *out_transport_status,
    double *out_current_time_seconds,
    int32_t *out_has_current_time,
    float *out_rate,
    float *out_volume,
    int32_t *out_is_muted,
    int32_t *out_has_current_item,
    int32_t *out_video_ready_for_display
);
/* Borrowed NSView *, valid until kick_player_destroy(handle). */
void *kick_player_nsview(void *handle);
/* destroy returns 0 on success, -1 for a null handle, or -2 off-main. */
int32_t kick_player_destroy(void *handle);

/* Cookie credential bridge. Cookie bytes are always caller-owned and must be wiped by the caller.
 * The native byte buffer returned by prompt/load must be released only with
 * kick_credentials_free_bytes; that function wipes the buffer before freeing it. */
enum kick_credentials_result {
    KICK_CREDENTIALS_OK = 0,
    KICK_CREDENTIALS_CANCELLED_OR_NOT_FOUND = 1,
    KICK_CREDENTIALS_INVALID_ARGUMENT = -1,
    KICK_CREDENTIALS_WRONG_THREAD = -2,
    KICK_CREDENTIALS_INVALID_COOKIE = -3,
    KICK_CREDENTIALS_COOKIE_TOO_LARGE = -4,
    KICK_CREDENTIALS_KEYCHAIN_FAILURE = -5,
    KICK_CREDENTIALS_ALLOCATION_FAILURE = -6,
    KICK_CREDENTIALS_PROMPT_ALREADY_OPEN = -7
};

typedef void (*kick_credentials_prompt_callback)(
    void *context,
    int32_t status,
    uint8_t *bytes,
    size_t length
);
/* Requests macOS notification permission. Must be called on the AppKit main thread. */
int32_t kick_notifications_request_authorization(void);
/* Returns 0 not determined, 1 denied, 2 authorized, 3 provisional, 4 ephemeral. */
int32_t kick_notifications_status(void);
/* Posts a local notification with an optional channel slug. Main thread only. */
int32_t kick_notifications_post(
    const char *title_utf8,
    const char *body_utf8,
    const char *slug_utf8
);
/* Opens System Settings > Notifications. */
void kick_notifications_open_settings(void);

/* Begins a masked, nonmodal NSSecureTextField window on the AppKit main thread and returns
 * immediately. Completion runs exactly once on that thread; it does not store the cookie. */
int32_t kick_credentials_begin_prompt_cookie(
    kick_credentials_prompt_callback callback,
    void *context
);
/* Load returns 1 if no app-owned cookie item exists. out_keychain_status is required and
 * receives the numeric OSStatus only when KEYCHAIN_FAILURE is returned. */
int32_t kick_credentials_load_cookie(
    uint8_t **out_bytes,
    size_t *out_length,
    int32_t *out_keychain_status
);
/* Save validates bounds/control bytes and updates only this service/account item. */
int32_t kick_credentials_save_cookie(
    const uint8_t *bytes,
    size_t length,
    int32_t *out_keychain_status
);
/* Delete is idempotent when the app-owned item is absent. */
int32_t kick_credentials_delete_cookie(int32_t *out_keychain_status);
/* Frees a buffer returned by prompt/load after wiping it. */
void kick_credentials_free_bytes(uint8_t *bytes, size_t length);

#ifdef __cplusplus
}
#endif

#endif
