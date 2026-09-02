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

/// Maximum size of a single bulk URB submitted to Linux usbfs (256 KiB).
///
/// This is an engineering heuristic matching upstream fastboot/adb conventions:
/// it maximizes xHCI DMA burst throughput while avoiding kernel scatter-gather
/// memory allocation failures or fragmentation issues on host systems.
const MAX_USBFS_BULK_WRITE_SIZE: usize = 256 * 1024;

/// Maximum number of bulk URBs queued in flight concurrently (16 URBs = 4 MiB).
///
/// This heuristic keeps the host controller's DMA hardware ring continuously saturated
/// without stalling between chunks, while remaining well within the Linux usbfs memory
/// budget (`usbfs_memory_mb`, default 16 MiB) and the interface URB pool limit.
const MAX_IN_FLIGHT_URBS: usize = 16;
const MAX_WRITE_BUFFER_SIZE: usize = MAX_USBFS_BULK_WRITE_SIZE * MAX_IN_FLIGHT_URBS;

/// Wraps an `Interface` and impls AsyncRead and AsyncWrite and reads and
/// writes to the appropriate In/Out endpoints of the interface
pub struct BulkInterface {
    inner: Arc<Interface>,
    guard: Arc<RwLock<()>>,
    read_future: Option<Pin<Box<dyn Future<Output = std::io::Result<Vec<u8>>> + Send>>>,
    write_future: Option<Pin<Box<dyn Future<Output = std::io::Result<usize>> + Send>>>,
}

impl std::fmt::Debug for BulkInterface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        f.debug_struct("BulkInterface")
            .field("read_future", &self.read_future.is_some())
            .field("write_future", &self.write_future.is_some())
            .finish()
    }
}

impl BulkInterface {
    pub fn new(inner: Interface) -> Self {
        Self {
            inner: Arc::new(inner),
            guard: Arc::new(RwLock::new(())),
            read_future: None,
            write_future: None,
        }
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
            let mut buffer = buf[..].to_vec();
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
            let to_write = std::cmp::min(buf.len(), MAX_WRITE_BUFFER_SIZE);
            let buffer = buf[..to_write].to_vec();
            let inner_ref = self.inner.clone();
            let guard_ref = self.guard.clone();
            let write_future = async move {
                // Get the bulk out interface
                for endpoint in inner_ref.endpoints() {
                    if let Endpoint::BulkOut(boe) = endpoint {
                        log::debug!(
                            "Breaking write of {} bytes into pipelined URB chunks of up to {}",
                            buffer.len(),
                            MAX_USBFS_BULK_WRITE_SIZE
                        );
                        let _guard = guard_ref.write().await;
                        let mut in_flight = std::collections::VecDeque::new();
                        for chunk in buffer.chunks(MAX_USBFS_BULK_WRITE_SIZE) {
                            let wait_fut = boe
                                .write_defer_wait(chunk, ZeroPacket::DoNotSend)
                                .await
                                .map_err(|e| {
                                    log::warn!("Error submitting bulk URB: {}", e);
                                    std::io::Error::new(
                                        std::io::ErrorKind::Other,
                                        format!("Error submitting to bulk endpoint: {}", e),
                                    )
                                })?;
                            in_flight.push_back(wait_fut);

                            if in_flight.len() >= MAX_IN_FLIGHT_URBS {
                                if let Some(fut) = in_flight.pop_front() {
                                    fut.await.map_err(|e| {
                                        log::warn!("Error awaiting bulk URB completion: {}", e);
                                        std::io::Error::new(
                                            std::io::ErrorKind::Other,
                                            format!("Error writing to bulk endpoint: {}", e),
                                        )
                                    })?;
                                }
                            }
                        }

                        while let Some(fut) = in_flight.pop_front() {
                            fut.await.map_err(|e| {
                                log::warn!("Error draining bulk URB: {}", e);
                                std::io::Error::new(
                                    std::io::ErrorKind::Other,
                                    format!("Error writing to bulk endpoint: {}", e),
                                )
                            })?;
                        }

                        return Ok(buffer.len());
                    }
                }
                Err(std::io::Error::new(std::io::ErrorKind::NotFound, "No bulk out endpoint found"))
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
