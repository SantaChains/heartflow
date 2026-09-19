use crate::error::ApiError;
use crate::types::StreamEvent;

/// One parsed SSE frame: optional `event:` name plus joined `data:` payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame {
    pub event: Option<String>,
    pub data: String,
}

/// Transport-level SSE splitter. Frame boundaries and field extraction only;
/// protocol decoding happens in the provider clients.
#[derive(Debug, Default)]
pub struct SseParser {
    buffer: Vec<u8>,
}

impl SseParser {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<SseFrame>, ApiError> {
        self.buffer.extend_from_slice(chunk);
        let mut frames = Vec::new();

        while let Some(frame) = self.next_frame() {
            frames.push(parse_frame(&frame));
        }

        Ok(frames)
    }

    pub fn finish(&mut self) -> Result<Vec<SseFrame>, ApiError> {
        if self.buffer.is_empty() {
            return Ok(Vec::new());
        }

        let trailing = std::mem::take(&mut self.buffer);
        Ok(vec![parse_frame(&String::from_utf8_lossy(&trailing))])
    }

    fn next_frame(&mut self) -> Option<String> {
        let separator = self
            .buffer
            .windows(2)
            .position(|window| window == b"\n\n")
            .map(|position| (position, 2))
            .or_else(|| {
                self.buffer
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .map(|position| (position, 4))
            })?;

        let (position, separator_len) = separator;
        let frame = self
            .buffer
            .drain(..position + separator_len)
            .collect::<Vec<_>>();
        let frame_len = frame.len().saturating_sub(separator_len);
        Some(String::from_utf8_lossy(&frame[..frame_len]).into_owned())
    }
}

/// Extract `event:` and joined `data:` fields from one raw SSE frame.
#[must_use]
pub fn parse_frame(frame: &str) -> SseFrame {
    let trimmed = frame.trim();
    if trimmed.is_empty() {
        return SseFrame {
            event: None,
            data: String::new(),
        };
    }

    let mut data_lines = Vec::new();
    let mut event_name: Option<&str> = None;

    for line in trimmed.lines() {
        if line.starts_with(':') {
            continue;
        }
        if let Some(name) = line.strip_prefix("event:") {
            event_name = Some(name.trim());
            continue;
        }
        if let Some(data) = line.strip_prefix("data:") {
            data_lines.push(data.trim_start());
        }
    }

    SseFrame {
        event: event_name.map(str::to_string),
        data: data_lines.join("\n"),
    }
}

/// Decode an Anthropic protocol frame into a typed stream event.
///
/// Comment frames, `ping` events, and the `[DONE]` sentinel yield `None`.
pub fn anthropic_event(frame: &SseFrame) -> Result<Option<StreamEvent>, ApiError> {
    if frame.data.is_empty() || matches!(frame.event.as_deref(), Some("ping")) {
        return Ok(None);
    }
    if frame.data == "[DONE]" {
        return Ok(None);
    }

    serde_json::from_str::<StreamEvent>(&frame.data)
        .map(Some)
        .map_err(ApiError::from)
}

#[cfg(test)]
mod tests {
    use super::{anthropic_event, parse_frame, SseFrame, SseParser};
    use crate::types::{
        ContentBlockDelta, ContentBlockDeltaEvent, MessageDelta, MessageDeltaEvent,
        MessageStopEvent, StreamEvent, Usage,
    };

    #[test]
    fn parses_single_frame_fields() {
        let frame = parse_frame(concat!(
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\"}\n\n"
        ));
        assert_eq!(frame.event.as_deref(), Some("content_block_start"));
        assert_eq!(frame.data, "{\"type\":\"content_block_start\"}");
    }

    #[test]
    fn parses_chunked_stream() {
        let mut parser = SseParser::new();
        let first = b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",";
        let second = b"\"delta\":{\"type\":\"text_delta\"}}\n\n";

        let frames = parser.push(first).expect("first chunk should buffer");
        assert_eq!(frames, []);
        let frames = parser.push(second).expect("second chunk should parse");

        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event.as_deref(), Some("content_block_delta"));
    }

    #[test]
    fn decodes_anthropic_events_and_skips_noise() {
        let mut parser = SseParser::new();
        let payload = concat!(
            ": keepalive\n",
            "event: ping\n",
            "data: {\"type\":\"ping\"}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
            "data: [DONE]\n\n"
        );

        let frames = parser
            .push(payload.as_bytes())
            .expect("parser should succeed");
        let events: Vec<StreamEvent> = frames
            .iter()
            .filter_map(|frame| anthropic_event(frame).expect("frame should decode"))
            .collect();

        assert_eq!(
            events,
            vec![
                StreamEvent::MessageDelta(MessageDeltaEvent {
                    delta: MessageDelta {
                        stop_reason: Some("tool_use".to_string()),
                        stop_sequence: None,
                    },
                    usage: Usage {
                        input_tokens: 1,
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 0,
                        output_tokens: 2,
                    },
                }),
                StreamEvent::MessageStop(MessageStopEvent {}),
            ]
        );
    }

    #[test]
    fn decodes_text_delta_event() {
        let frame = parse_frame(
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n",
        );
        let event = anthropic_event(&frame).expect("frame should decode");
        assert_eq!(
            event,
            Some(StreamEvent::ContentBlockDelta(ContentBlockDeltaEvent {
                index: 0,
                delta: ContentBlockDelta::TextDelta {
                    text: "Hello".to_string(),
                },
            }))
        );
    }

    #[test]
    fn handles_crlf_frame_boundaries() {
        let mut parser = SseParser::new();
        let frames = parser
            .push(b"event: message_stop\r\ndata: {\"type\":\"message_stop\"}\r\n\r\n")
            .expect("parser should succeed");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event.as_deref(), Some("message_stop"));
    }

    #[test]
    fn ignores_empty_and_data_less_frames() {
        assert_eq!(
            parse_frame(""),
            SseFrame {
                event: None,
                data: String::new()
            }
        );
        let frame = parse_frame("event: ping\n\n");
        assert_eq!(frame.data, "");
    }

    #[test]
    fn joins_split_json_across_data_lines() {
        let frame = parse_frame(concat!(
            "data: {\"type\":\"content_block_delta\",\"index\":0,\n",
            "data: \"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n"
        ));
        assert_eq!(
            frame.data,
            "{\"type\":\"content_block_delta\",\"index\":0,\n\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}"
        );
        assert!(anthropic_event(&frame).is_ok());
    }
}
