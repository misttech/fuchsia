// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::task::{CurrentTask, EventHandler, WaitCanceler, Waiter};
use crate::vfs::buffers::{AncillaryData, InputBuffer, MessageReadInfo, OutputBuffer};
use crate::vfs::socket::{
    Socket, SocketAddress, SocketHandle, SocketMessageFlags, SocketOps, SocketPeer,
    SocketShutdownFlags, SocketType,
};
use starnix_logging::track_stub;
use starnix_sync::{FileOpsCore, Locked};
use starnix_uapi::errors::Errno;
use starnix_uapi::vfs::FdEvents;
use starnix_uapi::{error, ucred};

pub struct QipcrtrSocket {}

impl QipcrtrSocket {
    pub fn new(_socket_type: SocketType) -> Self {
        Self {}
    }
}

impl SocketOps for QipcrtrSocket {
    fn connect(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _socket: &SocketHandle,
        _current_task: &CurrentTask,
        _peer: SocketPeer,
    ) -> Result<(), Errno> {
        track_stub!(TODO("https://fxbug.dev/388082019"), "QipcrtrSocket::connect");
        error!(EINVAL)
    }

    fn listen(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _socket: &Socket,
        _backlog: i32,
        _credentials: ucred,
    ) -> Result<(), Errno> {
        track_stub!(TODO("https://fxbug.dev/388082019"), "QipcrtrSocket::listen");
        error!(EINVAL)
    }

    fn accept(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _socket: &Socket,
    ) -> Result<SocketHandle, Errno> {
        track_stub!(TODO("https://fxbug.dev/388082019"), "QipcrtrSocket::accept");
        error!(EINVAL)
    }

    fn bind(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _socket: &Socket,
        _current_task: &CurrentTask,
        _socket_address: SocketAddress,
    ) -> Result<(), Errno> {
        track_stub!(TODO("https://fxbug.dev/388082019"), "QipcrtrSocket::bind");
        error!(EINVAL)
    }

    fn read(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _socket: &Socket,
        _current_task: &CurrentTask,
        _data: &mut dyn OutputBuffer,
        _flags: SocketMessageFlags,
    ) -> Result<MessageReadInfo, Errno> {
        track_stub!(TODO("https://fxbug.dev/388082019"), "QipcrtrSocket::read");
        error!(EINVAL)
    }

    fn write(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _socket: &Socket,
        _current_task: &CurrentTask,
        _data: &mut dyn InputBuffer,
        _dest_address: &mut Option<SocketAddress>,
        _ancillary_data: &mut Vec<AncillaryData>,
    ) -> Result<usize, Errno> {
        track_stub!(TODO("https://fxbug.dev/388082019"), "QipcrtrSocket::write");
        error!(EINVAL)
    }

    fn wait_async(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _socket: &Socket,
        _current_task: &CurrentTask,
        _waiter: &Waiter,
        _events: FdEvents,
        _handler: EventHandler,
    ) -> WaitCanceler {
        track_stub!(TODO("https://fxbug.dev/388082019"), "QipcrtrSocket::wait_async");
        WaitCanceler::new_noop()
    }

    fn query_events(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _socket: &Socket,
        _current_task: &CurrentTask,
    ) -> Result<FdEvents, Errno> {
        track_stub!(TODO("https://fxbug.dev/388082019"), "QipcrtrSocket::query_events");
        error!(EINVAL)
    }

    fn shutdown(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _socket: &Socket,
        _how: SocketShutdownFlags,
    ) -> Result<(), Errno> {
        track_stub!(TODO("https://fxbug.dev/388082019"), "QipcrtrSocket::shutdown");
        error!(EINVAL)
    }

    fn close(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _current_task: &CurrentTask,
        _socket: &Socket,
    ) {
        track_stub!(TODO("https://fxbug.dev/388082019"), "QipcrtrSocket::close");
    }

    fn getsockname(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _socket: &Socket,
    ) -> Result<SocketAddress, Errno> {
        track_stub!(TODO("https://fxbug.dev/388082019"), "QipcrtrSocket::getsockname");
        error!(EINVAL)
    }

    fn getpeername(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _socket: &Socket,
    ) -> Result<SocketAddress, Errno> {
        track_stub!(TODO("https://fxbug.dev/388082019"), "QipcrtrSocket::getpeername");
        error!(EINVAL)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::spawn_kernel_and_run;
    use crate::vfs::socket::{SocketDomain, SocketProtocol, SocketType};

    #[::fuchsia::test]
    async fn test_qipcrtr_socket_new() {
        spawn_kernel_and_run(async |locked, current_task| {
            let _kernel = current_task.kernel();
            let _socket = Socket::new(
                locked,
                &current_task,
                SocketDomain::Qipcrtr,
                SocketType::Datagram,
                SocketProtocol::default(),
                /* kernel_private = */ false,
            )
            .expect("Failed to create socket.");
        })
        .await;
    }
}
