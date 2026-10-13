//! The docs lookup against local mock servers: search backends, the page
//! fetcher's safety checks, and the whole search-fetch-extract pipeline.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use clicky::config::{DocsSearchBackend, Secret, Settings};
use clicky::docs::fetch::{
    FetchError, MAXIMUM_BODY_BYTES, UrlPolicy, build_fetch_client, fetch_page,
};
use clicky::docs::search::{SearchEndpoints, SearchError};
use clicky::docs::{DocsClients, DocsError, lookup_docs};
use serde_json::json;
use tokio::net::TcpListener;

#[derive(Clone, Default)]
struct Hits(Arc<Mutex<Vec<String>>>);

impl Hits {
    fn record(&self, what: impl Into<String>) {
        self.0.lock().unwrap().push(what.into());
    }
    fn all(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
    fn count(&self, what: &str) -> usize {
        self.all()
            .iter()
            .filter(|entry| entry.as_str() == what)
            .count()
    }
}

fn article(title: &str, topic: &str) -> String {
    format!(
        "<html><head><title>{title}</title></head><body><nav>Home | Pricing</nav><main><h1>{title}</h1>\
         <p>{}</p><ul><li>Open the {topic} panel.</li><li>Turn the option on.</li></ul></main>\
         <footer>Copyright</footer></body></html>",
        format!("This page explains how to use {topic} in detail, step by step. ").repeat(8)
    )
}

#[derive(Clone)]
struct ServerState {
    hits: Hits,
    /// Where `/redirect-to-secret` sends the client.
    secret_url: String,
    /// What the search endpoints should list.
    results: Arc<Mutex<Vec<serde_json::Value>>>,
}

async fn page(State(state): State<ServerState>, Path(name): Path<String>) -> Response {
    state.hits.record(format!("page:{name}"));
    match name.as_str() {
        "guide" => article("Official guide", "auto layout").into_response_html(),
        "blog" => article("A blog post", "auto layout").into_response_html(),
        "second" => article("Second guide", "wrapping").into_response_html(),
        "jsshell" => "<html><body><div id=app></div><script>render()</script></body></html>"
            .into_response_html(),
        "notes.md" => (
            [(header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
            format!(
                "# Notes\n\n{}",
                "Markdown notes about auto layout wrapping and gaps. ".repeat(10)
            ),
        )
            .into_response(),
        "report.pdf" => (
            [(header::CONTENT_TYPE, "application/pdf")],
            "%PDF-1.4 binary",
        )
            .into_response(),
        "missing" => StatusCode::NOT_FOUND.into_response(),
        "slow" => {
            // A site that accepts the connection and then never answers.
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            StatusCode::OK.into_response()
        }
        "redirect-to-guide" => Redirect::temporary("/page/guide").into_response(),
        "redirect-to-secret" => Redirect::temporary(&state.secret_url).into_response(),
        "huge" => {
            let filler =
                "<p>filler filler filler filler filler filler</p>".repeat(MAXIMUM_BODY_BYTES / 40);
            format!(
                "<html><head><title>Huge</title></head><body><main>{}{filler}</main></body></html>",
                "Start of the huge page. ".repeat(20)
            )
            .into_response_html()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

trait IntoHtml {
    fn into_response_html(self) -> Response;
}
impl IntoHtml for String {
    fn into_response_html(self) -> Response {
        ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], self).into_response()
    }
}
impl IntoHtml for &str {
    fn into_response_html(self) -> Response {
        self.to_string().into_response_html()
    }
}

async fn secret(State(state): State<ServerState>) -> Response {
    state.hits.record("SECRET");
    "internal admin page".into_response_html()
}

async fn searxng(State(state): State<ServerState>) -> Response {
    state.hits.record("search:searxng");
    let results = state.results.lock().unwrap().clone();
    axum::Json(json!({ "results": results })).into_response()
}

async fn brave(State(state): State<ServerState>, headers: HeaderMap) -> Response {
    state.hits.record(format!(
        "search:brave:{}",
        headers
            .get("x-subscription-token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    ));
    let results: Vec<_> = state
        .results
        .lock()
        .unwrap()
        .iter()
        .map(|entry| json!({"title": entry["title"], "url": entry["url"], "description": entry["content"]}))
        .collect();
    axum::Json(json!({"web": {"results": results}})).into_response()
}

async fn duckduckgo(State(state): State<ServerState>, body: String) -> Response {
    state.hits.record(format!("search:ddg:{body}"));
    let results = state.results.lock().unwrap().clone();
    let mut html = String::from("<html><body><div id=\"links\">");
    for entry in results {
        html.push_str(&format!(
            "<div class=\"result\"><div class=\"links_main result__body\"><h2 class=\"result__title\"><a class=\"result__a\" href=\"{}\">{}</a></h2><a class=\"result__snippet\">{}</a></div></div>",
            entry["url"].as_str().unwrap(),
            entry["title"].as_str().unwrap(),
            entry["content"].as_str().unwrap()
        ));
    }
    html.push_str("</div></body></html>");
    html.into_response_html()
}

struct Mock {
    base: String,
    secret_address: String,
    state: ServerState,
}

impl Mock {
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn set_results(&self, results: Vec<(&str, &str, &str)>) {
        *self.state.results.lock().unwrap() = results
            .into_iter()
            .map(|(title, url, content)| json!({"title": title, "url": url, "content": content}))
            .collect();
    }

    /// Allows the main mock (but never the "secret" internal server).
    fn policy(&self) -> UrlPolicy {
        UrlPolicy::strict().with_exception(self.base.trim_start_matches("http://"))
    }

    fn clients(&self) -> DocsClients {
        DocsClients::new(
            reqwest::Client::new(),
            SearchEndpoints {
                duckduckgo_html: self.url("/ddg/html/"),
                brave_web: self.url("/brave"),
            },
            self.policy(),
        )
    }

    fn settings(&self, backend: DocsSearchBackend) -> Settings {
        let mut settings = Settings::default();
        settings.docs.enabled = true;
        settings.docs.backend = backend;
        settings.docs.searxng_url = self.url("/searx");
        settings.docs.brave_api_key = Secret::new("brave-test-key");
        settings
    }
}

async fn start_mock() -> Mock {
    let hits = Hits::default();

    // A second server standing in for something on the user's network.
    let secret_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let secret_address = secret_listener.local_addr().unwrap();
    let secret_url = format!("http://{secret_address}/admin");

    let state = ServerState {
        hits: hits.clone(),
        secret_url,
        results: Arc::default(),
    };
    let secret_app = Router::new()
        .route("/admin", get(secret))
        .with_state(state.clone());
    tokio::spawn(async move { axum::serve(secret_listener, secret_app).await.unwrap() });

    let app = Router::new()
        .route("/page/{name}", get(page))
        .route("/searx/search", get(searxng))
        .route("/brave", get(brave))
        .route("/ddg/html/", post(duckduckgo))
        .with_state(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    Mock {
        base: format!("http://{address}"),
        secret_address: secret_address.to_string(),
        state,
    }
}

// ---------------------------------------------------------------------------
// The fetcher
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fetches_an_allowed_page() {
    let mock = start_mock().await;
    let policy = mock.policy();
    let client = build_fetch_client(&policy);
    let page = fetch_page(&client, &policy, &mock.url("/page/guide"))
        .await
        .unwrap();
    assert!(page.content_type.starts_with("text/html"));
    assert!(page.body.contains("Official guide"));
}

#[tokio::test]
async fn the_strict_policy_refuses_loopback_and_never_sends_a_request() {
    let mock = start_mock().await;
    let strict = UrlPolicy::strict();
    let client = build_fetch_client(&strict);
    let error = fetch_page(&client, &strict, &mock.url("/page/guide"))
        .await
        .unwrap_err();
    assert!(matches!(error, FetchError::Blocked(_)), "{error:?}");
    assert!(
        mock.state.hits.all().is_empty(),
        "the server must not have been contacted"
    );
}

#[tokio::test]
async fn follows_a_redirect_to_an_allowed_page() {
    let mock = start_mock().await;
    let policy = mock.policy();
    let client = build_fetch_client(&policy);
    let page = fetch_page(&client, &policy, &mock.url("/page/redirect-to-guide"))
        .await
        .unwrap();
    assert!(
        page.final_url.ends_with("/page/guide"),
        "{}",
        page.final_url
    );
    assert!(page.body.contains("Official guide"));
}

#[tokio::test]
async fn a_redirect_into_an_internal_service_is_blocked_and_never_requested() {
    let mock = start_mock().await;
    let policy = mock.policy(); // allows the mock, but not the "secret" server on another port
    let client = build_fetch_client(&policy);
    let error = fetch_page(&client, &policy, &mock.url("/page/redirect-to-secret"))
        .await
        .unwrap_err();
    assert!(matches!(error, FetchError::Blocked(_)), "{error:?}");
    assert_eq!(
        mock.state.hits.count("SECRET"),
        0,
        "the internal server must never be contacted ({})",
        mock.secret_address
    );
}

#[tokio::test]
async fn unreadable_content_error_pages_and_oversized_bodies_are_handled() {
    let mock = start_mock().await;
    let policy = mock.policy();
    let client = build_fetch_client(&policy);

    let pdf = fetch_page(&client, &policy, &mock.url("/page/report.pdf"))
        .await
        .unwrap_err();
    assert!(matches!(pdf, FetchError::UnsupportedContent(_)), "{pdf:?}");

    let missing = fetch_page(&client, &policy, &mock.url("/page/missing"))
        .await
        .unwrap_err();
    assert!(matches!(missing, FetchError::Status(404)), "{missing:?}");

    let huge = fetch_page(&client, &policy, &mock.url("/page/huge"))
        .await
        .unwrap();
    assert!(
        huge.body.len() <= MAXIMUM_BODY_BYTES,
        "body was {} bytes",
        huge.body.len()
    );
    assert!(
        huge.body.starts_with("<html>"),
        "the start of the page is what we keep"
    );
}

// ---------------------------------------------------------------------------
// Search backends
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_backend_returns_results_and_sends_what_it_should() {
    let mock = start_mock().await;
    mock.set_results(vec![(
        "Official guide",
        &mock.url("/page/guide"),
        "How to use auto layout",
    )]);
    let clients = mock.clients();

    for backend in [
        DocsSearchBackend::SearxNg,
        DocsSearchBackend::Brave,
        DocsSearchBackend::DuckDuckGo,
    ] {
        let settings = mock.settings(backend);
        let results = clicky::docs::search::search(
            &clients.search_http,
            &clients.endpoints,
            &settings.docs,
            &settings.brave_api_key(),
            "figma auto layout",
        )
        .await
        .unwrap_or_else(|error| panic!("{backend:?}: {error}"));
        assert_eq!(results.len(), 1, "{backend:?}");
        assert_eq!(results[0].title, "Official guide", "{backend:?}");
        assert_eq!(results[0].url, mock.url("/page/guide"), "{backend:?}");
    }

    let hits = mock.state.hits.all();
    assert!(
        hits.contains(&"search:brave:brave-test-key".to_string()),
        "{hits:?}"
    );
    assert!(
        hits.iter()
            .any(|hit| hit.starts_with("search:ddg:") && hit.contains("q=figma+auto+layout")),
        "{hits:?}"
    );
}

#[tokio::test]
async fn brave_without_a_key_explains_what_to_do_without_a_request() {
    let mock = start_mock().await;
    let clients = mock.clients();
    let mut settings = mock.settings(DocsSearchBackend::Brave);
    settings.docs.brave_api_key = Secret::default();
    // (BRAVE_API_KEY could be set in the environment of whoever runs the tests.)
    if settings.brave_api_key().is_empty() {
        let error = clicky::docs::search::search(
            &clients.search_http,
            &clients.endpoints,
            &settings.docs,
            "",
            "q",
        )
        .await
        .unwrap_err();
        assert!(matches!(error, SearchError::NotConfigured(_)), "{error:?}");
        assert!(mock.state.hits.all().is_empty());
    }
}

// ---------------------------------------------------------------------------
// The whole lookup
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lookup_reads_the_docs_first_skips_unreadable_pages_and_respects_the_limit() {
    let mock = start_mock().await;
    mock.set_results(vec![
        ("A blog post", &mock.url("/page/blog"), "blog"),
        ("Broken", &mock.url("/page/missing"), "missing page"),
        ("A PDF", &mock.url("/page/report.pdf"), "pdf"),
        (
            "Official guide",
            &mock.url("/page/guide"),
            "How to use auto layout",
        ),
        ("Second guide", &mock.url("/page/second"), "Wrapping"),
        ("Notes", &mock.url("/page/notes.md"), "markdown"),
        (
            "Cloud metadata",
            "https://169.254.169.254/latest/meta-data/",
            "should never be fetched",
        ),
        (
            "Localhost",
            "https://localhost/admin",
            "should never be fetched",
        ),
    ]);
    let mut settings = mock.settings(DocsSearchBackend::SearxNg);
    settings.docs.max_pages = 2;

    let bundle = lookup_docs(&mock.clients(), &settings, "figma auto layout")
        .await
        .unwrap();

    assert_eq!(bundle.query, "figma auto layout");
    assert_eq!(bundle.pages.len(), 2, "max_pages is honoured");
    assert!(
        bundle
            .pages
            .iter()
            .all(|page| page.text.chars().count() >= 200),
        "only substantial pages are kept"
    );
    let titles: Vec<&str> = bundle
        .pages
        .iter()
        .map(|page| page.title.as_str())
        .collect();
    assert!(titles.contains(&"Official guide"), "{titles:?}");
    assert!(
        bundle.pages[0]
            .text
            .contains("- Open the auto layout panel.")
    );
    assert!(
        !bundle.pages[0].text.contains("Pricing"),
        "navigation must not leak in"
    );

    // The search saw a documentation hint.
    assert_eq!(mock.state.hits.count("search:searxng"), 1);
    let sources = bundle.sources();
    assert_eq!(sources.len(), 2);
    assert!(
        sources
            .iter()
            .all(|source| source.url.starts_with(&mock.base))
    );
}

#[tokio::test]
async fn markdown_pages_are_used_as_they_are() {
    let mock = start_mock().await;
    mock.set_results(vec![("Notes", &mock.url("/page/notes.md"), "markdown")]);
    let bundle = lookup_docs(
        &mock.clients(),
        &mock.settings(DocsSearchBackend::SearxNg),
        "q",
    )
    .await
    .unwrap();
    assert!(
        bundle.pages[0].text.starts_with("# Notes"),
        "{}",
        bundle.pages[0].text
    );
}

#[tokio::test]
async fn when_pages_cannot_be_read_the_search_snippets_are_used() {
    let mock = start_mock().await;
    mock.set_results(vec![
        (
            "JS only",
            &mock.url("/page/jsshell"),
            "This documentation page explains exactly how to enable wrapping in auto layout.",
        ),
        ("Tiny snippet", &mock.url("/page/missing"), "short"),
    ]);
    let bundle = lookup_docs(
        &mock.clients(),
        &mock.settings(DocsSearchBackend::SearxNg),
        "q",
    )
    .await
    .unwrap();
    assert_eq!(
        bundle.pages.len(),
        1,
        "only the snippet long enough to say something"
    );
    assert_eq!(bundle.pages[0].title, "JS only");
    assert!(bundle.pages[0].text.contains("enable wrapping"));
}

#[tokio::test]
async fn nothing_usable_is_reported_as_such() {
    let mock = start_mock().await;
    let clients = mock.clients();
    let settings = mock.settings(DocsSearchBackend::SearxNg);

    mock.set_results(vec![]);
    assert!(matches!(
        lookup_docs(&clients, &settings, "q").await,
        Err(DocsError::NoResults)
    ));

    mock.set_results(vec![
        ("Broken", &mock.url("/page/missing"), "x"),
        ("PDF", &mock.url("/page/report.pdf"), ""),
    ]);
    assert!(matches!(
        lookup_docs(&clients, &settings, "q").await,
        Err(DocsError::NothingReadable)
    ));
}

#[tokio::test]
async fn a_hostile_redirect_found_by_search_never_reaches_the_internal_server() {
    let mock = start_mock().await;
    mock.set_results(vec![(
        "Looks legitimate",
        &mock.url("/page/redirect-to-secret"),
        "A normal-looking documentation result about auto layout wrapping.",
    )]);
    let bundle = lookup_docs(
        &mock.clients(),
        &mock.settings(DocsSearchBackend::SearxNg),
        "q",
    )
    .await;
    assert_eq!(
        mock.state.hits.count("SECRET"),
        0,
        "the internal page must never be contacted"
    );
    // The page itself was unreadable, so only the snippet survives.
    let bundle = bundle.unwrap();
    assert!(
        bundle.pages[0]
            .text
            .contains("normal-looking documentation")
    );
    assert!(
        !bundle
            .pages
            .iter()
            .any(|page| page.text.contains("internal admin"))
    );
}

#[tokio::test]
async fn one_slow_site_does_not_hold_up_the_answer_once_enough_pages_are_read() {
    let mock = start_mock().await;
    // The best-ranked result hangs; two others are fine and fast.
    mock.set_results(vec![
        ("Hangs", &mock.url("/page/slow"), "never arrives"),
        (
            "Official guide",
            &mock.url("/page/guide"),
            "How to use auto layout",
        ),
        ("Second guide", &mock.url("/page/second"), "Wrapping"),
    ]);
    let mut settings = mock.settings(DocsSearchBackend::SearxNg);
    settings.docs.max_pages = 2;

    let started = std::time::Instant::now();
    let bundle = lookup_docs(&mock.clients(), &settings, "q").await.unwrap();

    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "took {:?}: waited for the hanging site",
        started.elapsed()
    );
    let titles: Vec<&str> = bundle
        .pages
        .iter()
        .map(|page| page.title.as_str())
        .collect();
    assert_eq!(
        titles,
        ["Official guide", "Second guide"],
        "best-ranked first, hanging site left out"
    );
}
