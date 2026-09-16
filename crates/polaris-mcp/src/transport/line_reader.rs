//! Bounded line framing for managed MCP stdio.

use super::sensitive::SensitiveValues;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, ready};
use tokio::io::{AsyncBufRead, AsyncRead, BufReader, ReadBuf};

/// Bound each JSON-lines message before the SDK's unbounded read_until.
/// Keep state here, not in a cancellable receive Future. Once violated, fail
/// permanently so the tail of an oversized message cannot become a new one.
pub(super) struct LineReader<R: AsyncRead> {
    inner: BufReader<R>,
    line: Vec<u8>,
    offset: usize,
    ready: bool,
    failed: Option<crate::Error>,
    secrets: SensitiveValues,
    failures: super::failure::Failures,
}

impl<R: AsyncRead> LineReader<R> {
    pub(super) fn new(inner: R) -> Self {
        Self {
            inner: BufReader::new(inner),
            line: Vec::new(),
            offset: 0,
            ready: false,
            failed: None,
            secrets: SensitiveValues::default(),
            failures: super::failure::Failures::default(),
        }
    }

    pub(super) fn with_secrets(mut self, secrets: SensitiveValues) -> Self {
        self.secrets = secrets;
        self
    }

    pub(super) fn with_failures(mut self, failures: super::failure::Failures) -> Self {
        self.failures = failures;
        self
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for LineReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if let Some(error) = self.failed {
            return Poll::Ready(Err(io::Error::other(error)));
        }
        if output.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let this = &mut *self;
        // Retain a complete bounded line until credential reflection has been
        // checked. No partial secret-bearing line reaches SDK parsing/logging.
        while !this.ready {
            let input = ready!(Pin::new(&mut this.inner).poll_fill_buf(cx))?;
            if input.is_empty() && this.line.is_empty() {
                return Poll::Ready(Ok(()));
            }
            let count = input
                .iter()
                .position(|b| *b == b'\n')
                .map_or(input.len(), |n| n + 1);
            if count > super::MAX_MESSAGE_BYTES - this.line.len() {
                this.failed = Some(crate::Error::Limit);
                this.failures.record(crate::Error::Limit);
                return Poll::Ready(Err(io::Error::other(crate::Error::Limit)));
            }
            let complete = count == 0 || input[count - 1] == b'\n';
            this.line.extend_from_slice(&input[..count]);
            Pin::new(&mut this.inner).consume(count);
            if complete {
                if let Err(error) = this.secrets.json_bytes(&this.line) {
                    this.failed = Some(error);
                    this.failures.record(error);
                    return Poll::Ready(Err(io::Error::other(error)));
                }
                this.ready = true;
            }
        }
        let count = output.remaining().min(this.line.len() - this.offset);
        output.put_slice(&this.line[this.offset..this.offset + count]);
        this.offset += count;
        if this.offset == this.line.len() {
            this.line.clear();
            this.offset = 0;
            this.ready = false;
        }
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn bounds_messages_not_connection_and_rejects_an_unterminated_tail() {
        let limit = super::super::MAX_MESSAGE_BYTES;
        let mut line = vec![b'x'; limit];
        line[limit - 1] = b'\n';
        let input = [line.as_slice(), line.as_slice()].concat();
        let mut output = Vec::new();
        LineReader::new(input.as_slice())
            .read_to_end(&mut output)
            .await
            .unwrap();
        assert_eq!(output, input);
        let input = vec![b'x'; limit + 1];
        let mut reader = LineReader::new(input.as_slice());
        assert_eq!(
            reader
                .read_to_end(&mut Vec::new())
                .await
                .unwrap_err()
                .to_string(),
            "mcp_limit"
        );
        assert!(reader.read_u8().await.is_err());
    }

    #[tokio::test]
    async fn cancelled_receive_does_not_reset_the_current_line() {
        use tokio::io::AsyncWriteExt;
        let limit = super::super::MAX_MESSAGE_BYTES;
        let (mut writer, input) = tokio::io::duplex(limit + 1);
        let mut reader = LineReader::new(input);
        writer.write_all(&vec![b'x'; limit]).await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(5), reader.read_u8())
                .await
                .is_err()
        );
        writer.write_all(b"\n").await.unwrap();
        assert_eq!(reader.read_u8().await.unwrap_err().to_string(), "mcp_limit");
    }

    #[tokio::test]
    async fn reflected_secret_never_reaches_the_sdk_reader() {
        let input = b"{\"result\":\"fixture-\\u006bey\"}\n";
        let mut reader = LineReader::new(input.as_slice())
            .with_secrets(SensitiveValues::new(["fixture-key".into()]));
        let mut output = Vec::new();
        assert_eq!(
            reader
                .read_to_end(&mut output)
                .await
                .unwrap_err()
                .to_string(),
            "mcp_auth"
        );
        assert!(output.is_empty());
    }
}
