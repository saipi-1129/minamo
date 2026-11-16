import AppKit
import Foundation
import KickPlayerCore

@MainActor
private final class PlayerHandle {
    let session = PlayerSession()
}

private func onMainActor<T>(_ body: @MainActor () -> T) -> T? {
    guard Thread.isMainThread else { return nil }
    return MainActor.assumeIsolated(body)
}

/// Creates a player handle. Returns -1 for a missing output pointer or -2 off the main thread.
@_cdecl("kick_player_create")
public func kickPlayerCreate(_ outHandle: UnsafeMutablePointer<UnsafeMutableRawPointer?>?) -> Int32 {
    guard let outHandle else { return -1 }
    outHandle.pointee = nil
    guard Thread.isMainThread else { return -2 }

    guard let handle = onMainActor({ Unmanaged.passRetained(PlayerHandle()).toOpaque() }) else {
        return -2
    }
    outHandle.pointee = handle
    return 0
}

/// Starts an HTTP(S) playlist URL. Returns 0 on acceptance, -1 for a null handle,
/// -2 when called off the main thread, -3 for an invalid URL, and -4 after shutdown.
/// `startAtLiveEdge` is 0 or 1; 1 seeks to the end of the seekable range before playback.
@_cdecl("kick_player_play_hls")
public func kickPlayerPlayHLS(
    _ rawHandle: UnsafeMutableRawPointer?,
    _ urlCString: UnsafePointer<CChar>?,
    _ startAtLiveEdge: Int32
) -> Int32 {
    guard let rawHandle, let urlCString else { return -1 }
    guard Thread.isMainThread else { return -2 }

    return onMainActor {
        let handle = Unmanaged<PlayerHandle>.fromOpaque(rawHandle).takeUnretainedValue()
        do {
            try handle.session.playHLS(
                String(cString: urlCString),
                startAtLiveEdge: startAtLiveEdge != 0
            )
            return 0
        } catch KickPlayerError.sessionClosed {
            return -4
        } catch KickPlayerError.invalidURL {
            return -3
        } catch {
            return -5
        }
    } ?? -2
}

/// Starts a live/DVR pair. The live URL is the low-latency source; the DVR URL may be
/// empty or null and is used when the user seeks backward. Returns the same codes as
/// `kickPlayerPlayHLS`.
@_cdecl("kick_player_play_hls_pair")
public func kickPlayerPlayHLSPair(
    _ rawHandle: UnsafeMutableRawPointer?,
    _ liveURLCString: UnsafePointer<CChar>?,
    _ dvrURLCString: UnsafePointer<CChar>?,
    _ startAtLiveEdge: Int32
) -> Int32 {
    guard let rawHandle, let liveURLCString else { return -1 }
    guard Thread.isMainThread else { return -2 }

    return onMainActor {
        let handle = Unmanaged<PlayerHandle>.fromOpaque(rawHandle).takeUnretainedValue()
        let dvrURL = dvrURLCString.map { String(cString: $0) } ?? ""
        do {
            try handle.session.playHLSPair(
                liveURL: String(cString: liveURLCString),
                dvrURL: dvrURL,
                startAtLiveEdge: startAtLiveEdge != 0
            )
            return 0
        } catch KickPlayerError.sessionClosed {
            return -4
        } catch KickPlayerError.invalidURL {
            return -3
        } catch {
            return -5
        }
    } ?? -2
}

/// Pauses playback. Returns 0 on success, -1 for a null handle, or -2 off the main thread.
@_cdecl("kick_player_pause")
public func kickPlayerPause(_ rawHandle: UnsafeMutableRawPointer?) -> Int32 {
    guard let rawHandle else { return -1 }
    guard Thread.isMainThread else { return -2 }

    return onMainActor {
        Unmanaged<PlayerHandle>.fromOpaque(rawHandle).takeUnretainedValue().session.pause()
        return 0
    } ?? -2
}

/// Resumes the current item without reloading its URL. Returns 0 on acceptance,
/// -1 for a null handle, -2 off the main thread, -3 without a current item,
/// or -4 after shutdown.
@_cdecl("kick_player_resume")
public func kickPlayerResume(_ rawHandle: UnsafeMutableRawPointer?) -> Int32 {
    guard let rawHandle else { return -1 }
    guard Thread.isMainThread else { return -2 }

    return onMainActor {
        let handle = Unmanaged<PlayerHandle>.fromOpaque(rawHandle).takeUnretainedValue()
        do {
            try handle.session.resume()
            return 0
        } catch KickPlayerError.noCurrentItem {
            return -3
        } catch KickPlayerError.sessionClosed {
            return -4
        } catch {
            return -5
        }
    } ?? -2
}

/// Sets per-player volume in the inclusive range 0...1. Values are clamped.
@_cdecl("kick_player_set_volume")
public func kickPlayerSetVolume(_ rawHandle: UnsafeMutableRawPointer?, _ volume: Float) -> Int32 {
    guard let rawHandle else { return -1 }
    guard Thread.isMainThread else { return -2 }
    guard volume.isFinite else { return -3 }

    return onMainActor {
        Unmanaged<PlayerHandle>.fromOpaque(rawHandle).takeUnretainedValue().session.setVolume(volume)
        return 0
    } ?? -2
}

/// Sets per-player mute state. Returns 0 on success, -1 for a null handle, or -2 off the main thread.
@_cdecl("kick_player_set_muted")
public func kickPlayerSetMuted(_ rawHandle: UnsafeMutableRawPointer?, _ muted: Int32) -> Int32 {
    guard let rawHandle else { return -1 }
    guard Thread.isMainThread else { return -2 }

    return onMainActor {
        Unmanaged<PlayerHandle>.fromOpaque(rawHandle).takeUnretainedValue().session.setMuted(muted != 0)
        return 0
    } ?? -2
}

/// Seeks the current item to an absolute position in seconds.
@_cdecl("kick_player_seek")
public func kickPlayerSeek(_ rawHandle: UnsafeMutableRawPointer?, _ seconds: Double) -> Int32 {
    guard let rawHandle else { return -1 }
    guard Thread.isMainThread else { return -2 }

    return onMainActor {
        Unmanaged<PlayerHandle>.fromOpaque(rawHandle).takeUnretainedValue().session.seek(toSeconds: seconds)
        return 0
    } ?? -2
}

/// Adds a scrolling comment above the video.
@_cdecl("kick_player_push_danmaku")
public func kickPlayerPushDanmaku(_ rawHandle: UnsafeMutableRawPointer?, _ text: UnsafePointer<CChar>?) -> Int32 {
    guard let rawHandle else { return -1 }
    guard Thread.isMainThread else { return -2 }
    guard let text, let value = String(validatingUTF8: text) else { return -3 }

    return onMainActor {
        Unmanaged<PlayerHandle>.fromOpaque(rawHandle).takeUnretainedValue().session.pushDanmaku(value)
        return 0
    } ?? -2
}

/// Sets the danmaku font size (pt) and opacity (0...1).
@_cdecl("kick_player_set_danmaku_config")
public func kickPlayerSetDanmakuConfig(_ rawHandle: UnsafeMutableRawPointer?, _ fontSize: Double, _ opacity: Double) -> Int32 {
    guard let rawHandle else { return -1 }
    guard Thread.isMainThread else { return -2 }

    return onMainActor {
        Unmanaged<PlayerHandle>.fromOpaque(rawHandle).takeUnretainedValue().session.setDanmakuConfig(
            fontSize: CGFloat(fontSize),
            opacity: CGFloat(opacity)
        )
        return 0
    } ?? -2
}

/// Applies a quality cap: 0 automatic, 1080/720/480 for the resolution caps.
@_cdecl("kick_player_set_quality")
public func kickPlayerSetQuality(_ rawHandle: UnsafeMutableRawPointer?, _ cap: Int32) -> Int32 {
    guard let rawHandle else { return -1 }
    guard Thread.isMainThread else { return -2 }
    guard let quality = PlayerQualityCap(rawValue: Int(cap)) else { return -3 }

    return onMainActor {
        Unmanaged<PlayerHandle>.fromOpaque(rawHandle).takeUnretainedValue().session.setQualityCap(quality)
        return 0
    } ?? -2
}

/// Returns the active quality cap (0, 1080, 720, 480).
@_cdecl("kick_player_quality_state")
public func kickPlayerQualityState(_ rawHandle: UnsafeMutableRawPointer?) -> Int32 {
    guard let rawHandle else { return -1 }
    guard Thread.isMainThread else { return -2 }

    return onMainActor {
        let session = Unmanaged<PlayerHandle>.fromOpaque(rawHandle).takeUnretainedValue().session
        return Int32(session.qualityCap.rawValue)
    } ?? -2
}

/// Selects the native overlay language (0 Japanese, 1 English).
@_cdecl("kick_player_set_language")
public func kickPlayerSetLanguage(_ rawHandle: UnsafeMutableRawPointer?, _ language: Int32) -> Int32 {
    guard let rawHandle else { return -1 }
    guard Thread.isMainThread else { return -2 }

    return onMainActor {
        Unmanaged<PlayerHandle>.fromOpaque(rawHandle).takeUnretainedValue().session.setLanguage(
            PlayerLanguage(rawValue: language) ?? .japanese
        )
        return 0
    } ?? -2
}

/// Marks the current item as a VOD/clip (1) or live (0).
@_cdecl("kick_player_set_vod_mode")
public func kickPlayerSetVodMode(_ rawHandle: UnsafeMutableRawPointer?, _ enabled: Int32) -> Int32 {
    guard let rawHandle else { return -1 }
    guard Thread.isMainThread else { return -2 }

    return onMainActor {
        Unmanaged<PlayerHandle>.fromOpaque(rawHandle).takeUnretainedValue().session.setVodMode(enabled != 0)
        return 0
    } ?? -2
}

/// Selects a legible track: 0 turns subtitles off, 1...count selects one.
/// Returns 0 on acceptance, -1 for a null handle, -2 off-main, -3 for an
/// invalid index or no legible tracks, -4 without a current item, or -5 after shutdown.
@_cdecl("kick_player_set_subtitle")
public func kickPlayerSetSubtitle(_ rawHandle: UnsafeMutableRawPointer?, _ index: Int32) -> Int32 {
    guard let rawHandle else { return -1 }
    guard Thread.isMainThread else { return -2 }

    return onMainActor {
        let session = Unmanaged<PlayerHandle>.fromOpaque(rawHandle).takeUnretainedValue().session
        guard !session.isClosed else { return -5 }
        guard session.currentSnapshot().hasCurrentItem else { return -4 }
        return session.setSubtitle(index: Int(index)) ? 0 : -3
    } ?? -2
}

/// Copies the legible-track count and the selected index (0 = off).
@_cdecl("kick_player_subtitle_state")
public func kickPlayerSubtitleState(
    _ rawHandle: UnsafeMutableRawPointer?,
    _ outCount: UnsafeMutablePointer<Int32>?,
    _ outSelected: UnsafeMutablePointer<Int32>?
) -> Int32 {
    guard let rawHandle else { return -1 }
    guard let outCount, let outSelected else { return -3 }
    guard Thread.isMainThread else { return -2 }

    return onMainActor {
        let session = Unmanaged<PlayerHandle>.fromOpaque(rawHandle).takeUnretainedValue().session
        outCount.pointee = Int32(session.subtitleTrackTitles.count)
        outSelected.pointee = Int32(session.selectedSubtitleTrackIndex)
        return 0
    } ?? -2
}

/// Starts (1) or stops (0) picture in picture. Starting is asynchronous.
@_cdecl("kick_player_set_pip")
public func kickPlayerSetPictureInPicture(
    _ rawHandle: UnsafeMutableRawPointer?,
    _ enabled: Int32
) -> Int32 {
    guard let rawHandle else { return -1 }
    guard Thread.isMainThread else { return -2 }

    return onMainActor {
        let session = Unmanaged<PlayerHandle>.fromOpaque(rawHandle).takeUnretainedValue().session
        guard !session.isClosed else { return -5 }
        guard session.currentSnapshot().hasCurrentItem else { return -4 }
        guard session.isPictureInPictureSupported else { return -3 }
        session.setPictureInPicture(enabled: enabled != 0)
        return 0
    } ?? -2
}

/// Returns 1 while picture in picture is active, 0 otherwise.
@_cdecl("kick_player_pip_state")
public func kickPlayerPictureInPictureState(_ rawHandle: UnsafeMutableRawPointer?) -> Int32 {
    guard let rawHandle else { return -1 }
    guard Thread.isMainThread else { return -2 }

    return onMainActor {
        let session = Unmanaged<PlayerHandle>.fromOpaque(rawHandle).takeUnretainedValue().session
        return session.isPictureInPictureActive ? 1 : 0
    } ?? -2
}

/// Returns the numeric KickPlayerState or -1 for a null handle / -2 off the main thread.
/// States are idle 0, loading 1, ready 2, playback requested 3, paused 4, failed 5, playing 6.
@_cdecl("kick_player_state")
public func kickPlayerState(_ rawHandle: UnsafeMutableRawPointer?) -> Int32 {
    guard let rawHandle else { return -1 }
    guard Thread.isMainThread else { return -2 }

    return onMainActor {
        Unmanaged<PlayerHandle>.fromOpaque(rawHandle).takeUnretainedValue().session.stateCode
    } ?? -2
}

/// Copies a URL-free typed snapshot to caller-provided storage. Every output pointer is required.
/// Returns 0 on success, -1 for a null handle, -2 off the main thread, or -3 for a null output.
@_cdecl("kick_player_snapshot")
public func kickPlayerSnapshot(
    _ rawHandle: UnsafeMutableRawPointer?,
    _ outState: UnsafeMutablePointer<Int32>?,
    _ outItemStatus: UnsafeMutablePointer<Int32>?,
    _ outTransportStatus: UnsafeMutablePointer<Int32>?,
    _ outCurrentTimeSeconds: UnsafeMutablePointer<Double>?,
    _ outHasCurrentTime: UnsafeMutablePointer<Int32>?,
    _ outRate: UnsafeMutablePointer<Float>?,
    _ outVolume: UnsafeMutablePointer<Float>?,
    _ outIsMuted: UnsafeMutablePointer<Int32>?,
    _ outHasCurrentItem: UnsafeMutablePointer<Int32>?,
    _ outVideoReadyForDisplay: UnsafeMutablePointer<Int32>?
) -> Int32 {
    guard let rawHandle else { return -1 }
    guard
        let outState,
        let outItemStatus,
        let outTransportStatus,
        let outCurrentTimeSeconds,
        let outHasCurrentTime,
        let outRate,
        let outVolume,
        let outIsMuted,
        let outHasCurrentItem,
        let outVideoReadyForDisplay
    else { return -3 }
    guard Thread.isMainThread else { return -2 }

    return onMainActor {
        let handle = Unmanaged<PlayerHandle>.fromOpaque(rawHandle).takeUnretainedValue()
        let snapshot = handle.session.currentSnapshot()
        outState.pointee = handle.session.stateCode
        outItemStatus.pointee = snapshot.itemStatusCode
        outTransportStatus.pointee = snapshot.transportStatusCode
        outCurrentTimeSeconds.pointee = snapshot.currentTimeSeconds ?? 0
        outHasCurrentTime.pointee = snapshot.currentTimeSeconds == nil ? 0 : 1
        outRate.pointee = snapshot.rate
        outVolume.pointee = snapshot.volume
        outIsMuted.pointee = snapshot.isMuted ? 1 : 0
        outHasCurrentItem.pointee = snapshot.hasCurrentItem ? 1 : 0
        outVideoReadyForDisplay.pointee = snapshot.videoReadyForDisplay ? 1 : 0
        return 0
    } ?? -2
}

/// Returns a borrowed AppKit NSView pointer, valid until kick_player_destroy.
@_cdecl("kick_player_nsview")
public func kickPlayerNSView(_ rawHandle: UnsafeMutableRawPointer?) -> UnsafeMutableRawPointer? {
    guard let rawHandle else { return nil }
    return onMainActor {
        let handle = Unmanaged<PlayerHandle>.fromOpaque(rawHandle).takeUnretainedValue()
        return Unmanaged.passUnretained(handle.session.surface).toOpaque()
    }
}

/// Stops playback, removes the current item, and releases the retained handle.
/// Returns 0 on success, -1 for a null handle, or -2 off the main thread.
@_cdecl("kick_player_destroy")
public func kickPlayerDestroy(_ rawHandle: UnsafeMutableRawPointer?) -> Int32 {
    guard let rawHandle else { return -1 }
    guard Thread.isMainThread else { return -2 }

    return onMainActor {
        let handle = Unmanaged<PlayerHandle>.fromOpaque(rawHandle).takeRetainedValue()
        handle.session.shutdown()
        return 0
    } ?? -2
}
