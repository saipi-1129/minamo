//! Experimental loopback HLS relay.
//!
//! Proxies the IVS master playlist and its media playlists through
//! `127.0.0.1`, rewriting every URI so AVPlayer fetches through this process.
//! The media playlist's target duration is lowered to the actual 2 s segment
//! size, which makes AVPlayer refresh the live playlist sooner.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub struct LocalRelay {
    port: u16,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

/// Last successfully rewritten playlist per URL. On a flaky network the relay
/// keeps serving it while the upstream fetch fails, so AVPlayer does not see a
/// hard error mid-playback.
type PlaylistCache = Arc<Mutex<HashMap<String, String>>>;

impl LocalRelay {
    pub fn start(master_url: &str) -> Option<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).ok()?;
        let port = listener.local_addr().ok()?.port();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = stop.clone();
        let master = master_url.to_owned();
        let cache: PlaylistCache = Arc::new(Mutex::new(HashMap::new()));
        let handle = thread::spawn(move || {
            for incoming in listener.incoming() {
                if stop_for_thread.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(stream) = incoming else { continue };
                let master = master.clone();
                let cache = cache.clone();
                thread::spawn(move || {
                    let _ = handle_connection(stream, &master, &cache);
                });
            }
        });
        Some(Self {
            port,
            stop,
            handle: Some(handle),
        })
    }

    /// Wraps an absolute media URL so AVPlayer fetches it through the relay.
    pub fn proxy_url(&self, url: &str) -> String {
        format!(
            "http://127.0.0.1:{}/proxy?u={}",
            self.port,
            percent_encode(url)
        )
    }
}

impl Drop for LocalRelay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Wake the accept loop so the listener thread can exit.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn handle_connection(
    stream: TcpStream,
    master_url: &str,
    cache: &PlaylistCache,
) -> std::io::Result<()> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let clone = stream.try_clone()?;
    let mut reader = BufReader::new(clone);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut range: Option<String> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some(value) = line
            .strip_prefix("Range:")
            .or_else(|| line.strip_prefix("range:"))
        {
            range = Some(value.trim().to_owned());
        }
    }

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    if method != "GET" || !target.starts_with("/proxy?u=") {
        return write_response(&stream, 404, "text/plain", b"not found", None);
    }
    let encoded = &target["/proxy?u=".len()..];
    let Some(url) = percent_decode(encoded) else {
        return write_response(&stream, 400, "text/plain", b"bad url", None);
    };
    if !url_allowed(&url) {
        return write_response(&stream, 403, "text/plain", b"forbidden host", None);
    }

    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
    {
        Ok(client) => client,
        Err(_) => return write_response(&stream, 502, "text/plain", b"client", None),
    };
    let mut request = client.get(&url);
    if let Some(range) = range.as_deref() {
        request = request.header(reqwest::header::RANGE, range);
    }
    let is_playlist_url = url.contains(".m3u8");
    let response = match request.send() {
        Ok(response) => response,
        Err(_) => {
            if is_playlist_url
                && let Some(cached) = cache.lock().ok().and_then(|cache| cache.get(&url).cloned())
            {
                return write_response(
                    &stream,
                    200,
                    "application/vnd.apple.mpegurl",
                    cached.as_bytes(),
                    None,
                );
            }
            return write_response(&stream, 502, "text/plain", b"upstream", None);
        }
    };
    let status = response.status().as_u16();
    if is_playlist_url && !(200..300).contains(&status) {
        if let Some(cached) = cache.lock().ok().and_then(|cache| cache.get(&url).cloned()) {
            return write_response(
                &stream,
                200,
                "application/vnd.apple.mpegurl",
                cached.as_bytes(),
                None,
            );
        }
        return write_response(&stream, status, "text/plain", b"upstream", None);
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = match response.bytes() {
        Ok(body) => body.to_vec(),
        Err(_) => return write_response(&stream, 502, "text/plain", b"body", None),
    };

    let is_playlist = url.contains(".m3u8")
        || body.starts_with(b"#EXTM3U")
        || content_type
            .as_deref()
            .is_some_and(|value| value.contains("mpegurl"));
    if is_playlist {
        let text = String::from_utf8_lossy(&body);
        let base = reqwest::Url::parse(&url).ok();
        let rewritten = rewrite_playlist(&text, base.as_ref());
        if let Ok(mut cache) = cache.lock() {
            cache.insert(url.clone(), rewritten.clone());
        }
        return write_response(
            &stream,
            200,
            "application/vnd.apple.mpegurl",
            rewritten.as_bytes(),
            None,
        );
    }
    let content_type = content_type
        .as_deref()
        .unwrap_or("application/octet-stream");
    write_response(&stream, status, content_type, &body, Some(body.len()))
}

fn rewrite_playlist(text: &str, base: Option<&reqwest::Url>) -> String {
    // Master playlists must keep their variant playlists proxied so the media
    // playlist can be rewritten. Media playlists leave segments on the origin
    // CDN, so the relay only carries the small playlist requests.
    let is_master = text.contains("#EXT-X-STREAM-INF");
    let mut lines = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('#') {
            // Lower the target duration to the real 2 s segment size so AVPlayer
            // refreshes the live playlist sooner.
            if let Some(rest) = trimmed.strip_prefix("#EXT-X-TARGETDURATION:") {
                let _ = rest;
                lines.push("#EXT-X-TARGETDURATION:3".to_owned());
            } else {
                lines.push(trimmed.to_owned());
            }
            continue;
        }
        let absolute = base
            .and_then(|base| base.join(trimmed).ok())
            .map(|url| url.to_string())
            .unwrap_or_else(|| trimmed.to_owned());
        if is_master {
            lines.push(format!("/proxy?u={}", percent_encode(&absolute)));
        } else {
            lines.push(absolute);
        }
    }
    let mut result = lines.join("\n");
    result.push('\n');
    result
}

/// Only Kick and Amazon IVS hosts may be proxied; the relay must not become a
/// general-purpose local proxy.
fn url_allowed(value: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(value) else {
        return false;
    };
    if !matches!(url.scheme(), "https" | "http") {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    host == "kick.com"
        || host.ends_with(".kick.com")
        || host == "live-video.net"
        || host.ends_with(".live-video.net")
}

fn write_response(
    mut stream: &TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    content_length: Option<usize>,
) -> std::io::Result<()> {
    let length = content_length.unwrap_or(body.len());
    let head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nContent-Length: {length}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
        status_text(status)
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    let _ = stream.flush();
    Ok(())
}

fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        206 => "Partial Content",
        400 => "Bad Request",
        404 => "Not Found",
        502 => "Bad Gateway",
        _ => "OK",
    }
}

fn percent_encode(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            _ => {
                encoded.push('%');
                encoded.push(HEX[(byte >> 4) as usize] as char);
                encoded.push(HEX[(byte & 0x0f) as usize] as char);
            }
        }
    }
    encoded
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = bytes.get(index + 1..index + 3)?;
            let text = std::str::from_utf8(hex).ok()?;
            decoded.push(u8::from_str_radix(text, 16).ok()?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_playlist_lowers_target_duration_and_keeps_segments_direct() {
        let base = reqwest::Url::parse("https://example.test/path/media.m3u8").unwrap();
        let input = "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:2.0,\nseg-1.ts\n";
        let output = rewrite_playlist(input, Some(&base));
        assert!(output.contains("#EXT-X-TARGETDURATION:3"));
        assert!(output.contains("https://example.test/path/seg-1.ts"));
        assert!(!output.contains("/proxy?u="));
    }

    #[test]
    fn master_playlist_proxies_variant_playlists() {
        let base = reqwest::Url::parse("https://example.test/path/master.m3u8").unwrap();
        let input = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\n720p.m3u8\n";
        let output = rewrite_playlist(input, Some(&base));
        assert!(output.contains("/proxy?u=https%3A%2F%2Fexample.test%2Fpath%2F720p.m3u8"));
    }

    #[test]
    fn relay_only_allows_kick_and_ivs_hosts() {
        assert!(url_allowed(
            "https://fa723fc1b171.us-west-2.playback.live-video.net/api/v1/x.m3u8"
        ));
        assert!(url_allowed("https://kick.com/api/v2/channels/x"));
        assert!(!url_allowed("https://example.com/x.m3u8"));
        assert!(!url_allowed("file:///etc/passwd"));
        assert!(!url_allowed("http://127.0.0.1:8080/x"));
    }

    #[test]
    fn percent_round_trip() {
        let value = "https://a.test/x.m3u8?token=a+b/c=d";
        assert_eq!(
            percent_decode(&percent_encode(value)).as_deref(),
            Some(value)
        );
    }
}
