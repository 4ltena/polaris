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

/// Finds the first blank-line separator (`"\n\n"` or `"\r\n\r\n"`) that
/// appears in the buffer. Returns `(separator start position, separator
/// byte length)`. CRLF and LF forms can be mixed, so both are scanned and
/// whichever comes first is used.
fn find_block_separator(buf: &[u8]) -> Option<(usize, usize)> {
    let lf = buf.windows(2).position(|w| w == b"\n\n");
    let crlf = buf.windows(4).position(|w| w == b"\r\n\r\n");
    match (lf, crlf) {
        (Some(l), Some(c)) if c < l => Some((c, 4)),
        (Some(l), _) => Some((l, 2)),
        (None, Some(c)) => Some((c, 4)),
        (None, None) => None,
    }
}

impl SseDecoder {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        // A multibyte character can be split across a push boundary, so we
        // accumulate raw bytes without decoding. Decoding only after
        // slicing out a complete block (terminated by a blank line) means
        // a split character never gets mangled into U+FFFD.
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        // Slice out complete blocks (terminated by a blank line, "\n\n" or
        // "\r\n\r\n") one at a time.
        while let Some((idx, sep_len)) = find_block_separator(&self.buf) {
            let block_bytes: Vec<u8> = self.buf[..idx].to_vec();
            self.buf.drain(..idx + sep_len);
            let block = String::from_utf8_lossy(&block_bytes);
            let mut event: Option<String> = None;
            let mut data_lines: Vec<&str> = Vec::new();
            for line in block.split('\n') {
                let line = line.strip_suffix('\r').unwrap_or(line);
                if let Some(v) = line.strip_prefix("event:") {
                    event = Some(v.trim().to_string());
                } else if let Some(v) = line.strip_prefix("data:") {
                    data_lines.push(v.strip_prefix(' ').unwrap_or(v));
                }
                // Comment lines (starting with ":") and blank lines are ignored.
            }
            if !data_lines.is_empty() || event.is_some() {
                out.push(SseEvent {
                    event,
                    data: data_lines.join("\n"),
                });
            }
        }
        out
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
    fn parses_single_event_block() {
        let mut d = SseDecoder::new();
        let evs = d.push(b"event: message\ndata: {\"x\":1}\n\n");
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].event.as_deref(), Some("message"));
        assert_eq!(evs[0].data, "{\"x\":1}");
    }

    #[test]
    fn reassembles_across_chunk_boundaries() {
        let mut d = SseDecoder::new();
        assert!(d.push(b"data: hel").is_empty()); // partway through
        assert!(d.push(b"lo\n").is_empty()); // line complete but block not yet terminated
        let evs = d.push(b"\n"); // blank line finalizes the block
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].data, "hello");
    }

    #[test]
    fn concatenates_multiple_data_lines() {
        let mut d = SseDecoder::new();
        let evs = d.push(b"data: a\ndata: b\n\n");
        assert_eq!(evs[0].data, "a\nb");
    }

    #[test]
    fn handles_two_events_in_one_push() {
        let mut d = SseDecoder::new();
        let evs = d.push(b"data: 1\n\ndata: 2\n\n");
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].data, "1");
        assert_eq!(evs[1].data, "2");
    }

    #[test]
    fn reassembles_multibyte_char_split_across_pushes() {
        // "★" = E2 98 85. Push the first two bytes first, then deliver the
        // remaining byte plus the block terminator in the following push.
        let mut d = SseDecoder::new();
        assert!(d.push(b"data: \xe2\x98").is_empty());
        let evs = d.push(b"\x85\n\n");
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].data, "★");

        // Also confirm that reassembly recovers the same way for a split
        // spanning a 3-character string.
        let mut d2 = SseDecoder::new();
        let s = "★☆♪";
        let mut line = b"data: ".to_vec();
        line.extend_from_slice(s.as_bytes());
        let mid = line.len() - 1; // cut through the 1st and 2nd bytes of the trailing "♪" (3 bytes)
        assert!(d2.push(&line[..mid]).is_empty());
        let evs2 = d2.push(&line[mid..]);
        // There's no block terminator (blank line) yet, so nothing is finalized here.
        assert!(evs2.is_empty());
        let evs3 = d2.push(b"\n\n");
        assert_eq!(evs3.len(), 1);
        assert_eq!(evs3[0].data, s);
    }

    #[test]
    fn crlf_framed_event_terminates() {
        let mut d = SseDecoder::new();
        let evs = d.push(b"data: hello\r\n\r\n");
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].data, "hello");
    }

    #[test]
    fn crlf_reassembles_across_boundary() {
        let mut d = SseDecoder::new();
        assert!(d.push(b"data: hel").is_empty());
        assert!(d.push(b"lo\r\n").is_empty());
        let evs = d.push(b"\r\n");
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].data, "hello");
    }
}
