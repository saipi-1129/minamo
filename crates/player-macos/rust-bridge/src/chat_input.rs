//! Native chat input panel bridge. The panel uses NSTextField so the macOS
//! input method, selection, Ctrl+A, copy/paste, and standard editing work.

use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::mpsc::{self, Receiver};

type ChatInputCallback = unsafe extern "C" fn(*mut c_void, i32, *const c_char);

#[link(name = "KickPlayerCBridge", kind = "dylib")]
unsafe extern "C" {
    fn kick_chat_input_begin_prompt(
        initial_text: *const c_char,
        title: *const c_char,
        send_label: *const c_char,
        cancel_label: *const c_char,
        placeholder: *const c_char,
        instructions: *const c_char,
        callback: ChatInputCallback,
        context: *mut c_void,
    ) -> i32;
}

struct ChatInputContext {
    sender: mpsc::Sender<Option<String>>,
}

unsafe extern "C" fn chat_input_callback(context: *mut c_void, status: i32, text: *const c_char) {
    if context.is_null() {
        return;
    }
    let context = unsafe { Box::from_raw(context.cast::<ChatInputContext>()) };
    let value = if status == 0 {
        if text.is_null() {
            None
        } else {
            Some(
                unsafe { CStr::from_ptr(text) }
                    .to_string_lossy()
                    .into_owned(),
            )
        }
    } else {
        None
    };
    let _ = context.sender.send(value);
}

/// Opens the native chat input panel and returns a receiver for the entered
/// text (`None` when cancelled).
pub fn begin_chat_input_prompt(
    initial_text: &str,
    title: &str,
    send_label: &str,
    cancel_label: &str,
    placeholder: &str,
    instructions: &str,
) -> Receiver<Option<String>> {
    let (sender, receiver) = mpsc::channel();
    let context = Box::into_raw(Box::new(ChatInputContext { sender })).cast::<c_void>();
    let initial = CString::new(initial_text).unwrap_or_default();
    let title = CString::new(title).unwrap_or_default();
    let send = CString::new(send_label).unwrap_or_default();
    let cancel = CString::new(cancel_label).unwrap_or_default();
    let placeholder = CString::new(placeholder).unwrap_or_default();
    let instructions = CString::new(instructions).unwrap_or_default();
    let status = unsafe {
        kick_chat_input_begin_prompt(
            initial.as_ptr(),
            title.as_ptr(),
            send.as_ptr(),
            cancel.as_ptr(),
            placeholder.as_ptr(),
            instructions.as_ptr(),
            chat_input_callback,
            context,
        )
    };
    if status != 0 {
        unsafe { drop(Box::from_raw(context.cast::<ChatInputContext>())) };
    }
    receiver
}
