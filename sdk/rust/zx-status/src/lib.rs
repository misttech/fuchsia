// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Type-safe bindings for Zircon status.

use std::ffi::NulError;
use std::{error, fmt, io};
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
                        $(#[$attr])*
                        $value => Some(stringify!($name)),
                    )*
                    _ => None,
                }
            }
        }

        impl ::std::fmt::Debug for $typename {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(concat!(stringify!($typename), "("))?;
                match self.assoc_const_name() {
                    Some(name) => f.write_str(&name)?,
                    None => ::std::fmt::Debug::fmt(&self.0, f)?,
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
        if raw == Status::OK.0 {
            Ok(())
        } else {
            Err(Status(raw))
        }
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
    // Indicates an operation was successful.
    OK                     = sys::ZX_OK;
    // The system encountered an otherwise unspecified error while performing the
    // operation.
    INTERNAL               = sys::ZX_ERR_INTERNAL;
    // The operation is not implemented, supported, or enabled.
    NOT_SUPPORTED          = sys::ZX_ERR_NOT_SUPPORTED;
    // The system was not able to allocate some resource needed for the operation.
    NO_RESOURCES           = sys::ZX_ERR_NO_RESOURCES;
    // The system was not able to allocate memory needed for the operation.
    NO_MEMORY              = sys::ZX_ERR_NO_MEMORY;
    // The system call was interrupted, but should be retried. This should not be
    // seen outside of the VDSO.
    INTERRUPTED_RETRY      = sys::ZX_ERR_INTERRUPTED_RETRY;
    // An argument is invalid. For example, a null pointer when a null pointer is
    // not permitted.
    INVALID_ARGS           = sys::ZX_ERR_INVALID_ARGS;
    // A specified handle value does not refer to a handle.
    BAD_HANDLE             = sys::ZX_ERR_BAD_HANDLE;
    // The subject of the operation is the wrong type to perform the operation.
    //
    // For example: Attempting a message_read on a thread handle.
    WRONG_TYPE             = sys::ZX_ERR_WRONG_TYPE;
    // The specified syscall number is invalid.
    BAD_SYSCALL            = sys::ZX_ERR_BAD_SYSCALL;
    // An argument is outside the valid range for this operation.
    OUT_OF_RANGE           = sys::ZX_ERR_OUT_OF_RANGE;
    // The caller-provided buffer is too small for this operation.
    BUFFER_TOO_SMALL       = sys::ZX_ERR_BUFFER_TOO_SMALL;
    // The operation failed because the current state of the object does not allow
    // it, or a precondition of the operation is not satisfied.
    BAD_STATE              = sys::ZX_ERR_BAD_STATE;
    // The time limit for the operation elapsed before the operation completed.
    TIMED_OUT              = sys::ZX_ERR_TIMED_OUT;
    // The operation cannot be performed currently but potentially could succeed if
    // the caller waits for a prerequisite to be satisfied, like waiting for a
    // handle to be readable or writable.
    //
    // Example: Attempting to read from a channel that has no messages waiting but
    // has an open remote will return `ZX_ERR_SHOULD_WAIT`. In contrast, attempting
    // to read from a channel that has no messages waiting and has a closed remote
    // end will return `ZX_ERR_PEER_CLOSED`.
    SHOULD_WAIT            = sys::ZX_ERR_SHOULD_WAIT;
    // The in-progress operation, for example, a wait, has been canceled.
    CANCELED               = sys::ZX_ERR_CANCELED;
    // The operation failed because the remote end of the subject of the operation
    // was closed.
    PEER_CLOSED            = sys::ZX_ERR_PEER_CLOSED;
    // The requested entity is not found.
    NOT_FOUND              = sys::ZX_ERR_NOT_FOUND;
    // An object with the specified identifier already exists.
    //
    // Example: Attempting to create a file when a file already exists with that
    // name.
    ALREADY_EXISTS         = sys::ZX_ERR_ALREADY_EXISTS;
    // The operation failed because the named entity is already owned or controlled
    // by another entity. The operation could succeed later if the current owner
    // releases the entity.
    ALREADY_BOUND          = sys::ZX_ERR_ALREADY_BOUND;
    // The subject of the operation is currently unable to perform the operation.
    //
    // This is used when there's no direct way for the caller to observe when the
    // subject will be able to perform the operation and should thus retry.
    UNAVAILABLE            = sys::ZX_ERR_UNAVAILABLE;
    // The caller did not have permission to perform the specified operation.
    ACCESS_DENIED          = sys::ZX_ERR_ACCESS_DENIED;
    // Otherwise-unspecified error occurred during I/O.
    IO                     = sys::ZX_ERR_IO;
    // The entity the I/O operation is being performed on rejected the operation.
    //
    // Example: an I2C device NAK'ing a transaction or a disk controller rejecting
    // an invalid command, or a stalled USB endpoint.
    IO_REFUSED             = sys::ZX_ERR_IO_REFUSED;
    // The data in the operation failed an integrity check and is possibly
    // corrupted.
    //
    // Example: CRC or Parity error.
    IO_DATA_INTEGRITY      = sys::ZX_ERR_IO_DATA_INTEGRITY;
    // The data in the operation is currently unavailable and may be permanently
    // lost.
    //
    // Example: A disk block is irrecoverably damaged.
    IO_DATA_LOSS           = sys::ZX_ERR_IO_DATA_LOSS;
    // The device is no longer available (has been unplugged from the system,
    // powered down, or the driver has been unloaded).
    IO_NOT_PRESENT         = sys::ZX_ERR_IO_NOT_PRESENT;
    // More data was received from the device than expected.
    //
    // Example: a USB "babble" error due to a device sending more data than the
    // host queued to receive.
    IO_OVERRUN             = sys::ZX_ERR_IO_OVERRUN;
    // An operation did not complete within the required timeframe.
    //
    // Example: A USB isochronous transfer that failed to complete due to an
    // overrun or underrun.
    IO_MISSED_DEADLINE     = sys::ZX_ERR_IO_MISSED_DEADLINE;
    // The data in the operation is invalid parameter or is out of range.
    //
    // Example: A USB transfer that failed to complete with TRB Error
    IO_INVALID             = sys::ZX_ERR_IO_INVALID;
    // Path name is too long.
    BAD_PATH               = sys::ZX_ERR_BAD_PATH;
    // The object is not a directory or does not support directory operations.
    //
    // Example: Attempted to open a file as a directory or attempted to do
    // directory operations on a file.
    NOT_DIR                = sys::ZX_ERR_NOT_DIR;
    // Object is not a regular file.
    NOT_FILE               = sys::ZX_ERR_NOT_FILE;
    // This operation would cause a file to exceed a filesystem-specific size
    // limit.
    FILE_BIG               = sys::ZX_ERR_FILE_BIG;
    // The filesystem or device space is exhausted.
    NO_SPACE               = sys::ZX_ERR_NO_SPACE;
    // The directory is not empty for an operation that requires it to be empty.
    //
    // For example, non-recursively deleting a directory with files still in it.
    NOT_EMPTY              = sys::ZX_ERR_NOT_EMPTY;
    // An indicate to not call again.
    //
    // The flow control values `ZX_ERR_STOP`, `ZX_ERR_NEXT`, and `ZX_ERR_ASYNC` are
    // not errors and will never be returned by a system call or public API. They
    // allow callbacks to request their caller perform some other operation.
    //
    // For example, a callback might be called on every event until it returns
    // something other than `ZX_OK`. This status allows differentiation between
    // "stop due to an error" and "stop because work is done."
    STOP                   = sys::ZX_ERR_STOP;
    // Advance to the next item.
    //
    // The flow control values `ZX_ERR_STOP`, `ZX_ERR_NEXT`, and `ZX_ERR_ASYNC` are
    // not errors and will never be returned by a system call or public API. They
    // allow callbacks to request their caller perform some other operation.
    //
    // For example, a callback could use this value to indicate it did not consume
    // an item passed to it, but by choice, not due to an error condition.
    NEXT                   = sys::ZX_ERR_NEXT;
    // Ownership of the item has moved to an asynchronous worker.
    //
    // The flow control values `ZX_ERR_STOP`, `ZX_ERR_NEXT`, and `ZX_ERR_ASYNC` are
    // not errors and will never be returned by a system call or public API. They
    // allow callbacks to request their caller perform some other operation.
    //
    // Unlike `ZX_ERR_STOP`, which implies that iteration on an object
    // should stop, and `ZX_ERR_NEXT`, which implies that iteration
    // should continue to the next item, `ZX_ERR_ASYNC` implies
    // that an asynchronous worker is responsible for continuing iteration.
    //
    // For example, a callback will be called on every event, but one event needs
    // to handle some work asynchronously before it can continue. `ZX_ERR_ASYNC`
    // implies the worker is responsible for resuming iteration once its work has
    // completed.
    ASYNC                  = sys::ZX_ERR_ASYNC;
    // The specified protocol is not supported.
    PROTOCOL_NOT_SUPPORTED = sys::ZX_ERR_PROTOCOL_NOT_SUPPORTED;
    // The host is unreachable.
    ADDRESS_UNREACHABLE    = sys::ZX_ERR_ADDRESS_UNREACHABLE;
    // Address is being used by someone else.
    ADDRESS_IN_USE         = sys::ZX_ERR_ADDRESS_IN_USE;
    // The socket is not connected.
    NOT_CONNECTED          = sys::ZX_ERR_NOT_CONNECTED;
    // The remote peer rejected the connection.
    CONNECTION_REFUSED     = sys::ZX_ERR_CONNECTION_REFUSED;
    // The connection was reset.
    CONNECTION_RESET       = sys::ZX_ERR_CONNECTION_RESET;
    // The connection was aborted.
    CONNECTION_ABORTED     = sys::ZX_ERR_CONNECTION_ABORTED;
]);
// LINT.ThenChange(//zircon/vdso/errors.fidl)

impl Status {
    pub fn into_io_error(self) -> io::Error {
        self.into()
    }

    pub fn from_result(res: Result<(), Self>) -> Self {
        res.into()
    }
}

impl From<io::ErrorKind> for Status {
    fn from(kind: io::ErrorKind) -> Self {
        use std::io::ErrorKind::*;
        match kind {
            NotFound => Status::NOT_FOUND,
            PermissionDenied => Status::ACCESS_DENIED,
            ConnectionRefused => Status::IO_REFUSED,
            ConnectionAborted => Status::PEER_CLOSED,
            AddrInUse => Status::ALREADY_BOUND,
            AddrNotAvailable => Status::UNAVAILABLE,
            BrokenPipe => Status::PEER_CLOSED,
            AlreadyExists => Status::ALREADY_EXISTS,
            WouldBlock => Status::SHOULD_WAIT,
            InvalidInput => Status::INVALID_ARGS,
            TimedOut => Status::TIMED_OUT,
            Interrupted => Status::INTERRUPTED_RETRY,
            UnexpectedEof | WriteZero | ConnectionReset | NotConnected | Other | _ => Status::IO,
        }
    }
}

impl From<Status> for io::ErrorKind {
    fn from(status: Status) -> io::ErrorKind {
        use std::io::ErrorKind::*;
        match status {
            Status::INTERRUPTED_RETRY => Interrupted,
            Status::BAD_HANDLE => BrokenPipe,
            Status::TIMED_OUT => TimedOut,
            Status::SHOULD_WAIT => WouldBlock,
            Status::PEER_CLOSED => ConnectionAborted,
            Status::NOT_FOUND => NotFound,
            Status::ALREADY_EXISTS => AlreadyExists,
            Status::ALREADY_BOUND => AlreadyExists,
            Status::UNAVAILABLE => AddrNotAvailable,
            Status::ACCESS_DENIED => PermissionDenied,
            Status::IO_REFUSED => ConnectionRefused,
            Status::IO_DATA_INTEGRITY => InvalidData,

            Status::BAD_PATH | Status::INVALID_ARGS | Status::OUT_OF_RANGE | Status::WRONG_TYPE => {
                InvalidInput
            }

            Status::OK
            | Status::NEXT
            | Status::STOP
            | Status::NO_SPACE
            | Status::FILE_BIG
            | Status::NOT_FILE
            | Status::NOT_DIR
            | Status::IO_DATA_LOSS
            | Status::IO
            | Status::CANCELED
            | Status::BAD_STATE
            | Status::BUFFER_TOO_SMALL
            | Status::BAD_SYSCALL
            | Status::INTERNAL
            | Status::NOT_SUPPORTED
            | Status::NO_RESOURCES
            | Status::NO_MEMORY
            | _ => Other,
        }
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

impl error::Error for Status {}

impl From<io::Error> for Status {
    fn from(err: io::Error) -> Status {
        err.kind().into()
    }
}

impl From<Status> for io::Error {
    fn from(status: Status) -> io::Error {
        io::Error::from(io::ErrorKind::from(status))
    }
}

impl From<NulError> for Status {
    fn from(_error: NulError) -> Status {
        Status::INVALID_ARGS
    }
}

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

#[cfg(test)]
mod test {
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
            assert_eq!(expected, format!("{:?}", value));
        }
    }

    #[test]
    fn status_into_result() {
        let ok_result: Result<(), Status> = Status::OK.into();
        assert_eq!(ok_result, Ok(()));

        let err_result: Result<(), Status> = Status::BAD_SYSCALL.into();
        assert_eq!(err_result, Err(Status::BAD_SYSCALL));
    }
}
