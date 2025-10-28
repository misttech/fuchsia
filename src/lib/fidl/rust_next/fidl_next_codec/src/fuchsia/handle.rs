// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::fmt;
use core::mem::{MaybeUninit, forget};

use fidl_constants::{ALLOC_ABSENT_U32, ALLOC_PRESENT_U32};
use zx::Handle;
use zx::sys::{ZX_HANDLE_INVALID, zx_handle_t};

use crate::fuchsia::{HandleDecoder, HandleEncoder};
use crate::{
    Constrained, Decode, DecodeError, Encode, EncodeError, EncodeOption, FromWire, FromWireOption,
    IntoNatural, Slot, Unconstrained, Wire, WireU32, munge,
};

/// A Zircon handle.
#[repr(C, align(4))]
pub union WireHandle {
    encoded: WireU32,
    decoded: zx_handle_t,
}

impl Drop for WireHandle {
    fn drop(&mut self) {
        // SAFETY: `WireHandle` is always a valid `Handle`.
        let handle = unsafe { Handle::from_raw(self.as_raw_handle()) };
        drop(handle);
    }
}

unsafe impl Wire for WireHandle {
    type Decoded<'de> = Self;

    #[inline]
    fn zero_padding(_: &mut MaybeUninit<Self>) {
        // Wire handles have no padding
    }
}

impl WireHandle {
    /// Encodes a handle as present in an output.
    pub fn set_encoded_present(out: &mut MaybeUninit<Self>) {
        munge!(let Self { encoded } = out);
        encoded.write(WireU32(ALLOC_PRESENT_U32));
    }

    /// Returns whether the underlying `zx_handle_t` is invalid.
    pub fn is_invalid(&self) -> bool {
        self.as_raw_handle() == ZX_HANDLE_INVALID
    }

    /// Returns the underlying [`zx_handle_t`].
    #[inline]
    pub fn as_raw_handle(&self) -> zx_handle_t {
        unsafe { self.decoded }
    }
}

impl fmt::Debug for WireHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_raw_handle().fmt(f)
    }
}

unsafe impl<D: HandleDecoder + ?Sized> Decode<D> for WireHandle {
    fn decode(
        mut slot: Slot<'_, Self>,
        decoder: &mut D,
        _constraint: <Self as Constrained>::Constraint,
    ) -> Result<(), DecodeError> {
        munge!(let Self { encoded } = slot.as_mut());

        match **encoded {
            ALLOC_ABSENT_U32 => return Err(DecodeError::RequiredHandleAbsent),
            ALLOC_PRESENT_U32 => {
                let handle = decoder.take_raw_handle()?;
                munge!(let Self { mut decoded } = slot);
                decoded.write(handle);
            }
            e => return Err(DecodeError::InvalidHandlePresence(e)),
        }
        Ok(())
    }
}

impl Constrained for WireHandle {
    type Constraint = ();

    fn validate(
        _slot: Slot<'_, Self>,
        _constraint: Self::Constraint,
    ) -> Result<(), crate::ValidationError> {
        // TODO: validate handle rights.
        Ok(())
    }
}

/// An optional Zircon handle.
#[derive(Debug)]
#[repr(transparent)]
pub struct WireOptionalHandle {
    handle: WireHandle,
}

unsafe impl Wire for WireOptionalHandle {
    type Decoded<'de> = Self;

    #[inline]
    fn zero_padding(out: &mut MaybeUninit<Self>) {
        munge!(let Self { handle } = out);
        WireHandle::zero_padding(handle);
    }
}

impl WireOptionalHandle {
    /// Encodes a handle as present in a slot.
    pub fn set_encoded_present(out: &mut MaybeUninit<Self>) {
        munge!(let Self { handle } = out);
        WireHandle::set_encoded_present(handle);
    }

    /// Encodes a handle as absent in an output.
    pub fn set_encoded_absent(out: &mut MaybeUninit<Self>) {
        munge!(let Self { handle: WireHandle { encoded } } = out);
        encoded.write(WireU32(ZX_HANDLE_INVALID));
    }

    /// Returns whether a handle is present.
    pub fn is_some(&self) -> bool {
        !self.handle.is_invalid()
    }

    /// Returns whether a handle is absent.
    pub fn is_none(&self) -> bool {
        self.handle.is_invalid()
    }

    /// Returns the underlying [`zx_handle_t`], if any.
    #[inline]
    pub fn as_raw_handle(&self) -> Option<zx_handle_t> {
        self.is_some().then(|| self.handle.as_raw_handle())
    }
}

unsafe impl<D: HandleDecoder + ?Sized> Decode<D> for WireOptionalHandle {
    fn decode(mut slot: Slot<'_, Self>, decoder: &mut D, _: ()) -> Result<(), DecodeError> {
        munge!(let Self { handle: mut wire_handle } = slot.as_mut());
        munge!(let WireHandle { encoded } = wire_handle.as_mut());

        match **encoded {
            ALLOC_ABSENT_U32 => (),
            ALLOC_PRESENT_U32 => {
                let handle = decoder.take_raw_handle()?;
                munge!(let WireHandle { mut decoded } = wire_handle);
                decoded.write(handle);
            }
            e => return Err(DecodeError::InvalidHandlePresence(e)),
        }
        Ok(())
    }
}

unsafe impl<E: HandleEncoder + ?Sized> Encode<WireHandle, E> for Handle {
    fn encode(
        self,
        encoder: &mut E,
        out: &mut MaybeUninit<WireHandle>,
        _constraint: (),
    ) -> Result<(), EncodeError> {
        if self.is_invalid() {
            Err(EncodeError::InvalidRequiredHandle)
        } else {
            encoder.push_handle(self)?;
            WireHandle::set_encoded_present(out);
            Ok(())
        }
    }
}

impl FromWire<WireHandle> for Handle {
    fn from_wire(wire: WireHandle) -> Self {
        // SAFETY: `WireHandle` is always a valid `Handle`.
        let handle = unsafe { Handle::from_raw(wire.as_raw_handle()) };
        forget(wire);
        handle
    }
}

impl IntoNatural for WireHandle {
    type Natural = Handle;
}

unsafe impl<E: HandleEncoder + ?Sized> EncodeOption<WireOptionalHandle, E> for Handle {
    fn encode_option(
        this: Option<Self>,
        encoder: &mut E,
        out: &mut MaybeUninit<WireOptionalHandle>,
        _constraint: (),
    ) -> Result<(), EncodeError> {
        if let Some(handle) = this {
            encoder.push_handle(handle)?;
            WireOptionalHandle::set_encoded_present(out);
        } else {
            WireOptionalHandle::set_encoded_absent(out);
        }
        Ok(())
    }
}

impl FromWireOption<WireOptionalHandle> for Handle {
    fn from_wire_option(wire: WireOptionalHandle) -> Option<Self> {
        let raw_handle = wire.as_raw_handle();
        forget(wire);
        raw_handle.map(|raw| unsafe { Handle::from_raw(raw) })
    }
}

impl IntoNatural for WireOptionalHandle {
    type Natural = Option<Handle>;
}

// TODO: validate handle rights
impl Unconstrained for WireOptionalHandle {}
