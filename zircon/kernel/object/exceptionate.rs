// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::channel_dispatcher::ChannelDispatcher;
use super::exception_dispatcher::ExceptionDispatcher;
use super::handle::{HandleOwner, KernelHandle};
use super::message_packet::MessagePacket;
use core::convert::Infallible;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, Ordering};
use fbl::RefPtr;
use ksync::{KMutex, RawMutex, guarded};
use object_constants_rs as object_constants;
use pin_init::{PinInit, pin_data, pin_init, pinned_drop};
use zerocopy::{FromZeros, IntoBytes};
use zx_status::Status;
use zx_types::{ZX_KOID_INVALID, zx_exception_info_t, zx_rights_t};

/// Kernel-owned exception channel endpoint.
///
/// This type is thread-safe and does not require external synchronization.
#[guarded]
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct Exceptionate {
    type_: u32,
    has_channel: AtomicBool,
    #[mutex]
    lock: KMutex<RawMutex>,
    #[guarded_by(lock)]
    channel_handle: KernelHandle<ChannelDispatcher>,
    #[guarded_by(lock)]
    thread_rights: zx_rights_t,
    #[guarded_by(lock)]
    process_rights: zx_rights_t,
    #[guarded_by(lock)]
    is_shutdown: bool,
}

zr::static_assert_size_and_align!(
    Exceptionate,
    object_constants::kExceptionateStorageSize,
    object_constants::kExceptionateStorageAlign,
);

impl Exceptionate {
    /// Returns a pinned initializer for a new `Exceptionate` of the given
    /// `ZX_EXCEPTION_CHANNEL_TYPE_*`.
    #[inline]
    pub fn new(type_: u32) -> impl PinInit<Self, Infallible> {
        pin_init!(Self {
            type_,
            has_channel: AtomicBool::new(false),
            lock <- KMutex::init(),
            channel_handle: KernelHandle::empty().into(),
            thread_rights: 0.into(),
            process_rights: 0.into(),
            is_shutdown: false.into(),
        })
    }

    /// Returns the `ZX_EXCEPTION_CHANNEL_TYPE_*` constant associated with this `Exceptionate`.
    #[inline]
    pub fn type_(&self) -> u32 {
        self.type_
    }

    /// Returns `true` if a channel handle is currently stored in this `Exceptionate`.
    ///
    /// This is a lock-free fast-path check; the peer endpoint may have closed since the channel
    /// was bound. Call [`Self::has_valid_channel`] to also verify that the peer endpoint is open.
    #[inline]
    pub fn has_channel(&self) -> bool {
        self.has_channel.load(Ordering::Acquire)
    }

    /// Sets the backing `ChannelDispatcher` endpoint.
    ///
    /// The exception channel is first-come-first-served, so if there is already a valid channel
    /// in place (i.e. has a live peer) this will fail.
    ///
    /// The `*_rights` arguments give the rights to assign to task handles provided through this
    /// exception channel. A value of 0 indicates that the handle should not be made available
    /// through this channel.
    ///
    /// Returns:
    /// * `Err(Status::INVALID_ARGS)` if `channel_handle` is empty.
    /// * `Err(Status::ALREADY_BOUND)` if there is already a valid channel.
    /// * `Err(Status::BAD_STATE)` if [`Self::shutdown`] has already been called.
    pub fn set_channel(
        &self,
        channel_handle: KernelHandle<ChannelDispatcher>,
        thread_rights: zx_rights_t,
        process_rights: zx_rights_t,
    ) -> Result<(), Status> {
        if !channel_handle.is_valid() {
            return Err(Status::INVALID_ARGS);
        }

        ksync::lock!(let mut guard = self.lock_lock());

        if *guard.fields().is_shutdown {
            return Err(Status::BAD_STATE);
        }

        if Self::has_valid_channel_locked(guard.fields().channel_handle) {
            return Err(Status::ALREADY_BOUND);
        }

        // At this point we're certain that either there is no channel or it's a
        // dead channel with no peer (since channel endpoints can't re-open) so we
        // can overwrite it.
        let fields = guard.as_mut().fields_mut();
        *fields.channel_handle = channel_handle;
        *fields.thread_rights = thread_rights;
        *fields.process_rights = process_rights;
        self.has_channel.store(true, Ordering::Release);

        Ok(())
    }

    /// Removes any exception channel, which will signal `ZX_CHANNEL_PEER_CLOSED` for the
    /// userspace endpoint.
    ///
    /// Any further attempt to set a new channel will return `Err(Status::BAD_STATE)`.
    pub fn shutdown(&self) {
        ksync::lock!(let mut guard = self.lock_lock());
        let fields = guard.as_mut().fields_mut();
        fields.channel_handle.reset();
        *fields.is_shutdown = true;
        self.has_channel.store(false, Ordering::Release);
    }

    /// Returns `true` if the channel exists and has a valid userspace peer.
    #[inline]
    pub fn has_valid_channel(&self) -> bool {
        if !self.has_channel() {
            return false;
        }
        self.has_valid_channel_slow()
    }

    #[cold]
    fn has_valid_channel_slow(&self) -> bool {
        ksync::lock!(let guard = self.lock_lock());
        Self::has_valid_channel_locked(guard.fields().channel_handle)
    }

    fn has_valid_channel_locked(channel_handle: &KernelHandle<ChannelDispatcher>) -> bool {
        channel_handle.is_valid() && !channel_handle.dispatcher().peer_has_closed()
    }

    /// Sends an exception to userspace.
    ///
    /// The exception message contains:
    ///  * 1 struct: `zx_exception_info_t`
    ///  * 1 handle: `ExceptionDispatcher`
    ///
    /// Returns:
    ///  * `Err(Status::NEXT)` if there is no valid underlying channel.
    ///  * `Err(Status::NO_MEMORY)` if we failed to allocate memory.
    pub fn send_exception(&self, exception: &RefPtr<ExceptionDispatcher>) -> Result<(), Status> {
        if !self.has_channel() {
            return Err(Status::NEXT);
        }

        ksync::lock!(let mut guard = self.lock_lock());

        if !guard.fields().channel_handle.is_valid() {
            return Err(Status::NEXT);
        }

        let mut info = zx_exception_info_t::new_zeroed();
        info.tid = exception.thread().get_koid();
        info.pid = exception.thread().process().get_koid();
        info.type_ = exception.exception_type();

        // Since `info` will be copied to a usermode process, `IntoBytes::as_bytes` verifies at
        // compile time that it is safe to copy (no internal padding).
        let mut message = MessagePacket::create_from_kernel(info.as_bytes(), 1)?;

        // It's OK if the function fails after this point, all exception sending
        // funnels through here so the task rights will get overwritten next time
        // we try to send it.
        //
        // This is safe to do because we know that an ExceptionDispatcher only goes
        // to one handler at a time, so we'll never change the task rights while
        // the exception is out in userspace.
        let thread_rights = *guard.fields().thread_rights;
        let process_rights = *guard.fields().process_rights;
        exception.set_task_rights(thread_rights, process_rights);

        let exception_handle =
            HandleOwner::make_from_ref(exception.clone(), ExceptionDispatcher::default_rights())
                .ok_or(Status::NO_MEMORY)?;
        // SAFETY: `message` was allocated with `num_handles = 1`, so `handles_mut()` points to a
        // valid, aligned slot for one handle pointer.
        unsafe {
            message.handles_mut().write(exception_handle.release().as_ptr().cast());
        }
        message.set_owns_handles(true);

        let result = guard.fields().channel_handle.dispatcher().write(ZX_KOID_INVALID, message);

        // If sending failed for any reason, the exception handle never made it to
        // userspace and has now gone out of scope, triggering on_zero_handles(),
        // so we need to reset the exception.
        if result.is_err() {
            exception.discard_handle_close();
        }

        // ZX_ERR_PEER_CLOSED just indicates that there's no longer an endpoint
        // to receive exceptions, simplify things for callers by collapsing this
        // into the ZX_ERR_NEXT case since it means the same thing.
        if result == Err(Status::PEER_CLOSED) {
            guard.as_mut().fields_mut().channel_handle.reset();
            self.has_channel.store(false, Ordering::Release);
            return Err(Status::NEXT);
        }

        result
    }
}

#[pinned_drop]
impl PinnedDrop for Exceptionate {
    fn drop(self: Pin<&mut Self>) {
        // Shuts the underlying channel down if it's still connected to be sure the
        // userspace endpoint gets the PEER_CLOSED signal.
        //
        // In most cases the task manually shuts down the exceptionate when transitioning to a
        // dead state, so `channel_handle` is already empty here. Since `drop` has exclusive
        // `&mut Self` access, reset `channel_handle` directly without acquiring `self.lock`.
        self.project().channel_handle.get_inner_mut().reset();
    }
}

/// In-tree kernel unit tests for `Exceptionate`.
#[cfg(ktest)]
#[unittest::suite(name = "exception_rust")]
mod tests {
    use super::Exceptionate;
    use crate::arch_rs::ArchExceptionContext;
    use crate::object::job_dispatcher::JobDispatcher;
    use crate::object::process_dispatcher::ProcessDispatcher;
    use crate::object::thread_dispatcher::ThreadDispatcher;
    use crate::object::{
        ChannelDispatcher, ExceptionDispatcher, KernelHandle, get_root_job_dispatcher,
    };
    use pin_init::stack_pin_init;
    use unittest::{assert_ok, expect_eq, expect_false, expect_ok, expect_true, unwrap_ok};
    use zerocopy::FromZeros;
    use zx_status::Status;
    use zx_types::{
        ZX_EXCEPTION_CHANNEL_TYPE_THREAD, ZX_EXCEPTION_STATE_HANDLED, ZX_EXCEPTION_STATE_TRY_NEXT,
        ZX_EXCP_GENERAL, ZX_KOID_INVALID, ZX_RIGHT_READ, ZX_RIGHT_WRITE, zx_exception_info_t,
        zx_exception_report_t,
    };

    /// Verifies channel binding, peer-close replacement, drop cleanup, and `shutdown()`.
    #[test]
    fn channel_lifecycle() {
        let (ch0, ch1, _rights) = unwrap_ok!(ChannelDispatcher::create(), "channel 0 created");
        {
            stack_pin_init!(let exceptionate = Exceptionate::new(ZX_EXCEPTION_CHANNEL_TYPE_THREAD));
            expect_eq!(exceptionate.type_(), ZX_EXCEPTION_CHANNEL_TYPE_THREAD);
            expect_false!(exceptionate.has_channel());
            expect_false!(exceptionate.has_valid_channel());
            expect_true!(
                exceptionate.set_channel(KernelHandle::empty(), 0, 0) == Err(Status::INVALID_ARGS)
            );

            assert_ok!(exceptionate.set_channel(ch0, 0, 0), "initial set_channel succeeds");
            expect_true!(exceptionate.has_channel());
            expect_true!(exceptionate.has_valid_channel());
            expect_false!(ch1.dispatcher().peer_has_closed());
        }
        // Dropping `Exceptionate` without `shutdown()` still closes the peer endpoint.
        expect_true!(ch1.dispatcher().peer_has_closed());

        let (ch0, mut ch1, _rights) = unwrap_ok!(ChannelDispatcher::create(), "channel 0 created");
        let (ch2, _ch3, _rights) = unwrap_ok!(ChannelDispatcher::create(), "channel 1 created");
        let (ch4, ch5, _rights) = unwrap_ok!(ChannelDispatcher::create(), "channel 2 created");
        let (ch6, _ch7, _rights) = unwrap_ok!(ChannelDispatcher::create(), "channel 3 created");

        stack_pin_init!(let exceptionate = Exceptionate::new(ZX_EXCEPTION_CHANNEL_TYPE_THREAD));
        assert_ok!(exceptionate.set_channel(ch0, 0, 0), "initial set_channel succeeds");
        expect_true!(exceptionate.set_channel(ch2, 0, 0) == Err(Status::ALREADY_BOUND));

        // Closing the peer endpoint makes the channel invalid and allows replacing it.
        ch1.reset();
        expect_true!(exceptionate.has_channel());
        expect_false!(exceptionate.has_valid_channel());
        expect_ok!(exceptionate.set_channel(ch4, 0, 0));
        expect_true!(exceptionate.has_channel());
        expect_true!(exceptionate.has_valid_channel());
        expect_false!(ch5.dispatcher().peer_has_closed());

        // `shutdown()` closes the peer endpoint and rejects subsequent channels.
        exceptionate.shutdown();
        expect_true!(ch5.dispatcher().peer_has_closed());
        expect_false!(exceptionate.has_channel());
        expect_false!(exceptionate.has_valid_channel());
        expect_true!(exceptionate.set_channel(ch6, 0, 0) == Err(Status::BAD_STATE));
    }

    /// Verifies `send_exception` across unbound, peer-closed, and active channels.
    #[test]
    fn send_exception_lifecycle() {
        let (job, _job_rights) =
            JobDispatcher::create(0, get_root_job_dispatcher().clone()).expect("job created");
        let (process, _process_rights, _vmar, _vmar_rights) =
            ProcessDispatcher::create(job.dispatcher().clone(), b"exc-ut-p", 0)
                .expect("process created");
        let (thread, _thread_rights) =
            ThreadDispatcher::create(process.dispatcher().clone(), 0, b"exc-ut-t")
                .expect("thread created");

        let mut report = zx_exception_report_t::new_zeroed();
        report.header.size = core::mem::size_of::<zx_exception_report_t>() as u32;
        report.header.type_ = ZX_EXCP_GENERAL;
        let dummy_arch_context: u64 = 0;

        // SAFETY: `report` and `dummy_arch_context` outlive `_cleanup`.
        let exception = unsafe {
            ExceptionDispatcher::create(
                thread.dispatcher().clone(),
                ZX_EXCP_GENERAL,
                &raw const report,
                (&raw const dummy_arch_context).cast::<ArchExceptionContext>(),
            )
        }
        .expect("exception created");
        let _cleanup = zr::defer(|| exception.clear());

        stack_pin_init!(let exceptionate = Exceptionate::new(ZX_EXCEPTION_CHANNEL_TYPE_THREAD));

        // 1. Unbound channel returns `Err(Status::NEXT)`.
        expect_true!(exceptionate.send_exception(&exception) == Err(Status::NEXT));
        expect_false!(exception.is_response_event_signaled());

        // 2. Peer-closed channel clears the channel, discards handle close, and returns `NEXT`.
        let (ch0, mut ch1, _rights) = unwrap_ok!(ChannelDispatcher::create(), "channel 0 created");
        assert_ok!(
            exceptionate.set_channel(ch0, ZX_RIGHT_READ, ZX_RIGHT_WRITE),
            "set_channel succeeds"
        );
        ch1.reset();
        exception.set_disposition(ZX_EXCEPTION_STATE_HANDLED);

        expect_true!(exceptionate.send_exception(&exception) == Err(Status::NEXT));
        expect_false!(exceptionate.has_channel());
        expect_false!(exception.is_response_event_signaled());
        expect_eq!(exception.get_disposition(), ZX_EXCEPTION_STATE_TRY_NEXT);
        {
            ksync::lock!(let guard = exceptionate.lock_lock());
            expect_false!(guard.fields().channel_handle.is_valid());
        }

        // 3. Active channel delivers the `zx_exception_info_t` message and exception handle.
        let (ch2, ch3, _rights) = unwrap_ok!(ChannelDispatcher::create(), "channel 1 created");
        assert_ok!(
            exceptionate.set_channel(ch2, ZX_RIGHT_READ, ZX_RIGHT_WRITE),
            "set_channel succeeds"
        );

        assert_ok!(exceptionate.send_exception(&exception), "send_exception succeeds");
        expect_false!(exception.is_response_event_signaled());

        let thread_handle = unwrap_ok!(exception.make_thread_handle(), "thread handle created");
        expect_eq!(thread_handle.rights(), ZX_RIGHT_READ);
        let process_handle = unwrap_ok!(exception.make_process_handle(), "process handle created");
        expect_eq!(process_handle.rights(), ZX_RIGHT_WRITE);

        let mut msg_size = core::mem::size_of::<zx_exception_info_t>() as u32;
        let mut msg_handles = 1u32;
        let msg = unwrap_ok!(
            ch3.dispatcher().read(ZX_KOID_INVALID, &mut msg_size, &mut msg_handles, false),
            "read exception message"
        );
        expect_eq!(msg.data_size(), core::mem::size_of::<zx_exception_info_t>());
        expect_eq!(msg.num_handles(), 1usize);
        expect_false!(exception.is_response_event_signaled());

        // Dropping the message closes the attached exception handle, firing `on_zero_handles()`.
        drop(msg);
        expect_true!(exception.is_response_event_signaled());
        expect_true!(exception.wait_for_handle_close() == Err(Status::NEXT));
    }
}
