import AppKit
import Foundation
import UserNotifications

/// The app's notification permission is cached here because the C bridge
/// returns synchronously. `refreshStatus` updates the cache from
/// UNUserNotificationCenter's asynchronous settings query.
private let notificationStatusLock = NSLock()
private var cachedNotificationStatus: Int32 = 0

private final class KickNotificationDelegate: NSObject, UNUserNotificationCenterDelegate {
    func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        willPresent notification: UNNotification,
        withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void
    ) {
        completionHandler([.banner, .sound])
    }

    func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        didReceive response: UNNotificationResponse,
        withCompletionHandler completionHandler: @escaping () -> Void
    ) {
        // The helper has no window. Bring the main app forward instead.
        if let appURL = NSWorkspace.shared.urlForApplication(withBundleIdentifier: "dev.codex.kick-native") {
            NSWorkspace.shared.openApplication(
                at: appURL,
                configuration: NSWorkspace.OpenConfiguration()
            )
        } else {
            NSApp.activate(ignoringOtherApps: true)
        }
        completionHandler()
    }
}

private let notificationDelegate = KickNotificationDelegate()

private func mapAuthorizationStatus(_ status: UNAuthorizationStatus) -> Int32 {
    switch status {
    case .notDetermined:
        return 0
    case .denied:
        return 1
    case .authorized:
        return 2
    case .provisional:
        return 3
    case .ephemeral:
        return 4
    @unknown default:
        return 0
    }
}

private func storeNotificationStatus(_ status: Int32) {
    notificationStatusLock.lock()
    cachedNotificationStatus = status
    notificationStatusLock.unlock()
}

private func refreshNotificationStatus() {
    UNUserNotificationCenter.current().getNotificationSettings { settings in
        storeNotificationStatus(mapAuthorizationStatus(settings.authorizationStatus))
    }
}

@_cdecl("kick_notifications_request_authorization")
public func kickNotificationsRequestAuthorization() -> Int32 {
    guard Thread.isMainThread else { return -2 }
    let center = UNUserNotificationCenter.current()
    center.delegate = notificationDelegate
    center.requestAuthorization(options: [.alert, .sound, .badge]) { _, _ in
        refreshNotificationStatus()
    }
    refreshNotificationStatus()
    return 0
}

@_cdecl("kick_notifications_status")
public func kickNotificationsStatus() -> Int32 {
    guard Thread.isMainThread else { return -2 }
    refreshNotificationStatus()
    notificationStatusLock.lock()
    let status = cachedNotificationStatus
    notificationStatusLock.unlock()
    return status
}

@_cdecl("kick_notifications_post")
public func kickNotificationsPost(
    _ title: UnsafePointer<CChar>?,
    _ body: UnsafePointer<CChar>?,
    _ slug: UnsafePointer<CChar>?
) -> Int32 {
    guard Thread.isMainThread else { return -2 }
    let title = title.map { String(cString: $0) } ?? "Kick Native"
    let body = body.map { String(cString: $0) } ?? ""
    let slug = slug.map { String(cString: $0) } ?? ""

    let content = UNMutableNotificationContent()
    content.title = title
    content.body = body
    content.sound = .default
    if !slug.isEmpty {
        content.userInfo = ["slug": slug]
    }
    let request = UNNotificationRequest(
        identifier: UUID().uuidString,
        content: content,
        trigger: nil
    )
    UNUserNotificationCenter.current().add(request)
    return 0
}

@_cdecl("kick_notifications_open_settings")
public func kickNotificationsOpenSettings() {
    guard Thread.isMainThread else { return }
    if let url = URL(string: "x-apple.systempreferences:com.apple.Notifications-Settings.extension") {
        NSWorkspace.shared.open(url)
    }
}
