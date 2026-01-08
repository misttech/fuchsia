// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::WriteFlags;

use super::{
    ActiveRequests, DecodedRequest, IntoSessionManager, OffsetMap, Operation, RequestId,
    SessionHelper, TraceFlowId,
};
use anyhow::Error;
use block_protocol::{BlockFifoRequest, BlockFifoResponse};
use fidl::endpoints::RequestStream;
use fidl_fuchsia_storage_block as fblock;
use fidl_fuchsia_storage_block::MAX_TRANSFER_UNBOUNDED;
use fuchsia_async::{self as fasync, EHandle};
use fuchsia_sync::{Condvar, Mutex};
use futures::TryStreamExt;
use futures::stream::{AbortHandle, Abortable};
use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::ffi::{CStr, c_char, c_void};
use std::mem::MaybeUninit;
use std::num::NonZero;
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Weak};
use zx::{self as zx, AsHandleRef as _};

pub struct SessionManager {
    callbacks: Callbacks,
    open_sessions: Mutex<HashMap<usize, Weak<Session>>>,
    open_sessions_condvar: Condvar,
    active_requests: ActiveRequests<Arc<Session>>,
    inflight_requests: Mutex<usize>,
    no_inflight_requests_condvar: Condvar,
    mbox: ExecutorMailbox,
    info: super::DeviceInfo,
}

unsafe impl Send for SessionManager {}
unsafe impl Sync for SessionManager {}

impl SessionManager {
    fn submit_requests(&self, requests: &mut [Request]) {
        *self.inflight_requests.lock() += requests.len();
        // SAFETY: `request` points to a valid array of `requests.len()` elements.
        // The callback implementation is assumed to uphold its contract.
        unsafe {
            (self.callbacks.on_requests)(
                self.callbacks.context,
                std::ptr::from_mut(&mut requests[0]),
                requests.len(),
            )
        }
    }

    /// Waits for there to be no requests in-flight.
    ///
    /// NOTE: To void TOCTOUs, this must be called on the same thread which calls
    /// [`Self::submit_requests`].
    fn wait_for_no_inflight_requests(&self) {
        let mut guard = self.inflight_requests.lock();
        self.no_inflight_requests_condvar.wait_while(&mut guard, |count| *count > 0);
    }

    /// Called instead of `[Self::complete_request]` when a request is completed before it was
    /// actually submitted.
    fn complete_unsubmitted_request(&self, request_id: RequestId, status: zx::Status) {
        if let Some((session, response)) =
            self.active_requests.complete_and_take_response(request_id, status)
        {
            session.send_response(response);
        }
    }

    fn complete_request(&self, request_id: RequestId, status: zx::Status) {
        let notify = {
            let mut inflight_requests = self.inflight_requests.lock();
            *inflight_requests -= 1;
            *inflight_requests == 0
        };
        self.complete_unsubmitted_request(request_id, status);
        if notify {
            self.no_inflight_requests_condvar.notify_all();
        }
    }

    fn terminate(&self) {
        {
            // We must drop references to sessions whilst we're not holding the lock for
            // `open_sessions` because `Session::drop` needs to take that same lock.
            #[allow(clippy::collection_is_never_read)]
            let mut terminated_sessions = Vec::new();
            for (_, session) in &*self.open_sessions.lock() {
                if let Some(session) = session.upgrade() {
                    session.terminate();
                    terminated_sessions.push(session);
                }
            }
        }
        let mut guard = self.open_sessions.lock();
        self.open_sessions_condvar.wait_while(&mut guard, |s| !s.is_empty());
    }
}

impl super::SessionManager for SessionManager {
    const SUPPORTS_DECOMPRESSION: bool = false;
    type Session = Arc<Session>;

    async fn on_attach_vmo(self: Arc<Self>, _vmo: &Arc<zx::Vmo>) -> Result<(), zx::Status> {
        Ok(())
    }

    async fn open_session(
        self: Arc<Self>,
        mut stream: fblock::SessionRequestStream,
        offset_map: OffsetMap,
        block_size: u32,
    ) -> Result<(), Error> {
        let (helper, fifo) = SessionHelper::new(self.clone(), offset_map, block_size)?;
        let (abort_handle, registration) = AbortHandle::new_pair();
        let session = Arc::new(Session {
            manager: self.clone(),
            helper,
            fifo,
            queue: Mutex::default(),
            abort_handle,
        });
        self.open_sessions.lock().insert(Arc::as_ptr(&session) as usize, Arc::downgrade(&session));
        unsafe {
            (self.callbacks.on_new_session)(self.callbacks.context, Arc::into_raw(session.clone()));
        }

        let result = Abortable::new(
            async {
                while let Some(request) = stream.try_next().await? {
                    session.helper.handle_request(request).await?;
                }
                Ok(())
            },
            registration,
        )
        .await
        .unwrap_or_else(|e| Err(e.into()));

        let _ = session.fifo.signal(zx::Signals::empty(), zx::Signals::USER_0);

        result
    }

    fn get_info(&self) -> Cow<'_, super::DeviceInfo> {
        Cow::Borrowed(&self.info)
    }

    fn active_requests(&self) -> &ActiveRequests<Arc<Session>> {
        &self.active_requests
    }
}

impl Drop for SessionManager {
    fn drop(&mut self) {
        self.terminate();
    }
}

impl IntoSessionManager for Arc<SessionManager> {
    type SM = SessionManager;

    fn into_session_manager(self) -> Self {
        self
    }
}

#[repr(C)]
pub struct Callbacks {
    /// An opaque context object retained by this library.  The library will pass this back into all
    /// callbacks.  The memory pointed to by `context` must last until [`block_server_delete`] is
    /// called.
    pub context: *mut c_void,
    /// Starts a thread.  The implementation must call [`block_server_thread`] on this newly created
    /// thread, providing `arg`.  The implementation must then call [`block_server_thread_delete`]
    /// after [`block_server_thread`] returns (but before [`block_server_delete`] is called).
    pub start_thread: unsafe extern "C" fn(context: *mut c_void, arg: *const c_void),
    /// Notifies the implementation of a new session.  The implementation must call
    /// [`block_server_session_run`] on a separate thread, and must call
    /// [`block_server_session_release`] after [`block_server_session_run`] (but before
    /// [`block_server_delete`] is called).
    pub on_new_session: unsafe extern "C" fn(context: *mut c_void, session: *const Session),
    /// Submits a batch of requests to be handled by the implementation.  The implementation must
    /// not retain references to `requests` after it returns.  The implementation must ensure that
    /// [`block_server_send_reply`] is called exactly once with the request ID of each entry in
    /// `requests`, regardless of its status; this call can be asynchronous but must occur before
    /// [`block_server_delete`] is called.
    pub on_requests:
        unsafe extern "C" fn(context: *mut c_void, requests: *mut Request, request_count: usize),
    /// Logs `message` to the implementation's logger.  The implementation must not retain
    /// references to `message`.
    pub log: unsafe extern "C" fn(context: *mut c_void, message: *const c_char, message_len: usize),
}

impl Callbacks {
    #[allow(dead_code)]
    fn log(&self, msg: &str) {
        let msg = msg.as_bytes();
        // SAFETY: This is safe if `context` and `log` are good.
        unsafe {
            (self.log)(self.context, msg.as_ptr() as *const c_char, msg.len());
        }
    }
}

/// cbindgen:no-export
#[allow(dead_code)]
pub struct UnownedVmo(zx::sys::zx_handle_t);

#[repr(C)]
pub struct Request {
    pub request_id: RequestId,
    pub operation: Operation,
    pub trace_flow_id: TraceFlowId,
    pub vmo: UnownedVmo,
}

unsafe impl Send for Callbacks {}
unsafe impl Sync for Callbacks {}

pub struct Session {
    manager: Arc<SessionManager>,
    helper: SessionHelper<SessionManager>,
    fifo: zx::Fifo<BlockFifoRequest, BlockFifoResponse>,
    queue: Mutex<SessionQueue>,
    abort_handle: AbortHandle,
}

#[derive(Default)]
struct SessionQueue {
    responses: VecDeque<BlockFifoResponse>,
}

pub const MAX_REQUESTS: usize = super::FIFO_MAX_REQUESTS;

impl Session {
    fn run(self: &Arc<Self>) {
        self.fifo_loop();
        self.abort_handle.abort();
        self.helper.drop_active_requests(|s| Arc::ptr_eq(s, self));
    }

    fn fifo_loop(self: &Arc<Self>) {
        let mut requests = [MaybeUninit::uninit(); MAX_REQUESTS];

        loop {
            // Send queued responses.
            let is_queue_empty = {
                let mut queue = self.queue.lock();
                while !queue.responses.is_empty() {
                    let (front, _) = queue.responses.as_slices();
                    match self.fifo.write(front) {
                        Ok(count) => {
                            let full = count < front.len();
                            queue.responses.drain(..count);
                            if full {
                                break;
                            }
                        }
                        Err(zx::Status::SHOULD_WAIT) => break,
                        Err(_) => return,
                    }
                }
                queue.responses.is_empty()
            };

            // Process pending reads.
            match self.fifo.read_uninit(&mut requests) {
                Ok(valid_requests) => self.handle_requests(valid_requests.iter_mut()),
                Err(zx::Status::SHOULD_WAIT) => {
                    let mut signals =
                        zx::Signals::OBJECT_READABLE | zx::Signals::USER_0 | zx::Signals::USER_1;
                    if !is_queue_empty {
                        signals |= zx::Signals::OBJECT_WRITABLE;
                    }
                    let Ok(signals) =
                        self.fifo.wait_one(signals, zx::MonotonicInstant::INFINITE).to_result()
                    else {
                        return;
                    };
                    if signals.contains(zx::Signals::USER_0) {
                        return;
                    }
                    // Clear USER_1 signal if it's set.
                    if signals.contains(zx::Signals::USER_1) {
                        let _ = self.fifo.signal(zx::Signals::USER_1, zx::Signals::empty());
                    }
                }
                Err(_) => return,
            }
        }
    }

    /// Synchronously performs a device flush.
    fn pre_flush(self: &Arc<Self>, request_id: RequestId) -> Result<(), zx::Status> {
        let trace_flow_id = {
            let mut request = self.manager.active_requests.request(request_id);
            if let Some(id) = request.trace_flow_id {
                fuchsia_trace::async_instant!(
                    fuchsia_trace::Id::from(id.get()),
                    c"storage",
                    c"block_server::SimulatedBarrier",
                    "request_id" => request_id.0
                );
            }
            request.count += 1;
            request.trace_flow_id
        };
        self.manager.submit_requests(&mut [Request {
            request_id,
            operation: Operation::Flush,
            trace_flow_id,
            vmo: UnownedVmo(zx::sys::ZX_HANDLE_INVALID),
        }]);
        self.manager.wait_for_no_inflight_requests();
        let status = self.manager.active_requests.request(request_id).status;
        match status {
            zx::Status::OK => Ok(()),
            status => {
                // Respond for the unsubmitted request too.
                self.manager.complete_unsubmitted_request(request_id, status);
                Err(status)
            }
        }
    }

    /// Synchronously completes `decoded_requests`, and inserts a post-flush into `decoded_requests`
    /// to be submitted later.
    fn post_flush(self: &Arc<Self>, request_id: RequestId, decoded_requests: &mut DecodedRequests) {
        if decoded_requests.len() > 0 {
            self.manager.submit_requests(decoded_requests);
            decoded_requests.clear();
        }
        self.manager.wait_for_no_inflight_requests();
        let request = self.manager.active_requests.request(request_id);
        match request.status {
            zx::Status::OK => decoded_requests.push(Request {
                request_id,
                operation: Operation::Flush,
                trace_flow_id: request.trace_flow_id,
                vmo: UnownedVmo(zx::sys::ZX_HANDLE_INVALID),
            }),
            status => {
                drop(request);
                self.manager.complete_unsubmitted_request(request_id, status)
            }
        }
    }

    fn handle_requests<'a>(
        self: &Arc<Self>,
        requests: impl Iterator<Item = &'a mut BlockFifoRequest>,
    ) {
        let mut decoded_requests = DecodedRequests::default();
        for request in requests {
            match self.helper.decode_fifo_request(self.clone(), request) {
                Ok(DecodedRequest { operation: Operation::CloseVmo, request_id, .. }) => {
                    self.manager.complete_unsubmitted_request(request_id, zx::Status::OK);
                }
                Ok(mut request) => {
                    let request_id = request.request_id;
                    // Strip the PRE_BARRIER flag if we don't support it, and simulate the barrier
                    // with a pre-flush.
                    if !self
                        .manager
                        .info
                        .device_flags()
                        .contains(fblock::DeviceFlag::BARRIER_SUPPORT)
                        && request.operation.take_write_flag(WriteFlags::PRE_BARRIER)
                        && self.pre_flush(request_id).is_err()
                    {
                        continue;
                    }
                    // Strip the FORCE_ACCESS flag if we don't support it, and simulate the FUA with
                    // a post-flush.
                    let simulate_fua =
                        !self.manager.info.device_flags().contains(fblock::DeviceFlag::FUA_SUPPORT)
                            && request.operation.take_write_flag(WriteFlags::FORCE_ACCESS);
                    if simulate_fua {
                        // Account for the additional request we need at the end.
                        self.manager.active_requests.request(request_id).count += 1;
                    }

                    loop {
                        let result = self.helper.map_request(
                            request,
                            &mut self.manager.active_requests.request(request_id),
                        );
                        match result {
                            Ok((
                                DecodedRequest { request_id, operation, vmo, trace_flow_id },
                                remainder,
                            )) => {
                                // We are handing out unowned references to the VMO here.  This is
                                // safe because the VMO bin holds references to any closed VMOs
                                // until all preceding operations have finished.
                                decoded_requests.push(Request {
                                    request_id,
                                    operation,
                                    trace_flow_id,
                                    vmo: UnownedVmo(
                                        vmo.as_ref()
                                            .map(|vmo| vmo.raw_handle())
                                            .unwrap_or(zx::sys::ZX_HANDLE_INVALID),
                                    ),
                                });

                                if decoded_requests.is_full() {
                                    self.manager.submit_requests(&mut decoded_requests);
                                    decoded_requests.clear();
                                }
                                if let Some(r) = remainder {
                                    request = r;
                                } else {
                                    break;
                                }
                            }
                            Err(status) => {
                                self.manager.complete_unsubmitted_request(request_id, status);
                                break;
                            }
                        }
                    }

                    if simulate_fua {
                        self.post_flush(request_id, &mut decoded_requests);
                    }
                }
                Err(None) => {}
                Err(Some(response)) => self.send_response(response),
            }
        }
        if !decoded_requests.is_empty() {
            self.manager.submit_requests(&mut decoded_requests);
        }
    }

    fn send_response(&self, response: BlockFifoResponse) {
        let mut queue = self.queue.lock();
        if queue.responses.is_empty() {
            match self.fifo.write_one(&response) {
                Ok(()) => {
                    return;
                }
                Err(_) => {
                    // Wake `fifo_loop`.
                    let _ = self.fifo.signal(zx::Signals::empty(), zx::Signals::USER_1);
                }
            }
        }
        queue.responses.push_back(response);
    }

    fn terminate(&self) {
        let _ = self.fifo.signal(zx::Signals::empty(), zx::Signals::USER_0);
        self.abort_handle.abort();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let notify = {
            let mut open_sessions = self.manager.open_sessions.lock();
            open_sessions.remove(&(self as *const _ as usize));
            open_sessions.is_empty()
        };
        if notify {
            self.manager.open_sessions_condvar.notify_all();
        }
    }
}

pub struct BlockServer {
    server: super::BlockServer<SessionManager>,
    ehandle: EHandle,
    abort_handle: AbortHandle,
}

struct ExecutorMailbox(Mutex<Mail>, Condvar);

impl ExecutorMailbox {
    /// Returns the old mail.
    fn post(&self, mail: Mail) -> Mail {
        let old = std::mem::replace(&mut *self.0.lock(), mail);
        self.1.notify_all();
        old
    }

    fn new() -> Self {
        Self(Mutex::default(), Condvar::new())
    }
}

type ShutdownCallback = unsafe extern "C" fn(*mut c_void);

#[derive(Default)]
enum Mail {
    #[default]
    None,
    Initialized(EHandle, AbortHandle),
    AsyncShutdown(Box<BlockServer>, ShutdownCallback, *mut c_void),
    Finished,
}

impl Drop for BlockServer {
    fn drop(&mut self) {
        self.abort_handle.abort();
        let manager = &self.server.session_manager;
        let mut mbox = manager.mbox.0.lock();
        manager.mbox.1.wait_while(&mut mbox, |mbox| !matches!(mbox, Mail::Finished));
        manager.terminate();
        debug_assert!(Arc::strong_count(manager) > 0);
    }
}

#[repr(C)]
pub struct PartitionInfo {
    pub device_flags: u32,
    pub start_block: u64,
    pub block_count: u64,
    pub block_size: u32,
    pub type_guid: [u8; 16],
    pub instance_guid: [u8; 16],
    pub name: *const c_char,
    pub flags: u64,
    pub max_transfer_size: u32,
}

/// cbindgen:no-export
#[allow(non_camel_case_types)]
type zx_handle_t = zx::sys::zx_handle_t;

/// cbindgen:no-export
#[allow(non_camel_case_types)]
type zx_status_t = zx::sys::zx_status_t;

impl PartitionInfo {
    /// # Safety
    ///
    /// [`self.name`] must point to valid, null-terminated C-string, or be a nullptr.
    unsafe fn to_rust(&self) -> super::DeviceInfo {
        super::DeviceInfo::Partition(super::PartitionInfo {
            device_flags: fblock::DeviceFlag::from_bits_truncate(self.device_flags),
            block_range: Some(self.start_block..self.start_block + self.block_count),
            type_guid: self.type_guid,
            instance_guid: self.instance_guid,
            name: if self.name.is_null() {
                "".to_string()
            } else {
                String::from_utf8_lossy(unsafe { CStr::from_ptr(self.name).to_bytes() }).to_string()
            },
            flags: self.flags,
            max_transfer_blocks: if self.max_transfer_size != MAX_TRANSFER_UNBOUNDED {
                NonZero::new(self.max_transfer_size / self.block_size)
            } else {
                None
            },
        })
    }
}

struct DecodedRequests {
    requests: [MaybeUninit<Request>; MAX_REQUESTS],
    count: usize,
}

impl Default for DecodedRequests {
    fn default() -> Self {
        Self { requests: unsafe { MaybeUninit::uninit().assume_init() }, count: 0 }
    }
}

impl DecodedRequests {
    fn push(&mut self, request: Request) {
        assert!(self.count < MAX_REQUESTS);
        self.requests[self.count].write(request);
        self.count += 1;
    }

    fn is_full(&self) -> bool {
        self.count == MAX_REQUESTS
    }

    fn clear(&mut self) {
        self.count = 0;
    }
}

impl Deref for DecodedRequests {
    type Target = [Request];

    fn deref(&self) -> &Self::Target {
        // SAFETY: We wrote the request in [`Self::push`].
        unsafe { std::slice::from_raw_parts(self.requests[0].as_ptr(), self.count) }
    }
}

impl DerefMut for DecodedRequests {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: We wrote the request in [`Self::push`].
        unsafe { std::slice::from_raw_parts_mut(self.requests[0].as_mut_ptr(), self.count) }
    }
}

/// # Safety
///
/// All callbacks in `callbacks` must be safe.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn block_server_new(
    partition_info: &PartitionInfo,
    callbacks: Callbacks,
) -> *mut BlockServer {
    let session_manager = Arc::new(SessionManager {
        callbacks,
        open_sessions: Mutex::default(),
        active_requests: ActiveRequests::default(),
        open_sessions_condvar: Condvar::new(),
        inflight_requests: Mutex::default(),
        no_inflight_requests_condvar: Condvar::new(),
        mbox: ExecutorMailbox::new(),
        info: unsafe { partition_info.to_rust() },
    });

    unsafe {
        (session_manager.callbacks.start_thread)(
            session_manager.callbacks.context,
            Arc::into_raw(session_manager.clone()) as *const c_void,
        );
    }

    let mbox = &session_manager.mbox;
    let mail = {
        let mut mail = mbox.0.lock();
        mbox.1.wait_while(&mut mail, |mail| matches!(mail, Mail::None));
        std::mem::replace(&mut *mail, Mail::None)
    };

    let block_size = partition_info.block_size;
    match mail {
        Mail::Initialized(ehandle, abort_handle) => Box::into_raw(Box::new(BlockServer {
            server: super::BlockServer::new(block_size, session_manager),
            ehandle,
            abort_handle,
        })),
        Mail::Finished => std::ptr::null_mut(),
        _ => unreachable!(),
    }
}

/// # Safety
///
/// `arg` must be the value passed to the `start_thread` callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn block_server_thread(arg: *const c_void) {
    let session_manager = unsafe { &*(arg as *const SessionManager) };

    let mut executor = fasync::LocalExecutor::default();
    let (abort_handle, registration) = AbortHandle::new_pair();

    session_manager.mbox.post(Mail::Initialized(EHandle::local(), abort_handle));

    let _ = executor.run_singlethreaded(Abortable::new(std::future::pending::<()>(), registration));
}

/// Called to delete the thread.  This *must* always be called, regardless of whether starting the
/// thread is successful or not.
///
/// # Safety
///
/// `arg` must be the value passed to the `start_thread` callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn block_server_thread_delete(arg: *const c_void) {
    let mail = {
        let session_manager = unsafe { Arc::from_raw(arg as *const SessionManager) };
        debug_assert!(Arc::strong_count(&session_manager) > 0);
        session_manager.mbox.post(Mail::Finished)
    };

    if let Mail::AsyncShutdown(server, callback, arg) = mail {
        std::mem::drop(server);
        // SAFETY: Whoever supplied the callback must guarantee it's safe.
        unsafe {
            callback(arg);
        }
    }
}

/// # Safety
///
/// `block_server` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn block_server_delete(block_server: *mut BlockServer) {
    let _ = unsafe { Box::from_raw(block_server) };
}

/// # Safety
///
/// `block_server` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn block_server_delete_async(
    block_server: *mut BlockServer,
    callback: ShutdownCallback,
    arg: *mut c_void,
) {
    let block_server = unsafe { Box::from_raw(block_server) };
    let session_manager = block_server.server.session_manager.clone();
    let abort_handle = block_server.abort_handle.clone();
    session_manager.mbox.post(Mail::AsyncShutdown(block_server, callback, arg));
    abort_handle.abort();
}

/// Serves the Volume protocol for this server.  `handle` is consumed.
///
/// # Safety
///
/// `block_server` and `handle` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn block_server_serve(block_server: *const BlockServer, handle: zx_handle_t) {
    let block_server = unsafe { &*block_server };
    let ehandle = &block_server.ehandle;
    let handle = unsafe { zx::NullableHandle::from_raw(handle) };
    ehandle.global_scope().spawn(async move {
        let _ = block_server
            .server
            .handle_requests(fblock::BlockRequestStream::from_channel(
                fasync::Channel::from_channel(handle.into()),
            ))
            .await;
    });
}

/// # Safety
///
/// `session` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn block_server_session_run(session: &Session) {
    let session = unsafe { Arc::from_raw(session) };
    session.run();
    let _ = Arc::into_raw(session);
}

/// # Safety
///
/// `session` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn block_server_session_release(session: &Session) {
    session.terminate();
    unsafe { Arc::from_raw(session) };
}

/// # Safety
///
/// `block_server` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn block_server_send_reply(
    block_server: &BlockServer,
    request_id: RequestId,
    status: zx_status_t,
) {
    block_server.server.session_manager.complete_request(request_id, zx::Status::from_raw(status));
}
