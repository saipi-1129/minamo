//! Anonymous channel search through Kick's observed mobile search route.

use kick_core::StreamSummary;
use reqwest::Url;
use serde::Deserialize;

use crate::{KickApiError, KickClient};

const SEARCH_ENDPOINT: &str = "https://kick.com/api/search";
const MAX_SEARCH_QUERY_BYTES: usize = 256;
const MAX_SEARCH_RESULTS: usize = 50;

impl KickClient {
    /// Searches Kick channels without account credentials.
    ///
    /// This uses the observed mobile search endpoint and reads only the
    /// channel display fields needed by the app. It deliberately does not
    /// infer live state from search results; channel detail is authoritative
    /// for determining whether a channel is currently live.
    pub async fn anonymous_search(&self, query: &str) -> Result<Vec<StreamSummary>, KickApiError> {
        let url = search_url(query)?;
        let response: SearchResponse = self.anonymous_get_json(url).await?;
        Ok(search_summaries(response.channels))
    }
}

fn search_url(query: &str) -> Result<Url, KickApiError> {
    let query = normalize_query(query)?;
    let mut url = Url::parse(SEARCH_ENDPOINT)
        .map_err(|_| KickApiError::InvalidQuery("search endpoint URL is invalid"))?;
    url.query_pairs_mut().append_pair("searched_word", query);
    Ok(url)
}

fn normalize_query(query: &str) -> Result<&str, KickApiError> {
    if query.len() > MAX_SEARCH_QUERY_BYTES {
        return Err(KickApiError::InvalidQuery(
            "search query exceeds the 256-byte local limit",
        ));
    }
    if query.chars().any(char::is_control) {
        return Err(KickApiError::InvalidQuery(
            "search query contains a control character",
        ));
    }

    let query = query.trim();
    if query.is_empty() {
        return Err(KickApiError::InvalidQuery("search query must not be empty"));
    }
    Ok(query)
}

fn search_summaries(channels: Vec<SearchChannelWire>) -> Vec<StreamSummary> {
    channels
        .into_iter()
        .filter_map(SearchChannelWire::into_summary)
        .take(MAX_SEARCH_RESULTS)
        .collect()
}

#[derive(Deserialize)]
struct SearchResponse {
    channels: Vec<SearchChannelWire>,
}

#[derive(Deserialize)]
struct SearchChannelWire {
    slug: Option<String>,
    user: Option<SearchUserWire>,
}

#[derive(Deserialize)]
struct SearchUserWire {
    username: Option<String>,
    #[serde(rename = "profilePic")]
    profile_pic: Option<String>,
}

impl SearchChannelWire {
    fn into_summary(self) -> Option<StreamSummary> {
        let channel_slug = self.slug?;
        // Reuse the existing path-segment validator without making a request.
        crate::channel_lookup_url(&channel_slug).ok()?;

        let user = self.user?;
        let channel_name = user.username?;
        if channel_name.trim().is_empty() {
            return None;
        }

        Some(StreamSummary {
            channel_slug,
            channel_name,
            // The observed channel result has no title field.
            title: String::new(),
            category: None,
            // Follower counts are not viewer counts, so leave this absent.
            viewer_count: None,
            thumbnail_url: None,
            profile_picture_url: safe_thumbnail_url(user.profile_pic),
        })
    }
}

fn safe_thumbnail_url(value: Option<String>) -> Option<String> {
    let value = value?;
    let parsed = Url::parse(&value).ok()?;
    if parsed.scheme() == "https"
        && parsed.host_str().is_some()
        && parsed.username().is_empty()
        && parsed.password().is_none()
    {
        Some(value)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AccessToken;
    use reqwest::header::{AUTHORIZATION, COOKIE};

    #[test]
    fn search_url_encodes_the_observed_query_key_and_value() {
        let url = search_url("  live cats 東京 +/  ").unwrap();
        let pairs = url.query_pairs().collect::<Vec<_>>();

        assert_eq!(url.path(), "/api/search");
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0, "searched_word");
        assert_eq!(pairs[0].1, "live cats 東京 +/");
        assert!(!url.query().unwrap().contains(' '));
    }

    #[test]
    fn search_url_rejects_empty_oversized_and_control_input() {
        assert_eq!(
            search_url("   ").unwrap_err(),
            KickApiError::InvalidQuery("search query must not be empty")
        );
        assert_eq!(
            search_url(&"x".repeat(MAX_SEARCH_QUERY_BYTES + 1)).unwrap_err(),
            KickApiError::InvalidQuery("search query exceeds the 256-byte local limit")
        );
        assert_eq!(
            search_url("cats\nstreams").unwrap_err(),
            KickApiError::InvalidQuery("search query contains a control character")
        );
    }

    #[test]
    fn channel_search_maps_only_observed_display_fields() {
        let payload = r#"
        {
          "channels": [{
            "slug": "sample-channel",
            "user": {
              "username": "sample_user",
              "profilePic": "https://images.example.invalid/profile.png",
              "bio": "ignored",
              "id": 42
            },
            "followersCount": 900,
            "isLive": true,
            "recentCategories": [],
            "viewer_count": 77,
            "title": "not present in observed channel schema"
          }],
          "categories": [],
          "livestreams": null
        }
        "#;
        let response: SearchResponse = serde_json::from_str(payload).unwrap();
        let summaries = search_summaries(response.channels);
        let summary = summaries.first().unwrap();

        assert_eq!(summaries.len(), 1);
        assert_eq!(summary.channel_slug, "sample-channel");
        assert_eq!(summary.channel_name, "sample_user");
        assert!(summary.title.is_empty());
        assert_eq!(summary.category, None);
        assert_eq!(summary.viewer_count, None);
        assert_eq!(
            summary.profile_picture_url.as_deref(),
            Some("https://images.example.invalid/profile.png")
        );
        assert_eq!(summary.thumbnail_url, None);
    }

    #[test]
    fn search_drops_records_without_a_valid_slug_or_display_name() {
        let payload = r#"
        {
          "channels": [
            {"slug": "bad/slug", "user": {"username": "ignored"}},
            {"slug": "valid-slug", "user": {"username": " "}},
            {"slug": "valid-slug", "user": {"username": "visible_name"}}
          ]
        }
        "#;
        let response: SearchResponse = serde_json::from_str(payload).unwrap();
        let summaries = search_summaries(response.channels);

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].channel_slug, "valid-slug");
        assert_eq!(summaries[0].channel_name, "visible_name");
    }

    #[test]
    fn search_caps_returned_results_at_fifty() {
        let channels = (0..(MAX_SEARCH_RESULTS + 1))
            .map(|index| SearchChannelWire {
                slug: Some(format!("sample-{index}")),
                user: Some(SearchUserWire {
                    username: Some(format!("sample_user_{index}")),
                    profile_pic: None,
                }),
            })
            .collect();

        assert_eq!(search_summaries(channels).len(), MAX_SEARCH_RESULTS);
    }

    #[test]
    fn anonymous_search_request_does_not_forward_stored_auth_or_cookies() {
        let client = KickClient::new(AccessToken::new("test-only-token").unwrap()).unwrap();
        let request = client
            .anonymous_request(search_url("sample query").unwrap())
            .build()
            .unwrap();

        assert_eq!(request.method(), reqwest::Method::GET);
        assert!(!request.headers().contains_key(AUTHORIZATION));
        assert!(!request.headers().contains_key(COOKIE));
    }

    #[test]
    fn only_secure_thumbnail_urls_are_retained() {
        assert_eq!(
            safe_thumbnail_url(Some("https://images.example.invalid/a.png".into())),
            Some("https://images.example.invalid/a.png".into())
        );
        assert_eq!(
            safe_thumbnail_url(Some("http://images.example.invalid/a.png".into())),
            None
        );
        assert_eq!(safe_thumbnail_url(Some("javascript:alert(1)".into())), None);
    }
}
