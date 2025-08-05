// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_ffx_usb__common::{
    self as usb_fidl, control_ordinals, ffx_usb_ordinals, list_devices_ordinals,
};
use futures::channel::mpsc;
use futures::future::{select, Either, LocalBoxFuture};
use futures::lock::Mutex as AsyncMutex;
use futures::{FutureExt, SinkExt, StreamExt};
use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::num::NonZero;
use std::path::PathBuf;
use std::pin::{pin, Pin};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use usb_vsock_host::{IncomingConnection, UsbVsockHost, UsbVsockHostEvent};

mod adapters;

use adapters::WrapStream;

const CURRENT_VERSION: u32 = 0;

trait WriteWithLengthPrefix: AsyncWriteExt + Unpin {
    async fn write_with_length(&mut self, message: &[u8]) -> std::io::Result<()> {
        self.write_all(&u32::try_from(message.len()).unwrap().to_le_bytes()).await?;
        self.write_all(message).await
    }
}

impl<T: AsyncWriteExt + Unpin> WriteWithLengthPrefix for T {}

/// Represents an active port listen.
struct Listener {
    queue: VecDeque<IncomingConnection<WrapStream>>,
    session_id: u64,
    cancel_waker: Waker,
    is_cancelled: bool,
}

impl Listener {
    fn cancel(&mut self) {
        self.is_cancelled = true;
        self.cancel_waker.wake_by_ref();
    }

    fn remove(&mut self, cid: u32, port: u32) -> Option<IncomingConnection<WrapStream>> {
        if let Some((listener_idx, _)) = self
            .queue
            .iter()
            .enumerate()
            .find(|(_i, x)| x.address().device_cid == cid && x.address().device_port == port)
        {
            self.queue.remove(listener_idx)
        } else {
            None
        }
    }
}

/// A future that waits for a listen to be cancelled.
struct ListenerCancelWaiter {
    listeners: Arc<ListenerTable>,
    session_id: u64,
    port: u32,
}

impl Future for ListenerCancelWaiter {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut listeners = self.listeners.0.lock().unwrap();
        let Some(listener) = listeners.get_mut(&self.port) else {
            return Poll::Ready(());
        };

        listener.cancel_waker = cx.waker().clone();
        if listener.is_cancelled || listener.session_id != self.session_id {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

/// Table of listeners on various ports.
struct ListenerTable(Mutex<HashMap<u32, Listener>>);

impl ListenerTable {
    /// Remove and return an incoming connection.
    fn take_connection(
        &self,
        session_id: u64,
        conn: usb_fidl::ConnectionId,
    ) -> Option<IncomingConnection<WrapStream>> {
        let mut listener_table = self.0.lock().unwrap();
        let listeners = listener_table.get_mut(&conn.local_port);
        listeners.and_then(|listeners| {
            if listeners.session_id == session_id {
                listeners.remove(conn.remote_cid, conn.remote_port)
            } else {
                None
            }
        })
    }

    /// Cancel all listeners associated with the given session.
    fn cancel_session(&self, session_id: u64) {
        let mut listeners = self.0.lock().unwrap();
        for (_, listener) in &mut *listeners {
            if listener.session_id == session_id {
                listener.cancel();
            }
        }
    }

    /// Cancel a listen on a given port in the given session.
    fn cancel_port_listen(
        &self,
        session_id: u64,
        port: u32,
    ) -> Result<(), usb_fidl::StopListenError> {
        let mut listener_table = self.0.lock().unwrap();
        if let Some(listeners) =
            listener_table.get_mut(&port).filter(|x| x.session_id == session_id)
        {
            listeners.cancel();
            Ok(())
        } else {
            Err(usb_fidl::StopListenError::NotListening(usb_fidl::NotListening { port }))
        }
    }

    /// Initialize listening for the given session ID and port.
    fn init_listener(&self, session_id: u64, port: u32) {
        let mut listeners = self.0.lock().unwrap();
        match listeners.entry(port) {
            Entry::Occupied(mut e) => {
                let e = e.get_mut();
                if e.is_cancelled {
                    e.session_id = session_id;
                    e.is_cancelled = false;
                    e.queue.clear();
                    e.cancel_waker.wake_by_ref();
                } else if session_id != e.session_id {
                    panic!("Driver let us listen on a port that another session was listening on!");
                } else {
                    panic!("Driver let us establish two listens on the same port!");
                }
            }
            Entry::Vacant(e) => {
                e.insert(Listener {
                    queue: VecDeque::new(),
                    cancel_waker: Waker::noop().clone(),
                    session_id,
                    is_cancelled: false,
                });
            }
        }
    }

    /// Add an incoming connection to the appropriate table entry.
    fn add_incoming(&self, session_id: u64, port: u32, incoming: IncomingConnection<WrapStream>) {
        let mut listeners = self.0.lock().unwrap();
        listeners
            .get_mut(&port)
            .filter(|x| x.session_id == session_id)
            .expect("Listener state disappeared!")
            .queue
            .push_back(incoming);
    }
}

/// Hostside driver for the FFX USB interface.
pub struct HostDriver {
    driver: Arc<UsbVsockHost<WrapStream>>,
    listeners: Arc<ListenerTable>,
    listener_tasks: fuchsia_async::Scope,
    new_device_listeners: AsyncMutex<Vec<mpsc::Sender<UsbVsockHostEvent>>>,
}

impl HostDriver {
    /// Create a new [`HostDriver`] and listen for users at the given socket path.
    pub async fn run(socket_path: PathBuf) {
        let listener = match UnixListener::bind(socket_path) {
            Ok(l) => l,
            Err(e) => {
                log::error!("Could not listen on provided socket path: {e}");
                return;
            }
        };

        let (sender, receiver) = mpsc::channel(1);

        HostDriver::run_with_driver_and_listener(
            UsbVsockHost::new([] as [&str; 0], true, sender),
            receiver,
            listener,
        )
        .await;
    }

    async fn run_with_driver_and_listener(
        driver: Arc<UsbVsockHost<WrapStream>>,
        mut events: mpsc::Receiver<UsbVsockHostEvent>,
        listener: UnixListener,
    ) {
        let driver = HostDriver {
            driver,
            listeners: Arc::new(ListenerTable(Mutex::new(HashMap::new()))),
            listener_tasks: fuchsia_async::Scope::new_with_name("usb_driver_listeners"),
            new_device_listeners: AsyncMutex::new(Vec::new()),
        };
        let tasks = std::sync::Mutex::new(Vec::<LocalBoxFuture<'_, ()>>::new());
        let mut task_poller = futures::future::poll_fn(|ctx| {
            let mut tasks = tasks.lock().unwrap();
            tasks.retain_mut(|x| !x.poll_unpin(ctx).is_ready());
            Poll::<()>::Pending
        });
        tasks.lock().unwrap().push(Box::pin(async {
            while let Some(event) = events.next().await {
                let mut listeners = driver.new_device_listeners.lock().await;
                for listener in &mut *listeners {
                    let _ = listener.send(event.clone()).await;
                }
            }
        }));
        loop {
            match select(std::pin::pin!(listener.accept()), &mut task_poller).await {
                Either::Right(_) => unreachable!(),
                Either::Left((Ok((stream, _addr)), _)) => {
                    tasks.lock().unwrap().push(Box::pin(async {
                        if let Err(e) = driver.handle_connection(stream).await {
                            log::error!("Connection failed: {e}");
                        }
                    }));
                }
                Either::Left((Err(e), _)) => {
                    log::error!("Socket failed: {e}");
                    return;
                }
            }
        }
    }

    /// Create a new [`HostDriver`] and listen for users at the given socket path.
    pub fn new_for_test(
        socket_path: PathBuf,
    ) -> usb_vsock_host::TestConnection<
        impl From<UnixStream> + futures::AsyncRead + futures::AsyncWrite + Send + 'static,
    > {
        let listener = UnixListener::bind(socket_path).expect("Could not create socket for test");
        let (host, conn) = usb_vsock_host::TestConnection::<WrapStream>::new();
        let fut =
            HostDriver::run_with_driver_and_listener(host.host, host.event_receiver, listener);
        conn.scope.spawn_local(fut);
        conn
    }

    /// Handle a new connection from a tool on our socket.
    async fn handle_connection(&self, mut stream: UnixStream) -> anyhow::Result<()> {
        let mut initial_message_len = [0u8; 4];
        stream.read_exact(&mut initial_message_len).await?;
        let initial_message_len = u32::from_le_bytes(initial_message_len);
        let mut buf = vec![0u8; initial_message_len as usize];
        stream.read_exact(&mut buf).await?;

        let (header, body) = fidl::encoding::decode_transaction_header(&buf)?;
        match header.ordinal {
            ffx_usb_ordinals::INITIALIZE_CONTROL => {
                self.handle_initialize_control(stream, header, body).await
            }
            ffx_usb_ordinals::INITIALIZE_LIST_DEVICES => {
                self.handle_initialize_list_devices(stream, header, body).await
            }
            ffx_usb_ordinals::INITIALIZE_CONNECT_TO => {
                self.handle_initialize_connect_to(stream, header, body).await
            }
            ffx_usb_ordinals::INITIALIZE_ACCEPT => {
                self.handle_initialize_accept(stream, header, body).await
            }
            unknown_ordinal => {
                log::warn!("Main protocol got unknown ordinal {unknown_ordinal}");
                if header.dynamic_flags().contains(fidl::encoding::DynamicFlags::FLEXIBLE) {
                    let resp = fidl_message::encode_response_flexible_unknown(header)?;
                    stream.write_with_length(&resp).await.map_err(Into::into)
                } else {
                    Ok(())
                }
            }
        }
    }

    /// Handle "InitializeAccept" messages.
    async fn handle_initialize_accept(
        &self,
        mut stream: UnixStream,
        header: fidl_message::TransactionHeader,
        body: &[u8],
    ) -> Result<(), anyhow::Error> {
        let usb_fidl::FfxUsbInitializeAcceptRequest { conn, session_id } =
            fidl_message::decode_message(header, body)?;
        let listener = self.listeners.take_connection(session_id, conn);

        let (ready, resp) = if let Some(listener) = listener {
            (Some(listener.accept_late().await?), Ok(()))
        } else {
            (None, Err(usb_fidl::AcceptError::NoSuchConnection(conn)))
        };

        let resp = fidl_message::encode_response_result(header, resp)?;
        stream.write_with_length(&resp).await?;

        if let Some(ready) = ready {
            let _conn_state = ready.finish_connect(WrapStream(stream)).await;
        }

        Ok(())
    }

    /// Handle "InitializeConnectTo" messages.
    async fn handle_initialize_connect_to(
        &self,
        mut stream: UnixStream,
        header: fidl_message::TransactionHeader,
        body: &[u8],
    ) -> Result<(), anyhow::Error> {
        let usb_fidl::FfxUsbInitializeConnectToRequest { cid, port } =
            fidl_message::decode_message(header, body)?;

        let connect_result = if let Some(cid) = NonZero::new(cid) {
            self.driver.connect_late(cid, port).await.map_err(|e| match e {
                usb_vsock_host::ConnectError::NotFound(cid) => {
                    usb_fidl::ConnectionError::CidNotFound(usb_fidl::CidNotFound { cid })
                }
                usb_vsock_host::ConnectError::Failed(error) => {
                    usb_fidl::ConnectionError::Failed(usb_fidl::Failed {
                        message: trunc_error(format!("{error}")),
                    })
                }
                usb_vsock_host::ConnectError::PortInUse(port) => {
                    usb_fidl::ConnectionError::PortInUse(usb_fidl::PortInUse { port })
                }
                usb_vsock_host::ConnectError::PortOutOfRange => {
                    usb_fidl::ConnectionError::PortOutOfRange(usb_fidl::PortOutOfRange { port })
                }
            })
        } else {
            Err(usb_fidl::ConnectionError::CidInvalid(usb_fidl::CidInvalid { cid: 0 }))
        };
        let (ready, connect_result) = match connect_result {
            Ok(x) => (Some(x), Ok(())),
            Err(e) => (None, Err(e)),
        };
        let resp = fidl_message::encode_response_result(header, connect_result)?;
        stream.write_with_length(&resp).await?;

        if let Some(ready) = ready {
            let _conn_state = ready.finish_connect(WrapStream(stream)).await;
        }

        Ok(())
    }

    /// Handle "InitializeListDevices" messages.
    async fn handle_initialize_list_devices(
        &self,
        mut stream: UnixStream,
        header: fidl_message::TransactionHeader,
        body: &[u8],
    ) -> Result<(), anyhow::Error> {
        if !body.is_empty() {
            return Err(fidl::Error::ExtraBytes.into());
        }

        let resp = fidl_message::encode_response_flexible(header, ())?;
        stream.write_with_length(&resp).await?;
        self.handle_list_devices(stream).await
    }

    /// Handle "InitializeControl" messages.
    async fn handle_initialize_control(
        &self,
        mut stream: UnixStream,
        header: fidl_message::TransactionHeader,
        body: &[u8],
    ) -> Result<(), anyhow::Error> {
        if !body.is_empty() {
            return Err(fidl::Error::ExtraBytes.into());
        }

        let session_id = rand::random();
        // TODO(429272550): Report some sort of log ID to clients so we
        // can correlate log messages with a particular driver process.
        let res = usb_fidl::FfxUsbInitializeControlResponse {
            current: CURRENT_VERSION,
            minimum: CURRENT_VERSION,
            session_id,
        };

        let resp = fidl_message::encode_response_flexible(header, res)?;
        stream.write_with_length(&resp).await?;
        let ret = self.handle_control_connection(stream, session_id).await;
        self.listeners.cancel_session(session_id);
        ret
    }

    /// Handle a "list devices" connection. This will send back events every
    /// time a new USB target becomes available or disappears.
    async fn handle_list_devices(&self, mut stream: UnixStream) -> anyhow::Result<()> {
        let (event_sender, mut events) = mpsc::channel(1);
        self.new_device_listeners.lock().await.push(event_sender);

        let existing = self.driver.active_conns();
        for cid in existing.iter().copied() {
            let header = fidl::encoding::TransactionHeader::new(
                0,
                list_devices_ordinals::ON_DEVICE_APPEARED,
                fidl::encoding::DynamicFlags::empty(),
            );
            let body = usb_fidl::DeviceInfo { cid, meta: usb_fidl::DeviceMeta::default() };
            let data = fidl_message::encode_message(header, body)?;
            stream.write_with_length(&data).await?;
        }

        let mut existing = Some(existing);

        while let Some(event) = events.next().await {
            let data = match event {
                UsbVsockHostEvent::AddedCid(cid) => {
                    if let Some(existing_ref) = &existing {
                        if existing_ref.iter().copied().any(|x| x == cid) {
                            continue;
                        } else {
                            existing = None;
                        }
                    }
                    let header = fidl::encoding::TransactionHeader::new(
                        0,
                        list_devices_ordinals::ON_DEVICE_APPEARED,
                        fidl::encoding::DynamicFlags::empty(),
                    );
                    let body = usb_fidl::DeviceInfo { cid, meta: usb_fidl::DeviceMeta::default() };
                    fidl_message::encode_message(header, body)?
                }
                UsbVsockHostEvent::RemovedCid(cid) => {
                    if let Some(existing_ref) = &existing {
                        if existing_ref.iter().copied().any(|x| x == cid) {
                            existing = None;
                        } else {
                            continue;
                        }
                    }
                    let header = fidl::encoding::TransactionHeader::new(
                        0,
                        list_devices_ordinals::ON_DEVICE_DISAPPEARED,
                        fidl::encoding::DynamicFlags::empty(),
                    );
                    let body = usb_fidl::ListDevicesOnDeviceDisappearedRequest { cid };
                    fidl_message::encode_message(header, body)?
                }
            };
            stream.write_with_length(&data).await?;
        }
        Ok(())
    }

    /// Handle a control connection. This connection can be used to listen on
    /// ports and accept or reject incoming connections.
    async fn handle_control_connection(
        &self,
        mut stream: UnixStream,
        session_id: u64,
    ) -> anyhow::Result<()> {
        let _ = session_id;
        let (write_sender, mut writes) = mpsc::unbounded::<anyhow::Result<Vec<u8>>>();

        loop {
            let mut message_len = [0u8; 4];
            let read_fut = pin!(stream.read_exact(&mut message_len));
            match select(read_fut, writes.next()).await {
                Either::Left((res, _)) => {
                    res?;
                }
                Either::Right((write, _)) => {
                    let write = write.expect("Write sender is in this scope but somehow gone?!")?;
                    stream.write_with_length(&write).await?;
                    continue;
                }
            }
            let initial_message_len = u32::from_le_bytes(message_len);
            let mut buf = vec![0u8; initial_message_len as usize];
            stream.read_exact(&mut buf).await?;

            let (header, body) = fidl::encoding::decode_transaction_header(&buf)?;
            match header.ordinal {
                control_ordinals::LISTEN => {
                    self.handle_listen(session_id, &write_sender, header, body)?;
                }
                control_ordinals::STOP_LISTEN => {
                    self.handle_stop_listen(session_id, &write_sender, header, body)?;
                }
                control_ordinals::REJECT => {
                    self.handle_reject(session_id, &write_sender, header, body)?;
                }
                unknown_ordinal => {
                    log::warn!("Main protocol got unknown ordinal {unknown_ordinal}");
                    if header.dynamic_flags().contains(fidl::encoding::DynamicFlags::FLEXIBLE) {
                        let resp = fidl_message::encode_response_flexible_unknown(header)?;
                        write_sender
                            .unbounded_send(Ok(resp))
                            .expect("Write receiver is in this scope but somehow gone?!");
                    }
                }
            }
        }
    }

    /// Handle "Reject" messages from a control connection.
    fn handle_reject(
        &self,
        session_id: u64,
        write_sender: &mpsc::UnboundedSender<Result<Vec<u8>, anyhow::Error>>,
        header: fidl_message::TransactionHeader,
        body: &[u8],
    ) -> Result<(), anyhow::Error> {
        let connection_id: usb_fidl::ConnectionId = fidl_message::decode_message(header, body)?;
        let resp = self
            .listeners
            .take_connection(session_id, connection_id)
            .ok_or(usb_fidl::RejectError::NoSuchConnection(connection_id))
            .map(|_| ());
        let resp = fidl_message::encode_response_result(header, resp)?;
        write_sender
            .unbounded_send(Ok(resp))
            .expect("Write receiver is in this scope but somehow gone?!");
        Ok(())
    }

    /// Handle "StopListen" messages from a control connection.
    fn handle_stop_listen(
        &self,
        session_id: u64,
        write_sender: &mpsc::UnboundedSender<Result<Vec<u8>, anyhow::Error>>,
        header: fidl_message::TransactionHeader,
        body: &[u8],
    ) -> Result<(), anyhow::Error> {
        let usb_fidl::ControlStopListenRequest { port } =
            fidl_message::decode_message(header, body)?;
        let resp = self.listeners.cancel_port_listen(session_id, port);
        let resp = fidl_message::encode_response_result(header, resp)?;
        write_sender
            .unbounded_send(Ok(resp))
            .expect("Write receiver is in this scope but somehow gone?!");
        Ok(())
    }

    /// Handle "Listen" messages from a control connection.
    fn handle_listen(
        &self,
        session_id: u64,
        write_sender: &mpsc::UnboundedSender<Result<Vec<u8>, anyhow::Error>>,
        header: fidl_message::TransactionHeader,
        body: &[u8],
    ) -> Result<(), anyhow::Error> {
        let usb_fidl::ControlListenRequest { port } = fidl_message::decode_message(header, body)?;
        let resp = match self.driver.listen(port, None) {
            Ok(s) => {
                self.listeners.init_listener(session_id, port);

                let cancel_waiter = ListenerCancelWaiter {
                    listeners: Arc::clone(&self.listeners),
                    session_id,
                    port,
                };
                let cancel_waiter = cancel_waiter.into_stream();
                let mut stream =
                    futures::stream::select(s.map(Either::Left), cancel_waiter.map(Either::Right));

                let listeners = Arc::downgrade(&self.listeners);
                let write_sender = write_sender.clone();
                self.listener_tasks.spawn(async move {
                    while let Some(Either::Left(incoming)) = stream.next().await {
                        let Some(listeners) = listeners.upgrade() else {
                            log::debug!("Host driver disappeared while listener running");
                            break;
                        };

                        let address = incoming.address().clone();
                        if address.host_port != port {
                            log::warn!(
                                "Listener task for {port} got request for {}",
                                address.host_port
                            );
                            continue;
                        }
                        listeners.add_incoming(session_id, port, incoming);

                        let header = fidl::encoding::TransactionHeader::new(
                            0,
                            control_ordinals::ON_INCOMING,
                            fidl::encoding::DynamicFlags::empty(),
                        );
                        let encoded = fidl_message::encode_message(
                            header,
                            usb_fidl::ConnectionId {
                                remote_cid: address.device_cid,
                                remote_port: address.device_port,
                                local_port: address.host_port,
                            },
                        )
                        .map_err(anyhow::Error::from);
                        write_sender
                            .unbounded_send(encoded)
                            .expect("Write receiver is in this scope but somehow gone?!");
                    }
                });
                Ok(())
            }
            Err(e) => match e {
                usb_vsock_host::ListenError::NotFound(_) => {
                    unreachable!("Didn't specify CID but got CID not found!")
                }
                usb_vsock_host::ListenError::PortInUse(p) => {
                    debug_assert!(p == port);
                    Err(usb_fidl::ListenError::PortInUse(usb_fidl::PortInUse { port }))
                }
            },
        };
        let resp = fidl_message::encode_response_result(header, resp)?;
        write_sender
            .unbounded_send(Ok(resp))
            .expect("Write receiver is in this scope but somehow gone?!");
        Ok(())
    }
}

/// Make an error string fit in the maximum dimensions set by the FIDL protocol
/// for error strings.
fn trunc_error(i: String) -> String {
    if i.len() <= usb_fidl::MAX_ERROR_STRING as usize {
        i
    } else {
        for l in (0..=usb_fidl::MAX_ERROR_STRING as usize).rev() {
            if i.is_char_boundary(l) {
                return i[..l].to_owned();
            }
        }

        // If nothing else, the loop hitting 0 should always hit the return condition.
        unreachable!();
    }
}
