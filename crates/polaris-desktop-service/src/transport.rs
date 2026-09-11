//! P1 codecの有界非同期I/O。単一writerのフレーム全体に期限を適用する。

use polaris_desktop_protocol::{codec, request::Request};
use serde::{Serialize, de::DeserializeOwned};
use std::{io, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::{OwnedSemaphorePermit, Semaphore, mpsc},
    task::JoinSet,
    time::timeout,
};

pub(crate) const OUTPUT_EVENTS: usize = 256;
pub(crate) const OUTPUT_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const INPUT_REQUESTS: usize = 8;

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("通信I/Oに失敗しました")]
    Io(#[from] io::Error),
    #[error("通信フレームが不正です: {0}")]
    Codec(#[from] codec::CodecError),
    #[error("保存を確認できませんでした")]
    Store(#[from] polaris_core::desktop_store::StoreError),
    #[error("serviceの設定が不正です")]
    Options,
    #[error("送信queueの上限または送信期限に達しました")]
    OutputClosed,
    #[error("通信workerが終了しました")]
    Worker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Eof,
    Ready,
    OutputClosed,
}

pub(crate) async fn read_frame<R: AsyncRead + Unpin, T: DeserializeOwned>(
    reader: &mut R,
) -> Result<Option<T>, ServiceError> {
    let mut header = [0; 4];
    if reader.read(&mut header[..1]).await? == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut header[1..]).await.map_err(eof)?;
    let size = u32::from_be_bytes(header) as usize;
    if size > codec::MAX_FRAME_BYTES {
        return Err(codec::CodecError::FrameTooLarge.into());
    }
    let mut body = vec![0; size];
    reader.read_exact(&mut body).await.map_err(eof)?;
    Ok(Some(codec::from_json(&body)?))
}

fn eof(error: io::Error) -> ServiceError {
    if error.kind() == io::ErrorKind::UnexpectedEof {
        codec::CodecError::UnexpectedEof.into()
    } else {
        error.into()
    }
}

struct Frame {
    bytes: Vec<u8>,
    // writerの保留中もbyte予算を保持する。
    _permit: OwnedSemaphorePermit,
}

pub(crate) struct Outbox {
    sender: mpsc::Sender<Frame>,
    bytes: Arc<Semaphore>,
}

impl Outbox {
    pub(crate) fn send(&self, value: &impl Serialize) -> Result<(), ServiceError> {
        let bytes = codec::encode(value)?;
        let permit = self
            .bytes
            .clone()
            .try_acquire_many_owned(bytes.len() as u32)
            .map_err(|_| ServiceError::OutputClosed)?;
        self.sender
            .try_send(Frame {
                bytes,
                _permit: permit,
            })
            .map_err(|_| ServiceError::OutputClosed)
    }
}

pub(crate) enum WorkerExit {
    Reader(Result<(), ServiceError>),
    Writer(Result<(), ServiceError>),
}

pub(crate) fn workers<R, W>(
    reader: R,
    writer: W,
    deadline: Duration,
) -> (Outbox, mpsc::Receiver<Request>, JoinSet<WorkerExit>)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (input, receiver) = mpsc::channel(INPUT_REQUESTS);
    let (sender, mut output) = mpsc::channel::<Frame>(OUTPUT_EVENTS - 1);
    let mut tasks = JoinSet::new();
    tasks.spawn(async move {
        let mut reader = reader;
        let result = loop {
            match read_frame(&mut reader).await {
                Ok(Some(request)) => {
                    if input.send(request).await.is_err() {
                        break Ok(());
                    }
                }
                Ok(None) => break Ok(()),
                Err(error) => break Err(error),
            }
        };
        WorkerExit::Reader(result)
    });
    tasks.spawn(async move {
        let mut writer = writer;
        let result = async {
            while let Some(frame) = output.recv().await {
                timeout(deadline, async {
                    writer.write_all(&frame.bytes).await?;
                    writer.flush().await
                })
                .await
                .map_err(|_| ServiceError::OutputClosed)??;
            }
            Ok(())
        }
        .await;
        WorkerExit::Writer(result)
    });
    (
        Outbox {
            sender,
            bytes: Arc::new(Semaphore::new(OUTPUT_BYTES)),
        },
        receiver,
        tasks,
    )
}
