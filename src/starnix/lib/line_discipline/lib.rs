// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Terminal line discipline implementation (`N_TTY`) for Starnix pseudo-terminals.
//!
//! Organized into layered modules with strictly downward dependencies:
//! - [`termios`]: Default `termios2` configuration, control-character helpers, and flag queries.
//! - [`endpoint`]: Terminal side identity, open reference tracking, and `TIOCPKT` packet mode.
//! - [`queue`]: Unidirectional wait/line/read packet buffering and `InputBuffer` / `OutputBuffer`.
//! - [`output`]: `OPOST` formatting, cursor column tracking, XON/XOFF stop state, and echoes.
//! - [`input`]: Input flag translations, signal generation, and canonical line editing.
//! - [`LineDiscipline`] (this module): Coordinator uniting both endpoints and queues.

mod endpoint;
mod input;
mod output;
mod queue;
mod termios;

pub mod testing;

pub use endpoint::TerminalSide;
pub use input::PendingSignals;
pub use queue::{InputBuffer, OutputBuffer};
pub use termios::get_default_termios;

use endpoint::{EndpointRefs, PacketMode};
use input::InputDiscipline;
use output::OutputDiscipline;
use starnix_uapi::errors::Errno;
use starnix_uapi::vfs::FdEvents;
use starnix_uapi::{EXTPROC, ICANON, IXON, error, uapi};
use termios::TermiosExt;

/// Coordinates terminal state, `termios2` settings, packet mode, and bidirectional data flow
/// between the main and replica endpoints of a pseudo-terminal.
#[derive(Debug)]
pub struct LineDiscipline {
    /// `true` if the replica side of the terminal is locked (`TIOCSPTLCK`).
    pub locked: bool,

    /// Terminal window dimensions (`TIOCGWINSZ` / `TIOCSWINSZ`).
    pub window_size: uapi::winsize,

    /// Active terminal configuration (`TCGETS2` / `TCSETS2`).
    termios: uapi::termios2,

    /// Active open references to the main side of the terminal.
    main_refs: EndpointRefs,

    /// Active open references to the replica side of the terminal.
    replica_refs: EndpointRefs,

    /// `TIOCPKT` packet mode state for the main side of the terminal.
    packet_mode: PacketMode,

    /// Main-to-replica input processing, canonical line editing, and signal generation.
    input: InputDiscipline,

    /// Replica-to-main output processing (`OPOST`), flow control, and deferred echoing.
    output: OutputDiscipline,
}

impl Default for LineDiscipline {
    fn default() -> Self {
        Self {
            locked: true,
            window_size: uapi::winsize::default(),
            termios: get_default_termios(),
            main_refs: EndpointRefs::default(),
            replica_refs: EndpointRefs::default(),
            packet_mode: PacketMode::default(),
            input: InputDiscipline::default(),
            output: OutputDiscipline::default(),
        }
    }
}

impl LineDiscipline {
    /// Returns the active terminal configuration.
    pub fn termios(&self) -> &uapi::termios2 {
        &self.termios
    }

    /// Returns `true` if `TIOCPKT` packet mode is enabled on the main side.
    pub fn is_packet_mode_enabled(&self) -> bool {
        self.packet_mode.is_enabled()
    }

    /// Enables or disables `TIOCPKT` packet mode on the main side.
    pub fn set_packet_mode(&mut self, enabled: bool) {
        self.packet_mode.set_enabled(enabled);
    }

    /// Returns the number of readable payload bytes queued for `side` (`FIONREAD`).
    pub fn get_available_read_size(&self, side: TerminalSide) -> usize {
        match side {
            TerminalSide::Main => self.output.readable_size(),
            TerminalSide::Replica => self.input.readable_size(),
        }
    }

    /// Updates the terminal configuration, normalizing PTY control flags, recording `TIOCPKT`
    /// notifications, resuming stopped output if `IXON` was cleared, and coalescing buffered input
    /// when `ICANON` or `EXTPROC` changes.
    pub fn set_termios(&mut self, mut termios: uapi::termios2) -> PendingSignals {
        termios.normalize_pty_cflags();

        let old_ixon = self.termios.has_input_flags(IXON);
        let new_ixon = termios.has_input_flags(IXON);
        let old_flow = self.termios.is_standard_xon_xoff();
        let new_flow = termios.is_standard_xon_xoff();
        let extproc_active = ((self.termios.c_lflag | termios.c_lflag) & EXTPROC) != 0;
        let canon_or_extproc_changed =
            ((self.termios.c_lflag ^ termios.c_lflag) & (ICANON | EXTPROC)) != 0;

        self.termios = termios;

        if old_flow != new_flow {
            self.packet_mode.on_flow_control_changed(new_flow);
        }
        if extproc_active {
            self.packet_mode.record(uapi::TIOCPKT_IOCTL);
        }
        if old_ixon && !new_ixon {
            self.output.start(&self.termios, &mut self.packet_mode);
        }

        if canon_or_extproc_changed {
            self.input.on_mode_changed(&self.termios, &mut self.output, &mut self.packet_mode)
        } else {
            PendingSignals::new()
        }
    }

    /// Flushes queues relative to `side` according to `queue_selector` (`TCIFLUSH`, `TCOFLUSH`,
    /// or `TCIOFLUSH`).
    pub fn flush(&mut self, side: TerminalSide, queue_selector: u32) -> Result<(), Errno> {
        let flush_terminal_input = side == TerminalSide::Replica
            && matches!(queue_selector, uapi::TCIFLUSH | uapi::TCIOFLUSH);

        // Flush selectors are relative to the calling endpoint (`side`).
        let (inbound_queue, outbound_queue) = match side {
            TerminalSide::Main => (self.output.queue_mut(), self.input.queue_mut()),
            TerminalSide::Replica => (self.input.queue_mut(), self.output.queue_mut()),
        };

        // Input flushes discard all processed and unprocessed bytes in the inbound pipeline.
        // Output flushes discard only unprocessed `wait_buffers` bytes not yet delivered to the
        // peer's `read_queue`.
        let event = match queue_selector {
            uapi::TCIFLUSH => {
                inbound_queue.flush();
                uapi::TIOCPKT_FLUSHREAD
            }
            uapi::TCOFLUSH => {
                outbound_queue.flush_unprocessed();
                uapi::TIOCPKT_FLUSHWRITE
            }
            uapi::TCIOFLUSH => {
                inbound_queue.flush();
                outbound_queue.flush_unprocessed();
                uapi::TIOCPKT_FLUSHREAD | uapi::TIOCPKT_FLUSHWRITE
            }
            _ => return error!(EINVAL),
        };

        if flush_terminal_input {
            self.input.reset_state(&mut self.output);
        }
        if side == TerminalSide::Replica {
            self.packet_mode.record(event);
        }

        Ok(())
    }

    /// Opens a reference to the main side of the terminal.
    pub fn main_open(&mut self) {
        self.main_refs.open();
    }

    /// Closes a reference to the main side of the terminal, flushing queued data in both
    /// directions once the last main reference closes.
    pub fn main_close(&mut self) {
        self.main_refs.close();
        if self.is_main_closed() {
            let _ = self.flush(TerminalSide::Replica, uapi::TCIFLUSH);
            let _ = self.flush(TerminalSide::Main, uapi::TCIFLUSH);
        }
    }

    /// Returns `true` once all opened references to the main side have been closed.
    pub fn is_main_closed(&self) -> bool {
        self.main_refs.is_closed()
    }

    /// Queries poll readiness events for the main side of the terminal.
    pub fn main_query_events(&self) -> FdEvents {
        let mut events = self.output.read_readiness() | self.input.write_readiness();
        if self.packet_mode.has_pending_events() {
            events |= FdEvents::POLLIN | FdEvents::POLLPRI;
        }
        if self.is_replica_closed() {
            events |= FdEvents::POLLHUP;
        }
        events
    }

    /// Reads processed output (or `TIOCPKT` status packets) from the main side of the terminal.
    pub fn main_read(&mut self, data: &mut dyn OutputBuffer) -> Result<usize, Errno> {
        if let Some(event) = self.packet_mode.pending_events() {
            if data.available() == 0 {
                return Ok(0);
            }
            let written = data.write(&[event])?;
            if written > 0 {
                self.packet_mode.clear_pending_events();
            }
            return Ok(written);
        }
        if self.is_replica_closed() && self.output.readable_size() == 0 {
            return error!(EIO);
        }
        if self.packet_mode.is_enabled() {
            if self.output.readable_size() == 0 {
                return error!(EAGAIN);
            }
            if data.available() == 0 {
                return Ok(0);
            }
            let written = data.write(&[0])?;
            if written == 0 {
                return Ok(0);
            }
            return match self.output.read(data, &self.termios) {
                Ok(n) => Ok(n + 1),
                Err(_) => Ok(1),
            };
        }
        self.output.read(data, &self.termios)
    }

    /// Writes raw input bytes to the main side of the terminal, returning the byte count accepted
    /// and any signals generated during input processing.
    pub fn main_write(
        &mut self,
        data: &mut dyn InputBuffer,
    ) -> Result<(usize, PendingSignals), Errno> {
        self.input.write(data, &self.termios, &mut self.output, &mut self.packet_mode)
    }

    /// Opens a reference to the replica side of the terminal.
    pub fn replica_open(&mut self) {
        self.replica_refs.open();
    }

    /// Closes a reference to the replica side of the terminal.
    pub fn replica_close(&mut self) {
        self.replica_refs.close();
    }

    /// Returns `true` once all opened references to the replica side have been closed.
    pub fn is_replica_closed(&self) -> bool {
        self.replica_refs.is_closed()
    }

    /// Queries poll readiness events for the replica side of the terminal.
    pub fn replica_query_events(&self) -> FdEvents {
        if self.is_main_closed() {
            return FdEvents::POLLIN | FdEvents::POLLOUT | FdEvents::POLLERR | FdEvents::POLLHUP;
        }
        self.input.read_readiness() | self.output.write_readiness(&self.termios)
    }

    /// Reads processed input from the replica side of the terminal, returning the byte count read
    /// and any signals generated while draining waiting input buffers.
    pub fn replica_read(
        &mut self,
        data: &mut dyn OutputBuffer,
    ) -> Result<(usize, PendingSignals), Errno> {
        if self.is_main_closed() {
            return Ok((0, PendingSignals::new()));
        }
        self.input.read(data, &self.termios, &mut self.output, &mut self.packet_mode)
    }

    /// Writes output bytes from the replica side of the terminal.
    pub fn replica_write(&mut self, data: &mut dyn InputBuffer) -> Result<usize, Errno> {
        if self.is_main_closed() {
            return error!(EIO);
        }
        self.output.write(data, &self.termios)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::NON_CANON_MAX_BYTES;
    use starnix_uapi::signals::SIGINT;
    use starnix_uapi::{CREAD, CS8, CSIZE, ECHO, IUTF8, OPOST, PARENB, PARMRK, VEOL};

    struct TestBuffer {
        data: Vec<u8>,
    }

    impl InputBuffer for TestBuffer {
        fn available(&self) -> usize {
            self.data.len()
        }

        fn read_to_vec_exact(&mut self, size: usize) -> Result<Vec<u8>, Errno> {
            if size > self.data.len() {
                return error!(EAGAIN);
            }
            Ok(self.data.drain(0..size).collect())
        }
    }

    impl OutputBuffer for TestBuffer {
        fn available(&self) -> usize {
            usize::MAX
        }

        fn write(&mut self, data: &[u8]) -> Result<usize, Errno> {
            self.data.extend_from_slice(data);
            Ok(data.len())
        }
    }

    #[fuchsia::test]
    fn test_flush() {
        fn make_ld() -> LineDiscipline {
            let mut ld = LineDiscipline::default();
            ld.main_open();
            ld.replica_open();

            let mut termios = get_default_termios();
            termios.c_lflag &= !ECHO;
            termios.c_oflag &= !OPOST;
            let _ = ld.set_termios(termios);

            // Write some data from main to the replica (`input` queue).
            let mut input = TestBuffer { data: b"ping\n".to_vec() };
            let (written, signals) = ld.main_write(&mut input).unwrap();
            assert_eq!(written, 5);
            assert!(signals.signals().is_empty());

            // Write some data from replica to main (`output` queue).
            let mut output = TestBuffer { data: b"pong\n".to_vec() };
            let written = ld.replica_write(&mut output).unwrap();
            assert_eq!(written, 5);
            ld
        }

        let mut read_buf = TestBuffer { data: vec![] };

        // A TCIFLUSH from the main side should flush only the main's input (output_queue).
        let mut ld = make_ld();
        ld.flush(TerminalSide::Main, uapi::TCIFLUSH).unwrap();
        read_buf.data.clear();
        assert_eq!(error!(EAGAIN), ld.main_read(&mut read_buf));
        assert!(read_buf.data.is_empty());
        read_buf.data.clear();
        assert!(ld.replica_read(&mut read_buf).is_ok());
        assert_eq!(read_buf.data, b"ping\n");

        // A TCIFLUSH from the replica side should flush only the replica's input (input_queue).
        let mut ld = make_ld();
        ld.flush(TerminalSide::Replica, uapi::TCIFLUSH).unwrap();
        read_buf.data.clear();
        assert!(ld.main_read(&mut read_buf).is_ok());
        assert_eq!(read_buf.data, b"pong\n");
        read_buf.data.clear();
        assert_eq!(error!(EAGAIN), ld.replica_read(&mut read_buf));
        assert!(read_buf.data.is_empty());

        // A TCOFLUSH from the main side should do nothing (instantaneous transmission).
        let mut ld = make_ld();
        ld.flush(TerminalSide::Main, uapi::TCOFLUSH).unwrap();
        read_buf.data.clear();
        assert!(ld.main_read(&mut read_buf).is_ok());
        assert_eq!(read_buf.data, b"pong\n");
        read_buf.data.clear();
        assert!(ld.replica_read(&mut read_buf).is_ok());
        assert_eq!(read_buf.data, b"ping\n");

        // A TCOFLUSH from the replica side should do nothing.
        let mut ld = make_ld();
        ld.flush(TerminalSide::Replica, uapi::TCOFLUSH).unwrap();
        read_buf.data.clear();
        assert!(ld.main_read(&mut read_buf).is_ok());
        assert_eq!(read_buf.data, b"pong\n");
        read_buf.data.clear();
        assert!(ld.replica_read(&mut read_buf).is_ok());
        assert_eq!(read_buf.data, b"ping\n");

        // A TCIOFLUSH from main should flush only main's input (output_queue).
        let mut ld = make_ld();
        ld.flush(TerminalSide::Main, uapi::TCIOFLUSH).unwrap();
        read_buf.data.clear();
        assert_eq!(error!(EAGAIN), ld.main_read(&mut read_buf));
        read_buf.data.clear();
        assert!(ld.replica_read(&mut read_buf).is_ok());
        assert_eq!(read_buf.data, b"ping\n");

        // A TCIOFLUSH from replica should flush only replica's input (input_queue).
        let mut ld = make_ld();
        ld.flush(TerminalSide::Replica, uapi::TCIOFLUSH).unwrap();
        read_buf.data.clear();
        assert!(ld.main_read(&mut read_buf).is_ok());
        assert_eq!(read_buf.data, b"pong\n");
        read_buf.data.clear();
        assert_eq!(error!(EAGAIN), ld.replica_read(&mut read_buf));

        // A TCIFLUSH from replica while stopped via IXON should also clear pending_echoes.
        let mut ld = LineDiscipline::default();
        ld.main_open();
        ld.replica_open();
        let mut stop_and_text = TestBuffer { data: b"\x13stale\n".to_vec() };
        let _ = ld.main_write(&mut stop_and_text).unwrap();
        ld.flush(TerminalSide::Replica, uapi::TCIFLUSH).unwrap();
        let mut start_in = TestBuffer { data: b"\x11".to_vec() };
        let _ = ld.main_write(&mut start_in).unwrap();
        read_buf.data.clear();
        assert_eq!(error!(EAGAIN), ld.main_read(&mut read_buf));
    }

    #[fuchsia::test]
    fn test_canonical_max_line_length_with_erase_and_eof() {
        let mut ld = LineDiscipline::default();
        ld.main_open();
        ld.replica_open();

        // Write 4096 'A's (only 4095 fit), then backspace (erases 4095th 'A'), then 'B', then '\n'.
        let mut payload = vec![b'A'; 4096];
        payload.extend_from_slice(b"\x7fB\n");
        let mut input = TestBuffer { data: payload };
        let (written, signals) = ld.main_write(&mut input).unwrap();
        assert_eq!(written, 4099);
        assert!(signals.signals().is_empty());

        let mut replica_out = TestBuffer { data: vec![] };
        let (n, signals) = ld.replica_read(&mut replica_out).unwrap();
        assert_eq!(n, 4096);
        assert!(signals.signals().is_empty());
        assert_eq!(&replica_out.data[..4094], &[b'A'; 4094][..]);
        assert_eq!(&replica_out.data[4094..], b"B\n");

        let mut main_out = TestBuffer { data: vec![] };
        let _ = ld.main_read(&mut main_out).unwrap();
        assert_eq!(&main_out.data[..4095], &[b'A'; 4095][..]);
        assert_eq!(&main_out.data[4095..], b"\x08 \x08B\r\n");

        // Also test 4096 'C's terminated by VEOF (^D): replica should read 4095 'C's.
        let mut payload_eof = vec![b'C'; 4096];
        payload_eof.push(4);
        let mut input_eof = TestBuffer { data: payload_eof };
        let (written, _) = ld.main_write(&mut input_eof).unwrap();
        assert_eq!(written, 4097);

        replica_out.data.clear();
        let (n, signals) = ld.replica_read(&mut replica_out).unwrap();
        assert_eq!(n, 4095);
        assert!(signals.signals().is_empty());
        assert_eq!(replica_out.data, vec![b'C'; 4095]);
        replica_out.data.clear();
        assert_eq!(ld.replica_read(&mut replica_out), error!(EAGAIN));
    }

    #[fuchsia::test]
    fn test_pty_cflag_enforced() {
        let mut ld = LineDiscipline::default();
        let mut termios = get_default_termios();
        termios.c_cflag = uapi::CS5 | uapi::PARENB;
        let _ = ld.set_termios(termios);
        assert_eq!(ld.termios().c_cflag & CSIZE, CS8);
        assert_eq!(ld.termios().c_cflag & CREAD, CREAD);
        assert_eq!(ld.termios().c_cflag & PARENB, 0);
    }

    #[fuchsia::test]
    fn test_close_events_and_eio() {
        let mut ld = LineDiscipline::default();
        ld.main_open();
        ld.replica_open();

        ld.replica_close();
        assert!(ld.is_replica_closed());
        assert_eq!(ld.main_query_events(), FdEvents::POLLOUT | FdEvents::POLLHUP);
        let mut read_buf = TestBuffer { data: vec![] };
        assert_eq!(ld.main_read(&mut read_buf), error!(EIO));

        ld.replica_open();
        let mut main_in = TestBuffer { data: b"unread\n".to_vec() };
        let _ = ld.main_write(&mut main_in).unwrap();
        assert_eq!(ld.get_available_read_size(TerminalSide::Replica), 7);

        ld.main_close();
        assert!(ld.is_main_closed());
        assert_eq!(ld.get_available_read_size(TerminalSide::Replica), 0);
        assert_eq!(ld.get_available_read_size(TerminalSide::Main), 0);
        assert_eq!(
            ld.replica_query_events(),
            FdEvents::POLLIN | FdEvents::POLLOUT | FdEvents::POLLERR | FdEvents::POLLHUP
        );
        let (n, signals) = ld.replica_read(&mut read_buf).unwrap();
        assert_eq!(n, 0);
        assert!(signals.signals().is_empty());
        let mut write_buf = TestBuffer { data: b"x".to_vec() };
        assert_eq!(ld.replica_write(&mut write_buf), error!(EIO));
    }

    #[fuchsia::test]
    fn test_parmrk_capacity_limits() {
        let mut ld = LineDiscipline::default();
        ld.main_open();
        ld.replica_open();

        let mut termios = get_default_termios();
        termios.c_iflag |= PARMRK;
        termios.c_lflag &= !ECHO;
        termios.c_cc[VEOL as usize] = b';';
        let _ = ld.set_termios(termios);

        // Canonical mode: 4094 'A's + 0xff (would require 2 bytes = 4096, leaving no room for EOL)
        // must drop the 0xff so the subsequent '\n' still terminates a 4095-byte line.
        let mut payload = vec![b'A'; 4094];
        payload.extend_from_slice(&[0xff, b'\n']);
        let mut input = TestBuffer { data: payload };
        let (written, _) = ld.main_write(&mut input).unwrap();
        assert_eq!(written, 4096);

        let mut replica_out = TestBuffer { data: vec![] };
        let (n, _) = ld.replica_read(&mut replica_out).unwrap();
        assert_eq!(n, 4095);
        assert_eq!(&replica_out.data[..4094], &[b'A'; 4094][..]);
        assert_eq!(replica_out.data[4094], b'\n');

        // Canonical mode with VEOL = 0xff: 4095 'A's + 0xff (2 bytes > 4096) must not exceed
        // CANON_MAX_BYTES.
        termios.c_cc[VEOL as usize] = 0xff;
        let _ = ld.set_termios(termios);
        let mut payload_veol = vec![b'A'; 4095];
        payload_veol.extend_from_slice(&[0xff, b'\n']);
        let mut input_veol = TestBuffer { data: payload_veol };
        let _ = ld.main_write(&mut input_veol).unwrap();
        replica_out.data.clear();
        let (n, _) = ld.replica_read(&mut replica_out).unwrap();
        assert_eq!(n, 4096);
        assert_eq!(&replica_out.data[..4095], &[b'A'; 4095][..]);
        assert_eq!(replica_out.data[4095], b'\n');

        // Non-canonical mode: 4094 'A's + 0xff (2 bytes -> 4096 > NON_CANON_MAX_BYTES = 4095)
        // must stop before the 0xff so the first chunk is 4094 bytes and the second is [0xff, 0xff].
        termios.c_lflag &= !ICANON;
        let _ = ld.set_termios(termios);
        let mut payload_noncanon = vec![b'A'; 4094];
        payload_noncanon.push(0xff);
        let mut input_noncanon = TestBuffer { data: payload_noncanon };
        let (written, _) = ld.main_write(&mut input_noncanon).unwrap();
        assert_eq!(written, 4095);
        assert_eq!(ld.get_available_read_size(TerminalSide::Replica), 4094);
        replica_out.data.clear();
        let (n, _) = ld.replica_read(&mut replica_out).unwrap();
        assert_eq!(n, 4094);
        replica_out.data.clear();
        let (n, _) = ld.replica_read(&mut replica_out).unwrap();
        assert_eq!(n, 2);
        assert_eq!(replica_out.data, vec![0xff, 0xff]);
    }

    #[fuchsia::test]
    fn test_stopped_output_wait_buffer_not_drained_until_start() {
        let mut ld = LineDiscipline::default();
        ld.main_open();
        ld.replica_open();

        // Write 5000 'A's from replica: 4096 are transformed into output_queue.read_queue and 904
        // remain in output_queue.wait_buffers.
        let mut replica_in = TestBuffer { data: vec![b'A'; 5000] };
        let written = ld.replica_write(&mut replica_in).unwrap();
        assert_eq!(written, 5000);
        assert_eq!(ld.get_available_read_size(TerminalSide::Main), 4096);

        // Send VSTOP (^S) from main to stop output.
        let mut stop_in = TestBuffer { data: vec![0x13] };
        let (written, signals) = ld.main_write(&mut stop_in).unwrap();
        assert_eq!(written, 1);
        assert!(signals.signals().is_empty());

        // Reading from main consumes the 4096 already-transformed bytes, but must NOT drain the
        // remaining 904 bytes from output_queue.wait_buffers while stopped.
        let mut main_out = TestBuffer { data: vec![] };
        let n = ld.main_read(&mut main_out).unwrap();
        assert_eq!(n, 4096);
        assert_eq!(main_out.data.len(), 4096);
        assert_eq!(ld.get_available_read_size(TerminalSide::Main), 0);

        main_out.data.clear();
        assert_eq!(ld.main_read(&mut main_out), error!(EAGAIN));

        // Send VSTART (^Q) from main to resume output, which drains output_queue.wait_buffers.
        let mut start_in = TestBuffer { data: vec![0x11] };
        let (written, signals) = ld.main_write(&mut start_in).unwrap();
        assert_eq!(written, 1);
        assert!(signals.signals().is_empty());
        assert_eq!(ld.get_available_read_size(TerminalSide::Main), 904);

        let n = ld.main_read(&mut main_out).unwrap();
        assert_eq!(n, 904);
        assert_eq!(main_out.data, vec![b'A'; 904]);
    }

    #[fuchsia::test]
    fn test_veof_and_unread_queue_buffer_capacity() {
        let mut ld = LineDiscipline::default();
        ld.main_open();
        ld.replica_open();

        // Each empty VEOF (^D) occupies 1 byte in `buffer_len()` (matching `__DISABLED_CHAR` in
        // Linux `n_tty`), so at most `NON_CANON_MAX_BYTES` (4095) empty VEOF packets are drained
        // into `read_queue` while the 4096th remains in `wait_buffers`.
        let mut eofs = TestBuffer { data: vec![0x04; 4096] };
        let (written, _) = ld.main_write(&mut eofs).unwrap();
        assert_eq!(written, 4096);
        assert_eq!(ld.input.queue().read_packet_count(), NON_CANON_MAX_BYTES);
        assert_eq!(ld.input.queue().wait_buffer_len(), 1);

        // Reading 1 VEOF frees 1 slot and drains the 4096th VEOF from `wait_buffers`.
        let mut replica_out = TestBuffer { data: vec![] };
        let (n, _) = ld.replica_read(&mut replica_out).unwrap();
        assert_eq!(n, 0);
        assert_eq!(ld.input.queue().read_packet_count(), NON_CANON_MAX_BYTES);
        assert_eq!(ld.input.queue().wait_buffer_len(), 0);
    }

    #[fuchsia::test]
    fn test_iutf8_multibyte_at_canon_boundary() {
        let mut ld = LineDiscipline::default();
        ld.main_open();
        ld.replica_open();

        let mut termios = get_default_termios();
        termios.c_iflag |= IUTF8;
        termios.c_lflag &= !ECHO;
        let _ = ld.set_termios(termios);

        // 4094 'A's + "\xc3\xa9\n": Linux `n_tty` accepts the first byte 0xc3 (reaching 4095
        // non-EOL bytes), drops 0xa9, and accepts '\n' for a 4096-byte line.
        let mut payload = vec![b'A'; 4094];
        payload.extend_from_slice(b"\xc3\xa9\n");
        let mut input = TestBuffer { data: payload };
        let (written, _) = ld.main_write(&mut input).unwrap();
        assert_eq!(written, 4097);

        let mut replica_out = TestBuffer { data: vec![] };
        let (n, _) = ld.replica_read(&mut replica_out).unwrap();
        assert_eq!(n, 4096);
        assert_eq!(&replica_out.data[..4094], &[b'A'; 4094][..]);
        assert_eq!(&replica_out.data[4094..], b"\xc3\n");
    }

    #[fuchsia::test]
    fn test_signal_flush_preserves_subsequent_wait_buffers() {
        let mut ld = LineDiscipline::default();
        ld.main_open();
        ld.replica_open();

        // Fill `read_queue` to 4095 bytes (so `buffer_len() == NON_CANON_MAX_BYTES` and subsequent
        // writes queue in `wait_buffers`).
        let mut fill = vec![b'A'; 4094];
        fill.push(b'\n');
        let mut fill_in = TestBuffer { data: fill };
        let (written, _) = ld.main_write(&mut fill_in).unwrap();
        assert_eq!(written, 4095);

        // Queue two separate `main_write` buffers while full: one with VINTR (^C) and one with
        // "after\n".
        let mut sig_in = TestBuffer { data: b"\x03".to_vec() };
        let (written, signals) = ld.main_write(&mut sig_in).unwrap();
        assert_eq!(written, 1);
        assert!(signals.signals().is_empty());

        let mut after_in = TestBuffer { data: b"after\n".to_vec() };
        let (written, signals) = ld.main_write(&mut after_in).unwrap();
        assert_eq!(written, 6);
        assert!(signals.signals().is_empty());

        // Reading the initial 4095-byte line drains `wait_buffers`: `^C` raises SIGINT and flushes
        // any prior input, then `"after\n"` is processed into `read_queue`.
        let mut replica_out = TestBuffer { data: vec![] };
        let (n, signals) = ld.replica_read(&mut replica_out).unwrap();
        assert_eq!(n, 4095);
        assert_eq!(signals.signals(), &[SIGINT]);

        replica_out.data.clear();
        let (n, signals) = ld.replica_read(&mut replica_out).unwrap();
        assert_eq!(n, 6);
        assert!(signals.signals().is_empty());
        assert_eq!(replica_out.data, b"after\n");
    }
}
