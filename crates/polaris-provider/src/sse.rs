//! Incrementally decodes SSE (text/event-stream). Pushing a byte chunk
//! returns only the events that are complete so far, delimited by a blank
//! line. Tolerant of splits across lines or blocks.

pub struct SseDecoder {
    buf: Vec<u8>,
}

pub struct SseEvent {
    /// The SSE `event:` field. The current providers branch on the `data`
    /// side's JSON `type`, so this is never read from production code, but
    /// it's kept as part of the SSE model (tests and debugging do read
    /// it).
    #[allow(dead_code)]
    pub event: Option<String>,
    pub data: String,
}

/// Raw frame bytes, including the terminating blank line.
pub const MAX_SSE_EVENT_BYTES: usize = 8 * 1024 * 1024;

impl SseDecoder {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Compatibility collector for callers needing a batch. Production uses
    /// push_each so a network chunk never becomes an unbounded event vector.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>, crate::ProviderError> {
        crate::check_limit(bytes.len(), crate::MAX_RESPONSE_BYTES, "SSE chunk")?;
        let mut out = Vec::new();
        self.push_each(bytes, |event| {
            out.push(event);
            Ok(())
        })?;
        Ok(out)
    }

    /// Consume exactly one frame at a time. Scan only the new suffix, so a
    /// delimiter-free slow stream does not repeatedly rescan its entire buffer.
    pub fn push_each(
        &mut self,
        bytes: &[u8],
        mut consume: impl FnMut(SseEvent) -> Result<(), crate::ProviderError>,
    ) -> Result<(), crate::ProviderError> {
        for byte in bytes {
            crate::checked_size(self.buf.len(), 1, MAX_SSE_EVENT_BYTES, "SSE frame")?;
            self.buf.push(*byte);
            let separator = if self.buf.ends_with(b"\r\n\r\n") {
                4
            } else if self.buf.ends_with(b"\n\n") {
                2
            } else {
                continue;
            };
            let block = std::str::from_utf8(&self.buf[..self.buf.len() - separator])
                .map_err(|e| crate::ProviderError::Decode(format!("SSE is not UTF-8: {e}")))?;
            let mut event = None;
            let mut data = String::new();
            let mut has_data = false;
            for line in block.split('\n') {
                let line = line.strip_suffix('\r').unwrap_or(line);
                if let Some(value) = line.strip_prefix("event:") {
                    event = Some(value.trim().to_string());
                } else if let Some(value) = line.strip_prefix("data:") {
                    if has_data {
                        data.push('\n');
                    }
                    data.push_str(value.strip_prefix(' ').unwrap_or(value));
                    has_data = true;
                }
            }
            self.buf.clear();
            if has_data || event.is_some() {
                consume(SseEvent { event, data })?;
            }
        }
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

impl Default for SseDecoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p43_frame_boundary_and_unterminated_buffer_are_bounded() {
        for size in [
            MAX_SSE_EVENT_BYTES - 1,
            MAX_SSE_EVENT_BYTES,
            MAX_SSE_EVENT_BYTES + 1,
        ] {
            let mut decoder = SseDecoder::new();
            let mut frame = b"data: ".to_vec();
            frame.resize(size - 2, b'x');
            frame.extend_from_slice(b"\n\n");
            let mut consumed = 0;
            let result = decoder.push_each(&frame, |_| {
                consumed += 1;
                Ok(())
            });
            assert_eq!(result.is_ok(), size <= MAX_SSE_EVENT_BYTES);
            assert_eq!(consumed, usize::from(size <= MAX_SSE_EVENT_BYTES));
            assert!(decoder.buf.len() <= MAX_SSE_EVENT_BYTES);
        }
        let mut decoder = SseDecoder::new();
        decoder
            .push_each(&vec![b'x'; MAX_SSE_EVENT_BYTES], |_| panic!("unterminated"))
            .unwrap();
        assert!(decoder.push_each(b"x", |_| panic!("unterminated")).is_err());
        assert_eq!(decoder.buf.len(), MAX_SSE_EVENT_BYTES);
    }

    #[test]
    fn p43_consumer_failure_stops_before_later_frames() {
        let mut decoder = SseDecoder::new();
        let mut seen = 0;
        let result = decoder.push_each(b"data: first\n\ndata: second\n\n", |_| {
            seen += 1;
            Err(crate::ProviderError::Http("stop".into()))
        });
        assert!(result.is_err());
        assert_eq!(seen, 1);
    }

    #[test]
    fn parses_single_event_block() {
        let mut d = SseDecoder::new();
        let evs = d.push(b"event: message\ndata: {\"x\":1}\n\n").unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].event.as_deref(), Some("message"));
        assert_eq!(evs[0].data, "{\"x\":1}");
    }

    #[test]
    fn reassembles_across_chunk_boundaries() {
        let mut d = SseDecoder::new();
        assert!(d.push(b"data: hel").unwrap().is_empty()); // partway through
        assert!(d.push(b"lo\n").unwrap().is_empty()); // line complete but block not yet terminated
        let evs = d.push(b"\n").unwrap(); // blank line finalizes the block
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].data, "hello");
    }

    #[test]
    fn concatenates_multiple_data_lines() {
        let mut d = SseDecoder::new();
        let evs = d.push(b"data: a\ndata: b\n\n").unwrap();
        assert_eq!(evs[0].data, "a\nb");
    }

    #[test]
    fn handles_two_events_in_one_push() {
        let mut d = SseDecoder::new();
        let evs = d.push(b"data: 1\n\ndata: 2\n\n").unwrap();
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].data, "1");
        assert_eq!(evs[1].data, "2");
    }

    #[test]
    fn reassembles_multibyte_char_split_across_pushes() {
        // "★" = E2 98 85. Push the first two bytes first, then deliver the
        // remaining byte plus the block terminator in the following push.
        let mut d = SseDecoder::new();
        assert!(d.push(b"data: \xe2\x98").unwrap().is_empty());
        let evs = d.push(b"\x85\n\n").unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].data, "★");

        // Also confirm that reassembly recovers the same way for a split
        // spanning a 3-character string.
        let mut d2 = SseDecoder::new();
        let s = "★☆♪";
        let mut line = b"data: ".to_vec();
        line.extend_from_slice(s.as_bytes());
        let mid = line.len() - 1; // cut through the 1st and 2nd bytes of the trailing "♪" (3 bytes)
        assert!(d2.push(&line[..mid]).unwrap().is_empty());
        let evs2 = d2.push(&line[mid..]).unwrap();
        // There's no block terminator (blank line) yet, so nothing is finalized here.
        assert!(evs2.is_empty());
        let evs3 = d2.push(b"\n\n").unwrap();
        assert_eq!(evs3.len(), 1);
        assert_eq!(evs3[0].data, s);
    }

    #[test]
    fn crlf_framed_event_terminates() {
        let mut d = SseDecoder::new();
        let evs = d.push(b"data: hello\r\n\r\n").unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].data, "hello");
    }

    #[test]
    fn crlf_reassembles_across_boundary() {
        let mut d = SseDecoder::new();
        assert!(d.push(b"data: hel").unwrap().is_empty());
        assert!(d.push(b"lo\r\n").unwrap().is_empty());
        let evs = d.push(b"\r\n").unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].data, "hello");
    }
}
