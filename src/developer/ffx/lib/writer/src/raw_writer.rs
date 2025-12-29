// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{Result, ToolIO};
use async_trait::async_trait;
use fho::{FhoEnvironment, TryFromEnv};
use std::io::Write;
use writer::{Format, TestBuffers, Writer};

/// An object that can be used to produce output, but enforces that no machine
/// output format (like JSON) is requested.
pub struct RawWriter(Writer);

impl From<writer::Writer> for RawWriter {
    fn from(value: writer::Writer) -> Self {
        RawWriter(value)
    }
}

impl RawWriter {
    /// Create a new writer that doesn't support machine output at all, with the
    /// given streams underlying it.
    pub fn new_buffers<O, E>(stdout: O, stderr: E) -> Self
    where
        O: Write + 'static,
        E: Write + 'static,
    {
        Self(Writer::new_buffers(stdout, stderr))
    }

    /// Create a new Writer that doesn't support machine output at all
    pub fn new() -> Self {
        Self(Writer::new())
    }

    /// Returns a writer backed by string buffers that can be extracted after
    /// the writer is done with
    pub fn new_test(test_writer: &TestBuffers) -> Self {
        Self(Writer::new_test(test_writer))
    }
}

impl ToolIO for RawWriter {
    type OutputItem = String;

    fn is_machine_supported() -> bool {
        true
    }

    fn is_machine(&self) -> bool {
        false
    }

    fn item(&mut self, value: &String) -> Result<()> {
        self.line(value)
    }

    fn stderr(&mut self) -> &mut dyn Write {
        self.0.stderr()
    }
}

impl Write for RawWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

#[async_trait(?Send)]
impl TryFromEnv for RawWriter {
    async fn try_from_env(env: &FhoEnvironment) -> fho::Result<Self> {
        let machine_format: Option<Format> =
            env.ffx_command().global.machine.and_then(|mf| mf.into());
        match machine_format {
            Some(Format::Json) | Some(Format::JsonPretty) => {
                Err(fho::Error::User(anyhow::anyhow!("invalid machine format: only raw supported")))
            }
            // "--machine raw" produces "None" when parsed
            None => Ok(RawWriter::new()),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_not_machine_is_ok() {
        let test_buffers = TestBuffers::default();
        let mut writer = RawWriter::new_test(&test_buffers);
        let res = writer.item(&"hello".to_owned());
        assert!(res.is_ok());
    }

    #[test]
    fn test_item_for_test() {
        let test_buffers = TestBuffers::default();
        let mut writer = RawWriter::new_test(&test_buffers);
        writer.item(&"hello".to_owned()).unwrap();

        assert_eq!(test_buffers.into_stdout_str(), "hello\n");
    }

    #[test]
    fn test_is_machine_false() {
        let test_buffers = TestBuffers::default();
        let writer = RawWriter::new_test(&test_buffers);
        assert!(!writer.is_machine());
    }
}
