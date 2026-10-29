// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The Archivist collects and stores diagnostic data from components.

#![warn(missing_docs)]

use anyhow::{Context, Error};
use archivist_config::Config;
use archivist_lib::archivist::Archivist;
use archivist_lib::component_lifecycle;
use archivist_lib::logs::repository::{ARCHIVIST_MONIKER, ARCHIVIST_TAG};
use archivist_lib::logs::serial::MAX_SERIAL_WRITE_SIZE;
use archivist_lib::logs::shared_buffer::create_ring_buffer;
use diagnostics_log_encoding::encode::{
    Encoder, EncoderOpts, LogEvent, MutableBuffer, WriteEventParams,
};
use fidl_fuchsia_logger::MAX_DATAGRAM_LEN_BYTES;
use fuchsia_async as fasync;
use fuchsia_async::SendExecutorBuilder;
use fuchsia_component::server::{MissingStartupHandle, ServiceFs};
use fuchsia_inspect::component;
use fuchsia_inspect::health::Reporter;
use fuchsia_runtime as rt;
use log::{LevelFilter, debug};
use moniker::Moniker;
use std::fmt;
use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};

const INSPECTOR_SIZE: usize = 2 * 1024 * 1024 /* 2MB */;

fn main() -> Result<(), Error> {
    let config = Config::take_from_startup_handle();
    if config.num_threads == 1 {
        fasync::LocalExecutor::default().run_singlethreaded(async_main(config))
    } else {
        // The executor will spin up an extra thread which is only for monitoring, so we ignore
        // that.
        let mut executor = SendExecutorBuilder::new().num_threads(config.num_threads).build();
        executor.run(async_main(config))
    }
    .context("async main")?;
    debug!("Exiting.");
    Ok(())
}

fn write_to_serial(msg: &str) {
    let msg = msg.as_bytes();
    for chunk in msg.chunks(MAX_SERIAL_WRITE_SIZE) {
        unsafe {
            zx::sys::zx_debug_write(chunk.as_ptr(), chunk.len());
        }
    }
}

async fn async_main(config: Config) -> Result<(), Error> {
    let ring_buffer = create_ring_buffer(config.logs_max_cached_original_bytes as usize);

    // We assume that this is the embedded version of Archivist if enable_klog is false.
    let is_embedded = !config.enable_klog;

    if let Err(e) = init_diagnostics(&ring_buffer, is_embedded).await {
        write_to_serial(&format!("archivist: init_diagnostics failed: {e:?}\n"));
        return Err(e).context("init_diagnostics");
    }

    component::inspector()
        .root()
        .record_child("config", |config_node| config.record_inspect(config_node));

    let archivist = Archivist::new(ring_buffer, config).await;
    debug!("Archivist initialized from configuration.");

    let startup_handle =
        fuchsia_runtime::take_startup_handle(fuchsia_runtime::HandleType::DirectoryRequest.into())
            .ok_or(MissingStartupHandle)?;

    let mut fs = ServiceFs::new();
    fs.serve_connection(fidl::endpoints::ServerEnd::new(zx::Channel::from(startup_handle)))?;
    let lifecycle_requests = component_lifecycle::take_lifecycle_request_stream();
    archivist.run(fs, is_embedded, lifecycle_requests).await?;

    Ok(())
}

struct Logger {
    iob: zx::Iob,
    dropped: AtomicU64,
}

// Used for redirecting stdout and stderr.
struct Redirector {
    severity: log::Level,
}

struct Iovecs<'a>(&'a [zx::sys::zx_iovec_t]);

impl fmt::Display for Iovecs<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut iter = self.0.iter();
        let last = iter.next_back();
        for vec in iter {
            f.write_str(&String::from_utf8_lossy(unsafe {
                std::slice::from_raw_parts(vec.buffer, vec.capacity)
            }))?;
        }
        // Strip the trailing \n if there is one.
        if let Some(last) = last {
            let amount =
                if last.capacity > 0 && unsafe { *last.buffer.add(last.capacity - 1) } == b'\n' {
                    last.capacity - 1
                } else {
                    last.capacity
                };
            f.write_str(&String::from_utf8_lossy(unsafe {
                std::slice::from_raw_parts(last.buffer, amount)
            }))?;
        }
        Ok(())
    }
}

impl fdio_zxio::FdOps for Redirector {
    fn writev(&self, iovecs: &[zx::sys::zx_iovec_t]) -> Result<usize, zx::Status> {
        log::log!(self.severity, "{}", Iovecs(iovecs));

        Ok(iovecs.iter().map(|i| i.capacity).sum())
    }

    fn isatty(&self) -> Result<bool, zx::Status> {
        Ok(true)
    }
}

thread_local! {
    static PID_AND_TID: (zx::Koid, zx::Koid) = (
        rt::process_self()
            .koid()
            .unwrap_or_else(|_| zx::Koid::from_raw(zx::sys::zx_koid_t::MAX)),
        rt::with_thread_self(|thread| {
            thread.koid().unwrap_or_else(|_| zx::Koid::from_raw(zx::sys::zx_koid_t::MAX))
        }),
    );
}

impl log::Log for Logger {
    fn enabled(&self, _metadata: &log::Metadata<'_>) -> bool {
        // NOTE: we handle minimum severity directly through the log max_level. So we call,
        // log::set_max_level, log::max_level where appropriate.
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        let mut buf = [0u8; MAX_DATAGRAM_LEN_BYTES as _];
        let mut encoder = Encoder::new(Cursor::new(&mut buf[..]), EncoderOpts::default());
        let (pid, tid) = PID_AND_TID.with(|r| *r);
        let tags: &[&str] = &[];
        let dropped = self.dropped.swap(0, Ordering::Relaxed);
        if encoder
            .write_event(WriteEventParams {
                event: LogEvent::new(record),
                tags,
                metatags: std::iter::empty(),
                pid,
                tid,
                dropped,
            })
            .is_err()
        {
            self.dropped.fetch_add(dropped + 1, Ordering::Relaxed);
            return;
        }
        let end = encoder.inner().cursor();
        if self.iob.write(Default::default(), 0, &encoder.inner().get_ref()[..end]).is_err() {
            self.dropped.fetch_add(dropped + 1, Ordering::Relaxed);
        }
    }

    fn flush(&self) {}
}

async fn init_diagnostics(
    ring_buffer: &ring_buffer::Reader,
    is_embedded: bool,
) -> Result<(), Error> {
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        write_to_serial(&format!("[archivist] FATAL: {info}\n"));
        previous_hook(info);
    }));

    // To set up logging for Archivist, we need to create the ring buffer early. Later, we will
    // create a container for it with this moniker.
    ARCHIVIST_MONIKER
        .set(if is_embedded {
            Moniker::parse_str("archivist").unwrap()
        } else {
            Moniker::parse_str("bootstrap/archivist").unwrap()
        })
        .unwrap();

    log::set_max_level(LevelFilter::Info);
    log::set_boxed_logger(Box::new(Logger {
        iob: ring_buffer.new_iob_writer(ARCHIVIST_TAG)?.0,
        dropped: AtomicU64::new(0),
    }))?;

    // NOTE: For the embedded archivist, we use the Debug level because there are integration tests
    // that will fail if they spuriously see log messages from Archivist. As an example, the
    // fuchsia-async library can write to stderr when tasks appear to be stalled, and that can
    // happen on slow builders (through no fault of Archivist). If that happens, then it can cause
    // some integration tests that are particular about what log messages are produced to fail.
    fdio_zxio::bind_to_fd_with_ops(
        Redirector { severity: if is_embedded { log::Level::Debug } else { log::Level::Info } },
        1,
    )?;
    fdio_zxio::bind_to_fd_with_ops(
        Redirector { severity: if is_embedded { log::Level::Debug } else { log::Level::Warn } },
        2,
    )?;

    component::init_inspector_with_size(INSPECTOR_SIZE);
    component::health().set_starting_up();

    fuchsia_trace_provider::trace_provider_create_with_fdio();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fdio_zxio::FdOps;
    use log::Log;

    #[fuchsia::test]
    fn iovecs_display_formatting() {
        // Empty iovecs
        assert_eq!(format!("{}", Iovecs(&[])), "");

        // Single iovec without trailing newline
        let s1 = b"hello";
        let v1 = zx::sys::zx_iovec_t { buffer: s1.as_ptr(), capacity: s1.len() };
        assert_eq!(format!("{}", Iovecs(&[v1])), "hello");

        // Single iovec with trailing newline (should be stripped)
        let s2 = b"world\n";
        let v2 = zx::sys::zx_iovec_t { buffer: s2.as_ptr(), capacity: s2.len() };
        assert_eq!(format!("{}", Iovecs(&[v2])), "world");

        // Single iovec with empty buffer
        let v_empty = zx::sys::zx_iovec_t { buffer: [].as_ptr(), capacity: 0 };
        assert_eq!(format!("{}", Iovecs(&[v_empty])), "");

        // Multiple iovecs, trailing newline only stripped from last
        let s3 = b"hello \n";
        let s4 = b"world\n";
        let v3 = zx::sys::zx_iovec_t { buffer: s3.as_ptr(), capacity: s3.len() };
        let v4 = zx::sys::zx_iovec_t { buffer: s4.as_ptr(), capacity: s4.len() };
        assert_eq!(format!("{}", Iovecs(&[v3, v4])), "hello \nworld");

        // Multiple iovecs where last is empty
        assert_eq!(format!("{}", Iovecs(&[v1, v_empty])), "hello");
    }

    #[fuchsia::test]
    fn redirector_writev_and_isatty() {
        let redirector = Redirector { severity: log::Level::Debug };
        assert!(redirector.isatty().unwrap());

        let s1 = b"test message\n";
        let v1 = zx::sys::zx_iovec_t { buffer: s1.as_ptr(), capacity: s1.len() };
        let written = redirector.writev(&[v1]).unwrap();
        assert_eq!(written, s1.len());
    }

    #[fuchsia::test]
    async fn logger_log_success_and_flush() {
        let ring_buf = create_ring_buffer(65536);
        let (iob, _peer) = ring_buf.new_iob_writer(1).unwrap();
        let logger = Logger { iob, dropped: AtomicU64::new(0) };

        assert!(logger.enabled(&log::Metadata::builder().level(log::Level::Info).build()));

        let record = log::Record::builder()
            .args(format_args!("test log entry"))
            .level(log::Level::Info)
            .target("test")
            .build();
        logger.log(&record);
        assert_eq!(logger.dropped.load(Ordering::Relaxed), 0);

        logger.flush();
        fasync::Timer::new(std::time::Duration::from_nanos(1)).await;
    }

    #[fuchsia::test]
    async fn logger_enabled_for_all_severities_to_allow_dynamic_level_filtering() {
        let ring_buf = create_ring_buffer(65536);
        let (iob, _peer) = ring_buf.new_iob_writer(1).unwrap();
        let logger = Logger { iob, dropped: AtomicU64::new(0) };

        // Logger relies on log::max_level() for filtering minimum severity (configured
        // dynamically via LogSettings/SetComponentInterest), so enabled() returns true
        // for all levels to prevent prematurely discarding messages when severity is adjusted.
        for level in [
            log::Level::Error,
            log::Level::Warn,
            log::Level::Info,
            log::Level::Debug,
            log::Level::Trace,
        ] {
            assert!(logger.enabled(&log::Metadata::builder().level(level).build()));
        }
    }

    #[fuchsia::test]
    fn logger_log_dropped_on_write_error() {
        // Create an invalid Iob handle to simulate write failures.
        let invalid_iob = zx::NullableHandle::invalid().into();
        let logger = Logger { iob: invalid_iob, dropped: AtomicU64::new(0) };

        let record = log::Record::builder()
            .args(format_args!("failed write entry 1"))
            .level(log::Level::Warn)
            .target("test")
            .build();
        logger.log(&record);
        // Dropped count should increment to 1
        assert_eq!(logger.dropped.load(Ordering::Relaxed), 1);

        let record2 = log::Record::builder()
            .args(format_args!("failed write entry 2"))
            .level(log::Level::Warn)
            .target("test")
            .build();
        logger.log(&record2);
        // Dropped count should increment to 2 (previous dropped + 1)
        assert_eq!(logger.dropped.load(Ordering::Relaxed), 2);
    }

    #[fuchsia::test]
    async fn logger_log_dropped_on_encode_error() {
        let ring_buf = create_ring_buffer(65536);
        let (iob, _peer) = ring_buf.new_iob_writer(1).unwrap();
        let logger = Logger { iob, dropped: AtomicU64::new(0) };

        // Construct a message larger than MAX_DATAGRAM_LEN_BYTES (32768 bytes).
        let huge_msg = "A".repeat(MAX_DATAGRAM_LEN_BYTES as usize + 100);
        let args = format_args!("{huge_msg}");
        let record =
            log::Record::builder().args(args).level(log::Level::Error).target("test").build();
        logger.log(&record);
        // Encoding should fail and increment dropped count.
        assert_eq!(logger.dropped.load(Ordering::Relaxed), 1);
        fasync::Timer::new(std::time::Duration::from_nanos(1)).await;
    }
}
