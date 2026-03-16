// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Error};
use fidl_fuchsia_bluetooth_snoop::PacketFormat;
use fidl_fuchsia_feedback::{Attachment, CrashReport, CrashReporterProxy};
use fidl_fuchsia_hardware_bluetooth::VendorCrashParameters;
use fidl_fuchsia_mem as fmem;
use log::{error, info, warn};
use zx::{HandleBased, Vmo};

use crate::packet_logs::{
    PCAP_GLOBAL_HEADER_SIZE, PCAP_PACKET_HEADER_SIZE, write_pcap_header, write_pcap_packet_header,
};
use crate::snooper::SnoopPacket;

/// This is slightly more than the largest Broadcom controller core dump we've seen.
const MAX_SNOOP_VMO_SIZE: u64 = 4 * 1024 * 1024; // 4MB

/// HCI Event Code for Vendor Specific Events.
const VENDOR_EVENT_CODE: u8 = 0xFF;

const CORE_DUMP_FILE: &str = "bluetooth_core_dump.pcap";
const DEFAULT_PROGRAM_NAME: &str = "bt-snoop";
const DEFAULT_CRASH_SIGNATURE: &str = "bluetooth-controller-crash";

/// The debounce duration is a guess, but the events should be sent rapid-fire with less than a
/// second between each event.
pub(crate) const CRASH_REPORT_DEBOUNCE_DURATION: zx::MonotonicDuration =
    zx::MonotonicDuration::from_seconds(5);

const CRASH_REPORT_RATE_LIMIT_DURATION: zx::MonotonicDuration =
    zx::MonotonicDuration::from_hours(1);

pub(crate) struct CrashState {
    pub parameters: VendorCrashParameters,
    pub last_report_local_time: Option<fuchsia_async::MonotonicInstant>,
    pub collector: Option<CoreDumpCollector>,
    /// The target time for filing the crash report, used for debouncing.
    ///
    /// When a crash is detected, this is set to `now() + CRASH_REPORT_DEBOUNCE_DURATION`.
    /// Each subsequent packet received while collecting the core dump will push this time
    /// further into the future.
    ///
    /// Once the current time reaches `tentative_report_file_time` without being pushed back, the crash
    /// report is considered finished and is filed.
    pub tentative_report_file_time: Option<fuchsia_async::MonotonicInstant>,
}

impl CrashState {
    pub(crate) fn is_rate_limited(&self) -> bool {
        self.collector.is_none()
            && self.last_report_local_time.map_or(false, |t| {
                fuchsia_async::MonotonicInstant::now() - t < CRASH_REPORT_RATE_LIMIT_DURATION
            })
    }

    pub(crate) fn is_crash_event(&self, packet: &SnoopPacket) -> bool {
        packet.format == PacketFormat::Event
            && packet.payload.len() >= 3
            && packet.payload[0] == VENDOR_EVENT_CODE
            && self.parameters.vendor_subevent_code == Some(packet.payload[2])
    }

    fn maybe_start_new_crash(&mut self) -> bool {
        if self.collector.is_some() {
            return false;
        }
        let program_name = self
            .parameters
            .program_name
            .clone()
            .unwrap_or_else(|| DEFAULT_PROGRAM_NAME.to_string());
        let crash_signature = self
            .parameters
            .crash_signature
            .clone()
            .unwrap_or_else(|| DEFAULT_CRASH_SIGNATURE.to_string());
        match CoreDumpCollector::new(program_name, crash_signature) {
            Ok(c) => {
                self.collector = Some(c);
                self.last_report_local_time = Some(fuchsia_async::MonotonicInstant::now());
                true
            }
            Err(e) => {
                warn!("Failed to create CoreDumpCollector: {:?}", e);
                false
            }
        }
    }

    /// Returns true if the packet started a new crash dump collection.
    pub(crate) fn process_packet(&mut self, packet: &SnoopPacket) -> bool {
        if !self.is_crash_event(packet) {
            return false;
        }

        if self.is_rate_limited() {
            return false;
        }

        let started_new_crash = self.maybe_start_new_crash();

        if let Some(collector) = &mut self.collector {
            collector.on_packet(packet);
            self.tentative_report_file_time = Some(
                (fuchsia_async::MonotonicInstant::now() + CRASH_REPORT_DEBOUNCE_DURATION).into(),
            );
        }

        started_new_crash
    }
}

pub(crate) struct CoreDumpCollector {
    vmo: Vmo,
    /// The current offset into the VMO where the next packet should be written.
    offset: u64,
    program_name: String,
    crash_signature: String,
    logged_drop: bool,
}

impl CoreDumpCollector {
    pub fn new(program_name: String, crash_signature: String) -> Result<Self, Error> {
        let vmo = Vmo::create(MAX_SNOOP_VMO_SIZE).context("failed to create VMO for core dump")?;

        let mut header_buf = [0u8; PCAP_GLOBAL_HEADER_SIZE];
        write_pcap_header(&mut header_buf[..])?;
        vmo.write(&header_buf, 0).context("failed to write pcap header to VMO")?;

        Ok(Self {
            vmo,
            offset: header_buf.len() as u64,
            program_name,
            crash_signature,
            logged_drop: false,
        })
    }

    pub fn on_packet(&mut self, packet: &SnoopPacket) {
        let packet_len = PCAP_PACKET_HEADER_SIZE as u64 + packet.payload.len() as u64;
        let new_offset = self.offset + packet_len;

        if new_offset > MAX_SNOOP_VMO_SIZE {
            // Avoid log spam by only logging the first dropped packet.
            if !self.logged_drop {
                error!("Dropped packet because VMO limit exceeded");
                self.logged_drop = true;
            }
            return;
        }

        let mut header_buf = [0u8; PCAP_PACKET_HEADER_SIZE];
        if let Err(e) = write_pcap_packet_header(&mut header_buf[..], packet) {
            warn!("Failed to format pcap packet for core dump: {:?}", e);
            return;
        }

        if let Err(e) = self.vmo.write(&header_buf, self.offset) {
            warn!("Failed to write pcap packet header to core dump VMO: {:?}", e);
            return;
        }

        if let Err(e) =
            self.vmo.write(&packet.payload, self.offset + PCAP_PACKET_HEADER_SIZE as u64)
        {
            warn!("Failed to write pcap packet payload to core dump VMO: {:?}", e);
            return;
        }

        self.offset = new_offset;
    }

    pub async fn file_report(self, crash_reporter: &CrashReporterProxy) {
        let rights =
            zx::Rights::BASIC | zx::Rights::READ | zx::Rights::MAP | zx::Rights::GET_PROPERTY;
        let vmo = match self.vmo.duplicate_handle(rights) {
            Ok(v) => v,
            Err(e) => {
                warn!("Failed to duplicate VMO for crash report: {:?}", e);
                return;
            }
        };

        let attachment = Attachment {
            key: CORE_DUMP_FILE.to_string(),
            value: fmem::Buffer { vmo, size: self.offset },
        };

        let program_name = self.program_name.clone();
        let crash_signature = self.crash_signature.clone();
        let report = CrashReport {
            program_name: Some(self.program_name),
            crash_signature: Some(self.crash_signature),
            attachments: Some(vec![attachment]),
            is_fatal: Some(false),
            ..Default::default()
        };

        match crash_reporter.file_report(report).await {
            Ok(Ok(_)) => info!(
                "Crash report filed successfully for program_name: {}, crash_signature: {}",
                program_name, crash_signature
            ),
            Ok(Err(e)) => warn!("Server returned error filing crash report: {:?}", e),
            Err(e) => warn!("FIDL error filing crash report: {:?}", e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuchsia_async as fasync;

    #[fasync::run_singlethreaded(test)]
    async fn test_core_dump_collector_initialization() {
        let collector = CoreDumpCollector::new("test_program".to_string(), "test_sig".to_string())
            .expect("Failed to create collector");

        assert_eq!(collector.program_name, "test_program");
        assert_eq!(collector.crash_signature, "test_sig");
        // Ensure some data was written to VMO (pcap header)
        assert!(collector.offset > 0);
        let size = collector.vmo.get_size().unwrap();
        assert_eq!(size, MAX_SNOOP_VMO_SIZE);
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_core_dump_collector_on_packet() {
        let mut collector = CoreDumpCollector::new("test".to_string(), "sig".to_string()).unwrap();
        let packet = SnoopPacket::new(
            true,
            PacketFormat::Event,
            zx::MonotonicInstant::from_nanos(0),
            vec![0xFF, 0x01, 0x1B],
        );

        let initial_offset = collector.offset;
        collector.on_packet(&packet);
        assert!(collector.offset > initial_offset);
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_core_dump_collector_file_report() {
        use fidl_fuchsia_feedback::{CrashReporterMarker, CrashReporterRequest};
        use futures::StreamExt;

        let mut collector =
            CoreDumpCollector::new("test_prog".to_string(), "test_sig".to_string()).unwrap();
        let packet = SnoopPacket::new(
            true,
            PacketFormat::Event,
            zx::MonotonicInstant::from_nanos(0),
            vec![0xFF, 0x01, 0x1B],
        );
        let expected_offset =
            collector.offset + PCAP_PACKET_HEADER_SIZE as u64 + packet.payload.len() as u64;
        collector.on_packet(&packet);

        let (proxy, mut stream) = fidl::endpoints::create_proxy_and_stream::<CrashReporterMarker>();

        let server = async move {
            let req = stream.next().await.unwrap().unwrap();
            let CrashReporterRequest::FileReport { report, responder } = req;
            assert_eq!(report.program_name.as_deref(), Some("test_prog"));
            assert_eq!(report.crash_signature.as_deref(), Some("test_sig"));
            assert_eq!(report.is_fatal, Some(false));

            let attachments = report.attachments.unwrap();
            assert_eq!(attachments.len(), 1);
            assert_eq!(attachments[0].key, CORE_DUMP_FILE);
            assert_eq!(attachments[0].value.size, expected_offset);

            // Respond with success
            let _ = responder.send(Ok(&fidl_fuchsia_feedback::FileReportResults::default()));
        };

        futures::join!(server, collector.file_report(&proxy));
    }
}
