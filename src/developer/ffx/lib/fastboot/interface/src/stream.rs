// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use crate::util::{U32_SIZE, convert_log_err};
use std::cmp::min;
use std::range::Range;
use zerocopy::IntoBytes;

// Streaming flash is an optional protocol used to reduce bandwidth,
// break up images that are too large for the device download buffer,
// and leverage device async I/O to speed partition flashing.
//
// Terminology used for the data structures in this module:
// * A `StreamCommandList` is a list of commands that describe how to write a
//   partition image to the target partition. These commands are independent and
//   idempotent.
// * A `StreamCommand` is a range within the target partition and a description
//   of what the device should do within that range to generate data to write.
// * A `StreamOp` is a description of the bytes defining the owning `StreamCommand`:
//   either a single repeated 32 bit integer, or arbitrary data with a CRC32 checksum.
//
// `Chunk` and `Payload` correspond to `StreamCommand` and `StreamOp` but are
// used as intermediate structures while processing the partition image into operations.
#[derive(Debug, PartialEq)]
pub enum StreamOp<'a> {
    Flash { data: &'a [u8], crc32: u32 },
    Fill { val: u32, length_bytes: u64 },
}

impl<'a> StreamOp<'a> {
    fn from_data(data: &'a [u8]) -> Self {
        Self::Flash { data, crc32: crc32fast::hash(data) }
    }

    fn from_fill(val: u32, length_bytes: u64) -> Self {
        Self::Fill { val, length_bytes }
    }
}

#[derive(Debug, PartialEq)]
pub struct StreamCommand<'a> {
    pub offset_bytes: u64,
    pub op: StreamOp<'a>,
}

impl<'a> StreamCommand<'a> {
    fn from_fill(val: u32, offset_words: u64, length_words: u64) -> Self {
        Self {
            offset_bytes: offset_words * U32_SIZE,
            op: StreamOp::from_fill(val, length_words * U32_SIZE),
        }
    }

    fn from_data(data: &'a [u32], offset_words: u64) -> Self {
        Self { offset_bytes: offset_words * U32_SIZE, op: StreamOp::from_data(data.as_bytes()) }
    }
}

pub struct StreamCommandList<'a> {
    pub commands: Vec<StreamCommand<'a>>,
}

struct Chunk {
    range: Range<u64>,
    payload: Payload,
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum Payload {
    Fill(u32),
    Flash,
}

impl Payload {
    fn from_segment(segment: &[u32]) -> Self {
        // If segment.iter().next().is_none(),
        // we get a 0-length Fill segment with a payload of 0,
        // which is a no-op.
        // Technically we could filter these no-op commands,
        // but even though they are syntactically possible in this context
        // they aren't possible due to the way that chunk_range is constructed.
        let val = segment.iter().next().unwrap_or(&0);
        if segment.iter().all(|v| v == val) { Self::Fill(*val) } else { Self::Flash }
    }
}

fn range_length<T: std::ops::Sub>(range: Range<T>) -> <T as std::ops::Sub>::Output {
    let Range { start, end } = range;
    end - start
}

impl Chunk {
    fn from_payload(range: Range<u64>, payload: Payload) -> Self {
        Self { range, payload }
    }

    fn from_data(range: Range<u64>, data: &[u32]) -> Self {
        Self::from_payload(range, Payload::from_segment(&data[convert_range(range)]))
    }

    fn start_word(&self) -> u64 {
        self.range.start
    }

    fn len_words(&self) -> u64 {
        range_length(self.range)
    }

    fn with_extension(self, length: u64) -> Self {
        let Self { range: Range { start, end }, payload } = self;
        Self { range: Range { start, end: end + length }, payload }
    }
}

fn convert_range<T, U>(r: Range<T>) -> Range<U>
where
    U: TryFrom<T>,
    <U as TryFrom<T>>::Error: std::error::Error,
{
    Range { start: convert_log_err(r.start).unwrap(), end: convert_log_err(r.end).unwrap() }
}

// Process the segment into the work-in-progress chunk.
// The current chunk is either expanded to include the new segment OR
// is wrapped up into a completed command, pushed to the command vector,
// and a new work-in-progress chunk is returned.
fn process_segment<'a>(
    data: &'a [u32],
    segment_range: Range<u64>,
    max_download_words: u64,
    commands: &mut Vec<StreamCommand<'a>>,
    chunk: Chunk,
) -> Chunk {
    use Payload::*;

    let segment_len_words = range_length(segment_range);
    let new_payload = Payload::from_segment(&data[convert_range(segment_range)]);
    match (chunk.payload, new_payload) {
        // New payload is a flash or a fill with a different value
        (p1 @ Fill(val), p2) if p1 != p2 => {
            commands.push(StreamCommand::from_fill(val, chunk.start_word(), chunk.len_words()));
            Chunk::from_payload(segment_range, new_payload)
        }
        (Flash, Fill(_)) => {
            commands.push(StreamCommand::from_data(
                &data[convert_range(chunk.range)],
                chunk.start_word(),
            ));
            Chunk::from_payload(segment_range, new_payload)
        }
        (Flash, Flash)
            if chunk.len_words().checked_add(segment_len_words).expect("Integer overflow")
                > max_download_words =>
        {
            // Can't have a Flash segment longer than max_download_words.
            // Even though the following chunk is also raw data, we need to
            // split it up into multiple commands.
            commands.push(StreamCommand::from_data(
                &data[convert_range(chunk.range)],
                chunk.start_word(),
            ));
            Chunk::from_payload(segment_range, new_payload)
        }
        // Matching fill or under max size flash
        _ => chunk.with_extension(segment_len_words),
    }
}

/// Generate a list of streaming flash commands.
/// Takes a partition image, the max download size the device supports,
/// the streaming segment size, and the partition offset from the start of the device.
///
/// Note: `data` is a slice of u32 and `max_download_words`, `segment_size_words`,
/// and `partition_start_word` are all denominated in u32 to enforce good alignment
/// for checking if a segment can be represented as Fill, which takes a u32 value.
pub fn generate_command_list<'a>(
    data: &'a [u32],
    max_download_words: u64,
    segment_size_words: u64,
    partition_start_word: u64,
) -> StreamCommandList<'a> {
    let prefix_words =
        partition_start_word.next_multiple_of(segment_size_words) - partition_start_word;

    let data_len: u64 = convert_log_err(data.len()).unwrap();
    let prefix_words = if prefix_words == 0 { segment_size_words } else { prefix_words };
    // The first chunk primes the process_segment sequence
    // and handles a possible unaligned prefix.
    let mut chunk = Chunk::from_data(Range { start: 0, end: min(prefix_words, data_len) }, data);

    let mut commands = vec![];
    for segment_range in (prefix_words..data_len)
        .step_by(convert_log_err(segment_size_words).unwrap())
        .map(|start| Range { start, end: start + min(segment_size_words, data_len - start) })
    {
        chunk = process_segment(data, segment_range, max_download_words, &mut commands, chunk);
    }

    let final_command = match chunk.payload {
        Payload::Fill(val) => StreamCommand::from_fill(val, chunk.start_word(), chunk.len_words()),
        Payload::Flash => {
            StreamCommand::from_data(&data[convert_range(chunk.range)], chunk.start_word())
        }
    };
    commands.push(final_command);

    StreamCommandList { commands }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::util::multi_chain;

    #[fuchsia::test()]
    fn test_stream_command_generation_basic() {
        const SEGMENT_SIZE_WORDS: u32 = 1024;
        let flash_data = 0u32..SEGMENT_SIZE_WORDS;
        let fill_data = std::iter::repeat(0u32).take(SEGMENT_SIZE_WORDS as usize);
        let data = multi_chain!(
            flash_data.clone(),
            fill_data.clone(),
            fill_data.clone(),
            fill_data.clone(),
            flash_data.clone(),
        )
        .collect::<Vec<_>>();

        let command_list =
            generate_command_list(data.as_slice(), 1 << 28, SEGMENT_SIZE_WORDS.into(), 0);

        assert_eq!(
            command_list.commands,
            [
                StreamCommand::from_data(&Vec::from_iter(flash_data.clone()), 0),
                StreamCommand::from_fill(0, 1024, 3 * 1024),
                StreamCommand::from_data(&Vec::from_iter(flash_data.clone()), 4096),
            ]
        );
    }

    #[fuchsia::test()]
    fn test_stream_command_generation_different_fill() {
        const SEGMENT_SIZE_WORDS: u32 = 1024;
        let fill_zero = std::iter::repeat(0u32).take(SEGMENT_SIZE_WORDS as usize);
        let fill_one = std::iter::repeat(1u32).take(SEGMENT_SIZE_WORDS as usize);

        let data = multi_chain!(
            fill_zero.clone(),
            fill_zero.clone(),
            fill_one.clone(),
            fill_one.clone(),
            fill_zero.clone(),
            fill_one.clone(),
            fill_one.clone(),
            fill_one.clone(),
            fill_one.clone(),
            fill_zero.clone(),
            fill_zero.clone(),
        )
        .collect::<Vec<_>>();

        let command_list =
            generate_command_list(data.as_slice(), 1 << 28, SEGMENT_SIZE_WORDS.into(), 0);

        assert_eq!(
            command_list.commands,
            [
                StreamCommand::from_fill(0, 0, 2048),
                StreamCommand::from_fill(1, 2048, 2048),
                StreamCommand::from_fill(0, 4096, 1024),
                StreamCommand::from_fill(1, 5120, 4096),
                StreamCommand::from_fill(0, 9216, 2048),
            ]
        );
    }

    #[fuchsia::test()]
    fn test_stream_command_generation_split_flash() {
        const SEGMENT_SIZE_WORDS: u32 = 1024;

        let flash_data = 0..SEGMENT_SIZE_WORDS;
        let data = multi_chain!(
            flash_data.clone(),
            flash_data.clone(),
            flash_data.clone(),
            flash_data.clone(),
            flash_data.clone(),
            flash_data.clone(),
            flash_data.clone(),
            flash_data.clone(),
        )
        .collect::<Vec<_>>();

        let command_list = generate_command_list(
            data.as_slice(),
            (SEGMENT_SIZE_WORDS * 2).into(),
            SEGMENT_SIZE_WORDS.into(),
            0,
        );

        let command_data = multi_chain!(flash_data.clone(), flash_data.clone()).collect::<Vec<_>>();
        assert_eq!(
            command_list.commands,
            [
                StreamCommand::from_data(&command_data, 0),
                StreamCommand::from_data(&command_data, 2048),
                StreamCommand::from_data(&command_data, 4096),
                StreamCommand::from_data(&command_data, 6144),
            ]
        );
    }

    #[fuchsia::test()]
    fn test_stream_command_generation_unaligned_prefix() {
        const SEGMENT_SIZE_WORDS: u32 = 1024;

        let flash_data = 0..SEGMENT_SIZE_WORDS;
        let data = multi_chain!(
            0..(SEGMENT_SIZE_WORDS / 2),
            flash_data.clone(),
            flash_data.clone(),
            flash_data.clone(),
            flash_data.clone(),
        )
        .collect::<Vec<_>>();

        let command_list = generate_command_list(
            data.as_slice(),
            1 << 28,
            SEGMENT_SIZE_WORDS.into(),
            (SEGMENT_SIZE_WORDS / 2).into(),
        );

        assert_eq!(command_list.commands, [StreamCommand::from_data(&data, 0)]);
    }

    #[fuchsia::test()]
    fn test_stream_command_generation_trailing_suffix() {
        const SEGMENT_SIZE_WORDS: u32 = 1024;

        let fill_zero = std::iter::repeat(0u32).take(SEGMENT_SIZE_WORDS as usize);
        let flash_data = 0..(SEGMENT_SIZE_WORDS / 2);
        let data = multi_chain!(
            fill_zero.clone(),
            fill_zero.clone(),
            fill_zero.clone(),
            fill_zero.clone(),
            flash_data.clone(),
        )
        .collect::<Vec<_>>();

        let command_list =
            generate_command_list(data.as_slice(), 1 << 28, SEGMENT_SIZE_WORDS.into(), 0);

        assert_eq!(
            command_list.commands,
            [
                StreamCommand::from_fill(0, 0, 4096),
                StreamCommand::from_data(&flash_data.collect::<Vec<_>>(), 4096)
            ]
        );
    }

    #[fuchsia::test()]
    fn test_stream_command_generation_tiny_image() {
        const SEGMENT_SIZE_WORDS: u32 = 1024;

        let data = vec![0u32; 8];
        let command_list =
            generate_command_list(data.as_slice(), 1 << 28, SEGMENT_SIZE_WORDS.into(), 0);

        assert_eq!(command_list.commands, [StreamCommand::from_fill(0, 0, 8)]);
    }
}
