// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use siphasher::sip::SipHasher;
use std::hash::Hasher;

// We have to match the hash_code implementation used by f2fs to migrate direntry without
// decrypting them.

// See: https://en.wikipedia.org/wiki/Tiny_Encryption_Algorithm
fn tea(input: &[u32; 4], buf: &mut [u32; 2]) {
    const DELTA: u32 = 0x9e3779b9;
    let mut sum = 0u32;
    let mut v = buf.clone();
    for _ in 0..16 {
        sum = sum.wrapping_add(DELTA);
        v[0] = v[0].wrapping_add(
            (v[1] << 4).wrapping_add(input[0])
                ^ v[1].wrapping_add(sum)
                ^ (v[1] >> 5).wrapping_add(input[1]),
        );
        v[1] = v[1].wrapping_add(
            (v[0] << 4).wrapping_add(input[2])
                ^ v[0].wrapping_add(sum)
                ^ (v[0] >> 5).wrapping_add(input[3]),
        );
    }
    buf[0] = buf[0].wrapping_add(v[0]);
    buf[1] = buf[1].wrapping_add(v[1]);
}

/// This is the function used unless both casefolding + encryption are enabled.
pub fn tea_hash_filename(name_bytes: impl IntoIterator<Item = u8>) -> u32 {
    // The tea hash algorithm operates on groups of [4; u32], but the u32
    // need to be big-endian.
    let mut buf = [0x67452301, 0xefcdab89];
    let mut done = false;
    let mut name_bytes = name_bytes.into_iter();
    let mut bytes = [0u8; 16];
    while !done {
        let mut out = 0;
        while out < 16 {
            let Some(c) = name_bytes.next() else {
                if out == 0 {
                    return buf[0];
                }
                // Pad the remainder of the last buffer with the length of this
                // chunk.
                bytes[out..].fill(out as u8);
                done = true;
                break;
            };
            bytes[out] = c;
            out += 1;
        }
        let mut k = [
            u32::from_be_bytes(bytes[0..4].try_into().unwrap()),
            u32::from_be_bytes(bytes[4..8].try_into().unwrap()),
            u32::from_be_bytes(bytes[8..12].try_into().unwrap()),
            u32::from_be_bytes(bytes[12..16].try_into().unwrap()),
        ];
        if done {
            // Due to a quirk of how the buffer is filled, the last u32 needs
            // to be rotated.
            k[out / 4] = k[out / 4].rotate_left(out as u32 % 4 * 8);
        }
        tea(&k, &mut buf);
    }
    buf[0]
}

// A stronger hash function is used if casefold + FBE are used together.
// Nb: If encryption is used without casefolding, the hash_code is based on the encrypted filename.
pub fn casefold_encrypt_hash_filename(name: &str, dirhash_key: &[u8; 16]) -> u32 {
    let mut hasher = SipHasher::new_with_key(dirhash_key);
    hasher.write(name.as_bytes());
    hasher.finish() as u32
}
