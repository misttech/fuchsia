// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::reader::BlockService;
use crate::{Blobs, PageRequest};
use std::ops::Range;
use std::sync::Arc;
use zx::sys::zx_page_request_command_t::ZX_PAGER_VMO_READ;
use zx::{Packet, PacketContents, Port, Rights, UserPacket};

/// Runs a synchronous event loop that listens on `port` for pager page requests,
/// resolves the requested blob using `blobs`, creates a page request using
/// `request_factory`, and invokes `Blob::read_range`.
///
/// Notice that buffer management and page supply (such as calling `zx::Pager::supply_pages`
/// or transmitting over `vmo-fifo`) are entirely delegated to the `PageRequest` implementation
/// returned by `request_factory`.
pub fn run_pager_loop<B: PageRequest>(
    port: &Port,
    service: Arc<dyn BlockService>,
    blobs: &Blobs,
    mut request_factory: impl FnMut(u64, Range<u64>) -> B + Send + 'static,
) {
    loop {
        match port.wait(zx::MonotonicInstant::INFINITE) {
            Ok(packet) => match packet.contents() {
                PacketContents::User(_) => break,
                PacketContents::Pager(pager_packet) => {
                    let key = packet.key();
                    let command = pager_packet.command();
                    if command == ZX_PAGER_VMO_READ {
                        let offset = pager_packet.range().start;
                        let length = pager_packet.range().end - offset;
                        let range = offset..offset + length;
                        if let Some(blob) = blobs.get(key) {
                            let page_request = request_factory(key, range.clone());
                            blob.read_range(range, service.as_ref(), page_request);
                        }
                    }
                }
                _ => {}
            },
            Err(_) => break,
        }
    }
}

/// A handle to a spawned background thread running `run_pager_loop`.
/// Dropping this handle sends a shutdown packet to the port and cleanly joins the thread.
pub struct PagerThread {
    port: Port,
    blobs: Arc<Blobs>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl PagerThread {
    /// Spawns a background thread running `run_pager_loop`.
    pub fn spawn<B: PageRequest + 'static>(
        port: Port,
        service: Arc<dyn BlockService>,
        blobs: Arc<Blobs>,
        request_factory: impl FnMut(u64, Range<u64>) -> B + Send + 'static,
    ) -> Self {
        let thread_port = port.duplicate_handle(Rights::SAME_RIGHTS).expect("duplicate port");
        let thread_blobs = Arc::clone(&blobs);
        let thread = std::thread::spawn(move || {
            run_pager_loop(&thread_port, service, &thread_blobs, request_factory);
        });
        Self { port, blobs, thread: Some(thread) }
    }

    /// Returns a reference to the active blob registry.
    pub fn blobs(&self) -> &Arc<Blobs> {
        &self.blobs
    }
}

impl Drop for PagerThread {
    fn drop(&mut self) {
        let packet = Packet::from_user_packet(0, 0, UserPacket::from_u8_array([0; 32]));
        let _ = self.port.queue(&packet);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::tests::FakeBlockService;
    use crate::testing::TestVecBuffer;

    #[fuchsia::test]
    fn test_pager_thread_lifecycle() {
        let port = Port::create();
        let service = Arc::new(FakeBlockService::new(vec![0u8; 4096]));
        let blobs = Arc::new(Blobs::new());
        let thread =
            PagerThread::spawn(port, service, blobs, |_key, _range| TestVecBuffer::new(4096).0);
        drop(thread);
    }

    #[fuchsia::test]
    fn test_pager_packet() {
        let port = Port::create();
        let pager = zx::Pager::create(zx::PagerOptions::empty()).expect("create pager");
        let vmo = pager.create_vmo(zx::VmoOptions::empty(), &port, 1234, 4096).expect("create vmo");

        let vmo_clone = vmo.duplicate_handle(Rights::SAME_RIGHTS).expect("duplicate vmo");
        let _reader_thread = std::thread::spawn(move || {
            let mut b = [0u8; 1];
            let _ = vmo_clone.read(&mut b, 0);
        });

        let packet = port.wait(zx::MonotonicInstant::INFINITE).expect("wait packet");
        assert_eq!(packet.key(), 1234);
        if let PacketContents::Pager(pager_packet) = packet.contents() {
            assert_eq!(pager_packet.command(), ZX_PAGER_VMO_READ);
            assert_eq!(pager_packet.range(), 0..4096);
        } else {
            panic!("Expected pager packet");
        }
    }
}
