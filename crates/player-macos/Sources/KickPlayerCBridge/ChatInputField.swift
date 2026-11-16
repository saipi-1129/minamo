import AppKit

public typealias KickChatInputTextCallback = @convention(c) (UnsafeMutableRawPointer?) -> Void
public typealias KickChatInputSubmitCallback = @convention(c) (UnsafeMutableRawPointer?) -> Void

@MainActor private final class ChatInputFieldController: NSObject, NSTextFieldDelegate {
    let field: NSTextField
    let textCallback: KickChatInputTextCallback
    let submitCallback: KickChatInputSubmitCallback
    let context: UnsafeMutableRawPointer

    init(
        textCallback: @escaping KickChatInputTextCallback,
        submitCallback: @escaping KickChatInputSubmitCallback,
        context: UnsafeMutableRawPointer
    ) {
        self.field = NSTextField(frame: .zero)
        self.textCallback = textCallback
        self.submitCallback = submitCallback
        self.context = context
        super.init()
        field.isBezeled = false
        field.drawsBackground = false
        field.focusRingType = .none
        field.textColor = .labelColor
        field.font = .systemFont(ofSize: 12)
        field.usesSingleLineMode = true
        field.lineBreakMode = .byTruncatingTail
        field.delegate = self
        field.target = self
        field.action = #selector(submit)
    }

    func controlTextDidChange(_ notification: Notification) {
        textCallback(context)
    }

    @objc private func submit() {
        submitCallback(context)
    }
}

@_cdecl("kick_chat_input_field_create")
public func kickChatInputFieldCreate(
    _ textCallback: KickChatInputTextCallback?,
    _ submitCallback: KickChatInputSubmitCallback?,
    _ context: UnsafeMutableRawPointer?
) -> UnsafeMutableRawPointer? {
    guard Thread.isMainThread, let textCallback, let submitCallback, let context else { return nil }
    return MainActor.assumeIsolated {
        let controller = ChatInputFieldController(
            textCallback: textCallback,
            submitCallback: submitCallback,
            context: context
        )
        return Unmanaged.passRetained(controller).toOpaque()
    }
}

@_cdecl("kick_chat_input_field_attach")
public func kickChatInputFieldAttach(
    _ handle: UnsafeMutableRawPointer?,
    _ parent: UnsafeMutableRawPointer?,
    _ x: Double,
    _ y: Double,
    _ width: Double,
    _ height: Double
) -> Int32 {
    guard Thread.isMainThread, let handle, let parent else { return -1 }
    return MainActor.assumeIsolated {
        let controller = Unmanaged<ChatInputFieldController>.fromOpaque(handle).takeUnretainedValue()
        let parentView = Unmanaged<NSView>.fromOpaque(parent).takeUnretainedValue()
        let parentBounds = parentView.bounds
        let frameX = parentBounds.origin.x + x
        let frameY = parentView.isFlipped
            ? parentBounds.origin.y + y
            : parentBounds.origin.y + parentBounds.size.height - y - height
        controller.field.frame = CGRect(x: frameX, y: frameY, width: width, height: height)
        if controller.field.superview !== parentView {
            controller.field.removeFromSuperview()
            parentView.addSubview(controller.field)
        }
        return 0
    }
}

@_cdecl("kick_chat_input_field_set_frame")
public func kickChatInputFieldSetFrame(
    _ handle: UnsafeMutableRawPointer?,
    _ x: Double,
    _ y: Double,
    _ width: Double,
    _ height: Double
) -> Int32 {
    guard Thread.isMainThread, let handle else { return -1 }
    return MainActor.assumeIsolated {
        let controller = Unmanaged<ChatInputFieldController>.fromOpaque(handle).takeUnretainedValue()
        if let parentView = controller.field.superview {
            let parentBounds = parentView.bounds
            let frameX = parentBounds.origin.x + x
            let frameY = parentView.isFlipped
                ? parentBounds.origin.y + y
                : parentBounds.origin.y + parentBounds.size.height - y - height
            controller.field.frame = CGRect(x: frameX, y: frameY, width: width, height: height)
        }
        return 0
    }
}

@_cdecl("kick_chat_input_field_focus")
public func kickChatInputFieldFocus(_ handle: UnsafeMutableRawPointer?) -> Int32 {
    guard Thread.isMainThread, let handle else { return -1 }
    return MainActor.assumeIsolated {
        let controller = Unmanaged<ChatInputFieldController>.fromOpaque(handle).takeUnretainedValue()
        guard let window = controller.field.window else { return -3 }
        return window.makeFirstResponder(controller.field) ? 0 : -2
    }
}

@_cdecl("kick_chat_input_field_is_focused")
public func kickChatInputFieldIsFocused(_ handle: UnsafeMutableRawPointer?) -> Int32 {
    guard Thread.isMainThread, let handle else { return 0 }
    return MainActor.assumeIsolated {
        let controller = Unmanaged<ChatInputFieldController>.fromOpaque(handle).takeUnretainedValue()
        // While editing, AppKit installs a field editor as the first responder, so the
        // field itself is not the responder. Treat an active editor as focused.
        return controller.field.currentEditor() != nil ? 1 : 0
    }
}

@_cdecl("kick_chat_input_field_text")
public func kickChatInputFieldText(
    _ handle: UnsafeMutableRawPointer?,
    _ outBytes: UnsafeMutablePointer<UnsafeMutablePointer<UInt8>?>,
    _ outLength: UnsafeMutablePointer<UInt>
) -> Int32 {
    guard Thread.isMainThread, let handle else { return -1 }
    return MainActor.assumeIsolated {
        let controller = Unmanaged<ChatInputFieldController>.fromOpaque(handle).takeUnretainedValue()
        let data = Data(controller.field.stringValue.utf8)
        guard !data.isEmpty else {
            outBytes.pointee = nil
            outLength.pointee = 0
            return 0
        }
        guard let allocation = malloc(data.count) else { return -6 }
        let buffer = allocation.assumingMemoryBound(to: UInt8.self)
        data.copyBytes(to: buffer, count: data.count)
        outBytes.pointee = buffer
        outLength.pointee = UInt(data.count)
        return 0
    }
}

@_cdecl("kick_chat_input_field_set_text")
public func kickChatInputFieldSetText(_ handle: UnsafeMutableRawPointer?, _ text: UnsafePointer<CChar>?) -> Int32 {
    guard Thread.isMainThread, let handle else { return -1 }
    return MainActor.assumeIsolated {
        let controller = Unmanaged<ChatInputFieldController>.fromOpaque(handle).takeUnretainedValue()
        controller.field.stringValue = text.map { String(cString: $0) } ?? ""
        return 0
    }
}

@_cdecl("kick_chat_input_field_append_text")
public func kickChatInputFieldAppendText(_ handle: UnsafeMutableRawPointer?, _ text: UnsafePointer<CChar>?) -> Int32 {
    guard Thread.isMainThread, let handle, let text else { return -1 }
    return MainActor.assumeIsolated {
        let controller = Unmanaged<ChatInputFieldController>.fromOpaque(handle).takeUnretainedValue()
        controller.field.stringValue += String(cString: text)
        return 0
    }
}

@_cdecl("kick_chat_input_field_set_placeholder")
public func kickChatInputFieldSetPlaceholder(_ handle: UnsafeMutableRawPointer?, _ text: UnsafePointer<CChar>?) -> Int32 {
    guard Thread.isMainThread, let handle else { return -1 }
    return MainActor.assumeIsolated {
        let controller = Unmanaged<ChatInputFieldController>.fromOpaque(handle).takeUnretainedValue()
        controller.field.placeholderString = text.map { String(cString: $0) } ?? ""
        return 0
    }
}

@_cdecl("kick_chat_input_field_set_hidden")
public func kickChatInputFieldSetHidden(_ handle: UnsafeMutableRawPointer?, _ hidden: Int32) -> Int32 {
    guard Thread.isMainThread, let handle else { return -1 }
    return MainActor.assumeIsolated {
        let controller = Unmanaged<ChatInputFieldController>.fromOpaque(handle).takeUnretainedValue()
        controller.field.isHidden = hidden != 0
        return 0
    }
}

@_cdecl("kick_chat_input_field_destroy")
public func kickChatInputFieldDestroy(_ handle: UnsafeMutableRawPointer?) {
    guard Thread.isMainThread, let handle else { return }
    MainActor.assumeIsolated {
        let controller = Unmanaged<ChatInputFieldController>.fromOpaque(handle).takeRetainedValue()
        controller.field.removeFromSuperview()
    }
}
