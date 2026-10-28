// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::reply::Reply;

use chrono::Duration;
use command::Command;
use fuchsia_async::TimeoutExt;
use futures::io::{AsyncRead, AsyncWrite};
use futures::lock::{Mutex, MutexLockFuture};
use futures::{AsyncReadExt, AsyncWriteExt};
use std::future::Future;
use std::io::Read;
use std::sync::Arc;
use thiserror::Error;

pub mod command;
pub mod reply;
pub mod test_transport;

pub use crate::command::MAX_COMMAND_LENGTH;

pub const BUFFER_SIZE: usize = 4 * 1024 * 1024; // 4 MB

/// According to the fastboot specification this packet size should be
/// negotiated based on the speed of the device
///
/// Max packet size must be 64 bytes for full-speed, 512 bytes for high-speed
/// and 1024 bytes for Super Speed USB.
///
/// But we are leaving it at the maximum size to maximize compatibility
const MAX_PACKET_SIZE: usize = 1024;
const DEFAULT_READ_TIMEOUT_SECS: i64 = 30;
const DEFAULT_WRITE_TIMEOUT_SECS: u64 = 30;

async fn write_with_timeout<T: AsyncWrite + Unpin>(
    interface: &mut T,
    buf: &[u8],
    timeout: std::time::Duration,
) -> std::io::Result<()> {
    let end_time = std::time::Instant::now() + timeout;
    interface
        .write_all(buf)
        .on_timeout(end_time, || {
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timed out writing to fastboot interface",
            ))
        })
        .await
}

#[derive(Debug, Clone)]
pub struct FastbootContext {
    send_lock: Arc<Mutex<()>>,
    transfer_lock: Arc<Mutex<()>>,
}

impl FastbootContext {
    pub fn new() -> Self {
        Self { send_lock: Arc::new(Mutex::new(())), transfer_lock: Arc::new(Mutex::new(())) }
    }

    pub fn lock_transfer(&self) -> MutexLockFuture<'_, ()> {
        self.transfer_lock.lock()
    }
}

#[derive(Debug, Error)]
pub enum ReadError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Parse error: {0}")]
    Parse(#[from] crate::reply::ParseReplyError),
    #[error("Timed out")]
    Timeout,
}

#[derive(Debug, Error)]
pub enum FastbootError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Read error: {0}")]
    Read(#[from] ReadError),
    #[error("Download error: {0}")]
    Download(#[from] DownloadError),
    #[error("Upload error: {0}")]
    Upload(#[from] UploadError),
    #[error("Parse error: {0}")]
    Parse(#[from] crate::reply::ParseReplyError),
    #[error("Serialize command error: {0}")]
    Serialize(#[from] crate::command::SerializeCommandError),
    #[error("Invalid file size: {0}")]
    InvalidFileSize(#[from] std::num::TryFromIntError),
}

#[derive(Debug, Error)]
pub enum SendError {
    #[error("timed out reading a reply from device")]
    Timeout,
}

#[derive(Debug, Error)]
pub enum DownloadError {
    #[error("Did not get expected Data reply: {:?}", reply)]
    UnexpectedReply { reply: Reply },
    #[error("Could not verify download: {0}")]
    CouldNotVerifyDownload(#[source] ReadError),
    #[error("Could not read to interface")]
    CouldNotReadToInterface(#[source] std::io::Error),
}

#[derive(Debug, Error)]
pub enum UploadError {
    #[error("Target responded with wrong data size - received:{} expected:{}", received, expected)]
    WrongSizeResponse { received: u32, expected: u32 },
    #[error("Could not read bytes to upload")]
    CouldNotReadBytesToUpload { source: std::io::Error },
    #[error("Could not write to interface")]
    CouldNotWriteToInterface(#[source] std::io::Error),
    #[error("Could not verify upload: {0}")]
    CouldNotVerifyUpload(#[source] ReadError),
    #[error("Did not get expected Data reply: {:?}", reply)]
    UnexpectedReply { reply: Reply },
}

pub trait InfoListener {
    fn on_info(&self, info: String) -> impl Future<Output = Result<(), ReadError>> + Send {
        async move {
            log::info!("Fastboot Info: \"{}\"", info);
            Ok(())
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct LogInfoListener;
impl InfoListener for LogInfoListener {}

pub trait UploadProgressListener {
    fn on_started(&self, size: usize) -> impl Future<Output = Result<(), UploadError>> + Send;
    fn on_progress(
        &self,
        bytes_written: u64,
    ) -> impl Future<Output = Result<(), UploadError>> + Send;
    fn on_error(&self, error: &UploadError)
    -> impl Future<Output = Result<(), UploadError>> + Send;
    fn on_finished(&self) -> impl Future<Output = Result<(), UploadError>> + Send;
}

async fn read_from_interface<T: AsyncRead + Unpin>(interface: &mut T) -> Result<Reply, ReadError> {
    let mut buf = vec![0u8; MAX_PACKET_SIZE];
    let size = interface.read(&mut buf).await?;
    let trimmed = &buf[..size];
    let reply = Reply::try_from(trimmed).map_err(|e| {
        log::debug!("fastboot: could not parse reply: {}", String::from_utf8_lossy(trimmed));
        ReadError::Parse(e)
    })?;
    log::debug!("fastboot: received {reply:?}: {}", String::from_utf8_lossy(trimmed));
    Ok(reply)
}

async fn read<T: AsyncRead + Unpin>(
    interface: &mut T,
    listener: &(impl InfoListener + Sync),
) -> Result<Reply, ReadError> {
    read_with_timeout(
        interface,
        listener,
        std::time::Duration::from_secs(DEFAULT_READ_TIMEOUT_SECS as u64),
    )
    .await
}

async fn read_and_log_info<T: AsyncRead + Unpin>(interface: &mut T) -> Result<Reply, ReadError> {
    read_and_log_info_with_timeout(interface, Duration::seconds(DEFAULT_READ_TIMEOUT_SECS)).await
}

pub async fn read_and_log_info_with_timeout<T: AsyncRead + Unpin>(
    interface: &mut T,
    duration: Duration,
) -> Result<Reply, ReadError> {
    let std_duration = duration.to_std().expect("converting chrono Duration to std");
    read_with_timeout(interface, &LogInfoListener, std_duration).await
}

/// Returns true if an I/O error represents a transient USB read timeout during polling.
fn is_timeout_error(err: &std::io::Error) -> bool {
    if err.kind() == std::io::ErrorKind::TimedOut {
        return true;
    }
    #[cfg(target_os = "linux")]
    {
        if err.raw_os_error() == Some(110) {
            return true;
        }
        // Some legacy USB transports on Linux (such as usb_bulk) do not map raw OS errors
        // to ErrorKind::TimedOut, but instead return ErrorKind::Other with a formatted
        // error message (e.g. "Read error: -110" or "os error 110").
        let s = err.to_string();
        if s == "Read error: -110" || s.contains("os error 110") || s.contains("ETIMEDOUT") {
            return true;
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let s = err.to_string();
        if s.contains("ETIMEDOUT") {
            return true;
        }
    }
    false
}

async fn read_with_timeout<T: AsyncRead + Unpin>(
    interface: &mut T,
    listener: &(impl InfoListener + Sync),
    timeout: std::time::Duration,
) -> Result<Reply, ReadError> {
    let end_time = std::time::Instant::now() + timeout;
    loop {
        match read_from_interface(interface).on_timeout(end_time, || Err(ReadError::Timeout)).await
        {
            Ok(Reply::Info(msg)) => listener.on_info(msg).await?,
            Err(ReadError::Io(ref e)) if is_timeout_error(e) => {
                // If we get a TIMEDOUT response, keep reading -- the underlying USB
                // transport can return timeout every 800ms while polling for a result.
            }
            #[cfg(target_os = "macos")]
            Err(ReadError::Io(_)) => {
                // usb_bulk returns different values on mac vs. linux. On Linux it
                // returns ETIMEDOUT, but on the Mac it's just a generic -1. (And
                // Apple doesn't actually document how to determine whether a read
                // has timed out.)  So on Mac, we'll ignore IO errors, and cross
                // our fingers.
            }
            Err(e) => return Err(e),
            other => return other,
        }
        // We can't actually rely on `on_timeout()` to time out, because while
        // `usb_bulk` claims that it implements `AsyncRead`, it's not actually
        // async.  As a result, on_timeout() doesn't work.  We'll leave it in
        // to avoid problems in the future, and so our unit tests can remain
        // asynchronous.
        if std::time::Instant::now() > end_time {
            return Err(ReadError::Timeout);
        }
    }
}

/// Result of draining unread startup messages from the interface.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct DrainResult {
    /// Number of asynchronous INFO messages successfully drained.
    pub info_count: usize,
    /// Preserved non-INFO reply if the device returned a terminal response.
    pub pending_reply: Option<Reply>,
}

/// Drains any pending unread asynchronous INFO messages from the interface.
///
/// Flushes startup messages left in the device's TX FIFO by bootloaders (e.g., U-Boot)
/// to prevent bulk IN endpoint desynchronization.
///
/// The `timeout` parameter specifies the idle timeout (maximum time to wait between
/// successive messages) while draining.
///
/// If a non-INFO reply (such as OKAY, FAIL, or DATA) is encountered, draining stops
/// immediately and the reply is returned in `DrainResult` to prevent silent data loss.
pub async fn drain_unread<T: AsyncRead + Unpin>(
    interface: &mut T,
    listener: &(impl InfoListener + Sync),
    timeout: std::time::Duration,
) -> Result<DrainResult, ReadError> {
    let mut result = DrainResult::default();
    let mut deadline = std::time::Instant::now() + timeout;
    loop {
        match read_from_interface(interface).on_timeout(deadline, || Err(ReadError::Timeout)).await
        {
            Ok(Reply::Info(msg)) => {
                log::debug!("fastboot: drained asynchronous INFO message: {msg}");
                listener.on_info(msg).await?;
                result.info_count += 1;
                // Reset the idle timeout deadline since we received a message
                deadline = std::time::Instant::now() + timeout;
            }
            Ok(non_info) => {
                log::warn!(
                    "fastboot: drain_unread encountered unexpected terminal reply: {non_info:?}"
                );
                result.pending_reply = Some(non_info);
                break;
            }
            Err(ReadError::Timeout) => {
                break;
            }
            Err(ReadError::Parse(crate::reply::ParseReplyError::ReplyTooShort {
                reply_len: 0,
            })) => {
                break;
            }
            Err(ReadError::Io(ref e)) if is_timeout_error(e) => {
                // Ignore polling timeout and let the manual timeout check below handle it
            }
            #[cfg(target_os = "macos")]
            Err(ReadError::Io(_)) => {
                // Ignore generic Mac timeout and let the manual timeout check below handle it
            }
            Err(e) => {
                log::error!("fastboot: transport error during drain_unread: {e:?}");
                return Err(e);
            }
        }

        // Fallback manual timeout check for blocking transports like usb_bulk
        if std::time::Instant::now() > deadline {
            break;
        }
    }
    Ok(result)
}

pub async fn send_with_listener<T: AsyncRead + AsyncWrite + Unpin>(
    ctx: FastbootContext,
    cmd: Command,
    interface: &mut T,
    listener: &(impl InfoListener + Sync),
) -> Result<Reply, FastbootError> {
    let _lock = ctx.send_lock.lock().await;
    let bytes = Vec::<u8>::try_from(&cmd)?;
    log::debug!("Fastboot: writing command {cmd:?}: {}", String::from_utf8_lossy(&bytes));
    interface.write_all(&bytes).await?;
    Ok(read(interface, listener).await?)
}

pub async fn send<T: AsyncRead + AsyncWrite + Unpin>(
    ctx: FastbootContext,
    cmd: Command,
    interface: &mut T,
) -> Result<Reply, FastbootError> {
    let _lock = ctx.send_lock.lock().await;
    let bytes = Vec::<u8>::try_from(&cmd)?;
    log::debug!("Fastboot: writing command {cmd:?}: {}", String::from_utf8_lossy(&bytes));
    interface.write_all(&bytes).await?;
    Ok(read_and_log_info(interface).await?)
}

pub async fn send_with_timeout<T: AsyncRead + AsyncWrite + Unpin>(
    ctx: FastbootContext,
    cmd: Command,
    interface: &mut T,
    timeout: Duration,
) -> Result<Reply, FastbootError> {
    let _lock = ctx.send_lock.lock().await;
    let bytes = Vec::<u8>::try_from(&cmd)?;
    log::debug!("Fastboot: writing command {cmd:?}: {}", String::from_utf8_lossy(&bytes));
    interface.write_all(&bytes).await?;
    let std_timeout = timeout.to_std().expect("converting chrono Duration to std");
    Ok(read_with_timeout(interface, &LogInfoListener, std_timeout).await?)
}

const PIPELINE_BUFFER_CHUNKS: usize = 2;

pub async fn upload<T: AsyncRead + AsyncWrite + Unpin, R: Read + Send + 'static>(
    ctx: FastbootContext,
    size: u32,
    buf: R,
    interface: &mut T,
    listener: &impl UploadProgressListener,
) -> Result<Reply, FastbootError> {
    upload_with_read_timeout(
        ctx,
        size,
        buf,
        interface,
        listener,
        Duration::seconds(DEFAULT_READ_TIMEOUT_SECS),
    )
    .await
}

pub async fn upload_with_read_timeout<
    T: AsyncRead + AsyncWrite + Unpin,
    R: Read + Send + 'static,
>(
    ctx: FastbootContext,
    size: u32,
    mut buf: R,
    interface: &mut T,
    listener: &impl UploadProgressListener,
    timeout: Duration,
) -> Result<Reply, FastbootError> {
    let _lock = ctx.lock_transfer().await;
    // We are sending "Download" in our "upload" function because we are the
    // host -- from the device's point of view, it is a download
    let reply = send(ctx.clone(), Command::Download(size), interface).await?;
    let Reply::Data(s) = reply else {
        return Err(FastbootError::Upload(UploadError::UnexpectedReply { reply }));
    };

    if s != size {
        let err = UploadError::WrongSizeResponse { received: s, expected: size };
        log::error!("{}", err);
        listener.on_error(&err).await?;
        return Err(FastbootError::Upload(err));
    }
    listener.on_started(size.try_into().unwrap()).await?;
    log::debug!("fastboot: writing {} bytes", size);

    let (tx, mut rx) = tokio::sync::mpsc::channel(PIPELINE_BUFFER_CHUNKS);

    let reader_thread = std::thread::spawn(move || {
        let mut remaining = size as usize;
        while remaining > 0 {
            let to_read = std::cmp::min(remaining, BUFFER_SIZE);
            let mut chunk = vec![0; to_read];
            match buf.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    chunk.truncate(n);
                    remaining -= n;
                    if tx.blocking_send(Ok(chunk)).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.blocking_send(Err(e));
                    break;
                }
            }
        }
    });

    let write_timeout = std::cmp::min(
        timeout.to_std().unwrap_or(std::time::Duration::from_secs(DEFAULT_WRITE_TIMEOUT_SECS)),
        std::time::Duration::from_secs(DEFAULT_WRITE_TIMEOUT_SECS),
    );
    let mut upload_err = None;
    while let Some(chunk_res) = rx.recv().await {
        match chunk_res {
            Ok(chunk) => {
                let n = chunk.len();
                match write_with_timeout(interface, &chunk, write_timeout).await {
                    Err(e) => {
                        let err = UploadError::CouldNotWriteToInterface(e);
                        log::error!("{}", err);
                        let _ = listener.on_error(&err).await;
                        upload_err = Some(FastbootError::Upload(err));
                        break;
                    }
                    Ok(()) => {
                        listener.on_progress(n.try_into().unwrap()).await?;
                        log::trace!("fastboot: wrote {} bytes", n);
                    }
                }
            }
            Err(e) => {
                let err = UploadError::CouldNotReadBytesToUpload { source: e };
                log::error!("{}", err);
                let _ = listener.on_error(&err).await;
                upload_err = Some(FastbootError::Upload(err));
                break;
            }
        }
    }

    // Close the receiver so the reader thread unblocks if waiting on a full channel,
    // then join the thread before checking errors.
    drop(rx);
    let _ = reader_thread.join();

    if let Some(err) = upload_err {
        return Err(err);
    }

    log::debug!("fastboot: completed writing {} bytes", size);

    match read_and_log_info_with_timeout(interface, timeout).await {
        Ok(reply) => {
            listener.on_finished().await?;
            Ok(reply)
        }
        Err(e) => {
            let err = UploadError::CouldNotVerifyUpload(e);
            log::error!("{}", err);
            listener.on_error(&err).await?;
            Err(FastbootError::Upload(err))
        }
    }
}

pub async fn upload_from_reader<T: AsyncRead + AsyncWrite + Unpin, R: Read + ?Sized>(
    ctx: FastbootContext,
    size: u32,
    buf: &mut R,
    interface: &mut T,
    listener: &impl UploadProgressListener,
    timeout: Duration,
) -> Result<Reply, FastbootError> {
    let _lock = ctx.lock_transfer().await;
    let reply = send(ctx.clone(), Command::Download(size), interface).await?;
    let Reply::Data(s) = reply else {
        return Err(FastbootError::Upload(UploadError::UnexpectedReply { reply }));
    };

    if s != size {
        let err = UploadError::WrongSizeResponse { received: s, expected: size };
        log::error!("{}", err);
        listener.on_error(&err).await?;
        return Err(FastbootError::Upload(err));
    }
    listener.on_started(size.try_into().unwrap()).await?;
    log::debug!("fastboot: writing {} bytes", size);

    let write_timeout = std::cmp::min(
        timeout.to_std().unwrap_or(std::time::Duration::from_secs(DEFAULT_WRITE_TIMEOUT_SECS)),
        std::time::Duration::from_secs(DEFAULT_WRITE_TIMEOUT_SECS),
    );
    let mut remaining = size as usize;
    let chunk_size = std::cmp::min(size as usize, BUFFER_SIZE);
    let mut bytes = vec![0; chunk_size];
    loop {
        let to_read = std::cmp::min(remaining, BUFFER_SIZE);
        if to_read == 0 {
            break;
        }
        match buf.read(&mut bytes[..to_read]) {
            Ok(0) => break,
            Ok(n) => {
                remaining -= n;
                match write_with_timeout(interface, &bytes[..n], write_timeout).await {
                    Err(e) => {
                        let err = UploadError::CouldNotWriteToInterface(e);
                        log::error!("{}", err);
                        listener.on_error(&err).await?;
                        return Err(FastbootError::Upload(err));
                    }
                    Ok(()) => {
                        listener.on_progress(n.try_into().unwrap()).await?;
                        log::trace!("fastboot: wrote {} bytes", n);
                    }
                }
            }
            Err(e) => {
                let err = UploadError::CouldNotReadBytesToUpload { source: e };
                log::error!("{}", err);
                listener.on_error(&err).await?;
                return Err(FastbootError::Upload(err));
            }
        }
    }

    log::debug!("fastboot: completed writing {} bytes", size);

    match read_and_log_info_with_timeout(interface, timeout).await {
        Ok(reply) => {
            listener.on_finished().await?;
            Ok(reply)
        }
        Err(e) => {
            let err = UploadError::CouldNotVerifyUpload(e);
            log::error!("{}", err);
            listener.on_error(&err).await?;
            Err(FastbootError::Upload(err))
        }
    }
}

pub async fn download<T: AsyncRead + AsyncWrite + Unpin>(
    ctx: FastbootContext,
    path: &String,
    interface: &mut T,
) -> Result<Reply, FastbootError> {
    let _lock = ctx.lock_transfer().await;
    // We are sending "Upload" in our "download" function because we are the
    // host -- from the device's point of view, it is an upload
    let reply = send(ctx.clone(), Command::Upload, interface).await?;
    log::debug!("got reply from upload command: {:?}", reply);
    match reply {
        Reply::Data(s) => {
            let size = usize::try_from(s)?;
            let mut buffer: [u8; 100] = [0; 100];
            let mut bytes_read: usize = 0;
            let mut file = async_fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(path)
                .await?;
            while bytes_read < size {
                match interface.read(&mut buffer[..]).await {
                    Err(e) => {
                        return Err(FastbootError::Download(
                            DownloadError::CouldNotReadToInterface(e),
                        ));
                    }
                    Ok(len) => {
                        let len = if (bytes_read + len) > size { size - bytes_read } else { len };
                        bytes_read += len;
                        log::debug!("fastboot: upload got {bytes_read}/{size} bytes. Len {len}");
                        file.write_all(&buffer[..len]).await?;
                    }
                }
            }
            file.flush().await?;
            Ok(read_and_log_info(interface)
                .await
                .map_err(|e| FastbootError::Download(DownloadError::CouldNotVerifyDownload(e)))?)
        }
        rep @ _ => {
            return Err(FastbootError::Download(DownloadError::UnexpectedReply { reply: rep }));
        }
    }
}

////////////////////////////////////////////////////////////////////////////////
// tests

#[cfg(test)]
mod test {
    use super::*;
    use crate::command::ClientVariable;
    use crate::test_transport::TestTransport;
    use std::collections::VecDeque;
    use std::io::Cursor;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::{Context, Poll};

    #[derive(Debug, PartialEq)]
    enum UploadEvent {
        OnStarted(usize),
        OnProgress(u64),
        OnError(String),
        OnFinished,
    }

    struct PushEventsUploadProgressListener {
        event_queue: Arc<Mutex<Vec<UploadEvent>>>,
    }

    impl UploadProgressListener for PushEventsUploadProgressListener {
        async fn on_started(&self, size: usize) -> Result<(), UploadError> {
            let mut queue = self.event_queue.lock().await;
            queue.push(UploadEvent::OnStarted(size));
            Ok(())
        }
        async fn on_progress(&self, bytes_written: u64) -> Result<(), UploadError> {
            let mut queue = self.event_queue.lock().await;
            queue.push(UploadEvent::OnProgress(bytes_written));
            Ok(())
        }
        async fn on_error(&self, error: &UploadError) -> Result<(), UploadError> {
            let mut queue = self.event_queue.lock().await;
            queue.push(UploadEvent::OnError(error.to_string()));
            Ok(())
        }
        async fn on_finished(&self) -> Result<(), UploadError> {
            let mut queue = self.event_queue.lock().await;
            queue.push(UploadEvent::OnFinished);
            Ok(())
        }
    }

    #[fuchsia::test]
    async fn test_send_does_not_return_info_replies() {
        let mut test_transport = TestTransport::new();
        let ctx = FastbootContext::new();
        test_transport.push(Reply::Okay("0.4".to_string()));
        let response =
            send(ctx.clone(), Command::GetVar(ClientVariable::Version), &mut test_transport).await;
        assert!(!response.is_err());
        assert_eq!(response.unwrap(), Reply::Okay("0.4".to_string()));

        test_transport.extend([Reply::Info("Test".to_string()), Reply::Okay("0.4".to_string())]);
        let response_with_info =
            send(ctx.clone(), Command::GetVar(ClientVariable::Version), &mut test_transport).await;
        assert!(!response_with_info.is_err());
        assert_eq!(response_with_info.unwrap(), Reply::Okay("0.4".to_string()));

        test_transport.extend((0..10).map(|i| Reply::Info(format!("Test {i}"))));
        test_transport.push(Reply::Okay("0.4".to_string()));
        let response_with_info =
            send(ctx.clone(), Command::GetVar(ClientVariable::Version), &mut test_transport).await;
        assert!(!response_with_info.is_err());
        assert_eq!(response_with_info.unwrap(), Reply::Okay("0.4".to_string()));
    }

    #[fuchsia::test]
    #[allow(clippy::large_futures)]
    async fn test_uploading_data_to_partition() {
        let data: [u8; 14336] = [0; 14336];
        let mut test_transport = TestTransport::new();
        test_transport.extend([
            Reply::Data(14336),
            Reply::Info("Writing".to_string()),
            Reply::Okay("Done Writing".to_string()),
        ]);

        let events = Arc::new(Mutex::new(Vec::<UploadEvent>::new()));
        let listener = PushEventsUploadProgressListener { event_queue: events.clone() };

        let data_len = u32::try_from(data.len()).unwrap();
        let ctx = FastbootContext::new();
        let response =
            upload(ctx, data_len, Cursor::new(data), &mut test_transport, &listener).await;
        assert!(!response.is_err());
        assert_eq!(response.unwrap(), Reply::Okay("Done Writing".to_string()));

        let queue = events.lock().await;
        assert_eq!(
            *queue,
            vec![
                UploadEvent::OnStarted(14336),
                UploadEvent::OnProgress(14336),
                UploadEvent::OnFinished,
            ]
        );
    }

    #[fuchsia::test]
    async fn test_upload_from_reader_to_partition() {
        let data: [u8; 14336] = [0; 14336];
        let mut test_transport = TestTransport::new();
        test_transport.extend([
            Reply::Data(14336),
            Reply::Info("Writing".to_string()),
            Reply::Okay("Done Writing".to_string()),
        ]);

        let events = Arc::new(Mutex::new(Vec::<UploadEvent>::new()));
        let listener = PushEventsUploadProgressListener { event_queue: events.clone() };

        let data_len = u32::try_from(data.len()).unwrap();
        let ctx = FastbootContext::new();
        let mut reader = &data[..];
        let response = upload_from_reader(
            ctx,
            data_len,
            &mut reader,
            &mut test_transport,
            &listener,
            Duration::seconds(DEFAULT_READ_TIMEOUT_SECS),
        )
        .await;
        assert!(!response.is_err());
        assert_eq!(response.unwrap(), Reply::Okay("Done Writing".to_string()));

        let queue = events.lock().await;
        assert_eq!(
            *queue,
            vec![
                UploadEvent::OnStarted(14336),
                UploadEvent::OnProgress(14336),
                UploadEvent::OnFinished,
            ]
        );
    }

    #[fuchsia::test]
    async fn test_uploading_data_with_unexpected_reply() {
        let data: [u8; 1024] = [0; 1024];
        let mut test_transport = TestTransport::new();
        test_transport.push(Reply::Info("Writing".to_string()));
        let ctx = FastbootContext::new();

        let events = Arc::new(Mutex::new(Vec::<UploadEvent>::new()));
        let listener = PushEventsUploadProgressListener { event_queue: events.clone() };
        let data_len = u32::try_from(data.len()).unwrap();
        let response =
            upload(ctx, data_len, Cursor::new(data), &mut test_transport, &listener).await;
        assert!(response.is_err());
        let queue = events.lock().await;
        assert_eq!(*queue, vec![]);
    }

    #[fuchsia::test]
    async fn test_uploading_data_with_unexpected_data_size_reply() {
        let data: [u8; 1024] = [0; 1024];
        let mut test_transport = TestTransport::new();
        test_transport.push(Reply::Data(1000));
        let ctx = FastbootContext::new();

        let events = Arc::new(Mutex::new(Vec::<UploadEvent>::new()));
        let listener = PushEventsUploadProgressListener { event_queue: events.clone() };
        let data_len = u32::try_from(data.len()).unwrap();
        let response =
            upload(ctx, data_len, Cursor::new(data), &mut test_transport, &listener).await;
        assert!(response.is_err());
        let queue = events.lock().await;
        assert_eq!(
            *queue,
            vec![UploadEvent::OnError(
                "Target responded with wrong data size - received:1000 expected:1024".to_string()
            ),]
        );
    }

    #[fuchsia::test]
    async fn test_drain_unread_consumes_pending_info_messages() {
        let mut test_transport = TestTransport::new();
        test_transport.extend([
            Reply::Info("Startup notice 1".to_string()),
            Reply::Info("Startup notice 2".to_string()),
        ]);

        let drained = drain_unread(
            &mut test_transport,
            &LogInfoListener,
            std::time::Duration::from_millis(50),
        )
        .await
        .unwrap();
        assert_eq!(drained, DrainResult { info_count: 2, pending_reply: None });

        // After draining, the transport should be empty.
        let drained_more = drain_unread(
            &mut test_transport,
            &LogInfoListener,
            std::time::Duration::from_millis(10),
        )
        .await
        .unwrap();
        assert_eq!(drained_more, DrainResult { info_count: 0, pending_reply: None });
    }

    #[fuchsia::test]
    async fn test_drain_unread_stops_on_eof_when_empty() {
        let mut test_transport = TestTransport::new();
        let drained = drain_unread(
            &mut test_transport,
            &LogInfoListener,
            std::time::Duration::from_millis(10),
        )
        .await
        .unwrap();
        assert_eq!(drained, DrainResult { info_count: 0, pending_reply: None });
    }

    struct MockPendingTransport;

    impl AsyncRead for MockPendingTransport {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut [u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Pending
        }
    }

    #[fuchsia::test]
    async fn test_drain_unread_stops_on_timeout_when_pending() {
        let mut transport = MockPendingTransport;
        let drained =
            drain_unread(&mut transport, &LogInfoListener, std::time::Duration::from_millis(10))
                .await
                .unwrap();
        assert_eq!(drained, DrainResult { info_count: 0, pending_reply: None });
    }

    #[fuchsia::test]
    async fn test_drain_unread_preserves_unexpected_terminal_reply() {
        let mut test_transport = TestTransport::new();
        test_transport
            .extend([Reply::Info("Startup notice".to_string()), Reply::Okay("0.4".to_string())]);

        // Draining should consume the INFO message and stop upon seeing a non-INFO reply,
        // preserving the unexpected terminal reply in DrainResult.
        let drained = drain_unread(
            &mut test_transport,
            &LogInfoListener,
            std::time::Duration::from_millis(50),
        )
        .await
        .unwrap();
        assert_eq!(
            drained,
            DrainResult { info_count: 1, pending_reply: Some(Reply::Okay("0.4".to_string())) }
        );
    }

    struct MockRawTransport {
        chunks: VecDeque<std::io::Result<Vec<u8>>>,
    }

    impl MockRawTransport {
        fn new(chunks: impl IntoIterator<Item = std::io::Result<Vec<u8>>>) -> Self {
            Self { chunks: chunks.into_iter().collect() }
        }
    }

    impl AsyncRead for MockRawTransport {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<std::io::Result<usize>> {
            if let Some(res) = self.chunks.pop_front() {
                match res {
                    Ok(data) => {
                        assert!(
                            buf.len() >= data.len(),
                            "MockRawTransport buffer too small for chunk: {} < {}",
                            buf.len(),
                            data.len()
                        );
                        buf[..data.len()].copy_from_slice(&data);
                        Poll::Ready(Ok(data.len()))
                    }
                    Err(e) => Poll::Ready(Err(e)),
                }
            } else {
                Poll::Ready(Ok(0))
            }
        }
    }

    impl AsyncWrite for MockRawTransport {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[fuchsia::test]
    async fn test_read_with_timeout_fails_immediately_on_parse_error_containing_110() {
        // Device sends corrupted response "X110" that fails parsing and contains "110".
        let mut transport = MockRawTransport::new([Ok(b"X110".to_vec())]);
        let res = read_with_timeout(
            &mut transport,
            &LogInfoListener,
            std::time::Duration::from_millis(50),
        )
        .await;
        match res {
            Err(ReadError::Parse(crate::reply::ParseReplyError::UnknownReply {
                ref reply_type,
            })) if reply_type == "X110" => {}
            other => {
                panic!("Expected immediate ReadError::Parse(UnknownReply(\"X110\")), got {other:?}")
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[fuchsia::test]
    async fn test_read_with_timeout_retries_on_usb_timedout_io_error() {
        // First read yields OS error 110 (ETIMEDOUT); second read yields OKAYDone.
        let mut transport = MockRawTransport::new([
            Err(std::io::Error::from_raw_os_error(110)),
            Ok(b"OKAYDone".to_vec()),
        ]);
        let res =
            read_with_timeout(&mut transport, &LogInfoListener, std::time::Duration::from_secs(1))
                .await;
        assert_eq!(res.unwrap(), Reply::Okay("Done".to_string()));
    }

    #[fuchsia::test]
    async fn test_read_with_timeout_retries_on_kind_timedout_io_error() {
        // First read yields std::io::ErrorKind::TimedOut; second read yields OKAYDone.
        let mut transport = MockRawTransport::new([
            Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out")),
            Ok(b"OKAYDone".to_vec()),
        ]);
        let res =
            read_with_timeout(&mut transport, &LogInfoListener, std::time::Duration::from_secs(1))
                .await;
        assert_eq!(res.unwrap(), Reply::Okay("Done".to_string()));
    }

    #[fuchsia::test]
    async fn test_drain_unread_continues_past_intermittent_timedout_io_errors() {
        // Transport yields:
        // 1. INFO Notice 1
        // 2. ErrorKind::TimedOut (simulating an 800ms USB transport polling tick)
        // 3. INFO Notice 2
        let mut transport = MockRawTransport::new([
            Ok(b"INFO Notice 1".to_vec()),
            Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "polling timeout")),
            Ok(b"INFO Notice 2".to_vec()),
        ]);

        let drained =
            drain_unread(&mut transport, &LogInfoListener, std::time::Duration::from_millis(50))
                .await
                .unwrap();

        assert_eq!(drained, DrainResult { info_count: 2, pending_reply: None });
    }

    #[fuchsia::test]
    async fn test_drain_unread_stops_when_manual_deadline_exceeded() {
        // Transport returns TimedOut and deadline is 0, so manual deadline fallback triggers immediately.
        let mut transport = MockRawTransport::new([Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "polling timeout",
        ))]);

        let drained =
            drain_unread(&mut transport, &LogInfoListener, std::time::Duration::from_millis(0))
                .await
                .unwrap();

        assert_eq!(drained, DrainResult { info_count: 0, pending_reply: None });
    }

    struct MockWriteStallTransport {
        replies: VecDeque<Vec<u8>>,
        writes_allowed: usize,
    }

    impl MockWriteStallTransport {
        fn new(replies: impl IntoIterator<Item = Vec<u8>>, writes_allowed: usize) -> Self {
            Self { replies: replies.into_iter().collect(), writes_allowed }
        }
    }

    impl AsyncRead for MockWriteStallTransport {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<std::io::Result<usize>> {
            if let Some(data) = self.replies.pop_front() {
                buf[..data.len()].copy_from_slice(&data);
                Poll::Ready(Ok(data.len()))
            } else {
                Poll::Pending
            }
        }
    }

    impl AsyncWrite for MockWriteStallTransport {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            if self.writes_allowed > 0 {
                self.writes_allowed -= 1;
                Poll::Ready(Ok(buf.len()))
            } else {
                Poll::Pending
            }
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[fuchsia::test]
    async fn test_upload_with_read_timeout_fails_on_write_stall() {
        let data: [u8; 1024] = [0; 1024];
        // Allow 1 write (the "download:00000400" command), then stall on the payload write.
        let mut transport = MockWriteStallTransport::new([b"DATA00000400".to_vec()], 1);
        let events = Arc::new(Mutex::new(Vec::<UploadEvent>::new()));
        let listener = PushEventsUploadProgressListener { event_queue: events.clone() };
        let ctx = FastbootContext::new();

        let res = upload_with_read_timeout(
            ctx,
            1024,
            Cursor::new(data),
            &mut transport,
            &listener,
            Duration::milliseconds(50),
        )
        .on_timeout(std::time::Instant::now() + std::time::Duration::from_millis(500), || {
            panic!("upload_with_read_timeout hung on stalled write instead of timing out")
        })
        .await;

        match res {
            Err(FastbootError::Upload(UploadError::CouldNotWriteToInterface(ref e)))
                if e.kind() == std::io::ErrorKind::TimedOut => {}
            other => {
                panic!("Expected UploadError::CouldNotWriteToInterface(TimedOut), got {other:?}")
            }
        }
    }

    #[fuchsia::test]
    async fn test_upload_from_reader_fails_on_write_stall() {
        let data: [u8; 1024] = [0; 1024];
        // Allow 1 write (the "download:00000400" command), then stall on the payload write.
        let mut transport = MockWriteStallTransport::new([b"DATA00000400".to_vec()], 1);
        let events = Arc::new(Mutex::new(Vec::<UploadEvent>::new()));
        let listener = PushEventsUploadProgressListener { event_queue: events.clone() };
        let ctx = FastbootContext::new();
        let mut reader = &data[..];

        let res = upload_from_reader(
            ctx,
            1024,
            &mut reader,
            &mut transport,
            &listener,
            Duration::milliseconds(50),
        )
        .on_timeout(std::time::Instant::now() + std::time::Duration::from_millis(500), || {
            panic!("upload_from_reader hung on stalled write instead of timing out")
        })
        .await;

        match res {
            Err(FastbootError::Upload(UploadError::CouldNotWriteToInterface(ref e)))
                if e.kind() == std::io::ErrorKind::TimedOut => {}
            other => {
                panic!("Expected UploadError::CouldNotWriteToInterface(TimedOut), got {other:?}")
            }
        }
    }
}
