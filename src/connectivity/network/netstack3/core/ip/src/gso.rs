// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Generic Segmentation Offload (GSO) support.

use core::fmt::Debug;
use core::marker::PhantomData;

use net_types::ip::{IpAddress, Ipv4, Ipv6};
use netstack3_base::{
    ChecksumRxOffloading, Counter, GsoInfo, Ipv4IdMode, MAX_GSO_PAYLOAD_LEN, NetworkParsingContext,
    NetworkSerializationContext, Payload,
};
use netstack3_filter::{DynTransportSerializer, FilterIpExt, ForwardedPacket, RawIpBody};
use packet::{
    BufferMut, EmptyBuf, FragmentedBytesMut, InnerPacketBuilder, InnerSerializer,
    NestablePacketBuilder, NestableSerializer as _, Nested, PacketBuilder, PacketConstraints,
    ParsablePacket as _, ParseBuffer, SerializationContext, SerializeTarget,
};
use packet_formats::icmp::{IcmpIpExt, IcmpMessage, IcmpPacketBuilder};
use packet_formats::ip::{FragmentOffset, IpExt, IpPacket as _, IpProto};
use packet_formats::ipv4::{
    HDR_PREFIX_LEN, Ipv4Header, Ipv4Packet, Ipv4PacketBuilder, Ipv4PacketBuilderWithOptions,
};
use packet_formats::ipv6::{Ipv6Packet, Ipv6PacketBuilder, Ipv6PacketBuilderWithHbhOptions};
use packet_formats::tcp::options::TcpOptionsRef;
use packet_formats::tcp::{TcpParseArgs, TcpSegment, TcpSegmentBuilderWithOptions};
use packet_formats::udp::UdpPacketBuilder;

use crate::internal::fragmentation::{
    AsFragmentableIpPacketBuilder, FragmentationError, FragmentationIpExt,
};

/// Reasons a packet can't be segmented in software.
#[derive(Debug, Eq, PartialEq)]
pub enum GsoError {
    /// The packet's headers couldn't be parsed.
    Parse,
    /// The packet doesn't carry a protocol that can be segmented in software.
    NotSegmentable,
    /// The packet carries headers that can't be faithfully replicated into
    /// each segment, e.g. IPv4 options or IPv6 extension headers.
    UnsupportedHeaders,
    /// The packet's payload is longer than [`MAX_GSO_PAYLOAD_LEN`].
    PayloadTooLong,
}

/// An IP version extension trait providing the version-specific parts of
/// software segmentation.
pub trait GsoIpExt:
    IpExt<PacketBuilder<NetworkSerializationContext>: AsSegmentableIpPacketBuilder<Self>>
    + FragmentationIpExt
{
    /// The IP header template for the segments of a forwarded packet.
    type ForwardedSegmentBuilder: SegmentableIpPacketBuilder<Self>;

    /// Returns a template IP header builder for the segments of `packet`.
    ///
    /// Returns an error if `packet` carries headers that can't be replicated
    /// into each segment.
    fn try_segmented_packet_builder<'a>(
        packet: &Self::Packet<&'a [u8]>,
    ) -> Result<Self::ForwardedSegmentBuilder, GsoError>;
}

impl GsoIpExt for Ipv4 {
    type ForwardedSegmentBuilder = Ipv4PacketBuilder;

    fn try_segmented_packet_builder<'a>(
        packet: &Ipv4Packet<&'a [u8]>,
    ) -> Result<Ipv4PacketBuilder, GsoError> {
        // Options would have to be copied into every segment. Packets only
        // reach this path with `GsoInfo` attached by GRO, which refuses to
        // coalesce packets carrying options, so this is a defensive check.
        if packet.header_len() != HDR_PREFIX_LEN {
            return Err(GsoError::UnsupportedHeaders);
        }
        // A fragment doesn't carry a complete transport payload, so it can't
        // be segmented.
        if packet.fragment_offset() != FragmentOffset::ZERO || packet.mf_flag() {
            return Err(GsoError::UnsupportedHeaders);
        }
        Ok(Ipv4Header::builder(packet))
    }
}

impl GsoIpExt for Ipv6 {
    type ForwardedSegmentBuilder = ForwardedIpv6SegmentBuilder;

    fn try_segmented_packet_builder<'a>(
        packet: &Ipv6Packet<&'a [u8]>,
    ) -> Result<ForwardedIpv6SegmentBuilder, GsoError> {
        // Extension headers would have to be copied into every segment. Packets
        // only reach this path with `GsoInfo` attached by GRO, which refuses to
        // coalesce packets carrying extension headers, so this is a defensive
        // check.
        if packet.iter_extension_hdrs().next().is_some() {
            return Err(GsoError::UnsupportedHeaders);
        }
        Ok(ForwardedIpv6SegmentBuilder(packet.builder()))
    }
}

/// The IP header of a segment of a forwarded IPv6 packet.
///
/// Per RFC 8200 Section 4.5, routers don't fragment IPv6 packets, so unlike a
/// bare [`Ipv6PacketBuilder`], segments built with this type refuse to be
/// fragmented. Segments that don't fit the egress MTU instead fail to send,
/// causing the forwarding path to generate an ICMPv6 Packet Too Big error.
#[derive(Clone, Debug)]
pub struct ForwardedIpv6SegmentBuilder(Ipv6PacketBuilder);

impl SegmentableIpPacketBuilder<Ipv6> for ForwardedIpv6SegmentBuilder {
    type SegmentBuilder = Self;

    fn segment_builder(&self, _index: u16, _gso_info: GsoInfo) -> Self {
        // No header fields need to be updated.
        self.clone()
    }
}

impl AsFragmentableIpPacketBuilder<Ipv6> for ForwardedIpv6SegmentBuilder {
    type Builder<'a>
        = <Ipv6PacketBuilder as AsFragmentableIpPacketBuilder<Ipv6>>::Builder<'a>
    where
        Self: 'a;

    fn try_as_fragmentable(&self) -> Result<Self::Builder<'_>, FragmentationError> {
        // Forwarded packets that were reassembled on ingress must be
        // re-fragmented to their original size on egress (see the
        // `FragmentableIpSerializer` impl for `ForwardedPacket`). That never
        // applies here: segments only come from packets coalesced by GRO, and
        // GRO doesn't coalesce fragmented packets (it rejects IPv6 packets with
        // any extension headers, including the Fragment header), so a segmented
        // packet can't have been reassembled.
        Err(FragmentationError::NotAllowed)
    }
}

impl NestablePacketBuilder for ForwardedIpv6SegmentBuilder {
    fn constraints(&self) -> PacketConstraints {
        let Self(builder) = self;
        builder.constraints()
    }
}

impl PacketBuilder<NetworkSerializationContext> for ForwardedIpv6SegmentBuilder {
    fn context_state(&self) -> <NetworkSerializationContext as SerializationContext>::ContextState {
        let Self(builder) = self;
        PacketBuilder::<NetworkSerializationContext>::context_state(builder)
    }

    fn serialize(
        &self,
        context: &mut NetworkSerializationContext,
        target: &mut SerializeTarget<'_>,
        body: FragmentedBytesMut<'_, '_>,
    ) {
        let Self(builder) = self;
        builder.serialize(context, target, body)
    }
}

/// A [`packet::Serializer`] the IP egress path may be able to split into
/// segments.
///
/// This is the transport-layer segmentation counterpart of
/// [`FragmentableIpSerializer`]: rather than serializing the packet and parsing
/// it back, implementations hand out the header builders and payload they
/// already hold, which a [`GsoSegmenter`] stamps onto each segment.
pub trait MaybeSegmentableIpSerializer<I: GsoIpExt> {
    /// Where packets of this type requiring segmentation come from.
    const SOURCE: GsoSource;

    /// The IP header template for every segment.
    type IpBuilder<'a>: SegmentableIpPacketBuilder<I>
    where
        Self: 'a;

    /// The transport header template for every segment.
    type TransportBuilder<'a>: SegmentableTransportBuilder
    where
        Self: 'a;

    /// The transport payload split across the segments.
    type Payload<'a>: Payload + InnerPacketBuilder + Copy
    where
        Self: 'a;

    /// Returns a segmenter splitting this packet into `gso_info`-sized
    /// segments.
    ///
    /// Returns an error if the packet can't be segmented in software.
    fn try_segmenter(
        &self,
        gso_info: GsoInfo,
    ) -> Result<
        GsoSegmenter<I, Self::IpBuilder<'_>, Self::TransportBuilder<'_>, Self::Payload<'_>>,
        GsoError,
    >;
}

impl<I, B> MaybeSegmentableIpSerializer<I> for ForwardedPacket<I, B>
where
    I: GsoIpExt,
    B: BufferMut,
{
    const SOURCE: GsoSource = GsoSource::Forwarded;

    type IpBuilder<'a>
        = I::ForwardedSegmentBuilder
    where
        Self: 'a;
    type TransportBuilder<'a>
        = ParsedForwardedTransport<'a, I::Addr>
    where
        Self: 'a;
    type Payload<'a>
        = &'a [u8]
    where
        Self: 'a;

    fn try_segmenter(
        &self,
        gso_info: GsoInfo,
    ) -> Result<GsoSegmenter<I, Self::IpBuilder<'_>, Self::TransportBuilder<'_>, &[u8]>, GsoError>
    {
        segmenter_from_packet_bytes(self.buffer().as_ref(), gso_info)
    }
}

impl<I, S, B> MaybeSegmentableIpSerializer<I> for Nested<S, B>
where
    I: GsoIpExt,
    S: MaybeSegmentableTransportSerializer,
    B: AsSegmentableIpPacketBuilder<I>,
{
    // Packets the stack hasn't serialized yet were generated locally.
    const SOURCE: GsoSource = GsoSource::Local;

    type IpBuilder<'a>
        = B::Builder<'a>
    where
        Self: 'a;
    type TransportBuilder<'a>
        = S::Builder<'a>
    where
        Self: 'a;
    type Payload<'a>
        = S::Payload<'a>
    where
        Self: 'a;

    fn try_segmenter(
        &self,
        gso_info: GsoInfo,
    ) -> Result<GsoSegmenter<I, B::Builder<'_>, S::Builder<'_>, S::Payload<'_>>, GsoError> {
        let ip_builder = self.outer().try_as_segmentable()?;
        let (transport_builder, payload) = self.inner().try_as_segmentable()?;
        GsoSegmenter::new(ip_builder, transport_builder, payload, gso_info)
    }
}

/// A type that may be viewed as the IP header template for a packet's
/// segments.
pub trait AsSegmentableIpPacketBuilder<I: GsoIpExt> {
    /// The segment header template borrowed from this type.
    type Builder<'a>: SegmentableIpPacketBuilder<I>
    where
        Self: 'a;

    /// Attempts to borrow a segment header template, returning an error if
    /// this builder's headers can't be replicated into every segment.
    fn try_as_segmentable(&self) -> Result<Self::Builder<'_>, GsoError>;
}

impl AsSegmentableIpPacketBuilder<Ipv4> for Ipv4PacketBuilder {
    type Builder<'a>
        = &'a Ipv4PacketBuilder
    where
        Self: 'a;

    fn try_as_segmentable(&self) -> Result<&Ipv4PacketBuilder, GsoError> {
        Ok(self)
    }
}

impl AsSegmentableIpPacketBuilder<Ipv6> for Ipv6PacketBuilder {
    type Builder<'a>
        = &'a Ipv6PacketBuilder
    where
        Self: 'a;

    fn try_as_segmentable(&self) -> Result<&Ipv6PacketBuilder, GsoError> {
        Ok(self)
    }
}

impl<O> AsSegmentableIpPacketBuilder<Ipv4> for Ipv4PacketBuilderWithOptions<'_, O> {
    type Builder<'a>
        = !
    where
        Self: 'a;

    fn try_as_segmentable(&self) -> Result<!, GsoError> {
        // GSO doesn't currently support IPv4 options.
        Err(GsoError::UnsupportedHeaders)
    }
}

impl<O> AsSegmentableIpPacketBuilder<Ipv6> for Ipv6PacketBuilderWithHbhOptions<'_, O> {
    type Builder<'a>
        = !
    where
        Self: 'a;

    fn try_as_segmentable(&self) -> Result<!, GsoError> {
        // GSO doesn't currently support IPv6 extension headers.
        Err(GsoError::UnsupportedHeaders)
    }
}

/// A template for the IP header of each of the segments a packet is split into.
///
/// This is IP-layer counterpart of [`SegmentableTransportBuilder`].
pub trait SegmentableIpPacketBuilder<I: GsoIpExt> {
    /// The builder for a single segment's IP header.
    type SegmentBuilder: PacketBuilder<NetworkSerializationContext>
        + NestablePacketBuilder
        + AsFragmentableIpPacketBuilder<I>;

    /// Returns the header builder for the Nth segment, where N is a zero-based
    /// ordinal given by `index`.
    fn segment_builder(&self, index: u16, gso_info: GsoInfo) -> Self::SegmentBuilder;
}

impl SegmentableIpPacketBuilder<Ipv4> for Ipv4PacketBuilder {
    type SegmentBuilder = Self;

    fn segment_builder(&self, index: u16, gso_info: GsoInfo) -> Self {
        let GsoInfo { ipv4_id_mode, gso_size: _, gso_segs: _ } = gso_info;
        let mut builder = self.clone();
        match ipv4_id_mode {
            Some(Ipv4IdMode::Fixed) => {}
            Some(Ipv4IdMode::Incrementing) | None => {
                builder.id(self.read_id().wrapping_add(index));
            }
        }
        builder
    }
}

impl SegmentableIpPacketBuilder<Ipv6> for Ipv6PacketBuilder {
    type SegmentBuilder = Self;

    fn segment_builder(&self, _index: u16, _gso_info: GsoInfo) -> Self {
        // No header fields need to be updated.
        self.clone()
    }
}

impl SegmentableIpPacketBuilder<Ipv4> for ! {
    type SegmentBuilder = Ipv4PacketBuilder;

    fn segment_builder(&self, _index: u16, _gso_info: GsoInfo) -> Ipv4PacketBuilder {
        match *self {}
    }
}

impl SegmentableIpPacketBuilder<Ipv6> for ! {
    type SegmentBuilder = Ipv6PacketBuilder;

    fn segment_builder(&self, _index: u16, _gso_info: GsoInfo) -> Ipv6PacketBuilder {
        match *self {}
    }
}

// Allow both owned builders and builder references to be used as segment header
// templates.
impl<I: GsoIpExt, T: SegmentableIpPacketBuilder<I>> SegmentableIpPacketBuilder<I> for &T {
    type SegmentBuilder = T::SegmentBuilder;

    fn segment_builder(&self, index: u16, gso_info: GsoInfo) -> T::SegmentBuilder {
        T::segment_builder(*self, index, gso_info)
    }
}

/// A transport-layer [`packet::Serializer`] that may carry a payload splittable
/// in software.
pub trait MaybeSegmentableTransportSerializer {
    /// The transport header stamped onto every segment.
    type Builder<'a>: SegmentableTransportBuilder
    where
        Self: 'a;

    /// The transport payload split across the segments.
    type Payload<'a>: Payload + InnerPacketBuilder + Copy
    where
        Self: 'a;

    /// Attempts to extract the transport header template and the payload to
    /// split, returning an error if this serializer can't be segmented.
    fn try_as_segmentable(&self) -> Result<(Self::Builder<'_>, Self::Payload<'_>), GsoError>;
}

impl<A, P, B, O> MaybeSegmentableTransportSerializer
    for Nested<InnerSerializer<P, B>, TcpSegmentBuilderWithOptions<A, O>>
where
    A: IpAddress,
    P: Payload + InnerPacketBuilder + Copy,
    O: InnerPacketBuilder + Clone,
{
    type Builder<'a>
        = &'a TcpSegmentBuilderWithOptions<A, O>
    where
        Self: 'a;
    type Payload<'a>
        = P
    where
        Self: 'a;

    fn try_as_segmentable(&self) -> Result<(&TcpSegmentBuilderWithOptions<A, O>, P), GsoError> {
        Ok((self.outer(), *self.inner().inner()))
    }
}

/// Implements [`MaybeSegmentableTransportSerializer`] for serializers that do
/// not support segmentation.
macro_rules! impl_not_segmentable {
    ([$($generics:tt)*] $ty:ty) => {
        impl<$($generics)*> MaybeSegmentableTransportSerializer for $ty {
            type Builder<'x> = ! where Self: 'x;
            type Payload<'x> = &'x [u8] where Self: 'x;

            fn try_as_segmentable(
                &self,
            ) -> Result<(!, &[u8]), GsoError> {
                Err(GsoError::NotSegmentable)
            }
        }
    };
}

// TODO(https://fxbug.dev/566314871): Support UDP GSO.
impl_not_segmentable!([S, A: IpAddress] Nested<S, UdpPacketBuilder<A>>);

// TODO(https://fxbug.dev/452980285): Require serializers behind
// `DynTransportSerializer` to be GSO-aware.
impl_not_segmentable!(['a, I: FilterIpExt] DynTransportSerializer<'a, I>);
impl_not_segmentable!([I: IpExt, B: ParseBuffer] RawIpBody<I, B>);
impl_not_segmentable!([S, I: IcmpIpExt, M: IcmpMessage<I>] Nested<S, IcmpPacketBuilder<I, M>>);
impl_not_segmentable!([S] Nested<S, ()>);
impl_not_segmentable!([P, B] InnerSerializer<P, B>);

/// A template for the transport header of each of the segments a packet is
/// split into.
///
/// This is the transport-layer counterpart of [`SegmentableIpPacketBuilder`].
/// Implemented both for owned builders and for references to them, so a
/// template the stack already holds can be used without copying it up front.
pub trait SegmentableTransportBuilder {
    /// The builder for a single segment's transport header.
    type SegmentBuilder: PacketBuilder<NetworkSerializationContext> + NestablePacketBuilder;

    /// Returns the header builder for the segment carrying the payload bytes
    /// starting at `offset`.
    ///
    /// `has_more` indicates that further segments follow this one.
    fn segment_builder(&self, offset: u16, has_more: bool) -> Self::SegmentBuilder;
}

impl<A: IpAddress, O: InnerPacketBuilder + Clone> SegmentableTransportBuilder
    for TcpSegmentBuilderWithOptions<A, O>
{
    type SegmentBuilder = Self;

    fn segment_builder(&self, offset: u16, has_more: bool) -> Self {
        let mut builder = self.clone();
        let prefix_builder = builder.prefix_builder_mut();
        let seq_num = prefix_builder.seq_num().wrapping_add(u32::from(offset));
        prefix_builder.set_seq_num(seq_num);
        if has_more {
            // PSH and FIN carry end-of-data semantics, so they belong only on
            // the segment carrying the last byte of the payload.
            prefix_builder.psh(false);
            prefix_builder.fin(false);
        }
        builder
    }
}

/// The transport header template recovered by parsing a [`ForwardedPacket`].
#[derive(Clone, Debug)]
pub enum ParsedForwardedTransport<'a, A: IpAddress> {
    /// A TCP segment header.
    Tcp(TcpSegmentBuilderWithOptions<A, TcpOptionsRef<&'a [u8]>>),
}

impl<A: IpAddress> SegmentableTransportBuilder for ParsedForwardedTransport<'_, A> {
    type SegmentBuilder = Self;

    fn segment_builder(&self, offset: u16, has_more: bool) -> Self {
        match self {
            Self::Tcp(builder) => Self::Tcp(builder.segment_builder(offset, has_more)),
        }
    }
}

impl<A: IpAddress> NestablePacketBuilder for ParsedForwardedTransport<'_, A> {
    fn constraints(&self) -> PacketConstraints {
        match self {
            Self::Tcp(builder) => builder.constraints(),
        }
    }
}

impl<A: IpAddress> PacketBuilder<NetworkSerializationContext> for ParsedForwardedTransport<'_, A> {
    fn context_state(&self) -> <NetworkSerializationContext as SerializationContext>::ContextState {
        match self {
            Self::Tcp(builder) => {
                PacketBuilder::<NetworkSerializationContext>::context_state(builder)
            }
        }
    }

    fn serialize(
        &self,
        context: &mut NetworkSerializationContext,
        target: &mut SerializeTarget<'_>,
        body: FragmentedBytesMut<'_, '_>,
    ) {
        match self {
            Self::Tcp(builder) => builder.serialize(context, target, body),
        }
    }
}

impl<T: SegmentableTransportBuilder> SegmentableTransportBuilder for &T {
    type SegmentBuilder = T::SegmentBuilder;

    fn segment_builder(&self, offset: u16, has_more: bool) -> T::SegmentBuilder {
        T::segment_builder(*self, offset, has_more)
    }
}

impl SegmentableTransportBuilder for ! {
    type SegmentBuilder = !;

    fn segment_builder(&self, _offset: u16, _has_more: bool) -> ! {
        match *self {}
    }
}

/// Splits a single oversized packet into discrete segments.
///
/// Created via [`MaybeSegmentableIpSerializer::try_segmenter`], which fails if
/// the packet doesn't require (or doesn't support) software segmentation.
///
/// The header templates may be owned or borrowed (see
/// [`SegmentableIpPacketBuilder`] and [`SegmentableTransportBuilder`]); either
/// way, they are only copied once per emitted segment.
pub struct GsoSegmenter<I: GsoIpExt, IB, B, P> {
    /// The template for each segment's IP header.
    ip_builder: IB,
    /// The template for each segment's transport header.
    transport_builder: B,
    /// The transport payload being split.
    payload: P,
    /// The length of `payload`.
    payload_len: u16,
    /// The number of payload bytes already emitted.
    consumed: u16,
    /// The index of the next segment to be emitted.
    index: u16,
    gso_info: GsoInfo,
    _marker: PhantomData<I>,
}

impl<I, IB, B, P> GsoSegmenter<I, IB, B, P>
where
    I: GsoIpExt,
    IB: SegmentableIpPacketBuilder<I>,
    B: SegmentableTransportBuilder,
    P: Payload + InnerPacketBuilder + Copy,
{
    fn new(
        ip_builder: IB,
        transport_builder: B,
        payload: P,
        gso_info: GsoInfo,
    ) -> Result<Self, GsoError> {
        if payload.len() > usize::from(MAX_GSO_PAYLOAD_LEN) {
            return Err(GsoError::PayloadTooLong);
        }
        let payload_len =
            u16::try_from(payload.len()).expect("payload length is at most MAX_GSO_PAYLOAD_LEN");
        Ok(Self {
            ip_builder,
            transport_builder,
            payload,
            payload_len,
            consumed: 0,
            index: 0,
            gso_info,
            _marker: PhantomData,
        })
    }
}

/// The serializer for a single segment produced by a [`GsoSegmenter`].
type SegmentSerializer<P, TB, IB> = Nested<Nested<InnerSerializer<P, EmptyBuf>, TB>, IB>;

impl<I, IB, B, P> Iterator for GsoSegmenter<I, IB, B, P>
where
    I: GsoIpExt,
    IB: SegmentableIpPacketBuilder<I>,
    B: SegmentableTransportBuilder,
    P: Payload + InnerPacketBuilder + Copy,
{
    /// The serializer for the next segment and whether more segments are
    /// pending.
    type Item = (SegmentSerializer<P, B::SegmentBuilder, IB::SegmentBuilder>, bool);

    fn next(&mut self) -> Option<Self::Item> {
        let Self {
            ip_builder,
            transport_builder,
            payload,
            payload_len,
            consumed,
            index,
            gso_info,
            _marker: _,
        } = self;
        let GsoInfo { gso_size: max_segment_body, gso_segs: _, ipv4_id_mode: _ } = gso_info;

        let remaining = *payload_len - *consumed;
        if remaining == 0 {
            return None;
        }
        let take = remaining.min(max_segment_body.get());
        let has_more = take < remaining;
        let segment_body = payload.slice(u32::from(*consumed)..u32::from(*consumed + take));

        let transport_builder = transport_builder.segment_builder(*consumed, has_more);
        let ip_builder = ip_builder.segment_builder(*index, *gso_info);

        *consumed += take;
        *index = index.checked_add(1).expect("number of segments cannot exceed u16::MAX");

        Some((
            segment_body.into_serializer().wrap_in(transport_builder).wrap_in(ip_builder),
            has_more,
        ))
    }
}

/// Creates a segmenter for the IP packet serialized in `packet_bytes`.
///
/// Recovers the builders by parsing the packet, which is only necessary when
/// the stack doesn't hold them anymore; see
/// [`MaybeSegmentableIpSerializer::try_segmenter`].
fn segmenter_from_packet_bytes<I: GsoIpExt>(
    packet_bytes: &[u8],
    gso_info: GsoInfo,
) -> Result<
    GsoSegmenter<I, I::ForwardedSegmentBuilder, ParsedForwardedTransport<'_, I::Addr>, &[u8]>,
    GsoError,
> {
    let mut buffer = packet_bytes;
    let packet = <I as IpExt>::Packet::parse(&mut buffer, ()).map_err(|_| GsoError::Parse)?;
    if packet.proto() != IpProto::Tcp.into() {
        return Err(GsoError::NotSegmentable);
    }
    let ip_builder = I::try_segmented_packet_builder(&packet)?;

    let (src_ip, dst_ip) = (packet.src_ip(), packet.dst_ip());
    // Re-slice the IP bytes from `packet_bytes` for TCP parsing because
    // `packet` is dropped at the end of the function.
    let ip_body =
        &packet_bytes[packet.parse_metadata().header_len()..][..packet.parse_metadata().body_len()];
    let mut tcp_bytes = ip_body;
    // GRO produced this packet after verifying the transport-layer checksums of
    // the source packets, so there's no need to verify the checksum of the
    // coalesced packet here.
    let mut parsing_context = NetworkParsingContext::new(ChecksumRxOffloading::FullyOffloaded);
    let tcp = TcpSegment::parse(
        &mut tcp_bytes,
        TcpParseArgs::with_context(src_ip, dst_ip, &mut parsing_context),
    )
    .map_err(|_| GsoError::Parse)?;
    // Just as above, we re-slice the payload bytes here so that we can drop
    // `tcp` at the end of the function.
    let payload = &ip_body[tcp.header_len()..];
    let tcp_builder = ParsedForwardedTransport::Tcp(tcp.into_builder(src_ip, dst_ip));

    GsoSegmenter::new(ip_builder, tcp_builder, payload, gso_info)
}

/// Where a packet requiring software segmentation came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GsoSource {
    /// The packet is being forwarded.
    ///
    /// Only GRO attaches GSO metadata to received packets, so forwarded packets
    /// requiring segmentation must have been coalesced by GRO.
    Forwarded,
    /// The packet was generated by the local stack as a large segment.
    Local,
}

/// Counters kept by the IP stack pertaining to software segmentation, kept
/// separately for each [`GsoSource`].
#[derive(Default, Debug)]
#[cfg_attr(
    any(test, feature = "testutils"),
    derive(PartialEq, netstack3_macros::CounterCollection)
)]
pub struct GsoCounters<C = Counter> {
    /// Counters for [`GsoSource::Forwarded`] frames.
    pub forwarded: GsoSourceCounters<C>,
    /// Counters for [`GsoSource::Local`] frames.
    pub local: GsoSourceCounters<C>,
}

impl GsoCounters {
    pub(crate) fn for_source(&self, source: GsoSource) -> &GsoSourceCounters {
        let Self { forwarded, local } = self;
        match source {
            GsoSource::Forwarded => forwarded,
            GsoSource::Local => local,
        }
    }
}

/// Software segmentation counters for frames from a single [`GsoSource`].
#[derive(Default, Debug)]
#[cfg_attr(
    any(test, feature = "testutils"),
    derive(PartialEq, netstack3_macros::CounterCollection)
)]
pub struct GsoSourceCounters<C = Counter> {
    /// The number of IP frames requiring segmentation on egress.
    pub segmentation_required: C,
    /// The total number of segments produced by segmentation.
    pub segments: C,
    /// The number of [`GsoError::Parse`] errors encountered.
    pub error_parse: C,
    /// The number of [`GsoError::NotSegmentable`] errors encountered.
    pub error_not_segmentable: C,
    /// The number of [`GsoError::UnsupportedHeaders`] errors encountered.
    pub error_unsupported_headers: C,
    /// The number of [`GsoError::PayloadTooLong`] errors encountered.
    pub error_payload_too_long: C,
}

impl GsoSourceCounters {
    pub(crate) fn error_counter(&self, error: &GsoError) -> &Counter {
        match error {
            GsoError::Parse => &self.error_parse,
            GsoError::NotSegmentable => &self.error_not_segmentable,
            GsoError::UnsupportedHeaders => &self.error_unsupported_headers,
            GsoError::PayloadTooLong => &self.error_payload_too_long,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloc::vec::Vec;
    use core::num::NonZeroU16;

    use ip_test_macro::ip_test;
    use net_types::Witness as _;
    use net_types::ip::IpVersion;
    use netstack3_base::testutil::{TEST_ADDRS_V4, TEST_ADDRS_V6, TestIpExt};
    use packet::{Buf, Serializer};
    use packet_formats::ip::{IpPacketBuilder as _, IpProto};
    use packet_formats::ipv4::options::Ipv4Option;
    use packet_formats::tcp::TcpSegmentBuilder;
    use packet_formats::tcp::options::{
        TcpOptions as _, TcpOptionsBuilder, TcpSackBlock, TimestampOption,
    };
    use test_case::{test_case, test_matrix};

    const SRC_PORT: NonZeroU16 = NonZeroU16::new(1234).unwrap();
    const DST_PORT: NonZeroU16 = NonZeroU16::new(80).unwrap();
    const SEQ_NUM: u32 = 100;
    const TTL: u8 = 64;
    /// The payload length in the split segments (aside from a potentially
    /// shorter last segment).
    const GSO_SIZE: NonZeroU16 = NonZeroU16::new(100).unwrap();
    /// The number of segments a [`PAYLOAD_LEN`]-long payload splits into.
    const NUM_SEGMENTS: usize = 3;
    /// The payload length of the final, partially filled segment of
    /// [`PAYLOAD_LEN`].
    const LAST_SEGMENT_LEN: usize = GSO_SIZE.get() as usize / 2;
    /// A payload length that splits into [`NUM_SEGMENTS`] segments, the last
    /// one partially filled.
    const PAYLOAD_LEN: usize = (NUM_SEGMENTS - 1) * GSO_SIZE.get() as usize + LAST_SEGMENT_LEN;
    /// The TCP Timestamp option carried by the packets under test.
    const TIMESTAMP: TimestampOption = TimestampOption::new(1, 2);
    /// The TCP SACK blocks carried by the packets under test.
    static SACK_BLOCKS: [TcpSackBlock; 2] = [TcpSackBlock::new(10, 20), TcpSackBlock::new(30, 40)];

    /// Where a [`GsoSegmenter`] under test gets its header templates from.
    #[derive(Clone, Copy, Debug)]
    enum TcpPacketSource {
        /// Parsed from the bytes of a serialized packet, as for packets
        /// coalesced by GRO.
        Bytes,
        /// Borrowed from the builders of a packet that hasn't been serialized,
        /// as for packets generated by the TCP socket layer.
        Serializer,
    }

    fn gso_info<I: net_types::ip::Ip>(id_mode: Ipv4IdMode) -> GsoInfo {
        let ipv4_id_mode = match I::VERSION {
            IpVersion::V4 => Some(id_mode),
            IpVersion::V6 => None,
        };
        GsoInfo {
            gso_size: GSO_SIZE,
            gso_segs: NonZeroU16::new(NUM_SEGMENTS as u16).unwrap(),
            ipv4_id_mode,
        }
    }

    fn test_payload(len: usize) -> Vec<u8> {
        // Cycle bytes until 251, the largest prime that fits in a u8, so that
        // misaligned segmentation is unlikely to go unnoticed.
        (0u8..251).cycle().take(len).collect()
    }

    /// Returns a serializer for a TCP packet carrying `payload`, shaped like
    /// the one the TCP socket layer hands to the IP layer.
    fn new_tcp_serializer<'a, I: TestIpExt + GsoIpExt>(
        payload: &'a [u8],
        fin: bool,
        psh: bool,
    ) -> impl MaybeSegmentableIpSerializer<I> + Serializer<NetworkSerializationContext> + 'a {
        let src_ip = I::TEST_ADDRS.local_ip.get();
        let dst_ip = I::TEST_ADDRS.remote_ip.get();
        let mut prefix_builder =
            TcpSegmentBuilder::new(src_ip, dst_ip, SRC_PORT, DST_PORT, SEQ_NUM, None, u16::MAX);
        prefix_builder.fin(fin);
        prefix_builder.psh(psh);
        // Only options that may appear on data segments are included: options
        // that are only valid during the handshake (e.g. MSS or Window Scale)
        // are never seen by GSO.
        let options = TcpOptionsBuilder {
            timestamp: Some(TIMESTAMP),
            sack_blocks: Some(&SACK_BLOCKS),
            ..Default::default()
        };
        payload
            .into_serializer()
            .wrap_in(
                TcpSegmentBuilderWithOptions::new(prefix_builder, options).expect("create builder"),
            )
            .wrap_in(I::PacketBuilder::new(src_ip, dst_ip, TTL, IpProto::Tcp.into()))
    }

    /// Serializes a TCP packet with `payload` for IP version `I`.
    fn new_tcp_packet<I: TestIpExt + GsoIpExt>(payload: &[u8], fin: bool, psh: bool) -> Vec<u8> {
        new_tcp_serializer::<I>(payload, fin, psh)
            .serialize_vec_outer_no_reuse(&mut NetworkSerializationContext::default())
            .expect("serialize packet")
            .into_inner()
    }

    /// Creates a TCP packet carrying `payload` and splits it into
    /// `gso_info`-sized segments, returning each segment serialized.
    fn create_and_segment_tcp_packet<I: TestIpExt + GsoIpExt>(
        source: TcpPacketSource,
        payload: &[u8],
        fin: bool,
        psh: bool,
        gso_info: GsoInfo,
    ) -> Vec<Buf<Vec<u8>>> {
        match source {
            TcpPacketSource::Bytes => {
                let packet = new_tcp_packet::<I>(payload, fin, psh);
                collect_segments(
                    segmenter_from_packet_bytes::<I>(&packet, gso_info).expect("create segmenter"),
                )
            }
            TcpPacketSource::Serializer => {
                let serializer = new_tcp_serializer::<I>(payload, fin, psh);
                collect_segments(serializer.try_segmenter(gso_info).expect("create segmenter"))
            }
        }
    }

    /// Collects all the segments produced by `segmenter`.
    fn collect_segments<I, IB, B, P>(mut segmenter: GsoSegmenter<I, IB, B, P>) -> Vec<Buf<Vec<u8>>>
    where
        I: GsoIpExt,
        IB: SegmentableIpPacketBuilder<I>,
        B: SegmentableTransportBuilder,
        P: Payload + InnerPacketBuilder + Copy,
    {
        let mut segments = Vec::new();
        let mut expect_more = true;
        while let Some((segment, has_more)) = segmenter.next() {
            // `has_more` must agree with the segmenter running out of segments.
            assert!(expect_more);
            expect_more = has_more;
            let segment = segment
                .serialize_vec_outer(&mut NetworkSerializationContext::default())
                .map_err(|(err, _serializer)| err)
                .expect("serialize segment")
                .unwrap_b();
            segments.push(segment);
        }
        assert!(!expect_more);
        segments
    }

    /// Parses `buf` and returns its TCP payload, asserting that the
    /// headers match what's expected of the segment at `index`.
    fn check_tcp_ip_packet<I: TestIpExt>(
        buf: &mut Buf<Vec<u8>>,
        index: u16,
        id_mode: Ipv4IdMode,
        fin: bool,
        psh: bool,
    ) -> Vec<u8> {
        let src_ip = I::TEST_ADDRS.local_ip.get();
        let dst_ip = I::TEST_ADDRS.remote_ip.get();
        {
            let packet = buf.parse::<<I as IpExt>::Packet<_>>().expect("parse IP packet");
            assert_eq!(packet.src_ip(), src_ip);
            assert_eq!(packet.dst_ip(), dst_ip);
            assert_eq!(packet.proto(), IpProto::Tcp.into());
            assert_eq!(packet.ttl(), TTL);
            I::map_ip_in(
                &packet,
                |packet| {
                    let expected_id = match id_mode {
                        Ipv4IdMode::Fixed => 0,
                        Ipv4IdMode::Incrementing => index,
                    };
                    assert_eq!(packet.id(), expected_id);
                },
                |_packet| {},
            );
        }

        let tcp = buf
            .parse_with::<_, TcpSegment<_>>(TcpParseArgs::new(src_ip, dst_ip))
            .expect("parse TCP segment");
        assert_eq!(tcp.src_port(), SRC_PORT);
        assert_eq!(tcp.dst_port(), DST_PORT);
        assert_eq!(tcp.fin(), fin);
        assert_eq!(tcp.psh(), psh);
        assert_eq!(tcp.options().timestamp(), Some(&TIMESTAMP));
        assert_eq!(tcp.options().sack_blocks(), Some(&SACK_BLOCKS[..]));
        tcp.body().to_vec()
    }

    #[ip_test(I)]
    #[test_matrix(
        [TcpPacketSource::Bytes, TcpPacketSource::Serializer],
        [Ipv4IdMode::Incrementing, Ipv4IdMode::Fixed],
        [LAST_SEGMENT_LEN, usize::from(GSO_SIZE.get())],
        [true, false],
        [true, false]
    )]
    fn segments_tcp_packet<I: TestIpExt + GsoIpExt>(
        source: TcpPacketSource,
        id_mode: Ipv4IdMode,
        last_segment_len: usize,
        fin: bool,
        psh: bool,
    ) {
        let payload =
            test_payload((NUM_SEGMENTS - 1) * usize::from(GSO_SIZE.get()) + last_segment_len);
        let mut segments =
            create_and_segment_tcp_packet::<I>(source, &payload, fin, psh, gso_info::<I>(id_mode));
        let num_segments = segments.len();
        assert_eq!(num_segments, NUM_SEGMENTS);

        let mut collected = Vec::new();
        for (index, segment) in segments.iter_mut().enumerate() {
            let last = index == num_segments - 1;
            // FIN and PSH are only kept on the last segment.
            let expect_fin = last && fin;
            let expect_psh = last && psh;
            let body = check_tcp_ip_packet::<I>(
                segment,
                u16::try_from(index).unwrap(),
                id_mode,
                expect_fin,
                expect_psh,
            );
            assert_eq!(
                body.len(),
                if last { last_segment_len } else { usize::from(GSO_SIZE.get()) }
            );
            collected.extend_from_slice(&body);
        }
        assert_eq!(collected, payload);
    }

    #[ip_test(I)]
    fn rejects_non_tcp_serializer<I: TestIpExt + GsoIpExt>() {
        let payload = test_payload(PAYLOAD_LEN);
        let src_ip = I::TEST_ADDRS.local_ip.get();
        let dst_ip = I::TEST_ADDRS.remote_ip.get();
        let serializer = Buf::new(payload, ..)
            .wrap_in(UdpPacketBuilder::new(src_ip, dst_ip, Some(SRC_PORT), DST_PORT))
            .wrap_in(I::PacketBuilder::new(src_ip, dst_ip, TTL, IpProto::Udp.into()));
        assert_eq!(
            serializer.try_segmenter(gso_info::<I>(Ipv4IdMode::Incrementing)).err(),
            Some(GsoError::NotSegmentable)
        );
    }

    #[test_case(TcpPacketSource::Bytes; "bytes")]
    #[test_case(TcpPacketSource::Serializer; "serializer")]
    fn segments_advance_sequence_numbers(source: TcpPacketSource) {
        let payload = test_payload(PAYLOAD_LEN);
        let fin = false;
        let psh = true;
        let seq_nums: Vec<u32> = create_and_segment_tcp_packet::<Ipv4>(
            source,
            &payload,
            fin,
            psh,
            gso_info::<Ipv4>(Ipv4IdMode::Incrementing),
        )
        .into_iter()
        .map(|mut segment| {
            let _ = segment.parse::<Ipv4Packet<_>>().expect("parse IPv4 packet");
            segment
                .parse_with::<_, TcpSegment<_>>(TcpParseArgs::new(
                    TEST_ADDRS_V4.local_ip.get(),
                    TEST_ADDRS_V4.remote_ip.get(),
                ))
                .expect("parse TCP segment")
                .seq_num()
        })
        .collect();
        let gso_size = u32::from(GSO_SIZE.get());
        let expected: Vec<u32> =
            (0..NUM_SEGMENTS).map(|i| SEQ_NUM + u32::try_from(i).unwrap() * gso_size).collect();
        assert_eq!(seq_nums, expected);
    }

    #[ip_test(I)]
    fn rejects_non_tcp<I: TestIpExt + GsoIpExt>() {
        let src_ip = I::TEST_ADDRS.local_ip.get();
        let dst_ip = I::TEST_ADDRS.remote_ip.get();
        let packet = Buf::new(test_payload(PAYLOAD_LEN), ..)
            .wrap_in(UdpPacketBuilder::new(src_ip, dst_ip, Some(SRC_PORT), DST_PORT))
            .wrap_in(I::PacketBuilder::new(src_ip, dst_ip, TTL, IpProto::Udp.into()))
            .serialize_vec_outer(&mut NetworkSerializationContext::default())
            .map_err(|(err, _serializer)| err)
            .expect("serialize packet")
            .unwrap_b()
            .into_inner();
        assert_eq!(
            segmenter_from_packet_bytes::<I>(&packet, gso_info::<I>(Ipv4IdMode::Incrementing))
                .err(),
            Some(GsoError::NotSegmentable)
        );
    }

    #[test]
    fn rejects_unparseable_packet() {
        let packet = [0u8; 8];
        assert_eq!(
            segmenter_from_packet_bytes::<Ipv6>(
                &packet[..],
                gso_info::<Ipv6>(Ipv4IdMode::Incrementing)
            )
            .err(),
            Some(GsoError::Parse)
        );
    }

    #[test]
    fn rejects_ipv4_packet_with_options() {
        let src_ip = TEST_ADDRS_V4.local_ip.get();
        let dst_ip = TEST_ADDRS_V4.remote_ip.get();
        let packet = Buf::new(test_payload(PAYLOAD_LEN), ..)
            .wrap_in(TcpSegmentBuilder::new(
                src_ip,
                dst_ip,
                SRC_PORT,
                DST_PORT,
                SEQ_NUM,
                None,
                u16::MAX,
            ))
            .wrap_in(
                Ipv4PacketBuilderWithOptions::new(
                    Ipv4PacketBuilder::new(src_ip, dst_ip, TTL, IpProto::Tcp.into()),
                    [Ipv4Option::RouterAlert { data: 0 }],
                )
                .expect("create builder"),
            )
            .serialize_vec_outer(&mut NetworkSerializationContext::default())
            .map_err(|(err, _serializer)| err)
            .expect("serialize packet")
            .unwrap_b()
            .into_inner();
        assert_eq!(
            segmenter_from_packet_bytes::<Ipv4>(
                &packet,
                gso_info::<Ipv4>(Ipv4IdMode::Incrementing)
            )
            .err(),
            Some(GsoError::UnsupportedHeaders)
        );
    }

    #[test]
    fn rejects_ipv6_packet_with_extension_headers() {
        use packet_formats::ipv6::Ipv6PacketBuilderWithHbhOptions;
        use packet_formats::ipv6::ext_hdrs::{
            ExtensionHeaderOptionAction, HopByHopOption, HopByHopOptionData,
        };

        let src_ip = TEST_ADDRS_V6.local_ip.get();
        let dst_ip = TEST_ADDRS_V6.remote_ip.get();
        let packet = Buf::new(test_payload(PAYLOAD_LEN), ..)
            .wrap_in(TcpSegmentBuilder::new(
                src_ip,
                dst_ip,
                SRC_PORT,
                DST_PORT,
                SEQ_NUM,
                None,
                u16::MAX,
            ))
            .wrap_in(
                Ipv6PacketBuilderWithHbhOptions::new(
                    Ipv6PacketBuilder::new(src_ip, dst_ip, TTL, IpProto::Tcp.into()),
                    &[HopByHopOption {
                        action: ExtensionHeaderOptionAction::SkipAndContinue,
                        mutable: false,
                        data: HopByHopOptionData::RouterAlert { data: 0 },
                    }],
                )
                .expect("create builder"),
            )
            .serialize_vec_outer(&mut NetworkSerializationContext::default())
            .expect("serialize packet")
            .unwrap_b()
            .into_inner();
        assert_eq!(
            segmenter_from_packet_bytes::<Ipv6>(
                &packet,
                gso_info::<Ipv6>(Ipv4IdMode::Incrementing)
            )
            .err(),
            Some(GsoError::UnsupportedHeaders)
        );
    }
}
