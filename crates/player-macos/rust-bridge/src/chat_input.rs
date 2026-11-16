//! Native chat input field bridge. The field is an `NSTextField` embedded into the
//! GPUI window, so the macOS input method, selection, Ctrl+A, copy/paste, and
//! standard editing work without a separate popup window.

use std::cell::Cell;
use std::ffi::{CString, c_char, c_void};
use std::rc::Rc;

type FieldCallback = unsafe extern "C" fn(*mut c_void);

#[link(name = "KickPlayerCBridge", kind = "dylib")]
unsafe extern "C" {
    fn kick_chat_input_field_create(
        text_callback: FieldCallback,
        submit_callback: FieldCallback,
        context: *mut c_void,
    ) -> *mut c_void;
    fn kick_chat_input_field_attach(
        handle: *mut c_void,
        parent: *mut c_void,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    ) -> i32;
    fn kick_chat_input_field_focus(handle: *mut c_void) -> i32;
    fn kick_chat_input_field_is_focused(handle: *mut c_void) -> i32;
    fn kick_chat_input_field_text(
        handle: *mut c_void,
        out_bytes: *mut *mut u8,
        out_length: *mut usize,
    ) -> i32;
    fn kick_chat_input_field_set_text(handle: *mut c_void, text: *const c_char) -> i32;
    fn kick_chat_input_field_append_text(handle: *mut c_void, text: *const c_char) -> i32;
    fn kick_chat_input_field_set_placeholder(handle: *mut c_void, text: *const c_char) -> i32;
    fn kick_chat_input_field_set_hidden(handle: *mut c_void, hidden: i32) -> i32;
    fn kick_chat_input_field_destroy(handle: *mut c_void);
    fn kick_credentials_free_bytes(bytes: *mut u8, length: usize);
}

/// Shared flags written by the AppKit delegate and read by the UI thread.
struct FieldState {
    dirty: Cell<bool>,
    submit: Cell<bool>,
    painted: Cell<bool>,
}

unsafe extern "C" fn field_text_changed(context: *mut c_void) {
    if context.is_null() {
        return;
    }
    let state = unsafe { &*context.cast::<FieldState>() };
    state.dirty.set(true);
}

unsafe extern "C" fn field_submitted(context: *mut c_void) {
    if context.is_null() {
        return;
    }
    let state = unsafe { &*context.cast::<FieldState>() };
    state.submit.set(true);
}

/// Owns one native text field. All methods must run on the AppKit main thread.
pub struct ChatInputField {
    handle: *mut c_void,
    state: Rc<FieldState>,
}

impl ChatInputField {
    pub fn new() -> Option<Self> {
        let state = Rc::new(FieldState {
            dirty: Cell::new(false),
            submit: Cell::new(false),
            painted: Cell::new(false),
        });
        let context = Rc::as_ptr(&state).cast::<c_void>().cast_mut();
        let handle =
            unsafe { kick_chat_input_field_create(field_text_changed, field_submitted, context) };
        if handle.is_null() {
            return None;
        }
        Some(Self { handle, state })
    }

    /// Places the field inside the window content view using GPUI window coordinates.
    pub fn attach(&self, parent: *mut c_void, x: f64, y: f64, width: f64, height: f64) {
        let inset_x = 12.0;
        let inset_y = 8.0;
        unsafe {
            kick_chat_input_field_attach(
                self.handle,
                parent,
                x + inset_x,
                y + inset_y,
                (width - inset_x * 2.0).max(1.0),
                (height - inset_y * 2.0).max(1.0),
            );
            kick_chat_input_field_set_hidden(self.handle, 0);
        }
    }

    pub fn focus(&self) -> i32 {
        unsafe { kick_chat_input_field_focus(self.handle) }
    }

    pub fn is_focused(&self) -> bool {
        unsafe { kick_chat_input_field_is_focused(self.handle) == 1 }
    }

    pub fn text(&self) -> String {
        let mut bytes: *mut u8 = std::ptr::null_mut();
        let mut length: usize = 0;
        let status = unsafe { kick_chat_input_field_text(self.handle, &mut bytes, &mut length) };
        if status != 0 || bytes.is_null() || length == 0 {
            return String::new();
        }
        let slice = unsafe { std::slice::from_raw_parts(bytes, length) };
        let text = String::from_utf8_lossy(slice).into_owned();
        unsafe { kick_credentials_free_bytes(bytes, length) };
        text
    }

    pub fn set_text(&self, text: &str) {
        let text = CString::new(text).unwrap_or_default();
        unsafe {
            kick_chat_input_field_set_text(self.handle, text.as_ptr());
        }
    }

    pub fn append_text(&self, text: &str) {
        let text = CString::new(text).unwrap_or_default();
        unsafe {
            kick_chat_input_field_append_text(self.handle, text.as_ptr());
        }
    }

    pub fn set_placeholder(&self, text: &str) {
        let text = CString::new(text).unwrap_or_default();
        unsafe {
            kick_chat_input_field_set_placeholder(self.handle, text.as_ptr());
        }
    }

    pub fn set_hidden(&self, hidden: bool) {
        unsafe {
            kick_chat_input_field_set_hidden(self.handle, i32::from(hidden));
        }
    }

    pub fn take_dirty(&self) -> bool {
        self.state.dirty.replace(false)
    }

    pub fn take_submit(&self) -> bool {
        self.state.submit.replace(false)
    }

    /// Marks the start of a UI frame; the canvas marks the field as painted.
    pub fn begin_frame(&self) {
        self.state.painted.set(false);
    }

    pub fn mark_painted(&self) {
        self.state.painted.set(true);
    }

    pub fn painted_this_frame(&self) -> bool {
        self.state.painted.get()
    }
}

impl Drop for ChatInputField {
    fn drop(&mut self) {
        unsafe { kick_chat_input_field_destroy(self.handle) };
    }
}
