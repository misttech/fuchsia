// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![no_std]

use bitmap::{Bitmap, FixedStorage, RawBitmapGeneric};
use ksync::{KMutex, guarded, lock};
use num_traits::{AsPrimitive, Bounded, FromPrimitive, Unsigned};
use pin_init::{PinInit, pin_init};
use zx_status::Status;

// Allocates architecture-specific resource IDs.
//
// IDs of type `T` will be allocated in the range [`MIN_ID`, `MAX_ID`).
//
// `N` is the number of `usize` words required to back the bitmap for `MAX_ID` bits.
// We must pass `N` explicitly because stable Rust does not support using generic
// parameters (like `MAX_ID`) in const operations within type signatures (e.g.
// `FixedStorage<{ (MAX_ID - 1) / 64 + 1 }>`). Doing so requires the unstable
// `generic_const_exprs` feature.
#[guarded]
pub struct IdAllocator<
    T: Copy + Ord + Unsigned + Bounded + FromPrimitive + AsPrimitive<usize> + 'static,
    const MAX_ID: usize,
    const MIN_ID: usize,
    const N: usize,
> {
    #[mutex]
    mutex: KMutex,

    #[guarded_by(mutex)]
    next: T,

    #[guarded_by(mutex)]
    bitmap: RawBitmapGeneric<FixedStorage<N>>,
}

impl<
    T: Copy + Ord + Unsigned + Bounded + FromPrimitive + AsPrimitive<usize> + 'static,
    const MAX_ID: usize,
    const MIN_ID: usize,
    const N: usize,
> IdAllocator<T, MAX_ID, MIN_ID, N>
{
    const _STATIC_ASSERT: () = {
        assert!(MAX_ID > MIN_ID, "MAX_ID must be greater than MIN_ID");
        let required_words =
            if MAX_ID == 0 { 0 } else { (MAX_ID - 1) / (usize::BITS as usize) + 1 };
        assert!(
            N == required_words,
            "N must be exactly the number of words required to hold MAX_ID bits"
        );
    };

    pub fn init() -> impl PinInit<Self, Status> {
        let _ = Self::_STATIC_ASSERT;
        pin_init!(Self {
            mutex <- KMutex::init(),
            next: T::from_usize(MIN_ID).ok_or(Status::OUT_OF_RANGE)?.into(),
            bitmap: {
                let mut bitmap = RawBitmapGeneric::default();
                bitmap.reset(MAX_ID)?;
                bitmap
            }.into(),
            _: {
                // Runtime check that MAX_ID fits in T
                if MAX_ID > T::max_value().as_() {
                    return Err(Status::OUT_OF_RANGE);
                }
            }
        }? Status)
    }

    pub fn reset(&self, max_id: T) -> Result<(), Status> {
        let max_id_usize = max_id.as_();
        if max_id_usize <= MIN_ID || max_id_usize > MAX_ID {
            return Err(Status::OUT_OF_RANGE);
        }
        lock!(let mut guard = self.lock_mutex());
        guard.as_mut().bitmap_mut().reset(max_id_usize)
    }

    pub fn try_alloc(&self) -> Result<T, Status> {
        lock!(let mut guard = self.lock_mutex());
        let fields = guard.as_mut().fields_mut();
        let next_usize = (*fields.next).as_();

        let mut get_result = fields.bitmap.get(next_usize, MAX_ID);
        if get_result.all_set {
            get_result = fields.bitmap.get(MIN_ID, next_usize);
            if get_result.all_set {
                return Err(Status::NO_RESOURCES);
            }
        }

        let first_unset = get_result.first_unset;
        fields.bitmap.set_one(first_unset)?;
        let val = T::from_usize(first_unset).ok_or(Status::OUT_OF_RANGE)?;

        // Update next
        let next_val_usize = (first_unset + 1) % MAX_ID;
        let next_val_usize = if next_val_usize == 0 { MIN_ID } else { next_val_usize };
        *fields.next = T::from_usize(next_val_usize).ok_or(Status::OUT_OF_RANGE)?;

        Ok(val)
    }

    pub fn free(&self, id: T) -> Result<(), Status> {
        lock!(let mut guard = self.lock_mutex());
        let id_usize = id.as_();
        if !guard.bitmap().get_one(id_usize) {
            return Err(Status::INVALID_ARGS);
        }
        guard.as_mut().bitmap_mut().clear_one(id_usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pin_init::stack_try_pin_init;

    #[test]
    fn test_id_allocator_alloc_and_free() {
        const K_MAX_ID: usize = 8;
        const K_MIN_ID: usize = 1;

        stack_try_pin_init!(let allocator = IdAllocator::<u8, 254, K_MIN_ID, 4>::init());
        let allocator = allocator.unwrap();

        // Reset to invalid value, before using a valid value.
        assert_eq!(allocator.reset(K_MIN_ID as u8), Err(Status::OUT_OF_RANGE));
        assert_eq!(allocator.reset(u8::MAX), Err(Status::OUT_OF_RANGE));
        assert_eq!(allocator.reset(K_MAX_ID as u8), Ok(()));

        // Allocate all IDs.
        for i in K_MIN_ID..K_MAX_ID {
            assert_eq!(allocator.try_alloc(), Ok(i as u8));
        }

        // Allocate when no IDs are free.
        assert_eq!(allocator.try_alloc(), Err(Status::NO_RESOURCES));

        // Free an ID that was just allocated.
        const K_FREE_ID: u8 = (K_MAX_ID / 2) as u8;
        assert_eq!(allocator.free(K_FREE_ID), Ok(()));

        // Free an ID that was already freed.
        assert_eq!(allocator.free(K_FREE_ID), Err(Status::INVALID_ARGS));

        // Free an invalid ID.
        assert_eq!(allocator.free((K_MAX_ID + 1) as u8), Err(Status::INVALID_ARGS));
    }
}
