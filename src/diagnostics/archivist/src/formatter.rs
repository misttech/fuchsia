// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use crate::diagnostics::BatchIteratorConnectionStats;
use crate::error::AccessorError;
use crate::logs::servers::{ExtendRecordOpts, extend_fxt_record};
use crate::logs::shared_buffer::FxtMessage;
use fidl_fuchsia_diagnostics::{
    DataType, Format, FormattedContent, MAXIMUM_ENTRIES_PER_BATCH, StreamMode,
};

use futures::{Stream, StreamExt};
use log::warn;
use pin_project::pin_project;
use serde::Serialize;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};

pub type FormattedStream =
    Pin<Box<dyn Stream<Item = Vec<Result<FormattedContent, AccessorError>>> + Send>>;

#[pin_project]
pub struct FormattedContentBatcher<C> {
    #[pin]
    items: C,
    stats: Arc<BatchIteratorConnectionStats>,
}

/// Make a new `FormattedContentBatcher` with a chunking strategy depending on stream mode.
///
/// In snapshot mode, batched items will not be flushed to the client until the batch is complete
/// or the underlying stream has terminated.
///
/// In subscribe or snapshot-then-subscribe mode, batched items will be flushed whenever the
/// underlying stream is pending, ensuring clients always receive latest results.
pub fn new_batcher<I, T, E>(
    items: I,
    stats: Arc<BatchIteratorConnectionStats>,
    mode: StreamMode,
) -> FormattedStream
where
    I: Stream<Item = Result<T, E>> + Send + 'static,
    T: Into<FormattedContent> + Send,
    E: Into<AccessorError> + Send,
{
    match mode {
        StreamMode::Subscribe | StreamMode::SnapshotThenSubscribe => {
            Box::pin(FormattedContentBatcher {
                items: items.ready_chunks(MAXIMUM_ENTRIES_PER_BATCH as _),
                stats,
            })
        }
        StreamMode::Snapshot => Box::pin(FormattedContentBatcher {
            items: items.chunks(MAXIMUM_ENTRIES_PER_BATCH as _),
            stats,
        }),
    }
}

impl<I, T, E> Stream for FormattedContentBatcher<I>
where
    I: Stream<Item = Vec<Result<T, E>>>,
    T: Into<FormattedContent>,
    E: Into<AccessorError>,
{
    type Item = Vec<Result<FormattedContent, AccessorError>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        match this.items.poll_next(cx) {
            Poll::Ready(Some(chunk)) => {
                // loop over chunk instead of into_iter/map because we can't move `this`
                let mut batch = vec![];
                for item in chunk {
                    let result = match item {
                        Ok(i) => Ok(i.into()),
                        Err(e) => {
                            this.stats.add_result_error();
                            Err(e.into())
                        }
                    };
                    batch.push(result);
                }
                Poll::Ready(Some(batch))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Holds a VMO containing valid serialized data as well as the size of that data.
pub struct SerializedVmo {
    pub vmo: zx::Vmo,
    pub size: u64,
    format: Format,
}

impl SerializedVmo {
    pub fn serialize(
        source: &impl Serialize,
        data_type: DataType,
        format: Format,
    ) -> Result<Self, AccessorError> {
        let initial_buffer_capacity = match data_type {
            DataType::Inspect => inspect_format::constants::DEFAULT_VMO_SIZE_BYTES,
            // Logs won't go through this codepath anyway, but in case we ever want to serialize a
            // single log instance it makes sense to start at the page size.
            DataType::Logs => 4096, // page size
        };
        let mut buffer = Vec::with_capacity(initial_buffer_capacity);
        match format {
            Format::Json => {
                serde_json::to_writer(&mut buffer, source).map_err(AccessorError::Serialization)?
            }
            Format::Cbor => ciborium::into_writer(source, &mut buffer)
                .map_err(|err| AccessorError::CborSerialization(err.into()))?,
            Format::Text => unreachable!("We'll never get Text"),
            Format::Fxt => unreachable!("We'll never get FXT"),
        }
        let vmo = zx::Vmo::create(buffer.len() as u64).unwrap();
        vmo.write(&buffer, 0).unwrap();
        Ok(Self { vmo, size: buffer.len() as u64, format })
    }
}

impl From<SerializedVmo> for FormattedContent {
    fn from(content: SerializedVmo) -> FormattedContent {
        match content.format {
            Format::Json => {
                // set_content_size() is redundant, but consumers may expect the size there.
                content
                    .vmo
                    .set_content_size(&content.size)
                    .expect("set_content_size always returns Ok");
                FormattedContent::Json(fidl_fuchsia_mem::Buffer {
                    vmo: content.vmo,
                    size: content.size,
                })
            }
            Format::Cbor => {
                content
                    .vmo
                    .set_content_size(&content.size)
                    .expect("set_content_size always returns Ok");
                FormattedContent::Cbor(content.vmo)
            }
            Format::Fxt => {
                content
                    .vmo
                    .set_content_size(&content.size)
                    .expect("set_content_size always returns Ok");
                FormattedContent::Fxt(content.vmo)
            }
            Format::Text => unreachable!("We'll never get Text"),
        }
    }
}

trait PacketFormat {
    const FORMAT: Format;
    const HEADER: &[u8] = &[];
    const FOOTER: &[u8] = &[];

    /// Writes an item in the required format.  Returns `Poll::Ready(Some(<separator length>))` if
    /// an item was written, and `Poll::Ready(None)` if there are no more items.  If `first` is true,
    /// this is the first item in this batch.
    fn write_item(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        first: bool,
        buffer: &mut Vec<u8>,
    ) -> Poll<Option<usize>>;
}

#[pin_project]
pub struct PacketSerializer<T> {
    stats: Option<Arc<BatchIteratorConnectionStats>>,
    max_packet_size: u64,
    #[pin]
    format: T,
    overflow: Vec<u8>,
    finished: bool,
}

impl<T> PacketSerializer<T> {
    fn with_format(
        stats: Option<Arc<BatchIteratorConnectionStats>>,
        max_packet_size: u64,
        format: T,
    ) -> Self {
        Self { stats, max_packet_size, format, overflow: Vec::new(), finished: false }
    }
}

impl<T: PacketFormat> Stream for PacketSerializer<T> {
    type Item = Result<SerializedVmo, AccessorError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.finished {
            return Poll::Ready(None);
        }

        // Limit packet size to prevent unbounded memory use.
        const MAX_PACKET_SIZE_LIMIT: u64 = 1 << 20; // 1 MiB
        let max_packet_size = std::cmp::min(self.max_packet_size, MAX_PACKET_SIZE_LIMIT);
        let mut this = self.project();

        let mut buffer = Vec::with_capacity(256 * 1024);
        buffer.extend_from_slice(T::HEADER);

        let mut first = true;

        if !this.overflow.is_empty() {
            buffer.append(this.overflow);
            first = false;
            if let Some(stats) = &this.stats {
                stats.add_result();
            }
        }

        let mut vmo = None;
        let mut vmo_len = 0;

        loop {
            // Copy to the VMO if the room in the buffer drops below a threshold.
            if buffer.capacity() - buffer.len() < 512 {
                vmo.get_or_insert_with(|| zx::Vmo::create(max_packet_size).unwrap())
                    .write(&buffer, vmo_len as u64)
                    .unwrap();
                vmo_len += buffer.len();
                buffer.clear();
            }

            let last_len = buffer.len();

            let separator_len = match this.format.as_mut().write_item(cx, first, &mut buffer) {
                Poll::Ready(Some(separator_len)) => separator_len,
                Poll::Ready(None) => {
                    *this.finished = true;
                    if first {
                        return Poll::Ready(None);
                    } else {
                        break;
                    }
                }
                Poll::Pending => {
                    if first {
                        return Poll::Pending;
                    } else {
                        break;
                    }
                }
            };

            let item_len = buffer.len() - last_len - separator_len;

            if (item_len + T::HEADER.len() + T::FOOTER.len()) as u64 >= max_packet_size {
                warn!("dropping oversize item (limit={max_packet_size} len={item_len})");
                buffer.truncate(last_len);
            } else {
                if (vmo_len + buffer.len() + T::FOOTER.len()) as u64 > max_packet_size {
                    // Last item put us over the maximum packet size, keep it for the next batch.
                    // We should have at least one item because otherwise we should have gone
                    // through the branch above.
                    assert!(!first);
                    this.overflow.extend_from_slice(&buffer[last_len + separator_len..]);
                    buffer.truncate(last_len);
                    break;
                }

                first = false;

                if let Some(stats) = &this.stats {
                    stats.add_result();
                }
            }
        }

        buffer.extend_from_slice(T::FOOTER);

        let vmo = match vmo {
            Some(vmo) => {
                vmo.set_stream_size((vmo_len + buffer.len()) as u64).unwrap();
                vmo
            }
            None => zx::Vmo::create(buffer.len() as u64).unwrap(),
        };
        vmo.write(&buffer, vmo_len as u64).unwrap();
        vmo_len += buffer.len();
        Poll::Ready(Some(Ok(SerializedVmo { vmo, size: vmo_len as u64, format: T::FORMAT })))
    }
}

#[pin_project]
pub struct FxtPacketFormat<I>(#[pin] I);

impl<I: Stream<Item = FxtMessage>> PacketFormat for FxtPacketFormat<I> {
    const FORMAT: Format = Format::Fxt;

    fn write_item(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        _first: bool,
        buffer: &mut Vec<u8>,
    ) -> Poll<Option<usize>> {
        if let Some(item) = ready!(self.project().0.poll_next(cx)) {
            buffer.extend_from_slice(item.data());
            extend_fxt_record(
                item.component_identity(),
                item.dropped(),
                &ExtendRecordOpts {
                    component_url: true,
                    moniker: true,
                    rolled_out: true,
                    subscribe_to_manifest: false,
                },
                buffer,
            );
            Poll::Ready(Some(0))
        } else {
            Poll::Ready(None)
        }
    }
}

pub type FxtPacketSerializer<I> = PacketSerializer<FxtPacketFormat<I>>;

impl<I> FxtPacketSerializer<I> {
    pub fn new(stats: Arc<BatchIteratorConnectionStats>, max_packet_size: u64, items: I) -> Self {
        Self::with_format(Some(stats), max_packet_size, FxtPacketFormat(items))
    }
}

#[pin_project]
pub struct JsonPacketFormat<I>(#[pin] I);

impl<I: Stream<Item = impl Serialize>> PacketFormat for JsonPacketFormat<I> {
    const FORMAT: Format = Format::Json;
    const HEADER: &[u8] = b"[";
    const FOOTER: &[u8] = b"]";

    fn write_item(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        first: bool,
        buffer: &mut Vec<u8>,
    ) -> Poll<Option<usize>> {
        const SEPARATOR: &[u8] = b",\n";

        if let Some(item) = ready!(self.project().0.poll_next(cx)) {
            let separator_len = if !first {
                buffer.extend_from_slice(SEPARATOR);
                SEPARATOR.len()
            } else {
                0
            };
            // We don't expect serialization to fail because we should always be able to write to
            // `buffer` and `item` is a type we control which we know should always be serializable.
            serde_json::to_writer(buffer, &item).expect("failed to serialize item");
            Poll::Ready(Some(separator_len))
        } else {
            Poll::Ready(None)
        }
    }
}

pub type JsonPacketSerializer<I> = PacketSerializer<JsonPacketFormat<I>>;

impl<I> JsonPacketSerializer<I> {
    pub fn new(stats: Arc<BatchIteratorConnectionStats>, max_packet_size: u64, items: I) -> Self {
        Self::with_format(Some(stats), max_packet_size, JsonPacketFormat(items))
    }

    pub fn new_without_stats(max_packet_size: u64, items: I) -> Self {
        Self::with_format(None, max_packet_size, JsonPacketFormat(items))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::AccessorStats;
    use futures::stream::iter;

    #[fuchsia::test]
    async fn two_items_joined_and_split() {
        let inputs = &[&"FFFFFFFFFF", &"GGGGGGGGGG"];
        let joined = &["[\"FFFFFFFFFF\",\n\"GGGGGGGGGG\"]"];
        let split = &[r#"["FFFFFFFFFF"]"#, r#"["GGGGGGGGGG"]"#];
        let smallest_possible_joined_len = joined[0].len() as u64;

        let make_packets = |max| async move {
            let node = fuchsia_inspect::Node::default();
            let accessor_stats = Arc::new(AccessorStats::new(node));
            let test_stats = Arc::new(accessor_stats.new_logs_batch_iterator());
            JsonPacketSerializer::new(test_stats, max, iter(inputs.iter()))
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .map(|r| {
                    let result = r.unwrap();
                    let mut buf = vec![0; result.size as usize];
                    result.vmo.read(&mut buf, 0).expect("reading vmo");
                    std::str::from_utf8(&buf).unwrap().to_string()
                })
                .collect::<Vec<_>>()
        };

        let actual_joined = make_packets(smallest_possible_joined_len).await;
        assert_eq!(&actual_joined[..], joined);

        let actual_split = make_packets(smallest_possible_joined_len - 1).await;
        assert_eq!(&actual_split[..], split);
    }

    #[fuchsia::test]
    async fn overflow_separator_added() {
        let inputs = &[&"A", &"B", &"C"];
        // "[" + "A" + "]" = 4 bytes.
        // "[" + "A" + ",\n" + "B" + "]" = 4 + 2 + 3 = 9 bytes.
        // If max is 8, "B" will overflow.
        // Second packet starts with "B" from overflow.
        // "[" + "B" + ",\n" + "C" + "]" = 4 + 2 + 3 = 9 bytes.
        // If max is 8, "C" will overflow.
        // Third packet starts with "C" from overflow.

        let make_packets = |max| async move {
            let node = fuchsia_inspect::Node::default();
            let accessor_stats = Arc::new(AccessorStats::new(node));
            let test_stats = Arc::new(accessor_stats.new_logs_batch_iterator());
            JsonPacketSerializer::new(test_stats, max, iter(inputs.iter()))
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .map(|r| {
                    let result = r.unwrap();
                    let mut buf = vec![0; result.size as usize];
                    result.vmo.read(&mut buf, 0).expect("reading vmo");
                    std::str::from_utf8(&buf).unwrap().to_string()
                })
                .collect::<Vec<_>>()
        };

        let packets = make_packets(8).await;
        assert_eq!(packets.len(), 3);
        assert_eq!(packets[0], r#"["A"]"#);
        assert_eq!(packets[1], r#"["B"]"#);
        assert_eq!(packets[2], r#"["C"]"#);

        let packets = make_packets(10).await;
        assert_eq!(packets.len(), 2);
        assert_eq!(packets[0], "[\"A\",\n\"B\"]");
        assert_eq!(packets[1], r#"["C"]"#);
    }

    #[fuchsia::test]
    async fn oversize_item_not_dropped_incorrectly() {
        let inputs = &[&"A", &"BCDEF"];
        // Packet 1: ["A"] (4 bytes)
        // Item 2: "BCDEF" (7 bytes)
        // "[" + "A" + ",\n" + "BCDEF" + "]" = 1 + 3 + 2 + 7 + 1 = 14 bytes.
        // If max is 11:
        // "A" fits (4 bytes).
        // "BCDEF" overflows.
        // NEXT packet:
        // "[" + "BCDEF" + "]" = 9 bytes.
        // 9 fits in 11.

        let make_packets = |max| async move {
            let node = fuchsia_inspect::Node::default();
            let accessor_stats = Arc::new(AccessorStats::new(node));
            let test_stats = Arc::new(accessor_stats.new_logs_batch_iterator());
            JsonPacketSerializer::new(test_stats, max, iter(inputs.iter()))
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .map(|r| {
                    let result = r.unwrap();
                    let mut buf = vec![0; result.size as usize];
                    result.vmo.read(&mut buf, 0).expect("reading vmo");
                    std::str::from_utf8(&buf).unwrap().to_string()
                })
                .collect::<Vec<_>>()
        };

        let packets = make_packets(11).await;
        assert_eq!(packets.len(), 2);
        assert_eq!(packets[0], r#"["A"]"#);
        assert_eq!(packets[1], r#"["BCDEF"]"#);
    }

    #[fuchsia::test]
    async fn item_too_big_for_packet_is_dropped() {
        let inputs = &[&"ABCDE"]; // 7 bytes
        // "[" + 7 + "]" = 9 bytes.
        // If max is 8, it should be dropped.

        let make_packets = |max| async move {
            let node = fuchsia_inspect::Node::default();
            let accessor_stats = Arc::new(AccessorStats::new(node));
            let test_stats = Arc::new(accessor_stats.new_logs_batch_iterator());
            JsonPacketSerializer::new(test_stats, max, iter(inputs.iter()))
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .map(|r| {
                    let result = r.unwrap();
                    let mut buf = vec![0; result.size as usize];
                    result.vmo.read(&mut buf, 0).expect("reading vmo");
                    std::str::from_utf8(&buf).unwrap().to_string()
                })
                .collect::<Vec<_>>()
        };

        let packets = make_packets(8).await;
        // Item should be dropped, so we get no packets.
        assert_eq!(packets.len(), 0);
    }
}
