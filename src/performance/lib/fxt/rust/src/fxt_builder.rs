// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use thiserror::Error;
use zerocopy::IntoBytes;

#[derive(Clone)]
pub struct FxtBuilder<H> {
    header: H,
    buf: Vec<u64>,
}

#[derive(Error, Debug)]
pub enum SerializeError {
    #[error("Encountered Empty StringRefs when serializing argument's name")]
    MissingArgName,
}

impl<H: crate::header::TraceHeader> FxtBuilder<H> {
    /// Start a new fxt record with a typed header. The header should be completely configured for
    /// the corresponding record except for its size in words which will be updated by the builder.
    pub fn new(mut header: H) -> Self {
        // Make space for our header word before anything gets added.
        let buf = vec![0];

        // Set an initial size, we'll update as we go.
        header.set_size_words(1);

        Self { header, buf }
    }

    pub fn atom(mut self, atom: impl AsRef<[u8]>) -> Self {
        let atom = atom.as_ref();
        let start = self.buf.len() * 8;
        let size_words = self.buf.len() + atom.len().div_ceil(8);
        assert!(
            size_words <= crate::MAX_RECORD_SIZE_WORDS,
            "maximum record size is {} words",
            crate::MAX_RECORD_SIZE_WORDS
        );
        self.buf.resize(size_words, 0);
        self.buf.as_mut_slice().as_mut_bytes()[start..start + atom.len()].copy_from_slice(atom);
        self.header.set_size_words(
            size_words.try_into().expect("trace records size in words must fit in a u16"),
        );
        self
    }

    /// Append an already serialized record without copying it to a byte vector.
    pub fn atom_words(self, words: impl AsRef<[u64]>) -> Self {
        self.atom(words.as_ref().as_bytes())
    }

    /// Return the words of a possibly-valid FXT record with the header in place.
    pub fn build(mut self) -> Vec<u64> {
        self.buf[0] = u64::from_ne_bytes(self.header.to_le_bytes());
        self.buf
    }
}

impl<H: std::fmt::Debug> std::fmt::Debug for FxtBuilder<H> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Print in word-aligned chunks, exclude the zeroes we keep for the header.
        let chunks = self.buf.iter().skip(1).collect::<Vec<_>>();
        f.debug_struct("FxtBuilder").field("header", &self.header).field("buf", &chunks).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy)]
    struct TestHeader(u64);

    impl crate::header::TraceHeader for TestHeader {
        fn set_size_words(&mut self, n: u16) {
            self.0 = n.into();
        }

        fn to_le_bytes(&self) -> [u8; 8] {
            self.0.to_le_bytes()
        }
    }

    #[test]
    fn builder_preserves_record_bytes_and_allocation() {
        let first_atom = [1, 2, 3];
        let second_atom = [4, 5, 6, 7, 8, 9, 10, 11, 12];
        let builder = FxtBuilder::new(TestHeader(0)).atom(first_atom).atom(second_atom);
        let backing = builder.buf.as_ptr();
        let words: Vec<u64> = builder.build();
        // Building must keep the aligned allocation rather than copy the whole record.
        assert_eq!(words.as_ptr(), backing);
        assert_eq!((words.as_ptr() as usize) % std::mem::align_of::<u64>(), 0);
        let bytes = words.as_slice().as_bytes();
        let expected = [
            4, 0, 0, 0, 0, 0, 0, 0, // header: four words
            1, 2, 3, 0, 0, 0, 0, 0, // first atom and padding
            4, 5, 6, 7, 8, 9, 10, 11, 12, 0, 0, 0, 0, 0, 0, 0, // second atom and padding
        ];
        assert_eq!(bytes, expected);
    }

    #[test]
    fn serialized_words_can_be_parsed_as_session_bytes() {
        let magic = [0x10, 0x00, 0x04, 0x46, 0x78, 0x54, 0x16, 0x00];
        let mut header = crate::event::EventHeader::empty();
        header.set_thread_ref(1);
        let event = FxtBuilder::new(header).atom(42u64.to_le_bytes()).build();
        let mut session = magic.to_vec();
        session.extend_from_slice(event.as_bytes());

        let mut parser = crate::SessionParser::new(std::io::Cursor::new(session));
        assert!(matches!(parser.next(), Some(Ok(crate::TraceRecord::Event(_)))));
    }
}
