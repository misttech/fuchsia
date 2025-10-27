// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::mem::MaybeUninit;

use fidl_next_codec::{
    Decode, DecodeError, Encode, EncodeError, Slot, Unconstrained, Wire, WireI32, WireU32, WireU64,
    bitflags,
};

use zerocopy::IntoBytes;

/// The transactional message header flags in byte 0.
#[derive(Clone, Copy, Debug, IntoBytes)]
#[repr(transparent)]
pub struct MessageHeaderFlags0(u8);

/// The transactional message header flags in byte 1.
#[derive(Clone, Copy, Debug, IntoBytes)]
#[repr(transparent)]
pub struct MessageHeaderFlags1(u8);

/// The transactional message header flags in byte 2.
#[derive(Clone, Copy, Debug, IntoBytes)]
#[repr(transparent)]
pub struct MessageHeaderFlags2(u8);

bitflags::bitflags! {
    impl MessageHeaderFlags0: u8 {
        /// The bit set to indicate that the FIDL wire format is version 2.
        const WIRE_FORMAT_V2 = 1 << 1;
    }

    impl MessageHeaderFlags1: u8 {
    }

    impl MessageHeaderFlags2: u8 {
        /// The bit set to indicate that the FIDL method is flexible.
        const FLEXIBLE_METHOD = 1 << 7;
    }
}

/// A FIDL protocol message header
#[derive(Clone, Copy, Debug, IntoBytes)]
#[repr(C)]
pub struct WireMessageHeader {
    /// The transaction ID of the message header
    pub txid: WireU32,
    /// Flags byte 0
    pub flags_0: MessageHeaderFlags0,
    /// Flags byte 1
    pub flags_1: MessageHeaderFlags1,
    /// Flags byte 2
    pub flags_2: MessageHeaderFlags2,
    /// Magic number
    pub magic_number: u8,
    /// The ordinal of the message following this header
    pub ordinal: WireU64,
}

unsafe impl Wire for WireMessageHeader {
    type Decoded<'de> = Self;

    #[inline]
    fn zero_padding(_: &mut MaybeUninit<Self>) {
        // Wire message headers have no padding
    }
}

/// The magic number indicating FIDL protocol compatibility.
pub const MAGIC_NUMBER: u8 = 0x01;

unsafe impl<E: ?Sized> Encode<WireMessageHeader, E> for WireMessageHeader {
    #[inline]
    fn encode(
        self,
        _: &mut E,
        out: &mut MaybeUninit<WireMessageHeader>,
        _: (),
    ) -> Result<(), EncodeError> {
        out.write(self);
        Ok(())
    }
}

unsafe impl<E: ?Sized> Encode<WireMessageHeader, E> for &WireMessageHeader {
    #[inline]
    fn encode(
        self,
        encoder: &mut E,
        out: &mut MaybeUninit<WireMessageHeader>,
        constraint: (),
    ) -> Result<(), EncodeError> {
        Encode::encode(*self, encoder, out, constraint)
    }
}

impl Unconstrained for WireMessageHeader {}

unsafe impl<D: ?Sized> Decode<D> for WireMessageHeader {
    #[inline]
    fn decode(_: Slot<'_, Self>, _: &mut D, _: ()) -> Result<(), DecodeError> {
        Ok(())
    }
}

/// A FIDL protocol epitaph.
#[derive(Clone, Copy, Debug, IntoBytes)]
#[repr(C)]
pub struct WireEpitaph {
    /// The error status.
    pub error: WireI32,
}

unsafe impl Wire for WireEpitaph {
    type Decoded<'de> = Self;

    #[inline]
    fn zero_padding(_: &mut MaybeUninit<Self>) {
        // Wire epitaphs have no padding
    }
}

unsafe impl<E: ?Sized> Encode<WireEpitaph, E> for WireEpitaph {
    #[inline]
    fn encode(
        self,
        _: &mut E,
        out: &mut MaybeUninit<WireEpitaph>,
        _: (),
    ) -> Result<(), EncodeError> {
        out.write(self);
        Ok(())
    }
}

unsafe impl<E: ?Sized> Encode<WireEpitaph, E> for &WireEpitaph {
    #[inline]
    fn encode(
        self,
        encoder: &mut E,
        out: &mut MaybeUninit<WireEpitaph>,
        constraint: (),
    ) -> Result<(), EncodeError> {
        Encode::encode(*self, encoder, out, constraint)
    }
}

impl Unconstrained for WireEpitaph {}

unsafe impl<D: ?Sized> Decode<D> for WireEpitaph {
    #[inline]
    fn decode(_: Slot<'_, Self>, _: &mut D, _: ()) -> Result<(), DecodeError> {
        Ok(())
    }
}
