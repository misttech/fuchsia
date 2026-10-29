// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::termios::DISABLED_CHAR;
use starnix_uapi::error;
use starnix_uapi::errors::Errno;
use starnix_uapi::vfs::FdEvents;
use std::collections::VecDeque;

/// Maximum number of bytes that fit into a single canonical terminal input line (or `read_queue`
/// window). Matches gVisor and Linux `N_TTY_BUF_SIZE`.
pub(crate) const CANON_MAX_BYTES: usize = 4096;

/// Maximum number of non-EOL bytes buffered in `read_queue` + `line_buffer` in non-canonical mode
/// (or in canonical mode prior to the terminating delimiter).
pub(crate) const NON_CANON_MAX_BYTES: usize = CANON_MAX_BYTES - 1;

/// Maximum unprocessed byte capacity in `wait_buffers` before writes apply backpressure (`EAGAIN`).
const WAIT_BUFFER_MAX_BYTES: usize = 131072;

/// Abstraction over a data source providing bytes written into a terminal endpoint.
pub trait InputBuffer {
    fn available(&self) -> usize;
    fn read_to_vec_exact(&mut self, size: usize) -> Result<Vec<u8>, Errno>;
}

/// Abstraction over a data sink receiving bytes read from a terminal endpoint.
pub trait OutputBuffer {
    fn available(&self) -> usize;
    fn write(&mut self, data: &[u8]) -> Result<usize, Errno>;
}

#[derive(Debug, Default)]
struct ReadPacket {
    data: Vec<u8>,
    has_eof: bool,
}

/// Manages a unidirectional byte pipeline through three stages:
/// 1. `wait_buffers`: unprocessed raw bytes awaiting line-discipline transformation.
/// 2. `line_buffer`: in-progress canonical line (being edited) or non-canonical batch.
/// 3. `read_queue`: completed packets ready for endpoint reads.
#[derive(Debug, Default)]
pub(crate) struct Queue {
    /// Queue of completed lines or chunks ready to be read.
    /// An empty `data` vector with `has_eof: true` represents a standalone EOF (`read` returns 0).
    read_queue: VecDeque<ReadPacket>,

    /// Incomplete line or chunk currently being assembled before moving to `read_queue`.
    line_buffer: Vec<u8>,

    /// Unprocessed bytes waiting for space in `read_queue` / `line_buffer`.
    wait_buffers: VecDeque<Vec<u8>>,

    /// Total byte count across all buffers in `wait_buffers`.
    total_wait_buffer_length: usize,
}

impl Queue {
    /// Returns `POLLOUT` if `wait_buffers` has room for more bytes.
    pub(crate) fn write_readiness(&self) -> FdEvents {
        if self.total_wait_buffer_length < WAIT_BUFFER_MAX_BYTES {
            FdEvents::POLLOUT
        } else {
            FdEvents::empty()
        }
    }

    /// Returns `POLLIN` if `read_queue` has at least one packet (including a 0-byte EOF marker).
    pub(crate) fn read_readiness(&self) -> FdEvents {
        if !self.read_queue.is_empty() { FdEvents::POLLIN } else { FdEvents::empty() }
    }

    /// Returns the total readable payload bytes in `read_queue` (for `FIONREAD` and EOF checks).
    pub(crate) fn readable_size(&self) -> usize {
        self.read_queue.iter().map(|packet| packet.data.len()).sum()
    }

    /// Returns the total buffer occupancy across `read_queue` (counting 1 byte per `VEOF` marker,
    /// matching `__DISABLED_CHAR` in Linux `n_tty`'s `read_buf`) and `line_buffer`.
    pub(crate) fn buffer_len(&self) -> usize {
        self.read_queue
            .iter()
            .map(|packet| packet.data.len() + usize::from(packet.has_eof))
            .sum::<usize>()
            + self.line_buffer.len()
    }

    /// Returns `true` if `read_queue` currently holds at least one unread packet.
    pub(crate) fn has_unread_packets(&self) -> bool {
        !self.read_queue.is_empty()
    }

    /// Returns a slice of the current incomplete `line_buffer`.
    pub(crate) fn line_bytes(&self) -> &[u8] {
        &self.line_buffer
    }

    /// Returns `true` if the current incomplete `line_buffer` is empty.
    pub(crate) fn is_line_empty(&self) -> bool {
        self.line_buffer.is_empty()
    }

    /// Clears the current incomplete `line_buffer`.
    pub(crate) fn clear_line_buffer(&mut self) {
        self.line_buffer.clear();
    }

    /// Drains and returns the trailing bytes of `line_buffer` starting at `start`.
    pub(crate) fn drain_line_from(&mut self, start: usize) -> Vec<u8> {
        self.line_buffer.drain(start..).collect()
    }

    /// Appends a mutable reference to `line_buffer` for output formatting, or extends raw bytes.
    pub(crate) fn line_buffer_mut(&mut self) -> &mut Vec<u8> {
        &mut self.line_buffer
    }

    /// Appends a single processed input byte to `line_buffer`, doubling `0xff` when `parmrk_double`
    /// is true (matching Linux `n_tty`, where canonical erase and reprint see both `0xff` bytes).
    pub(crate) fn push_line_byte(&mut self, byte: u8, parmrk_double: bool) {
        if parmrk_double {
            self.line_buffer.extend_from_slice(&[0xff, 0xff]);
        } else {
            self.line_buffer.push(byte);
        }
    }

    /// Moves a non-empty `line_buffer` into `read_queue` as a packet without EOF.
    pub(crate) fn flush_line_buffer(&mut self) {
        if !self.line_buffer.is_empty() {
            self.read_queue.push_back(ReadPacket {
                data: std::mem::take(&mut self.line_buffer),
                has_eof: false,
            });
        }
    }

    /// Flushes `line_buffer` into `read_queue` with `has_eof: true` (even if `line_buffer` is
    /// empty, producing a 0-byte EOF packet).
    pub(crate) fn flush_line_with_eof(&mut self) {
        let data = std::mem::take(&mut self.line_buffer);
        self.read_queue.push_back(ReadPacket { data, has_eof: true });
    }

    /// Pushes a pre-formatted packet (such as echoes or raw `!OPOST` output) directly to
    /// `read_queue`.
    pub(crate) fn push_ready_packet(&mut self, data: Vec<u8>) {
        if !data.is_empty() {
            self.read_queue.push_back(ReadPacket { data, has_eof: false });
        }
    }

    /// Clears all processed, in-progress, and unprocessed buffers in the queue.
    pub(crate) fn flush(&mut self) {
        self.flush_buffers();
        self.flush_unprocessed();
    }

    /// Clears only the processed `read_queue` and in-progress `line_buffer`, preserving
    /// `wait_buffers`.
    pub(crate) fn flush_buffers(&mut self) {
        self.read_queue.clear();
        self.line_buffer.clear();
    }

    /// Clears only the unprocessed `wait_buffers`.
    pub(crate) fn flush_unprocessed(&mut self) {
        self.wait_buffers.clear();
        self.total_wait_buffer_length = 0;
    }

    /// Coalesces `read_queue` packets and `line_buffer` when `ICANON` or `EXTPROC` changes.
    pub(crate) fn coalesce_for_mode_change(&mut self, is_canon_enabled: bool) {
        let mut combined = Vec::new();
        for packet in self.read_queue.drain(..) {
            combined.extend(packet.data);
            if packet.has_eof {
                combined.push(DISABLED_CHAR);
            }
        }
        combined.append(&mut self.line_buffer);
        if !combined.is_empty() {
            let has_eof = is_canon_enabled && combined.last() == Some(&DISABLED_CHAR);
            if has_eof {
                combined.pop();
            }
            self.read_queue.push_back(ReadPacket { data: combined, has_eof });
        }
    }

    /// Reads available bytes from `data` into `wait_buffers`, returning `EAGAIN` if full.
    pub(crate) fn enqueue_wait_buffer(
        &mut self,
        data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        let room = WAIT_BUFFER_MAX_BYTES - self.total_wait_buffer_length;
        let data_length = data.available();
        if room == 0 && data_length > 0 {
            return error!(EAGAIN);
        }
        let buffer = data.read_to_vec_exact(std::cmp::min(room, data_length))?;
        let read_from_userspace = buffer.len();
        self.total_wait_buffer_length += read_from_userspace;
        self.wait_buffers.push_back(buffer);
        Ok(read_from_userspace)
    }

    /// Pops the next unprocessed chunk from `wait_buffers`.
    pub(crate) fn pop_wait_buffer(&mut self) -> Option<Vec<u8>> {
        let buffer = self.wait_buffers.pop_front()?;
        self.total_wait_buffer_length -= buffer.len();
        Some(buffer)
    }

    /// Pushes unconsumed trailing bytes back to the front of `wait_buffers`.
    pub(crate) fn requeue_wait_buffer(&mut self, remaining: Vec<u8>) {
        self.total_wait_buffer_length += remaining.len();
        self.wait_buffers.push_front(remaining);
    }

    /// Reads ready packets from `read_queue` into `data`.
    ///
    /// Returns `(bytes_written, should_drain_wait_buffers)`. When `stop_after_packet` is `true`
    /// (canonical input mode), stops after a single line/packet.
    pub(crate) fn read_into(
        &mut self,
        data: &mut dyn OutputBuffer,
        stop_after_packet: bool,
    ) -> Result<(usize, bool), Errno> {
        if self.read_queue.is_empty() {
            return error!(EAGAIN);
        }
        if data.available() == 0 {
            return Ok((0, false));
        }

        let mut total_written = 0;
        while let Some(mut packet) = self.read_queue.pop_front() {
            if packet.data.is_empty() {
                if total_written > 0 {
                    // Complete the read with the bytes already copied and leave the empty EOF
                    // packet at the front of `read_queue` for the next read.
                    self.read_queue.push_front(packet);
                }
                break;
            }

            match data.write(&packet.data) {
                Ok(written) => {
                    total_written += written;
                    if written < packet.data.len() {
                        let remaining = packet.data.split_off(written);
                        self.read_queue
                            .push_front(ReadPacket { data: remaining, has_eof: packet.has_eof });
                        break;
                    }
                    if stop_after_packet || packet.has_eof {
                        break;
                    }
                }
                Err(err) => {
                    self.read_queue.push_front(packet);
                    if total_written > 0 {
                        return Ok((total_written, true));
                    }
                    return Err(err);
                }
            }
        }

        Ok((total_written, true))
    }

    #[cfg(test)]
    pub(crate) fn read_packet_count(&self) -> usize {
        self.read_queue.len()
    }

    #[cfg(test)]
    pub(crate) fn wait_buffer_len(&self) -> usize {
        self.total_wait_buffer_length
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct VecOutputBuffer {
        data: Vec<u8>,
        limit: usize,
    }

    impl OutputBuffer for VecOutputBuffer {
        fn available(&self) -> usize {
            self.limit.saturating_sub(self.data.len())
        }

        fn write(&mut self, data: &[u8]) -> Result<usize, Errno> {
            let count = std::cmp::min(self.available(), data.len());
            self.data.extend_from_slice(&data[..count]);
            Ok(count)
        }
    }

    #[fuchsia::test]
    fn test_queue_parmrk_eof_and_coalesce() {
        let mut queue = Queue::default();
        queue.push_line_byte(0xff, false);
        queue.push_line_byte(0xff, true);
        assert_eq!(queue.line_bytes(), &[0xff, 0xff, 0xff]);

        queue.flush_line_buffer();
        queue.flush_line_with_eof();
        assert_eq!(queue.readable_size(), 3);
        assert_eq!(queue.buffer_len(), 4);

        queue.push_line_byte(b'x', false);
        queue.coalesce_for_mode_change(false);
        assert!(queue.is_line_empty());
        assert_eq!(queue.readable_size(), 5);

        let mut out = VecOutputBuffer { data: Vec::new(), limit: 16 };
        let (written, drained) = queue.read_into(&mut out, false).unwrap();
        assert_eq!(written, 5);
        assert!(drained);
        assert_eq!(out.data, &[0xff, 0xff, 0xff, DISABLED_CHAR, b'x']);
    }

    #[fuchsia::test]
    fn test_partial_read_preserves_eof_delimiter() {
        let mut queue = Queue::default();
        queue.line_buffer_mut().extend_from_slice(b"abcd");
        queue.flush_line_with_eof();
        queue.line_buffer_mut().extend_from_slice(b"ef");
        queue.flush_line_buffer();

        // Even with `stop_after_packet == false`, a packet terminated by `VEOF` must not merge
        // with the following packet across reads, even after a partial read splits it.
        let mut first = VecOutputBuffer { data: Vec::new(), limit: 2 };
        let (n, _) = queue.read_into(&mut first, false).unwrap();
        assert_eq!(n, 2);
        assert_eq!(first.data, b"ab");

        let mut second = VecOutputBuffer { data: Vec::new(), limit: 16 };
        let (n, _) = queue.read_into(&mut second, false).unwrap();
        assert_eq!(n, 2);
        assert_eq!(second.data, b"cd");

        let mut third = VecOutputBuffer { data: Vec::new(), limit: 16 };
        let (n, _) = queue.read_into(&mut third, false).unwrap();
        assert_eq!(n, 2);
        assert_eq!(third.data, b"ef");
    }
}
