// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use ffx_futures::LocalBoxFuture;
use ffx_metrics::{UsbDriverConnectionEvent, UsbDriverShutdownEvent, UsbDriverTelemetryEvent};
use fidl_fuchsia_ffx_usb_common::{
    self as usb_fidl, control_ordinals, ffx_usb_ordinals, list_devices_ordinals,
};
use futures::channel::{mpsc, oneshot};
use futures::future::{Either, select};
use futures::lock::Mutex as AsyncMutex;
use futures::{FutureExt, SinkExt, StreamExt};
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::io::ErrorKind;
use std::num::NonZero;
pub use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::PathBuf;
use std::pin::{Pin, pin};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll, Waker};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener as TokioUnixListener, UnixStream};
use usb_vsock_host::{ActiveDevice, IncomingConnection, UsbVsockHost, UsbVsockHostEvent};

mod adapters;

use adapters::{AtomicUsbProtocolCounts, WrapStream};

const CURRENT_VERSION: u32 = 0;

#[derive(Debug, Error)]
enum DriverConnectionError {
    #[error("I/O error on driver socket: {0}")]
    Io(#[from] std::io::Error),
    #[error("FIDL encoding/decoding error: {0}")]
    Fidl(#[from] fidl::Error),
    #[error("USB VSOCK error: {0}")]
    UsbVsock(#[from] usb_vsock_host::UsbVsockError),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum DeviceKey {
    Serial(String),
    Cid(u32),
}

impl DeviceKey {
    fn from_cid_and_serial(cid: u32, serial: Option<&str>) -> Self {
        match serial.filter(|s| !s.is_empty()) {
            Some(s) => Self::Serial(s.to_owned()),
            None => Self::Cid(cid),
        }
    }
}

#[derive(Debug, Default)]
struct SessionDeviceTracker {
    cid_to_key: HashMap<u32, DeviceKey>,
    discovered_devices: HashSet<DeviceKey>,
    communicated_devices: HashSet<DeviceKey>,
}

impl SessionDeviceTracker {
    fn record_discovered(&mut self, cid: u32, serial: Option<&str>) {
        let key = DeviceKey::from_cid_and_serial(cid, serial);
        self.cid_to_key.insert(cid, key.clone());
        if self.discovered_devices.insert(key.clone()) {
            log::debug!("Recorded discovered USB device metric: cid={cid}, key={key:?}");
        }
    }

    fn record_communicated(&mut self, cid: u32) {
        let key = self.cid_to_key.entry(cid).or_insert_with(|| DeviceKey::Cid(cid)).clone();
        if self.discovered_devices.insert(key.clone()) {
            log::debug!("Recorded discovered USB device metric: cid={cid}, key={key:?}");
        }
        if self.communicated_devices.insert(key.clone()) {
            log::debug!("Recorded communicated USB device metric: cid={cid}, key={key:?}");
        }
    }

    fn counts(&self) -> (u64, u64) {
        (
            u64::try_from(self.discovered_devices.len()).unwrap_or(0),
            u64::try_from(self.communicated_devices.len()).unwrap_or(0),
        )
    }
}

trait WriteWithLengthPrefix: AsyncWriteExt + Unpin {
    /// Write the entire contents of the buffer, preceded by the length of the
    /// buffer as a 32-bit little-endian unsigned integer.
    async fn write_with_length(&mut self, message: &[u8]) -> std::io::Result<()> {
        self.write_all(&u32::try_from(message.len()).unwrap().to_le_bytes()).await?;
        self.write_all(message).await
    }
}

impl<T: AsyncWriteExt + Unpin> WriteWithLengthPrefix for T {}

trait ReadWithLengthPrefix: AsyncReadExt + Unpin {
    /// Read a parcel of bytes where the first four bytes contain the length of
    /// the following data as a 32-bit unsigned little-endian integer. Fails if
    /// it can't read the entire parcel.
    async fn read_with_length(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        let mut buf = [0u8; 4];

        let got = self.read(&mut buf).await?;

        if got == 0 {
            return Ok(None);
        }

        if got < buf.len() {
            self.read_exact(&mut buf[..got]).await?;
        }

        let mut buf = vec![0u8; u32::from_le_bytes(buf) as usize];
        self.read_exact(&mut buf).await?;
        Ok(Some(buf))
    }
}

impl<T: AsyncReadExt + Unpin> ReadWithLengthPrefix for T {}

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

/// Error returned from [`remove_and_bind_socket`]
#[derive(Error, Debug)]
pub enum RemoveAndBindError {
    #[error("Socket {0} already exists and is in use")]
    InUse(PathBuf),
    #[error("Could not remove stale socket at {0}: {1}")]
    RemoveStale(PathBuf, std::io::Error),
    #[error("Unexpected error when checking for stale socket at {0}: {1}")]
    ConnectCheck(PathBuf, std::io::Error),
    #[error("Could not listen on socket at {0}: {1}")]
    Bind(PathBuf, std::io::Error),
}

/// Bind a socket. If the socket already exits, check if it is in use, and if
/// not, remove it.
pub fn remove_and_bind_socket(socket_path: PathBuf) -> Result<UnixListener, RemoveAndBindError> {
    match StdUnixStream::connect(&socket_path) {
        Err(e) if e.kind() == ErrorKind::NotFound => (),
        Err(e) if e.kind() == ErrorKind::ConnectionRefused => {
            // The socket is stale. Try to remove it.
            if let Err(e) = std::fs::remove_file(&socket_path) {
                return Err(RemoveAndBindError::RemoveStale(socket_path, e));
            }
        }
        Ok(_) => {
            return Err(RemoveAndBindError::InUse(socket_path));
        }
        Err(e) => {
            return Err(RemoveAndBindError::ConnectCheck(socket_path, e));
        }
    }

    let listener = match UnixListener::bind(&socket_path) {
        Ok(s) => s,
        Err(e) => return Err(RemoveAndBindError::Bind(socket_path, e)),
    };
    listener.set_nonblocking(true).map_err(|e| RemoveAndBindError::Bind(socket_path.clone(), e))?;
    Ok(listener)
}

async fn wait_for_shutdown_signal() {
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt());
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());
    match (&mut sigint, &mut sigterm) {
        (Ok(sigint), Ok(sigterm)) => {
            let sigint_fut = pin!(sigint.recv());
            let sigterm_fut = pin!(sigterm.recv());
            let _ = select(sigint_fut, sigterm_fut).await;
        }
        (Ok(sigint), Err(e)) => {
            log::warn!("Could not register SIGTERM handler: {e}");
            let _ = sigint.recv().await;
        }
        (Err(e), Ok(sigterm)) => {
            log::warn!("Could not register SIGINT handler: {e}");
            let _ = sigterm.recv().await;
        }
        (Err(e_int), Err(e_term)) => {
            log::warn!("Could not register signal handlers (SIGINT: {e_int}, SIGTERM: {e_term})");
            futures::future::pending::<()>().await;
        }
    }
}

/// Handle returned by [`HostDriver::new_for_test_with_telemetry`] for controlling
/// driver shutdown, injecting simulated USB host events, and inspecting emitted
/// telemetry events in tests.
pub struct HostDriverTestHandle {
    host: Weak<UsbVsockHost<WrapStream>>,
    host_event_sender: mpsc::UnboundedSender<UsbVsockHostEvent>,
    shutdown_sender: Option<oneshot::Sender<()>>,
    telemetry_receiver: mpsc::UnboundedReceiver<UsbDriverTelemetryEvent>,
}

impl std::fmt::Debug for HostDriverTestHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostDriverTestHandle")
            .field("shutdown_available", &self.shutdown_sender.is_some())
            .finish()
    }
}

impl HostDriverTestHandle {
    /// Upgrades and returns the underlying [`UsbVsockHost`] if the driver is still running.
    pub fn host(&self) -> Option<Arc<UsbVsockHost<WrapStream>>> {
        self.host.upgrade()
    }

    /// Injects a simulated [`UsbVsockHostEvent`] into the running [`HostDriver`].
    pub fn send_host_event(&self, event: UsbVsockHostEvent) {
        let _ = self.host_event_sender.unbounded_send(event);
    }

    /// Triggers graceful shutdown of the running [`HostDriver`].
    pub fn shutdown(&mut self) {
        if let Some(sender) = self.shutdown_sender.take() {
            let _ = sender.send(());
        }
    }

    /// Waits for the next [`UsbDriverTelemetryEvent`] emitted by the [`HostDriver`].
    pub async fn next_telemetry_event(&mut self) -> Option<UsbDriverTelemetryEvent> {
        self.telemetry_receiver.next().await
    }

    /// Attempts to immediately read the next [`UsbDriverTelemetryEvent`] if one is available.
    pub fn try_next_telemetry_event(&mut self) -> Option<UsbDriverTelemetryEvent> {
        self.telemetry_receiver.next().now_or_never().flatten()
    }
}

/// Hostside driver for the FFX USB interface.
pub struct HostDriver {
    driver: Arc<UsbVsockHost<WrapStream>>,
    listeners: Arc<ListenerTable>,
    listener_tasks: fuchsia_async::Scope,
    new_device_listeners: AsyncMutex<Vec<mpsc::Sender<UsbVsockHostEvent>>>,
    log_path: String,
    device_tracker: Mutex<SessionDeviceTracker>,
    protocol_counts: Arc<AtomicUsbProtocolCounts>,
}

impl std::fmt::Debug for HostDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostDriver").field("log_path", &self.log_path).finish()
    }
}

impl HostDriver {
    /// Create a new [`HostDriver`] and listen for users at the given socket path.
    pub async fn run(listener: UnixListener, log_path: String, serial: Option<String>) {
        let (sender, receiver) = mpsc::channel(1);
        let listener =
            TokioUnixListener::from_std(listener).expect("Could not register listener with tokio");
        let shutdown = wait_for_shutdown_signal();

        HostDriver::run_with_driver(
            UsbVsockHost::new([] as [&str; 0], true, sender, serial.into_iter().collect()),
            receiver,
            listener,
            log_path,
            shutdown,
            None,
        )
        .await;
    }

    async fn emit_telemetry_event(
        event: UsbDriverTelemetryEvent,
        telemetry_sender: &Option<mpsc::UnboundedSender<UsbDriverTelemetryEvent>>,
    ) {
        log::debug!("Recording USB driver metric event: {event:?}");
        if let Some(tx) = telemetry_sender {
            let _ = tx.unbounded_send(event.clone());
        }
        let result = match &event {
            UsbDriverTelemetryEvent::Launch => ffx_metrics::add_usb_driver_launch_event().await,
            UsbDriverTelemetryEvent::ConnectionClosed(conn_event) => {
                ffx_metrics::add_usb_driver_connection_event(conn_event).await
            }
            UsbDriverTelemetryEvent::Shutdown(shutdown_event) => {
                ffx_metrics::add_usb_driver_shutdown_event(shutdown_event).await
            }
        };
        if let Err(e) = result {
            log::debug!("Could not emit USB driver metric event: {e}");
        }
    }

    async fn run_with_driver<S>(
        driver: Arc<UsbVsockHost<WrapStream>>,
        mut events: mpsc::Receiver<UsbVsockHostEvent>,
        listener: TokioUnixListener,
        log_path: String,
        shutdown: S,
        telemetry_sender: Option<mpsc::UnboundedSender<UsbDriverTelemetryEvent>>,
    ) where
        S: Future<Output = ()>,
    {
        let mut initial_tracker = SessionDeviceTracker::default();
        for ActiveDevice { cid, serial } in driver.active_devices() {
            initial_tracker.record_discovered(cid, serial.as_deref());
        }

        Self::emit_telemetry_event(UsbDriverTelemetryEvent::Launch, &telemetry_sender).await;

        let protocol_counts = Arc::new(AtomicUsbProtocolCounts::default());
        let host_driver = HostDriver {
            driver,
            listeners: Arc::new(ListenerTable(Mutex::new(HashMap::new()))),
            listener_tasks: fuchsia_async::Scope::new_with_name("usb_driver_listeners"),
            new_device_listeners: AsyncMutex::new(Vec::new()),
            log_path,
            device_tracker: Mutex::new(initial_tracker),
            protocol_counts: Arc::clone(&protocol_counts),
        };

        let (conn_event_tx, mut conn_event_rx) = mpsc::unbounded::<UsbDriverConnectionEvent>();
        let tasks = Mutex::new(Vec::<LocalBoxFuture<'_, ()>>::new());
        let mut task_poller = futures::future::poll_fn(|ctx| {
            let mut tasks = tasks.lock().unwrap();
            tasks.retain_mut(|x| !x.poll_unpin(ctx).is_ready());
            Poll::<()>::Pending
        });
        tasks.lock().unwrap().push(Box::pin(async {
            while let Some(event) = events.next().await {
                if let UsbVsockHostEvent::AddedCid { cid, ref serial } = event {
                    host_driver
                        .device_tracker
                        .lock()
                        .unwrap()
                        .record_discovered(cid, serial.as_deref());
                }
                let mut listeners = host_driver.new_device_listeners.lock().await;
                for listener in &mut *listeners {
                    let _ = listener.send(event.clone()).await;
                }
            }
        }));

        let mut shutdown = pin!(shutdown);
        loop {
            let accept_fut = pin!(listener.accept());
            let accept_or_shutdown = select(accept_fut, &mut shutdown);
            let conn_or_tasks = select(conn_event_rx.next(), &mut task_poller);
            match select(accept_or_shutdown, conn_or_tasks).await {
                Either::Left((Either::Left((Ok((stream, _addr)), _)), _)) => {
                    let tracked_stream = WrapStream::new_tracked(stream, conn_event_tx.clone());
                    tasks.lock().unwrap().push(Box::pin(async {
                        if let Err(e) = host_driver.handle_connection(tracked_stream).await {
                            log::error!("Connection failed: {e}");
                        }
                    }));
                }
                Either::Left((Either::Left((Err(e), _)), _)) => {
                    log::error!("Socket failed: {e}");
                    break;
                }
                Either::Left((Either::Right(((), _)), _)) => {
                    break;
                }
                Either::Right((Either::Left((Some(conn_event), _)), _)) => {
                    Self::emit_telemetry_event(
                        UsbDriverTelemetryEvent::ConnectionClosed(conn_event),
                        &telemetry_sender,
                    )
                    .await;
                }
                Either::Right((Either::Left((None, _)), _))
                | Either::Right((Either::Right(_), _)) => {
                    unreachable!()
                }
            }
        }

        std::mem::drop(tasks);
        let (unique_devices_discovered, unique_devices_communicated) =
            host_driver.device_tracker.lock().unwrap().counts();
        std::mem::drop(host_driver);
        std::mem::drop(conn_event_tx);

        while let Ok(conn_event) = conn_event_rx.try_recv() {
            Self::emit_telemetry_event(
                UsbDriverTelemetryEvent::ConnectionClosed(conn_event),
                &telemetry_sender,
            )
            .await;
        }

        let shutdown_event = UsbDriverShutdownEvent {
            unique_devices_discovered,
            unique_devices_communicated,
            protocol_counts: protocol_counts.snapshot(),
        };
        Self::emit_telemetry_event(
            UsbDriverTelemetryEvent::Shutdown(shutdown_event),
            &telemetry_sender,
        )
        .await;
    }

    /// Create a new [`HostDriver`] and listen for users at the given socket path.
    pub fn new_for_test(
        socket_path: PathBuf,
    ) -> usb_vsock_host::TestConnection<
        impl From<UnixStream> + futures::AsyncRead + futures::AsyncWrite + Send + 'static,
    > {
        let (conn, _handle) = Self::new_for_test_with_telemetry(socket_path);
        conn
    }

    /// Create a new [`HostDriver`] for testing alongside a [`HostDriverTestHandle`]
    /// for triggering graceful shutdown and inspecting telemetry events.
    pub fn new_for_test_with_telemetry(
        socket_path: PathBuf,
    ) -> (usb_vsock_host::TestConnection<WrapStream>, HostDriverTestHandle) {
        let listener =
            TokioUnixListener::bind(socket_path).expect("Could not create socket for test");
        let (host, conn) = usb_vsock_host::TestConnection::<WrapStream>::new();
        let weak_host = Arc::downgrade(&host.host);
        let (host_event_sender, mut host_event_receiver) = mpsc::unbounded();
        let (mut merged_tx, merged_rx) = mpsc::channel(8);
        let mut orig_rx = host.event_receiver;
        conn.scope.spawn_local(async move {
            let mut merged = futures::stream::select(&mut orig_rx, &mut host_event_receiver);
            while let Some(ev) = merged.next().await {
                if merged_tx.send(ev).await.is_err() {
                    break;
                }
            }
        });
        let (shutdown_sender, shutdown_receiver) = oneshot::channel();
        let (telemetry_sender, telemetry_receiver) = mpsc::unbounded();
        let fut = HostDriver::run_with_driver(
            host.host,
            merged_rx,
            listener,
            "<test>".to_owned(),
            async move {
                if shutdown_receiver.await.is_err() {
                    futures::future::pending::<()>().await;
                }
            },
            Some(telemetry_sender),
        );
        conn.scope.spawn_local(fut);
        (
            conn,
            HostDriverTestHandle {
                host: weak_host,
                host_event_sender,
                shutdown_sender: Some(shutdown_sender),
                telemetry_receiver,
            },
        )
    }

    fn record_communicated_cid(&self, cid: u32) {
        let mut tracker = self.device_tracker.lock().unwrap();
        if !tracker.cid_to_key.contains_key(&cid) {
            if let Some(active) = self.driver.active_devices().into_iter().find(|d| d.cid == cid) {
                tracker.record_discovered(active.cid, active.serial.as_deref());
            }
        }
        tracker.record_communicated(cid);
    }

    /// Handle a new connection from a tool on our socket.
    async fn handle_connection(&self, mut stream: WrapStream) -> Result<(), DriverConnectionError> {
        let Some(buf) = stream.read_with_length().await? else { return Ok(()) };

        let (header, body) = fidl::encoding::decode_transaction_header(&buf)?;
        match header.ordinal {
            ffx_usb_ordinals::INITIALIZE_CONTROL => {
                if let Some(counts) = stream.protocol_counts() {
                    counts.record_initialize_control();
                }
                self.protocol_counts.record_initialize_control();
                log::debug!("Recorded USB driver protocol metric: InitializeControl");
                self.handle_initialize_control(stream, header, body).await
            }
            ffx_usb_ordinals::INITIALIZE_LIST_DEVICES => {
                if let Some(counts) = stream.protocol_counts() {
                    counts.record_initialize_list_devices();
                }
                self.protocol_counts.record_initialize_list_devices();
                log::debug!("Recorded USB driver protocol metric: InitializeListDevices");
                self.handle_initialize_list_devices(stream, header, body).await
            }
            ffx_usb_ordinals::INITIALIZE_CONNECT_TO => {
                if let Some(counts) = stream.protocol_counts() {
                    counts.record_initialize_connect_to();
                }
                self.protocol_counts.record_initialize_connect_to();
                log::debug!("Recorded USB driver protocol metric: InitializeConnectTo");
                self.handle_initialize_connect_to(stream, header, body).await
            }
            ffx_usb_ordinals::INITIALIZE_ACCEPT => {
                if let Some(counts) = stream.protocol_counts() {
                    counts.record_initialize_accept();
                }
                self.protocol_counts.record_initialize_accept();
                log::debug!("Recorded USB driver protocol metric: InitializeAccept");
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
        mut stream: WrapStream,
        header: fidl_message::TransactionHeader,
        body: &[u8],
    ) -> Result<(), DriverConnectionError> {
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
            self.record_communicated_cid(conn.remote_cid);
            let _conn_state = ready.finish_connect(stream).await;
        }

        Ok(())
    }

    /// Handle "InitializeConnectTo" messages.
    async fn handle_initialize_connect_to(
        &self,
        mut stream: WrapStream,
        header: fidl_message::TransactionHeader,
        body: &[u8],
    ) -> Result<(), DriverConnectionError> {
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
            self.record_communicated_cid(cid);
            let _conn_state = ready.finish_connect(stream).await;
        }

        Ok(())
    }

    /// Handle "InitializeListDevices" messages.
    async fn handle_initialize_list_devices(
        &self,
        mut stream: WrapStream,
        header: fidl_message::TransactionHeader,
        body: &[u8],
    ) -> Result<(), DriverConnectionError> {
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
        mut stream: WrapStream,
        header: fidl_message::TransactionHeader,
        body: &[u8],
    ) -> Result<(), DriverConnectionError> {
        if !body.is_empty() {
            return Err(fidl::Error::ExtraBytes.into());
        }

        let session_id = rand::random();
        let res = usb_fidl::FfxUsbInitializeControlResponse {
            current: CURRENT_VERSION,
            minimum: CURRENT_VERSION,
            session_id,
            log_path: self.log_path.clone(),
        };

        let resp = fidl_message::encode_response_flexible(header, res)?;
        stream.write_with_length(&resp).await?;
        let ret = self.handle_control_connection(stream, session_id).await;
        self.listeners.cancel_session(session_id);
        ret
    }

    /// Handle a "list devices" connection. This will send back events every
    /// time a new USB target becomes available or disappears.
    async fn handle_list_devices(
        &self,
        mut stream: WrapStream,
    ) -> Result<(), DriverConnectionError> {
        let (event_sender, mut events) = mpsc::channel(1);
        self.new_device_listeners.lock().await.push(event_sender);
        let conn_protocol_counts = stream.protocol_counts();

        let existing = self.driver.active_devices();
        for ActiveDevice { cid, serial } in existing.iter().cloned() {
            let header = fidl::encoding::TransactionHeader::new(
                0,
                list_devices_ordinals::ON_DEVICE_APPEARED,
                fidl::encoding::DynamicFlags::empty(),
            );
            let body = usb_fidl::DeviceInfo {
                cid,
                meta: usb_fidl::DeviceMeta { serial, ..Default::default() },
            };
            let data = fidl_message::encode_message(header, body)?;
            if let Some(counts) = &conn_protocol_counts {
                counts.record_on_device_appeared();
            }
            self.protocol_counts.record_on_device_appeared();
            log::debug!("Recorded USB driver protocol metric: OnDeviceAppeared (cid={cid})");
            stream.write_with_length(&data).await?;
        }

        let mut existing = Some(existing);

        loop {
            let event = {
                let read_fut = pin!(stream.read_with_length());
                match select(read_fut, events.next()).await {
                    Either::Left((Ok(None), _)) | Either::Left((Err(_), _)) => {
                        break;
                    }
                    Either::Left((Ok(Some(_)), _)) => {
                        continue;
                    }
                    Either::Right((Some(event), _)) => event,
                    Either::Right((None, _)) => break,
                }
            };

            let data = match event {
                UsbVsockHostEvent::AddedCid { cid, serial } => {
                    if let Some(existing_ref) = &existing {
                        if existing_ref.iter().any(|x| x.cid == cid) {
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
                    let body = usb_fidl::DeviceInfo {
                        cid,
                        meta: usb_fidl::DeviceMeta { serial, ..Default::default() },
                    };
                    let encoded = fidl_message::encode_message(header, body)?;
                    if let Some(counts) = &conn_protocol_counts {
                        counts.record_on_device_appeared();
                    }
                    self.protocol_counts.record_on_device_appeared();
                    log::debug!(
                        "Recorded USB driver protocol metric: OnDeviceAppeared (cid={cid})"
                    );
                    encoded
                }
                UsbVsockHostEvent::RemovedCid(cid) => {
                    if let Some(existing_ref) = &existing {
                        if existing_ref.iter().any(|x| x.cid == cid) {
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
                    let encoded = fidl_message::encode_message(header, body)?;
                    if let Some(counts) = &conn_protocol_counts {
                        counts.record_on_device_disappeared();
                    }
                    self.protocol_counts.record_on_device_disappeared();
                    log::debug!(
                        "Recorded USB driver protocol metric: OnDeviceDisappeared (cid={cid})"
                    );
                    encoded
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
        mut stream: WrapStream,
        session_id: u64,
    ) -> Result<(), DriverConnectionError> {
        let conn_protocol_counts = stream.protocol_counts();
        let (write_sender, mut writes) =
            mpsc::unbounded::<Result<Vec<u8>, DriverConnectionError>>();

        loop {
            let buf_or_msg = {
                let read_fut = pin!(stream.read_with_length());
                match select(read_fut, writes.next()).await {
                    Either::Left((res, _)) => {
                        let Some(buf) = res? else {
                            return Ok(());
                        };

                        Either::Left(buf)
                    }
                    Either::Right((write, _)) => {
                        let write =
                            write.expect("Write sender is in this scope but somehow gone?!")?;
                        Either::Right(write)
                    }
                }
            };

            let buf = match buf_or_msg {
                Either::Left(buf) => buf,
                Either::Right(write) => {
                    stream.write_with_length(&write).await?;
                    continue;
                }
            };

            let (header, body) = fidl::encoding::decode_transaction_header(&buf)?;
            match header.ordinal {
                control_ordinals::LISTEN => {
                    if let Some(counts) = &conn_protocol_counts {
                        counts.record_listen();
                    }
                    self.protocol_counts.record_listen();
                    log::debug!("Recorded USB driver protocol metric: Listen");
                    self.handle_listen(
                        session_id,
                        &write_sender,
                        conn_protocol_counts.clone(),
                        header,
                        body,
                    )?;
                }
                control_ordinals::STOP_LISTEN => {
                    if let Some(counts) = &conn_protocol_counts {
                        counts.record_stop_listen();
                    }
                    self.protocol_counts.record_stop_listen();
                    log::debug!("Recorded USB driver protocol metric: StopListen");
                    self.handle_stop_listen(session_id, &write_sender, header, body)?;
                }
                control_ordinals::REJECT => {
                    if let Some(counts) = &conn_protocol_counts {
                        counts.record_reject();
                    }
                    self.protocol_counts.record_reject();
                    log::debug!("Recorded USB driver protocol metric: Reject");
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
        write_sender: &mpsc::UnboundedSender<Result<Vec<u8>, DriverConnectionError>>,
        header: fidl_message::TransactionHeader,
        body: &[u8],
    ) -> Result<(), DriverConnectionError> {
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
        write_sender: &mpsc::UnboundedSender<Result<Vec<u8>, DriverConnectionError>>,
        header: fidl_message::TransactionHeader,
        body: &[u8],
    ) -> Result<(), DriverConnectionError> {
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
        write_sender: &mpsc::UnboundedSender<Result<Vec<u8>, DriverConnectionError>>,
        conn_protocol_counts: Option<Arc<AtomicUsbProtocolCounts>>,
        header: fidl_message::TransactionHeader,
        body: &[u8],
    ) -> Result<(), DriverConnectionError> {
        let usb_fidl::ControlListenRequest { port } = fidl_message::decode_message(header, body)?;
        let resp = match self.driver.listen(port) {
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
                let session_protocol_counts = Arc::clone(&self.protocol_counts);
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

                        if let Some(counts) = &conn_protocol_counts {
                            counts.record_on_incoming();
                        }
                        session_protocol_counts.record_on_incoming();
                        log::debug!(
                            "Recorded USB driver protocol metric: OnIncoming (remote_cid={}, remote_port={}, local_port={})",
                            address.device_cid,
                            address.device_port,
                            address.host_port
                        );

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
                        .map_err(DriverConnectionError::from);
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

#[cfg(test)]
mod test {
    use super::*;
    use ffx_metrics::UsbProtocolCounts;

    #[fuchsia::test]
    async fn remove_and_bind_removes() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("test_sock");
        let sock = remove_and_bind_socket(sock_path.clone()).unwrap();
        std::mem::drop(sock);
        let _ = remove_and_bind_socket(sock_path).unwrap();
    }

    #[fuchsia::test]
    async fn remove_and_bind_respects_in_use() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("test_sock");
        let _sock = remove_and_bind_socket(sock_path.clone()).unwrap();
        let e = remove_and_bind_socket(sock_path).unwrap_err();
        assert!(matches!(e, RemoveAndBindError::InUse(_)));
    }

    #[fuchsia::test]
    async fn remove_and_bind_permission_fail() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("test_sock");
        let sock = remove_and_bind_socket(sock_path.clone()).unwrap();
        std::mem::drop(sock);
        let mut permissions = dir.path().metadata().unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(dir.path(), permissions).unwrap();
        let e = remove_and_bind_socket(sock_path).unwrap_err();
        assert!(matches!(e, RemoveAndBindError::RemoveStale(_, _)), "Unexpected failure: {e:?}");
    }
    #[test]
    fn remove_and_bind_without_tokio() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("test_sock");
        let sock = remove_and_bind_socket(sock_path).unwrap();
        std::mem::drop(sock);
    }

    #[fuchsia::test]
    async fn read_with_length_handles_partial_prefix() {
        let (mut writer, mut reader) = UnixStream::pair().unwrap();
        let payload = b"hello partial prefix";
        let len_bytes = u32::try_from(payload.len()).unwrap().to_le_bytes();

        // Write the 4-byte length prefix in two separate chunks (1 byte, then 3 bytes).
        writer.write_all(&len_bytes[..1]).await.unwrap();
        let read_task = fuchsia_async::Task::local(async move { reader.read_with_length().await });
        writer.write_all(&len_bytes[1..]).await.unwrap();
        writer.write_all(payload).await.unwrap();

        let got = read_task.await.unwrap().unwrap();
        assert_eq!(got, payload);
    }

    #[fuchsia::test]
    async fn session_device_tracker_deduplicates_discovered_and_communicated() {
        let mut tracker = SessionDeviceTracker::default();

        // Initial discovery of device A (serial "SER-A") on cid 3.
        tracker.record_discovered(3, Some("SER-A"));
        // Duplicate discovery of device A on cid 3 (e.g., active_devices + AddedCid).
        tracker.record_discovered(3, Some("SER-A"));
        // Device A reconnects with a new cid 4 but same serial "SER-A".
        tracker.record_discovered(4, Some("SER-A"));
        // Second distinct device B (serial "SER-B") on cid 5.
        tracker.record_discovered(5, Some("SER-B"));
        // Third distinct device C without a serial on cid 6.
        tracker.record_discovered(6, None);

        assert_eq!(tracker.counts(), (3, 0));

        // Communicate with device A across both cid 3 and cid 4 -> still 1 unique communicated device.
        tracker.record_communicated(3);
        tracker.record_communicated(4);
        assert_eq!(tracker.counts(), (3, 1));

        // Communicate with device C (cid 6).
        tracker.record_communicated(6);
        assert_eq!(tracker.counts(), (3, 2));
    }

    #[fuchsia::test]
    async fn host_driver_launch_and_graceful_shutdown_telemetry() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("test_sock");
        let (conn, mut handle) = HostDriver::new_for_test_with_telemetry(sock_path.clone());

        // First telemetry event is Launch.
        assert_eq!(handle.next_telemetry_event().await, Some(UsbDriverTelemetryEvent::Launch));

        // Open a ListDevices connection and read the initial response + initial OnDeviceAppeared.
        let mut list_stream = UnixStream::connect(&sock_path).await.unwrap();
        let init_hdr = fidl::encoding::TransactionHeader::new(
            1,
            ffx_usb_ordinals::INITIALIZE_LIST_DEVICES,
            fidl::encoding::DynamicFlags::FLEXIBLE,
        );
        let init_msg = fidl_message::encode_message(init_hdr, ()).unwrap();
        list_stream.write_with_length(&init_msg).await.unwrap();
        let _resp = list_stream.read_with_length().await.unwrap().unwrap();
        let _ev1 = list_stream.read_with_length().await.unwrap().unwrap();

        // Simulate the same device re-appearing on a new CID with the same serial, plus a second device.
        handle.send_host_event(UsbVsockHostEvent::AddedCid {
            cid: conn.cid + 1,
            serial: Some(conn.serial.clone()),
        });
        let _ev2 = list_stream.read_with_length().await.unwrap().unwrap();

        handle.send_host_event(UsbVsockHostEvent::AddedCid {
            cid: conn.cid + 2,
            serial: Some("second-device-serial".to_string()),
        });
        let _ev3 = list_stream.read_with_length().await.unwrap().unwrap();

        // Simulate device disappearance.
        handle.send_host_event(UsbVsockHostEvent::RemovedCid(conn.cid + 2));
        let _ev4 = list_stream.read_with_length().await.unwrap().unwrap();

        // Drop the ListDevices stream while no USB events are occurring; it should close immediately.
        std::mem::drop(list_stream);
        let Some(UsbDriverTelemetryEvent::ConnectionClosed(list_conn_event)) =
            handle.next_telemetry_event().await
        else {
            panic!("Expected ConnectionClosed event for ListDevices stream");
        };
        assert!(list_conn_event.rx_bytes > 0);
        assert!(list_conn_event.tx_bytes > 0);
        assert_eq!(
            list_conn_event.protocol_counts,
            UsbProtocolCounts {
                initialize_list_devices: 1,
                on_device_appeared: 3,
                on_device_disappeared: 1,
                ..Default::default()
            }
        );

        // Attempt a failed InitializeConnectTo to a non-existent CID (should count protocol call
        // and connection close, but NOT increment unique_devices_communicated).
        let mut bad_conn_stream = UnixStream::connect(&sock_path).await.unwrap();
        let connect_hdr = fidl::encoding::TransactionHeader::new(
            1,
            ffx_usb_ordinals::INITIALIZE_CONNECT_TO,
            fidl::encoding::DynamicFlags::FLEXIBLE,
        );
        let bad_connect_req = usb_fidl::FfxUsbInitializeConnectToRequest { cid: 999, port: 1234 };
        let bad_connect_msg = fidl_message::encode_message(connect_hdr, bad_connect_req).unwrap();
        bad_conn_stream.write_with_length(&bad_connect_msg).await.unwrap();
        let _bad_resp = bad_conn_stream.read_with_length().await.unwrap().unwrap();
        std::mem::drop(bad_conn_stream);

        let Some(UsbDriverTelemetryEvent::ConnectionClosed(bad_conn_event)) =
            handle.next_telemetry_event().await
        else {
            panic!("Expected ConnectionClosed event for failed InitializeConnectTo");
        };
        assert_eq!(
            bad_conn_event.protocol_counts,
            UsbProtocolCounts { initialize_connect_to: 1, ..Default::default() }
        );

        // Trigger graceful shutdown and verify Shutdown event reports 2 unique discovered devices
        // and 0 communicated devices.
        handle.shutdown();
        let Some(UsbDriverTelemetryEvent::Shutdown(shutdown_event)) =
            handle.next_telemetry_event().await
        else {
            panic!("Expected Shutdown telemetry event");
        };
        assert_eq!(shutdown_event.unique_devices_discovered, 2);
        assert_eq!(shutdown_event.unique_devices_communicated, 0);
        assert_eq!(
            shutdown_event.protocol_counts,
            UsbProtocolCounts {
                initialize_list_devices: 1,
                initialize_connect_to: 1,
                on_device_appeared: 3,
                on_device_disappeared: 1,
                ..Default::default()
            }
        );
    }

    #[fuchsia::test]
    async fn host_driver_control_and_vsock_bridging_telemetry() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("test_sock");
        let (mut conn, mut handle) = HostDriver::new_for_test_with_telemetry(sock_path.clone());

        assert_eq!(handle.next_telemetry_event().await, Some(UsbDriverTelemetryEvent::Launch));

        // 1. InitializeControl on a control socket.
        let mut ctrl_stream = UnixStream::connect(&sock_path).await.unwrap();
        let ctrl_hdr = fidl::encoding::TransactionHeader::new(
            1,
            ffx_usb_ordinals::INITIALIZE_CONTROL,
            fidl::encoding::DynamicFlags::FLEXIBLE,
        );
        let ctrl_msg = fidl_message::encode_message(ctrl_hdr, ()).unwrap();
        ctrl_stream.write_with_length(&ctrl_msg).await.unwrap();
        let ctrl_resp_buf = ctrl_stream.read_with_length().await.unwrap().unwrap();
        let (ctrl_resp_hdr, ctrl_resp_body) =
            fidl::encoding::decode_transaction_header(&ctrl_resp_buf).unwrap();
        let ctrl_resp: usb_fidl::FfxUsbInitializeControlResponse =
            match fidl_message::decode_response_flexible(ctrl_resp_hdr, ctrl_resp_body).unwrap() {
                fidl_message::MaybeUnknown::Known(x) => x,
                fidl_message::MaybeUnknown::Unknown => panic!("Unexpected unknown response"),
            };
        let session_id = ctrl_resp.session_id;

        // 2. Listen on port 4321.
        let listen_hdr = fidl::encoding::TransactionHeader::new(
            2,
            control_ordinals::LISTEN,
            fidl::encoding::DynamicFlags::FLEXIBLE,
        );
        let listen_msg =
            fidl_message::encode_message(listen_hdr, usb_fidl::ControlListenRequest { port: 4321 })
                .unwrap();
        ctrl_stream.write_with_length(&listen_msg).await.unwrap();
        let _listen_resp = ctrl_stream.read_with_length().await.unwrap().unwrap();

        // 3. Target device connects to host port 4321 twice: reject first, accept second.
        let cid = conn.cid;
        let dev_conn_1 = Arc::clone(&conn.connection);
        let (dev_stream_1, other_end_1) = UnixStream::pair().unwrap();
        let incoming_task_1 = fuchsia_async::Task::local(async move {
            dev_conn_1
                .connect(
                    usb_vsock::Address {
                        device_cid: cid,
                        host_cid: 2,
                        device_port: 9001,
                        host_port: 4321,
                    },
                    other_end_1.into(),
                )
                .await
        });

        // Read OnIncoming #1 from control stream and Reject it.
        let on_inc_buf_1 = ctrl_stream.read_with_length().await.unwrap().unwrap();
        let (on_inc_hdr_1, on_inc_body_1) =
            fidl::encoding::decode_transaction_header(&on_inc_buf_1).unwrap();
        assert_eq!(on_inc_hdr_1.ordinal, control_ordinals::ON_INCOMING);
        let conn_id_1: usb_fidl::ConnectionId =
            fidl_message::decode_message(on_inc_hdr_1, on_inc_body_1).unwrap();

        let reject_hdr = fidl::encoding::TransactionHeader::new(
            3,
            control_ordinals::REJECT,
            fidl::encoding::DynamicFlags::FLEXIBLE,
        );
        let reject_msg = fidl_message::encode_message(reject_hdr, conn_id_1).unwrap();
        ctrl_stream.write_with_length(&reject_msg).await.unwrap();
        let _reject_resp = ctrl_stream.read_with_length().await.unwrap().unwrap();
        assert!(incoming_task_1.await.is_err());
        std::mem::drop(dev_stream_1);

        // Second incoming connection: accept via InitializeAccept and exchange VSOCK payload.
        let dev_conn_2 = Arc::clone(&conn.connection);
        let (mut dev_stream_2, other_end_2) = UnixStream::pair().unwrap();
        let incoming_task_2 = fuchsia_async::Task::local(async move {
            dev_conn_2
                .connect(
                    usb_vsock::Address {
                        device_cid: cid,
                        host_cid: 2,
                        device_port: 9002,
                        host_port: 4321,
                    },
                    other_end_2.into(),
                )
                .await
                .unwrap()
        });

        let on_inc_buf_2 = ctrl_stream.read_with_length().await.unwrap().unwrap();
        let (on_inc_hdr_2, on_inc_body_2) =
            fidl::encoding::decode_transaction_header(&on_inc_buf_2).unwrap();
        let conn_id_2: usb_fidl::ConnectionId =
            fidl_message::decode_message(on_inc_hdr_2, on_inc_body_2).unwrap();

        let mut accept_stream = UnixStream::connect(&sock_path).await.unwrap();
        let accept_hdr = fidl::encoding::TransactionHeader::new(
            1,
            ffx_usb_ordinals::INITIALIZE_ACCEPT,
            fidl::encoding::DynamicFlags::FLEXIBLE,
        );
        let accept_msg = fidl_message::encode_message(
            accept_hdr,
            usb_fidl::FfxUsbInitializeAcceptRequest { conn: conn_id_2, session_id },
        )
        .unwrap();
        let accept_req_wire_len = (4 + accept_msg.len()) as u64;
        accept_stream.write_with_length(&accept_msg).await.unwrap();
        let accept_resp_buf = accept_stream.read_with_length().await.unwrap().unwrap();
        let accept_resp_wire_len = (4 + accept_resp_buf.len()) as u64;
        let _dev_conn_state = incoming_task_2.await;

        let host_to_dev = b"host-vsock-payload";
        let dev_to_host = b"device-vsock-payload";
        accept_stream.write_all(host_to_dev).await.unwrap();
        let mut recv_on_dev = vec![0u8; host_to_dev.len()];
        dev_stream_2.read_exact(&mut recv_on_dev).await.unwrap();
        assert_eq!(&recv_on_dev, host_to_dev);

        dev_stream_2.write_all(dev_to_host).await.unwrap();
        let mut recv_on_host = vec![0u8; dev_to_host.len()];
        accept_stream.read_exact(&mut recv_on_host).await.unwrap();
        assert_eq!(&recv_on_host, dev_to_host);

        std::mem::drop(accept_stream);
        std::mem::drop(dev_stream_2);
        std::mem::drop(_dev_conn_state);

        let Some(UsbDriverTelemetryEvent::ConnectionClosed(accept_conn_event)) =
            handle.next_telemetry_event().await
        else {
            panic!("Expected ConnectionClosed event for InitializeAccept stream");
        };
        assert_eq!(accept_conn_event.rx_bytes, accept_req_wire_len + host_to_dev.len() as u64);
        assert_eq!(accept_conn_event.tx_bytes, accept_resp_wire_len + dev_to_host.len() as u64);
        assert_eq!(
            accept_conn_event.protocol_counts,
            UsbProtocolCounts { initialize_accept: 1, ..Default::default() }
        );

        // 4. StopListen on port 4321 and close control socket.
        let stop_hdr = fidl::encoding::TransactionHeader::new(
            4,
            control_ordinals::STOP_LISTEN,
            fidl::encoding::DynamicFlags::FLEXIBLE,
        );
        let stop_msg = fidl_message::encode_message(
            stop_hdr,
            usb_fidl::ControlStopListenRequest { port: 4321 },
        )
        .unwrap();
        ctrl_stream.write_with_length(&stop_msg).await.unwrap();
        let _stop_resp = ctrl_stream.read_with_length().await.unwrap().unwrap();
        std::mem::drop(ctrl_stream);

        let Some(UsbDriverTelemetryEvent::ConnectionClosed(ctrl_conn_event)) =
            handle.next_telemetry_event().await
        else {
            panic!("Expected ConnectionClosed event for Control stream");
        };
        assert_eq!(
            ctrl_conn_event.protocol_counts,
            UsbProtocolCounts {
                initialize_control: 1,
                listen: 1,
                stop_listen: 1,
                reject: 1,
                on_incoming: 2,
                ..Default::default()
            }
        );

        // 5. Outbound InitializeConnectTo to the same device (cid) should succeed and keep
        // unique_devices_communicated == 1.
        let mut out_stream = UnixStream::connect(&sock_path).await.unwrap();
        let out_hdr = fidl::encoding::TransactionHeader::new(
            1,
            ffx_usb_ordinals::INITIALIZE_CONNECT_TO,
            fidl::encoding::DynamicFlags::FLEXIBLE,
        );
        let out_msg = fidl_message::encode_message(
            out_hdr,
            usb_fidl::FfxUsbInitializeConnectToRequest { cid, port: 5555 },
        )
        .unwrap();
        out_stream.write_with_length(&out_msg).await.unwrap();
        let incoming_req = conn.incoming_requests.next().await.unwrap();
        let (_dev_out_stream, other_end_out) = UnixStream::pair().unwrap();
        let _out_state = conn.connection.accept(incoming_req, other_end_out.into()).await.unwrap();
        let _out_resp = out_stream.read_with_length().await.unwrap().unwrap();
        std::mem::drop(out_stream);
        std::mem::drop(_dev_out_stream);
        std::mem::drop(_out_state);

        let Some(UsbDriverTelemetryEvent::ConnectionClosed(out_conn_event)) =
            handle.next_telemetry_event().await
        else {
            panic!("Expected ConnectionClosed event for InitializeConnectTo stream");
        };
        assert_eq!(
            out_conn_event.protocol_counts,
            UsbProtocolCounts { initialize_connect_to: 1, ..Default::default() }
        );

        handle.shutdown();
        let Some(UsbDriverTelemetryEvent::Shutdown(shutdown_event)) =
            handle.next_telemetry_event().await
        else {
            panic!("Expected Shutdown event");
        };
        assert_eq!(shutdown_event.unique_devices_discovered, 1);
        assert_eq!(shutdown_event.unique_devices_communicated, 1);
        assert_eq!(
            shutdown_event.protocol_counts,
            UsbProtocolCounts {
                initialize_control: 1,
                initialize_connect_to: 1,
                initialize_accept: 1,
                listen: 1,
                stop_listen: 1,
                reject: 1,
                on_incoming: 2,
                ..Default::default()
            }
        );
    }
}
