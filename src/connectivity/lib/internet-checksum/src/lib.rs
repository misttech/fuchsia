// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! RFC 1071 "internet checksum" computation.
//!
//! This crate implements the "internet checksum" defined in [RFC 1071] and
//! updated in [RFC 1141] and [RFC 1624], which is used by many different
//! protocols' packet formats. The checksum operates by computing the 1s
//! complement of the 1s complement sum of successive 16-bit words of the input.
//!
//! [RFC 1071]: https://tools.ietf.org/html/rfc1071
//! [RFC 1141]: https://tools.ietf.org/html/rfc1141
//! [RFC 1624]: https://tools.ietf.org/html/rfc1624

// Optimizations applied:
//
// 0. Byteorder independence: as described in RFC 1071 section 2.(B)
//    The sum of 16-bit integers can be computed in either byte order,
//    so this actually saves us from the unnecessary byte swapping on
//    an LE machine. As perfed on a gLinux workstation, that swapping
//    can account for ~20% of the runtime.
//
// 1. Widen the accumulator: doing so enables us to process a bigger
//    chunk of data once at a time, achieving some kind of poor man's
//    SIMD. Currently a u128 counter is used on x86-64 and a u64 is
//    used conservatively on other architectures.
//
// 2. Process more at a time: the old implementation uses a u32 accumulator
//    but it only adds one u16 each time to implement deferred carry. In
//    the current implementation we are processing a u128 once at a time
//    on x86-64, which is 8 u16's. On other platforms, we are processing
//    a u64 at a time, which is 4 u16's.
//
// 3. Induce the compiler to produce `adc` instruction: this is a very
//    useful instruction to implement 1's complement addition and available
//    on both x86 and ARM. The functions `adc_uXX` are for this use.
//
// 4. Eliminate branching as much as possible: the old implementation has
//    if statements for detecting overflow of the u32 accumulator which
//    is not needed when we can access the carry flag with `adc`. The old
//    `normalize` function used to have a while loop to fold the u32,
//    however, we can unroll that loop because we know ahead of time how
//    much additions we need.
//
// 5. In the loop of `add_bytes`, the `adc_u64` is not used, instead,
//    the `overflowing_add` is directly used. `adc_u64`'s carry flag
//    comes from the current number being added while the slightly
//    convoluted version in `add_bytes`, adding each number depends on
//    the carry flag of the previous computation. I checked under release
//    mode this issues 3 instructions instead of 4 for x86 and it should
//    theoretically be beneficial, however, measurement showed me that it
//    helps only a little. So this trick is not used for `update`.
//
// Results:
//
// Micro-benchmarks are run on an x86-64 gLinux workstation. In summary,
// compared the baseline 0 which is prior to the byteorder independence
// patch, there is a ~4x speedup.
//
// TODO: run this optimization on other platforms. I would expect
// the situation on ARM a bit different because I am not sure
// how much penalty there will be for misaligned read on ARM, or
// whether it is even supported (On x86 there is generally no
// penalty for misaligned read). If there will be penalties, we
// should consider alignment as an optimization opportunity on ARM.

// TODO(joshlf): Right-justify the columns above

// TODO(joshlf):
// - Investigate optimizations proposed in RFC 1071 Section 2. The most
//   promising on modern hardware is probably (C) Parallel Summation, although
//   that needs to be balanced against (1) Deferred Carries. Benchmarks will
//   need to be performed to determine which is faster in practice, and under
//   what scenarios.

/// Compute the checksum of "bytes".
///
/// `checksum(bytes)` is shorthand for:
///
/// ```rust
/// # use internet_checksum::Checksum;
/// # let bytes = &[];
/// # let _ = {
/// let mut c = Checksum::new();
/// c.add_bytes(bytes);
/// c.checksum()
/// # };
/// ```
#[inline]
pub fn checksum(bytes: &[u8]) -> [u8; 2] {
    let mut c = Checksum::new();
    c.add_bytes(bytes);
    c.checksum()
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
type Accumulator = u128;
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
type Accumulator = u64;

/// Updates bytes in an existing checksum.
///
/// `update` updates a checksum to reflect that the already-checksummed bytes
/// `old` have been updated to contain the values in `new`. It implements the
/// algorithm described in Equation 3 in [RFC 1624]. The first byte must be at
/// an even number offset in the original input. If an odd number offset byte
/// needs to be updated, the caller should simply include the preceding byte as
/// well. If an odd number of bytes is given, it is assumed that these are the
/// last bytes of the input. If an odd number of bytes in the middle of the
/// input needs to be updated, the preceding or following byte of the input
/// should be added to make an even number of bytes.
///
/// # Panics
///
/// `update` panics if `old.len() != new.len()`.
///
/// [RFC 1624]: https://tools.ietf.org/html/rfc1624
#[inline]
pub fn update(checksum: [u8; 2], old: &[u8], new: &[u8]) -> [u8; 2] {
    assert_eq!(old.len(), new.len());
    // We compute on the sum, not the one's complement of the sum. checksum
    // is the one's complement of the sum, so we need to get back to the
    // sum. Thus, we negate checksum.
    // HC' = ~HC
    let mut sum = !u16::from_ne_bytes(checksum) as Accumulator;

    // Let's reuse `Checksum::add_bytes` to update our checksum
    // so that we can get the speedup for free. Using
    // [RFC 1071 Eqn. 3], we can efficiently update our new checksum.
    let mut c1 = Checksum::new();
    let mut c2 = Checksum::new();
    c1.add_bytes(old);
    c2.add_bytes(new);

    // Note, `c1.checksum_inner()` is actually ~m in [Eqn. 3]
    // `c2.checksum_inner()` is actually ~m' in [Eqn. 3]
    // so we have to negate `c2.checksum_inner()` first to get m'.
    // HC' += ~m, c1.checksum_inner() == ~m.
    sum = adc_accumulator(sum, c1.checksum_inner() as Accumulator);
    // HC' += m', c2.checksum_inner() == ~m'.
    sum = adc_accumulator(sum, !c2.checksum_inner() as Accumulator);
    // HC' = ~HC.
    (!normalize(sum)).to_ne_bytes()
}

/// RFC 1071 "internet checksum" computation.
///
/// `Checksum` implements the "internet checksum" defined in [RFC 1071] and
/// updated in [RFC 1141] and [RFC 1624], which is used by many different
/// protocols' packet formats. The checksum operates by computing the 1s
/// complement of the 1s complement sum of successive 16-bit words of the input.
///
/// [RFC 1071]: https://tools.ietf.org/html/rfc1071
/// [RFC 1141]: https://tools.ietf.org/html/rfc1141
/// [RFC 1624]: https://tools.ietf.org/html/rfc1624
#[derive(Default)]
pub struct Checksum {
    sum: Accumulator,
    // Since odd-length inputs are treated specially, we store the trailing byte
    // for use in future calls to add_bytes(), and only treat it as a true
    // trailing byte in checksum().
    trailing_byte: Option<u8>,
}

impl Checksum {
    /// Initialize a new checksum.
    #[inline]
    pub const fn new() -> Self {
        Checksum { sum: 0, trailing_byte: None }
    }

    /// Add bytes to the checksum.
    ///
    /// If `bytes` does not contain an even number of bytes, a single zero byte
    /// will be added to the end before updating the checksum.
    ///
    /// Note that `add_bytes` has some fixed overhead regardless of the size of
    /// `bytes`. Where performance is a concern, prefer fewer calls to
    /// `add_bytes` with larger input over more calls with smaller input.
    #[inline]
    pub fn add_bytes(&mut self, mut bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }

        let mut sum = self.sum;
        let mut carry = false;

        // We are not using `adc_uXX` functions here, instead, we manually track
        // the carry flag. This is because in `adc_uXX` functions, the carry
        // flag depends on addition itself. So the assembly for that function
        // reads as follows:
        //
        // mov %rdi, %rcx
        // mov %rsi, %rax
        // add %rcx, %rsi -- waste! only used to generate CF.
        // adc %rdi, $rax -- the real useful instruction.
        //
        // So we had better to make us depend on the CF generated by the
        // addition of the previous 16-bit word. The ideal assembly should look
        // like:
        //
        // add 0(%rdi), %rax
        // adc 8(%rdi), %rax
        // adc 16(%rdi), %rax
        // .... and so on ...
        //
        // Sadly, there are too many instructions that can affect the carry
        // flag, and LLVM is not that optimized to find out the pattern and let
        // all these adc instructions not interleaved. However, doing so results
        // in 3 instructions instead of the original 4 instructions (the two
        // mov's are still there) and it makes a difference on input size like
        // 1023.
        macro_rules! update_sum_carry {
            ($ty: ident, $chunk: expr) => {
                let (s, c) = sum.overflowing_add($ty::from_ne_bytes($chunk) as Accumulator);
                sum = s.wrapping_add(carry as Accumulator);
                carry = c;
            };
        }

        // Deal with previous trailing byte, if we have one.
        // NB: Don't use `if let Some(t) = self.trailing_byte.take()`. It slows
        // down the fast path (i.e. the `None` case).
        if self.trailing_byte.is_some() {
            let trailing = self.trailing_byte.take().unwrap();
            update_sum_carry!(u16, [trailing, bytes[0]]);
            bytes = &bytes[1..];
        }

        const ACCUMULATOR_BYTES: usize = (Accumulator::BITS / 8) as usize;
        while let Some(chunk) = bytes.first_chunk::<ACCUMULATOR_BYTES>() {
            update_sum_carry!(Accumulator, *chunk);
            bytes = &bytes[ACCUMULATOR_BYTES..];
        }

        // Handle the tail.
        if let Some(chunk) = bytes.first_chunk::<8>() {
            update_sum_carry!(u64, *chunk);
            bytes = &bytes[8..];
        }
        if let Some(chunk) = bytes.first_chunk::<4>() {
            update_sum_carry!(u32, *chunk);
            bytes = &bytes[4..];
        }
        if let Some(chunk) = bytes.first_chunk::<2>() {
            update_sum_carry!(u16, *chunk);
            bytes = &bytes[2..];
        }
        if bytes.len() == 1 {
            // Stash the trailing byte.
            self.trailing_byte = Some(bytes[0]);
        }

        self.sum = sum + (carry as Accumulator);
    }

    /// Computes the checksum, but in big endian byte order.
    fn checksum_inner(&self) -> u16 {
        let mut sum = self.sum;
        if let Some(byte) = self.trailing_byte {
            sum = adc_accumulator(sum, u16::from_ne_bytes([byte, 0]) as Accumulator);
        }
        !normalize(sum)
    }

    /// Computes the one's complement sum and returns the array representation.
    ///
    /// `partial_checksum` returns the one's complement sum of all data added
    /// using `add_bytes` so far. Calling `partial_checksum` does *not* reset
    /// the checksum. More bytes may be added after calling `partial_checksum`,
    /// and they will be added to the checksum as expected.
    ///
    /// `partial_checksum` will return `None` if an odd number of bytes have
    /// been added so far.
    pub fn partial_checksum(&self) -> Option<[u8; 2]> {
        if self.trailing_byte.is_some() {
            return None;
        }
        Some(normalize(self.sum).to_ne_bytes())
    }

    /// Computes the checksum, and returns the array representation.
    ///
    /// `checksum` returns the checksum of all data added using `add_bytes` so
    /// far. Calling `checksum` does *not* reset the checksum. More bytes may be
    /// added after calling `checksum`, and they will be added to the checksum
    /// as expected.
    ///
    /// If an odd number of bytes have been added so far, the checksum will be
    /// computed as though a single 0 byte had been added at the end in order to
    /// even out the length of the input.
    #[inline]
    pub fn checksum(&self) -> [u8; 2] {
        self.checksum_inner().to_ne_bytes()
    }
}

macro_rules! impl_adc {
    ($name: ident, $t: ty) => {
        /// implements 1's complement addition for $t,
        /// exploiting the carry flag on a 2's complement machine.
        /// In practice, the adc instruction will be generated.
        fn $name(a: $t, b: $t) -> $t {
            let (s, c) = a.overflowing_add(b);
            s + (c as $t)
        }
    };
}

impl_adc!(adc_u16, u16);
impl_adc!(adc_u32, u32);
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
impl_adc!(adc_u64, u64);
impl_adc!(adc_accumulator, Accumulator);

/// Normalizes the accumulator by mopping up the
/// overflow until it fits in a `u16`.
fn normalize(a: Accumulator) -> u16 {
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    return normalize_64(adc_u64(a as u64, (a >> 64) as u64));
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    return normalize_64(a);
}

fn normalize_64(a: u64) -> u16 {
    let t = adc_u32(a as u32, (a >> 32) as u32);
    adc_u16(t as u16, (t >> 16) as u16)
}

#[cfg(test)]
mod tests {
    use rand::{Rng, SeedableRng};

    use rand_xorshift::XorShiftRng;

    use super::*;

    /// Create a new deterministic RNG from a seed.
    fn new_rng(mut seed: u128) -> XorShiftRng {
        if seed == 0 {
            // XorShiftRng can't take 0 seeds
            seed = 1;
        }
        XorShiftRng::from_seed(seed.to_ne_bytes())
    }

    #[test]
    fn test_checksum() {
        for buf in IPV4_HEADERS {
            // compute the checksum as normal
            let mut c = Checksum::new();
            c.add_bytes(&buf);
            assert_eq!(c.checksum(), [0u8; 2]);
            // compute the checksum one byte at a time to make sure our
            // trailing_byte logic works
            let mut c = Checksum::new();
            for byte in *buf {
                c.add_bytes(&[*byte]);
            }
            assert_eq!(c.checksum(), [0u8; 2]);

            // Make sure that it works even if we overflow u32. Performing this
            // loop 2 * 2^16 times is guaranteed to cause such an overflow
            // because 0xFFFF + 0xFFFF > 2^16, and we're effectively adding
            // (0xFFFF + 0xFFFF) 2^16 times. We verify the overflow as well by
            // making sure that, at least once, the sum gets smaller from one
            // loop iteration to the next.
            let mut c = Checksum::new();
            c.add_bytes(&[0xFF, 0xFF]);
            for _ in 0..((2 * (1 << 16)) - 1) {
                c.add_bytes(&[0xFF, 0xFF]);
            }
            assert_eq!(c.checksum(), [0u8; 2]);
        }
    }

    #[test]
    fn test_partial_checksum() {
        for buf in IPV4_HEADERS {
            // Partial checksum should compute for even length slices.
            for i in (0..buf.len()).step_by(2) {
                let mut part = Checksum::new();
                part.add_bytes(&buf[..i]);

                let mut c = Checksum::new();
                c.add_bytes(
                    &part
                        .partial_checksum()
                        .expect("partial checksum should compute for even length slices"),
                );
                c.add_bytes(&buf[i..]);
                assert_eq!(c.checksum(), [0u8; 2]);
            }
            // Partial checksum should not compute for odd length slices.
            for i in (1..buf.len()).step_by(2) {
                let mut part = Checksum::new();
                part.add_bytes(&buf[..i]);
                assert_eq!(part.partial_checksum(), None);
            }
            // Partial checksum should be the complement of the checksum.
            let mut c = Checksum::new();
            c.add_bytes(buf);
            assert_eq!(c.partial_checksum(), Some([0xFF; 2]));
        }
    }

    #[test]
    fn test_update() {
        for b in IPV4_HEADERS {
            let mut buf = Vec::new();
            buf.extend_from_slice(b);

            let mut c = Checksum::new();
            c.add_bytes(&buf);
            assert_eq!(c.checksum(), [0u8; 2]);

            // replace the destination IP with the loopback address
            let old = [buf[16], buf[17], buf[18], buf[19]];
            (&mut buf[16..20]).copy_from_slice(&[127, 0, 0, 1]);
            let updated = update(c.checksum(), &old, &[127, 0, 0, 1]);
            let from_scratch = {
                let mut c = Checksum::new();
                c.add_bytes(&buf);
                c.checksum()
            };
            assert_eq!(updated, from_scratch);
        }
    }

    #[test]
    fn test_update_noop() {
        for b in IPV4_HEADERS {
            let mut buf = Vec::new();
            buf.extend_from_slice(b);

            let mut c = Checksum::new();
            c.add_bytes(&buf);
            assert_eq!(c.checksum(), [0u8; 2]);

            // Replace the destination IP with the same address. I.e. this
            // update should be a no-op.
            let old = [buf[16], buf[17], buf[18], buf[19]];
            let updated = update(c.checksum(), &old, &old);
            let from_scratch = {
                let mut c = Checksum::new();
                c.add_bytes(&buf);
                c.checksum()
            };
            assert_eq!(updated, from_scratch);
        }
    }

    #[test]
    fn test_smoke_update() {
        let mut rng = new_rng(70_812_476_915_813);

        for _ in 0..2048 {
            // use an odd length so we test the odd length logic
            const BUF_LEN: usize = 31;
            let buf: [u8; BUF_LEN] = rng.random();
            let mut c = Checksum::new();
            c.add_bytes(&buf);

            let (begin, end) = loop {
                let begin = rng.random_range(0..BUF_LEN);
                let end = begin + (rng.random_range(0..(BUF_LEN + 1 - begin)));
                // update requires that begin is even and end is either even or
                // the end of the input
                if begin % 2 == 0 && (end % 2 == 0 || end == BUF_LEN) {
                    break (begin, end);
                }
            };

            let mut new_buf = buf;
            for i in begin..end {
                new_buf[i] = rng.random();
            }
            let updated = update(c.checksum(), &buf[begin..end], &new_buf[begin..end]);
            let from_scratch = {
                let mut c = Checksum::new();
                c.add_bytes(&new_buf);
                c.checksum()
            };
            assert_eq!(updated, from_scratch);
        }
    }

    /// IPv4 headers.
    ///
    /// This data was obtained by capturing live network traffic.
    const IPV4_HEADERS: &[&[u8]] = &[
        &[
            0x45, 0x00, 0x00, 0x34, 0x00, 0x00, 0x40, 0x00, 0x40, 0x06, 0xae, 0xea, 0xc0, 0xa8,
            0x01, 0x0f, 0xc0, 0xb8, 0x09, 0x6a,
        ],
        &[
            0x45, 0x20, 0x00, 0x74, 0x5b, 0x6e, 0x40, 0x00, 0x37, 0x06, 0x5c, 0x1c, 0xc0, 0xb8,
            0x09, 0x6a, 0xc0, 0xa8, 0x01, 0x0f,
        ],
        &[
            0x45, 0x20, 0x02, 0x8f, 0x00, 0x00, 0x40, 0x00, 0x3b, 0x11, 0xc9, 0x3f, 0xac, 0xd9,
            0x05, 0x6e, 0xc0, 0xa8, 0x01, 0x0f,
        ],
    ];

    // This test checks that an input, found by a fuzzer, no longer causes a crash due to addition
    // overflow.
    #[test]
    fn test_large_buffer_addition_overflow() {
        let mut sum = Checksum { sum: 0, trailing_byte: None };
        let bytes = [
            0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
            255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
        ];
        sum.add_bytes(&bytes[..]);
    }

    // Regression test for https://fxbug.dev/515774797.
    //
    // Verify that checksum calculations produce the same result, no matter if
    // the bytes are added at once, or in odd-length chunks.
    #[test]
    fn test_odd_length_checksum() {
        // Determine the expected value. Per RFC 1071, an odd length of bytes
        // should be padded at the end with a 0.
        let mut c = Checksum::new();
        c.add_bytes(&[1, 2, 3, 0]);
        let expected_checksum = c.checksum();

        // Add the bytes all at once.
        let mut c = Checksum::new();
        c.add_bytes(&[1, 2, 3]);
        assert_eq!(c.checksum(), expected_checksum);

        // Add the bytes in two passes (first pass uses an odd number of bytes).
        let mut c = Checksum::new();
        c.add_bytes(&[1]);
        c.add_bytes(&[2, 3]);
        assert_eq!(c.checksum(), expected_checksum);
    }

    // Verify that we properly perform bounds checks against the byte buffer.
    // Failure to do so would result in index-out-of-bounds panics.
    #[test]
    fn test_add_zero_bytes() {
        let mut c = Checksum::new();
        c.add_bytes(&[]);
        assert_eq!(c.checksum(), [255, 255]);

        // Try again, but this time set a trailing_byte.
        let mut c = Checksum::new();
        c.add_bytes(&[0]);
        c.add_bytes(&[]);
        assert_eq!(c.checksum(), [255, 255]);

        // Try once more, but now complete the trailing byte exactly (no remainder).
        let mut c = Checksum::new();
        c.add_bytes(&[0]);
        c.add_bytes(&[0]);
        assert_eq!(c.checksum(), [255, 255]);
    }
}
