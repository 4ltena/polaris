//! SSE(text/event-stream)を逐次デコードする。バイト片を push すると、
//! 空行で区切られた完成イベントだけを返す。行やブロックの分割に耐える。

pub struct SseDecoder {
    buf: Vec<u8>,
}

pub struct SseEvent {
    /// SSE の `event:` フィールド。現行のプロバイダは `data` 側の JSON `type` で
    /// 分岐するため本体コードからは読まれないが、SSE モデルの一部として保持する
    /// (テストとデバッグでは参照される)。
    #[allow(dead_code)]
    pub event: Option<String>,
    pub data: String,
}

/// バッファ中で最初に現れる空行区切り(`"\n\n"` または `"\r\n\r\n"`)を探す。
/// 戻り値は `(区切り開始位置, 区切りのバイト長)`。CRLF 版も LF 版も
/// 混在しうるため、両方を走査していちばん手前のものを採用する。
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
        // マルチバイト文字が push 境界で分断されうるため、デコードせず
        // 生バイトのまま蓄積する。完成ブロック(空行終端)だけを
        // 切り出してからデコードすれば、分断された文字が
        // U+FFFD 化することはない。
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        // 完成ブロック(空行 "\n\n" または "\r\n\r\n" 終端)を順に切り出す。
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
                // コメント行(":" 始まり)や空行は無視。
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
        assert!(d.push(b"data: hel").is_empty()); // 途中まで
        assert!(d.push(b"lo\n").is_empty()); // 行完成だがブロック未終端
        let evs = d.push(b"\n"); // 空行でブロック確定
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
        // "あ" = E3 81 82。1バイト目のみを先に push し、
        // 残り2バイトとブロック終端を後続 push で渡す。
        let mut d = SseDecoder::new();
        assert!(d.push(b"data: \xe3\x81").is_empty());
        let evs = d.push(b"\x82\n\n");
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].data, "あ");

        // 3文字にまたがる分断でも同様に復元できることを確認。
        let mut d2 = SseDecoder::new();
        let s = "日本語";
        let mut line = b"data: ".to_vec();
        line.extend_from_slice(s.as_bytes());
        let mid = line.len() - 1; // 末尾の"語"(3バイト)の1バイト目・2バイト目までで切る
        assert!(d2.push(&line[..mid]).is_empty());
        let evs2 = d2.push(&line[mid..]);
        // まだブロック終端(空行)がないため、ここでは何も確定しない。
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
