// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fdf_component::{Driver, DriverContext, DriverError, Node, driver_register};
use fidl_next::{Request, Responder, ServerEnd};
use fidl_next_fuchsia_hardware_platform_device as fpdev;
use fidl_next_fuchsia_hardware_rtc as frtc;
use fidl_next_fuchsia_hardware_rtc::device::{Get, Set2};
use fuchsia_async as fasync;
use fuchsia_component::server::ServiceFs;
use fuchsia_rtc::{is_rtc_valid, sanitize_rtc, seconds_since_epoch, seconds_to_rtc};
use fuchsia_sync::Mutex;
use futures::StreamExt;
use log::error;
use mmio::region::MmioRegion;
use mmio::vmo::VmoMemory;
use mmio::{register, register_block};
use pdev::PlatformDevice;
use std::sync::Arc;
use zx::Status;

register! {
    #[register(offset = 0x00, mode = RW)]
    pub struct TimeLow(u32);

    #[register(offset = 0x04, mode = RW)]
    pub struct TimeHigh(u32);
}

register_block! {
    pub struct GoldfishRtcRegsBlock<M> {
        time_low: TimeLow,
        time_high: TimeHigh,
    }
}

struct Registers {
    regs: GoldfishRtcRegsBlock<MmioRegion<VmoMemory>>,
}

impl Registers {
    fn new(regs: GoldfishRtcRegsBlock<MmioRegion<VmoMemory>>) -> Self {
        Self { regs }
    }

    fn get_rtc(&mut self) -> frtc::Time {
        // Reading time_low latches time_high.
        let low = u64::from(self.regs.time_low().read().value());
        let high = u64::from(self.regs.time_high().read().value());
        let nsec = (high << 32) | low;
        seconds_to_rtc(nsec / 1_000_000_000)
    }

    fn set_rtc(&mut self, rtc: &frtc::Time) -> Result<(), Status> {
        if !is_rtc_valid(rtc) {
            return Err(Status::OUT_OF_RANGE);
        }
        let nsec = seconds_since_epoch(rtc) * 1_000_000_000;
        // Write the high word first so the timestamp is complete once the low word is written.
        self.regs.time_high_mut().write(TimeHigh((nsec >> 32) as u32));
        self.regs.time_low_mut().write(TimeLow(nsec as u32));
        Ok(())
    }
}

struct DeviceServer {
    state: Arc<Mutex<Registers>>,
}

impl frtc::DeviceServerHandler for DeviceServer {
    async fn get(&mut self, responder: Responder<Get>) {
        let rtc = self.state.lock().get_rtc();
        let _ = responder.respond(&rtc).await;
    }

    async fn set2(&mut self, request: Request<Set2>, responder: Responder<Set2>) {
        let rtc = request.payload().rtc;
        let result = self.state.lock().set_rtc(&rtc);
        match result {
            Ok(()) => {
                let _ = responder.respond(()).await;
            }
            Err(status) => {
                let _ = responder.respond_err(status).await;
            }
        }
    }
}

struct RtcService {
    state: Arc<Mutex<Registers>>,
    scope: fasync::ScopeHandle,
}

impl frtc::ServiceHandler for RtcService {
    fn device(&self, server_end: ServerEnd<frtc::Device>) {
        server_end.spawn_on(DeviceServer { state: self.state.clone() }, &self.scope);
    }
}

struct GoldfishRtcDriver {
    _node: Node,
    _scope: fasync::Scope,
}

driver_register!(GoldfishRtcDriver);

impl Driver for GoldfishRtcDriver {
    const NAME: &str = "goldfish-rtc";

    async fn start(mut context: DriverContext) -> Result<Self, DriverError> {
        let node = context.take_node().inspect_err(|&e| error!("Failed to take node: {e:?}"))?;

        let service = context
            .incoming
            .service::<fdf_component::ServiceInstance<fpdev::Service>>()
            .connect_next()?;
        let (client_end, server_end) = fidl_next::fuchsia::create_channel();
        service.device(server_end).map_err(|e| {
            error!("Failed to open fpdev::Device: {e:?}");
            DriverError::from(Status::INTERNAL)
        })?;
        let pdev = client_end.spawn();

        let mmio =
            pdev.map_mmio_by_id(0).await.inspect_err(|e| error!("Failed to map MMIO: {e:?}"))?;
        let mut registers = Registers::new(GoldfishRtcRegsBlock::new(mmio));

        let rtc = registers.get_rtc();
        let sanitized = sanitize_rtc(rtc);
        if let Err(status) = registers.set_rtc(&sanitized) {
            error!("Failed to set sanitized RTC: {status:?}");
        }

        let scope = fasync::Scope::new_with_name("goldfish-rtc");
        let state = Arc::new(Mutex::new(registers));

        let mut outgoing = ServiceFs::new();
        outgoing.dir("svc").add_fidl_next_service_instance::<frtc::Service, _>(
            "default",
            RtcService { state, scope: scope.to_handle() },
        );

        context
            .serve_outgoing(&mut outgoing)
            .inspect_err(|&e| error!("Failed to serve outgoing: {e:?}"))?;
        scope.spawn(outgoing.collect());

        Ok(Self { _node: node, _scope: scope })
    }

    async fn stop(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use mmio::Register;
    use mmio::vmo::VmoMapping;
    use zx::Vmo;

    const TEST_VMO_SIZE: usize = 0x1000;

    fn create_test_registers() -> (Vmo, Registers) {
        let vmo = Vmo::create(TEST_VMO_SIZE as u64).unwrap();
        let mmio = VmoMapping::map_with_cache_policy(
            0,
            TEST_VMO_SIZE,
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            zx::CachePolicy::Cached,
        )
        .unwrap();
        (vmo, Registers::new(GoldfishRtcRegsBlock::new(mmio)))
    }

    #[fuchsia::test]
    fn test_mmio_get_set_rtc() {
        let (vmo, mut registers) = create_test_registers();

        let time =
            frtc::Time { seconds: 12, minutes: 34, hours: 16, day: 14, month: 10, year: 2026 };
        registers.set_rtc(&time).expect("set_rtc failed");

        let nsec = seconds_since_epoch(&time) * 1_000_000_000;
        let mut low_bytes = [0u8; 4];
        let mut high_bytes = [0u8; 4];
        vmo.read(&mut low_bytes, TimeLow::OFFSET as u64).unwrap();
        vmo.read(&mut high_bytes, TimeHigh::OFFSET as u64).unwrap();
        let low = u32::from_le_bytes(low_bytes);
        let high = u32::from_le_bytes(high_bytes);
        assert_eq!(low, (nsec & 0xffff_ffff) as u32);
        assert_eq!(high, (nsec >> 32) as u32);

        let read_back = registers.get_rtc();
        assert_eq!(read_back.year, time.year);
        assert_eq!(read_back.month, time.month);
        assert_eq!(read_back.day, time.day);
        assert_eq!(read_back.hours, time.hours);
        assert_eq!(read_back.minutes, time.minutes);
        assert_eq!(read_back.seconds, time.seconds);
    }

    #[fuchsia::test]
    fn test_set_rtc_invalid() {
        let (vmo, mut registers) = create_test_registers();

        let invalid_time =
            frtc::Time { seconds: 60, minutes: 0, hours: 0, day: 1, month: 1, year: 2026 };
        assert_eq!(registers.set_rtc(&invalid_time), Err(Status::OUT_OF_RANGE));

        let mut low_bytes = [0xffu8; 4];
        let mut high_bytes = [0xffu8; 4];
        vmo.read(&mut low_bytes, TimeLow::OFFSET as u64).unwrap();
        vmo.read(&mut high_bytes, TimeHigh::OFFSET as u64).unwrap();
        assert_eq!(u32::from_le_bytes(low_bytes), 0);
        assert_eq!(u32::from_le_bytes(high_bytes), 0);
    }
}
