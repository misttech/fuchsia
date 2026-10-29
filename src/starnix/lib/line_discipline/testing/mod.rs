// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Trace replay harness for verifying [`LineDiscipline`] against recorded Linux `N_TTY` traces.

use crate::{InputBuffer, LineDiscipline, OutputBuffer, PendingSignals, TerminalSide};
use serde::Deserialize;
use starnix_uapi::errors::{EAGAIN, Errno};
use starnix_uapi::signals::{SIGINT, SIGQUIT, SIGTSTP, Signal};
use starnix_uapi::vfs::FdEvents;
use starnix_uapi::{error, uapi};
use std::collections::HashMap;

const DEFAULT_READ_SIZE: usize = 4096;

const fn default_read_size() -> usize {
    DEFAULT_READ_SIZE
}

#[derive(Deserialize, Debug)]
struct Scenario {
    name: String,
    initial_termios: TermiosConfig,
    events: Vec<Event>,
    final_termios: TermiosConfig,
}

#[derive(Deserialize, Debug)]
struct TermiosConfig {
    #[serde(default)]
    c_iflag: Vec<String>,
    #[serde(default)]
    c_oflag: Vec<String>,
    #[serde(default)]
    c_lflag: Vec<String>,
    #[serde(rename = "c_cflag")]
    _c_cflag: Option<u32>,
    c_cc: Option<HashMap<String, u8>>,
}

#[derive(Deserialize, Debug)]
#[serde(untagged)]
enum TraceData {
    Bytes(Vec<u8>),
    String(String),
}

impl TraceData {
    fn to_bytes(&self) -> Vec<u8> {
        match self {
            Self::Bytes(bytes) => bytes.clone(),
            Self::String(text) => text.as_bytes().to_vec(),
        }
    }
}

#[derive(Deserialize, Debug, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum Side {
    Main,
    Replica,
}

impl From<Side> for TerminalSide {
    fn from(side: Side) -> Self {
        match side {
            Side::Main => Self::Main,
            Side::Replica => Self::Replica,
        }
    }
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type")]
enum Event {
    #[serde(rename = "write_to_master")]
    WriteToMaster {
        data: TraceData,
        #[serde(default)]
        signals: Option<Vec<String>>,
    },
    #[serde(rename = "write_to_master_blocked")]
    WriteToMasterBlocked { data: TraceData },
    #[serde(rename = "read_from_master")]
    ReadFromMaster { data: TraceData },
    #[serde(rename = "read_from_slave")]
    ReadFromSlave {
        data: TraceData,
        #[serde(default)]
        signals: Option<Vec<String>>,
    },
    #[serde(rename = "read_once_from_master")]
    ReadOnceFromMaster {
        #[serde(default = "default_read_size")]
        size: usize,
        data: TraceData,
    },
    #[serde(rename = "read_once_from_master_blocked")]
    ReadOnceFromMasterBlocked {
        #[serde(default = "default_read_size")]
        size: usize,
    },
    #[serde(rename = "read_once_from_master_eio")]
    ReadOnceFromMasterEio {
        #[serde(default = "default_read_size")]
        size: usize,
    },
    #[serde(rename = "read_once_from_slave")]
    ReadOnceFromSlave {
        #[serde(default = "default_read_size")]
        size: usize,
        data: TraceData,
        #[serde(default)]
        signals: Option<Vec<String>>,
    },
    #[serde(rename = "read_once_from_slave_blocked")]
    ReadOnceFromSlaveBlocked {
        #[serde(default = "default_read_size")]
        size: usize,
    },
    #[serde(rename = "write_to_slave")]
    WriteToSlave { data: TraceData },
    #[serde(rename = "write_to_slave_blocked")]
    WriteToSlaveBlocked { data: TraceData },
    #[serde(rename = "write_to_slave_eio")]
    WriteToSlaveEio { data: TraceData },
    #[serde(rename = "set_packet_mode")]
    SetPacketMode { enabled: bool },
    #[serde(rename = "set_termios")]
    SetTermios {
        termios: TermiosConfig,
        #[serde(default)]
        signals: Option<Vec<String>>,
    },
    #[serde(rename = "flush")]
    Flush { side: Side, queue_selector: String },
    #[serde(rename = "wait_until_readable")]
    WaitUntilReadable { side: Side },
    #[serde(rename = "check_readable_size")]
    CheckReadableSize { side: Side, size: usize },
    #[serde(rename = "check_poll")]
    CheckPoll { side: Side, events: Vec<String> },
    #[serde(rename = "close")]
    Close { side: Side },
}

struct TestInputBuffer {
    data: Vec<u8>,
}

impl TestInputBuffer {
    fn new(data: Vec<u8>) -> Self {
        Self { data }
    }
}

impl InputBuffer for TestInputBuffer {
    fn available(&self) -> usize {
        self.data.len()
    }

    fn read_to_vec_exact(&mut self, size: usize) -> Result<Vec<u8>, Errno> {
        if size > self.data.len() {
            return error!(EAGAIN);
        }
        Ok(self.data.drain(0..size).collect())
    }
}

struct TestOutputBuffer {
    data: Vec<u8>,
    max_size: Option<usize>,
}

impl TestOutputBuffer {
    fn unbounded() -> Self {
        Self { data: Vec::new(), max_size: None }
    }

    fn with_limit(max_size: usize) -> Self {
        Self { data: Vec::new(), max_size: Some(max_size) }
    }
}

impl OutputBuffer for TestOutputBuffer {
    fn available(&self) -> usize {
        match self.max_size {
            Some(limit) => limit.saturating_sub(self.data.len()),
            None => usize::MAX,
        }
    }

    fn write(&mut self, data: &[u8]) -> Result<usize, Errno> {
        let to_write = std::cmp::min(self.available(), data.len());
        self.data.extend_from_slice(&data[..to_write]);
        Ok(to_write)
    }
}

const IFLAG_MAPPING: &[(u32, &str)] = &[
    (starnix_uapi::IGNBRK, "IGNBRK"),
    (starnix_uapi::BRKINT, "BRKINT"),
    (starnix_uapi::IGNPAR, "IGNPAR"),
    (starnix_uapi::PARMRK, "PARMRK"),
    (starnix_uapi::INPCK, "INPCK"),
    (starnix_uapi::ISTRIP, "ISTRIP"),
    (starnix_uapi::INLCR, "INLCR"),
    (starnix_uapi::IGNCR, "IGNCR"),
    (starnix_uapi::ICRNL, "ICRNL"),
    (starnix_uapi::IUCLC, "IUCLC"),
    (starnix_uapi::IXON, "IXON"),
    (starnix_uapi::IXANY, "IXANY"),
    (starnix_uapi::IXOFF, "IXOFF"),
    (starnix_uapi::IMAXBEL, "IMAXBEL"),
    (starnix_uapi::IUTF8, "IUTF8"),
];

const OFLAG_MAPPING: &[(u32, &str)] = &[
    (starnix_uapi::OPOST, "OPOST"),
    (starnix_uapi::OLCUC, "OLCUC"),
    (starnix_uapi::ONLCR, "ONLCR"),
    (starnix_uapi::OCRNL, "OCRNL"),
    (starnix_uapi::ONOCR, "ONOCR"),
    (starnix_uapi::ONLRET, "ONLRET"),
    (starnix_uapi::OFILL, "OFILL"),
    (starnix_uapi::OFDEL, "OFDEL"),
    (starnix_uapi::XTABS, "XTABS"),
];

const LFLAG_MAPPING: &[(u32, &str)] = &[
    (starnix_uapi::ISIG, "ISIG"),
    (starnix_uapi::ICANON, "ICANON"),
    (starnix_uapi::XCASE, "XCASE"),
    (starnix_uapi::ECHO, "ECHO"),
    (starnix_uapi::ECHOE, "ECHOE"),
    (starnix_uapi::ECHOK, "ECHOK"),
    (starnix_uapi::ECHONL, "ECHONL"),
    (starnix_uapi::ECHOCTL, "ECHOCTL"),
    (starnix_uapi::ECHOPRT, "ECHOPRT"),
    (starnix_uapi::ECHOKE, "ECHOKE"),
    (starnix_uapi::FLUSHO, "FLUSHO"),
    (starnix_uapi::NOFLSH, "NOFLSH"),
    (starnix_uapi::TOSTOP, "TOSTOP"),
    (starnix_uapi::PENDIN, "PENDIN"),
    (starnix_uapi::IEXTEN, "IEXTEN"),
    (starnix_uapi::EXTPROC, "EXTPROC"),
];

const CC_MAPPING: &[(&str, usize)] = &[
    ("VMIN", starnix_uapi::VMIN as usize),
    ("VTIME", starnix_uapi::VTIME as usize),
    ("VINTR", starnix_uapi::VINTR as usize),
    ("VQUIT", starnix_uapi::VQUIT as usize),
    ("VERASE", starnix_uapi::VERASE as usize),
    ("VKILL", starnix_uapi::VKILL as usize),
    ("VEOF", starnix_uapi::VEOF as usize),
    ("VSTART", starnix_uapi::VSTART as usize),
    ("VSTOP", starnix_uapi::VSTOP as usize),
    ("VSUSP", starnix_uapi::VSUSP as usize),
    ("VEOL", starnix_uapi::VEOL as usize),
    ("VREPRINT", starnix_uapi::VREPRINT as usize),
    ("VDISCARD", starnix_uapi::VDISCARD as usize),
    ("VWERASE", starnix_uapi::VWERASE as usize),
    ("VLNEXT", starnix_uapi::VLNEXT as usize),
    ("VEOL2", starnix_uapi::VEOL2 as usize),
];

fn parse_flags(flags: &[String], mapping: &[(u32, &str)]) -> u32 {
    let mut result = 0;
    for flag in flags {
        if let Some((val, _)) = mapping.iter().find(|(_, name)| name == flag) {
            result |= val;
        } else {
            panic!("Unknown flag {}", flag);
        }
    }
    result
}

fn apply_termios_config(mut termios: uapi::termios2, config: &TermiosConfig) -> uapi::termios2 {
    termios.c_iflag = parse_flags(&config.c_iflag, IFLAG_MAPPING);
    termios.c_oflag = parse_flags(&config.c_oflag, OFLAG_MAPPING);
    termios.c_lflag = parse_flags(&config.c_lflag, LFLAG_MAPPING);
    if let Some(cc) = &config.c_cc {
        for (name, &val) in cc {
            if let Some(&(_, idx)) = CC_MAPPING.iter().find(|&&(cc_name, _)| cc_name == name) {
                if idx < termios.c_cc.len() {
                    termios.c_cc[idx] = val;
                }
            } else {
                panic!("Unknown c_cc name {}", name);
            }
        }
    }
    termios
}

fn signal_to_name(sig: Signal) -> &'static str {
    if sig == SIGINT {
        "SIGINT"
    } else if sig == SIGQUIT {
        "SIGQUIT"
    } else if sig == SIGTSTP {
        "SIGTSTP"
    } else {
        panic!("Unexpected signal {:?}", sig);
    }
}

fn check_signals(
    pending_signals: PendingSignals,
    expected_signals: Option<Vec<String>>,
    event_name: &str,
    scenario_name: &str,
) {
    let actual: Vec<String> =
        pending_signals.signals().iter().map(|&s| signal_to_name(s).to_string()).collect();
    let expected = expected_signals.unwrap_or_default();
    assert_eq!(actual, expected, "{} signals mismatch in {}", event_name, scenario_name);
}

fn decompose_fd_events(events: FdEvents) -> Vec<String> {
    let mut out = Vec::new();
    for (flag, name) in [
        (FdEvents::POLLIN, "POLLIN"),
        (FdEvents::POLLOUT, "POLLOUT"),
        (FdEvents::POLLPRI, "POLLPRI"),
        (FdEvents::POLLERR, "POLLERR"),
        (FdEvents::POLLHUP, "POLLHUP"),
    ] {
        if events.contains(flag) {
            out.push(name.to_string());
        }
    }
    out
}

fn query_events_for_side(ld: &LineDiscipline, side: Side) -> FdEvents {
    match side {
        Side::Main => ld.main_query_events(),
        Side::Replica => ld.replica_query_events(),
    }
}

/// Replays a serialized Linux trace scenario against [`LineDiscipline`] and asserts all intermediate
/// reads, signals, poll readiness masks, and final `termios` flags match.
pub fn test_replay_trace(name: &str, json_data: &str) {
    println!("Running trace: {}", name);
    let scenario: Scenario = serde_json::from_str(json_data).unwrap_or_else(|e| {
        panic!("Failed to parse trace {}: {}", name, e);
    });
    run_scenario(scenario);
}

fn run_scenario(scenario: Scenario) {
    let mut ld = LineDiscipline::default();
    ld.main_open();
    ld.replica_open();

    let termios = apply_termios_config(*ld.termios(), &scenario.initial_termios);
    let initial_signals = ld.set_termios(termios);
    assert!(initial_signals.signals().is_empty());

    for event in scenario.events {
        match event {
            Event::WriteToMaster { data, signals: expected_signals } => {
                let bytes = data.to_bytes();
                let mut buffer = TestInputBuffer::new(bytes.clone());
                let (written, pending_signals) =
                    ld.main_write(&mut buffer).expect("main_write failed");
                assert_eq!(
                    written,
                    bytes.len(),
                    "WriteToMaster partial write in {}",
                    scenario.name
                );
                check_signals(pending_signals, expected_signals, "WriteToMaster", &scenario.name);
            }
            Event::WriteToMasterBlocked { data } => {
                let mut buffer = TestInputBuffer::new(data.to_bytes());
                assert_eq!(
                    ld.main_write(&mut buffer),
                    error!(EAGAIN),
                    "Expected main_write to return EAGAIN in {}",
                    scenario.name
                );
            }
            Event::ReadFromMaster { data } => {
                let mut buffer = TestOutputBuffer::unbounded();
                loop {
                    match ld.main_read(&mut buffer) {
                        Ok(_) => {}
                        Err(err) if err.code == EAGAIN => break,
                        Err(err) => panic!("main_read failed: {:?}", err),
                    }
                }
                let expected = data.to_bytes();
                assert_eq!(
                    buffer.data,
                    expected,
                    "ReadFromMaster mismatch in {} (actual={:?}, expected={:?})",
                    scenario.name,
                    String::from_utf8_lossy(&buffer.data),
                    String::from_utf8_lossy(&expected),
                );
            }
            Event::ReadFromSlave { data, signals: expected_signals } => {
                let mut buffer = TestOutputBuffer::unbounded();
                let mut pending_signals = PendingSignals::new();
                loop {
                    match ld.replica_read(&mut buffer) {
                        Ok((_, signals)) => pending_signals.append(signals),
                        Err(err) if err.code == EAGAIN => break,
                        Err(err) => panic!("replica_read failed: {:?}", err),
                    }
                }
                check_signals(pending_signals, expected_signals, "ReadFromSlave", &scenario.name);
                let expected = data.to_bytes();
                assert_eq!(
                    buffer.data,
                    expected,
                    "ReadFromSlave mismatch in {} (actual={:?}, expected={:?})",
                    scenario.name,
                    String::from_utf8_lossy(&buffer.data),
                    String::from_utf8_lossy(&expected),
                );
            }
            Event::ReadOnceFromMaster { size, data } => {
                let mut buffer = TestOutputBuffer::with_limit(size);
                let expected = data.to_bytes();
                let n = ld.main_read(&mut buffer).unwrap_or_else(|e| {
                    panic!("ReadOnceFromMaster failed in {}: {:?}", scenario.name, e)
                });
                assert_eq!(
                    n,
                    expected.len(),
                    "ReadOnceFromMaster length mismatch in {}",
                    scenario.name
                );
                assert_eq!(
                    buffer.data,
                    expected,
                    "ReadOnceFromMaster data mismatch in {} (actual={:?}, expected={:?})",
                    scenario.name,
                    String::from_utf8_lossy(&buffer.data),
                    String::from_utf8_lossy(&expected),
                );
            }
            Event::ReadOnceFromMasterBlocked { size } => {
                let mut buffer = TestOutputBuffer::with_limit(size);
                assert_eq!(
                    ld.main_read(&mut buffer),
                    error!(EAGAIN),
                    "Expected ReadOnceFromMaster to return EAGAIN in {}",
                    scenario.name
                );
            }
            Event::ReadOnceFromMasterEio { size } => {
                let mut buffer = TestOutputBuffer::with_limit(size);
                assert_eq!(
                    ld.main_read(&mut buffer),
                    error!(EIO),
                    "Expected ReadOnceFromMaster to return EIO in {}",
                    scenario.name
                );
            }
            Event::ReadOnceFromSlave { size, data, signals: expected_signals } => {
                let mut buffer = TestOutputBuffer::with_limit(size);
                let expected = data.to_bytes();
                let (n, pending_signals) = ld.replica_read(&mut buffer).unwrap_or_else(|e| {
                    panic!("ReadOnceFromSlave failed in {}: {:?}", scenario.name, e)
                });
                check_signals(
                    pending_signals,
                    expected_signals,
                    "ReadOnceFromSlave",
                    &scenario.name,
                );
                assert_eq!(
                    n,
                    expected.len(),
                    "ReadOnceFromSlave length mismatch in {}",
                    scenario.name
                );
                assert_eq!(
                    buffer.data,
                    expected,
                    "ReadOnceFromSlave data mismatch in {} (actual={:?}, expected={:?})",
                    scenario.name,
                    String::from_utf8_lossy(&buffer.data),
                    String::from_utf8_lossy(&expected),
                );
            }
            Event::ReadOnceFromSlaveBlocked { size } => {
                let mut buffer = TestOutputBuffer::with_limit(size);
                assert_eq!(
                    ld.replica_read(&mut buffer),
                    error!(EAGAIN),
                    "Expected ReadOnceFromSlave to return EAGAIN in {}",
                    scenario.name
                );
            }
            Event::WriteToSlave { data } => {
                let bytes = data.to_bytes();
                let mut buffer = TestInputBuffer::new(bytes.clone());
                let written = ld.replica_write(&mut buffer).expect("replica_write failed");
                assert_eq!(written, bytes.len(), "WriteToSlave partial write in {}", scenario.name);
            }
            Event::WriteToSlaveBlocked { data } => {
                let mut buffer = TestInputBuffer::new(data.to_bytes());
                assert_eq!(
                    ld.replica_write(&mut buffer),
                    error!(EAGAIN),
                    "Expected EAGAIN in {}",
                    scenario.name
                );
            }
            Event::WriteToSlaveEio { data } => {
                let mut buffer = TestInputBuffer::new(data.to_bytes());
                assert_eq!(
                    ld.replica_write(&mut buffer),
                    error!(EIO),
                    "Expected replica_write to return EIO in {}",
                    scenario.name
                );
            }
            Event::SetPacketMode { enabled } => {
                ld.set_packet_mode(enabled);
            }
            Event::SetTermios { termios: ref termios_config, signals: expected_signals } => {
                let termios = apply_termios_config(*ld.termios(), termios_config);
                let pending_signals = ld.set_termios(termios);
                check_signals(pending_signals, expected_signals, "SetTermios", &scenario.name);
            }
            Event::Flush { side, queue_selector } => {
                let selector = match queue_selector.as_str() {
                    "TCIFLUSH" => starnix_uapi::TCIFLUSH,
                    "TCOFLUSH" => starnix_uapi::TCOFLUSH,
                    "TCIOFLUSH" => starnix_uapi::TCIOFLUSH,
                    _ => panic!("Unknown queue_selector {}", queue_selector),
                };
                ld.flush(TerminalSide::from(side), selector).expect("flush failed");
            }
            Event::WaitUntilReadable { side } => {
                let events = query_events_for_side(&ld, side);
                assert!(
                    events.intersects(FdEvents::POLLIN | FdEvents::POLLHUP),
                    "Expected {:?} to be readable in {}, got {:?}",
                    side,
                    scenario.name,
                    events
                );
            }
            Event::CheckReadableSize { side, size } => {
                assert_eq!(
                    ld.get_available_read_size(TerminalSide::from(side)),
                    size,
                    "CheckReadableSize ({:?}) mismatch in {}",
                    side,
                    scenario.name
                );
            }
            Event::CheckPoll { side, events } => {
                let actual_events = query_events_for_side(&ld, side);
                assert_eq!(
                    decompose_fd_events(actual_events),
                    events,
                    "CheckPoll ({:?}) mismatch in {}",
                    side,
                    scenario.name
                );
            }
            Event::Close { side } => match side {
                Side::Main => ld.main_close(),
                Side::Replica => ld.replica_close(),
            },
        }
    }

    assert_eq!(
        ld.termios().c_iflag,
        parse_flags(&scenario.final_termios.c_iflag, IFLAG_MAPPING),
        "final_termios.c_iflag mismatch in {}",
        scenario.name
    );
    assert_eq!(
        ld.termios().c_oflag,
        parse_flags(&scenario.final_termios.c_oflag, OFLAG_MAPPING),
        "final_termios.c_oflag mismatch in {}",
        scenario.name
    );
    assert_eq!(
        ld.termios().c_lflag,
        parse_flags(&scenario.final_termios.c_lflag, LFLAG_MAPPING),
        "final_termios.c_lflag mismatch in {}",
        scenario.name
    );
}
