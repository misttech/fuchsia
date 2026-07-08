// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Type-safe bindings for Zircon status.

#![no_std]

use core::fmt;
use zx_types as sys;

// Creates associated constants of TypeName of the form
// `pub const NAME: TypeName = TypeName(path::to::value);`
// and provides a private `assoc_const_name` method and a `Debug` implementation
// for the type based on `$name`.
// If multiple names match, the first will be used in `name` and `Debug`.
#[macro_export]
macro_rules! assoc_values {
    ($typename:ident, [$($(#[$attr:meta])* $name:ident = $value:path;)*]) => {
        #[allow(non_upper_case_globals)]
        impl $typename {
            $(
                $(#[$attr])*
                pub const $name: $typename = $typename($value);
            )*

            fn assoc_const_name(&self) -> Option<&'static str> {
                match self.0 {
                    $(
                        $value => Some(stringify!($name)),
                    )*
                    _ => None,
                }
            }
        }

        impl ::core::fmt::Debug for $typename {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                f.write_str(concat!(stringify!($typename), "("))?;
                match self.assoc_const_name() {
                    Some(name) => f.write_str(&name)?,
                    None => ::core::fmt::Debug::fmt(&self.0, f)?,
                }
                f.write_str(")")
            }
        }
    }
}

/// Status type indicating the result of a Fuchsia syscall.
///
/// This type is generally used to indicate the reason for an error.
/// While this type can contain `Status::OK` (`ZX_OK` in C land), elements of this type are
/// generally constructed using the `ok` method, which checks for `ZX_OK` and returns a
/// `Result<(), Status>` appropriately.
#[derive(Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Hash)]
#[repr(transparent)]
pub struct Status(sys::zx_status_t);
impl Status {
    /// Returns `Ok(())` if the status was `OK`,
    /// otherwise returns `Err(status)`.
    pub fn ok(raw: sys::zx_status_t) -> Result<(), Status> {
        if raw == Status::OK.0 { Ok(()) } else { Err(Status(raw)) }
    }

    pub fn from_raw(raw: sys::zx_status_t) -> Self {
        Status(raw)
    }

    pub fn into_raw(self) -> sys::zx_status_t {
        self.0
    }
}

/// Convenience re-export of `Status::ok`.
pub fn ok(raw: sys::zx_status_t) -> Result<(), Status> {
    Status::ok(raw)
}

// LINT.IfChange(zx_status_t)
assoc_values!(Status, [
    #[doc = "Indicates an operation was successful."]
    OK                     = sys::ZX_OK;
    #[doc = "The system encountered an otherwise unspecified error while performing the"]
    #[doc = "operation."]
    INTERNAL               = sys::ZX_ERR_INTERNAL;
    #[doc = "The operation is not implemented, supported, or enabled."]
    NOT_SUPPORTED          = sys::ZX_ERR_NOT_SUPPORTED;
    #[doc = "The system was not able to allocate some resource needed for the operation."]
    NO_RESOURCES           = sys::ZX_ERR_NO_RESOURCES;
    #[doc = "The system was not able to allocate memory needed for the operation."]
    NO_MEMORY              = sys::ZX_ERR_NO_MEMORY;
    #[doc = "The system call was interrupted, but should be retried. This should not be"]
    #[doc = "seen outside of the VDSO."]
    INTERRUPTED_RETRY      = sys::ZX_ERR_INTERRUPTED_RETRY;
    #[doc = "An argument is invalid. For example, a null pointer when a null pointer is"]
    #[doc = "not permitted."]
    INVALID_ARGS           = sys::ZX_ERR_INVALID_ARGS;
    #[doc = "A specified handle value does not refer to a handle."]
    BAD_HANDLE             = sys::ZX_ERR_BAD_HANDLE;
    #[doc = "The subject of the operation is the wrong type to perform the operation."]
    #[doc = ""]
    #[doc = "For example: Attempting a message_read on a thread handle."]
    WRONG_TYPE             = sys::ZX_ERR_WRONG_TYPE;
    #[doc = "The specified syscall number is invalid."]
    BAD_SYSCALL            = sys::ZX_ERR_BAD_SYSCALL;
    #[doc = "An argument is outside the valid range for this operation."]
    OUT_OF_RANGE           = sys::ZX_ERR_OUT_OF_RANGE;
    #[doc = "The caller-provided buffer is too small for this operation."]
    BUFFER_TOO_SMALL       = sys::ZX_ERR_BUFFER_TOO_SMALL;
    #[doc = "The operation failed because the current state of the object does not allow"]
    #[doc = "it, or a precondition of the operation is not satisfied."]
    BAD_STATE              = sys::ZX_ERR_BAD_STATE;
    #[doc = "The time limit for the operation elapsed before the operation completed."]
    TIMED_OUT              = sys::ZX_ERR_TIMED_OUT;
    #[doc = "The operation cannot be performed currently but potentially could succeed if"]
    #[doc = "the caller waits for a prerequisite to be satisfied, like waiting for a"]
    #[doc = "handle to be readable or writable."]
    #[doc = ""]
    #[doc = "Example: Attempting to read from a channel that has no messages waiting but"]
    #[doc = "has an open remote will return `ZX_ERR_SHOULD_WAIT`. In contrast, attempting"]
    #[doc = "to read from a channel that has no messages waiting and has a closed remote"]
    #[doc = "end will return `ZX_ERR_PEER_CLOSED`."]
    SHOULD_WAIT            = sys::ZX_ERR_SHOULD_WAIT;
    #[doc = "The in-progress operation, for example, a wait, has been canceled."]
    CANCELED               = sys::ZX_ERR_CANCELED;
    #[doc = "The operation failed because the remote end of the subject of the operation"]
    #[doc = "was closed."]
    PEER_CLOSED            = sys::ZX_ERR_PEER_CLOSED;
    #[doc = "The requested entity is not found."]
    NOT_FOUND              = sys::ZX_ERR_NOT_FOUND;
    #[doc = "An object with the specified identifier already exists."]
    #[doc = ""]
    #[doc = "Example: Attempting to create a file when a file already exists with that"]
    #[doc = "name."]
    ALREADY_EXISTS         = sys::ZX_ERR_ALREADY_EXISTS;
    #[doc = "The operation failed because the named entity is already owned or controlled"]
    #[doc = "by another entity. The operation could succeed later if the current owner"]
    #[doc = "releases the entity."]
    ALREADY_BOUND          = sys::ZX_ERR_ALREADY_BOUND;
    #[doc = "The subject of the operation is currently unable to perform the operation."]
    #[doc = ""]
    #[doc = "This is used when there's no direct way for the caller to observe when the"]
    #[doc = "subject will be able to perform the operation and should thus retry."]
    UNAVAILABLE            = sys::ZX_ERR_UNAVAILABLE;
    #[doc = "The caller did not have permission to perform the specified operation."]
    ACCESS_DENIED          = sys::ZX_ERR_ACCESS_DENIED;
    #[doc = "Otherwise-unspecified error occurred during I/O."]
    IO                     = sys::ZX_ERR_IO;
    #[doc = "The entity the I/O operation is being performed on rejected the operation."]
    #[doc = ""]
    #[doc = "Example: an I2C device NAK'ing a transaction or a disk controller rejecting"]
    #[doc = "an invalid command, or a stalled USB endpoint."]
    IO_REFUSED             = sys::ZX_ERR_IO_REFUSED;
    #[doc = "The data in the operation failed an integrity check and is possibly"]
    #[doc = "corrupted."]
    #[doc = ""]
    #[doc = "Example: CRC or Parity error."]
    IO_DATA_INTEGRITY      = sys::ZX_ERR_IO_DATA_INTEGRITY;
    #[doc = "The data in the operation is currently unavailable and may be permanently"]
    #[doc = "lost."]
    #[doc = ""]
    #[doc = "Example: A disk block is irrecoverably damaged."]
    IO_DATA_LOSS           = sys::ZX_ERR_IO_DATA_LOSS;
    #[doc = "The device is no longer available (has been unplugged from the system,"]
    #[doc = "powered down, or the driver has been unloaded)."]
    IO_NOT_PRESENT         = sys::ZX_ERR_IO_NOT_PRESENT;
    #[doc = "More data was received from the device than expected."]
    #[doc = ""]
    #[doc = "Example: a USB \"babble\" error due to a device sending more data than the"]
    #[doc = "host queued to receive."]
    IO_OVERRUN             = sys::ZX_ERR_IO_OVERRUN;
    #[doc = "An operation did not complete within the required timeframe."]
    #[doc = ""]
    #[doc = "Example: A USB isochronous transfer that failed to complete due to an"]
    #[doc = "overrun or underrun."]
    IO_MISSED_DEADLINE     = sys::ZX_ERR_IO_MISSED_DEADLINE;
    #[doc = "The data in the operation is invalid parameter or is out of range."]
    #[doc = ""]
    #[doc = "Example: A USB transfer that failed to complete with TRB Error"]
    IO_INVALID             = sys::ZX_ERR_IO_INVALID;
    #[doc = "Path name is too long."]
    BAD_PATH               = sys::ZX_ERR_BAD_PATH;
    #[doc = "The object is not a directory or does not support directory operations."]
    #[doc = ""]
    #[doc = "Example: Attempted to open a file as a directory or attempted to do"]
    #[doc = "directory operations on a file."]
    NOT_DIR                = sys::ZX_ERR_NOT_DIR;
    #[doc = "Object is not a regular file."]
    NOT_FILE               = sys::ZX_ERR_NOT_FILE;
    #[doc = "This operation would cause a file to exceed a filesystem-specific size"]
    #[doc = "limit."]
    FILE_BIG               = sys::ZX_ERR_FILE_BIG;
    #[doc = "The filesystem or device space is exhausted."]
    NO_SPACE               = sys::ZX_ERR_NO_SPACE;
    #[doc = "The directory is not empty for an operation that requires it to be empty."]
    #[doc = ""]
    #[doc = "For example, non-recursively deleting a directory with files still in it."]
    NOT_EMPTY              = sys::ZX_ERR_NOT_EMPTY;
    #[doc = "An indicate to not call again."]
    #[doc = ""]
    #[doc = "The flow control values `ZX_ERR_STOP`, `ZX_ERR_NEXT`, and `ZX_ERR_ASYNC` are"]
    #[doc = "not errors and will never be returned by a system call or public API. They"]
    #[doc = "allow callbacks to request their caller perform some other operation."]
    #[doc = ""]
    #[doc = "For example, a callback might be called on every event until it returns"]
    #[doc = "something other than `ZX_OK`. This status allows differentiation between"]
    #[doc = "\"stop due to an error\" and \"stop because work is done.\""]
    STOP                   = sys::ZX_ERR_STOP;
    #[doc = "Advance to the next item."]
    #[doc = ""]
    #[doc = "The flow control values `ZX_ERR_STOP`, `ZX_ERR_NEXT`, and `ZX_ERR_ASYNC` are"]
    #[doc = "not errors and will never be returned by a system call or public API. They"]
    #[doc = "allow callbacks to request their caller perform some other operation."]
    #[doc = ""]
    #[doc = "For example, a callback could use this value to indicate it did not consume"]
    #[doc = "an item passed to it, but by choice, not due to an error condition."]
    NEXT                   = sys::ZX_ERR_NEXT;
    #[doc = "Ownership of the item has moved to an asynchronous worker."]
    #[doc = ""]
    #[doc = "The flow control values `ZX_ERR_STOP`, `ZX_ERR_NEXT`, and `ZX_ERR_ASYNC` are"]
    #[doc = "not errors and will never be returned by a system call or public API. They"]
    #[doc = "allow callbacks to request their caller perform some other operation."]
    #[doc = ""]
    #[doc = "Unlike `ZX_ERR_STOP`, which implies that iteration on an object"]
    #[doc = "should stop, and `ZX_ERR_NEXT`, which implies that iteration"]
    #[doc = "should continue to the next item, `ZX_ERR_ASYNC` implies"]
    #[doc = "that an asynchronous worker is responsible for continuing iteration."]
    #[doc = ""]
    #[doc = "For example, a callback will be called on every event, but one event needs"]
    #[doc = "to handle some work asynchronously before it can continue. `ZX_ERR_ASYNC`"]
    #[doc = "implies the worker is responsible for resuming iteration once its work has"]
    #[doc = "completed."]
    ASYNC                  = sys::ZX_ERR_ASYNC;
    #[doc = "The specified protocol is not supported."]
    PROTOCOL_NOT_SUPPORTED = sys::ZX_ERR_PROTOCOL_NOT_SUPPORTED;
    #[doc = "The host is unreachable."]
    ADDRESS_UNREACHABLE    = sys::ZX_ERR_ADDRESS_UNREACHABLE;
    #[doc = "Address is being used by someone else."]
    ADDRESS_IN_USE         = sys::ZX_ERR_ADDRESS_IN_USE;
    #[doc = "The socket is not connected."]
    NOT_CONNECTED          = sys::ZX_ERR_NOT_CONNECTED;
    #[doc = "The remote peer rejected the connection."]
    CONNECTION_REFUSED     = sys::ZX_ERR_CONNECTION_REFUSED;
    #[doc = "The connection was reset."]
    CONNECTION_RESET       = sys::ZX_ERR_CONNECTION_RESET;
    #[doc = "The connection was aborted."]
    CONNECTION_ABORTED     = sys::ZX_ERR_CONNECTION_ABORTED;
]);
// LINT.ThenChange(//zircon/vdso/errors.fidl)

impl Status {
    pub fn from_result(res: Result<(), Self>) -> Self {
        res.into()
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.assoc_const_name() {
            Some(name) => name.fmt(f),
            None => write!(f, "Unknown zircon status code: {}", self.0),
        }
    }
}

impl core::error::Error for Status {}

impl From<Result<(), Status>> for Status {
    fn from(res: Result<(), Status>) -> Status {
        match res {
            Ok(()) => Self::OK,
            Err(status) => status,
        }
    }
}

impl From<Status> for Result<(), Status> {
    fn from(src: Status) -> Result<(), Status> {
        Status::ok(src.into_raw())
    }
}

impl From<core::convert::Infallible> for Status {
    fn from(x: core::convert::Infallible) -> Status {
        match x {}
    }
}

/// A non-zero Zircon status code representing an error.
///
/// Because this wraps a `NonZero<sys::zx_status_t>`, `Result<T, ErrorStatus>` has a niche at `0`
/// (`ZX_OK`), guaranteeing that `Result<(), ErrorStatus>` has the exact same 4-byte memory layout
/// and machine ABI as `sys::zx_status_t` (`Status`).
#[derive(Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Hash)]
#[repr(transparent)]
pub struct ErrorStatus(core::num::NonZero<sys::zx_status_t>);

impl ErrorStatus {
    pub fn from_raw(raw: sys::zx_status_t) -> Option<Self> {
        core::num::NonZero::new(raw).map(ErrorStatus)
    }

    pub fn into_raw(self) -> sys::zx_status_t {
        self.0.get()
    }

    pub fn ok(raw: sys::zx_status_t) -> Result<(), Self> {
        match core::num::NonZero::new(raw) {
            Some(err) => Err(ErrorStatus(err)),
            None => Ok(()),
        }
    }
}

impl From<Status> for ErrorStatus {
    #[inline]
    fn from(status: Status) -> Self {
        ErrorStatus(
            core::num::NonZero::new(status.into_raw())
                .expect("Attempted to convert Status::OK into ErrorStatus"),
        )
    }
}

impl From<ErrorStatus> for Status {
    #[inline]
    fn from(err: ErrorStatus) -> Self {
        Status::from_raw(err.0.get())
    }
}

impl fmt::Debug for ErrorStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&Status::from_raw(self.0.get()), f)
    }
}

impl fmt::Display for ErrorStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&Status::from_raw(self.0.get()), f)
    }
}

impl core::error::Error for ErrorStatus {}

#[cfg(test)]
mod test {
    extern crate std;
    use super::Status;

    #[test]
    fn status_debug_format() {
        let cases = [
            ("Status(OK)", Status::OK),
            ("Status(BAD_SYSCALL)", Status::BAD_SYSCALL),
            ("Status(NEXT)", Status::NEXT),
            ("Status(-5050)", Status(-5050)),
        ];
        for &(expected, value) in &cases {
            assert_eq!(expected, std::format!("{:?}", value));
        }
    }

    #[test]
    fn status_into_result() {
        let ok_result: Result<(), Status> = Status::OK.into();
        assert_eq!(ok_result, Ok(()));

        let err_result: Result<(), Status> = Status::BAD_SYSCALL.into();
        assert_eq!(err_result, Err(Status::BAD_SYSCALL));
    }

    #[test]
    fn error_status_conversions() {
        let err_res: Result<(), super::ErrorStatus> = Err(Status::BAD_SYSCALL.into());
        assert_eq!(err_res, Err(Status::BAD_SYSCALL.into()));
    }
}
