// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#[cfg(test)]
mod tests {
    use crate::*;
    use serde::Deserialize;
    use starnix_uapi::errors::Errno;

    #[derive(Deserialize, Debug)]
    struct Scenario {
        name: String,
        initial_termios: TermiosConfig,
        events: Vec<Event>,
        #[allow(dead_code)]
        final_termios: TermiosConfig,
    }

    #[derive(Deserialize, Debug)]
    struct TermiosConfig {
        c_iflag: Vec<String>,
        c_oflag: Vec<String>,
        c_lflag: Vec<String>,
        #[allow(dead_code)]
        c_cflag: Option<u32>, // Keeping cflag simple for now or assume default
        c_cc: Option<Vec<u8>>,
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
                TraceData::Bytes(b) => b.clone(),
                TraceData::String(s) => s.as_bytes().to_vec(),
            }
        }
    }

    #[derive(Deserialize, Debug)]
    #[serde(tag = "type")]
    enum Event {
        #[serde(rename = "write_to_master")]
        WriteToMaster { data: TraceData },
        #[serde(rename = "read_from_master")]
        ReadFromMaster { data: TraceData },
        #[serde(rename = "read_from_slave")]
        ReadFromSlave { data: TraceData },
        #[serde(rename = "write_to_slave")]
        WriteToSlave { data: TraceData },
    }

    #[derive(Deserialize, Debug)]
    struct Trace {
        scenarios: Vec<Scenario>,
    }

    struct TestBuffer {
        data: Vec<u8>,
    }

    impl TestBuffer {
        fn new(data: Vec<u8>) -> Self {
            Self { data }
        }
    }

    impl InputBuffer for TestBuffer {
        fn available(&self) -> usize {
            self.data.len()
        }
        fn read_to_vec_exact(&mut self, size: usize) -> Result<Vec<u8>, Errno> {
            if size > self.data.len() {
                return error!(EAGAIN);
            }
            let result = self.data.drain(0..size).collect();
            Ok(result)
        }
    }

    struct TestOutputBuffer {
        data: Vec<u8>,
    }

    impl TestOutputBuffer {
        fn new() -> Self {
            Self { data: vec![] }
        }
    }

    impl OutputBuffer for TestOutputBuffer {
        fn write(&mut self, data: &[u8]) -> Result<usize, Errno> {
            self.data.extend_from_slice(data);
            Ok(data.len())
        }
    }

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

    fn get_iflag_mapping() -> Vec<(u32, &'static str)> {
        vec![
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
        ]
    }

    fn get_oflag_mapping() -> Vec<(u32, &'static str)> {
        vec![
            (starnix_uapi::OPOST, "OPOST"),
            (starnix_uapi::OLCUC, "OLCUC"),
            (starnix_uapi::ONLCR, "ONLCR"),
            (starnix_uapi::OCRNL, "OCRNL"),
            (starnix_uapi::ONOCR, "ONOCR"),
            (starnix_uapi::ONLRET, "ONLRET"),
            (starnix_uapi::OFILL, "OFILL"),
            (starnix_uapi::OFDEL, "OFDEL"),
        ]
    }

    fn get_lflag_mapping() -> Vec<(u32, &'static str)> {
        vec![
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
        ]
    }

    #[test]
    fn test_replay_traces() {
        let json_data = include_str!("testing/traces/canon_basic.json");
        let trace: Trace = serde_json::from_str(json_data).expect("Failed to parse trace");

        let iflags = get_iflag_mapping();
        let oflags = get_oflag_mapping();
        let lflags = get_lflag_mapping();

        for scenario in trace.scenarios {
            println!("Running scenario: {}", scenario.name);
            let mut ld = LineDiscipline::default();
            ld.main_open();
            ld.replica_open();

            // Set initial termios
            let mut termios = crate::get_default_termios();
            termios.c_iflag = parse_flags(&scenario.initial_termios.c_iflag, &iflags);
            termios.c_oflag = parse_flags(&scenario.initial_termios.c_oflag, &oflags);
            termios.c_lflag = parse_flags(&scenario.initial_termios.c_lflag, &lflags);

            if let Some(cc) = scenario.initial_termios.c_cc {
                for (i, &b) in cc.iter().enumerate() {
                    if i < termios.c_cc.len() {
                        termios.c_cc[i] = b;
                    }
                }
            }

            let _ = ld.set_termios(termios);

            for event in scenario.events {
                match event {
                    Event::WriteToMaster { data } => {
                        let mut buffer = TestBuffer::new(data.to_bytes());
                        let _ = ld.main_write(&mut buffer).expect("main_write failed");
                    }
                    Event::ReadFromMaster { data } => {
                        let mut buffer = TestOutputBuffer::new();
                        let _ = ld.main_read(&mut buffer).expect("main_read failed");
                        assert_eq!(
                            String::from_utf8_lossy(&buffer.data),
                            String::from_utf8_lossy(&data.to_bytes()),
                            "ReadFromMaster mismatch in {}",
                            scenario.name
                        );
                    }
                    Event::ReadFromSlave { data } => {
                        let mut buffer = TestOutputBuffer::new();
                        let _ = ld.replica_read(&mut buffer).expect("replica_read failed");
                        assert_eq!(
                            String::from_utf8_lossy(&buffer.data),
                            String::from_utf8_lossy(&data.to_bytes()),
                            "ReadFromSlave mismatch in {}",
                            scenario.name
                        );
                    }
                    Event::WriteToSlave { data } => {
                        let mut buffer = TestBuffer::new(data.to_bytes());
                        let _ = ld.replica_write(&mut buffer).expect("replica_write failed");
                    }
                }
            }
        }
    }
}
