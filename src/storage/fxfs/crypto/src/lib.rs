// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::anyhow;
use async_trait::async_trait;
use chacha20::cipher::{KeyIvInit, StreamCipher as _, StreamCipherSeek};
use chacha20::{self, ChaCha20};
use fprint::TypeFingerprint;
use futures::TryStreamExt as _;
use futures::stream::FuturesUnordered;
use serde::de::{Error as SerdeError, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;
use zx_status as zx;

mod cipher;
pub mod ff1;

pub use cipher::fscrypt_ino_lblk32::FscryptSoftwareInoLblk32FileCipher;
pub use cipher::fxfs::FxfsCipher;
pub use cipher::{Cipher, CipherHolder, CipherSet, FindKeyResult, KeyType, key_to_cipher};
pub use fidl_fuchsia_fxfs::{
    EmptyStruct, FscryptKeyIdentifier, FscryptKeyIdentifierAndNonce, ObjectType, WrappedKey,
};

pub use cipher::FSCRYPT_PADDING;
pub const FXFS_KEY_SIZE: usize = 256 / 8;
pub const FXFS_WRAPPED_KEY_SIZE: usize = FXFS_KEY_SIZE + 16;

/// Essentially just a vector by another name to indicate that it holds unwrapped key material.
/// The length of an unwrapped key depends on the type of key that is wrapped.
#[derive(Debug)]
pub struct UnwrappedKey(Vec<u8>);
impl UnwrappedKey {
    pub fn new(key: Vec<u8>) -> Self {
        UnwrappedKey(key)
    }
}
impl std::ops::Deref for UnwrappedKey {
    type Target = Vec<u8>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// A fixed length array of 48 bytes that holds an AES-256-GCM-SIV wrapped key.
#[repr(transparent)]
#[derive(Clone, Debug, PartialEq)]
pub struct WrappedKeyBytes(pub [u8; FXFS_WRAPPED_KEY_SIZE]);
impl Default for WrappedKeyBytes {
    fn default() -> Self {
        Self([0u8; FXFS_WRAPPED_KEY_SIZE])
    }
}
impl TryFrom<Vec<u8>> for WrappedKeyBytes {
    type Error = anyhow::Error;

    fn try_from(buf: Vec<u8>) -> Result<Self, Self::Error> {
        Ok(Self(buf.try_into().map_err(|_| anyhow!("wrapped key wrong length"))?))
    }
}
impl From<[u8; FXFS_WRAPPED_KEY_SIZE]> for WrappedKeyBytes {
    fn from(buf: [u8; FXFS_WRAPPED_KEY_SIZE]) -> Self {
        Self(buf)
    }
}
impl TypeFingerprint for WrappedKeyBytes {
    fn fingerprint() -> String {
        "WrappedKeyBytes".to_owned()
    }
}

impl std::ops::Deref for WrappedKeyBytes {
    type Target = [u8; FXFS_WRAPPED_KEY_SIZE];
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for WrappedKeyBytes {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

// Because default impls of Serialize/Deserialize for [T; N] are only defined for N in 0..=32, we
// have to define them ourselves.
impl Serialize for WrappedKeyBytes {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bytes(&self[..])
    }
}

impl<'de> Deserialize<'de> for WrappedKeyBytes {
    fn deserialize<D>(deserializer: D) -> Result<WrappedKeyBytes, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct WrappedKeyVisitor;

        impl<'d> Visitor<'d> for WrappedKeyVisitor {
            type Value = WrappedKeyBytes;

            fn expecting(&self, formatter: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                formatter.write_str("Expected wrapped keys to be 48 bytes")
            }

            fn visit_bytes<E>(self, bytes: &[u8]) -> Result<WrappedKeyBytes, E>
            where
                E: SerdeError,
            {
                self.visit_byte_buf(bytes.to_vec())
            }

            fn visit_byte_buf<E>(self, bytes: Vec<u8>) -> Result<WrappedKeyBytes, E>
            where
                E: SerdeError,
            {
                let orig_len = bytes.len();
                let bytes: [u8; FXFS_WRAPPED_KEY_SIZE] =
                    bytes.try_into().map_err(|_| SerdeError::invalid_length(orig_len, &self))?;
                Ok(WrappedKeyBytes::from(bytes))
            }
        }
        deserializer.deserialize_byte_buf(WrappedKeyVisitor)
    }
}

/// This specifies a single key to be used to encrypt/decrypt.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TypeFingerprint)]
pub enum EncryptionKey {
    /// Legacy Fxfs key that derives XTS tweaks using only the sector offset.
    LegacyFxfs(FxfsKey),
    // NOTE: `key_identifier` can be thought of as the "name" of the key to use; it is not a
    // per-file or per-directory key. It is similar to Fxfs's wrapping key ID, although it
    // doesn't wrap anything. Files using the same `key_identifier` are encrypted using the
    // same underlying key, with just differences in the tweak used. Directories also use the
    // same underlying key, but some structures are further salted using the provided nonce.
    FscryptInoLblk32File {
        key_identifier: [u8; 16],
    },
    FscryptInoLblk32Dir {
        key_identifier: [u8; 16],
        nonce: [u8; 16],
    },
    /// Fxfs key that domain-separates XTS tweaks using `(attribute_id << 64) | sector_offset`.
    Fxfs(FxfsKey),
}

impl<'a> arbitrary::Arbitrary<'a> for EncryptionKey {
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        Ok(match u.int_in_range(0..=3)? {
            0 => EncryptionKey::LegacyFxfs(u.arbitrary()?),
            1 => EncryptionKey::FscryptInoLblk32File { key_identifier: u.arbitrary()? },
            2 => EncryptionKey::FscryptInoLblk32Dir {
                key_identifier: u.arbitrary()?,
                nonce: u.arbitrary()?,
            },
            3 => EncryptionKey::Fxfs(u.arbitrary()?),
            _ => unreachable!(),
        })
    }
}

impl From<EncryptionKey> for WrappedKey {
    fn from(value: EncryptionKey) -> Self {
        match value {
            EncryptionKey::LegacyFxfs(key) | EncryptionKey::Fxfs(key) => {
                WrappedKey::Fxfs(key.into())
            }
            EncryptionKey::FscryptInoLblk32File { key_identifier } => {
                WrappedKey::FscryptInoLblk32File(FscryptKeyIdentifier { key_identifier })
            }
            EncryptionKey::FscryptInoLblk32Dir { key_identifier, nonce } => {
                WrappedKey::FscryptInoLblk32Dir(FscryptKeyIdentifierAndNonce {
                    key_identifier,
                    nonce,
                })
            }
        }
    }
}

impl From<&EncryptionKey> for KeyType {
    fn from(value: &EncryptionKey) -> Self {
        match value {
            EncryptionKey::LegacyFxfs(_) => KeyType::LegacyFxfs,
            EncryptionKey::Fxfs(_) => KeyType::Fxfs,
            EncryptionKey::FscryptInoLblk32File { .. } => KeyType::FscryptInoLblk32File,
            EncryptionKey::FscryptInoLblk32Dir { .. } => KeyType::FscryptInoLblk32Dir,
        }
    }
}

impl TryFrom<WrappedKey> for EncryptionKey {
    type Error = zx::Status;

    fn try_from(value: WrappedKey) -> Result<Self, Self::Error> {
        Ok(match value {
            WrappedKey::Fxfs(fidl_fuchsia_fxfs::FxfsKey { wrapping_key_id, wrapped_key }) => {
                EncryptionKey::Fxfs(FxfsKey { wrapping_key_id, key: WrappedKeyBytes(wrapped_key) })
            }
            WrappedKey::FscryptInoLblk32File(FscryptKeyIdentifier { key_identifier }) => {
                EncryptionKey::FscryptInoLblk32File { key_identifier }
            }
            WrappedKey::FscryptInoLblk32Dir(FscryptKeyIdentifierAndNonce {
                key_identifier,
                nonce,
            }) => EncryptionKey::FscryptInoLblk32Dir { key_identifier, nonce },
            _ => return Err(zx::Status::NOT_SUPPORTED),
        })
    }
}

/// An Fxfs encryption key wrapped in AES-256-GCM-SIV and the associated wrapping key ID.
/// This can be provided to Crypt::unwrap_key to obtain the unwrapped key.
#[derive(Clone, Default, Debug, Serialize, Deserialize, TypeFingerprint, PartialEq)]
pub struct FxfsKey {
    /// The identifier of the wrapping key.  The identifier has meaning to whatever is doing the
    /// unwrapping.
    pub wrapping_key_id: WrappingKeyId,
    /// AES 256 requires a 512 bit key, which is made of two 256 bit keys, one for the data and one
    /// for the tweak.  It is safe to use the same 256 bit key for both (see
    /// https://csrc.nist.gov/CSRC/media/Projects/Block-Cipher-Techniques/documents/BCM/Comments/XTS/follow-up_XTS_comments-Ball.pdf)
    /// which is what we do here.  Since the key is wrapped with AES-GCM-SIV, there are an
    /// additional 16 bytes paid per key (so the actual key material is 32 bytes once unwrapped).
    pub key: WrappedKeyBytes,
}

pub type WrappingKeyId = [u8; 16];

impl From<FxfsKey> for fidl_fuchsia_fxfs::FxfsKey {
    fn from(value: FxfsKey) -> Self {
        fidl_fuchsia_fxfs::FxfsKey {
            wrapping_key_id: value.wrapping_key_id,
            wrapped_key: value.key.0,
        }
    }
}

impl<'a> arbitrary::Arbitrary<'a> for FxfsKey {
    fn arbitrary(_u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        // There doesn't seem to be much point to randomly generate crypto keys.
        return Ok(FxfsKey::default());
    }
}

/// A thin wrapper around a ChaCha20 stream cipher.  This will use a zero nonce. **NOTE**: Great
/// care must be taken not to encrypt different plaintext with the same key and offset (even across
/// multiple boots), so consider if this suits your purpose before using it.
pub struct StreamCipher(ChaCha20);

impl StreamCipher {
    pub fn new(key: &UnwrappedKey, offset: u64) -> Self {
        let mut cipher = Self(ChaCha20::new(
            &chacha20::Key::try_from(&key[..]).expect("Invalid StreamCipher key length"),
            /* nonce: */ &[0; 12].into(),
        ));
        cipher.0.seek(offset);
        cipher
    }

    pub fn encrypt(&mut self, buffer: &mut [u8]) {
        fxfs_trace::duration!("StreamCipher::encrypt", "len" => buffer.len());
        self.0.apply_keystream(buffer);
    }

    pub fn decrypt(&mut self, buffer: &mut [u8]) {
        fxfs_trace::duration!("StreamCipher::decrypt", "len" => buffer.len());
        self.0.apply_keystream(buffer);
    }

    pub fn offset(&self) -> u64 {
        self.0.current_pos()
    }
}

/// Different keys are used for metadata and data in order to make certain operations requiring a
/// metadata key rotation (e.g. secure erase) more efficient.
pub enum KeyPurpose {
    /// The key will be used to wrap user data.
    Data,
    /// The key will be used to wrap internal metadata.
    Metadata,
}

impl TryFrom<fidl_fuchsia_fxfs::KeyPurpose> for KeyPurpose {
    type Error = zx::Status;

    fn try_from(purpose: fidl_fuchsia_fxfs::KeyPurpose) -> Result<Self, Self::Error> {
        match purpose {
            fidl_fuchsia_fxfs::KeyPurpose::Data => Ok(KeyPurpose::Data),
            fidl_fuchsia_fxfs::KeyPurpose::Metadata => Ok(KeyPurpose::Metadata),
            _ => Err(zx::Status::INVALID_ARGS),
        }
    }
}

/// The `Crypt` trait below provides a mechanism to unwrap a key or set of keys.
/// The wrapping keys can be one of these types.
pub enum WrappingKey {
    /// This is used for keys of the type WrappedKey::Fxfs.
    Aes256GcmSiv([u8; 32]),
    /// This is used for legacy fscrypt keys that use a 64-byte main key.
    Fscrypt([u8; 64]),
}
impl From<[u8; 32]> for WrappingKey {
    fn from(value: [u8; 32]) -> Self {
        WrappingKey::Aes256GcmSiv(value)
    }
}
impl From<[u8; 64]> for WrappingKey {
    fn from(value: [u8; 64]) -> Self {
        WrappingKey::Fscrypt(value)
    }
}

/// The keys it unwraps can be wrapped with either Aes256GcmSiv (ideally) or using via
/// legacy fscrypt master key + HKDF.

/// An interface trait with the ability to wrap and unwrap encryption keys.
///
/// Note that existence of this trait does not imply that an object will **securely**
/// wrap and unwrap keys; rather just that it presents an interface for wrapping operations.
#[async_trait]
pub trait Crypt: Send + Sync {
    /// `owner` is intended to be used such that when the key is wrapped, it appears to be different
    /// to that of the same key wrapped by a different owner.  In this way, keys can be shared
    /// amongst different filesystem objects (e.g. for clones), but it is not possible to tell just
    /// by looking at the wrapped keys.
    async fn create_key(
        &self,
        owner: u64,
        purpose: KeyPurpose,
    ) -> Result<(FxfsKey, UnwrappedKey), zx::Status>;

    /// `owner` is intended to be used such that when the key is wrapped, it appears to be different
    /// to that of the same key wrapped by a different owner.  In this way, keys can be shared
    /// amongst different filesystem objects (e.g. for clones), but it is not possible to tell just
    /// by looking at the wrapped keys.
    async fn create_key_with_id(
        &self,
        owner: u64,
        wrapping_key_id: WrappingKeyId,
        object_type: ObjectType,
    ) -> Result<(EncryptionKey, UnwrappedKey), zx::Status>;

    /// Unwraps a single key, returning a raw unwrapped key.
    /// This method is generally only used with StreamCipher and FF1.
    /// Returns `zx::Status::UNAVAILABLE` if the key is known but cannot be unwrapped (e.g. it is
    /// locked).
    /// Returns `zx::Status::NOT_FOUND` if the wrapping key is not known.
    async fn unwrap_key(
        &self,
        wrapped_key: &WrappedKey,
        owner: u64,
    ) -> Result<UnwrappedKey, zx::Status>;

    /// Unwraps object keys and stores the result as a CipherSet mapping key_id to:
    ///   - Some(cipher) if unwrapping key was found or
    ///   - None if unwrapping key was missing.
    /// The cipher can be used directly to encrypt/decrypt data.
    async fn unwrap_keys(
        &self,
        keys: &[(u64, EncryptionKey)],
        owner: u64,
    ) -> Result<CipherSet, zx::Status> {
        let futures: FuturesUnordered<_> = keys
            .iter()
            .map(|(key_id, key)| {
                let key_id = *key_id;
                let wrapped_key = WrappedKey::from(key.clone());
                let owner = owner;
                async move {
                    match self.unwrap_key(&wrapped_key, owner).await {
                        Ok(unwrapped_key) => cipher::key_to_cipher(key, &unwrapped_key)
                            .map(|c| (key_id, cipher::CipherHolder::Cipher(c))),
                        Err(zx::Status::UNAVAILABLE) => {
                            Ok((key_id, cipher::CipherHolder::Unavailable))
                        }
                        Err(e) => Err(e),
                    }
                }
            })
            .collect();
        let result = futures.try_collect::<BTreeMap<u64, _>>().await?;
        Ok(result.into())
    }
}

#[cfg(test)]
mod tests {
    use super::{StreamCipher, UnwrappedKey};

    #[test]
    fn test_stream_cipher_offset() {
        let key = UnwrappedKey::new(vec![
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
            25, 26, 27, 28, 29, 30, 31, 32,
        ]);
        let mut cipher1 = StreamCipher::new(&key, 0);
        let mut p1 = [1, 2, 3, 4];
        let mut c1 = p1.clone();
        cipher1.encrypt(&mut c1);

        let mut cipher2 = StreamCipher::new(&key, 1);
        let p2 = [5, 6, 7, 8];
        let mut c2 = p2.clone();
        cipher2.encrypt(&mut c2);

        let xor_fn = |buf1: &mut [u8], buf2| {
            for (b1, b2) in buf1.iter_mut().zip(buf2) {
                *b1 ^= b2;
            }
        };

        // Check that c1 ^ c2 != p1 ^ p2 (which would be the case if the same offset was used for
        // both ciphers).
        xor_fn(&mut c1, &c2);
        xor_fn(&mut p1, &p2);
        assert_ne!(c1, p1);
    }
}
