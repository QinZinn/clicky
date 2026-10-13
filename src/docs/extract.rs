//! Turning a web page into the plain text a model can use.
//!
//! Docs pages are mostly navigation, scripts and cookie banners around a small
//! article. The extractor prefers the page's main content area, drops the
//! furniture, keeps headings, lists, tables and code blocks readable, and cuts
//! the result to a size budget.

use scraper::{ElementRef, Html, Node, Selector};

pub struct ExtractedPage {
    pub title: Option<String>,
    pub text: String,
}

/// Elements whose contents are never part of the readable text.
const SKIPPED_TAGS: [&str; 17] = [
    "script", "style", "noscript", "template", "nav", "header", "footer", "aside", "form", "svg",
    "iframe", "button", "select", "dialog", "canvas", "object", "embed",
];

/// ARIA landmark roles that mark page furniture.
const SKIPPED_ROLES: [&str; 5] = [
    "navigation",
    "banner",
    "contentinfo",
    "complementary",
    "search",
];

/// Class/id tokens (matched whole, never as substrings: `has-sidebar` is often
/// the wrapper around the entire page) that mark page furniture.
const SKIPPED_NAMES: [&str; 11] = [
    "cookie-banner",
    "cookie-consent",
    "cookie-notice",
    "cookies",
    "sidebar",
    "breadcrumb",
    "breadcrumbs",
    "skip-link",
    "skip-to-content",
    "navbar",
    "site-footer",
];

/// Where documentation sites keep the actual article, most specific first.
const MAIN_CONTENT_SELECTORS: [&str; 10] = [
    "main",
    "article",
    "[role=\"main\"]",
    ".markdown-body",      // GitHub
    ".theme-doc-markdown", // Docusaurus
    ".md-content",         // MkDocs Material
    ".document",           // Sphinx
    "#main-content",
    "#content",
    "[itemprop=\"articleBody\"]",
];

/// A candidate content area must hold at least this much text to be trusted.
const MINIMUM_CONTENT_CHARACTERS: usize = 300;

#[derive(Default)]
struct TextCollector {
    output: String,
    /// Newlines owed before the next text (0, 1 or 2).
    pending_breaks: usize,
    /// A space is owed before the next inline text.
    pending_space: bool,
    /// Nothing to separate from yet on this line (start, or just after a prefix).
    at_line_start: bool,
}

impl TextCollector {
    fn new() -> Self {
        Self {
            at_line_start: true,
            ..Self::default()
        }
    }

    fn request_break(&mut self, breaks: usize) {
        self.pending_breaks = self.pending_breaks.max(breaks);
    }

    fn flush_breaks(&mut self) {
        if self.pending_breaks > 0 && !self.output.is_empty() {
            for _ in 0..self.pending_breaks {
                self.output.push('\n');
            }
            self.at_line_start = true;
            self.pending_space = false;
        }
        self.pending_breaks = 0;
    }

    fn push_inline_text(&mut self, text: &str) {
        let words: Vec<&str> = text.split_whitespace().collect();
        let starts_with_space = text.starts_with(char::is_whitespace);
        let ends_with_space = text.ends_with(char::is_whitespace);
        if words.is_empty() {
            if !text.is_empty() {
                self.pending_space = true;
            }
            return;
        }
        self.flush_breaks();
        if (starts_with_space || self.pending_space) && !self.at_line_start {
            self.output.push(' ');
        }
        self.output.push_str(&words.join(" "));
        self.at_line_start = false;
        self.pending_space = ends_with_space;
    }

    /// Writes a prefix such as `## ` or `- ` that the next text follows directly.
    fn push_prefix(&mut self, prefix: &str) {
        self.flush_breaks();
        self.output.push_str(prefix);
        self.at_line_start = true;
        self.pending_space = false;
    }

    fn push_preformatted(&mut self, text: &str) {
        self.request_break(2);
        self.flush_breaks();
        self.output.push_str("```\n");
        self.output.push_str(text.trim_matches('\n'));
        self.output.push_str("\n```");
        self.at_line_start = false;
        self.pending_space = false;
        self.request_break(2);
    }
}

fn has_token(attribute: Option<&str>, names: &[&str]) -> bool {
    attribute.is_some_and(|value| {
        value
            .split_whitespace()
            .any(|token| names.contains(&token.to_ascii_lowercase().as_str()))
    })
}

fn is_page_furniture(element: ElementRef<'_>) -> bool {
    let value = element.value();
    SKIPPED_TAGS.contains(&value.name())
        || value.attr("hidden").is_some()
        || value.attr("aria-hidden") == Some("true")
        || value
            .attr("role")
            .is_some_and(|role| SKIPPED_ROLES.contains(&role))
        || has_token(value.attr("class"), &SKIPPED_NAMES)
        || has_token(value.attr("id"), &SKIPPED_NAMES)
}

fn collect_element(element: ElementRef<'_>, collector: &mut TextCollector) {
    if is_page_furniture(element) {
        return;
    }
    let name = element.value().name();

    match name {
        "pre" => {
            let code: String = element.text().collect();
            collector.push_preformatted(&code);
            return;
        }
        "br" => {
            collector.request_break(1);
            return;
        }
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            let level = usize::from(name.as_bytes()[1] - b'0');
            collector.request_break(2);
            collector.push_prefix(&format!("{} ", "#".repeat(level)));
        }
        "li" => {
            collector.request_break(1);
            collector.push_prefix("- ");
        }
        "p" | "ul" | "ol" | "table" | "blockquote" | "section" | "article" | "figure" | "dl"
        | "details" | "summary" => collector.request_break(2),
        "div" | "tr" | "dt" | "dd" | "main" | "fieldset" | "caption" => collector.request_break(1),
        "td" | "th" if !collector.at_line_start => {
            collector.push_inline_text(" | ");
        }
        _ => {}
    }

    for child in element.children() {
        match child.value() {
            Node::Text(text) => collector.push_inline_text(text),
            Node::Element(_) => {
                if let Some(child_element) = ElementRef::wrap(child) {
                    collect_element(child_element, collector);
                }
            }
            _ => {}
        }
    }

    if matches!(
        name,
        "p" | "ul"
            | "ol"
            | "table"
            | "blockquote"
            | "section"
            | "article"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "figure"
            | "dl"
            | "details"
    ) {
        collector.request_break(2);
    } else if matches!(name, "div" | "li" | "tr" | "dt" | "dd") {
        collector.request_break(1);
    }
}

fn clean_up(text: &str) -> String {
    let mut lines: Vec<&str> = Vec::new();
    let mut previous_blank = true;
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            if !previous_blank {
                lines.push("");
            }
            previous_blank = true;
            continue;
        }
        // Menus and repeated widgets often print the same line many times in a row.
        if lines.last() == Some(&line) {
            continue;
        }
        lines.push(line);
        previous_blank = false;
    }
    lines.join("\n").trim().to_string()
}

/// Cuts `text` to at most `maximum_characters`, at a line boundary when one is
/// close, and says so.
pub fn truncate_to_budget(text: &str, maximum_characters: usize) -> String {
    if text.chars().count() <= maximum_characters {
        return text.to_string();
    }
    let cut_at = text
        .char_indices()
        .nth(maximum_characters)
        .map_or(text.len(), |(index, _)| index);
    let mut kept = &text[..cut_at];
    if let Some(last_newline) = kept.rfind('\n') {
        // Only snap back if it does not throw away much.
        if kept[last_newline..].chars().count() < 400 {
            kept = &kept[..last_newline];
        }
    }
    format!("{}\n[... cut to fit]", kept.trim_end())
}

fn text_of(root: ElementRef<'_>) -> String {
    let mut collector = TextCollector::new();
    collect_element(root, &mut collector);
    clean_up(&collector.output)
}

fn page_title(document: &Html) -> Option<String> {
    let selector = Selector::parse("title").ok()?;
    let title: String = document.select(&selector).next()?.text().collect();
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    (!title.is_empty()).then_some(title)
}

/// Extracts the readable text of an HTML page.
pub fn extract_readable_text(html: &str, maximum_characters: usize) -> ExtractedPage {
    let document = Html::parse_document(html);
    let title = page_title(&document);

    let mut text = String::new();
    for selector_text in MAIN_CONTENT_SELECTORS {
        let Ok(selector) = Selector::parse(selector_text) else {
            continue;
        };
        // The largest match wins: pages sometimes have a small <article> teaser
        // next to a big <main>, or several nested wrappers.
        let best = document
            .select(&selector)
            .map(text_of)
            .max_by_key(|candidate| candidate.chars().count());
        if let Some(candidate) = best
            && candidate.chars().count() >= MINIMUM_CONTENT_CHARACTERS
        {
            text = candidate;
            break;
        }
    }
    if text.is_empty()
        && let Ok(selector) = Selector::parse("body")
        && let Some(body) = document.select(&selector).next()
    {
        text = text_of(body);
    }
    ExtractedPage {
        title,
        text: truncate_to_budget(&text, maximum_characters),
    }
}

/// Plain text and Markdown pages need no extraction, only tidying.
pub fn extract_plain_text(body: &str, maximum_characters: usize) -> ExtractedPage {
    ExtractedPage {
        title: None,
        text: truncate_to_budget(body.trim(), maximum_characters),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<!doctype html><html><head><title>  Auto layout   properties – Example Help </title>
        <style>.x { color: red }</style><script>var tracking = "do not include me";</script></head>
        <body>
        <header><a href="/">Home</a><nav><ul><li>Pricing</li><li>Login</li></ul></nav></header>
        <div class="cookie-banner">We use cookies. Accept all.</div>
        <div class="layout has-sidebar">
          <aside><ul><li>Sidebar link one</li></ul></aside>
          <main>
            <h1>Explore auto layout properties</h1>
            <p>Auto layout lets frames <strong>resize</strong> as you add content.
               Press <kbd>Shift</kbd>+<kbd>A</kbd> to add it.</p>
            <h2>Wrapping</h2>
            <ul><li>Turn on <em>wrap</em> in the panel.</li><li>Set the gap.</li></ul>
            <pre><code>auto_layout(wrap=true,
    gap=8)</code></pre>
            <table><tr><th>Name</th><th>Meaning</th></tr><tr><td>Gap</td><td>Space between items</td></tr></table>
            <p>Repeat repeat repeat to pad this page out beyond the minimum, because the extractor only trusts a content area with a few hundred characters of text. Docs pages are long and this is a stand in for that.</p>
          </main>
        </div>
        <footer>Copyright Example Inc. Terms. Privacy.</footer>
        </body></html>"#;

    #[test]
    fn extracts_the_article_and_drops_the_furniture() {
        let page = extract_readable_text(SAMPLE, 10_000);
        assert_eq!(
            page.title.as_deref(),
            Some("Auto layout properties – Example Help")
        );
        let text = page.text;
        assert!(
            text.starts_with("# Explore auto layout properties"),
            "{text}"
        );
        assert!(
            text.contains("Auto layout lets frames resize as you add content."),
            "{text}"
        );
        assert!(text.contains("## Wrapping"));
        assert!(text.contains("- Turn on wrap in the panel."));
        assert!(text.contains("- Set the gap."));
        for noise in [
            "do not include me",
            "Pricing",
            "We use cookies",
            "Sidebar link",
            "Copyright",
            "color: red",
        ] {
            assert!(!text.contains(noise), "{noise:?} leaked into: {text}");
        }
    }

    #[test]
    fn keeps_code_blocks_and_tables_readable() {
        let text = extract_readable_text(SAMPLE, 10_000).text;
        assert!(
            text.contains("```\nauto_layout(wrap=true,\n    gap=8)\n```"),
            "{text}"
        );
        assert!(text.contains("Name | Meaning"), "{text}");
        assert!(text.contains("Gap | Space between items"), "{text}");
    }

    #[test]
    fn a_has_sidebar_wrapper_does_not_hide_the_page() {
        // `layout has-sidebar` contains everything; only a whole `sidebar` token is dropped.
        let text = extract_readable_text(SAMPLE, 10_000).text;
        assert!(text.contains("Explore auto layout"));
    }

    #[test]
    fn falls_back_to_the_body_when_there_is_no_main_area() {
        let html = format!(
            "<html><body><div><p>{}</p></div></body></html>",
            "Plain body text. ".repeat(5)
        );
        let text = extract_readable_text(&html, 10_000).text;
        assert!(
            text.starts_with("Plain body text. Plain body text."),
            "{text}"
        );
    }

    #[test]
    fn ignores_a_tiny_main_and_uses_the_body_instead() {
        let html = format!(
            "<html><body><main>Menu</main><div><p>{}</p></div></body></html>",
            "The real documentation is outside the tiny main element. ".repeat(10)
        );
        let text = extract_readable_text(&html, 10_000).text;
        assert!(text.contains("The real documentation"), "{text}");
    }

    #[test]
    fn hidden_and_landmark_elements_are_skipped() {
        let html = format!(
            r#"<html><body><main>
                <p hidden>hidden paragraph</p><p aria-hidden="true">aria hidden</p>
                <div role="navigation">site menu</div><div role="search">search box</div>
                <p>{}</p></main></body></html>"#,
            "Visible text that is long enough to be trusted as the main content. ".repeat(6)
        );
        let text = extract_readable_text(&html, 10_000).text;
        for hidden in ["hidden paragraph", "aria hidden", "site menu", "search box"] {
            assert!(!text.contains(hidden), "{hidden:?} in {text}");
        }
        assert!(text.contains("Visible text"));
    }

    #[test]
    fn repeated_lines_are_collapsed_and_blank_runs_squeezed() {
        assert_eq!(clean_up("a\na\na\n\n\n\nb\n  \nb"), "a\n\nb\n\nb");
    }

    #[test]
    fn truncation_respects_the_budget_and_says_so() {
        let text = (0..200)
            .map(|index| format!("Line number {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let cut = truncate_to_budget(&text, 500);
        assert!(cut.chars().count() <= 500 + "\n[... cut to fit]".len());
        assert!(cut.ends_with("[... cut to fit]"));
        assert!(cut.starts_with("Line number 0"));
        // Under budget: untouched.
        assert_eq!(truncate_to_budget("short", 500), "short");
    }

    #[test]
    fn truncation_never_splits_a_multibyte_character() {
        let text = "é".repeat(1000);
        let cut = truncate_to_budget(&text, 101);
        assert!(cut.starts_with(&"é".repeat(101)));
    }

    #[test]
    fn plain_text_is_only_tidied_and_capped() {
        let page = extract_plain_text("  # Title\n\nBody  \n", 1000);
        assert_eq!(page.text, "# Title\n\nBody");
        assert!(page.title.is_none());
    }

    #[test]
    fn malformed_html_does_not_panic() {
        for html in [
            "",
            "<",
            "<div><p>unclosed",
            "<<<>>>",
            "\u{0}\u{1}",
            "<script>",
        ] {
            let _ = extract_readable_text(html, 100);
        }
    }
}
