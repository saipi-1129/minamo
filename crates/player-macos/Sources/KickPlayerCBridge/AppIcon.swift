import AppKit

private var appearanceIconObserver: NSObjectProtocol?
private var activationIconObserver: NSObjectProtocol?

@MainActor private func systemPrefersDark() -> Bool {
    // `AppleInterfaceStyle` lives in the global domain, not the app domain.
    let global = UserDefaults.standard.persistentDomain(forName: UserDefaults.globalDomain)
    if (global?["AppleInterfaceStyle"] as? String) == "Dark" {
        return true
    }
    return NSApp.effectiveAppearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
}

@MainActor private func applyAppearanceIcon() {
    let dark = systemPrefersDark()
    let name = dark ? "AppIcon-dark" : "AppIcon-light"
    guard let url = Bundle.main.url(forResource: name, withExtension: "png"),
          let image = NSImage(contentsOf: url) else {
        return
    }
    NSApp.applicationIconImage = image
}

/// Swaps the Dock icon between the light and dark artwork when the system
/// appearance changes. `NSApp.applicationIconImage` overrides the bundle icon
/// for the running process without touching the installed bundle.
@_cdecl("kick_apply_appearance_icon")
public func kickApplyAppearanceIcon() {
    guard Thread.isMainThread else { return }
    MainActor.assumeIsolated {
        if appearanceIconObserver == nil {
            appearanceIconObserver = DistributedNotificationCenter.default.addObserver(
                forName: Notification.Name("AppleInterfaceThemeChangedNotification"),
                object: nil,
                queue: .main
            ) { _ in
                MainActor.assumeIsolated {
                    applyAppearanceIcon()
                }
            }
        }
        if activationIconObserver == nil {
            // AppKit can report the default appearance until the app becomes
            // active, so re-apply once activation settles.
            activationIconObserver = NotificationCenter.default.addObserver(
                forName: NSApplication.didBecomeActiveNotification,
                object: nil,
                queue: .main
            ) { _ in
                MainActor.assumeIsolated {
                    applyAppearanceIcon()
                }
            }
        }
        applyAppearanceIcon()
    }
}
