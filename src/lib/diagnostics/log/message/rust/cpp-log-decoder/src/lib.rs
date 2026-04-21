// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be found in the LICENSE file.

use bumpalo::Bump;
use diagnostics_log_encoding;
use diagnostics_log_encoding::parse::ParseError;
use diagnostics_message::error::MessageError;
use diagnostics_message::ffi::{CPPArray, LogMessage};
use diagnostics_message::{self as message, MonikerWithUrl};
use std::collections::HashMap;
use std::ffi::CString;
use std::os::raw::c_char;
use thiserror::Error;

const BASE_TAG_SHIFT: u32 = 16;
const BASE_TAG_MASK: u64 = 0x7FFF_FFFF;
const MANIFEST_SHIFT: u32 = 47;
const MANIFEST_MASK: u64 = 1u64 << MANIFEST_SHIFT;

const fn is_manifest(header: u64) -> bool {
    (header & MANIFEST_MASK) != 0
}

/// # Safety
///
/// Same as for `std::slice::from_raw_parts`. Summarizing in terms of this API:
///
/// - `msg` must be valid for reads for `size`, and it must be properly aligned.
/// - `msg` must point to `size` consecutive u8 values.
/// - The `size` of the slice must be no larger than `isize::MAX`, and adding
///   that size to data must not "wrap around" the address space. See the safety
///   documentation of pointer::offset.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fuchsia_decode_log_message_to_json(
    msg: *const u8,
    size: usize,
) -> *mut c_char {
    let managed_ptr = unsafe { std::slice::from_raw_parts(msg, size) };
    let data = &message::from_structured(
        MonikerWithUrl { moniker: "test_moniker".try_into().unwrap(), url: "".into() },
        managed_ptr,
    )
    .unwrap();
    let item = serde_json::to_string(&data).unwrap();
    CString::new(format!("[{}]", item)).unwrap().into_raw()
}

/// Memory-managed state to be free'd on the Rust side
/// when the log messages are destroyed.
pub struct ManagedState<'a> {
    allocator: Bump,
    message_array: Vec<*mut LogMessage<'a>>,
}

impl Drop for LogMessages<'_> {
    fn drop(&mut self) {
        unsafe {
            // SAFETY: All pointers in message_array are assumed to be valid.
            // Other unsafe code in this file and in C++ ensures this invariant.

            // Free all managed state in the log messages.
            // The log messages themselves don't need to be explicitly free'd
            // as they are owned by the Bump allocator.
            let state = Box::from_raw(self.state);
            for msg in &state.message_array {
                std::ptr::drop_in_place(*msg);
            }
        }
    }
}

/// LogMessages struct containing log messages
/// It is created by calling fuchsia_decode_log_messages_to_struct,
/// and freed by calling fuchsia_free_log_messages.
/// Log messages contain embedded pointers to the bytes from
/// which they were created, so the memory referred to
/// by the LogMessages must not be modified or free'd until
/// the LogMessages are free'd.
#[repr(C)]
pub struct LogMessages<'a> {
    messages: CPPArray<*mut LogMessage<'a>>,
    state: *mut ManagedState<'a>,
    error_str: *mut c_char,
}

#[derive(Error, Debug)]
pub enum DecodeError {
    #[error(transparent)]
    Message(#[from] MessageError),
    #[error(transparent)]
    ParserError(#[from] ParseError),
}

/// # Safety
///
/// Same as for `std::slice::from_raw_parts`. Summarizing in terms of this API:
///
/// - `msg` must be valid for reads for `size`, and it must be properly aligned.
/// - `msg` must point to `size` consecutive u8 values.
/// - 'msg' must outlive the returned LogMessages struct, and must not be free'd
///   until fuchsia_free_log_messages has been called.
/// - The `size` of the slice must be no larger than `isize::MAX`, and adding
///   that size to data must not "wrap around" the address space. See the safety
///   documentation of pointer::offset.
/// If identity is provided, it must contain a valid moniker and URL.
///
/// The returned LogMessages may be free'd with fuchsia_free_log_messages(log_messages).
/// Free'ing the LogMessages struct does the following, in this order:
/// * Frees memory associated with each individual log message
/// * Frees the bump allocator itself (and everything allocated from it), as well as
/// the message array itself.
/// If a malformed message is passed, returns nullptr.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fuchsia_decode_log_messages_to_struct(
    msg: *const u8,
    size: usize,
    expect_extended_attribution: bool,
) -> LogMessages<'static> {
    unsafe {
        fuchsia_decode_log_messages_to_struct_internal(msg, size, expect_extended_attribution)
    }
    .unwrap_or_else(|err| LogMessages {
        messages: (&vec![]).into(),
        state: std::ptr::null_mut(),
        error_str: CString::new(err.to_string())
            .map(|value| value.into_raw())
            .unwrap_or(std::ptr::null_mut()),
    })
}

/// # Safety
///
/// Same as for `std::slice::from_raw_parts`. Summarizing in terms of this API:
///
/// - `msg` must be valid for reads for `size`, and it must be properly aligned.
/// - `msg` must point to `size` consecutive u8 values.
/// - 'msg' must outlive the returned LogMessages struct, and must not be free'd
///   until fuchsia_free_log_messages has been called.
/// - The `size` of the slice must be no larger than `isize::MAX`, and adding
///   that size to data must not "wrap around" the address space. See the safety
///   documentation of pointer::offset.
/// If identity is provided, it must contain a valid moniker and URL.
///
/// The returned LogMessages may be free'd with fuchsia_free_log_messages(log_messages).
/// Free'ing the LogMessages struct does the following, in this order:
/// * Frees memory associated with each individual log message
/// * Frees the bump allocator itself (and everything allocated from it), as well as
/// the message array itself.
unsafe fn fuchsia_decode_log_messages_to_struct_internal(
    msg: *const u8,
    size: usize,
    expect_extended_attribution: bool,
) -> Result<LogMessages<'static>, DecodeError> {
    let mut state = Box::new(ManagedState { allocator: Bump::new(), message_array: vec![] });
    let buf = unsafe { std::slice::from_raw_parts(msg, size) };
    let mut current_slice: &[u8] = buf;
    let mut tag_map: std::collections::HashMap<u32, (String, String)> =
        std::collections::HashMap::new();

    loop {
        if current_slice.len() < 8 {
            if current_slice.is_empty() {
                break;
            } else {
                return Err(diagnostics_log_encoding::parse::ParseError::InvalidHeader.into());
            }
        }
        let (data, remaining) = if expect_extended_attribution {
            parse_extended_record(&state, current_slice, &mut tag_map)?
        } else {
            let (input, remaining_after_parse) =
                diagnostics_log_encoding::parse::parse_record(current_slice)?;
            let data = diagnostics_message::ffi::build_logs_data(input, None, unsafe {
                // SAFETY: The returned LogMessage must NOT outlive the bump allocator.
                // This is ensured by the allocator living in the heap-allocated ManagedState
                // struct which frees the LogMessages first when dropped, before allowing the bump
                // allocator itself to be freed.
                &*(&state.allocator as *const Bump)
            })?
            .build();
            (Some(data), remaining_after_parse)
        };

        if let Some(data) = data {
            state.message_array.push(data as *mut LogMessage<'static>);
        }
        if remaining.is_empty() {
            break;
        }
        current_slice = remaining;
    }

    Ok(LogMessages {
        messages: (&state.message_array).into(),
        state: Box::into_raw(state),
        error_str: std::ptr::null_mut(),
    })
}

fn parse_extended_record<'a>(
    state: &Box<ManagedState<'a>>,
    current_slice: &'a [u8],
    tag_map: &mut HashMap<u32, (String, String)>,
) -> Result<(Option<&'a mut LogMessage<'a>>, &'a [u8]), DecodeError> {
    let header_bytes: [u8; 8] = current_slice[0..8].try_into().unwrap();
    let header_val = u64::from_le_bytes(header_bytes);
    let base_tag = ((header_val >> BASE_TAG_SHIFT) & BASE_TAG_MASK) as u32;
    let is_manifest = is_manifest(header_val);
    let (input, remaining_after_parse) =
        diagnostics_log_encoding::parse::parse_record(current_slice)?;
    Ok(if is_manifest {
        let mut moniker = None;
        let mut url = None;
        for arg in &input.arguments {
            if arg.name() == "moniker" {
                if let diagnostics_log_encoding::Value::Text(v) = arg.value() {
                    moniker = Some(v.to_string());
                }
            } else if arg.name() == "url" {
                if let diagnostics_log_encoding::Value::Text(v) = arg.value() {
                    url = Some(v.to_string());
                }
            }
        }
        if let (Some(m), Some(u)) = (moniker, url) {
            tag_map.insert(base_tag, (m, u));
        }
        (None, remaining_after_parse)
    } else {
        let metadata =
            tag_map.get(&base_tag).map(|(m, u)| diagnostics_message::ffi::ExtendedMetadata {
                moniker: m.as_str(),
                url: u.as_str(),
                rolled_out_logs: 0,
            });
        let data = diagnostics_message::ffi::build_logs_data(input, metadata, unsafe {
            // SAFETY: The returned LogMessage must NOT outlive the bump allocator.
            // This is ensured by the allocator living in the heap-allocated ManagedState
            // struct which frees the LogMessages first when dropped, before allowing the bump
            // allocator itself to be freed.
            &*(&state.allocator as *const Bump)
        })?
        .build();
        (Some(data), remaining_after_parse)
    })
}

/// # Safety
///
/// This should only be called with a pointer obtained through
/// `fuchsia_decode_log_message_to_json`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fuchsia_free_decoded_log_message(msg: *mut c_char) {
    let str_to_free = unsafe { CString::from_raw(msg) };
    let _freer = str_to_free;
}

/// # Safety
///
/// This should only be called with a pointer obtained through
/// `fuchsia_decode_log_messages_to_struct`. This method
/// should not be called if state is nullptr.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fuchsia_free_log_messages(input: LogMessages<'_>) {
    drop(input);
}
