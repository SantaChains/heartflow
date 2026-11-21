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

    /// Split one received chunk into complete frames, buffering partial input.
    ///
    /// # Errors
    /// Never fails today: every code path returns `Ok` — bytes are buffered or
    /// drained and non-UTF-8 fragments are lossily decoded. The `Result`
    /// reserves room for frame-level errors without changing call sites.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<SseFrame>, ApiError> {
        self.buffer.extend_from_slice(chunk);
        let mut frames = Vec::new();

        while let Some(frame) = self.next_frame() {
            frames.push(parse_frame(&frame));
        }

        Ok(frames)
    }

    /// Drain a trailing partial frame once the stream has ended.
    ///
    /// # Errors
    /// Never fails today: an empty buffer returns `Ok(vec![])` and a leftover
    /// partial frame is lossily decoded and parsed. The `Result` reserves room
    /// for frame-level errors without changing call sites.
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
///
/// # Errors
/// Fails when the frame's `data` payload is not a valid `StreamEvent` JSON
/// document (`Json`); the `None` cases above never error.
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

    #[test]
    fn finish_drains_a_trailing_partial_frame() {
        // No \n\n terminator: push must buffer, finish must hand the frame over.
        let mut parser = SseParser::new();
        let frames = parser
            .push(b"data: {\"type\":\"ping\"}")
            .expect("partial frame should buffer");
        assert_eq!(frames, []);
        let frames = parser.finish().expect("drain should succeed");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "{\"type\":\"ping\"}");
    }

    #[test]
    fn finish_on_an_empty_buffer_is_empty() {
        let mut parser = SseParser::new();
        assert_eq!(
            parser
                .finish()
                .expect("drain on empty buffer should succeed"),
            []
        );
    }

    #[test]
    fn invalid_json_payloads_error_without_panic() {
        // The transport layer parses any `data:` line into a frame; protocol
        // decoding is the one that reports garbage, and it must not panic.
        let mut parser = SseParser::new();
        let frames = parser
            .push(b"data: not-json\n\n")
            .expect("frame should parse");
        assert_eq!(frames.len(), 1);
        assert!(anthropic_event(&frames[0]).is_err());
    }

    #[test]
    fn multibyte_utf8_split_across_chunks_survives() {
        // A 3-byte UTF-8 char with the chunk boundary inside it: the parser
        // buffers raw bytes and only decodes once the frame is complete, so the
        // char is reassembled losslessly — no U+FFFD, no panic.
        let payload = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"段落\"}}\n\n";
        let bytes = payload.as_bytes();
        let mid = payload.find('段').expect("payload contains the CJK char") + 1; // one byte into the 3-byte sequence
        let mut parser = SseParser::new();
        assert_eq!(parser.push(&bytes[..mid]).expect("chunk 1 buffers"), []);
        let frames = parser.push(&bytes[mid..]).expect("chunk 2 parses");
        assert_eq!(frames.len(), 1);
        let event = anthropic_event(&frames[0]).expect("JSON decodes");
        let Some(StreamEvent::ContentBlockDelta(event)) = event else {
            panic!("expected a content-block delta, got {event:?}");
        };
        let ContentBlockDelta::TextDelta { text } = event.delta else {
            panic!("expected a text delta, got {:?}", event.delta);
        };
        assert_eq!(text, "段落");
    }

    #[test]
    fn truncated_trailing_multibyte_decodes_lossily_without_panicking() {
        // The genuinely lossy path: a stream that dies mid-char leaves a
        // partial sequence in the buffer; finish() must hand it over as a
        // replacement char instead of panicking.
        let mut parser = SseParser::new();
        let head =
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"段"
                .to_string();
        // Drop the last of 段's three UTF-8 bytes so the buffer ends mid-char.
        let mut bytes = head.into_bytes();
        let _ = bytes.pop();
        let _ = parser.push(&bytes).expect("chunk buffers");
        let frames = parser.finish().expect("drain should succeed");
        assert_eq!(frames.len(), 1);
        assert!(frames[0].data.contains('\u{FFFD}'));
    }

    #[test]
    fn data_field_without_space_still_parses() {
        let frame = parse_frame("data:{\"type\":\"ping\"}\n\n");
        assert_eq!(frame.data, "{\"type\":\"ping\"}");
    }
}
