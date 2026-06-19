// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::error::MessageError;
use crate::{ExtendedMetadata, MessageFormatter};
use bumpalo::Bump;
use bumpalo::collections::{String as BumpaloString, Vec as BumpaloVec};
use diagnostics_data::{ExtendedMoniker, Severity};
use diagnostics_log_encoding::{Argument, Record, Value};
use flyweights::FlyStr;
use static_assertions::const_assert;
use std::fmt::Write;
use std::marker::PhantomData;
use std::str;
use zx::BootInstant;

pub use crate::constants::*;

/// Array for FFI purposes between C++ and Rust.
/// If len is 0, ptr is allowed to be nullptr,
/// otherwise, ptr must be valid.
#[repr(C)]
pub struct CppArray<'a, T> {
    /// Number of elements in the array
    pub len: usize,
    /// Pointer to the first element in the array,
    /// may be null in the case of a 0 length array,
    /// but is not guaranteed to always be null of
    /// len is 0.
    pub ptr: *const T,

    phantom: PhantomData<&'a T>,
}

impl<T> Default for CppArray<'_, T> {
    fn default() -> Self {
        CppArray { len: 0, ptr: std::ptr::null(), phantom: PhantomData }
    }
}

impl CppArray<'_, u8> {
    /// # Safety
    ///
    /// input must refer to a valid string, sized according to len.
    /// A valid string consists of UTF-8 characters. The caller
    /// is responsible for ensuring the byte sequence consists of valid UTF-8
    /// characters.
    ///
    pub unsafe fn as_utf8_str(&self) -> &str {
        unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(self.ptr, self.len)) }
    }
}

impl<'a> From<&'a str> for CppArray<'a, u8> {
    fn from(value: &'a str) -> Self {
        value.as_bytes().into()
    }
}

impl<'a> From<Option<&'a str>> for CppArray<'a, u8> {
    fn from(value: Option<&'a str>) -> Self {
        value.map(|v| v.into()).unwrap_or_default()
    }
}

impl<'a, T> From<&'a [T]> for CppArray<'a, T> {
    fn from(value: &'a [T]) -> Self {
        CppArray { len: value.len(), ptr: value.as_ptr(), phantom: PhantomData }
    }
}

impl<'a> From<BumpaloString<'a>> for CppArray<'a, u8> {
    fn from(value: BumpaloString<'a>) -> Self {
        value.into_bump_str().into()
    }
}

/// Log message representation for FFI with C++
#[repr(C)]
pub struct LogMessage<'a> {
    /// Severity of a log message.
    severity: u8,
    /// Tags in a log message, guaranteed to be non-null.
    tags: CppArray<'a, CppArray<'a, u8>>,
    /// Process ID from a LogMessage, or 0 if unknown
    pid: u64,
    /// Thread ID from a LogMessage, or 0 if unknown
    tid: u64,
    /// Number of dropped log messages.
    dropped: u64,
    /// The UTF-encoded log message, guaranteed to be valid UTF-8.
    message: CppArray<'a, u8>,
    /// Timestamp on the boot timeline of the log message,
    /// in nanoseconds.
    timestamp: i64,
}

// These are allocated using the Bumpalo allocator.
const_assert!(!std::mem::needs_drop::<LogMessage<'_>>());

pub struct CPPLogMessageBuilder<'a> {
    severity: u8,
    tags: BumpaloVec<'a, BumpaloString<'a>>,
    pid: Option<u64>,
    tid: Option<u64>,
    dropped: u64,
    file: Option<String>,
    line: Option<u64>,
    moniker: Option<BumpaloString<'a>>,
    message: Option<String>,
    timestamp: i64,
    kvps: String,
    allocator: &'a Bump,
}

// Escape quotes in a string per the Feedback format
fn escape_quotes(input: &str, output: &mut String) {
    for ch in input.chars() {
        if ch == '"' || ch == '\\' {
            output.push('\\');
        }
        output.push(ch);
    }
}

impl<'a> CPPLogMessageBuilder<'a> {
    fn set_raw_severity(mut self, raw_severity: u8) -> Self {
        self.severity = raw_severity;
        self
    }

    fn add_tag(mut self, tag: impl Into<String>) -> Self {
        self.tags.push(BumpaloString::from_str_in(&tag.into(), self.allocator));
        self
    }

    fn set_pid(mut self, pid: u64) -> Self {
        self.pid = Some(pid);
        self
    }

    fn set_tid(mut self, tid: u64) -> Self {
        self.tid = Some(tid);
        self
    }

    fn set_dropped(mut self, dropped: u64) -> Self {
        self.dropped = dropped;
        self
    }

    fn set_file(mut self, file: impl Into<String>) -> Self {
        self.file = Some(file.into());
        self
    }

    fn set_line(mut self, line: u64) -> Self {
        self.line = Some(line);
        self
    }

    fn set_message(mut self, msg: impl Into<String>) -> Self {
        self.message = Some(msg.into());
        self
    }

    fn add_kvp(mut self, kvp: &Argument<'_>) -> Self {
        if !self.kvps.is_empty() {
            self.kvps.push(' ');
        }

        self.kvps.push_str(kvp.name());
        self.kvps.push('=');
        match kvp.value() {
            Value::Text(value) => {
                self.kvps.push('"');
                escape_quotes(&value, &mut self.kvps);
                self.kvps.push('"');
            }
            Value::SignedInt(value) => {
                write!(self.kvps, "{value}").unwrap();
            }
            Value::UnsignedInt(value) => {
                write!(self.kvps, "{value}").unwrap();
            }
            Value::Floating(value) => {
                write!(self.kvps, "{value}").unwrap();
            }
            Value::Boolean(value) => {
                if value {
                    write!(self.kvps, "true").unwrap();
                } else {
                    write!(self.kvps, "false").unwrap();
                }
            }
        }
        self
    }

    fn set_moniker(mut self, value: &str) -> Self {
        self.moniker = Some(BumpaloString::from_str_in(value, self.allocator));
        self
    }

    pub fn build(mut self) -> &'a mut LogMessage<'a> {
        let allocator = self.allocator;

        // Format the message in accordance with the Feedback format
        let msg_str = self
            .message
            .as_ref()
            .map(|value| bumpalo::format!(in &allocator,"{value}",))
            .unwrap_or_else(|| BumpaloString::new_in(allocator));

        let mut output = match (&self.file, &self.line) {
            (Some(file), Some(line)) => {
                let mut value = bumpalo::format!(in &allocator, "[{file}({line})]",);
                if !msg_str.is_empty() {
                    value.push(' ');
                }
                value
            }
            _ => BumpaloString::new_in(allocator),
        };

        output.push_str(&msg_str);
        if !msg_str.is_empty() && !self.kvps.is_empty() {
            output.push(' ');
        }
        output.push_str(&self.kvps);

        if let Some(moniker) = &self.moniker {
            let component_name = moniker.split("/").last();
            if let Some(component_name) = component_name
                && !self.tags.iter().any(|value| value.as_str() == component_name)
            {
                self.tags.insert(0, bumpalo::format!(in &allocator, "{}", component_name));
            }
        }

        let tags: &[_] =
            self.allocator.alloc_slice_fill_iter(self.tags.drain(..).map(|s| s.into()));

        allocator.alloc(LogMessage {
            severity: self.severity,
            dropped: self.dropped,
            tags: tags.into(),
            pid: self.pid.unwrap_or(0),
            tid: self.tid.unwrap_or(0),
            message: output.into(),
            timestamp: self.timestamp,
        })
    }
}

struct CPPLogMessageBuilderBuilder<'a>(&'a Bump);

impl<'a> CPPLogMessageBuilderBuilder<'a> {
    fn configure(
        self,
        _component_url: Option<FlyStr>,
        moniker: Option<ExtendedMoniker>,
        severity: Severity,
        timestamp: BootInstant,
    ) -> Result<CPPLogMessageBuilder<'a>, MessageError> {
        Ok(CPPLogMessageBuilder {
            severity: severity as u8,
            tags: BumpaloVec::new_in(self.0),
            pid: None,
            tid: None,
            dropped: 0,
            file: None,
            timestamp: timestamp.into_nanos(),
            line: None,
            allocator: self.0,
            kvps: String::new(),
            moniker: moniker.map(|value| bumpalo::format!(in self.0,"{}", value)),
            message: None,
        })
    }
}

pub fn build_logs_data<'a>(
    input: &Record<'_>,
    source: Option<ExtendedMetadata>,
    allocator: &'a Bump,
) -> Result<&'a mut LogMessage<'a>, MessageError> {
    let builder = CPPLogMessageBuilderBuilder(allocator);
    let (raw_severity, severity) = Severity::parse_exact(input.severity);
    let (maybe_moniker, maybe_url, _) = source
        .map(|value| (Some(value.moniker), Some(value.url), Some(value.rolled_out_logs)))
        .unwrap_or((None, None, None));
    let mut builder =
        builder.configure(maybe_url.map(FlyStr::new), None, severity, input.timestamp)?;
    if let Some(moniker) = maybe_moniker {
        builder = builder.set_moniker(moniker.as_ref());
    }
    if let Some(raw_severity) = raw_severity {
        builder = builder.set_raw_severity(raw_severity);
    }

    for argument in input.arguments.iter() {
        match argument {
            Argument::Tag(tag) => {
                builder = builder.add_tag(tag.as_ref());
            }
            Argument::Pid(pid) => {
                builder = builder.set_pid(pid.raw_koid());
            }
            Argument::Tid(tid) => {
                builder = builder.set_tid(tid.raw_koid());
            }
            Argument::Dropped(dropped) => {
                builder = builder.set_dropped(*dropped);
            }
            Argument::File(file) => {
                builder = builder.set_file(file.as_ref());
            }
            Argument::Line(line) => {
                builder = builder.set_line(*line);
            }
            Argument::Message(msg) => {
                builder = builder.set_message(msg.as_ref());
            }
            Argument::Other { value: _, name: _ } => builder = builder.add_kvp(argument),
        }
    }

    Ok(builder.build())
}

/// Constructs a `CPPLogsMessage` from the provided bytes, assuming the bytes
/// are in the format specified as in the [log encoding], and come from
///
/// an Archivist LogStream with moniker, URL, and dropped logs output enabled.
/// [log encoding] https://fuchsia.dev/fuchsia-src/development/logs/encodings
pub fn ffi_from_extended_record<'a, 'b>(
    bytes: &'a [u8],
    allocator: &'b Bump,
) -> Result<(&'b mut LogMessage<'b>, &'a [u8]), MessageError> {
    let (input, remaining) = diagnostics_log_encoding::parse::parse_record(bytes)?;
    let (source, new_remaining) = if remaining.len() >= 16 {
        let moniker_len = u32::from_le_bytes(remaining[0..4].try_into().unwrap()) as usize;
        let component_url_len = u32::from_le_bytes(remaining[4..8].try_into().unwrap()) as usize;
        let rolled_out_logs = u64::from_le_bytes(remaining[8..16].try_into().unwrap());
        let mut offset: usize = 16;

        // NOTE: This addition is safe as all platforms Fuchsia supports are 64-bit,
        // so usize will never overflow.
        let moniker_padded_len = (moniker_len + 7) & !7;
        let component_url_padded_len = (component_url_len + 7) & !7;
        let moniker_padded_end = offset + moniker_padded_len;
        let url_padded_end = moniker_padded_end + component_url_padded_len;
        if url_padded_end > remaining.len() {
            return Err(MessageError::OutOfBounds);
        }

        let moniker = str::from_utf8(&remaining[offset..offset + moniker_len])?;
        offset += moniker_padded_len;
        let url = str::from_utf8(&remaining[offset..offset + component_url_len])?;
        offset += component_url_padded_len;
        (
            Some(ExtendedMetadata {
                moniker: ExtendedMoniker::parse_str(moniker)?,
                url: url.into(),
                rolled_out_logs,
            }),
            &remaining[offset..],
        )
    } else {
        (None, remaining)
    };
    let record = build_logs_data(&input, source, allocator)?;
    Ok((record, new_remaining))
}

pub struct CPPMessageFormatter<'a>(pub &'a Bump);
impl<'a> MessageFormatter for &CPPMessageFormatter<'a> {
    type Result = &'a mut LogMessage<'a>;

    fn format(
        &mut self,
        record: &Record<'_>,
        metadata: Option<ExtendedMetadata>,
    ) -> Result<Self::Result, MessageError> {
        build_logs_data(record, metadata, self.0)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::MessageParser;
    use bumpalo::Bump;
    use diagnostics_log_encoding::encode::{Encoder, EncoderOpts};
    use diagnostics_log_encoding::{Argument, Header, LOG_CONTROL_BIT, Record};
    use std::io::Cursor;
    use zx::BootInstant;

    fn overwrite_header_tag(bytes: &mut [u8], tag: u32) {
        if bytes.len() >= 8 {
            let mut header = Header(u64::from_le_bytes(bytes[0..8].try_into().unwrap()));
            header.set_tag(tag);
            bytes[0..8].copy_from_slice(&header.0.to_le_bytes());
        }
    }

    #[fuchsia::test]
    fn test_short_read() {
        let mut parser = MessageParser::default();
        let allocator = Bump::new();
        let formatter = CPPMessageFormatter(&allocator);
        let bytes = vec![0u8; 7];
        let res = parser.parse_next(&bytes, &formatter);
        assert!(matches!(res, Err(MessageError::ShortRead { len: 7 })));
    }

    #[fuchsia::test]
    fn test_normal_parsing() {
        let mut parser = MessageParser::default();
        let allocator = Bump::new();
        let formatter = CPPMessageFormatter(&allocator);

        let record = Record {
            timestamp: BootInstant::from_nanos(72),
            severity: 0x30,
            arguments: vec![Argument::message("hello world")],
        };
        let mut buffer = Cursor::new(vec![0u8; 1024]);
        let mut encoder = Encoder::new(&mut buffer, EncoderOpts::default());
        encoder.write_record(record).unwrap();

        let len = buffer.position() as usize;
        let mut bytes = buffer.into_inner();
        bytes.truncate(len);

        let res = parser.parse_next(&bytes, &formatter).unwrap();
        assert!(res.0.is_some());
        let log_message = res.0.unwrap();
        assert_eq!(unsafe { log_message.message.as_utf8_str() }, "hello world");
        assert_eq!(log_message.timestamp, 72);
        assert_eq!(log_message.severity, 0x30);
    }

    #[fuchsia::test]
    fn test_escaping_in_kvp() {
        let mut parser = MessageParser::default();
        let allocator = Bump::new();
        let formatter = CPPMessageFormatter(&allocator);

        let record = Record {
            timestamp: BootInstant::from_nanos(72),
            severity: 0x30,
            arguments: vec![
                Argument::message("hello world"),
                Argument::new("key", r#"val"with\escapes"#),
            ],
        };
        let mut buffer = Cursor::new(vec![0u8; 1024]);
        let mut encoder = Encoder::new(&mut buffer, EncoderOpts::default());
        encoder.write_record(record).unwrap();

        let len = buffer.position() as usize;
        let mut bytes = buffer.into_inner();
        bytes.truncate(len);

        let res = parser.parse_next(&bytes, &formatter).unwrap();
        assert!(res.0.is_some());
        let log_message = &res.0.unwrap();
        assert_eq!(
            unsafe { log_message.message.as_utf8_str() },
            r#"hello world key="val\"with\\escapes""#
        );
    }

    #[fuchsia::test]
    fn test_out_of_bounds_extended_record() {
        let allocator = Bump::new();

        let record = Record {
            timestamp: BootInstant::from_nanos(72),
            severity: 0x30,
            arguments: vec![Argument::message("hello world")],
        };
        let mut buffer = Cursor::new(vec![0u8; 1024]);
        let mut encoder = Encoder::new(&mut buffer, EncoderOpts::default());
        encoder.write_record(record).unwrap();
        let len = buffer.position() as usize;
        let mut bytes = buffer.into_inner();
        bytes.truncate(len);

        // Append a corrupt moniker_len or component_url_len in the remaining slice.
        let extended_metadata_suffix = [
            0xE8, 0x03, 0x00, 0x00, // moniker_len = 1000
            0xE8, 0x03, 0x00, 0x00, // component_url_len = 1000
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // rolled_out_logs = 0
        ];
        bytes.extend_from_slice(&extended_metadata_suffix);

        let res = ffi_from_extended_record(&bytes, &allocator);
        assert!(matches!(res, Err(MessageError::OutOfBounds)));
    }

    #[fuchsia::test]
    fn test_control_message_tags() {
        let allocator = Bump::new();
        let formatter = CPPMessageFormatter(&allocator);
        let mut parser = MessageParser::default();

        let tag_id = 0;

        let control_record = Record {
            timestamp: BootInstant::from_nanos(72),
            severity: 0x30,
            arguments: vec![
                Argument::new("moniker", "test/moniker"),
                Argument::new("url", "fuchsia-pkg://test"),
            ],
        };

        let mut buffer = Cursor::new(vec![0u8; 1024]);
        let mut encoder = Encoder::new(&mut buffer, EncoderOpts::default());
        encoder.write_record(control_record).unwrap();

        let len = buffer.position() as usize;
        let mut bytes = buffer.into_inner();
        bytes.truncate(len);

        overwrite_header_tag(&mut bytes, LOG_CONTROL_BIT);

        let (log, _) = parser.parse_next(&bytes, &formatter).unwrap();
        assert!(log.is_none());

        let tag_data = parser.tag_map.get(&tag_id).unwrap();
        assert_eq!(tag_data.moniker, ExtendedMoniker::parse_str("test/moniker").unwrap());
        assert_eq!(tag_data.url, "fuchsia-pkg://test");

        let rolled_out_record = Record {
            timestamp: BootInstant::from_nanos(73),
            severity: 0x30,
            arguments: vec![Argument::new("rolled_out", 5u64)],
        };

        let mut buffer2 = Cursor::new(vec![0u8; 1024]);
        let mut encoder2 = Encoder::new(&mut buffer2, EncoderOpts::default());
        encoder2.write_record(rolled_out_record).unwrap();

        let len2 = buffer2.position() as usize;
        let mut bytes2 = buffer2.into_inner();
        bytes2.truncate(len2);

        overwrite_header_tag(&mut bytes2, LOG_CONTROL_BIT);

        let (log2, _) = parser.parse_next(&bytes2, &formatter).unwrap();
        assert!(log2.is_some());

        let tag_data2 = parser.tag_map.get(&tag_id).unwrap();
        assert_eq!(unsafe { log2.unwrap().message.as_utf8_str() }, "rolled_out=5");
        assert_eq!(tag_data2.moniker, ExtendedMoniker::parse_str("test/moniker").unwrap());

        let normal_record = Record {
            timestamp: BootInstant::from_nanos(74),
            severity: 0x30,
            arguments: vec![Argument::message("some log with tag")],
        };

        let mut buffer3 = Cursor::new(vec![0u8; 1024]);
        let mut encoder3 = Encoder::new(&mut buffer3, EncoderOpts::default());
        encoder3.write_record(normal_record).unwrap();

        let len3 = buffer3.position() as usize;
        let mut bytes3 = buffer3.into_inner();
        bytes3.truncate(len3);

        overwrite_header_tag(&mut bytes3, tag_id);

        let (log3, _) = parser.parse_next(&bytes3, &formatter).unwrap();

        let log_msg3 = log3.unwrap();
        assert_eq!(unsafe { log_msg3.message.as_utf8_str() }, "some log with tag");
        assert_eq!(log_msg3.tags.len, 1);
        let tag_str = unsafe { (*log_msg3.tags.ptr).as_utf8_str() };
        assert_eq!(tag_str, "moniker");
    }

    #[fuchsia::test]
    fn test_message_with_kvps() {
        let mut parser = MessageParser::default();
        let allocator = Bump::new();
        let formatter = CPPMessageFormatter(&allocator);

        let record = Record {
            timestamp: BootInstant::from_nanos(100),
            severity: 0x10,
            arguments: vec![
                Argument::message("A message"),
                Argument::new("key1", "value1"),
                Argument::new("key2", 123u64),
            ],
        };
        let mut buffer = Cursor::new(vec![0u8; 1024]);
        let mut encoder = Encoder::new(&mut buffer, EncoderOpts::default());
        encoder.write_record(record).unwrap();

        let len = buffer.position() as usize;
        let mut bytes = buffer.into_inner();
        bytes.truncate(len);

        let res = parser.parse_next(&bytes, &formatter).unwrap();
        assert!(res.0.is_some());
        let log_message = res.0.unwrap();
        assert_eq!(
            unsafe { log_message.message.as_utf8_str() },
            "A message key1=\"value1\" key2=123"
        );
    }

    #[fuchsia::test]
    fn test_file_line_message_with_kvps() {
        let mut parser = MessageParser::default();
        let allocator = Bump::new();
        let formatter = CPPMessageFormatter(&allocator);

        let record = Record {
            timestamp: BootInstant::from_nanos(100),
            severity: 0x10,
            arguments: vec![
                Argument::file("src/file.rs"),
                Argument::line(42),
                Argument::message("Another message"),
                Argument::new("temp", 30.5),
                Argument::new("valid", true),
            ],
        };
        let mut buffer = Cursor::new(vec![0u8; 1024]);
        let mut encoder = Encoder::new(&mut buffer, EncoderOpts::default());
        encoder.write_record(record).unwrap();

        let len = buffer.position() as usize;
        let mut bytes = buffer.into_inner();
        bytes.truncate(len);

        let res = parser.parse_next(&bytes, &formatter).unwrap();
        assert!(res.0.is_some());
        let log_message = res.0.unwrap();
        assert_eq!(
            unsafe { log_message.message.as_utf8_str() },
            "[src/file.rs(42)] Another message temp=30.5 valid=true"
        );
    }

    #[fuchsia::test]
    fn test_only_kvps() {
        let mut parser = MessageParser::default();
        let allocator = Bump::new();
        let formatter = CPPMessageFormatter(&allocator);

        let record = Record {
            timestamp: BootInstant::from_nanos(100),
            severity: 0x10,
            arguments: vec![Argument::new("status", "ok"), Argument::new("code", 200i64)],
        };
        let mut buffer = Cursor::new(vec![0u8; 1024]);
        let mut encoder = Encoder::new(&mut buffer, EncoderOpts::default());
        encoder.write_record(record).unwrap();

        let len = buffer.position() as usize;
        let mut bytes = buffer.into_inner();
        bytes.truncate(len);

        let res = parser.parse_next(&bytes, &formatter).unwrap();
        assert!(res.0.is_some());
        let log_message = res.0.unwrap();
        assert_eq!(unsafe { log_message.message.as_utf8_str() }, "status=\"ok\" code=200");
    }
}
