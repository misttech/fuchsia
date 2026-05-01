// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::commands::LibraryCommand;
use crate::ext_buffer::ExtBuffer;
use crate::fdomain::FDomainState;
use anyhow::Result;
use async_lock::{Mutex as AsyncMutex, MutexGuard};
use fdomain_client::Error as FDomainInternalError;
use fuchsia_async::{LocalExecutor, Task};
use signal_hook::consts::signal;
use std::ops::DerefMut;
use std::os::fd::{IntoRawFd, RawFd};
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use zx_types;

pub type Notifier = Arc<AsyncMutex<Option<LibNotifier>>>;

pub struct LibContext {
    buf: Mutex<ExtBuffer<u8>>,
    notifier: Notifier,
    cmd_sender: async_channel::Sender<LibraryCommand>,
    thread_ctx: Mutex<Option<std::thread::JoinHandle<()>>>,
    signal_handler_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    fdomain_state: AsyncMutex<FDomainState>,
    signal_handle: signal_hook::iterator::Handle,
}

impl LibContext {
    pub(crate) fn new(buf: ExtBuffer<u8>) -> Self {
        let notifier = Notifier::default();
        let (cmd_sender, receiver) = async_channel::unbounded::<LibraryCommand>();
        let (signal_sender, signal_receiver) = async_channel::bounded(1);
        let mut sig_watcher =
            signal_hook::iterator::Signals::new(&[signal::SIGINT, signal::SIGTERM]).unwrap();
        let signal_handle = sig_watcher.handle();
        let signal_handler_thread = std::thread::spawn(move || {
            // Signal behavior for both is the same: say we've been interrupted and then let the
            // above handle that.
            for s in sig_watcher.forever() {
                let debug_str = match s {
                    signal::SIGINT => "SIGINT",
                    signal::SIGTERM => "SIGTERM",
                    _ => unreachable!(),
                };
                log::info!("Received signal '{debug_str}'. Sending interrupt message to thread.");
                match signal_sender.try_send(s) {
                    Ok(()) => {
                        log::info!("signal interrupt sent successfully");
                    }
                    Err(e) => match e {
                        async_channel::TrySendError::Full(_) => {
                            log::info!("signal interrupt queue full.");
                        }
                        async_channel::TrySendError::Closed(_) => {
                            log::info!("signal interupt queue closed.");
                            break;
                        }
                    },
                }
            }
        });
        Self {
            cmd_sender,
            buf: Mutex::new(buf),
            notifier: notifier.clone(),
            thread_ctx: Mutex::new(Some(new_command_thread(
                receiver,
                signal_receiver,
                notifier.clone(),
            ))),
            fdomain_state: AsyncMutex::new(FDomainState::new(notifier)),
            signal_handler_thread: Mutex::new(Some(signal_handler_thread)),
            signal_handle,
        }
    }

    fn write_fidl_to_buffer(&self, fidl_err: impl fidl::Persistable) {
        let mut guard = self.buf.lock().unwrap();
        let buf = guard.deref_mut();
        let msg = fidl::persist(&fidl_err).expect("encoding fdomain fidl error");
        buf[0..8].clone_from_slice(&msg.len().to_ne_bytes());
        buf[8..(8 + msg.len())].clone_from_slice(&msg);
    }

    pub(crate) fn write_fdomain_err(&self, err: &FDomainInternalError) {
        use FDomainInternalError::*;
        match err {
            SocketWrite(s) => self.write_fidl_to_buffer(s.clone()),
            ChannelWrite(c) => self.write_fidl_to_buffer(c.clone()),
            FDomain(f) => self.write_fidl_to_buffer(f.clone()),
            ProtocolObjectTypeIncompatible
            | ProtocolRightsIncompatible
            | ConnectionMismatch
            | StreamingAborted
            | ProtocolSignalsIncompatible
            | ProtocolStreamEventIncompatible => {}
            t @ Transport(_) => self.write_err(t),
            p @ Protocol(_) => self.write_err(p),
        }
    }

    pub(crate) fn write_err<T: std::fmt::Display>(&self, err: T) {
        // LINT.IfChange(no_fdomain_client)
        let error = format!("FFX Library Error: {err}");
        // LINT.ThenChange(//tools/testing/tefmocheck/string_in_log_check.go:no_fdomain_client)
        let mut guard = self.buf.lock().unwrap();
        let buf = guard.deref_mut();
        buf[0..8].clone_from_slice(&error.len().to_ne_bytes());
        buf[8..(8 + error.len())].clone_from_slice(error.as_bytes());
        buf[8 + error.len()] = 0.into();
    }

    pub(crate) fn run(&self, cmd: LibraryCommand) {
        // Should not fail as this is an unbounded channel. In the future, when
        // updating to more recent versions of the async_channel library, this
        // can be handled using send_blocking instead.
        self.cmd_sender.try_send(cmd).expect("Sending to command channel");
    }

    pub(crate) async fn notifier_descriptor(&self) -> Result<RawFd> {
        let mut notifier = self.notifier.lock().await;
        if !notifier.is_some() {
            *notifier = Some(LibNotifier::new()?);
        }
        Ok(notifier.as_ref().unwrap().receiver())
    }

    pub(crate) fn shutdown_cmd_thread(&self) {
        self.run(LibraryCommand::ShutdownLib);
        let thread =
            self.thread_ctx.lock().unwrap().take().expect("thread context must have been set");
        assert_ne!(
            std::thread::current().id(),
            thread.thread().id(),
            "thread is being dropped from inside itself"
        );
        thread.join().expect("joining thread");

        // Signal the signal handler thread to exit.
        self.signal_handle.close();

        let signal_thread = self
            .signal_handler_thread
            .lock()
            .unwrap()
            .take()
            .expect("signal handler thread must have been set");
        assert_ne!(
            std::thread::current().id(),
            signal_thread.thread().id(),
            "thread is being dropped from inside itself"
        );
        signal_thread.join().expect("joining signal thread");
    }

    pub(crate) async fn fdomain_state<'a>(&'a self) -> MutexGuard<'a, FDomainState> {
        self.fdomain_state.lock().await
    }
}

fn new_command_thread(
    receiver: async_channel::Receiver<LibraryCommand>,
    sigint_receiver: async_channel::Receiver<i32>,
    notifier: Notifier,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(|| {
        let mut executor = LocalExecutor::default();
        executor.run_singlethreaded(async move {
            while let Ok(cmd) = receiver.recv().await {
                // If there was a signal from the previous iteration, then clear it out
                // so it doesn't cause an immediate error. This is mostly to prevent racing with
                // the REPL, as a user could potentially pre-load some signals before hitting the
                // command itself. In the event that SIGINT is received by a real program this
                // section of code won't matter anyway.
                if let Ok(_) = sigint_receiver.try_recv() {
                    log::info!("received signal before command, (this will have been received by the caller). Ignoring");
                }
                if let LibraryCommand::ShutdownLib = cmd {
                    // Dropping the notifier will cause spawned tasks to be dropped.
                    *notifier.lock().await = None;
                    break;
                }
                let cmd_fut = cmd.run();
                let _ = futures_lite::FutureExt::or(cmd_fut, async {
                    sigint_receiver.recv().await.unwrap();
                    log::info!("command thread received signal.");
                })
                .await;
            }
        });
    })
}

pub(crate) struct LibNotifier {
    _pipe_reader_task: Task<()>,
    handle_notification_sender: async_channel::Sender<zx_types::zx_handle_t>,
    stream_fd: RawFd,
}

impl LibNotifier {
    // This function isn't actually async, but it should be called inside an
    // executor to ensure spawned tasks are scheduled correctly.
    fn new() -> Result<Self> {
        let (stream_rx, mut stream_tx) = UnixStream::pair()?;
        let (tx, rx) = async_channel::unbounded::<zx_types::zx_handle_t>();
        let pipe_reader_task = fuchsia_async::Task::local(async move {
            while let Ok(raw_handle) = rx.recv().await {
                match stream_tx.write_u32_le(raw_handle).await {
                    Ok(_) => {}
                    Err(e) => {
                        log::info!("Exiting pipe reader task. Error: {e:?}");
                        break;
                    }
                }
            }
        });
        Ok(Self {
            handle_notification_sender: tx,
            _pipe_reader_task: pipe_reader_task,
            stream_fd: stream_rx.into_std()?.into_raw_fd(),
        })
    }

    fn receiver(&self) -> RawFd {
        self.stream_fd
    }

    pub fn sender(&self) -> async_channel::Sender<zx_types::zx_handle_t> {
        self.handle_notification_sender.clone()
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::sync::Mutex as SyncMutex;

    static SCRATCH_LOCK: Mutex<()> = SyncMutex::new(());
    static mut SCRATCH: [u8; 1024] = [0; 1024];
    fn testing_lib_context() -> LibContext {
        let raw = std::ptr::addr_of_mut!(SCRATCH) as *mut u8;
        // SAFETY: This is unsafe because multiple threads can read it, so is protected by way of a
        // mutex.
        let buf = unsafe { ExtBuffer::new(raw, 1024) };
        LibContext::new(buf)
    }

    fn decode_string_error<'a>(_guard: &std::sync::MutexGuard<'a, ()>) -> &'a str {
        // SAFETY: While it can't be proven this is the right lock, we should at least
        // be holding the lock here when testing usage of the shared scratch buffer.
        unsafe {
            let msg_len = usize::from_ne_bytes(SCRATCH[0..8].try_into().unwrap());
            std::str::from_utf8(&SCRATCH[8..(8 + msg_len)]).unwrap()
        }
    }

    // Tests for a bug in which `Transport(None)` would simply print `None`.
    #[test]
    fn test_transport_error_prints_readable_message() {
        let _lock = SCRATCH_LOCK.lock().unwrap();
        let ctx = testing_lib_context();
        let err = FDomainInternalError::Transport(None);
        ctx.write_fdomain_err(&err);
        let s = decode_string_error(&_lock);
        assert!(s.contains(&format!("{err}")), "GOT: '{s}'; WANT: '{err}'");
    }
}
