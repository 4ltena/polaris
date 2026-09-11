//! Bounded result-only framing. Closing this transport does not stop its owner.
use polaris_desktop_protocol::{codec, source_recovery};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::{Duration, timeout};

const FRAME_DEADLINE: Duration = Duration::from_secs(2);

#[derive(Debug, thiserror::Error)]
pub enum RecoveryTransportError {
    #[error("recovery channel I/O")]
    Io(#[from] std::io::Error),
    #[error("recovery channel schema")]
    Codec(#[from] codec::CodecError),
    #[error("recovery channel deadline")]
    Deadline,
    #[error("recovery channel is unusable")]
    Unusable,
    #[error("recovery owner unavailable")]
    OwnerUnavailable,
}

/// Runs only this channel. An error never cancels, drops or terminates the
/// engine. Once enqueued, a result save belongs to the engine even if this
/// future is cancelled while waiting for its acknowledgement.
#[cfg(target_os = "macos")]
pub async fn serve_source_recovery<S>(
    stream: S,
    owner: crate::SourceRecoveryHandle,
) -> Result<(), RecoveryTransportError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    serve_source_recovery_draining(stream, owner, std::future::pending()).await
}

/// Stop accepting on `stop`, but finish the one accepted owner operation and
/// its bounded response write. The caller closes the owner's ingress when the
/// engine ends so an unanswered queued request cannot wait forever.
#[cfg(target_os = "macos")]
pub async fn serve_source_recovery_draining<S, F>(
    stream: S,
    owner: crate::SourceRecoveryHandle,
    stop: F,
) -> Result<(), RecoveryTransportError>
where
    S: AsyncRead + AsyncWrite + Unpin,
    F: std::future::Future<Output = ()>,
{
    tokio::pin!(stop);
    let mut transport = RecoveryTransport::new(stream);
    loop {
        let request = tokio::select! {
            biased;
            _ = &mut stop => return Ok(()),
            request = transport.receive() => match request? {
                Some(request) => request,
                None => return Ok(()),
            },
        };
        let response = owner
            .try_request(request.clone())
            .map_err(|_| RecoveryTransportError::OwnerUnavailable)?
            .await
            .map_err(|_| RecoveryTransportError::OwnerUnavailable)?;
        transport.send(&request, &response).await?;
    }
}

pub struct RecoveryTransport<S> {
    stream: S,
    usable: bool,
}

impl<S: AsyncRead + AsyncWrite + Unpin> RecoveryTransport<S> {
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            usable: true,
        }
    }

    /// Idle time is unbounded. Once a frame begins, partial bytes never extend
    /// its fixed deadline. EOF in a partial frame is an error, not a request.
    pub async fn receive(
        &mut self,
    ) -> Result<Option<source_recovery::Request>, RecoveryTransportError> {
        if !self.usable {
            return Err(RecoveryTransportError::Unusable);
        }
        let mut header = [0_u8; 4];
        if self.stream.read(&mut header[..1]).await? == 0 {
            return Ok(None);
        }
        self.usable = false;
        let result = timeout(FRAME_DEADLINE, async {
            self.stream.read_exact(&mut header[1..]).await?;
            let length = u32::from_be_bytes(header) as usize;
            if length == 0 || length > source_recovery::MAX_FRAME_BYTES {
                return Err(RecoveryTransportError::Codec(
                    codec::CodecError::FrameTooLarge,
                ));
            }
            let mut frame = Vec::with_capacity(length + 4);
            frame.extend_from_slice(&header);
            frame.resize(length + 4, 0);
            self.stream.read_exact(&mut frame[4..]).await?;
            match source_recovery::decode(&frame) {
                codec::Decode::Decoded { value, consumed } if consumed == frame.len() => {
                    Ok(Some(value))
                }
                codec::Decode::Invalid(error) => Err(error.into()),
                _ => Err(codec::CodecError::Schema.into()),
            }
        })
        .await
        .map_err(|_| RecoveryTransportError::Deadline)?;
        if result.is_ok() {
            self.usable = true;
        }
        result
    }

    /// The caller retains request identity and all owner evidence after errors.
    pub async fn send(
        &mut self,
        request: &source_recovery::Request,
        response: &source_recovery::Response,
    ) -> Result<(), RecoveryTransportError> {
        if !self.usable {
            return Err(RecoveryTransportError::Unusable);
        }
        response.validate_for_request(request)?;
        let frame = source_recovery::encode(response)?;
        self.usable = false;
        timeout(FRAME_DEADLINE, async {
            self.stream.write_all(&frame).await?;
            self.stream.flush().await
        })
        .await
        .map_err(|_| RecoveryTransportError::Deadline)??;
        self.usable = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn hello() -> source_recovery::Request {
        source_recovery::Request::Hello {
            version: Default::default(),
            request_id: polaris_desktop_protocol::ids::RequestId::new("hello").unwrap(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn blocked_reply_has_fixed_deadline_and_cannot_be_reused() {
        use polaris_desktop_protocol::ids::{EngineEpoch, ProjectId, SessionId};
        let (_peer, stream) = tokio::io::duplex(8);
        let mut transport = RecoveryTransport::new(stream);
        let request = hello();
        let response = source_recovery::Response {
            version: Default::default(),
            request_id: request.request_id().clone(),
            engine_epoch: EngineEpoch::new("epoch").unwrap(),
            project_id: ProjectId::new("project").unwrap(),
            session_id: SessionId::new("session").unwrap(),
            state: source_recovery::State::Working,
            result: None,
            retry_target: None,
            error: None,
        };
        assert!(matches!(
            transport.send(&request, &response).await,
            Err(RecoveryTransportError::Deadline)
        ));
        assert!(matches!(
            transport.send(&request, &response).await,
            Err(RecoveryTransportError::Unusable)
        ));
    }

    #[tokio::test]
    async fn coalesced_frames_are_read_individually_then_clean_eof() {
        let (mut peer, stream) = tokio::io::duplex(1024);
        let frame = source_recovery::encode(&hello()).unwrap();
        peer.write_all(&frame).await.unwrap();
        peer.write_all(&frame).await.unwrap();
        peer.shutdown().await.unwrap();
        let mut transport = RecoveryTransport::new(stream);
        assert_eq!(transport.receive().await.unwrap(), Some(hello()));
        assert_eq!(transport.receive().await.unwrap(), Some(hello()));
        assert!(transport.receive().await.unwrap().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn partial_frame_timeout_poison_is_not_reset_by_another_receive() {
        let (mut peer, stream) = tokio::io::duplex(1024);
        peer.write_all(&[0]).await.unwrap();
        let mut transport = RecoveryTransport::new(stream);
        assert!(matches!(
            transport.receive().await,
            Err(RecoveryTransportError::Deadline)
        ));
        assert!(matches!(
            transport.receive().await,
            Err(RecoveryTransportError::Unusable)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_partial_receive_keeps_channel_unusable() {
        let (mut peer, stream) = tokio::io::duplex(1024);
        peer.write_all(&[0]).await.unwrap();
        let mut transport = RecoveryTransport::new(stream);
        assert!(
            timeout(Duration::from_millis(10), transport.receive())
                .await
                .is_err()
        );
        assert!(matches!(
            transport.receive().await,
            Err(RecoveryTransportError::Unusable)
        ));
    }

    #[tokio::test]
    async fn excessive_length_is_rejected_before_body_allocation() {
        let (mut peer, stream) = tokio::io::duplex(16);
        peer.write_all(&8193_u32.to_be_bytes()).await.unwrap();
        let mut transport = RecoveryTransport::new(stream);
        assert!(matches!(
            transport.receive().await,
            Err(RecoveryTransportError::Codec(
                codec::CodecError::FrameTooLarge
            ))
        ));
        assert!(matches!(
            transport.receive().await,
            Err(RecoveryTransportError::Unusable)
        ));
    }
}
