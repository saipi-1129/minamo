//! Masked cookie input and app-owned Keychain persistence.
//!
//! `SecretCookieInput` keeps bytes out of `Debug` output. The UI may inspect them only inside
//! `with_bytes`, for example to run the app's `SessionCookie::parse_bytes` validator before
//! calling `save_cookie`.

use std::error::Error;
use std::ffi::c_void;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::NonNull;
use std::slice;

const MAX_COOKIE_BYTES: usize = 8 * 1024;
const NATIVE_OK: i32 = 0;
const NATIVE_NONE: i32 = 1;
const NATIVE_WRONG_THREAD: i32 = -2;
const NATIVE_INVALID_COOKIE: i32 = -3;
const NATIVE_TOO_LARGE: i32 = -4;
const NATIVE_KEYCHAIN_FAILURE: i32 = -5;
const NATIVE_PROMPT_ALREADY_OPEN: i32 = -7;

pub type PromptResult = Result<Option<SecretCookieInput>, CredentialError>;
type PromptCompletion = Box<dyn FnOnce(PromptResult)>;

#[link(name = "KickPlayerCBridge", kind = "dylib")]
unsafe extern "C" {
    fn kick_credentials_begin_prompt_cookie(
        callback: unsafe extern "C" fn(*mut c_void, i32, *mut u8, usize),
        context: *mut c_void,
    ) -> i32;
    fn kick_credentials_load_cookie(
        out_bytes: *mut *mut u8,
        out_length: *mut usize,
        out_keychain_status: *mut i32,
    ) -> i32;
    fn kick_credentials_save_cookie(
        bytes: *const u8,
        length: usize,
        out_keychain_status: *mut i32,
    ) -> i32;
    fn kick_credentials_delete_cookie(out_keychain_status: *mut i32) -> i32;
    fn kick_credentials_free_bytes(bytes: *mut u8, length: usize);
    fn kick_clipboard_read(out_bytes: *mut *mut u8, out_length: *mut usize) -> i32;
    fn kick_show_login_help();
}

/// Safe credential error state. Keychain errors expose only their numeric `OSStatus`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialError {
    WrongThread,
    PromptAlreadyOpen,
    InvalidCookie,
    CookieTooLarge,
    NativeFailure,
    KeychainStatus(i32),
    InvalidNativeBuffer,
}

impl fmt::Display for CredentialError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongThread => {
                formatter.write_str("cookie prompt requires the AppKit main thread")
            }
            Self::PromptAlreadyOpen => formatter.write_str("a cookie prompt is already open"),
            Self::InvalidCookie => formatter.write_str("cookie input is invalid"),
            Self::CookieTooLarge => formatter.write_str("cookie input exceeds the 8 KiB limit"),
            Self::NativeFailure => formatter.write_str("native credential operation failed"),
            Self::KeychainStatus(status) => {
                write!(formatter, "Keychain operation failed (status {status})")
            }
            Self::InvalidNativeBuffer => {
                formatter.write_str("native credential buffer was invalid")
            }
        }
    }
}

impl Error for CredentialError {}

/// Opaque, bounded cookie input. It intentionally has no `Debug`, `Clone`, or raw-slice accessor.
pub struct SecretCookieInput {
    bytes: Vec<u8>,
}

impl SecretCookieInput {
    /// Gives temporary read-only access so a caller can validate the cookie without making a
    /// `String` copy. Do not log or retain the bytes from the closure.
    pub fn with_bytes<R>(&self, f: impl FnOnce(&[u8]) -> R) -> R {
        f(&self.bytes)
    }

    /// Wraps raw login text (a bare token, a Cookie header, or a JSON export
    /// value) without requiring a valid Cookie header. The app's login parser
    /// extracts the session_token and validates it.
    pub fn from_login_text(text: &str) -> Result<Self, CredentialError> {
        let bytes = text.as_bytes().to_vec();
        if bytes.is_empty() {
            return Err(CredentialError::InvalidCookie);
        }
        if bytes.len() > MAX_COOKIE_BYTES {
            return Err(CredentialError::CookieTooLarge);
        }
        Ok(Self { bytes })
    }

    fn from_vec(mut bytes: Vec<u8>) -> Result<Self, CredentialError> {
        if let Err(error) = validate_cookie_header(&bytes) {
            wipe_bytes(&mut bytes);
            return Err(error);
        }
        Ok(Self { bytes })
    }
}

impl Drop for SecretCookieInput {
    fn drop(&mut self) {
        wipe_bytes(&mut self.bytes);
    }
}

fn wipe_bytes(bytes: &mut [u8]) {
    for byte in bytes {
        // Volatile writes prevent the optimizer from eliding the secret wipe before deallocation.
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

struct NativeSecretBuffer {
    pointer: NonNull<u8>,
    length: usize,
}

impl NativeSecretBuffer {
    /// Takes ownership of a buffer allocated by the Swift bridge.
    unsafe fn from_raw(pointer: *mut u8, length: usize) -> Result<Self, CredentialError> {
        let pointer = NonNull::new(pointer).ok_or(CredentialError::InvalidNativeBuffer)?;
        Ok(Self { pointer, length })
    }

    fn as_slice(&self) -> &[u8] {
        // The Swift bridge returned a live allocation of `length` bytes.
        unsafe { slice::from_raw_parts(self.pointer.as_ptr(), self.length) }
    }
}

impl Drop for NativeSecretBuffer {
    fn drop(&mut self) {
        // Swift allocated this buffer and its matching free API wipes then releases it.
        unsafe { kick_credentials_free_bytes(self.pointer.as_ptr(), self.length) };
    }
}

/// Opens the masked native Cookie window without running a nested AppKit modal loop. Completion
/// runs once on the AppKit main thread after submit, cancel, or close; no Keychain write occurs.
pub fn begin_prompt_cookie(
    callback: impl FnOnce(PromptResult) + 'static,
) -> Result<(), CredentialError> {
    let callback: PromptCompletion = Box::new(callback);
    let context = Box::into_raw(Box::new(callback)).cast::<c_void>();
    let status = unsafe { kick_credentials_begin_prompt_cookie(cookie_prompt_completed, context) };
    if status == NATIVE_OK {
        return Ok(());
    }

    // Native begin failures promise that no callback will be retained or invoked.
    drop(unsafe { Box::from_raw(context.cast::<PromptCompletion>()) });
    match status {
        NATIVE_WRONG_THREAD => Err(CredentialError::WrongThread),
        NATIVE_PROMPT_ALREADY_OPEN => Err(CredentialError::PromptAlreadyOpen),
        _ => Err(CredentialError::NativeFailure),
    }
}

unsafe extern "C" fn cookie_prompt_completed(
    context: *mut c_void,
    status: i32,
    pointer: *mut u8,
    length: usize,
) {
    if context.is_null() {
        free_unexpected_buffer(pointer, length);
        return;
    }

    let completion = unsafe { Box::from_raw(context.cast::<PromptCompletion>()) };
    let result = match status {
        NATIVE_OK => unsafe { copy_native_secret(pointer, length).map(Some) },
        NATIVE_NONE => {
            free_unexpected_buffer(pointer, length);
            Ok(None)
        }
        NATIVE_WRONG_THREAD => {
            free_unexpected_buffer(pointer, length);
            Err(CredentialError::WrongThread)
        }
        NATIVE_INVALID_COOKIE => {
            free_unexpected_buffer(pointer, length);
            Err(CredentialError::InvalidCookie)
        }
        NATIVE_TOO_LARGE => {
            free_unexpected_buffer(pointer, length);
            Err(CredentialError::CookieTooLarge)
        }
        _ => {
            free_unexpected_buffer(pointer, length);
            Err(CredentialError::NativeFailure)
        }
    };

    let callback = *completion;
    let _ = catch_unwind(AssertUnwindSafe(|| callback(result)));
}

/// Loads the app-owned, non-synchronizable cookie item. No matching item returns `Ok(None)`.
/// Shows the native session_token copy instructions.
pub fn show_login_help() {
    unsafe { kick_show_login_help() };
}

/// Reads the general pasteboard as raw login text. The buffer is copied into a
/// zeroizing `SecretCookieInput` and the native allocation is freed.
pub fn read_clipboard() -> Result<SecretCookieInput, CredentialError> {
    let mut bytes: *mut u8 = std::ptr::null_mut();
    let mut length: usize = 0;
    let status = unsafe { kick_clipboard_read(&mut bytes, &mut length) };
    if status != NATIVE_OK || bytes.is_null() {
        if !bytes.is_null() {
            unsafe { kick_credentials_free_bytes(bytes, length) };
        }
        return Err(CredentialError::NativeFailure);
    }
    let slice = unsafe { slice::from_raw_parts(bytes, length) };
    let result = std::str::from_utf8(slice)
        .map_err(|_| CredentialError::InvalidCookie)
        .and_then(SecretCookieInput::from_login_text);
    unsafe { kick_credentials_free_bytes(bytes, length) };
    result
}

/// Normalizes text from the GPUI login panel into a minimal Cookie header.
/// The Keychain save path requires a valid Cookie header, so a bare
/// `session_token` value is wrapped as `session_token=<value>`.
pub fn secret_cookie_from_login_text(text: &str) -> Result<SecretCookieInput, CredentialError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(CredentialError::InvalidCookie);
    }
    let header =
        if trimmed.contains("kick_session=") || (trimmed.contains(';') && trimmed.contains('=')) {
            // Preserve a full Cookie header (including kick_session) so chat
            // sending and other POST routes have the same cookies as the browser.
            trimmed
                .strip_prefix("Cookie:")
                .unwrap_or(trimmed)
                .trim()
                .to_owned()
        } else if let Some(start) = trimmed.find("session_token=") {
            let value = trimmed[start + "session_token=".len()..]
                .split(';')
                .next()
                .unwrap_or("")
                .trim();
            let value = value
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
                .unwrap_or(value);
            format!("session_token={value}")
        } else if trimmed.contains('=') || trimmed.contains(';') || trimmed.starts_with("Cookie:") {
            trimmed.to_owned()
        } else {
            format!("session_token={trimmed}")
        };
    SecretCookieInput::from_vec(header.into_bytes())
}

pub fn load_cookie() -> Result<Option<SecretCookieInput>, CredentialError> {
    let mut pointer = std::ptr::null_mut();
    let mut length = 0usize;
    let mut keychain_status = 0i32;
    let status =
        unsafe { kick_credentials_load_cookie(&mut pointer, &mut length, &mut keychain_status) };
    match status {
        NATIVE_OK => unsafe { copy_native_secret(pointer, length).map(Some) },
        NATIVE_NONE => {
            free_unexpected_buffer(pointer, length);
            Ok(None)
        }
        NATIVE_INVALID_COOKIE => {
            free_unexpected_buffer(pointer, length);
            Err(CredentialError::InvalidCookie)
        }
        NATIVE_TOO_LARGE => {
            free_unexpected_buffer(pointer, length);
            Err(CredentialError::CookieTooLarge)
        }
        NATIVE_KEYCHAIN_FAILURE => {
            free_unexpected_buffer(pointer, length);
            Err(CredentialError::KeychainStatus(keychain_status))
        }
        _ => {
            free_unexpected_buffer(pointer, length);
            Err(CredentialError::NativeFailure)
        }
    }
}

/// Saves only a `SecretCookieInput` returned by the native prompt or Keychain load.
/// Callers should run `SessionCookie::parse_bytes` through `with_bytes` before saving.
pub fn save_cookie(cookie: &SecretCookieInput) -> Result<(), CredentialError> {
    validate_cookie_header(&cookie.bytes)?;
    let mut keychain_status = 0i32;
    let status = unsafe {
        kick_credentials_save_cookie(
            cookie.bytes.as_ptr(),
            cookie.bytes.len(),
            &mut keychain_status,
        )
    };
    match status {
        NATIVE_OK => Ok(()),
        NATIVE_INVALID_COOKIE => Err(CredentialError::InvalidCookie),
        NATIVE_TOO_LARGE => Err(CredentialError::CookieTooLarge),
        NATIVE_KEYCHAIN_FAILURE => Err(CredentialError::KeychainStatus(keychain_status)),
        _ => Err(CredentialError::NativeFailure),
    }
}

/// Deletes this app's cookie item. Deleting an absent item succeeds.
pub fn delete_cookie() -> Result<(), CredentialError> {
    let mut keychain_status = 0i32;
    let status = unsafe { kick_credentials_delete_cookie(&mut keychain_status) };
    match status {
        NATIVE_OK => Ok(()),
        NATIVE_KEYCHAIN_FAILURE => Err(CredentialError::KeychainStatus(keychain_status)),
        _ => Err(CredentialError::NativeFailure),
    }
}

unsafe fn copy_native_secret(
    pointer: *mut u8,
    length: usize,
) -> Result<SecretCookieInput, CredentialError> {
    let native = unsafe { NativeSecretBuffer::from_raw(pointer, length)? };
    if length > MAX_COOKIE_BYTES {
        return Err(CredentialError::CookieTooLarge);
    }
    if length == 0 {
        return Err(CredentialError::InvalidCookie);
    }
    SecretCookieInput::from_vec(native.as_slice().to_vec())
}

fn free_unexpected_buffer(pointer: *mut u8, length: usize) {
    if !pointer.is_null() {
        // Error/cancel paths should not return a buffer, but release one defensively if they do.
        unsafe { kick_credentials_free_bytes(pointer, length) };
    }
}

fn validate_cookie_header(bytes: &[u8]) -> Result<(), CredentialError> {
    if bytes.len() > MAX_COOKIE_BYTES {
        return Err(CredentialError::CookieTooLarge);
    }
    if bytes.is_empty()
        || bytes
            .iter()
            .any(|byte| *byte < 0x20 || *byte > 0x7e || *byte == 0x7f)
    {
        return Err(CredentialError::InvalidCookie);
    }

    for raw_pair in bytes.split(|byte| *byte == b';') {
        let pair = trim_spaces(raw_pair);
        let Some(separator) = pair.iter().position(|byte| *byte == b'=') else {
            return Err(CredentialError::InvalidCookie);
        };
        let name = &pair[..separator];
        let value = &pair[separator + 1..];
        if name.is_empty()
            || !name.iter().copied().all(is_cookie_name_byte)
            || value.contains(&b' ')
        {
            return Err(CredentialError::InvalidCookie);
        }
    }
    Ok(())
}

fn trim_spaces(mut bytes: &[u8]) -> &[u8] {
    while bytes.first() == Some(&b' ') {
        bytes = &bytes[1..];
    }
    while bytes.last() == Some(&b' ') {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

fn is_cookie_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                ..=b'\'' | b'*' | b'+' | b'-' | b'.' | b':' | b'^' | b'_' | b'`' | b'|' | b'~'
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_header_shape_and_bounds() {
        assert!(validate_cookie_header(b"example=value; theme=dark").is_ok());
        assert!(validate_cookie_header(b"kick:region=v1").is_ok());
        assert_eq!(
            validate_cookie_header(b"missing-equals"),
            Err(CredentialError::InvalidCookie)
        );
        assert_eq!(
            validate_cookie_header(b"bad name=value"),
            Err(CredentialError::InvalidCookie)
        );
        assert_eq!(
            validate_cookie_header(b"a=b\r\nInjected: x"),
            Err(CredentialError::InvalidCookie)
        );
        assert_eq!(
            validate_cookie_header(b"a=b\0"),
            Err(CredentialError::InvalidCookie)
        );
        assert_eq!(
            validate_cookie_header(&vec![b'x'; MAX_COOKIE_BYTES + 1]),
            Err(CredentialError::CookieTooLarge)
        );
    }

    #[test]
    fn secret_input_exposes_bytes_only_inside_closure() {
        let input = SecretCookieInput::from_vec(b"example=value".to_vec());
        assert!(input.is_ok());
        if let Ok(input) = input {
            assert_eq!(input.with_bytes(|bytes| bytes.len()), 13);
        }
    }

    #[test]
    fn prompt_rejects_a_worker_thread_without_opening_ui() {
        let callback_called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let callback_called_in_worker = callback_called.clone();
        let result = std::thread::spawn(move || {
            begin_prompt_cookie(move |_| {
                callback_called_in_worker.store(true, std::sync::atomic::Ordering::SeqCst);
            })
        })
        .join()
        .unwrap();
        assert!(matches!(result, Err(CredentialError::WrongThread)));
        assert!(!callback_called.load(std::sync::atomic::Ordering::SeqCst));
    }
}
