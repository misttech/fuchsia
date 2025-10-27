// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::{HandleEncoder, WireHandle, WireOptionalHandle};
use crate::{Channel, Event, EventPair, Handle, HandleBased, Socket};
use fidl_next_codec::{Encode, EncodeError, EncodeOption, FromWire, FromWireOption};
use std::mem::MaybeUninit;

macro_rules! handle_type {
    ($name:ident) => {
        unsafe impl<E: HandleEncoder + ?Sized> Encode<WireHandle, E> for $name {
            fn encode(
                self,
                encoder: &mut E,
                out: &mut MaybeUninit<WireHandle>,
                constraint: (),
            ) -> Result<(), EncodeError> {
                Encode::encode(self.into_handle(), encoder, out, constraint)
            }
        }

        impl FromWire<WireHandle> for $name {
            fn from_wire(wire: WireHandle) -> Self {
                $name::from_handle(Handle::from_wire(wire))
            }
        }

        unsafe impl<E: HandleEncoder + ?Sized> EncodeOption<WireOptionalHandle, E> for $name {
            fn encode_option(
                this: Option<Self>,
                encoder: &mut E,
                out: &mut MaybeUninit<WireOptionalHandle>,
                constraint: (),
            ) -> Result<(), EncodeError> {
                EncodeOption::encode_option(
                    this.map(HandleBased::into_handle),
                    encoder,
                    out,
                    constraint,
                )
            }
        }

        impl FromWireOption<WireOptionalHandle> for $name {
            fn from_wire_option(wire: WireOptionalHandle) -> Option<Self> {
                Handle::from_wire_option(wire).map(Self::from_handle)
            }
        }
    };
}

handle_type!(Channel);
handle_type!(Event);
handle_type!(EventPair);
handle_type!(Socket);
