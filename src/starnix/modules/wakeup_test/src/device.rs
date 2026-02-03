// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::input::{create_media_buttons_proxy, wakeup_send_power_button};
use crate::ioctl::{CommandCode, WakeupMethod, WakeupTestType, WakeupTimerInfo};
use crate::tracing;
use anyhow::{Result, anyhow};
use fuchsia_component::client::connect_to_protocol;
use starnix_core::device::DeviceOps;
use starnix_core::mm::MemoryAccessorExt;
use starnix_core::perf::TraceEventQueue;
use starnix_core::task::{CurrentTask, Kernel};
use starnix_core::vfs::{CloseFreeSafe, FileObject, FileOps, NamespaceNode, default_ioctl};
use starnix_core::{fileops_impl_dataless, fileops_impl_noop_sync, fileops_impl_seekless};
use starnix_logging::{log_error, log_info, log_warn};
use starnix_sync::{FileOpsCore, Locked, Unlocked};
use starnix_syscalls::{SUCCESS, SyscallResult};
use starnix_uapi::errors::Errno;
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::user_address::UserRef;
use starnix_uapi::{device_type, error};
use std::sync::{Arc, Weak};
use {fidl_fuchsia_time_alarms as fta, fidl_fuchsia_ui_test_input as futinput, zx};

#[derive(Clone)]
pub struct WakeupTestDevice {
    commands: Commands,
}

impl CloseFreeSafe for WakeupTestDevice {}

impl WakeupTestDevice {
    pub fn new(current_task: &CurrentTask) -> Self {
        Self {
            commands: Commands {
                tid: current_task.get_tid(),
                kernel: Arc::downgrade(current_task.kernel()),
            },
        }
    }
}

impl DeviceOps for WakeupTestDevice {
    fn open(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        current_task: &CurrentTask,
        _id: device_type::DeviceType,
        _node: &NamespaceNode,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        Ok(Box::new(WakeupTestDevice::new(current_task)))
    }
}

#[derive(Clone)]
struct Commands {
    kernel: Weak<Kernel>,
    tid: i32,
}

impl Commands {
    fn set_alarm(&self, alarm_id: String, time_ns: i64, timer_info: WakeupTimerInfo) -> Result<()> {
        log_info!(
            "WakeupTestDevice::set_alarm creating async task for alarm_id: {alarm_id}, time_ns: {time_ns}"
        );
        let kernel = self.kernel.upgrade().expect("kernel should exist");

        kernel.kthreads.spawn_future(
            move || async move {
                let alarm_proxy = match connect_to_protocol::<fta::WakeAlarmsMarker>() {
                    Ok(proxy) => proxy,
                    Err(e) => {
                        log_error!("Failed to connect to WakeAlarms protocol: {:?}", e);
                        return;
                    }
                };
                let media_button_proxy = match create_media_buttons_proxy().await {
                    Ok(proxy) => proxy,
                    Err(e) => {
                        log_error!("Failed to create media buttons proxy: {:?}", e);
                        return;
                    }
                };
                let timer_handler =
                    WakeupTimerHandler::new(alarm_id, time_ns, timer_info, media_button_proxy);

                let deadline = zx::BootInstant::after(zx::Duration::from_nanos(time_ns));
                let (_lease, peer) = zx::EventPair::create();
                match alarm_proxy
                    .set_and_wait(deadline, fta::SetMode::KeepAlive(peer), &timer_handler.alarm_id)
                    .await
                {
                    Ok(Ok(_)) => {
                        log_info!("Wakeup alarm set and triggered successfully!");
                        timer_handler.on_timer().await
                    }
                    Ok(Err(e)) => log_error!("Failed to set wakeup alarm: {:?}", e),
                    Err(e) => log_error!("FIDL error: {:?}", e),
                }
            },
            "wakeup_test",
        );
        Ok(())
    }

    fn run_wakeup_set_timers(&self, timer_info: WakeupTimerInfo) -> Result<()> {
        let method = WakeupMethod::from(timer_info.method);

        // TODO(https://fxbug.dev/458389823): Use other input events to wakeup the system.
        match method {
            WakeupMethod::PowerButton => (),
            _ => {
                return Err(anyhow!(
                    "Only PowerButton wakeup method is currently supported: b/458389823"
                ));
            }
        };

        let test_type = WakeupTestType::from(timer_info.test_type);
        log_info!(
            "WakeupTestDevice::WakeupSetTimers test_type: {test_type:?} Setting {} timers for {:?}, interval {} ns, starting {} ns",
            timer_info.num_events,
            method,
            timer_info.interval,
            timer_info.offset
        );
        tracing::trace_wakeup_test_type(self.get_trace_event_queue(), self.tid, test_type);
        for index in 0..timer_info.num_events {
            let time = (timer_info.interval * (index as i64)) + timer_info.offset;
            log_info!("WakeupTestDevice::set_timer i: {index} for {time}");
            tracing::trace_wakeup_set_timer(self.get_trace_event_queue(), self.tid, index, time);
            self.set_alarm(format!("starnix_wakeup_test_alarm_{index}"), time, timer_info)?;
        }
        Ok(())
    }

    /// Gets the trace event queue if available to emit trace events.
    fn get_trace_event_queue(&self) -> Option<Arc<TraceEventQueue>> {
        if let Some(k) = self.kernel.upgrade() {
            let event_queue = TraceEventQueue::from(&k);
            if event_queue.is_enabled() {
                return Some(event_queue);
            } else {
                log_warn!("Trace event queue is not enabled");
            }
        }
        None
    }
}

struct WakeupTimerHandler {
    alarm_id: String,
    timer_ns: i64,
    timer_info: WakeupTimerInfo,
    media_button_proxy: futinput::MediaButtonsDeviceProxy,
}

impl WakeupTimerHandler {
    fn new(
        alarm_id: String,
        timer_ns: i64,
        timer_info: WakeupTimerInfo,
        media_button_proxy: futinput::MediaButtonsDeviceProxy,
    ) -> Self {
        Self { alarm_id, timer_ns, timer_info, media_button_proxy }
    }

    async fn on_timer(&self) {
        log_info!("WakeupTestDevice::on_timer called: {info:?}", info = self.timer_info);
        let method = WakeupMethod::from(self.timer_info.method);

        // Trace how long we spend processing the timer. It should be inconsequential to the resume
        // latency.
        fuchsia_trace::duration!(
            tracing::POWER_CATEGORY,
            "WakeupTest:OnTimer",
            "alarm_id" => self.alarm_id.as_str(),
            "timer_ns" => self.timer_ns,
            "method" => <WakeupMethod as Into<&'static str>>::into(method)
        );

        match method {
            WakeupMethod::PowerButton => wakeup_send_power_button(&self.media_button_proxy).await,
            WakeupMethod::WakeupByTouch
            | WakeupMethod::WakeupBySwipeUp
            | WakeupMethod::WakeupBySwipeDown
            | WakeupMethod::WakeupBySwipeLeft
            | WakeupMethod::WakeupBySwipeRight => {
                log_error!("unimplemented wakeup method: {:?}", method)
            }
        }
    }
}

impl FileOps for WakeupTestDevice {
    fileops_impl_seekless!();
    fileops_impl_dataless!();
    fileops_impl_noop_sync!();

    fn ioctl(
        &self,
        locked: &mut Locked<Unlocked>,
        file: &FileObject,
        current_task: &CurrentTask,
        request: u32,
        arg: starnix_syscalls::SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        let cmd_num = CommandCode::from(request);
        log_info!("WakeupTestDevice::ioctl cmd_num {cmd_num:?}, arg: {arg:?}");

        match cmd_num {
            CommandCode::WakeupSetTimers => {
                let timer_ref = UserRef::<WakeupTimerInfo>::new(arg.into());
                let timer_info = current_task.read_object(timer_ref)?;
                log_info!("WakeupTestDevice::WakeupSetTimers {timer_info:?}");
                log_info!("WakeupTestDevice::WakeupSetTimers version 0x{:x}", timer_info.version);

                match self.commands.run_wakeup_set_timers(timer_info) {
                    Ok(_) => Ok(SUCCESS),
                    Err(e) => {
                        log_error!("WakeupTestDevice::WakeupSetTimers failed: {:?}", e);
                        return error!(EINVAL);
                    }
                }
            }
            CommandCode::WakeupTest => error!(ENOSYS),
            CommandCode::WakeupHowManyTimers => error!(ENOSYS),
            CommandCode::WakeupCancelTimers => error!(ENOSYS),
            _ => default_ioctl(file, locked, current_task, request, arg),
        }
    }
}
