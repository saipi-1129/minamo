import AppKit
import Darwin
import Foundation
import Security
import UniformTypeIdentifiers

private enum CredentialBridgeStatus {
    static let ok: Int32 = 0
    static let cancelledOrNotFound: Int32 = 1
    static let invalidArgument: Int32 = -1
    static let wrongThread: Int32 = -2
    static let invalidCookie: Int32 = -3
    static let tooLarge: Int32 = -4
    static let keychainFailure: Int32 = -5
    static let allocationFailure: Int32 = -6
    static let promptAlreadyOpen: Int32 = -7
}

private let maximumCookieLength = 8 * 1024
private let cookieKeychainService = "dev.codex.kick-native.session-cookie"
private let cookieKeychainAccount = "default"
public typealias KickCookiePromptCallback = @convention(c) (
    UnsafeMutableRawPointer?, Int32, UnsafeMutablePointer<UInt8>?, UInt
) -> Void

@MainActor private var activeCookiePrompt: CookiePromptController?

private func onCredentialsMainActor<T>(_ body: @MainActor () -> T) -> T? {
    guard Thread.isMainThread else { return nil }
    return MainActor.assumeIsolated(body)
}

private func isValidNativeCookieBytes(_ bytes: UnsafeBufferPointer<UInt8>) -> Bool {
    guard !bytes.isEmpty, bytes.count <= maximumCookieLength else { return false }
    return bytes.allSatisfy { byte in
        byte >= 0x20 && byte <= 0x7e
    }
}

private func wipe(_ data: inout Data) {
    guard !data.isEmpty else { return }
    data.withUnsafeMutableBytes { bytes in
        guard let baseAddress = bytes.baseAddress else { return }
        _ = memset_s(baseAddress, bytes.count, 0, bytes.count)
    }
}

private func copyToNativeBuffer(
    _ data: inout Data,
    outBytes: UnsafeMutablePointer<UnsafeMutablePointer<UInt8>?>,
    outLength: UnsafeMutablePointer<UInt>
) -> Int32 {
    guard !data.isEmpty else { return CredentialBridgeStatus.invalidCookie }
    guard data.count <= maximumCookieLength else { return CredentialBridgeStatus.tooLarge }

    let byteCount = data.count
    guard let allocation = malloc(byteCount) else {
        return CredentialBridgeStatus.allocationFailure
    }
    let buffer = allocation.assumingMemoryBound(to: UInt8.self)
    data.copyBytes(to: buffer, count: byteCount)
    wipe(&data)

    outBytes.pointee = buffer
    outLength.pointee = UInt(byteCount)
    return CredentialBridgeStatus.ok
}

private func makeKeychainQuery() -> [String: Any] {
    [
        kSecClass as String: kSecClassGenericPassword,
        kSecAttrService as String: cookieKeychainService,
        kSecAttrAccount as String: cookieKeychainAccount,
        kSecAttrSynchronizable as String: kCFBooleanFalse as Any,
    ]
}

/// Owns a separate, nonmodal native window. Using a nested modal event loop while a GPUI entity is
/// borrowed can reenter GPUI callbacks; completion therefore always arrives asynchronously.
@MainActor private final class CookiePromptController: NSObject, NSWindowDelegate {
    private let callback: KickCookiePromptCallback
    private let context: UnsafeMutableRawPointer
    private let window: NSWindow
    private let field: NSTextField
    private let importErrorLabel: NSTextField
    private var completed = false
    private var keyMonitor: Any?
    private var pastedText: String?

    init(callback: @escaping KickCookiePromptCallback, context: UnsafeMutableRawPointer) {
        self.callback = callback
        self.context = context
        self.window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 560, height: 244),
            styleMask: [.titled, .closable],
            backing: .buffered,
            defer: false
        )
        self.field = NSTextField(frame: NSRect(x: 20, y: 104, width: 520, height: 28))
        self.importErrorLabel = NSTextField(labelWithString: "")
        super.init()

        window.title = "Kick にログイン / Sign in to Kick"
        window.isReleasedWhenClosed = false
        window.delegate = self

        let content = NSView(frame: NSRect(x: 0, y: 0, width: 560, height: 244))
        let instructions = NSTextField(
            wrappingLabelWithString: "Kick の session_token の値（DevTools → Application → Cookies → kick.com）を貼り付けてください。コピー方法は右のボタン。Cmd+V /「ペースト」対応。"
        )
        instructions.frame = NSRect(x: 20, y: 194, width: 520, height: 44)
        content.addSubview(instructions)

        let importButton = NSButton(title: "JSONファイルを読み込む… / Load JSON…", target: self, action: #selector(chooseJSONFile))
        importButton.frame = NSRect(x: 20, y: 151, width: 190, height: 30)
        content.addSubview(importButton)

        let helpButton = NSButton(title: "コピー方法", target: self, action: #selector(showCookieHelp))
        helpButton.frame = NSRect(x: 220, y: 151, width: 120, height: 30)
        content.addSubview(helpButton)

        field.frame = NSRect(x: 20, y: 104, width: 420, height: 28)
        field.placeholderString = "session_token / Cookie header"
        content.addSubview(field)

        let pasteButton = NSButton(title: "ペースト / Paste", target: self, action: #selector(pasteFromClipboard))
        pasteButton.frame = NSRect(x: 450, y: 104, width: 90, height: 28)
        content.addSubview(pasteButton)

        importErrorLabel.frame = NSRect(x: 20, y: 68, width: 520, height: 22)
        importErrorLabel.textColor = .systemRed
        importErrorLabel.font = .systemFont(ofSize: 12)
        content.addSubview(importErrorLabel)

        let cancelButton = NSButton(title: "キャンセル / Cancel", target: self, action: #selector(cancel))
        cancelButton.frame = NSRect(x: 360, y: 20, width: 90, height: 32)
        content.addSubview(cancelButton)

        let continueButton = NSButton(title: "続ける / Continue", target: self, action: #selector(submit))
        continueButton.frame = NSRect(x: 460, y: 20, width: 80, height: 32)
        continueButton.keyEquivalent = "\r"
        content.addSubview(continueButton)

        window.contentView = content
        window.initialFirstResponder = field
    }

    func show() {
        window.center()
        NSApp.activate(ignoringOtherApps: true)
        window.makeKeyAndOrderFront(nil)
        window.makeFirstResponder(field)
        keyMonitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] event in
            MainActor.assumeIsolated {
                guard let self else { return event }
                if event.modifierFlags.contains(.command),
                   event.charactersIgnoringModifiers?.lowercased() == "v" {
                    self.pasteFromClipboard()
                    return nil
                }
                return event
            }
        }
    }

    @objc private func pasteFromClipboard() {
        guard let text = NSPasteboard.general.string(forType: .string), !text.isEmpty else {
            importErrorLabel.stringValue = "クリップボードにテキストがありません"
            importErrorLabel.textColor = .systemRed
            return
        }
        pastedText = text
        field.stringValue = text
        if let editor = field.currentEditor() {
            editor.string = text
        }
        importErrorLabel.stringValue = "クリップボードから貼り付けました（\(text.count)文字）"
        importErrorLabel.textColor = .secondaryLabelColor
    }

    @objc private func submit() {
        var enteredText = (pastedText ?? field.currentEditor()?.string ?? field.stringValue)
            .trimmingCharacters(in: .whitespacesAndNewlines)
        pastedText = nil
        field.stringValue = ""
        field.currentEditor()?.string = ""
        var data = Data(enteredText.utf8)
        enteredText.removeAll(keepingCapacity: false)
        defer { wipe(&data) }

        let isValid = data.withUnsafeBytes { rawBytes in
            isValidNativeCookieBytes(rawBytes.bindMemory(to: UInt8.self))
        }
        guard isValid else {
            let reason = if data.isEmpty {
                "空"
            } else if data.count > maximumCookieLength {
                "長すぎる"
            } else {
                "使用できない文字"
            }
            importErrorLabel.stringValue = "入力が無効です: \(reason)（\(data.count)文字）"
            importErrorLabel.textColor = .systemRed
            return
        }

        var outputBytes: UnsafeMutablePointer<UInt8>?
        var outputLength: UInt = 0
        let status = copyToNativeBuffer(
            &data,
            outBytes: &outputBytes,
            outLength: &outputLength
        )
        finish(status: status, bytes: outputBytes, length: outputLength)
    }

    @objc private func showCookieHelp() {
        let alert = NSAlert()
        alert.messageText = "session_token のコピー方法 / How to copy"
        alert.informativeText = """
        Kick にログインしたブラウザで DevTools を開きます。
        1. Application タブ → Cookies → https://kick.com
        2. 名前が session_token の行を探す
        3. その Value をコピー（session_token= は不要）
        4. この画面に貼り付けて「続ける」
        ※ kick_session ではログインできません。
        """
        alert.addButton(withTitle: "OK")
        alert.runModal()
    }

    @objc private func chooseJSONFile() {
        let panel = NSOpenPanel()
        panel.allowedContentTypes = [.json]
        panel.allowsOtherFileTypes = false
        panel.canChooseFiles = true
        panel.canChooseDirectories = false
        panel.allowsMultipleSelection = false
        panel.message = "Kick用のCookie JSONを選択してください。 / Select a Kick Cookie JSON file."
        panel.beginSheetModal(for: window) { [weak self] response in
            guard response == .OK, let self, let url = panel.url else { return }
            self.importJSON(from: url)
        }
    }

    private func importJSON(from url: URL) {
        let hasScopedAccess = url.startAccessingSecurityScopedResource()
        defer {
            if hasScopedAccess { url.stopAccessingSecurityScopedResource() }
        }

        do {
            let fileSize = try url.resourceValues(forKeys: [.fileSizeKey]).fileSize
            guard let fileSize else { throw CookieJSONImportError.malformed }
            guard fileSize <= CookieJSONImporter.maximumJSONBytes else {
                throw CookieJSONImportError.tooLarge
            }

            var data = try Data(contentsOf: url)
            defer { wipe(&data) }
            var header = try CookieJSONImporter.cookieHeader(from: data)
            field.stringValue = header
            header.removeAll(keepingCapacity: false)
            importErrorLabel.stringValue = ""
            window.makeFirstResponder(field)
        } catch let error as CookieJSONImportError {
            switch error {
            case .tooLarge, .tooManyCookies:
                importErrorLabel.stringValue = "ファイルが大きすぎるか、Cookieの件数が上限を超えています。 / File too large or too many cookies."
            case .noMatchingKickCookies:
                importErrorLabel.stringValue = "kick.com用の有効なCookieが見つかりません。 / No valid kick.com cookies found."
            case .invalidCookie, .malformed:
                importErrorLabel.stringValue = "Cookie JSONを読み込めません。形式を確認してください。 / Could not read the Cookie JSON."
            }
        } catch {
            importErrorLabel.stringValue = "ファイルを読み込めません。 / Could not read the file."
        }
    }

    @objc private func cancel() {
        finish(status: CredentialBridgeStatus.cancelledOrNotFound)
    }

    func windowWillClose(_ notification: Notification) {
        finish(status: CredentialBridgeStatus.cancelledOrNotFound, closeWindow: false)
    }

    private func finish(
        status: Int32,
        bytes: UnsafeMutablePointer<UInt8>? = nil,
        length: UInt = 0,
        closeWindow: Bool = true
    ) {
        guard !completed else {
            if let bytes { kickCredentialsFreeBytes(bytes, length) }
            return
        }
        completed = true
        if let keyMonitor {
            NSEvent.removeMonitor(keyMonitor)
            self.keyMonitor = nil
        }
        pastedText = nil
        field.stringValue = ""
        field.currentEditor()?.string = ""
        window.delegate = nil
        if closeWindow { window.close() }
        activeCookiePrompt = nil
        callback(context, status, bytes, length)
    }
}

/// Begins a masked Cookie prompt and returns immediately. Completion runs once on the main thread.
/// It never writes to Keychain; callers validate and persist only after completion.
/// Shows the session_token copy instructions from the GPUI login panel.
@_cdecl("kick_show_login_help")
public func kickShowLoginHelp() {
    let alert = NSAlert()
    alert.messageText = "session_token のコピー方法 / How to copy"
    alert.informativeText = """
    Kick にログインしたブラウザで DevTools を開きます。
    1. Application タブ → Cookies → https://kick.com
    2. 名前が session_token の行を探す
    3. その Value をコピー
    4. アプリのログイン画面で「クリップボードから貼り付け」
    ※ kick_session ではログインできません。
    """
    alert.addButton(withTitle: "OK")
    alert.runModal()
}

/// Reads the general pasteboard as UTF-8 bytes for the GPUI login panel.
@_cdecl("kick_clipboard_read")
public func kickClipboardRead(
    _ outBytes: UnsafeMutablePointer<UnsafeMutablePointer<UInt8>?>,
    _ outLength: UnsafeMutablePointer<UInt>
) -> Int32 {
    guard let text = NSPasteboard.general.string(forType: .string) else {
        return CredentialBridgeStatus.cancelledOrNotFound
    }
    var data = Data(text.utf8)
    return copyToNativeBuffer(&data, outBytes: outBytes, outLength: outLength)
}

@_cdecl("kick_credentials_begin_prompt_cookie")
public func kickCredentialsBeginPromptCookie(
    _ callback: KickCookiePromptCallback?,
    _ context: UnsafeMutableRawPointer?
) -> Int32 {
    guard let callback, let context else { return CredentialBridgeStatus.invalidArgument }
    guard Thread.isMainThread else { return CredentialBridgeStatus.wrongThread }

    return onCredentialsMainActor {
        guard activeCookiePrompt == nil else { return CredentialBridgeStatus.promptAlreadyOpen }
        let prompt = CookiePromptController(callback: callback, context: context)
        activeCookiePrompt = prompt
        prompt.show()
        return CredentialBridgeStatus.ok
    } ?? CredentialBridgeStatus.wrongThread
}

/// Loads only the app-owned, non-synchronizable generic-password item.
@_cdecl("kick_credentials_load_cookie")
public func kickCredentialsLoadCookie(
    _ outBytes: UnsafeMutablePointer<UnsafeMutablePointer<UInt8>?>?,
    _ outLength: UnsafeMutablePointer<UInt>?,
    _ outKeychainStatus: UnsafeMutablePointer<Int32>?
) -> Int32 {
    guard let outBytes, let outLength, let outKeychainStatus else {
        return CredentialBridgeStatus.invalidArgument
    }
    outBytes.pointee = nil
    outLength.pointee = 0
    outKeychainStatus.pointee = 0

    var query = makeKeychainQuery()
    query[kSecMatchLimit as String] = kSecMatchLimitOne
    query[kSecReturnData as String] = kCFBooleanTrue

    var result: CFTypeRef?
    let status = SecItemCopyMatching(query as CFDictionary, &result)
    guard status != errSecItemNotFound else { return CredentialBridgeStatus.cancelledOrNotFound }
    guard status == errSecSuccess else {
        outKeychainStatus.pointee = status
        return CredentialBridgeStatus.keychainFailure
    }

    guard let keychainResult = result else { return CredentialBridgeStatus.invalidArgument }
    let copyStatus = copyKeychainResultToNativeBuffer(
        keychainResult,
        outBytes: outBytes,
        outLength: outLength
    )
    result = nil
    return copyStatus
}

private func copyKeychainResult(_ result: CFTypeRef) -> Data? {
    guard let source = result as? Data else { return nil }
    var copy = Data()
    copy.reserveCapacity(source.count)
    source.withUnsafeBytes { rawBytes in
        guard let baseAddress = rawBytes.baseAddress else { return }
        let bytes = UnsafeBufferPointer(
            start: baseAddress.assumingMemoryBound(to: UInt8.self),
            count: rawBytes.count
        )
        copy.append(contentsOf: bytes)
    }
    return copy
}

private func copyKeychainResultToNativeBuffer(
    _ result: CFTypeRef,
    outBytes: UnsafeMutablePointer<UnsafeMutablePointer<UInt8>?>,
    outLength: UnsafeMutablePointer<UInt>
) -> Int32 {
    guard let source = result as? Data else { return CredentialBridgeStatus.invalidArgument }
    guard source.count <= maximumCookieLength else { return CredentialBridgeStatus.tooLarge }
    guard source.count > 0 else { return CredentialBridgeStatus.invalidCookie }

    guard var data = copyKeychainResult(result) else { return CredentialBridgeStatus.invalidArgument }
    defer { wipe(&data) }

    let isValid = data.withUnsafeBytes { rawBytes in
        isValidNativeCookieBytes(rawBytes.bindMemory(to: UInt8.self))
    }
    guard isValid else {
        return data.count > maximumCookieLength
            ? CredentialBridgeStatus.tooLarge
            : CredentialBridgeStatus.invalidCookie
    }
    return copyToNativeBuffer(&data, outBytes: outBytes, outLength: outLength)
}

/// Stores or replaces only this app's designated generic-password item.
@_cdecl("kick_credentials_save_cookie")
public func kickCredentialsSaveCookie(
    _ bytes: UnsafePointer<UInt8>?,
    _ length: UInt,
    _ outKeychainStatus: UnsafeMutablePointer<Int32>?
) -> Int32 {
    guard let bytes, let outKeychainStatus else { return CredentialBridgeStatus.invalidArgument }
    outKeychainStatus.pointee = 0
    guard length <= UInt(maximumCookieLength) else { return CredentialBridgeStatus.tooLarge }
    guard length > 0 else { return CredentialBridgeStatus.invalidCookie }

    let byteCount = Int(length)
    let input = UnsafeBufferPointer(start: bytes, count: byteCount)
    guard isValidNativeCookieBytes(input) else { return CredentialBridgeStatus.invalidCookie }

    var data = Data(bytes: bytes, count: byteCount)
    defer { wipe(&data) }

    let query = makeKeychainQuery()
    let updates = [kSecValueData as String: data] as CFDictionary
    let updateStatus = SecItemUpdate(query as CFDictionary, updates)
    if updateStatus == errSecSuccess { return CredentialBridgeStatus.ok }
    guard updateStatus == errSecItemNotFound else {
        outKeychainStatus.pointee = updateStatus
        return CredentialBridgeStatus.keychainFailure
    }

    var addQuery = query
    addQuery[kSecValueData as String] = data
    let addStatus = SecItemAdd(addQuery as CFDictionary, nil)
    if addStatus == errSecSuccess { return CredentialBridgeStatus.ok }
    if addStatus == errSecDuplicateItem {
        let retryStatus = SecItemUpdate(query as CFDictionary, updates)
        if retryStatus == errSecSuccess { return CredentialBridgeStatus.ok }
        outKeychainStatus.pointee = retryStatus
        return CredentialBridgeStatus.keychainFailure
    }

    outKeychainStatus.pointee = addStatus
    return CredentialBridgeStatus.keychainFailure
}

/// Deletes only this app's designated generic-password item; absence is success.
@_cdecl("kick_credentials_delete_cookie")
public func kickCredentialsDeleteCookie(
    _ outKeychainStatus: UnsafeMutablePointer<Int32>?
) -> Int32 {
    guard let outKeychainStatus else { return CredentialBridgeStatus.invalidArgument }
    outKeychainStatus.pointee = 0

    let status = SecItemDelete(makeKeychainQuery() as CFDictionary)
    guard status == errSecSuccess || status == errSecItemNotFound else {
        outKeychainStatus.pointee = status
        return CredentialBridgeStatus.keychainFailure
    }
    return CredentialBridgeStatus.ok
}

/// Wipes and frees native buffers returned by the prompt and load calls.
@_cdecl("kick_credentials_free_bytes")
public func kickCredentialsFreeBytes(_ bytes: UnsafeMutablePointer<UInt8>?, _ length: UInt) {
    guard let bytes else { return }
    let wipeLength = min(length, UInt(maximumCookieLength))
    if wipeLength > 0 {
        let count = Int(wipeLength)
        _ = memset_s(UnsafeMutableRawPointer(bytes), count, 0, count)
    }
    free(UnsafeMutableRawPointer(bytes))
}
