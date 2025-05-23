// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::{
    DecodeResult, IntoSessionManager, OffsetMap, Operation, RequestTracking, SessionHelper,
};
use anyhow::Error;
use block_protocol::{BlockFifoRequest, BlockFifoResponse};
use fidl::endpoints::RequestStream;
use fidl_fuchsia_hardware_block::MAX_TRANSFER_UNBOUNDED;
use fuchsia_async::{self as fasync, EHandle};
use fuchsia_sync::{Condvar, Mutex};
use futures::stream::{AbortHandle, Abortable};
use futures::TryStreamExt;
use slab::Slab;
use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::ffi::{c_char, c_void, CStr};
use std::mem::MaybeUninit;
use std::num::NonZero;
use std::sync::{Arc, Weak};
use zx::{self as zx, AsHandleRef as _};
use {fidl_fuchsia_hardware_block as fblock, fidl_fuchsia_hardware_block_volume as fvolume};

/// We internally keep track of active requests, so that when the server is torn down, we can
/// deallocate all of the resources for pending requests.
struct ActiveRequest {
    session: Arc<Session>,
    request_tracking: RequestTracking,
    // Retain a stronng reference to the VMO the request targets while it is active.
    _vmo: Option<Arc<zx::Vmo>>,
}

pub struct SessionManager {
    callbacks: Callbacks,
    open_sessions: Mutex<HashMap<usize, Weak<Session>>>,
    active_requests: Mutex<Slab<ActiveRequest>>,
    condvar: Condvar,
    mbox: ExecutorMailbox,
    info: super::DeviceInfo,
}

unsafe impl Send for SessionManager {}
unsafe impl Sync for SessionManager {}

impl SessionManager {
    fn start_request(
        &self,
        session: Arc<Session>,
        request_tracking: RequestTracking,
        operation: Operation,
        vmo: Option<Arc<zx::Vmo>>,
    ) -> Request {
        let mut active_requests = self.active_requests.lock();
        let vacant = active_requests.vacant_entry();
        let request = Request {
            request_id: RequestId(vacant.key()),
            operation,
            trace_flow_id: request_tracking.trace_flow_id,
            vmo: UnownedVmo(
                vmo.as_ref().map(|vmo| vmo.raw_handle()).unwrap_or(zx::sys::ZX_HANDLE_INVALID),
            ),
        };
        vacant.insert(ActiveRequest { session, request_tracking, _vmo: vmo });
        request
    }

    fn complete_request(&self, request_id: RequestId, status: zx::Status) {
        let request = self
            .active_requests
            .lock()
            .try_remove(request_id.0)
            .unwrap_or_else(|| panic!("Invalid request id {}", request_id.0));
        request.session.send_reply(request.request_tracking, status);
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
        self.condvar.wait_while(&mut guard, |s| !s.is_empty());
    }
}

impl super::SessionManager for SessionManager {
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

        let _ = session.fifo.signal_handle(zx::Signals::empty(), zx::Signals::USER_0);

        result
    }

    async fn get_info(&self) -> Result<Cow<'_, super::DeviceInfo>, zx::Status> {
        Ok(Cow::Borrowed(&self.info))
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
    pub context: *mut c_void,
    pub start_thread: unsafe extern "C" fn(context: *mut c_void, arg: *const c_void),
    pub on_new_session: unsafe extern "C" fn(context: *mut c_void, session: *const Session),
    pub on_requests:
        unsafe extern "C" fn(context: *mut c_void, requests: *mut Request, request_count: usize),
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

#[repr(transparent)]
#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub struct RequestId(usize);

#[repr(C)]
pub struct Request {
    pub request_id: RequestId,
    pub operation: Operation,
    pub trace_flow_id: Option<NonZero<u64>>,
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
    fn run(&self) {
        self.fifo_loop();
        self.abort_handle.abort();
    }

    fn fifo_loop(&self) {
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
                        self.fifo.wait_handle(signals, zx::MonotonicInstant::INFINITE).to_result()
                    else {
                        return;
                    };
                    if signals.contains(zx::Signals::USER_0) {
                        return;
                    }
                    // Clear USER_1 signal if it's set.
                    if signals.contains(zx::Signals::USER_1) {
                        let _ = self.fifo.signal_handle(zx::Signals::USER_1, zx::Signals::empty());
                    }
                }
                Err(_) => return,
            }
        }
    }

    fn handle_requests<'a>(&self, requests: impl Iterator<Item = &'a mut BlockFifoRequest>) {
        let mut c_requests: [MaybeUninit<Request>; MAX_REQUESTS] =
            unsafe { MaybeUninit::uninit().assume_init() };
        let mut count = 0;
        let this = self
            .manager
            .open_sessions
            .lock()
            .get(&(self as *const _ as usize))
            .and_then(Weak::upgrade)
            .unwrap();
        for request in requests {
            let mut in_split = false;
            loop {
                if count >= MAX_REQUESTS {
                    unsafe {
                        (self.manager.callbacks.on_requests)(
                            self.manager.callbacks.context,
                            c_requests[0].as_mut_ptr(),
                            count,
                        );
                    }
                    count = 0;
                }
                match self.helper.decode_fifo_request(request, in_split) {
                    DecodeResult::Ok(decoded_request) => {
                        if let Operation::CloseVmo = decoded_request.operation {
                            self.send_reply(decoded_request.request_tracking, zx::Status::OK);
                            break;
                        }
                        let c_request = self.manager.start_request(
                            this.clone(),
                            decoded_request.request_tracking,
                            decoded_request.operation,
                            decoded_request.vmo,
                        );
                        c_requests[count].write(c_request);
                        count += 1;
                        break;
                    }
                    DecodeResult::Split(decoded_request) => {
                        let c_request = self.manager.start_request(
                            this.clone(),
                            decoded_request.request_tracking,
                            decoded_request.operation,
                            decoded_request.vmo,
                        );
                        c_requests[count].write(c_request);
                        count += 1;
                        in_split = true;
                    }
                    DecodeResult::InvalidRequest(tracking, status) => {
                        self.send_reply(tracking, status);
                        break;
                    }
                    DecodeResult::IgnoreRequest => {
                        break;
                    }
                }
            }
        }
        if count > 0 {
            unsafe {
                (self.manager.callbacks.on_requests)(
                    self.manager.callbacks.context,
                    c_requests[0].as_mut_ptr(),
                    count,
                );
            }
        }
    }

    fn send_reply(&self, tracking: RequestTracking, status: zx::Status) {
        let response = match self.helper.finish_fifo_request(tracking, status) {
            Some(response) => response,
            None => return,
        };
        let mut queue = self.queue.lock();
        if queue.responses.is_empty() {
            match self.fifo.write_one(&response) {
                Ok(()) => {
                    return;
                }
                Err(_) => {
                    // Wake `fifo_loop`.
                    let _ = self.fifo.signal_handle(zx::Signals::empty(), zx::Signals::USER_1);
                }
            }
        }
        queue.responses.push_back(response);
    }

    fn terminate(&self) {
        let _ = self.fifo.signal_handle(zx::Signals::empty(), zx::Signals::USER_0);
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
            self.manager.condvar.notify_all();
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
    unsafe fn to_rust(&self) -> super::DeviceInfo {
        super::DeviceInfo::Partition(super::PartitionInfo {
            device_flags: fblock::Flag::from_bits_truncate(self.device_flags),
            block_range: Some(self.start_block..self.start_block + self.block_count),
            type_guid: self.type_guid,
            instance_guid: self.instance_guid,
            name: if self.name.is_null() {
                "".to_string()
            } else {
                String::from_utf8_lossy(CStr::from_ptr(self.name).to_bytes()).to_string()
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

/// # Safety
///
/// All callbacks in `callbacks` must be safe.
#[no_mangle]
pub unsafe extern "C" fn block_server_new(
    partition_info: &PartitionInfo,
    callbacks: Callbacks,
) -> *mut BlockServer {
    let session_manager = Arc::new(SessionManager {
        callbacks,
        open_sessions: Mutex::default(),
        active_requests: Mutex::new(Slab::with_capacity(MAX_REQUESTS)),
        condvar: Condvar::new(),
        mbox: ExecutorMailbox::new(),
        info: partition_info.to_rust(),
    });

    (session_manager.callbacks.start_thread)(
        session_manager.callbacks.context,
        Arc::into_raw(session_manager.clone()) as *const c_void,
    );

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
#[no_mangle]
pub unsafe extern "C" fn block_server_thread(arg: *const c_void) {
    let session_manager = &*(arg as *const SessionManager);

    let mut executor = fasync::LocalExecutor::new();
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
#[no_mangle]
pub unsafe extern "C" fn block_server_thread_delete(arg: *const c_void) {
    let mail = {
        let session_manager = Arc::from_raw(arg as *const SessionManager);
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
#[no_mangle]
pub unsafe extern "C" fn block_server_delete(block_server: *mut BlockServer) {
    let _ = Box::from_raw(block_server);
}

/// # Safety
///
/// `block_server` must be valid.
#[no_mangle]
pub unsafe extern "C" fn block_server_delete_async(
    block_server: *mut BlockServer,
    callback: ShutdownCallback,
    arg: *mut c_void,
) {
    let block_server = Box::from_raw(block_server);
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
#[no_mangle]
pub unsafe extern "C" fn block_server_serve(block_server: *const BlockServer, handle: zx_handle_t) {
    let block_server = &*block_server;
    let ehandle = &block_server.ehandle;
    let handle = zx::Handle::from_raw(handle);
    ehandle.global_scope().spawn(async move {
        let _ = block_server
            .server
            .handle_requests(fvolume::VolumeRequestStream::from_channel(
                fasync::Channel::from_channel(handle.into()),
            ))
            .await;
    });
}

/// # Safety
///
/// `session` must be valid.
#[no_mangle]
pub unsafe extern "C" fn block_server_session_run(session: &Session) {
    session.run();
}

/// # Safety
///
/// `session` must be valid.
#[no_mangle]
pub unsafe extern "C" fn block_server_session_release(session: &Session) {
    session.terminate();
    Arc::from_raw(session);
}

/// # Safety
///
/// `block_server` must be valid.
#[no_mangle]
pub unsafe extern "C" fn block_server_send_reply(
    block_server: &BlockServer,
    request_id: RequestId,
    status: zx_status_t,
) {
    block_server.server.session_manager.complete_request(request_id, zx::Status::from_raw(status));
}
