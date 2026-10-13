//! Minimal Server-Sent Events decoder.
//!
//! Network chunks can split an event (or even a UTF-8 character) anywhere, so
//! the parser buffers raw bytes and only decodes complete lines.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// The `event:` field, if the server sent one.
    pub event: Option<String>,
    pub data: String,
}

#[derive(Default)]
pub struct SseParser {
    pending_bytes: Vec<u8>,
    current_event_name: Option<String>,
    current_data_lines: Vec<String>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds a network chunk and returns every event it completed.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.pending_bytes.extend_from_slice(chunk);
        let mut completed_events = Vec::new();

        while let Some(newline_index) = self.pending_bytes.iter().position(|byte| *byte == b'\n') {
            let mut line_bytes: Vec<u8> = self.pending_bytes.drain(..=newline_index).collect();
            line_bytes.pop(); // the '\n'
            if line_bytes.last() == Some(&b'\r') {
                line_bytes.pop();
            }
            let line = String::from_utf8_lossy(&line_bytes);
            if let Some(event) = self.handle_line(&line) {
                completed_events.push(event);
            }
        }
        completed_events
    }

    /// Flushes an event the server never terminated with a blank line.
    pub fn finish(&mut self) -> Option<SseEvent> {
        if !self.pending_bytes.is_empty() {
            let leftover = String::from_utf8_lossy(&self.pending_bytes).into_owned();
            self.pending_bytes.clear();
            let _ = self.handle_line(leftover.trim_end_matches('\r'));
        }
        self.dispatch()
    }

    fn handle_line(&mut self, line: &str) -> Option<SseEvent> {
        if line.is_empty() {
            return self.dispatch();
        }
        if line.starts_with(':') {
            return None; // comment / keep-alive
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "event" => self.current_event_name = Some(value.to_string()),
            "data" => self.current_data_lines.push(value.to_string()),
            _ => {} // id, retry: not needed
        }
        None
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        if self.current_data_lines.is_empty() {
            self.current_event_name = None;
            return None;
        }
        let event = SseEvent {
            event: self.current_event_name.take(),
            data: self.current_data_lines.join("\n"),
        };
        self.current_data_lines.clear();
        Some(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_events() {
        let mut parser = SseParser::new();
        let events = parser.push(b"data: one\n\ndata: two\n\n");
        assert_eq!(
            events,
            vec![
                SseEvent {
                    event: None,
                    data: "one".into()
                },
                SseEvent {
                    event: None,
                    data: "two".into()
                },
            ]
        );
    }

    #[test]
    fn handles_events_split_across_chunks_and_crlf() {
        let mut parser = SseParser::new();
        assert!(parser.push(b"event: content_blo").is_empty());
        assert!(parser.push(b"ck_delta\r\ndata: {\"a\"").is_empty());
        let events = parser.push(b":1}\r\n\r\n");
        assert_eq!(
            events,
            vec![SseEvent {
                event: Some("content_block_delta".into()),
                data: "{\"a\":1}".into()
            }]
        );
    }

    #[test]
    fn does_not_corrupt_multibyte_characters_split_between_chunks() {
        let full = "data: héllo 🌍\n\n".as_bytes();
        // Split in the middle of the 4-byte emoji.
        let split_at = full.len() - 5;
        let mut parser = SseParser::new();
        let mut events = parser.push(&full[..split_at]);
        events.extend(parser.push(&full[split_at..]));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "héllo 🌍");
    }

    #[test]
    fn ignores_comments_and_joins_multiline_data() {
        let mut parser = SseParser::new();
        let events = parser.push(b": OPENROUTER PROCESSING\n\ndata: a\ndata: b\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "a\nb");
    }

    #[test]
    fn finish_flushes_an_unterminated_event() {
        let mut parser = SseParser::new();
        assert!(parser.push(b"data: tail").is_empty());
        assert_eq!(parser.finish().map(|event| event.data), Some("tail".into()));
    }
}
