//! Cookie-based session validation for the native app's explicit sign-in flow.
//!
//! The only authenticated request in this module is a fixed GET to
//! `https://kick.com/api/v1/user`. The supplied Cookie header is never attached
//! to public API, search, chat, or playback requests. The anonymous response
//! was an empty object; the authenticated response schema remains unverified.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use reqwest::header::{
    ACCEPT, AUTHORIZATION, CONTENT_TYPE, COOKIE, HeaderMap, HeaderValue, ORIGIN, REFERER,
};
use reqwest::{RequestBuilder, Response, Url};
use serde::Deserialize;
use serde::de::IgnoredAny;
use zeroize::{Zeroize, Zeroizing};

use super::{KickClient, TransportFailure, classify_transport};

const SESSION_USER_URL: &str = "https://kick.com/api/v1/user";
const SESSION_TOKEN_COOKIE: &str = "session_token";
const SESSION_REFERER: &str = "https://kick.com";
const MAX_SESSION_COOKIE_BYTES: usize = 8 * 1024;
const MAX_SESSION_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_SESSION_ID_BYTES: usize = 128;
const MAX_SESSION_USERNAME_BYTES: usize = 128;
const SESSION_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// A parsed Cookie header whose contents are zeroized when dropped and never
/// exposed in `Debug` output.
pub struct SessionCookie(Zeroizing<String>);

impl SessionCookie {
    /// Parses a pasted Cookie header such as `name=value; other=value`.
    ///
    /// An optional `Cookie:` field-name prefix is accepted. Values are only
    /// retained as one canonical header string; this does not assume which
    /// cookie names Kick requires.
    pub fn parse(value: &str) -> Result<Self, SessionAuthError> {
        Self::parse_bytes(value.as_bytes())
    }

    /// Parses bytes from a Keychain bridge without exposing a public raw-value
    /// accessor. Non-ASCII and control bytes are rejected before conversion.
    pub fn parse_bytes(value: &[u8]) -> Result<Self, SessionAuthError> {
        if value.is_empty()
            || value.len() > MAX_SESSION_COOKIE_BYTES
            || !value.is_ascii()
            || value.iter().any(|byte| *byte <= 0x1f || *byte == 0x7f)
        {
            return Err(SessionAuthError::InvalidCookie);
        }

        let input = std::str::from_utf8(value).map_err(|_| SessionAuthError::InvalidCookie)?;
        let normalized = normalize_cookie_header(input).ok_or(SessionAuthError::InvalidCookie)?;
        Ok(Self(normalized))
    }

    fn header_value(&self) -> Result<HeaderValue, SessionAuthError> {
        let mut header =
            HeaderValue::from_str(self.0.as_str()).map_err(|_| SessionAuthError::InvalidCookie)?;
        header.set_sensitive(true);
        Ok(header)
    }

    /// Extracts the `session_token` cookie value observed in Kick's web client.
    ///
    /// The web API wrapper sends this value as a Bearer token in addition to
    /// cookies. Browser exports may percent-encode the value, while the web
    /// client's cookie reader decodes it before building the header. The value
    /// stays in a zeroizing buffer and is never formatted.
    fn session_token(&self) -> Option<Zeroizing<String>> {
        for pair in self.0.split(';') {
            let pair = pair.trim();
            let Some((name, value)) = pair.split_once('=') else {
                continue;
            };
            if name == SESSION_TOKEN_COOKIE && !value.is_empty() {
                return Some(percent_decode_cookie_value(value));
            }
        }
        None
    }

    /// Builds a Cookie header whose Bearer is the raw input token. `%` is
    /// encoded as `%25` so the existing percent-decoding Bearer path returns
    /// the original input. Used as a second candidate when the pasted value is
    /// already decoded.
    pub fn from_session_token_raw(value: &str) -> Result<Self, SessionAuthError> {
        let value = value.trim();
        if value.is_empty()
            || value.len() > MAX_SESSION_COOKIE_BYTES
            || !value.is_ascii()
            || value.bytes().any(|byte| byte <= 0x1f || byte == 0x7f)
        {
            return Err(SessionAuthError::InvalidCookie);
        }
        let mut encoded = String::with_capacity(value.len());
        for byte in value.bytes() {
            match byte {
                b'%' | b';' | b' ' | b'\t' | b'\r' | b'\n' | b',' | b'"' | b'\\' => {
                    const HEX: &[u8; 16] = b"0123456789ABCDEF";
                    encoded.push('%');
                    encoded.push(HEX[(byte >> 4) as usize] as char);
                    encoded.push(HEX[(byte & 0x0f) as usize] as char);
                }
                _ => encoded.push(byte as char),
            }
        }
        let mut header = Zeroizing::new(format!("session_token={encoded}"));
        Self::parse(header.as_str())
    }

    /// Returns the percent-decoded `session_token` value for diagnostics.
    /// The value is sensitive and must never be printed or logged.
    pub fn session_token_value(&self) -> Option<Zeroizing<String>> {
        self.session_token()
    }

    /// Builds a minimal Cookie header from a bare `session_token` value.
    ///
    /// The diagnostic probe showed that `session_token` alone authenticates the
    /// current-user route (and the Bearer-only form also works). Unsafe cookie
    /// delimiters are percent-encoded; an already encoded value is preserved.
    pub fn from_session_token(value: &str) -> Result<Self, SessionAuthError> {
        let value = value.trim();
        if value.is_empty()
            || value.len() > MAX_SESSION_COOKIE_BYTES
            || !value.is_ascii()
            || value.bytes().any(|byte| byte <= 0x1f || byte == 0x7f)
        {
            return Err(SessionAuthError::InvalidCookie);
        }
        let mut encoded = String::with_capacity(value.len());
        for byte in value.bytes() {
            match byte {
                b';' | b' ' | b'\t' | b'\r' | b'\n' | b',' | b'"' | b'\\' => {
                    const HEX: &[u8; 16] = b"0123456789ABCDEF";
                    encoded.push('%');
                    encoded.push(HEX[(byte >> 4) as usize] as char);
                    encoded.push(HEX[(byte & 0x0f) as usize] as char);
                }
                _ => encoded.push(byte as char),
            }
        }
        let mut header = Zeroizing::new(format!("session_token={encoded}"));
        Self::parse(header.as_str())
    }

    /// Returns the cookie names only, never the values. Useful for diagnosing
    /// which browser cookies a minimal sign-in needs.
    pub fn cookie_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        for pair in self.0.split(';') {
            let pair = pair.trim();
            let Some((name, _)) = pair.split_once('=') else {
                continue;
            };
            if !name.is_empty() && !names.iter().any(|existing| existing == name) {
                names.push(name.to_owned());
            }
            if names.len() >= 32 {
                break;
            }
        }
        names
    }

    /// Builds a new Cookie header containing only the named cookies. Returns
    /// `None` when none of the names are present.
    pub fn select_cookies(&self, names: &[&str]) -> Option<Self> {
        let mut selected = Vec::new();
        for pair in self.0.split(';') {
            let pair = pair.trim();
            let Some((name, _)) = pair.split_once('=') else {
                continue;
            };
            if names.iter().any(|wanted| *wanted == name) {
                selected.push(pair.to_owned());
            }
        }
        if selected.is_empty() {
            return None;
        }
        let mut header = Zeroizing::new(selected.join("; "));
        Self::parse(header.as_str()).ok()
    }

    /// Applies only the observed web-client headers and Bearer token, without
    /// a Cookie header. Used by the diagnostic probe.
    pub(crate) fn apply_bearer_only_headers(
        &self,
        mut request: RequestBuilder,
    ) -> Result<RequestBuilder, SessionAuthError> {
        request = request
            .header(ACCEPT, "application/json")
            .header(REFERER, SESSION_REFERER)
            .header(ORIGIN, "https://kick.com")
            .header("x-app-platform", "web");
        if let Some(token) = self.session_token() {
            let mut bearer = Zeroizing::new(format!("Bearer {}", token.as_str()));
            let mut header = HeaderValue::from_str(bearer.as_str())
                .map_err(|_| SessionAuthError::InvalidCookie)?;
            header.set_sensitive(true);
            request = request.header(AUTHORIZATION, header);
            bearer.zeroize();
        }
        Ok(request)
    }

    /// Applies the observed web-client authentication headers to a request.
    ///
    /// The Cookie header keeps the browser export's encoding. The Bearer value
    /// is the percent-decoded `session_token` used by Kick's web client.
    pub(crate) fn apply_auth_headers(
        &self,
        mut request: RequestBuilder,
    ) -> Result<RequestBuilder, SessionAuthError> {
        request = request
            .header(COOKIE, self.header_value()?)
            .header(ACCEPT, "application/json")
            .header(REFERER, SESSION_REFERER)
            .header("x-app-platform", "web");
        if let Some(token) = self.session_token() {
            let mut bearer = Zeroizing::new(format!("Bearer {}", token.as_str()));
            let mut header = HeaderValue::from_str(bearer.as_str())
                .map_err(|_| SessionAuthError::InvalidCookie)?;
            header.set_sensitive(true);
            request = request.header(AUTHORIZATION, header);
            bearer.zeroize();
        }
        Ok(request)
    }
}

impl fmt::Debug for SessionCookie {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SessionCookie(<redacted>)")
    }
}

/// A minimal identity returned after session validation.
///
/// Only `id` and `username` are retained. Other response fields are ignored,
/// and both identity values are redacted from `Debug` output.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionUser {
    pub id: String,
    pub username: String,
}

impl fmt::Debug for SessionUser {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionUser")
            .field("id", &"<redacted>")
            .field("username", &"<redacted>")
            .finish()
    }
}

/// Safe failures for pasted-cookie validation.
///
/// This type contains status and transport classes only. It never stores a
/// request header, response body, username, or other response value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionAuthError {
    InvalidCookie,
    Transport(TransportFailure),
    HttpStatus(u16),
    ResponseTooLarge,
    InvalidResponse,
    InvalidIdentity,
    UnrecognizedResponse(Option<Vec<String>>),
}

impl fmt::Display for SessionAuthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCookie => {
                formatter.write_str("pasted Cookie header is malformed or too large")
            }
            Self::Transport(TransportFailure::Timeout) => {
                formatter.write_str("Kick session validation timed out")
            }
            Self::Transport(TransportFailure::Connection) => {
                formatter.write_str("could not connect to Kick session endpoint")
            }
            Self::Transport(TransportFailure::Request) => {
                formatter.write_str("Kick session request could not be sent")
            }
            Self::Transport(TransportFailure::ResponseBody) => {
                formatter.write_str("Kick session response could not be read")
            }
            Self::Transport(TransportFailure::Other) => {
                formatter.write_str("Kick session validation failed")
            }
            Self::HttpStatus(status) => {
                write!(formatter, "Kick session endpoint returned HTTP {status}")
            }
            Self::ResponseTooLarge => {
                formatter.write_str("Kick session response exceeded the local size limit")
            }
            Self::InvalidResponse => {
                formatter.write_str("Kick returned a non-JSON or malformed session response")
            }
            Self::InvalidIdentity => {
                formatter.write_str(
                    "Kick response did not contain a usable account ID and username; values were withheld",
                )
            }
            Self::UnrecognizedResponse(Some(keys)) if keys.is_empty() => formatter.write_str(
                "Kick returned an empty JSON object for the current-user check; sign-in could not be verified",
            ),
            Self::UnrecognizedResponse(Some(keys)) => {
                write!(
                    formatter,
                    "Kick returned an unrecognized session schema (top-level keys: {})",
                    keys.join(", ")
                )
            }
            Self::UnrecognizedResponse(None) => {
                formatter.write_str("Kick returned a JSON value that is not a session object")
            }
        }
    }
}

impl std::error::Error for SessionAuthError {}

impl KickClient {
    /// Validates the pasted Cookie header against Kick's current-user route.
    ///
    /// This method sends the Cookie header to the fixed `kick.com` origin only.
    /// It does not use the client's bearer token, follow redirects, or include
    /// response values in errors. The response body is capped at 64 KiB and is
    /// reduced to the allowlisted `id` and `username` fields.
    pub async fn validate_session(
        &self,
        cookie: &SessionCookie,
    ) -> Result<SessionUser, SessionAuthError> {
        let request = session_user_request(self, cookie)?;
        self.validate_session_request(request).await
    }

    /// Diagnostic variant that sends the Bearer token without a Cookie header.
    pub async fn validate_session_bearer_only(
        &self,
        cookie: &SessionCookie,
    ) -> Result<SessionUser, SessionAuthError> {
        let endpoint =
            Url::parse(SESSION_USER_URL).map_err(|_| SessionAuthError::InvalidResponse)?;
        let request = cookie.apply_bearer_only_headers(self.http.get(endpoint))?;
        self.validate_session_request(request).await
    }

    async fn validate_session_request(
        &self,
        request: RequestBuilder,
    ) -> Result<SessionUser, SessionAuthError> {
        let response = request
            .timeout(SESSION_REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|error| SessionAuthError::Transport(classify_transport(&error)))?;

        if !response.status().is_success() {
            return Err(SessionAuthError::HttpStatus(response.status().as_u16()));
        }
        if !has_json_content_type(response.headers()) {
            return Err(SessionAuthError::InvalidResponse);
        }

        let mut body = limited_session_body(response).await?;
        let root_keys = safe_session_response_keys(&body);
        let parsed = serde_json::from_slice::<SessionUserWire>(&body);
        let result = match parsed {
            Ok(wire) => SessionUser::try_from(wire),
            Err(_) => Err(SessionAuthError::UnrecognizedResponse(root_keys)),
        };
        body.zeroize();
        result
    }
}

/// Summarizes only top-level JSON object keys. Values are skipped by `IgnoredAny` and
/// are never retained in diagnostics; keys are filtered and capped before display.
fn safe_session_response_keys(body: &[u8]) -> Option<Vec<String>> {
    let object = serde_json::from_slice::<BTreeMap<String, IgnoredAny>>(body).ok()?;
    Some(
        object
            .keys()
            .take(8)
            .map(|key| {
                key.chars()
                    .filter(|character| {
                        character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
                    })
                    .take(32)
                    .collect::<String>()
            })
            .filter(|key| !key.is_empty())
            .collect(),
    )
}

fn session_user_request(
    client: &KickClient,
    cookie: &SessionCookie,
) -> Result<RequestBuilder, SessionAuthError> {
    let endpoint = Url::parse(SESSION_USER_URL).map_err(|_| SessionAuthError::InvalidResponse)?;
    cookie.apply_auth_headers(client.http.get(endpoint))
}

/// Decodes `%XX` escapes the way the web client's cookie reader does.
///
/// Malformed escapes and invalid UTF-8 fall back to the original cookie value,
/// matching the web reader's defensive behavior. The result is used only for
/// the Bearer header; the Cookie header keeps the original encoding.
fn percent_decode_cookie_value(value: &str) -> Zeroizing<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
            {
                decoded.push((high << 4) | low);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    match String::from_utf8(decoded) {
        Ok(decoded) => Zeroizing::new(decoded),
        Err(error) => {
            let mut invalid = error.into_bytes();
            invalid.zeroize();
            Zeroizing::new(value.to_owned())
        }
    }
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn normalize_cookie_header(raw: &str) -> Option<Zeroizing<String>> {
    if raw.is_empty()
        || raw.len() > MAX_SESSION_COOKIE_BYTES
        || !raw.is_ascii()
        || raw.bytes().any(|byte| byte <= 0x1f || byte == 0x7f)
    {
        return None;
    }

    let mut input = raw.trim_matches(' ');
    if input
        .get(..b"Cookie:".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("Cookie:"))
    {
        input = input[b"Cookie:".len()..].trim_start_matches(' ');
    }

    let mut normalized = Zeroizing::new(String::with_capacity(input.len()));
    let mut pair_count = 0usize;
    for raw_pair in input.split(';') {
        let pair = raw_pair.trim_matches(' ');
        let (name, value) = pair.split_once('=')?;
        if name.is_empty()
            || !name.bytes().all(is_cookie_name_byte)
            || !is_valid_cookie_value(value)
        {
            return None;
        }

        if pair_count > 0 {
            normalized.push_str("; ");
        }
        normalized.push_str(name);
        normalized.push('=');
        normalized.push_str(value);
        pair_count += 1;

        if normalized.len() > MAX_SESSION_COOKIE_BYTES {
            return None;
        }
    }

    (pair_count > 0).then_some(normalized)
}

fn is_cookie_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b':'
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

fn is_valid_cookie_value(value: &str) -> bool {
    let bytes = value.as_bytes();
    let cookie_octets = if bytes.len() >= 2 && bytes.first() == Some(&b'"') {
        if bytes.last() != Some(&b'"') {
            return false;
        }
        &bytes[1..bytes.len() - 1]
    } else {
        bytes
    };

    cookie_octets.iter().copied().all(is_cookie_octet)
}

fn is_cookie_octet(byte: u8) -> bool {
    byte == 0x21
        || (0x23..=0x2b).contains(&byte)
        || (0x2d..=0x3a).contains(&byte)
        || (0x3c..=0x5b).contains(&byte)
        || (0x5d..=0x7e).contains(&byte)
}

fn has_json_content_type(headers: &HeaderMap) -> bool {
    let Some(content_type) = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };

    let media_type = content_type.split(';').next().unwrap_or_default().trim();
    media_type.eq_ignore_ascii_case("application/json")
        || media_type
            .rsplit_once('/')
            .is_some_and(|(_, subtype)| subtype.to_ascii_lowercase().ends_with("+json"))
}

async fn limited_session_body(response: Response) -> Result<Vec<u8>, SessionAuthError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_SESSION_RESPONSE_BYTES as u64)
    {
        return Err(SessionAuthError::ResponseTooLarge);
    }

    let initial_capacity = response
        .content_length()
        .unwrap_or(8 * 1024)
        .min(MAX_SESSION_RESPONSE_BYTES as u64) as usize;
    let mut body = Vec::with_capacity(initial_capacity);
    let mut response = response;
    loop {
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => return Ok(body),
            Err(error) => {
                body.zeroize();
                return Err(SessionAuthError::Transport(classify_transport(&error)));
            }
        };
        if body.len().saturating_add(chunk.len()) > MAX_SESSION_RESPONSE_BYTES {
            body.zeroize();
            return Err(SessionAuthError::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
}

#[derive(Deserialize)]
struct SessionUserWire {
    id: SessionUserIdWire,
    username: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum SessionUserIdWire {
    Number(u64),
    Text(String),
}

impl TryFrom<SessionUserWire> for SessionUser {
    type Error = SessionAuthError;

    fn try_from(wire: SessionUserWire) -> Result<Self, Self::Error> {
        let id = match wire.id {
            SessionUserIdWire::Number(value) => value.to_string(),
            SessionUserIdWire::Text(value) => value,
        };
        if !valid_identity_text(&id, MAX_SESSION_ID_BYTES)
            || !valid_identity_text(&wire.username, MAX_SESSION_USERNAME_BYTES)
        {
            return Err(SessionAuthError::InvalidIdentity);
        }
        Ok(Self {
            id,
            username: wire.username,
        })
    }
}

fn valid_identity_text(value: &str, max_bytes: usize) -> bool {
    !value.trim().is_empty() && value.len() <= max_bytes && !value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AccessToken;
    use reqwest::header::AUTHORIZATION;

    #[test]
    fn raw_session_token_input_preserves_percent_characters() {
        let raw = SessionCookie::from_session_token_raw("a%2Fb").unwrap();
        assert_eq!(raw.0.as_str(), "session_token=a%252Fb");
        assert_eq!(
            raw.session_token().as_deref().map(String::as_str),
            Some("a%2Fb")
        );
    }

    #[test]
    fn session_token_input_builds_a_minimal_cookie() {
        let cookie = SessionCookie::from_session_token("abc123").unwrap();
        assert_eq!(cookie.0.as_str(), "session_token=abc123");
        assert_eq!(
            cookie.session_token().as_deref().map(String::as_str),
            Some("abc123")
        );

        let encoded = SessionCookie::from_session_token("a;b c").unwrap();
        assert_eq!(encoded.0.as_str(), "session_token=a%3Bb%20c");
        assert_eq!(
            encoded.session_token().as_deref().map(String::as_str),
            Some("a;b c")
        );

        let already_encoded = SessionCookie::from_session_token("a%2Fb").unwrap();
        assert_eq!(already_encoded.0.as_str(), "session_token=a%2Fb");
        assert_eq!(
            already_encoded
                .session_token()
                .as_deref()
                .map(String::as_str),
            Some("a/b")
        );

        assert!(SessionCookie::from_session_token("").is_err());
        assert!(SessionCookie::from_session_token("a\nb").is_err());
    }

    #[test]
    fn cookie_parser_accepts_cookie_field_prefix_and_multiple_pairs() {
        let cookie = SessionCookie::parse(
            "Cookie: fixture_one=alpha123; fixture_two=beta%2Fgamma; kick:region=v1",
        )
        .unwrap();
        let header = cookie.header_value().unwrap();

        assert!(header.is_sensitive());
        assert_eq!(
            header.to_str().unwrap(),
            "fixture_one=alpha123; fixture_two=beta%2Fgamma; kick:region=v1"
        );
        assert_eq!(format!("{cookie:?}"), "SessionCookie(<redacted>)");
        assert!(!format!("{header:?}").contains("alpha123"));
    }

    #[test]
    fn cookie_parser_rejects_malformed_control_and_oversized_values_without_echoing_them() {
        for input in [
            String::new(),
            "Cookie:".to_owned(),
            "missing_equals".to_owned(),
            "bad name=value".to_owned(),
            "name=value;".to_owned(),
            "fixture=marker-value; malformed".to_owned(),
            "name=value\r\nX-Evil: yes".to_owned(),
            "name=non-ascii-あ".to_owned(),
            "name=contains space".to_owned(),
            format!("name={}", "x".repeat(MAX_SESSION_COOKIE_BYTES)),
        ] {
            let error = SessionCookie::parse_bytes(input.as_bytes()).unwrap_err();
            assert_eq!(error, SessionAuthError::InvalidCookie);
            assert!(!format!("{error:?}").contains("marker-value"));
            assert!(!error.to_string().contains("marker-value"));
        }
    }

    #[test]
    fn session_request_is_fixed_to_kick_and_uses_only_the_session_cookie_bearer() {
        let mut client = KickClient::unauthenticated_for_probe().unwrap();
        client.access_token = Some(AccessToken::new("local-test-bearer").unwrap());
        let cookie = SessionCookie::parse("fixture=local-test-value").unwrap();
        let request = session_user_request(&client, &cookie)
            .unwrap()
            .build()
            .unwrap();

        assert_eq!(request.method(), reqwest::Method::GET);
        assert_eq!(request.url().scheme(), "https");
        assert_eq!(request.url().host_str(), Some("kick.com"));
        assert_eq!(request.url().path(), "/api/v1/user");
        assert!(request.url().query().is_none());
        assert!(request.url().fragment().is_none());
        assert!(request.headers().get(AUTHORIZATION).is_none());
        assert!(request.headers().get(COOKIE).unwrap().is_sensitive());

        let session_cookie =
            SessionCookie::parse("session_token=fixture-session-token; kick_session=fixture")
                .unwrap();
        let session_request = session_user_request(&client, &session_cookie)
            .unwrap()
            .build()
            .unwrap();
        let authorization = session_request
            .headers()
            .get(AUTHORIZATION)
            .expect("session_token must be sent as a bearer header");
        assert_eq!(authorization, "Bearer fixture-session-token");
        assert!(authorization.is_sensitive());
        assert_eq!(
            session_request.headers().get("x-app-platform").unwrap(),
            "web"
        );

        let public_url = Url::parse("https://kick.com/api/search?searched_word=fixture").unwrap();
        let public_request = client.anonymous_request(public_url).build().unwrap();
        assert!(public_request.headers().get(AUTHORIZATION).is_none());
        assert!(public_request.headers().get(COOKIE).is_none());
    }

    #[test]
    fn session_token_bearer_decodes_percent_escapes_without_changing_the_cookie_header() {
        let client = KickClient::unauthenticated_for_probe().unwrap();
        let cookie =
            SessionCookie::parse("session_token=abc%3D%2Fdef; kick_session=fixture").unwrap();
        let request = session_user_request(&client, &cookie)
            .unwrap()
            .build()
            .unwrap();

        assert_eq!(
            request.headers().get(AUTHORIZATION).unwrap(),
            "Bearer abc=/def"
        );
        assert_eq!(
            request.headers().get(COOKIE).unwrap(),
            "session_token=abc%3D%2Fdef; kick_session=fixture"
        );

        let malformed = SessionCookie::parse("session_token=abc%ZZ").unwrap();
        let malformed_request = session_user_request(&client, &malformed)
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(
            malformed_request.headers().get(AUTHORIZATION).unwrap(),
            "Bearer abc%ZZ"
        );
    }

    #[test]
    fn identity_dto_keeps_only_capped_id_and_username_and_redacts_debug() {
        let fixture = br#"{
            "id": 42,
            "username": "fixture_user",
            "email": "fixture@example.invalid",
            "access_token": "local-fixture-only"
        }"#;
        let parsed = serde_json::from_slice::<SessionUserWire>(fixture).unwrap();
        let user = SessionUser::try_from(parsed).unwrap();

        assert_eq!(user.id, "42");
        assert_eq!(user.username, "fixture_user");
        let debug = format!("{user:?}");
        assert!(!debug.contains("fixture_user"));
        assert!(!debug.contains("fixture@example.invalid"));
        assert!(!debug.contains("local-fixture-only"));

        let too_long = SessionUserWire {
            id: SessionUserIdWire::Text("x".repeat(MAX_SESSION_ID_BYTES + 1)),
            username: "fixture_user".to_owned(),
        };
        assert_eq!(
            SessionUser::try_from(too_long),
            Err(SessionAuthError::InvalidIdentity)
        );
    }

    #[test]
    fn unrecognized_response_diagnostic_keeps_only_sanitized_keys() {
        let body = br#"{"user":{"email":"private@example.invalid"},"unsafe key":"private-value"}"#;
        let keys = safe_session_response_keys(body).unwrap();
        let error = SessionAuthError::UnrecognizedResponse(Some(keys));
        let diagnostic = error.to_string();

        assert!(diagnostic.contains("top-level keys: unsafekey, user"));
        assert!(!diagnostic.contains("private@example.invalid"));
        assert!(!diagnostic.contains("private-value"));
    }

    #[test]
    fn empty_user_response_diagnostic_does_not_claim_cookie_is_invalid() {
        let error = SessionAuthError::UnrecognizedResponse(Some(Vec::new()));
        let diagnostic = error.to_string();

        assert!(diagnostic.contains("empty JSON object"));
        assert!(diagnostic.contains("sign-in could not be verified"));
        assert!(!diagnostic.contains("invalid cookie"));
        assert!(!diagnostic.contains("empty session object"));
    }

    #[test]
    fn content_type_accepts_json_and_vendor_json_only() {
        let make_headers = |content_type: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(CONTENT_TYPE, HeaderValue::from_str(content_type).unwrap());
            headers
        };

        assert!(has_json_content_type(&make_headers(
            "application/json; charset=utf-8"
        )));
        assert!(has_json_content_type(&make_headers(
            "application/vnd.kick+json"
        )));
        assert!(!has_json_content_type(&make_headers("text/html")));
    }
}
