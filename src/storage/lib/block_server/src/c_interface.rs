// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::{Operation, RequestId, TraceFlowId};
use crate::{IntoOrchestrator, callback_interface};
use fidl::endpoints::RequestStream;
use fidl_fuchsia_storage_block as fblock;
use fidl_fuchsia_storage_block::MAX_TRANSFER_UNBOUNDED;
use fuchsia_async::{self as fasync, EHandle};
use fuchsia_sync::{Condvar, Mutex};
use futures::stream::AbortHandle;
use std::borrow::{Borrow, Cow};
use std::ffi::{CStr, c_char, c_void};
use std::num::NonZero;
use std::sync::Arc;

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
    pub on_new_session: unsafe extern "C" fn(
        context: *mut c_void,
        session: *const callback_interface::Session<InterfaceAdapter>,
    ),
    /// Submits a batch of requests to be handled by the implementation.  The implementation must
    /// not retain references to `requests` after it returns.  The implementation must ensure that
    /// [`block_server_send_reply`] is called exactly once with the request ID of each entry in
    /// `requests`, regardless of its status; this call can be asynchronous but must occur before
    /// [`block_server_delete`] is called.  Note that a reply must be sent for every request before
    /// shutdown.
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

/// Implements [`callback_interface::Interface`] using C callbacks.
pub struct InterfaceAdapter {
    callbacks: Callbacks,
    info: super::DeviceInfo,
}

impl callback_interface::Interface for InterfaceAdapter {
    type Orchestrator = Orchestrator;

    fn get_info(&self) -> Cow<'_, super::DeviceInfo> {
        Cow::Borrowed(&self.info)
    }

    fn spawn_session(&self, session: Arc<callback_interface::Session<Self>>) {
        unsafe {
            (self.callbacks.on_new_session)(self.callbacks.context, Arc::into_raw(session));
        }
    }

    fn on_requests(&self, requests: &[callback_interface::Request]) {
        let mut c_requests = Vec::with_capacity(requests.len());
        for req in requests {
            c_requests.push(Request {
                request_id: req.request_id,
                operation: req.operation.clone(),
                trace_flow_id: req.trace_flow_id,
                // We are handing out unowned references to the VMO here.  This is safe because the
                // VMO bin holds references to any closed VMOs until all preceding operations have
                // finished.
                vmo: UnownedVmo(
                    req.vmo.as_ref().map(|v| v.raw_handle()).unwrap_or(zx::sys::ZX_HANDLE_INVALID),
                ),
            });
        }
        unsafe {
            (self.callbacks.on_requests)(
                self.callbacks.context,
                c_requests.as_mut_ptr(),
                c_requests.len(),
            )
        }
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

struct ExecutorMailbox(Mutex<Mail>, Condvar);

impl ExecutorMailbox {
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

#[derive(Clone, Copy)]
#[repr(transparent)]
struct ContextPtr(*mut c_void);

// SAFETY: `ContextPtr` wraps a `*mut c_void` representing an opaque context pointer. Thread safety
// for this pointer is guaranteed by the caller's C API contract.
unsafe impl Send for ContextPtr {}
unsafe impl Sync for ContextPtr {}

#[derive(Default)]
enum Mail {
    #[default]
    None,
    Initialized(EHandle, AbortHandle),
    AsyncShutdown(Box<BlockServer>, ShutdownCallback, ContextPtr),
    Finished,
}

pub struct Orchestrator {
    session_manager: callback_interface::SessionManager<InterfaceAdapter>,
    mbox: ExecutorMailbox,
}

impl IntoOrchestrator for Arc<Orchestrator> {
    type SM = callback_interface::SessionManager<InterfaceAdapter>;

    fn into_orchestrator(self) -> Arc<Orchestrator> {
        self
    }
}

impl Borrow<callback_interface::SessionManager<InterfaceAdapter>> for Orchestrator {
    fn borrow(&self) -> &callback_interface::SessionManager<InterfaceAdapter> {
        &self.session_manager
    }
}

pub struct BlockServer {
    server: super::BlockServer<callback_interface::SessionManager<InterfaceAdapter>>,
    ehandle: EHandle,
    abort_handle: AbortHandle,
    orchestrator: Arc<Orchestrator>,
}

impl Drop for BlockServer {
    fn drop(&mut self) {
        self.abort_handle.abort();
        Borrow::<callback_interface::SessionManager<InterfaceAdapter>>::borrow(
            self.orchestrator.as_ref(),
        )
        .terminate();
        let mbox = &self.orchestrator.mbox;
        let mut mail = mbox.0.lock();
        mbox.1.wait_while(&mut mail, |mbox| !matches!(mbox, Mail::Finished));
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
    let start_thread = callbacks.start_thread;
    let context = callbacks.context;

    let session_manager = callback_interface::SessionManager::new(Arc::new(InterfaceAdapter {
        callbacks,
        info: unsafe { partition_info.to_rust() },
    }));

    let orchestrator = Arc::new(Orchestrator { session_manager, mbox: ExecutorMailbox::new() });

    unsafe {
        (start_thread)(context, Arc::into_raw(orchestrator.clone()) as *const c_void);
    }

    let mbox = &orchestrator.mbox;
    let mail = {
        let mut mail = mbox.0.lock();
        mbox.1.wait_while(&mut mail, |mail| matches!(mail, Mail::None));
        std::mem::replace(&mut *mail, Mail::None)
    };

    let block_size = partition_info.block_size;
    match mail {
        Mail::Initialized(ehandle, abort_handle) => Box::into_raw(Box::new(BlockServer {
            server: super::BlockServer::new(block_size, orchestrator.clone()),
            ehandle,
            abort_handle,
            orchestrator: orchestrator.clone(),
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
    let orchestrator = unsafe { &*(arg as *const Orchestrator) };

    let mut executor = fasync::LocalExecutor::default();
    let (abort_handle, registration) = futures::stream::AbortHandle::new_pair();

    orchestrator.mbox.post(Mail::Initialized(EHandle::local(), abort_handle));

    let _ = executor.run_singlethreaded(futures::stream::Abortable::new(
        std::future::pending::<()>(),
        registration,
    ));
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
        let orchestrator = unsafe { Arc::from_raw(arg as *const Orchestrator) };
        orchestrator.mbox.post(Mail::Finished)
    };

    if let Mail::AsyncShutdown(server, callback, arg) = mail {
        std::mem::drop(server);
        // SAFETY: Whoever supplied the callback must guarantee it's safe.
        unsafe {
            callback(arg.0);
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
    let orchestrator = block_server.orchestrator.clone();
    let abort_handle = block_server.abort_handle.clone();
    orchestrator.mbox.post(Mail::AsyncShutdown(block_server, callback, ContextPtr(arg)));
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
pub unsafe extern "C" fn block_server_session_run(
    session: &callback_interface::Session<InterfaceAdapter>,
) {
    let session = unsafe { Arc::from_raw(session) };
    session.run();
    let _ = Arc::into_raw(session);
}

/// # Safety
///
/// `session` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn block_server_session_release(
    session: &callback_interface::Session<InterfaceAdapter>,
) {
    session.terminate_async();
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
    block_server
        .orchestrator
        .session_manager
        .complete_request(request_id, zx::Status::from_raw(status));
}
