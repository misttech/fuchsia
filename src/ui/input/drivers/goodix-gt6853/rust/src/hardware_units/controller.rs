// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! High-level touch controller state management for the Goodix GT6853 Touch IC.

use crate::data_types::hardware_limits::MAX_CONTACTS;
use crate::data_types::reports::{
    EventStatus, InitialTouchReport, RemainingTouchContacts, TouchContact,
};
use crate::hardware_units::i2c::MessageInterfaceUnitI2c;
use fuchsia_async as fasync;
use fuchsia_async::DurationExt;

/// Touch controller managing the GT6853 touch IC state and coordinate reporting.
pub struct Controller {
    i2c: MessageInterfaceUnitI2c,
}

impl Controller {
    /// Polling interval for reading touch coordinates.
    pub const POLL_INTERVAL: zx::MonotonicDuration = zx::MonotonicDuration::from_millis(10);

    /// Creates a new [`Controller`] wrapping the given [`i2c`] interface unit.
    pub fn new(i2c: MessageInterfaceUnitI2c) -> Self {
        Self { i2c }
    }

    async fn read_remaining_contacts(
        &self,
        remaining_count: usize,
    ) -> Result<Vec<TouchContact>, zx::Status> {
        // TODO(https://fxbug.dev/558557195): Investigate alternative designs for reading variable
        // remaining contacts.
        macro_rules! match_remaining {
            ($($n:literal),*) => {
                match remaining_count {
                    $(
                        $n => self
                            .i2c
                            .read_reg::<RemainingTouchContacts<$n>>()
                            .await
                            .map(|r| r.contacts.to_vec()),
                    )*
                    _ => {
                        log::warn!("Invalid remaining contact count: {remaining_count}");
                        Err(zx::Status::BAD_STATE)
                    }
                }
            };
        }

        match_remaining!(1, 2, 3, 4, 5, 6, 7, 8, 9)
    }

    /// Reads and processes coordinate data from the hardware touch buffer.
    ///
    /// If new coordinates are ready, parses contact count and contact coordinates,
    /// logs each active contact, and clears the coordinate buffer to unlock it for future touches.
    ///
    /// Returns `Ok(true)` if a touch event was processed, `Ok(false)` if the
    /// hardware had no new event ready, or `Err(status)` on hardware transfer failure.
    pub async fn process_touch_report(&mut self) -> Result<bool, zx::Status> {
        let report = self.i2c.read_reg::<InitialTouchReport>().await?;

        if !report.status.coordinates_ready() {
            return Ok(false);
        }

        let count = report.contact_count.count();
        if count > MAX_CONTACTS {
            log::warn!("Touch event with too many contacts: {count}");
            // Release the buffer lock to allow hardware recovery.
            self.i2c.write_reg(&EventStatus::CLEAR).await?;
            return Err(zx::Status::BAD_STATE);
        }

        if count > 0 {
            log::info!("Touch event: count={count}");
            // TODO(https://fxbug.dev/558521383): Validate the coordinate checksum.
            let remaining =
                if count > 1 { self.read_remaining_contacts(count - 1).await? } else { Vec::new() };

            let contacts_iter = core::iter::once(&report.first_contact).chain(&remaining);
            for (index, contact) in contacts_iter.enumerate() {
                log::info!(
                    "  contact[{index}]: id={}, x={}, y={}, pressure={}",
                    contact.id(),
                    contact.x(),
                    contact.y(),
                    contact.pressure()
                );
            }
        } else {
            log::info!("Touch release (all contacts lifted)");
        }

        // Acknowledge the touch report and release the coordinate buffer lock.
        self.i2c.write_reg(&EventStatus::CLEAR).await?;

        Ok(true)
    }

    /// Runs the polling loop asynchronously.
    ///
    /// Periodically calls [`Self::process_touch_report`] every [`Self::POLL_INTERVAL`].
    /// Transient hardware errors are logged as warnings and retried on subsequent ticks
    /// to maintain task resilience.
    pub async fn run(mut self) {
        loop {
            if let Err(error) = self.process_touch_report().await {
                log::warn!("Hardware error during touch poll: {error:?}");
            }
            fasync::Timer::new(Self::POLL_INTERVAL.after_now()).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data_types::traits::AddressableRegister;
    use crate::testing::mock_i2c::MockI2cDevice;
    use fidl_next_fuchsia_hardware_i2c as fidl_i2c;

    struct TestFixture {
        mock: MockI2cDevice,
        controller: Controller,
        // The background server task must be kept alive for the lifetime of the test fixture.
        #[expect(dead_code)]
        i2c_server_task: fasync::Task<()>,
    }

    impl TestFixture {
        fn new() -> Self {
            let mock = MockI2cDevice::new();
            let (i2c_client_end, i2c_server_end) =
                fidl_next::fuchsia::create_channel::<fidl_i2c::Device>();
            let client = i2c_client_end.spawn();
            let server_task = i2c_server_end.spawn(mock.clone());
            let i2c_server_task = fasync::Task::spawn(async move {
                let _ = server_task.await;
            });
            let unit = MessageInterfaceUnitI2c::new(client);
            let controller = Controller::new(unit);
            Self { mock, controller, i2c_server_task }
        }
    }

    #[fuchsia::test]
    async fn test_process_touch_report_idle() {
        let mut fixture = TestFixture::new();
        let idle_report = vec![0x00; 10];
        fixture.mock.expect_read(InitialTouchReport::ADDRESS, 10, Ok(idle_report));

        let has_event =
            fixture.controller.process_touch_report().await.expect("process_touch_report failed");
        assert!(!has_event);
        fixture.mock.check_all_expectations_replayed();
    }

    #[fuchsia::test]
    async fn test_process_touch_report_single_touch() {
        let mut fixture = TestFixture::new();
        // EventStatus: coordinates_ready = true (0x80), ContactCount = 1 (0x01)
        let mut report_bytes = vec![0x80, 0x01];
        // contact0: id=0, x=530 (0x0212), y=940 (0x03AC), pressure=40 (0x28)
        let contact0 = [0x00, 0x12, 0x02, 0xAC, 0x03, 0x28, 0x00, 0x00];
        report_bytes.extend_from_slice(&contact0);

        fixture.mock.expect_read(InitialTouchReport::ADDRESS, 10, Ok(report_bytes));
        fixture.mock.expect_write(EventStatus::ADDRESS, &[0x00], Ok(()));

        let has_event =
            fixture.controller.process_touch_report().await.expect("process_touch_report failed");
        assert!(has_event);
        fixture.mock.check_all_expectations_replayed();
    }

    #[fuchsia::test]
    async fn test_process_touch_report_two_contacts() {
        let mut fixture = TestFixture::new();
        // EventStatus: coordinates_ready = true (0x80), ContactCount = 2 (0x02)
        let mut initial_report_bytes = vec![0x80, 0x02];
        // contact0: id=0, x=300 (0x012C), y=400 (0x0190), pressure=50 (0x32)
        let contact0 = [0x00, 0x2C, 0x01, 0x90, 0x01, 0x32, 0x00, 0x00];
        initial_report_bytes.extend_from_slice(&contact0);

        // Remaining 1 contact (contact1): id=1, x=500 (0x01F4), y=600 (0x0258), pressure=60 (0x3C)
        let contact1 = [0x01, 0xF4, 0x01, 0x58, 0x02, 0x3C, 0x00, 0x00];

        fixture.mock.expect_read(InitialTouchReport::ADDRESS, 10, Ok(initial_report_bytes));
        fixture.mock.expect_read(RemainingTouchContacts::<1>::ADDRESS, 8, Ok(contact1.to_vec()));
        fixture.mock.expect_write(EventStatus::ADDRESS, &[0x00], Ok(()));

        let has_event =
            fixture.controller.process_touch_report().await.expect("process_touch_report failed");
        assert!(has_event);
        fixture.mock.check_all_expectations_replayed();
    }

    #[fuchsia::test]
    async fn test_process_touch_report_three_contacts() {
        let mut fixture = TestFixture::new();
        // EventStatus: coordinates_ready = true (0x80), ContactCount = 3 (0x03)
        let mut initial_report_bytes = vec![0x80, 0x03];
        // contact0: id=0, x=100 (0x0064), y=200 (0x00C8), pressure=30 (0x1E)
        let contact0 = [0x00, 0x64, 0x00, 0xC8, 0x00, 0x1E, 0x00, 0x00];
        initial_report_bytes.extend_from_slice(&contact0);

        // Remaining 2 contacts (contact1 and contact2) read together in a single pass (16 bytes).
        // contact1: id=1, x=300 (0x012C), y=400 (0x0190), pressure=40 (0x28)
        let contact1 = [0x01, 0x2C, 0x01, 0x90, 0x01, 0x28, 0x00, 0x00];
        // contact2: id=2, x=500 (0x01F4), y=600 (0x0258), pressure=50 (0x32)
        let contact2 = [0x02, 0xF4, 0x01, 0x58, 0x02, 0x32, 0x00, 0x00];
        let mut remaining_bytes = Vec::new();
        remaining_bytes.extend_from_slice(&contact1);
        remaining_bytes.extend_from_slice(&contact2);

        fixture.mock.expect_read(InitialTouchReport::ADDRESS, 10, Ok(initial_report_bytes));
        fixture.mock.expect_read(RemainingTouchContacts::<2>::ADDRESS, 16, Ok(remaining_bytes));
        fixture.mock.expect_write(EventStatus::ADDRESS, &[0x00], Ok(()));

        let has_event =
            fixture.controller.process_touch_report().await.expect("process_touch_report failed");
        assert!(has_event);
        fixture.mock.check_all_expectations_replayed();
    }

    #[fuchsia::test]
    async fn test_process_touch_report_zero_contacts() {
        let mut fixture = TestFixture::new();

        // Hardware reports coordinates_ready = true (0x80), but contact_count = 0 (0x00).
        let report_bytes = vec![0x80, 0x00, 0, 0, 0, 0, 0, 0, 0, 0];
        fixture.mock.expect_read(InitialTouchReport::ADDRESS, 10, Ok(report_bytes));
        fixture.mock.expect_write(EventStatus::ADDRESS, &[0x00], Ok(()));

        let has_event =
            fixture.controller.process_touch_report().await.expect("process_touch_report failed");
        assert!(has_event);
        fixture.mock.check_all_expectations_replayed();
    }

    #[fuchsia::test]
    async fn test_process_touch_report_invalid_contact_count() {
        let mut fixture = TestFixture::new();

        // Hardware reports coordinates_ready = true (0x80), but contact_count = 15
        // (0x0F > MAX_CONTACTS).
        let mut report_bytes = vec![0x80, 0x0F];
        report_bytes.extend_from_slice(&[0u8; 8]);

        fixture.mock.expect_read(InitialTouchReport::ADDRESS, 10, Ok(report_bytes));
        fixture.mock.expect_write(EventStatus::ADDRESS, &[0x00], Ok(()));

        let result = fixture.controller.process_touch_report().await;
        assert_eq!(result.err(), Some(zx::Status::BAD_STATE));
        fixture.mock.check_all_expectations_replayed();
    }

    #[fuchsia::test]
    async fn test_process_touch_report_read_error_propagates() {
        let mut fixture = TestFixture::new();
        fixture.mock.expect_read(InitialTouchReport::ADDRESS, 10, Err(zx::Status::PEER_CLOSED));

        let result = fixture.controller.process_touch_report().await;
        assert_eq!(result.err(), Some(zx::Status::PEER_CLOSED));
        fixture.mock.check_all_expectations_replayed();
    }

    #[fuchsia::test]
    async fn test_process_touch_report_write_error_propagates() {
        let mut fixture = TestFixture::new();
        // EventStatus: coordinates_ready = true (0x80), ContactCount = 1 (0x01)
        let mut report_bytes = vec![0x80, 0x01];
        let contact0 = [0x00, 0x12, 0x02, 0xAC, 0x03, 0x28, 0x00, 0x00];
        report_bytes.extend_from_slice(&contact0);

        fixture.mock.expect_read(InitialTouchReport::ADDRESS, 10, Ok(report_bytes));
        fixture.mock.expect_write(EventStatus::ADDRESS, &[0x00], Err(zx::Status::IO_REFUSED));

        let result = fixture.controller.process_touch_report().await;
        assert_eq!(result.err(), Some(zx::Status::IO_REFUSED));
        fixture.mock.check_all_expectations_replayed();
    }
}
