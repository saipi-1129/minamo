import AppKit

public typealias KickChatInputCallback = @convention(c) (
    UnsafeMutableRawPointer?, Int32, UnsafePointer<CChar>?
) -> Void

@MainActor private var activeChatInputPrompt: ChatInputPromptController?

/// A native single-line input panel for chat. NSTextField provides the macOS
/// input method (Japanese/English switching), selection, Ctrl+A, copy/paste,
/// undo, and the standard editing commands that the custom GPUI input lacks.
@MainActor private final class ChatInputPromptController: NSObject, NSWindowDelegate {
    private let callback: KickChatInputCallback
    private let context: UnsafeMutableRawPointer
    private let window: NSWindow
    private let field: NSTextField
    private var completed = false

    init(
        initialText: String,
        title: String,
        sendLabel: String,
        cancelLabel: String,
        placeholder: String,
        instructions: String,
        callback: @escaping KickChatInputCallback,
        context: UnsafeMutableRawPointer
    ) {
        self.callback = callback
        self.context = context
        self.window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 560, height: 150),
            styleMask: [.titled, .closable],
            backing: .buffered,
            defer: false
        )
        self.field = NSTextField(frame: NSRect(x: 20, y: 76, width: 520, height: 28))
        super.init()

        window.title = title
        window.isReleasedWhenClosed = false
        window.delegate = self
        window.appearance = NSAppearance(named: .darkAqua)

        let content = NSView(frame: NSRect(x: 0, y: 0, width: 560, height: 150))
        let instructions = NSTextField(
            wrappingLabelWithString: instructions
        )
        instructions.frame = NSRect(x: 20, y: 112, width: 520, height: 22)
        instructions.textColor = .secondaryLabelColor
        instructions.font = .systemFont(ofSize: 11)
        content.addSubview(instructions)

        field.stringValue = initialText
        field.placeholderString = placeholder
        field.font = .systemFont(ofSize: 14)
        content.addSubview(field)

        let cancelButton = NSButton(title: cancelLabel, target: self, action: #selector(cancel))
        cancelButton.frame = NSRect(x: 360, y: 20, width: 90, height: 32)
        content.addSubview(cancelButton)

        let sendButton = NSButton(title: sendLabel, target: self, action: #selector(submit))
        sendButton.frame = NSRect(x: 460, y: 20, width: 80, height: 32)
        sendButton.keyEquivalent = "\r"
        content.addSubview(sendButton)

        window.contentView = content
        window.initialFirstResponder = field
    }

    func show() {
        window.center()
        NSApp.activate(ignoringOtherApps: true)
        window.makeKeyAndOrderFront(nil)
        window.makeFirstResponder(field)
    }

    @objc private func submit() {
        finish(text: field.stringValue)
    }

    @objc private func cancel() {
        finish(text: nil)
    }

    func windowWillClose(_ notification: Notification) {
        finish(text: nil, closeWindow: false)
    }

    private func finish(text: String?, closeWindow: Bool = true) {
        guard !completed else { return }
        completed = true
        window.delegate = nil
        if closeWindow { window.close() }
        activeChatInputPrompt = nil
        if let text {
            text.withCString { callback(context, 0, $0) }
        } else {
            callback(context, 1, nil)
        }
    }
}

/// Opens the native chat input panel. The callback runs once on the main thread
/// with status 0 and the entered text, or status 1 when cancelled.
@_cdecl("kick_chat_input_begin_prompt")
public func kickChatInputBeginPrompt(
    _ initialText: UnsafePointer<CChar>?,
    _ title: UnsafePointer<CChar>?,
    _ sendLabel: UnsafePointer<CChar>?,
    _ cancelLabel: UnsafePointer<CChar>?,
    _ placeholder: UnsafePointer<CChar>?,
    _ instructions: UnsafePointer<CChar>?,
    _ callback: KickChatInputCallback?,
    _ context: UnsafeMutableRawPointer?
) -> Int32 {
    guard Thread.isMainThread else { return -2 }
    guard let callback, let context else { return -1 }
    return MainActor.assumeIsolated {
        guard activeChatInputPrompt == nil else { return -7 }
        let prompt = ChatInputPromptController(
            initialText: initialText.map { String(cString: $0) } ?? "",
            title: title.map { String(cString: $0) } ?? "Comment",
            sendLabel: sendLabel.map { String(cString: $0) } ?? "Send",
            cancelLabel: cancelLabel.map { String(cString: $0) } ?? "Cancel",
            placeholder: placeholder.map { String(cString: $0) } ?? "",
            instructions: instructions.map { String(cString: $0) } ?? "",
            callback: callback,
            context: context
        )
        activeChatInputPrompt = prompt
        prompt.show()
        return 0
    }
}
