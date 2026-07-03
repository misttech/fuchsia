// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use cipher::generic_array::GenericArray;
use cipher::generic_array::typenum::consts::U16;
use cipher::{BlockBackend, BlockClosure, BlockSizeUser};
use static_assertions::assert_cfg;
use storage_ptr_slice::{MutPtrByteSlice, PtrByteSlice};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

// This assumes little-endianness which is likely to always be the case.
assert_cfg!(target_endian = "little");

#[derive(IntoBytes, KnownLayout, FromBytes, Immutable, Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct Tweak(pub u128);

impl Tweak {
    pub fn new(val: u128) -> Self {
        Self(val)
    }

    fn update(&mut self) {
        self.0 = (self.0 << 1) ^ ((self.0 as i128 >> 127) as u128 & 0x87);
    }
}

// To be used with encrypt|decrypt_with_backend.
pub struct XtsProcessor<'a, 'b> {
    tweak: Tweak,
    src: PtrByteSlice<'a>,
    dst: MutPtrByteSlice<'b>,
}

impl<'a, 'b> XtsProcessor<'a, 'b> {
    // `tweak` should be encrypted. `src` and `dst` must have the same length and be 16 byte
    // aligned.
    pub fn new(tweak: Tweak, src: PtrByteSlice<'a>, dst: MutPtrByteSlice<'b>) -> Self {
        assert_eq!(src.len(), dst.len(), "Source and destination lengths must match");
        assert!(src.as_ptr().cast::<u128>().is_aligned(), "src must be 16 byte aligned");
        assert!(dst.as_ptr().cast::<u128>().is_aligned(), "dst must be 16 byte aligned");
        Self { tweak, src, dst }
    }

    // Creates an XtsProcessor for in-place operation on a single buffer.
    pub fn new_in_place(tweak: Tweak, buf: MutPtrByteSlice<'a>) -> XtsProcessor<'a, 'a> {
        assert!(buf.as_ptr().cast::<u128>().is_aligned(), "buf must be 16 byte aligned");
        let len = buf.len();
        let ptr = buf.as_ptr_slice().as_ptr();
        // SAFETY: We are creating a PtrByteSlice that aliases with the MutPtrByteSlice.
        // This is safe because PtrByteSlice only allows read access, and we control the
        // execution in `call` to ensure we don't violate safety (we read a block, then write it,
        // so we don't have concurrent read/write on the same sub-block).
        let src = unsafe { PtrByteSlice::new(std::ptr::slice_from_raw_parts(ptr, len)) };
        XtsProcessor { tweak, src, dst: buf }
    }
}

impl BlockSizeUser for XtsProcessor<'_, '_> {
    type BlockSize = U16;
}

impl BlockClosure for XtsProcessor<'_, '_> {
    fn call<B: BlockBackend<BlockSize = Self::BlockSize>>(self, backend: &mut B) {
        let Self { mut tweak, src, mut dst } = self;
        let src_chunks = src.chunks::<u128>();
        let dst_chunks = dst.chunks_mut::<u128>();

        for (src_chunk, dst_chunk) in src_chunks.zip(dst_chunks) {
            let mut val = src_chunk.read();

            // XOR plaintext with tweak.
            val ^= tweak.0;

            backend.proc_block(GenericArray::from_mut_slice(val.as_mut_bytes()).into());

            // XOR ciphertext with tweak.
            val ^= tweak.0;

            dst_chunk.write(val);

            tweak.update();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cipher::generic_array::typenum::consts::U1;
    use cipher::inout::InOut;
    use cipher::{Block, ParBlocksSizeUser};

    struct MockCipher {
        recorded_blocks: Vec<u128>,
        key: u128,
    }

    impl MockCipher {
        fn new(key: u128) -> Self {
            Self { recorded_blocks: Vec::new(), key }
        }
    }

    impl BlockSizeUser for MockCipher {
        type BlockSize = U16;
    }

    impl ParBlocksSizeUser for MockCipher {
        type ParBlocksSize = U1;
    }

    impl BlockBackend for MockCipher {
        fn proc_block(&mut self, mut block: InOut<'_, '_, Block<Self>>) {
            // SAFETY: GenericArray<u8, U16> is 16 bytes.
            let mut val =
                unsafe { std::ptr::read_unaligned(block.get_in().as_ptr() as *const u128) };
            self.recorded_blocks.push(val);
            val ^= self.key;
            // SAFETY: GenericArray<u8, U16> is 16 bytes.
            unsafe {
                std::ptr::write_unaligned(block.get_out().as_mut_ptr() as *mut u128, val);
            }
        }
    }

    #[test]
    fn test_xts_out_of_place() {
        let mut plaintext = [0u8; 32];
        for (i, x) in plaintext.iter_mut().enumerate() {
            *x = i as u8;
        }
        let mut ciphertext = [0u8; 32];

        let src = PtrByteSlice::from(&plaintext[..]);
        let dst = MutPtrByteSlice::from(&mut ciphertext[..]);

        let tweak_val = 0x123456789abcdef0123456789abcdef0u128;
        let tweak = Tweak::new(tweak_val);
        let key = 0xffeeddccbbaa99887766554433221100u128;

        let processor = XtsProcessor::new(tweak, src, dst);
        let mut cipher = MockCipher::new(key);

        processor.call(&mut cipher);

        // Verify ciphertext.
        // Since our mock cipher is just XOR with key, the tweak should cancel out.
        // C = P ^ K.
        let expected_c0 = u128::from_le_bytes(plaintext[0..16].try_into().unwrap()) ^ key;
        let expected_c1 = u128::from_le_bytes(plaintext[16..32].try_into().unwrap()) ^ key;

        let actual_c0 = u128::from_le_bytes(ciphertext[0..16].try_into().unwrap());
        let actual_c1 = u128::from_le_bytes(ciphertext[16..32].try_into().unwrap());

        assert_eq!(actual_c0, expected_c0);
        assert_eq!(actual_c1, expected_c1);

        // Verify recorded blocks (should be P ^ T).
        assert_eq!(cipher.recorded_blocks.len(), 2);

        let p0 = u128::from_le_bytes(plaintext[0..16].try_into().unwrap());
        let p1 = u128::from_le_bytes(plaintext[16..32].try_into().unwrap());

        let mut t0 = tweak;
        assert_eq!(cipher.recorded_blocks[0], p0 ^ t0.0);
        t0.update();
        assert_eq!(cipher.recorded_blocks[1], p1 ^ t0.0);
    }

    #[test]
    fn test_xts_in_place() {
        let mut buf = [0u8; 32];
        for (i, x) in buf.iter_mut().enumerate() {
            *x = i as u8;
        }

        let tweak_val = 0x123456789abcdef0123456789abcdef0u128;
        let tweak = Tweak::new(tweak_val);
        let key = 0xffeeddccbbaa99887766554433221100u128;

        // Save original plaintext for verification.
        let p0 = u128::from_le_bytes(buf[0..16].try_into().unwrap());
        let p1 = u128::from_le_bytes(buf[16..32].try_into().unwrap());

        let slice = MutPtrByteSlice::from(&mut buf[..]);
        let processor = XtsProcessor::new_in_place(tweak, slice);
        let mut cipher = MockCipher::new(key);

        processor.call(&mut cipher);

        // Verify in-place ciphertext.
        let expected_c0 = p0 ^ key;
        let expected_c1 = p1 ^ key;

        let actual_c0 = u128::from_le_bytes(buf[0..16].try_into().unwrap());
        let actual_c1 = u128::from_le_bytes(buf[16..32].try_into().unwrap());

        assert_eq!(actual_c0, expected_c0);
        assert_eq!(actual_c1, expected_c1);

        // Verify recorded blocks.
        assert_eq!(cipher.recorded_blocks.len(), 2);
        let mut t0 = tweak;
        assert_eq!(cipher.recorded_blocks[0], p0 ^ t0.0);
        t0.update();
        assert_eq!(cipher.recorded_blocks[1], p1 ^ t0.0);
    }
}
