// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use crate::{Endpoint, Interface, ZeroPacket};
use futures::io::{AsyncRead, AsyncWrite};
use std::future::Future;
use std::io::Write;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
use tokio::sync::RwLock;

/// Maximum size of a single bulk URB submitted to Linux usbfs (512 KiB).
///
/// This is an engineering heuristic: it maximizes xHCI DMA burst throughput
/// while remaining safely under kernel memory limits.
const MAX_USBFS_BULK_WRITE_SIZE: usize = 512 * 1024;

/// Minimum bulk URB write size when downscaling due to memory pressure (16 KiB).
/// Matches Android fastboot standards.
const MIN_USBFS_BULK_WRITE_SIZE: usize = 16 * 1024;

/// Maximum number of bulk URBs queued in flight concurrently (16 URBs = 8 MiB).
///
/// This heuristic keeps the host controller's DMA hardware ring continuously saturated
/// without stalling between chunks, while remaining well within the Linux usbfs memory
/// budget (`usbfs_memory_mb`, default 16 MiB) and the interface URB pool limit (32).
const MAX_IN_FLIGHT_URBS: usize = 16;

/// Maximum number of retries when kernel usbfs memory is exhausted (`ENOMEM`)
/// at the minimum chunk size before failing the transfer.
const MAX_ENOMEM_RETRIES: usize = 10;

/// Default timeout for awaiting completion of an in-flight bulk write URB.
pub const DEFAULT_BULK_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

fn is_out_of_memory(err: &crate::Error) -> bool {
    match err {
        crate::Error::IOError(io_err) => {
            io_err.raw_os_error() == Some(libc::ENOMEM)
                || io_err.kind() == std::io::ErrorKind::OutOfMemory
        }
        _ => false,
    }
}

/// Wraps an `Interface` and impls AsyncRead and AsyncWrite and reads and
/// writes to the appropriate In/Out endpoints of the interface
pub struct BulkInterface {
    inner: Arc<Interface>,
    guard: Arc<RwLock<()>>,
    read_future: Option<Pin<Box<dyn Future<Output = std::io::Result<Box<[u8]>>> + Send>>>,
    write_future: Option<Pin<Box<dyn Future<Output = std::io::Result<usize>> + Send>>>,
    max_chunk_size: usize,
    enomem_retries: usize,
    write_timeout: std::time::Duration,
}

impl std::fmt::Debug for BulkInterface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        f.debug_struct("BulkInterface")
            .field("read_future", &self.read_future.is_some())
            .field("write_future", &self.write_future.is_some())
            .field("max_chunk_size", &self.max_chunk_size)
            .field("enomem_retries", &self.enomem_retries)
            .field("write_timeout", &self.write_timeout)
            .finish()
    }
}

enum SubmissionErrorAction {
    /// In-flight URBs must be drained before submitting more.
    DrainInFlight,
    /// Chunk size was downscaled or bounded retry was triggered; wake and re-poll.
    Retry,
    /// Memory exhaustion retries were exceeded or error is unrecoverable.
    Fatal(std::io::Error),
}

impl BulkInterface {
    pub fn new(inner: Interface) -> Self {
        Self {
            inner: Arc::new(inner),
            guard: Arc::new(RwLock::new(())),
            read_future: None,
            write_future: None,
            max_chunk_size: MAX_USBFS_BULK_WRITE_SIZE,
            enomem_retries: 0,
            write_timeout: DEFAULT_BULK_WRITE_TIMEOUT,
        }
    }

    pub fn with_write_timeout(mut self, write_timeout: std::time::Duration) -> Self {
        self.write_timeout = write_timeout;
        self
    }

    pub fn set_write_timeout(&mut self, write_timeout: std::time::Duration) {
        self.write_timeout = write_timeout;
    }

    /// Handles an error returned by `try_write_defer_wait`.
    ///
    /// Manages dynamic downscaling of chunk sizes under `ENOMEM`, draining of in-flight
    /// buffers, and bounded retry backoff at minimum chunk size.
    fn handle_submission_error(
        &mut self,
        err: crate::Error,
        in_flight_len: usize,
        bytes_submitted: usize,
        current_chunk_size: usize,
        cx: &mut std::task::Context<'_>,
    ) -> SubmissionErrorAction {
        if is_out_of_memory(&err) {
            if in_flight_len > 0 {
                log::debug!(
                    "Kernel usbfs memory saturated after submitting {} bytes across {} in-flight URBs; draining before submitting more",
                    bytes_submitted,
                    in_flight_len
                );
                return SubmissionErrorAction::DrainInFlight;
            }
            if current_chunk_size > MIN_USBFS_BULK_WRITE_SIZE {
                let new_chunk_size =
                    std::cmp::max(MIN_USBFS_BULK_WRITE_SIZE, current_chunk_size / 2);
                log::info!(
                    "Downscaling bulk URB chunk size from {} to {} due to ENOMEM",
                    current_chunk_size,
                    new_chunk_size
                );
                self.max_chunk_size = new_chunk_size;
                cx.waker().wake_by_ref();
                return SubmissionErrorAction::Retry;
            }
            let retries = self.enomem_retries;
            self.enomem_retries += 1;
            if retries < MAX_ENOMEM_RETRIES {
                log::warn!(
                    "Kernel usbfs memory saturated at minimum chunk size ({} bytes); waiting 50ms before retry ({}/{})",
                    MIN_USBFS_BULK_WRITE_SIZE,
                    retries + 1,
                    MAX_ENOMEM_RETRIES
                );
                std::thread::sleep(std::time::Duration::from_millis(50));
                cx.waker().wake_by_ref();
                return SubmissionErrorAction::Retry;
            }
        }
        log::warn!("Error submitting bulk URB: {}", err);
        let err_msg = if is_out_of_memory(&err) {
            format!("Error submitting to bulk endpoint: {err} Linux usbfs_memory_mb exhausted")
        } else {
            format!("Error submitting to bulk endpoint: {err}")
        };
        SubmissionErrorAction::Fatal(std::io::Error::new(std::io::ErrorKind::Other, err_msg))
    }
}

impl AsyncRead for BulkInterface {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        mut buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        log::debug!("BulkInterface Poll read: {:?}", self);
        if self.read_future.is_none() {
            let inner_ref = self.inner.clone();
            let guard_ref = self.guard.clone();
            let mut buffer = buf.to_vec().into_boxed_slice();
            let read_future = async move {
                // Get the bulk in interface
                for endpoint in inner_ref.endpoints() {
                    if let Endpoint::BulkIn(bie) = endpoint {
                        let _guard = guard_ref.read().await;
                        log::debug!(
                            "Found bulk in endpoint, reading to buf of len: {}",
                            buffer.len()
                        );
                        bie.read(&mut buffer).await.map_err(|e| {
                            std::io::Error::new(
                                std::io::ErrorKind::Other,
                                format!("Error reading from bulk endpoint: {}", e),
                            )
                        })?;
                        return Ok(buffer);
                    }
                }
                Err(std::io::Error::new(std::io::ErrorKind::NotFound, "No bulk in endpoint found"))
            };

            self.read_future = Some(Box::pin(read_future));
        }

        let future = self.read_future.as_mut().unwrap();
        match future.as_mut().poll(cx) {
            Poll::Ready(Ok(buffer)) => {
                self.read_future = None;
                Poll::Ready(buf.write(&buffer))
            }
            Poll::Ready(Err(e)) => {
                self.read_future = None;
                Poll::Ready(Err(e))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for BulkInterface {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        if self.write_future.is_none() {
            let Ok(guard) = self.guard.clone().try_write_owned() else {
                let guard_ref = self.guard.clone();
                let wait_lock = async move {
                    let _g = guard_ref.write_owned().await;
                    Ok(0)
                };
                return match self.write_future.insert(Box::pin(wait_lock)).as_mut().poll(cx) {
                    Poll::Ready(Ok(0)) => {
                        self.write_future = None;
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                    Poll::Ready(Ok(s)) => Poll::Ready(Ok(s)),
                    Poll::Ready(Err(e)) => {
                        self.write_future = None;
                        Poll::Ready(Err(e))
                    }
                    Poll::Pending => Poll::Pending,
                };
            };

            let Some(boe) = self.inner.endpoints().into_iter().find_map(|endpoint| {
                if let Endpoint::BulkOut(boe) = endpoint { Some(boe) } else { None }
            }) else {
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "No bulk out endpoint found",
                )));
            };

            let current_chunk_size = self.max_chunk_size;
            let to_write = std::cmp::min(buf.len(), current_chunk_size * MAX_IN_FLIGHT_URBS);
            let mut in_flight = std::collections::VecDeque::new();
            let mut bytes_submitted = 0;

            // Pipeline up to MAX_IN_FLIGHT_URBS (or until the interface URB pool is
            // exhausted) directly from `buf` into kernel URBs without intermediate copies.
            // If the buffer is larger than this batch or the pool runs out of URBs, we stop
            // submitting and await the in-flight requests. Returning `Poll::Ready(Ok(bytes_submitted))`
            // fulfills standard `AsyncWrite::poll_write` partial-write semantics, allowing
            // the caller (e.g. `write_all`) to re-invoke `poll_write` for subsequent chunks.
            for chunk in buf[..to_write].chunks(current_chunk_size) {
                match boe.try_write_defer_wait(chunk, ZeroPacket::DoNotSend) {
                    Ok(Some(wait_fut)) => {
                        in_flight.push_back(wait_fut);
                        bytes_submitted += chunk.len();
                        if in_flight.len() >= MAX_IN_FLIGHT_URBS {
                            break;
                        }
                    }
                    Ok(None) => {
                        // Pool has no free URBs right now.
                        break;
                    }
                    Err(e) => {
                        match self.handle_submission_error(
                            e,
                            in_flight.len(),
                            bytes_submitted,
                            current_chunk_size,
                            cx,
                        ) {
                            SubmissionErrorAction::DrainInFlight => break,
                            SubmissionErrorAction::Retry => return Poll::Pending,
                            SubmissionErrorAction::Fatal(io_err) => {
                                return Poll::Ready(Err(io_err));
                            }
                        }
                    }
                }
            }

            if bytes_submitted > 0 {
                self.enomem_retries = 0;
            }

            if in_flight.is_empty() {
                // All URBs are in flight; yield so reaper thread can reap completed ones
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }

            log::debug!(
                "Submitted {} bytes across {} pipelined URBs directly from buffer",
                bytes_submitted,
                in_flight.len()
            );

            let write_timeout = self.write_timeout;
            let write_future = async move {
                let _guard = guard;
                while let Some(fut) = in_flight.pop_front() {
                    let res = tokio::time::timeout(write_timeout, fut).await.map_err(|_| {
                        log::warn!(
                            "Timed out after {:?} awaiting bulk URB completion",
                            write_timeout
                        );
                        std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            format!("Timed out after {:?} writing to bulk endpoint", write_timeout),
                        )
                    })?;
                    res.map_err(|e| {
                        log::warn!("Error awaiting bulk URB completion: {}", e);
                        std::io::Error::new(
                            std::io::ErrorKind::Other,
                            format!("Error writing to bulk endpoint: {}", e),
                        )
                    })?;
                }
                Ok(bytes_submitted)
            };

            self.write_future = Some(Box::pin(write_future));
        }

        let future = self.write_future.as_mut().unwrap();
        match future.as_mut().poll(cx) {
            Poll::Ready(Ok(size)) => {
                self.write_future = None;
                Poll::Ready(Ok(size))
            }
            Poll::Ready(Err(e)) => {
                log::debug!("Poll write: error: {:?}", e);
                self.write_future = None;
                Poll::Ready(Err(e))
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(
        self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_out_of_memory() {
        let enomem_err = crate::Error::IOError(std::io::Error::from_raw_os_error(libc::ENOMEM));
        assert!(is_out_of_memory(&enomem_err));

        let other_err = crate::Error::IOError(std::io::Error::from_raw_os_error(libc::EINVAL));
        assert!(!is_out_of_memory(&other_err));

        let short_err = crate::Error::ShortWrite(100, 50);
        assert!(!is_out_of_memory(&short_err));
    }
}
