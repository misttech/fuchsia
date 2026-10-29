// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::endpoint::PacketMode;
use crate::queue::{CANON_MAX_BYTES, InputBuffer, OutputBuffer, Queue};
use crate::termios::{
    BACKSPACE_CHAR, CONTROL_OFFSET, SPACES_PER_TAB, TermiosExt, is_ascii_control,
};
use starnix_uapi::errors::Errno;
use starnix_uapi::vfs::FdEvents;
use starnix_uapi::{
    ECHO, ECHOCTL, ECHOE, ECHOK, ECHOPRT, IXON, OCRNL, OLCUC, ONLCR, ONLRET, ONOCR, OPOST, TABDLY,
    VERASE, XTABS, error, uapi,
};

/// Deferred echo operation staged during input processing and rendered in `commit_echoes` when
/// terminal output is not stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EchoOp {
    /// Format and emit a single raw echo byte through `OPOST`.
    Byte(u8),
    /// Record the current cursor `column` as `canon_column` (start of a canonical input line).
    SetCanonColumn,
    /// Erase a tab character (`ECHOE`), computing the backspace count from `preceding_columns` and
    /// `canon_column` (when `!after_tab`).
    EraseTab { preceding_columns: usize, after_tab: bool },
}

/// Tracks the terminal cursor position across output formatting and canonical line editing.
#[derive(Debug, Default)]
struct CursorState {
    /// Current 0-indexed cursor column in the row.
    column: usize,
    /// Cursor column where the current canonical input line began.
    canon_column: usize,
}

impl CursorState {
    fn format_byte(&mut self, mut byte: u8, termios: &uapi::termios2, out: &mut Vec<u8>) {
        if !termios.has_output_flags(OPOST) {
            out.push(byte);
            return;
        }
        match byte {
            b'\n' => {
                if termios.has_output_flags(ONLRET) {
                    self.column = 0;
                }
                if termios.has_output_flags(ONLCR) {
                    self.canon_column = 0;
                    self.column = 0;
                    out.extend_from_slice(b"\r\n");
                    return;
                }
                self.canon_column = self.column;
            }
            b'\r' => {
                if termios.has_output_flags(ONOCR) && self.column == 0 {
                    return;
                }
                if termios.has_output_flags(OCRNL) {
                    byte = b'\n';
                    if termios.has_output_flags(ONLRET) {
                        self.canon_column = 0;
                        self.column = 0;
                    }
                } else {
                    self.canon_column = 0;
                    self.column = 0;
                }
            }
            b'\t' => {
                let spaces = SPACES_PER_TAB - (self.column % SPACES_PER_TAB);
                self.column += spaces;
                if termios.c_oflag & TABDLY == XTABS {
                    out.extend(std::iter::repeat_n(b' ', spaces));
                    return;
                }
            }
            BACKSPACE_CHAR => {
                self.column = self.column.saturating_sub(1);
            }
            _ => {
                if !is_ascii_control(byte) {
                    if termios.has_output_flags(OLCUC) {
                        byte.make_ascii_uppercase();
                    }
                    if !termios.is_utf8_continuation(byte) {
                        self.column += 1;
                    }
                }
            }
        }
        out.push(byte);
    }
}

/// Manages replica-to-main output processing (`OPOST`), XON/XOFF output flow control, cursor
/// column tracking, and deferred input echoing (`ECHO*`).
#[derive(Debug, Default)]
pub(crate) struct OutputDiscipline {
    /// Output queue carrying formatted replica output and committed echoes to the main endpoint.
    queue: Queue,

    /// `true` if output is currently stopped by `VSTOP` (`IXON`).
    stopped: bool,

    /// `true` if an `ECHOPRT` (`\.../`) erase sequence is currently open.
    erasing: bool,

    /// Terminal cursor column state.
    cursor: CursorState,

    /// Echo operations staged while processing input, flushed once output is unstopped.
    pending_echoes: Vec<EchoOp>,
}

impl OutputDiscipline {
    pub(crate) fn queue_mut(&mut self) -> &mut Queue {
        &mut self.queue
    }

    pub(crate) fn read_readiness(&self) -> FdEvents {
        self.queue.read_readiness()
    }

    pub(crate) fn write_readiness(&self, termios: &uapi::termios2) -> FdEvents {
        if self.is_stopped(termios) { FdEvents::empty() } else { self.queue.write_readiness() }
    }

    pub(crate) fn readable_size(&self) -> usize {
        self.queue.readable_size()
    }

    /// Returns `true` if output transmission is currently paused by `IXON` flow control.
    pub(crate) fn is_stopped(&self, termios: &uapi::termios2) -> bool {
        self.stopped && termios.has_input_flags(IXON)
    }

    /// Pauses output transmission (`VSTOP`), recording `TIOCPKT_STOP` if packet mode is active.
    pub(crate) fn stop(&mut self, packet_mode: &mut PacketMode) {
        if !self.stopped {
            self.stopped = true;
            packet_mode.on_stop();
        }
    }

    /// Resumes output transmission (`VSTART`, `IXANY`, `ISIG`, or clearing `IXON`), recording
    /// `TIOCPKT_START`, committing deferred echoes, and draining waiting replica output.
    pub(crate) fn start(&mut self, termios: &uapi::termios2, packet_mode: &mut PacketMode) {
        if self.stopped {
            self.stopped = false;
            packet_mode.on_start();
            self.commit_echoes(termios);
            self.drain_wait_buffers(termios);
        }
    }

    /// Resumes output if `stopped` and both `IXON` and `IXANY` are enabled.
    pub(crate) fn restart_if_ixany(
        &mut self,
        termios: &uapi::termios2,
        packet_mode: &mut PacketMode,
    ) {
        if self.stopped && termios.has_input_flags(IXON | starnix_uapi::IXANY) {
            self.start(termios, packet_mode);
        }
    }

    /// Resets transient `ECHOPRT` state without discarding queued echoes (used on mode switch).
    pub(crate) fn reset_erasing(&mut self) {
        self.erasing = false;
    }

    /// Clears transient `ECHOPRT` state and discards uncommitted echoes (used on input flush).
    pub(crate) fn reset_echo_state(&mut self) {
        self.erasing = false;
        self.pending_echoes.clear();
    }

    /// Writes bytes from the replica endpoint into the output queue and processes what fits.
    pub(crate) fn write(
        &mut self,
        data: &mut dyn InputBuffer,
        termios: &uapi::termios2,
    ) -> Result<usize, Errno> {
        if self.is_stopped(termios) {
            return error!(EAGAIN);
        }
        let read_from_userspace = self.queue.enqueue_wait_buffer(data)?;
        self.drain_wait_buffers(termios);
        Ok(read_from_userspace)
    }

    /// Reads processed output bytes into the main endpoint buffer, draining waiting output if
    /// unstopped.
    pub(crate) fn read(
        &mut self,
        data: &mut dyn OutputBuffer,
        termios: &uapi::termios2,
    ) -> Result<usize, Errno> {
        let (written, should_drain) = self.queue.read_into(data, false)?;
        if should_drain {
            self.drain_wait_buffers(termios);
        }
        Ok(written)
    }

    pub(crate) fn drain_wait_buffers(&mut self, termios: &uapi::termios2) {
        if self.is_stopped(termios) {
            return;
        }
        while let Some(wait_buffer) = self.queue.pop_wait_buffer() {
            let count = self.transform_output(&wait_buffer, termios);
            if count != wait_buffer.len() {
                self.queue.requeue_wait_buffer(wait_buffer[count..].to_vec());
                break;
            }
        }
    }

    fn transform_output(&mut self, buffer: &[u8], termios: &uapi::termios2) -> usize {
        if self.is_stopped(termios) {
            return 0;
        }

        if !termios.has_output_flags(OPOST) {
            let limit = CANON_MAX_BYTES.saturating_sub(self.queue.readable_size());
            if limit == 0 {
                return 0;
            }
            let to_write = std::cmp::min(limit, buffer.len());
            self.queue.push_ready_packet(buffer[..to_write].to_vec());
            return to_write;
        }

        let mut consumed = 0;
        for &byte in buffer {
            if self.queue.buffer_len() >= CANON_MAX_BYTES {
                break;
            }
            consumed += 1;
            self.cursor.format_byte(byte, termios, self.queue.line_buffer_mut());
        }
        self.queue.flush_line_buffer();
        consumed
    }

    pub(crate) fn echo_raw_byte(&mut self, byte: u8) {
        self.pending_echoes.push(EchoOp::Byte(byte));
    }

    pub(crate) fn set_canon_column(&mut self) {
        self.pending_echoes.push(EchoOp::SetCanonColumn);
    }

    pub(crate) fn echo_char(&mut self, byte: u8, termios: &uapi::termios2) {
        if termios.has_local_flags(ECHOCTL) && is_ascii_control(byte) && byte != b'\t' {
            self.echo_raw_byte(b'^');
            self.echo_raw_byte(byte ^ CONTROL_OFFSET);
        } else {
            self.echo_raw_byte(byte);
        }
    }

    pub(crate) fn finish_erasing(&mut self) {
        if self.erasing {
            self.echo_raw_byte(b'/');
            self.erasing = false;
        }
    }

    pub(crate) fn echo_vlnext(&mut self, termios: &uapi::termios2) {
        if termios.has_local_flags(ECHO) {
            self.finish_erasing();
            if termios.has_local_flags(ECHOCTL) {
                self.echo_raw_byte(b'^');
                self.echo_raw_byte(BACKSPACE_CHAR);
            }
        }
    }

    pub(crate) fn echo_reprint(&mut self, reprint_byte: u8, line: &[u8], termios: &uapi::termios2) {
        self.finish_erasing();
        self.echo_char(reprint_byte, termios);
        self.echo_raw_byte(b'\n');
        for &byte in line {
            self.echo_char(byte, termios);
        }
    }

    pub(crate) fn echo_kill_without_echoke(&mut self, kill_byte: u8, termios: &uapi::termios2) {
        self.finish_erasing();
        self.echo_char(kill_byte, termios);
        if termios.has_local_flags(ECHOK) {
            self.echo_raw_byte(b'\n');
        }
    }

    pub(crate) fn echo_erased_char(
        &mut self,
        first_byte: u8,
        erased_char: &[u8],
        remaining_line: &[u8],
        single_char_erase: bool,
        termios: &uapi::termios2,
    ) {
        if !termios.has_local_flags(ECHO) {
            return;
        }
        if termios.has_local_flags(ECHOPRT) {
            if !self.erasing {
                self.echo_raw_byte(b'\\');
                self.erasing = true;
            }
            for &byte in erased_char {
                self.echo_char(byte, termios);
            }
        } else if single_char_erase && !termios.has_local_flags(ECHOE) {
            self.echo_char(termios.c_cc[VERASE as usize], termios);
        } else if first_byte == b'\t' {
            let mut preceding_columns = 0;
            let mut after_tab = false;
            for &byte in remaining_line.iter().rev() {
                if byte == b'\t' {
                    after_tab = true;
                    break;
                }
                preceding_columns += termios.echoed_char_width(byte);
            }
            self.pending_echoes.push(EchoOp::EraseTab { preceding_columns, after_tab });
        } else {
            self.echo_rubout(termios.echoed_char_width(first_byte));
        }
    }

    fn echo_rubout(&mut self, columns: usize) {
        for _ in 0..columns {
            self.echo_raw_byte(BACKSPACE_CHAR);
            self.echo_raw_byte(b' ');
            self.echo_raw_byte(BACKSPACE_CHAR);
        }
    }

    /// Formats and flushes all staged `pending_echoes` into `queue` if output is not stopped.
    pub(crate) fn commit_echoes(&mut self, termios: &uapi::termios2) {
        if self.is_stopped(termios) || self.pending_echoes.is_empty() {
            return;
        }
        let ops = std::mem::take(&mut self.pending_echoes);
        let mut echoes = Vec::with_capacity(ops.len());
        for op in ops {
            match op {
                EchoOp::Byte(byte) => self.cursor.format_byte(byte, termios, &mut echoes),
                EchoOp::SetCanonColumn => {
                    self.cursor.canon_column = self.cursor.column;
                }
                EchoOp::EraseTab { mut preceding_columns, after_tab } => {
                    if !after_tab {
                        preceding_columns += self.cursor.canon_column;
                    }
                    let backspace_count = SPACES_PER_TAB - (preceding_columns % SPACES_PER_TAB);
                    for _ in 0..backspace_count {
                        self.cursor.format_byte(BACKSPACE_CHAR, termios, &mut echoes);
                    }
                }
            }
        }
        self.queue.push_ready_packet(echoes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::termios::get_default_termios;

    struct VecBuffer(Vec<u8>);

    impl InputBuffer for VecBuffer {
        fn available(&self) -> usize {
            self.0.len()
        }

        fn read_to_vec_exact(&mut self, size: usize) -> Result<Vec<u8>, Errno> {
            Ok(self.0.drain(0..size).collect())
        }
    }

    impl OutputBuffer for VecBuffer {
        fn available(&self) -> usize {
            usize::MAX
        }

        fn write(&mut self, data: &[u8]) -> Result<usize, Errno> {
            self.0.extend_from_slice(data);
            Ok(data.len())
        }
    }

    #[fuchsia::test]
    fn test_opost_tab_expansion_and_deferred_echoes() {
        let mut output = OutputDiscipline::default();
        let mut packet_mode = PacketMode::default();
        let mut termios = get_default_termios();
        termios.c_oflag |= XTABS;

        let mut replica_in = VecBuffer(b"ab\tc".to_vec());
        assert_eq!(output.write(&mut replica_in, &termios).unwrap(), 4);

        let mut main_out = VecBuffer(Vec::new());
        assert_eq!(output.read(&mut main_out, &termios).unwrap(), 9);
        assert_eq!(main_out.0, b"ab      c");

        // Echoes staged while stopped remain deferred until `start()`.
        output.stop(&mut packet_mode);
        output.echo_char(0x03, &termios);
        output.commit_echoes(&termios);
        assert_eq!(output.readable_size(), 0);

        output.start(&termios, &mut packet_mode);
        main_out.0.clear();
        assert_eq!(output.read(&mut main_out, &termios).unwrap(), 2);
        assert_eq!(main_out.0, b"^C");
    }
}
