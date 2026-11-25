//! Server-sent events, the framing every model provider streams in: `event:`
//! and `data:` lines, a blank line ending each event, `:` lines comments.

/// One event: its name (Anthropic names them; OpenAI-style streams don't)
/// and its data lines joined by `\n`.
#[derive(Debug, PartialEq, Default, Clone)]
pub struct Event {
    pub event: Option<String>,
    pub data: String,
}

/// Turns lines into events, one at each blank line.
#[derive(Default)]
pub struct Parser {
    event: Option<String>,
    data: Vec<String>,
}

impl Parser {
    /// A line without its line break; the event it completes, if any.
    pub fn line(&mut self, line: &str) -> Option<Event> {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            return self.take();
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => self.data.push(value.to_string()),
            _ => {}
        }
        None
    }

    /// The event left when the stream ended without a blank line.
    pub fn finish(&mut self) -> Option<Event> {
        self.take()
    }

    fn take(&mut self) -> Option<Event> {
        if self.data.is_empty() && self.event.is_none() {
            return None;
        }
        Some(Event { event: self.event.take(), data: std::mem::take(&mut self.data).join("\n") })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Vec<Event> {
        let mut parser = Parser::default();
        let mut events: Vec<Event> = text.split('\n').filter_map(|line| parser.line(line)).collect();
        events.extend(parser.finish());
        events
    }

    #[test]
    fn named_and_unnamed_events() {
        let events = parse("event: message_start\ndata: {\"a\":1}\n\n: keep-alive\ndata: {\"b\":2}\r\n\r\ndata: [DONE]\n\n");
        assert_eq!(
            events,
            vec![
                Event { event: Some("message_start".into()), data: "{\"a\":1}".into() },
                Event { event: None, data: "{\"b\":2}".into() },
                Event { event: None, data: "[DONE]".into() },
            ]
        );
    }

    #[test]
    fn data_lines_join_and_a_last_event_without_a_blank_line_counts() {
        assert_eq!(parse("data: one\ndata:two\n\ndata: three"), vec![
            Event { event: None, data: "one\ntwo".into() },
            Event { event: None, data: "three".into() },
        ]);
        assert!(parse("\n\n: only a comment\n\n").is_empty());
    }
}
