// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::cell::UnsafeCell;
use std::fmt;
use std::mem::MaybeUninit;
use std::sync::RwLock;
use std::sync::atomic::{AtomicPtr, Ordering};

use super::codec::{HandleDecoder, HandleEncoder};
use fidl_next_codec::{
    Decode, DecodeError, Encodable, EncodableOption, Encode, EncodeError, EncodeOption, FromWire,
    FromWireOption, Slot, Wire, WireU32, munge,
};

use crate::{Client, Handle};

struct HandleAssoc {
    hid: UnsafeCell<u32>,
    client: AtomicPtr<Client>,
}

// SAFETY: We use the atomic pointer to synchronize access to the hid field.
unsafe impl Send for HandleAssoc {}
unsafe impl Sync for HandleAssoc {}

const HANDLE_CLIENT_ASSOC_START_SIZE: usize = 32;
static HANDLE_CLIENT_ASSOC: RwLock<&'static [HandleAssoc]> = RwLock::new(&[]);

/// An FDomain handle.
#[repr(C, align(4))]
pub union WireHandle {
    encoded: WireU32,
    decoded: u32,
}

impl From<Handle> for WireHandle {
    fn from(mut handle: Handle) -> WireHandle {
        let id = handle.id;
        let client = std::mem::replace(&mut handle.client, std::sync::Weak::new());
        let ptr = client.into_raw() as *mut Client;

        loop {
            let table = HANDLE_CLIENT_ASSOC.read().unwrap();

            for (got_id, entry) in table.iter().enumerate() {
                let got_id: u32 = got_id.try_into().expect("Handle table overflowed u32");
                if entry
                    .client
                    .compare_exchange(
                        std::ptr::null_mut(),
                        ptr,
                        Ordering::Acquire,
                        Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    // SAFETY: If we were able to populate the client field then
                    // we own this slot and it is ours to write.
                    unsafe {
                        *entry.hid.get() = id;
                        return WireHandle { decoded: got_id + 1 };
                    }
                }
            }

            std::mem::drop(table);
            let mut table = HANDLE_CLIENT_ASSOC.write().unwrap();
            let new_len = std::cmp::max(table.len() * 2, HANDLE_CLIENT_ASSOC_START_SIZE);

            let new = std::iter::repeat_with(|| HandleAssoc {
                hid: UnsafeCell::new(0),
                client: AtomicPtr::new(std::ptr::null_mut()),
            })
            .take(new_len)
            .collect::<Box<[_]>>();

            let new = Box::leak(new);
            let old = std::mem::replace(&mut *table, new);

            if old.len() > 0 {
                // SAFETY: If this isn't the zero-length starting slice then it was
                // leaked just above in a previous call/iteration.
                unsafe { drop(Box::from_raw(old as *const [HandleAssoc] as *mut [HandleAssoc])) }
            }
        }
    }
}

impl Drop for WireHandle {
    fn drop(&mut self) {
        drop(self.take_handle());
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
        encoded.write(WireU32(u32::MAX));
    }

    /// Returns whether the underlying u32 is invalid.
    pub fn is_invalid(&self) -> bool {
        self.as_raw_handle() == 0
    }

    pub fn invalidate(&mut self) {
        self.decoded = 0;
    }

    /// Returns the underlying `u1`.
    #[inline]
    pub fn as_raw_handle(&self) -> u32 {
        unsafe { self.decoded }
    }

    /// Takes the raw handle out of the handle table.
    pub(crate) fn take_handle(&mut self) -> Handle {
        // SAFETY: `WireHandle` is always a valid index into the association table,
        // and the handle value is always in the association table.
        unsafe {
            let pos = self.decoded as usize;
            self.decoded = 0;
            let Some(pos) = pos.checked_sub(1) else {
                return Handle::invalid();
            };
            let (id, ptr) = {
                let table = HANDLE_CLIENT_ASSOC.read().unwrap();
                let entry = &table[pos];
                // We have to read the hid first as when we swap out the client
                // that is when we mark the slot free.
                let hid = *entry.hid.get();
                let ptr = entry.client.swap(std::ptr::null_mut(), Ordering::Release);
                (hid, ptr)
            };

            let client = std::sync::Weak::from_raw(ptr);

            Handle { id, client }
        }
    }
}

impl fmt::Debug for WireHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_raw_handle().fmt(f)
    }
}

unsafe impl<D: HandleDecoder + ?Sized> Decode<D> for WireHandle {
    fn decode(mut slot: Slot<'_, Self>, decoder: &mut D) -> Result<(), DecodeError> {
        munge!(let Self { encoded } = slot.as_mut());

        match **encoded {
            0 => (),
            u32::MAX => {
                let handle = decoder.take_raw_handle()?;
                munge!(let Self { mut decoded } = slot);
                decoded.write(handle);
            }
            e => return Err(DecodeError::InvalidHandlePresence(e)),
        }
        Ok(())
    }
}

/// An optional Zircon handle.
#[derive(Debug)]
#[repr(transparent)]
pub struct WireOptionalHandle {
    pub(crate) handle: WireHandle,
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
        encoded.write(WireU32(0));
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
    pub fn as_raw_handle(&self) -> Option<u32> {
        self.is_some().then(|| self.handle.as_raw_handle())
    }
}

unsafe impl<D: HandleDecoder + ?Sized> Decode<D> for WireOptionalHandle {
    fn decode(mut slot: Slot<'_, Self>, decoder: &mut D) -> Result<(), DecodeError> {
        munge!(let Self { handle } = slot.as_mut());
        WireHandle::decode(handle, decoder)
    }
}

impl Encodable for Handle {
    type Encoded = WireHandle;
}

unsafe impl<E: HandleEncoder + ?Sized> Encode<E> for Handle {
    fn encode(
        self,
        encoder: &mut E,
        out: &mut MaybeUninit<Self::Encoded>,
    ) -> Result<(), EncodeError> {
        if self.client.upgrade().is_none() {
            Err(EncodeError::InvalidRequiredHandle)
        } else {
            encoder.push_handle(self)?;
            WireHandle::set_encoded_present(out);
            Ok(())
        }
    }
}

impl FromWire<WireHandle> for Handle {
    fn from_wire(mut wire: WireHandle) -> Self {
        wire.take_handle()
    }
}

impl EncodableOption for Handle {
    type EncodedOption = WireOptionalHandle;
}

unsafe impl<E: HandleEncoder + ?Sized> EncodeOption<E> for Handle {
    fn encode_option(
        this: Option<Self>,
        encoder: &mut E,
        out: &mut MaybeUninit<Self::EncodedOption>,
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
    fn from_wire_option(mut wire: WireOptionalHandle) -> Option<Self> {
        if wire.handle.is_invalid() { None } else { Some(wire.handle.take_handle()) }
    }
}
