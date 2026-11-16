//! Bounded master-playlist inspection (RFC 8216 section 4.3.4.2).
use crate::{KickApiError, KickClient, classify_transport};
use kick_core::PlaybackSource;
use reqwest::Url;

#[derive(Clone, Debug)]
pub struct PlaybackQuality {
    pub bandwidth: u64,
    pub resolution: Option<(u32, u32)>,
    pub source: PlaybackSource,
}

fn attributes(text: &str) -> Result<Vec<(&str, &str)>, KickApiError> {
    let mut quoted = false;
    let mut start = 0;
    let mut result = Vec::new();
    for (i, c) in text.char_indices() {
        if c == '"' {
            quoted = !quoted;
        }
        if c == ',' && !quoted {
            let (key, value) = text[start..i]
                .split_once('=')
                .ok_or(KickApiError::InvalidJson)?;
            if result.len() >= 64 {
                return Err(KickApiError::ResponseTooLarge);
            }
            result.push((key.trim(), value.trim()));
            start = i + 1;
        }
    }
    if quoted {
        return Err(KickApiError::InvalidJson);
    }
    let (key, value) = text[start..]
        .split_once('=')
        .ok_or(KickApiError::InvalidJson)?;
    result.push((key.trim(), value.trim()));
    Ok(result)
}

pub fn parse_master(base: &str, text: &str) -> Result<Vec<PlaybackQuality>, KickApiError> {
    if text.len() > 1024 * 1024 {
        return Err(KickApiError::ResponseTooLarge);
    }
    if !text.starts_with("#EXTM3U") {
        return Err(KickApiError::InvalidJson);
    }
    let base = Url::parse(base).map_err(|_| KickApiError::InvalidJson)?;
    let mut pending = None;
    let mut qualities = Vec::new();
    for line in text
        .lines()
        .skip(1)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if let Some(attrs) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            if pending.is_some() {
                return Err(KickApiError::InvalidJson);
            }
            if attrs.len() > 16384 {
                return Err(KickApiError::ResponseTooLarge);
            }
            pending = Some(attributes(attrs)?);
        } else if !line.starts_with('#') {
            if let Some(attrs) = pending.take() {
                if qualities.len() >= 32 || line.len() > 16384 {
                    return Err(KickApiError::ResponseTooLarge);
                }
                let bandwidth = attrs
                    .iter()
                    .find(|(k, _)| *k == "BANDWIDTH")
                    .and_then(|(_, v)| v.parse::<u64>().ok())
                    .filter(|v| *v > 0)
                    .ok_or(KickApiError::InvalidJson)?;
                let resolution = attrs
                    .iter()
                    .find(|(k, _)| *k == "RESOLUTION")
                    .and_then(|(_, v)| v.split_once('x'))
                    .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)));
                let url = base.join(line).map_err(|_| KickApiError::InvalidJson)?;
                if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some()
                {
                    return Err(KickApiError::InvalidJson);
                }
                let label = resolution
                    .map(|(w, h)| format!("{w}×{h}"))
                    .unwrap_or_else(|| format!("{} kbps", bandwidth / 1000));
                qualities.push(PlaybackQuality {
                    bandwidth,
                    resolution,
                    source: PlaybackSource {
                        url: url.to_string(),
                        quality_label: Some(label),
                    },
                });
            }
        }
    }
    if pending.is_some() {
        return Err(KickApiError::InvalidJson);
    }
    Ok(qualities)
}

impl KickClient {
    pub async fn playback_qualities(
        &self,
        source: &PlaybackSource,
    ) -> Result<Vec<PlaybackQuality>, KickApiError> {
        let url = Url::parse(&source.url).map_err(|_| KickApiError::InvalidJson)?;
        if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
            return Err(KickApiError::InvalidJson);
        }
        let mut response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| KickApiError::Transport(classify_transport(&e)))?;
        if !response.status().is_success() {
            return Err(KickApiError::HttpStatus(response.status().as_u16()));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| KickApiError::Transport(classify_transport(&e)))?
        {
            if bytes.len().saturating_add(chunk.len()) > 1024 * 1024 {
                return Err(KickApiError::ResponseTooLarge);
            }
            bytes.extend_from_slice(&chunk);
        }
        parse_master(
            &source.url,
            std::str::from_utf8(&bytes).map_err(|_| KickApiError::InvalidJson)?,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quoted_commas_and_relative_variant_are_preserved_without_debug_url() {
        let rows = parse_master("https://example.test/live/master.m3u8?token=master", "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=3000000,CODECS=\"avc1,mp4a\",RESOLUTION=1280x720\n720/index.m3u8?token=variant-secret\n").unwrap();
        assert_eq!(rows[0].resolution, Some((1280, 720)));
        assert_eq!(
            rows[0].source.url,
            "https://example.test/live/720/index.m3u8?token=variant-secret"
        );
        assert!(!format!("{:?}", rows[0]).contains("variant-secret"));
    }
    #[test]
    fn media_playlist_does_not_invent_quality_choices() {
        assert!(
            parse_master(
                "https://example.test/live.m3u8",
                "#EXTM3U\n#EXTINF:2,\nsegment.ts\n"
            )
            .unwrap()
            .is_empty()
        );
    }
    #[test]
    fn rejects_invalid_or_unsafe_variant() {
        for body in [
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=3\nhttp://example.test/a\n",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=3\n",
        ] {
            assert!(parse_master("https://example.test/a", body).is_err());
        }
    }
}
