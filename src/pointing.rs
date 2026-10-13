//! Parsing of the `[POINT:x,y:label:screenN]` tag the model appends to its
//! reply when it wants the blue cursor to fly to something on screen.

use std::sync::OnceLock;

use regex::Regex;

use crate::geometry::Point;

#[derive(Debug, Clone, PartialEq)]
pub struct PointingParseResult {
    /// The reply with the tag removed. This is what gets shown and spoken.
    pub spoken_text: String,
    /// Pixel coordinate in the screenshot, or `None` for `[POINT:none]` or no tag.
    pub coordinate: Option<Point>,
    /// Short description of the element (e.g. "save button"), or "none".
    pub element_label: Option<String>,
    /// 1-based screen number the coordinate refers to; `None` means the
    /// screen the cursor is on.
    pub screen_number: Option<usize>,
}

fn pointing_tag_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        // [POINT:none]  |  [POINT:123,456]  |  [POINT:123,456:label]  |
        // [POINT:123,456:label:screen2]  (label and screen are optional)
        Regex::new(
            r"(?i)\[POINT:\s*(?:none|(\d+)\s*,\s*(\d+)\s*(?::\s*([^\]:\s][^\]:]*?))?\s*(?::\s*screen(\d+))?)\s*\]",
        )
        .expect("pointing regex is valid")
    })
}

/// Extracts the pointing tag (the last one, if the model wrote several) and
/// returns the reply without it.
pub fn parse_pointing_coordinates(response_text: &str) -> PointingParseResult {
    let Some(captures) = pointing_tag_regex().captures_iter(response_text).last() else {
        return PointingParseResult {
            spoken_text: response_text.trim().to_string(),
            coordinate: None,
            element_label: None,
            screen_number: None,
        };
    };

    let tag_range = captures.get(0).expect("group 0 always exists").range();
    let text_before_tag = response_text[..tag_range.start].trim_end();
    let text_after_tag = response_text[tag_range.end..].trim_start();
    let spoken_text = if text_after_tag.is_empty() {
        text_before_tag.trim().to_string()
    } else {
        format!("{text_before_tag} {text_after_tag}")
            .trim()
            .to_string()
    };

    let coordinate_pair = captures
        .get(1)
        .zip(captures.get(2))
        .and_then(|(x_text, y_text)| {
            Some((
                x_text.as_str().parse::<f32>().ok()?,
                y_text.as_str().parse::<f32>().ok()?,
            ))
        });

    let Some((x_coordinate, y_coordinate)) = coordinate_pair else {
        // [POINT:none]
        return PointingParseResult {
            spoken_text,
            coordinate: None,
            element_label: Some("none".to_string()),
            screen_number: None,
        };
    };

    PointingParseResult {
        spoken_text,
        coordinate: Some(Point::new(x_coordinate, y_coordinate)),
        element_label: captures
            .get(3)
            .map(|label| label.as_str().trim().to_string()),
        screen_number: captures
            .get(4)
            .and_then(|screen| screen.as_str().parse::<usize>().ok()),
    }
}

/// The reply as it should be displayed while it is still streaming in:
/// complete pointing tags are removed, and so is a tag that has started but
/// not finished (`... over here [POIN`), so the user never sees raw markup.
pub fn text_for_display(streaming_text: &str) -> String {
    let without_complete_tags = pointing_tag_regex().replace_all(streaming_text, "");

    if let Some(last_bracket) = without_complete_tags.rfind('[') {
        let unfinished = &without_complete_tags[last_bracket..];
        let could_become_a_tag = !unfinished.contains(']')
            && (unfinished.len() <= "[POINT:".len()
                && "[POINT:"
                    .to_ascii_lowercase()
                    .starts_with(&unfinished.to_ascii_lowercase())
                || unfinished.to_ascii_lowercase().starts_with("[point:"));
        if could_become_a_tag {
            return without_complete_tags[..last_bracket].trim_end().to_string();
        }
    }
    without_complete_tags.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_display_hides_complete_and_unfinished_tags() {
        assert_eq!(
            text_for_display("click save [POINT:10,20:save button]"),
            "click save"
        );
        assert_eq!(text_for_display("click save [POINT:10,2"), "click save");
        assert_eq!(text_for_display("click save [POIN"), "click save");
        assert_eq!(text_for_display("click save ["), "click save");
        assert_eq!(text_for_display("click save [POINT:none]"), "click save");
        // Ordinary brackets are not hidden.
        assert_eq!(text_for_display("use array[0] here"), "use array[0] here");
        assert_eq!(text_for_display("see [1] and [2]"), "see [1] and [2]");
        assert_eq!(text_for_display("plain"), "plain");
    }

    #[test]
    fn parses_the_documented_examples() {
        let result = parse_pointing_coordinates(
            "you'll want to open the color inspector. it's up in the toolbar. [POINT:1100,42:color inspector]",
        );
        assert_eq!(
            result.spoken_text,
            "you'll want to open the color inspector. it's up in the toolbar."
        );
        assert_eq!(result.coordinate, Some(Point::new(1100.0, 42.0)));
        assert_eq!(result.element_label.as_deref(), Some("color inspector"));
        assert_eq!(result.screen_number, None);
    }

    #[test]
    fn parses_a_screen_number() {
        let result = parse_pointing_coordinates(
            "that's on your other monitor [POINT:400,300:terminal:screen2]",
        );
        assert_eq!(result.coordinate, Some(Point::new(400.0, 300.0)));
        assert_eq!(result.element_label.as_deref(), Some("terminal"));
        assert_eq!(result.screen_number, Some(2));
    }

    #[test]
    fn parses_coordinates_without_a_label() {
        let result = parse_pointing_coordinates("here [POINT:10,20]");
        assert_eq!(result.coordinate, Some(Point::new(10.0, 20.0)));
        assert_eq!(result.element_label, None);
    }

    #[test]
    fn point_none_means_no_pointing() {
        let result = parse_pointing_coordinates("html is the skeleton of a web page. [POINT:none]");
        assert_eq!(result.spoken_text, "html is the skeleton of a web page.");
        assert_eq!(result.coordinate, None);
        assert_eq!(result.element_label.as_deref(), Some("none"));
    }

    #[test]
    fn replies_without_a_tag_are_untouched() {
        let result = parse_pointing_coordinates("just an answer");
        assert_eq!(result.spoken_text, "just an answer");
        assert_eq!(result.coordinate, None);
        assert_eq!(result.element_label, None);
    }

    #[test]
    fn tolerates_case_whitespace_and_trailing_newlines() {
        let result = parse_pointing_coordinates("click it [point: 5 , 6 : ok button ]\n\n");
        assert_eq!(result.coordinate, Some(Point::new(5.0, 6.0)));
        assert_eq!(result.element_label.as_deref(), Some("ok button"));
        assert_eq!(result.spoken_text, "click it");
    }

    #[test]
    fn a_tag_in_the_middle_is_removed_from_the_spoken_text() {
        let result = parse_pointing_coordinates("see that button [POINT:10,20:save] over there");
        assert_eq!(result.spoken_text, "see that button over there");
        assert_eq!(result.coordinate, Some(Point::new(10.0, 20.0)));
    }

    #[test]
    fn the_last_tag_wins() {
        let result = parse_pointing_coordinates("a [POINT:1,1:first] b [POINT:2,2:second]");
        assert_eq!(result.coordinate, Some(Point::new(2.0, 2.0)));
        assert_eq!(result.element_label.as_deref(), Some("second"));
    }

    #[test]
    fn malformed_tags_are_left_alone() {
        let result = parse_pointing_coordinates("odd [POINT:abc,def] text");
        assert_eq!(result.coordinate, None);
        assert_eq!(result.spoken_text, "odd [POINT:abc,def] text");
    }
}
