// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::endpoint::PacketMode;
use crate::output::OutputDiscipline;
use crate::queue::{CANON_MAX_BYTES, InputBuffer, NON_CANON_MAX_BYTES, OutputBuffer, Queue};
use crate::termios::{TermiosExt, is_word_erase_alnum};
use starnix_uapi::errors::Errno;
use starnix_uapi::signals::Signal;
use starnix_uapi::vfs::FdEvents;
use starnix_uapi::{
    ECHO, ECHOE, ECHOK, ECHOKE, ECHONL, ICRNL, IEXTEN, IGNCR, INLCR, ISIG, ISTRIP, IUCLC, IXON,
    NOFLSH, PARMRK, VLNEXT, VREPRINT, VSTART, VSTOP, uapi,
};

/// Accumulates signals generated while processing terminal input.
#[derive(Debug, Default, PartialEq, Eq)]
#[must_use]
pub struct PendingSignals {
    signals: Vec<Signal>,
}

impl PendingSignals {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `signal` to the list of signals to deliver to the foreground process group.
    pub(crate) fn add(&mut self, signal: Signal) {
        self.signals.push(signal);
    }

    /// Appends all pending signals in `other` to `self`.
    pub fn append(&mut self, mut other: Self) {
        self.signals.append(&mut other.signals);
    }

    /// Returns a slice of the pending signals.
    pub fn signals(&self) -> &[Signal] {
        &self.signals
    }
}

/// Kind of canonical erase operation triggered on the current input line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EraseType {
    /// Erase a single character (`VERASE`, typically Backspace/DEL).
    Character,
    /// Erase a word (`VWERASE`, typically `^W`).
    Word,
    /// Erase the entire line (`VKILL`, typically `^U`).
    Line,
}

/// Outcome of inspecting a byte for canonical-mode control handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CanonicalOutcome {
    /// Not a canonical control byte; proceed to regular character insertion.
    NotSpecial,
    /// Consumed as a canonical control or delimiter byte.
    Consumed,
    /// Input queue is full (`VEOL`/`VEOL2` with unread packets); stop consuming wait buffer.
    BufferFull,
}

/// Manages main-to-replica input processing, signal generation, flow-control triggers, and
/// canonical line editing.
#[derive(Debug, Default)]
pub(crate) struct InputDiscipline {
    /// Input queue carrying processed lines or raw chunks to the replica endpoint.
    queue: Queue,

    /// `true` if the next input byte is escaped by literal-next (`VLNEXT`).
    lnext: bool,
}

impl InputDiscipline {
    #[cfg(test)]
    pub(crate) fn queue(&self) -> &Queue {
        &self.queue
    }

    pub(crate) fn queue_mut(&mut self) -> &mut Queue {
        &mut self.queue
    }

    pub(crate) fn read_readiness(&self) -> FdEvents {
        self.queue.read_readiness()
    }

    pub(crate) fn write_readiness(&self) -> FdEvents {
        self.queue.write_readiness()
    }

    pub(crate) fn readable_size(&self) -> usize {
        self.queue.readable_size()
    }

    /// Resets transient input editing and deferred echo state (used on replica input flush).
    pub(crate) fn reset_state(&mut self, output: &mut OutputDiscipline) {
        self.lnext = false;
        output.reset_echo_state();
    }

    /// Coalesces buffered input packets and drains waiting input when `ICANON` or `EXTPROC`
    /// changes.
    pub(crate) fn on_mode_changed(
        &mut self,
        termios: &uapi::termios2,
        output: &mut OutputDiscipline,
        packet_mode: &mut PacketMode,
    ) -> PendingSignals {
        self.lnext = false;
        output.reset_erasing();
        self.queue.coalesce_for_mode_change(termios.is_canon_enabled());
        self.drain_wait_buffers(termios, output, packet_mode)
    }

    /// Writes raw bytes from the main endpoint into the input wait buffer and processes what fits.
    pub(crate) fn write(
        &mut self,
        data: &mut dyn InputBuffer,
        termios: &uapi::termios2,
        output: &mut OutputDiscipline,
        packet_mode: &mut PacketMode,
    ) -> Result<(usize, PendingSignals), Errno> {
        let read_from_userspace = self.queue.enqueue_wait_buffer(data)?;
        let signals = self.drain_wait_buffers(termios, output, packet_mode);
        Ok((read_from_userspace, signals))
    }

    /// Reads processed input into the replica endpoint buffer and drains waiting input.
    pub(crate) fn read(
        &mut self,
        data: &mut dyn OutputBuffer,
        termios: &uapi::termios2,
        output: &mut OutputDiscipline,
        packet_mode: &mut PacketMode,
    ) -> Result<(usize, PendingSignals), Errno> {
        let (written, should_drain) = self.queue.read_into(data, termios.is_canon_enabled())?;
        let signals = if should_drain {
            self.drain_wait_buffers(termios, output, packet_mode)
        } else {
            PendingSignals::new()
        };
        Ok((written, signals))
    }

    fn drain_wait_buffers(
        &mut self,
        termios: &uapi::termios2,
        output: &mut OutputDiscipline,
        packet_mode: &mut PacketMode,
    ) -> PendingSignals {
        let mut signals_to_return = PendingSignals::new();
        while let Some(wait_buffer) = self.queue.pop_wait_buffer() {
            let (count, signals) = self.transform_input(&wait_buffer, termios, output, packet_mode);
            signals_to_return.append(signals);
            if count != wait_buffer.len() {
                self.queue.requeue_wait_buffer(wait_buffer[count..].to_vec());
                break;
            }
        }
        signals_to_return
    }

    fn transform_input(
        &mut self,
        original_buffer: &[u8],
        termios: &uapi::termios2,
        output: &mut OutputDiscipline,
        packet_mode: &mut PacketMode,
    ) -> (usize, PendingSignals) {
        let mut buffer = original_buffer;
        let mut consumed = 0;
        let mut signals = PendingSignals::new();

        while !buffer.is_empty()
            && self.queue.buffer_len()
                < if termios.is_canon_enabled() && !self.queue.has_unread_packets() {
                    CANON_MAX_BYTES
                } else {
                    NON_CANON_MAX_BYTES
                }
        {
            let mut byte = buffer[0];

            if termios.has_input_flags(ISTRIP) {
                byte &= 0x7f;
            }
            if termios.has_input_flags(IUCLC) && termios.has_local_flags(IEXTEN) {
                byte.make_ascii_lowercase();
            }

            // Step 1: Handle literal-next (`VLNEXT`) active from the previous character.
            if self.lnext {
                self.lnext = false;
                output.restart_if_ixany(termios, packet_mode);
                if !self.try_append_char(byte, false, termios, output) {
                    break;
                }
                buffer = &buffer[1..];
                consumed += 1;
                continue;
            }

            // Step 2: `EXTPROC` bypasses all special character and echo processing.
            if termios.is_extproc_enabled() {
                if self.queue.buffer_len() + 1 > NON_CANON_MAX_BYTES {
                    break;
                }
                self.queue.push_line_byte(byte, false);
                buffer = &buffer[1..];
                consumed += 1;
                continue;
            }

            // Step 3: `IXON` flow control (`VSTART` / `VSTOP`) takes precedence over `ISIG` and
            // `ICANON`.
            if termios.has_input_flags(IXON) {
                if termios.is_cc(VSTART, byte) {
                    output.start(termios, packet_mode);
                    buffer = &buffer[1..];
                    consumed += 1;
                    continue;
                }
                if termios.is_cc(VSTOP, byte) {
                    output.stop(packet_mode);
                    buffer = &buffer[1..];
                    consumed += 1;
                    continue;
                }
            }

            // Step 4: `ISIG` signal characters (`VINTR`, `VQUIT`, `VSUSP`).
            if self.handle_signal_char(byte, termios, output, packet_mode, &mut signals) {
                buffer = &buffer[1..];
                consumed += 1;
                continue;
            }

            // Step 5: `IXANY` restarts output on any character not consumed above.
            output.restart_if_ixany(termios, packet_mode);

            // Step 6: CR/NL input translations (`IGNCR`, `ICRNL`, `INLCR`).
            match byte {
                b'\r' => {
                    if termios.has_input_flags(IGNCR) {
                        buffer = &buffer[1..];
                        consumed += 1;
                        continue;
                    }
                    if termios.has_input_flags(ICRNL) {
                        byte = b'\n';
                    }
                }
                b'\n' if termios.has_input_flags(INLCR) => {
                    byte = b'\r';
                }
                _ => {}
            }

            // Step 7: Canonical mode editing and line delimiters.
            if termios.is_canon_enabled() {
                match self.handle_canonical_char(byte, termios, output) {
                    CanonicalOutcome::Consumed => {
                        buffer = &buffer[1..];
                        consumed += 1;
                        continue;
                    }
                    CanonicalOutcome::BufferFull => break,
                    CanonicalOutcome::NotSpecial => {}
                }
            }

            // Step 8: Regular character (or `\n` in non-canonical mode).
            if !self.try_append_char(byte, false, termios, output) {
                break;
            }
            buffer = &buffer[1..];
            consumed += 1;
        }

        output.commit_echoes(termios);

        // In non-canonical mode (or `EXTPROC`), everything is immediately readable.
        if !termios.is_canon_enabled() {
            self.queue.flush_line_buffer();
        }

        (consumed, signals)
    }

    fn handle_signal_char(
        &mut self,
        byte: u8,
        termios: &uapi::termios2,
        output: &mut OutputDiscipline,
        packet_mode: &mut PacketMode,
        signals: &mut PendingSignals,
    ) -> bool {
        if !termios.has_local_flags(ISIG) {
            return false;
        }
        let Some(signal) = termios.signal(byte) else {
            return false;
        };

        signals.add(signal);
        if !termios.has_local_flags(NOFLSH) {
            self.queue.flush_buffers();
            self.reset_state(output);
            output.queue_mut().flush();
            packet_mode.record(uapi::TIOCPKT_FLUSHREAD | uapi::TIOCPKT_FLUSHWRITE);
        }
        if termios.has_input_flags(IXON) {
            output.start(termios, packet_mode);
        }
        if termios.has_local_flags(ECHO) {
            output.echo_char(byte, termios);
            output.commit_echoes(termios);
        }
        true
    }

    fn handle_canonical_char(
        &mut self,
        byte: u8,
        termios: &uapi::termios2,
        output: &mut OutputDiscipline,
    ) -> CanonicalOutcome {
        if termios.is_erase(byte) || termios.is_kill(byte) || termios.is_werase(byte) {
            self.erase_input(byte, termios, output);
            return CanonicalOutcome::Consumed;
        }

        if termios.is_iexten_cc(VLNEXT, byte) {
            self.lnext = true;
            output.echo_vlnext(termios);
            return CanonicalOutcome::Consumed;
        }

        if termios.is_iexten_cc(VREPRINT, byte) && termios.has_local_flags(ECHO) {
            output.echo_reprint(byte, self.queue.line_bytes(), termios);
            return CanonicalOutcome::Consumed;
        }

        if byte == b'\n' {
            if termios.has_local_flags(ECHO) || termios.has_local_flags(ECHONL) {
                output.echo_raw_byte(b'\n');
            }
            self.queue.push_line_byte(b'\n', false);
            self.queue.flush_line_buffer();
            return CanonicalOutcome::Consumed;
        }

        if termios.is_eof(byte) {
            self.queue.flush_line_with_eof();
            return CanonicalOutcome::Consumed;
        }

        if termios.is_eol(byte) {
            return if self.try_append_char(byte, true, termios, output) {
                CanonicalOutcome::Consumed
            } else {
                CanonicalOutcome::BufferFull
            };
        }

        CanonicalOutcome::NotSpecial
    }

    /// Appends `byte` to `line_buffer` (doubling `0xff` under `PARMRK`) and stages echoes.
    ///
    /// Returns `false` if the input buffer cannot accept `byte` right now and the caller should
    /// leave `byte` in `wait_buffers`.
    fn try_append_char(
        &mut self,
        byte: u8,
        is_eol: bool,
        termios: &uapi::termios2,
        output: &mut OutputDiscipline,
    ) -> bool {
        let parmrk_double = byte == 0xff && termios.has_input_flags(PARMRK);
        let pushed_len = if parmrk_double { 2 } else { 1 };
        let max_bytes = if is_eol && !self.queue.has_unread_packets() {
            CANON_MAX_BYTES
        } else {
            NON_CANON_MAX_BYTES
        };

        if self.queue.buffer_len() + pushed_len > max_bytes {
            // In canonical mode with an empty `read_queue`, bytes beyond the 4095-byte non-EOL
            // limit are consumed and discarded while waiting for a delimiter or erase.
            return termios.is_canon_enabled() && !self.queue.has_unread_packets();
        }

        if termios.has_local_flags(ECHO) {
            // Matches Linux `n_tty_receive_char_special`: `EOL_CHAR` / `EOL2_CHAR` (like `\n`)
            // does not close an open `ECHOPRT` `\.../` sequence; the next regular character does.
            if !is_eol {
                output.finish_erasing();
            }
            if self.queue.is_line_empty() {
                output.set_canon_column();
            }
            output.echo_char(byte, termios);
        }

        self.queue.push_line_byte(byte, parmrk_double);
        if is_eol {
            self.queue.flush_line_buffer();
        }
        true
    }

    fn erase_input(
        &mut self,
        erase_byte: u8,
        termios: &uapi::termios2,
        output: &mut OutputDiscipline,
    ) {
        if self.queue.is_line_empty() {
            return;
        }

        let erase_type = if termios.is_erase(erase_byte) {
            EraseType::Character
        } else if termios.is_werase(erase_byte) {
            EraseType::Word
        } else {
            if !termios.has_local_flags(ECHO) {
                self.queue.clear_line_buffer();
                return;
            }
            if !termios.has_local_flags(ECHOK | ECHOKE | ECHOE) {
                self.queue.clear_line_buffer();
                output.echo_kill_without_echoke(erase_byte, termios);
                return;
            }
            EraseType::Line
        };

        let mut seen_alnums = 0;
        while !self.queue.is_line_empty() {
            let line = self.queue.line_bytes();
            let mut pos = line.len();
            while pos > 0 {
                pos -= 1;
                if !termios.is_utf8_continuation(line[pos]) {
                    break;
                }
            }
            let first_byte = line[pos];
            if termios.is_utf8_continuation(first_byte) {
                // Do not partially erase an incomplete or stray UTF-8 continuation sequence.
                break;
            }
            if erase_type == EraseType::Word {
                if is_word_erase_alnum(first_byte) {
                    seen_alnums += 1;
                } else if seen_alnums > 0 {
                    break;
                }
            }

            let erased_char = self.queue.drain_line_from(pos);
            output.echo_erased_char(
                first_byte,
                &erased_char,
                self.queue.line_bytes(),
                erase_type == EraseType::Character,
                termios,
            );
            if erase_type == EraseType::Character {
                break;
            }
        }

        if self.queue.is_line_empty() && termios.has_local_flags(ECHO) {
            output.finish_erasing();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::termios::get_default_termios;
    use starnix_uapi::signals::SIGINT;

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
    fn test_canonical_word_erase_and_lnext() {
        let mut input = InputDiscipline::default();
        let mut output = OutputDiscipline::default();
        let mut packet_mode = PacketMode::default();
        let termios = get_default_termios();

        // "foo bar" -> ^W erases "bar" -> ^V^C inserts literal 0x03 -> "\n" completes line.
        let mut raw = VecBuffer(b"foo bar\x17\x16\x03\n".to_vec());
        let (written, signals) =
            input.write(&mut raw, &termios, &mut output, &mut packet_mode).unwrap();
        assert_eq!(written, 11);
        assert!(signals.signals().is_empty());

        let mut replica_out = VecBuffer(Vec::new());
        let (n, signals) =
            input.read(&mut replica_out, &termios, &mut output, &mut packet_mode).unwrap();
        assert_eq!(n, 6);
        assert!(signals.signals().is_empty());
        assert_eq!(replica_out.0, b"foo \x03\n");
    }

    #[fuchsia::test]
    fn test_signal_generation_and_packet_mode_flush() {
        let mut input = InputDiscipline::default();
        let mut output = OutputDiscipline::default();
        let mut packet_mode = PacketMode::default();
        packet_mode.set_enabled(true);
        let termios = get_default_termios();

        let mut raw = VecBuffer(b"partial\x03".to_vec());
        let (written, signals) =
            input.write(&mut raw, &termios, &mut output, &mut packet_mode).unwrap();
        assert_eq!(written, 8);
        assert_eq!(signals.signals(), &[SIGINT]);
        assert!(input.queue().is_line_empty());
        assert_eq!(
            packet_mode.pending_events(),
            Some((uapi::TIOCPKT_FLUSHREAD | uapi::TIOCPKT_FLUSHWRITE) as u8)
        );
    }
}
