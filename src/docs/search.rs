//! Web search backends used to find documentation pages.
//!
//! Three are supported: DuckDuckGo (no key, but it sometimes answers automated
//! searches with a CAPTCHA), Brave Search (API key) and SearXNG (a server the
//! user runs). All of them reduce to a list of [`SearchResult`]s.

use scraper::{Html, Selector};
use serde_json::Value;
use url::Url;

use crate::config::{DocsSearchBackend, DocsSettings};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("{0}")]
    NotConfigured(String),
    #[error("could not reach the search service: {0}")]
    Network(String),
    #[error("the search service answered HTTP {status}: {message}")]
    Http { status: u16, message: String },
    /// The service refused an automated search (CAPTCHA, bad key, rate limit).
    #[error("{0}")]
    Refused(String),
    #[error("unexpected search response: {0}")]
    Unexpected(String),
}

/// Where the hosted backends live. Overridable so tests can use mock servers.
#[derive(Clone, Debug)]
pub struct SearchEndpoints {
    pub duckduckgo_html: String,
    pub brave_web: String,
}

impl Default for SearchEndpoints {
    fn default() -> Self {
        Self {
            duckduckgo_html: "https://html.duckduckgo.com/html/".to_string(),
            brave_web: "https://api.search.brave.com/res/v1/web/search".to_string(),
        }
    }
}

const RESULTS_WANTED: usize = 8;

/// Searches with the backend chosen in `docs`.
pub async fn search(
    http: &reqwest::Client,
    endpoints: &SearchEndpoints,
    docs: &DocsSettings,
    brave_api_key: &str,
    query: &str,
) -> Result<Vec<SearchResult>, SearchError> {
    match docs.backend {
        DocsSearchBackend::DuckDuckGo => {
            search_duckduckgo(http, &endpoints.duckduckgo_html, query).await
        }
        DocsSearchBackend::Brave => {
            search_brave(http, &endpoints.brave_web, brave_api_key, query).await
        }
        DocsSearchBackend::SearxNg => search_searxng(http, &docs.searxng_url, query).await,
    }
}

fn network_error(error: reqwest::Error) -> SearchError {
    SearchError::Network(error.without_url().to_string())
}

fn short(text: &str) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() > 200 {
        format!("{}…", text.chars().take(200).collect::<String>())
    } else {
        text
    }
}

// ---------------------------------------------------------------------------
// DuckDuckGo
// ---------------------------------------------------------------------------

async fn search_duckduckgo(
    http: &reqwest::Client,
    url: &str,
    query: &str,
) -> Result<Vec<SearchResult>, SearchError> {
    let form_body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("q", query)
        .append_pair("b", "")
        .append_pair("kl", "wt-wt")
        .finish();
    let response = http
        .post(url)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("accept", "text/html")
        .body(form_body)
        .send()
        .await
        .map_err(network_error)?;
    let status = response.status();
    let body = response.text().await.map_err(network_error)?;
    if !status.is_success() && status.as_u16() != 202 {
        return Err(SearchError::Http {
            status: status.as_u16(),
            message: short(&body),
        });
    }
    parse_duckduckgo_html(&body)
}

/// DuckDuckGo wraps result links as `//duckduckgo.com/l/?uddg=<real url>&...`.
fn unwrap_duckduckgo_redirect(href: &str) -> Option<String> {
    let absolute = if let Some(rest) = href.strip_prefix("//") {
        format!("https://{rest}")
    } else {
        href.to_string()
    };
    let parsed = Url::parse(&absolute).ok()?;
    let is_redirector = parsed
        .host_str()
        .is_some_and(|host| host.ends_with("duckduckgo.com"));
    if !is_redirector {
        return Some(parsed.to_string());
    }
    if parsed.path() == "/y.js" {
        return None; // an advertisement
    }
    parsed
        .query_pairs()
        .find(|(key, _)| key == "uddg")
        .map(|(_, target)| target.into_owned())
}

pub fn parse_duckduckgo_html(html: &str) -> Result<Vec<SearchResult>, SearchError> {
    if html.contains("Select all squares")
        || html.contains("anomaly-modal")
        || html.contains("unusual traffic")
    {
        return Err(SearchError::Refused(
            "DuckDuckGo asked for a CAPTCHA, which it does for automated searches from some networks. \
             Use Brave Search or your own SearXNG server in Settings > Docs instead."
                .to_string(),
        ));
    }

    let document = Html::parse_document(html);
    let container = Selector::parse("div.result__body").expect("valid selector");
    let title_link = Selector::parse("a.result__a, h2 a").expect("valid selector");
    let snippet = Selector::parse(".result__snippet").expect("valid selector");

    let mut results = Vec::new();
    for result in document.select(&container) {
        let Some(link) = result.select(&title_link).next() else {
            continue;
        };
        let Some(href) = link.value().attr("href") else {
            continue;
        };
        let Some(url) = unwrap_duckduckgo_redirect(href) else {
            continue;
        };
        let title = link
            .text()
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let snippet_text = result
            .select(&snippet)
            .next()
            .map(|element| {
                element
                    .text()
                    .collect::<String>()
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        results.push(SearchResult {
            title,
            url,
            snippet: snippet_text,
        });
        if results.len() >= RESULTS_WANTED {
            break;
        }
    }
    Ok(results)
}

// ---------------------------------------------------------------------------
// Brave Search
// ---------------------------------------------------------------------------

async fn search_brave(
    http: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    query: &str,
) -> Result<Vec<SearchResult>, SearchError> {
    if api_key.trim().is_empty() {
        return Err(SearchError::NotConfigured(
            "Add your Brave Search API key in Settings > Docs (or set BRAVE_API_KEY).".to_string(),
        ));
    }
    let url = Url::parse_with_params(
        base_url,
        [("q", query), ("count", "8"), ("safesearch", "moderate")],
    )
    .map_err(|error| SearchError::NotConfigured(format!("bad Brave address: {error}")))?;
    let response = http
        .get(url)
        .header("accept", "application/json")
        .header("x-subscription-token", api_key.trim())
        .send()
        .await
        .map_err(network_error)?;
    let status = response.status();
    let body = response.text().await.map_err(network_error)?;
    if !status.is_success() {
        return Err(brave_error(status.as_u16(), &body));
    }
    let value: Value =
        serde_json::from_str(&body).map_err(|error| SearchError::Unexpected(error.to_string()))?;
    Ok(parse_brave_json(&value))
}

fn brave_error(status: u16, body: &str) -> SearchError {
    let value: Option<Value> = serde_json::from_str(body).ok();
    let code = value
        .as_ref()
        .and_then(|value| value.pointer("/error/code"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let detail = value
        .as_ref()
        .and_then(|value| value.pointer("/error/detail"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| short(body));
    if code == "SUBSCRIPTION_TOKEN_INVALID" || status == 401 || status == 403 {
        SearchError::Refused(
            "Brave Search rejected the API key. Check it in Settings > Docs.".to_string(),
        )
    } else if status == 429 {
        SearchError::Refused(
            "Brave Search says you are over your rate or monthly limit.".to_string(),
        )
    } else {
        SearchError::Http {
            status,
            message: detail,
        }
    }
}

/// Brave highlights matches with `<strong>` tags; keep only the text.
fn strip_tags(text: &str) -> String {
    let fragment = Html::parse_fragment(text);
    fragment
        .root_element()
        .text()
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn parse_brave_json(value: &Value) -> Vec<SearchResult> {
    value
        .pointer("/web/results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let url = entry.get("url")?.as_str()?.to_string();
            let title = strip_tags(entry.get("title")?.as_str()?);
            let snippet = strip_tags(
                entry
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            );
            (!title.is_empty()).then_some(SearchResult {
                title,
                url,
                snippet,
            })
        })
        .take(RESULTS_WANTED)
        .collect()
}

// ---------------------------------------------------------------------------
// SearXNG
// ---------------------------------------------------------------------------

async fn search_searxng(
    http: &reqwest::Client,
    base_url: &str,
    query: &str,
) -> Result<Vec<SearchResult>, SearchError> {
    let base = base_url.trim().trim_end_matches('/');
    let parsed = Url::parse(base).map_err(|_| {
        SearchError::NotConfigured(
            "Set the address of your SearXNG server in Settings > Docs.".to_string(),
        )
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(SearchError::NotConfigured(
            "The SearXNG address must start with http:// or https://".to_string(),
        ));
    }
    let url = Url::parse_with_params(
        &format!("{base}/search"),
        [("q", query), ("format", "json"), ("language", "en")],
    )
    .map_err(|error| SearchError::NotConfigured(format!("bad SearXNG address: {error}")))?;
    let response = http
        .get(url)
        .header("accept", "application/json")
        .send()
        .await
        .map_err(network_error)?;
    let status = response.status();
    let body = response.text().await.map_err(network_error)?;
    if !status.is_success() {
        let hint = if status.as_u16() == 403 {
            " (enable the json format under search.formats in your SearXNG settings.yml)"
        } else {
            ""
        };
        return Err(SearchError::Http {
            status: status.as_u16(),
            message: format!("{}{hint}", short(&body)),
        });
    }
    let value: Value = serde_json::from_str(&body).map_err(|_| {
        SearchError::Unexpected(
            "the server did not return JSON. Enable the json format under search.formats in your SearXNG settings.yml."
                .to_string(),
        )
    })?;
    Ok(parse_searxng_json(&value))
}

pub fn parse_searxng_json(value: &Value) -> Vec<SearchResult> {
    value
        .get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let url = entry.get("url")?.as_str()?.to_string();
            let title = entry
                .get("title")?
                .as_str()?
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let snippet = entry
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            (!title.is_empty()).then_some(SearchResult {
                title,
                url,
                snippet,
            })
        })
        .take(RESULTS_WANTED)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Markup in the shape DuckDuckGo's HTML endpoint serves (confirmed against the
    /// selectors used by the `ddgs` library): result links are wrapped in a redirector.
    const DUCKDUCKGO_PAGE: &str = r#"<html><body><div id="links" class="results">
        <div class="result results_links results_links_deep web-result ">
          <div class="links_main links_deep result__body">
            <h2 class="result__title"><a rel="nofollow" class="result__a"
                href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fhelp.figma.com%2Fhc%2Fen-us%2Farticles%2F360040451373&amp;rut=abc">
                Explore <b>auto layout</b> properties – Figma Learn</a></h2>
            <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fhelp.figma.com%2F&amp;rut=abc">
                Auto layout lets you <b>create</b> responsive designs.</a>
          </div></div>
        <div class="result result--ad"><div class="links_main result__body">
            <h2 class="result__title"><a class="result__a" href="https://duckduckgo.com/y.js?ad_domain=shop.example&amp;x=1">Buy stuff</a></h2>
            <a class="result__snippet">An advertisement</a></div></div>
        <div class="result results_links web-result"><div class="links_main result__body">
            <h2 class="result__title"><a class="result__a" href="https://docs.example.com/guide?a=1&amp;b=2">Example guide</a></h2>
            <a class="result__snippet">Direct link, no redirector.</a></div></div>
      </div></body></html>"#;

    #[test]
    fn parses_duckduckgo_results_unwraps_redirects_and_drops_ads() {
        let results = parse_duckduckgo_html(DUCKDUCKGO_PAGE).unwrap();
        assert_eq!(results.len(), 2, "{results:#?}");
        assert_eq!(
            results[0].title,
            "Explore auto layout properties – Figma Learn"
        );
        assert_eq!(
            results[0].url,
            "https://help.figma.com/hc/en-us/articles/360040451373"
        );
        assert_eq!(
            results[0].snippet,
            "Auto layout lets you create responsive designs."
        );
        assert_eq!(results[1].url, "https://docs.example.com/guide?a=1&b=2");
    }

    #[test]
    fn a_duckduckgo_captcha_page_is_reported_clearly_not_as_no_results() {
        // The real challenge page served to this project's own test network.
        let challenge = r#"<html><body><div class="anomaly-modal__modal"><div class="anomaly-modal__title">
            Unfortunately, bots use DuckDuckGo too.</div><div>Select all squares containing a duck:</div>
            Select all squares</div></body></html>"#;
        let error = parse_duckduckgo_html(challenge).unwrap_err();
        assert!(matches!(error, SearchError::Refused(_)));
        assert!(error.to_string().contains("CAPTCHA"));
        assert!(error.to_string().contains("Brave"));
    }

    #[test]
    fn an_empty_results_page_is_an_empty_list() {
        assert!(
            parse_duckduckgo_html(
                "<html><body><div class=\"no-results\">No results.</div></body></html>"
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn parses_brave_results_and_strips_highlight_tags() {
        let body = json!({"web": {"results": [
            {"title": "Docs <strong>home</strong>", "url": "https://docs.example.com/", "description": "Learn <strong>fast</strong> &amp; easy"},
            {"url": "https://no-title.example.com/"},
            {"title": "Second", "url": "https://example.org/two"}
        ]}});
        let results = parse_brave_json(&body);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Docs home");
        assert_eq!(results[0].snippet, "Learn fast & easy");
        assert_eq!(results[1].snippet, "");
    }

    #[test]
    fn maps_brave_errors_to_actionable_messages() {
        // The real body Brave returns for a bad key.
        let invalid = r#"{"error":{"code":"SUBSCRIPTION_TOKEN_INVALID","detail":"The provided subscription token is invalid.","status":422},"type":"ErrorResponse"}"#;
        assert!(
            brave_error(422, invalid)
                .to_string()
                .contains("rejected the API key")
        );
        assert!(brave_error(429, "{}").to_string().contains("limit"));
        assert!(matches!(
            brave_error(500, "boom"),
            SearchError::Http { status: 500, .. }
        ));
    }

    #[test]
    fn parses_searxng_results() {
        let body = json!({"results": [
            {"title": "A  guide", "url": "https://a.example.com/", "content": "About   A"},
            {"title": "", "url": "https://skip.example.com/"},
            {"title": "B", "url": "https://b.example.com/"}
        ]});
        let results = parse_searxng_json(&body);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "A guide");
        assert_eq!(results[0].snippet, "About A");
    }

    #[test]
    fn redirect_unwrapping_handles_odd_inputs() {
        assert_eq!(
            unwrap_duckduckgo_redirect("https://example.com/x").as_deref(),
            Some("https://example.com/x")
        );
        assert_eq!(
            unwrap_duckduckgo_redirect("https://duckduckgo.com/y.js?ad=1"),
            None
        );
        assert_eq!(
            unwrap_duckduckgo_redirect("//duckduckgo.com/l/?rut=nothing"),
            None
        );
        assert_eq!(unwrap_duckduckgo_redirect("not a url"), None);
    }
}
