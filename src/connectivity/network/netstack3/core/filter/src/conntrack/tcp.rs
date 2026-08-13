// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! TCP state tracking.

use core::time::Duration;

use netstack3_base::{Control, SegmentHeader, SeqNum, UnscaledWindowSize, WindowScale, WindowSize};
use replace_with::replace_with_and;

use super::{
    ConnectionDirection, ConnectionUpdateAction, ConnectionUpdateError, EstablishmentLifecycle,
};

/// A struct that completely encapsulates tracking a bidirectional TCP
/// connection.
#[derive(Debug, Clone)]
pub(crate) struct Connection {
    /// The current state of the TCP connection.
    state: State,
}

impl Connection {
    pub fn new(segment: &SegmentHeader, payload_len: usize, self_connected: bool) -> Option<Self> {
        Some(Self {
            // TODO(https://fxbug.dev/355699182): Properly support self-connected
            // connections.
            state: if self_connected {
                State::Untracked
            } else {
                State::new(segment, payload_len)?
            },
        })
    }

    pub fn expiry_duration(&self, establishment_lifecycle: EstablishmentLifecycle) -> Duration {
        self.state.expiry_duration(establishment_lifecycle)
    }

    pub fn update(
        &mut self,
        segment: &SegmentHeader,
        payload_len: usize,
        dir: ConnectionDirection,
    ) -> Result<ConnectionUpdateAction, ConnectionUpdateError> {
        let valid =
            replace_with_and(&mut self.state, |state| state.update(segment, payload_len, dir));

        if !valid {
            return Err(ConnectionUpdateError::InvalidPacket);
        }

        match self.state {
            State::Closed => Ok(ConnectionUpdateAction::RemoveEntry),
            State::Untracked | State::SynSent(_) | State::Established(_) => {
                Ok(ConnectionUpdateAction::NoAction)
            }
        }
    }
}

/// States for a TCP connection as a whole.
///
/// These vaguely correspond to states from RFC 9293, but since they apply to
/// the whole connection, and we're just snooping in the middle, they can't line
/// up perfectly 1:1. They exist to encode assumptions about the state of the
/// connection that would otherwise be unwieldy to manage if they were all
/// stuffed into [`Peer`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    /// The connection has properties that break standard state tracking. This
    /// state does a good-enough job tracking the connection.
    ///
    /// This is a short-circuit state that can never be left.
    Untracked,

    /// The connection has been closed, either by the FIN handshake or valid
    /// RST.
    ///
    /// This is a short-circuit state that can never be left.
    Closed,

    /// The initial SYN for this connection has been sent. State contained
    /// within is everything that can be gleaned from the initial SYN packet
    /// sent in the original direction.
    ///
    /// Expected peer states:
    /// - Original: `SYN_SENT`
    /// - Reply: `SYN_RECEIVED` (upon receipt)
    SynSent(SynSent),

    /// The handshake has completed, save for the final ACK (i.e. the last `ACK`
    /// of `SYN`, `SYN/ACK`, `ACK`).
    ///
    /// The connection stays in this state until it is completely torn down
    /// by the closing handshake or a valid RST.
    Established(PeerPair<Peer>),
}

impl State {
    fn new(segment: &SegmentHeader, payload_len: usize) -> Option<Self> {
        // We explicitly don't want to track any connections that we haven't
        // seen from the beginning because:
        //
        // a) This shouldn't happen, since we run from boot
        // b) Window scale is only negotiated during the initial handshake.
        match segment.control {
            Some(Control::SYN) => {}
            None | Some(Control::FIN) | Some(Control::RST) => return None,
        }

        Some(Self::SynSent(SynSent {
            iss: segment.seq,
            logical_len: segment.len(payload_len),
            advertised_window_scale: segment.options.window_scale(),
            // This unwrap cannot fail because WindowSize::MAX is 2^30-1,
            // which is larger than the largest possible unscaled window
            // size (2^16).
            window_size: WindowSize::from_u32(u16::from(segment.wnd).into()).unwrap(),
            dir: ConnectionDirection::Original,
        }))
    }

    fn expiry_duration(&self, establishment_lifecycle: EstablishmentLifecycle) -> Duration {
        const MAXIMUM_SEGMENT_LIFETIME: Duration = Duration::from_secs(120);

        // These are all picked to optimize purging connections from the table
        // as soon as is reasonable. Unlike Linux, we are choosing to be more
        // conservative with our timeouts and setting the most aggressive one to
        // the standard MSL of 120 seconds.
        match self {
            State::Untracked => {
                match establishment_lifecycle {
                    // This is small because it's just meant to be the time for
                    // the initial handshake.
                    EstablishmentLifecycle::SeenOriginal | EstablishmentLifecycle::SeenReply => {
                        MAXIMUM_SEGMENT_LIFETIME
                    }
                    EstablishmentLifecycle::Established => Duration::from_secs(6 * 60 * 60),
                }
            }
            State::Closed => Duration::ZERO,
            State::SynSent(_) => MAXIMUM_SEGMENT_LIFETIME,
            State::Established(PeerPair { original, reply }) => {
                // If there is no data outstanding, make the timeout large, and
                // otherwise small so we can purge the connection quickly if one
                // of the endpoints disappears.
                //
                // We treat a connection that's ever had a valid FIN the same
                // way. This pessimizes things for half-closed connections, but
                // that's a very uncommon case.
                if original.unacked_data
                    || reply.unacked_data
                    || original.fin_state.sent()
                    || reply.fin_state.sent()
                {
                    MAXIMUM_SEGMENT_LIFETIME
                } else {
                    Duration::from_secs(5 * 60 * 60 * 24)
                }
            }
        }
    }

    /// Returns a new state that unconditionally replaces the previous one. The
    /// boolean represents whether the segment was valid or not.
    ///
    /// In the case where the segment was invalid, the returned state will be
    /// equivalent to the one that `update` was called on.
    fn update(
        self,
        segment: &SegmentHeader,
        payload_len: usize,
        dir: ConnectionDirection,
    ) -> (State, bool) {
        match self {
            State::Untracked => (State::Untracked, true),
            State::Closed => (State::Closed, true),
            State::SynSent(state) => update_for_syn_sent(state, segment, payload_len, dir),
            State::Established(peers) => {
                update_for_established(peers.into_update_peers(dir), segment, payload_len)
            }
        }
    }
}

/// Contains all of the information required for a single peer in an established
/// TCP connection.
///
/// Packets are valid with the following equations (taken from ["Real Stateful
/// TCP Packet Filtering in IP Filter"][paper] and updated to allow for segments
/// intersecting the valid ranges, rather than needing to be entirely contained
/// within them).
///
/// Definitions:
/// - s: The sequence number of the first octet of the segment
/// - n: The (virtual) length of the segment in octets
/// - a: The ACK number of the segment
///
/// I:   Data upper bound: s   <= receiver.max_wnd_seq
/// II:  Data lower bound: s+n >= sender.max_next_seq - receiver.max_wnd
/// III: ACK  upper bound: a   <= receiver.max_next_seq
/// IV:  ACK  lower bound: a   >= receiver.max_next_seq - MAXACKWINDOW
///
/// MAXACKWINDOW is defined in the paper to be 66000, which is larger than
/// the largest possible window (without scaling). We scale by
/// sender.window_scale to ensure this property remains true.
///
/// [paper]: https://www.usenix.org/legacy/events/sec01/invitedtalks/rooij.pdf
#[derive(Debug, Clone, PartialEq, Eq)]
struct Peer {
    /// How much to scale window updates from this peer.
    window_scale: WindowScale,

    /// The maximum window size ever sent by this peer.
    ///
    /// On every packet sent by this peer, the larger of itself and the
    /// (scaled) window from the packet is taken.
    max_wnd: WindowSize,

    /// The maximum sequence number that is in the window for this peer.
    ///
    /// On every packet sent by this peer, this is updated to be the larger of
    /// itself or the current advertised window (scaled) plus the ACK number in
    /// the packet.
    max_wnd_seq: SeqNum,

    /// The largest "next octet" ever sent by this peer. This is equivalent to
    /// one more than the largest sequence number ever sent by this peer.
    ///
    /// On every packet sent by this peer, this is updated to be the larger of
    /// itself or the sequence number plus the length of the packet.
    max_next_seq: SeqNum,

    /// Whether this peer has sent unACKed sequence numbers, accounting for SYN
    /// and FIN.
    ///
    /// Set when max_next_seq is increased. Unset when a reply segment is seen
    /// that has an ACK number equal to `max_next_seq` (larger would mean an
    /// invalid packet).
    unacked_data: bool,

    /// The state of the first FIN segment sent by this peer.
    fin_state: FinState,
}

impl Peer {
    /// Checks that an ACK segment is within the windows defined in the comment on [`Peer`].
    fn ack_segment_valid(peers: UpdatePeers<&Self>, seq: SeqNum, len: u32, ack: SeqNum) -> bool {
        let UpdatePeers { sender, receiver, dir: _ } = peers;

        // All checks below are for the negation of the equation referenced in
        // the associated comment.

        // I: Segment sequence numbers upper bound.
        if seq.after(receiver.max_wnd_seq) {
            return false;
        }

        // II: Segment sequence numbers lower bound.
        if (seq + len).before(sender.max_next_seq - receiver.max_wnd) {
            return false;
        }

        // III: ACK upper bound.
        if ack.after(receiver.max_next_seq) {
            return false;
        }

        // IV: ACK lower bound.
        if ack.before(receiver.max_next_seq - receiver.max_ack_window()) {
            return false;
        }

        true
    }

    /// Returns a new `Peer` updated using the provided information from a
    /// segment which it has sent.
    fn update_sender(
        self,
        seq: SeqNum,
        len: u32,
        ack: SeqNum,
        wnd: UnscaledWindowSize,
        control: Option<Control>,
    ) -> Self {
        let Self { window_scale, max_wnd, max_wnd_seq, max_next_seq, unacked_data, fin_state } =
            self;

        // The minimum window size is assumed to be 1. From the paper:
        //   On BSD systems, a window probing is always done with a packet
        //   containing one octet of data.
        let window_size = {
            // RFC 7323 2.2:
            //    The window field in a segment where the SYN bit is set (i.e., a <SYN>
            //    or <SYN,ACK>) MUST NOT be scaled.
            let window_scale =
                if control == Some(Control::SYN) { WindowScale::ZERO } else { window_scale };
            let window_size = wnd << window_scale;
            // The unwrap below won't fail because 1 is less than
            // WindowSize::MAX.
            core::cmp::max(window_size, WindowSize::from_u32(1).unwrap())
        };

        // The largest sequence number allowed by the current window.
        let wnd_seq = ack + window_size;
        // The octet one past the last one in the segment.
        let end = seq + len;
        // The largest `end` value sent by this peer.
        let sender_max_next_seq = if max_next_seq.before(end) { end } else { max_next_seq };

        Peer {
            window_scale,
            max_wnd: core::cmp::max(max_wnd, window_size),
            max_wnd_seq: if max_wnd_seq.before(wnd_seq) { wnd_seq } else { max_wnd_seq },
            max_next_seq: sender_max_next_seq,
            unacked_data: if sender_max_next_seq.after(max_next_seq) { true } else { unacked_data },
            fin_state: if control == Some(Control::FIN) {
                fin_state.update_fin_sent(end - 1)
            } else {
                fin_state
            },
        }
    }

    /// Returns a new `Peer` updated using the provided information from a
    /// segment which it received.
    fn update_receiver(self, ack: SeqNum) -> Self {
        let Self { window_scale, max_wnd, max_wnd_seq, max_next_seq, unacked_data, fin_state } =
            self;

        Peer {
            window_scale,
            max_wnd,
            max_wnd_seq,
            max_next_seq,
            // It's not possible for ack to be > self.max_next_seq due to
            // equation III, which is checked on every segment.
            unacked_data: if ack == max_next_seq { false } else { unacked_data },
            fin_state: fin_state.update_ack_received(ack),
        }
    }

    fn max_ack_window(&self) -> u32 {
        // The paper gives 66000 as the MAXACKWINDOW constant because it's a
        // little larger than the largest possible TCP window. With winow
        // scaling in effect, this constant is no longer valid, so we scale it
        // up by that scaling factor.
        //
        // This shift is guaranteed to never overflow because the maximum value
        // for self.window_scale is 14 and 66000 << 14 < u32::MAX.
        66000u32 << (self.window_scale.get() as u32)
    }
}

/// Tracks the state of the FIN process for a peer.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FinState {
    /// This peer has not sent a FIN yet.
    NotSent,

    /// This peer has sent a FIN with the provided sequence number.
    ///
    /// Updated to ensure it is the sequence number of the first FIN sent.
    Sent(SeqNum),

    /// The FIN sent by this peer has been ACKed.
    Acked,
}

impl FinState {
    /// To be called when the peer has sent a FIN segment with the sequence
    /// number of the FIN.
    ///
    /// Returns an updated `FinState`.
    fn update_fin_sent(self, seq: SeqNum) -> Self {
        match self {
            FinState::NotSent => FinState::Sent(seq),
            FinState::Sent(s) => {
                // NOTE: We want to track the first FIN in the sequence
                // space, not the first one we saw.
                if s.before(seq) { FinState::Sent(s) } else { FinState::Sent(seq) }
            }
            FinState::Acked => FinState::Acked,
        }
    }

    /// To be called when the peer has received an ACK.
    ///
    /// Returns an updated `FinState`.
    fn update_ack_received(self, ack: SeqNum) -> Self {
        match self {
            FinState::NotSent => FinState::NotSent,
            FinState::Sent(seq) => {
                if ack.after(seq) {
                    FinState::Acked
                } else {
                    FinState::Sent(seq)
                }
            }
            FinState::Acked => FinState::Acked,
        }
    }

    /// Has this FIN been acked?
    fn acked(&self) -> bool {
        match self {
            FinState::NotSent => false,
            FinState::Sent(_) => false,
            FinState::Acked => true,
        }
    }

    /// Has this FIN been sent?
    fn sent(&self) -> bool {
        match self {
            FinState::NotSent => false,
            FinState::Sent(_) => true,
            FinState::Acked => true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PeerPair<T> {
    original: T,
    reply: T,
}

impl<T> PeerPair<T> {
    fn into_update_peers(self, dir: ConnectionDirection) -> UpdatePeers<T> {
        let Self { original, reply } = self;
        match dir {
            ConnectionDirection::Original => UpdatePeers { sender: original, receiver: reply, dir },
            ConnectionDirection::Reply => UpdatePeers { sender: reply, receiver: original, dir },
        }
    }
}

/// Centralizes and names the peers for performing updates. Avoids ambiguity
/// about which peer is the sender/receiver.
struct UpdatePeers<T> {
    sender: T,
    receiver: T,
    dir: ConnectionDirection,
}

impl<T> UpdatePeers<T> {
    fn into_peer_pair(self) -> PeerPair<T> {
        let Self { sender, receiver, dir } = self;
        match dir {
            ConnectionDirection::Original => PeerPair { original: sender, reply: receiver },
            ConnectionDirection::Reply => PeerPair { original: receiver, reply: sender },
        }
    }

    fn as_ref(&self) -> UpdatePeers<&T> {
        let Self { sender, receiver, dir } = self;
        UpdatePeers { sender: &sender, receiver: &receiver, dir: *dir }
    }
}

/// State for [`State::SynSent`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct SynSent {
    /// The ISS (initial send sequence number).
    iss: SeqNum,

    /// The logical length of the segment.
    logical_len: u32,

    /// The window scale (if set in the initial SYN).
    advertised_window_scale: Option<WindowScale>,

    /// The advertised window size. Converted into a sequence number once we
    /// know what the other stack's ISN is.
    ///
    /// RFC 1323 2.2:
    ///   The Window field in a SYN (i.e., a <SYN> or <SYN,ACK>) segment itself
    ///   is never scaled.
    window_size: WindowSize,

    /// The stack the above state comes from.
    dir: ConnectionDirection,
}

/// State transitions for in-range segments by direction:
/// - Original
///   - SYN: Update data, stay in SynSent
///   - SYN/ACK: Invalid
///   - RST: Invalid
///   - FIN: Invalid
///   - ACK: Invalid
/// - Reply
///   - SYN: Untracked, until we support simultaneous open.
///   - SYN/ACK: Established
///   - RST: Delete connection
///   - FIN: Invalid
///   - ACK: Invalid
fn update_for_syn_sent(
    state: SynSent,
    segment: &SegmentHeader,
    payload_len: usize,
    dir: ConnectionDirection,
) -> (State, bool) {
    let SynSent { iss, logical_len, advertised_window_scale, window_size, dir: state_dir } = state;

    // This is another packet in the same direction as the first one.  Update
    // existing parameters for initial SYN, but only for packets that look to be
    // valid retransmits of the initial SYN. This behavior is copied over from
    // gVisor.
    if dir == state_dir {
        if let Some(_) = segment.ack {
            return (State::SynSent(state), false);
        }

        match segment.control {
            None | Some(Control::FIN) | Some(Control::RST) => {
                return (State::SynSent(state), false);
            }
            Some(Control::SYN) => {}
        };

        if segment.seq != state.iss || segment.options.window_scale() != advertised_window_scale {
            return (State::SynSent(state), false);
        }

        // If it's a valid retransmit of the original SYN, update
        // any state that changed and let it through.
        let seg_window_size = WindowSize::from_u32(u16::from(segment.wnd).into()).unwrap();

        (
            State::SynSent(SynSent {
                iss,
                logical_len: u32::max(segment.len(payload_len), logical_len),
                advertised_window_scale: advertised_window_scale,
                window_size: core::cmp::max(seg_window_size, window_size),
                dir: state_dir,
            }),
            true,
        )
    } else {
        // RFC 9293 3.10.7.3:
        //   If SND.UNA < SEG.ACK =< SND.NXT, then the ACK is
        //   acceptable.
        match segment.ack {
            None => {}
            Some(ack) => {
                if !(ack.after(iss) && ack.before(iss + logical_len + 1)) {
                    return (State::SynSent(state), false);
                }
            }
        };

        match segment.control {
            None | Some(Control::FIN) => (State::SynSent(state), false),
            // RFC 9293 3.10.7.3:
            //   If the RST bit is set,
            //   If the ACK was acceptable, then signal to the user
            //   "error: connection reset", drop the segment, enter
            //   CLOSED state, delete TCB, and return. Otherwise (no
            //   ACK), drop the segment and return.
            //
            // For our purposes, we delete the connection because we
            // know the receiver will tear down the connection.
            Some(Control::RST) => match segment.ack {
                None => (State::SynSent(state), false),
                // We previously validated the ACK, so this RST must be valid.
                Some(_) => (State::Closed, true),
            },

            Some(Control::SYN) => {
                let Some(ack) = segment.ack else {
                    // TODO(https://fxbug.dev/355200767): Support
                    // simultaneous open.
                    log::warn!("Unsupported TCP simultaneous open. Giving up on detailed tracking");

                    return (State::Untracked, true);
                };

                let sender_window_scale = segment.options.window_scale();
                let sender_window_size =
                    WindowSize::from_u32(u16::from(segment.wnd).into()).unwrap();

                // RFC 1323 2.2:
                //   This option is an offer, not a promise; both sides
                //   must send Window Scale options in their SYN
                //   segments to enable window scaling in either
                //   direction.
                let (receiver_window_scale, sender_window_scale) =
                    match (advertised_window_scale, sender_window_scale) {
                        (Some(receiver), Some(sender)) => (receiver, sender),
                        _ => (WindowScale::ZERO, WindowScale::ZERO),
                    };

                let receiver_max_next_seq = iss + logical_len;

                (
                    State::Established(
                        (UpdatePeers {
                            sender: Peer {
                                window_scale: sender_window_scale,
                                max_wnd: sender_window_size,
                                max_wnd_seq: ack + sender_window_size,
                                max_next_seq: segment.seq + segment.len(payload_len),
                                // The sender peer will always have unacked data here
                                // because the SYN we just saw sent needs to be acked.
                                unacked_data: true,
                                fin_state: FinState::NotSent,
                            },
                            receiver: Peer {
                                window_scale: receiver_window_scale,
                                max_wnd: window_size,
                                // We're still waiting on an ACK from the receiver
                                // stack, so this is slightly different from the normal
                                // calculation. It's still valid because we can assume
                                // that the implicit ACK number is the sender ISS (no
                                // data ACKed).
                                max_wnd_seq: segment.seq + window_size,
                                max_next_seq: receiver_max_next_seq,
                                unacked_data: ack.before(receiver_max_next_seq),
                                fin_state: FinState::NotSent,
                            },
                            dir,
                        })
                        .into_peer_pair(),
                    ),
                    true,
                )
            }
        }
    }
}

/// State transitions for in-range segments regardless of direction:
/// - SYN: Invalid
/// - RST: Delete connection
/// - FIN: Established (Closing)
/// - ACK: Established
///
/// This state deletes the connection once FINs from both peers have been ACKed.
fn update_for_established(
    mut peers: UpdatePeers<Peer>,
    segment: &SegmentHeader,
    payload_len: usize,
) -> (State, bool) {
    // NOTE: Segments after a FIN are somewhat invalid, but we do not
    // attempt to handle them specially. Per RFC 9293 3.10.7.4:
    //
    //   Seventh, process the segment text. [After FIN,] this should not
    //   occur since a FIN has been received from the remote side. Ignore
    //   the segment text.
    //
    // Because these segments aren't completely invalid, handling them
    // properly (and consistently with the endpoints) is difficult. It is
    // not needed for correctness, since the connection will be torn down as
    // soon as there's an ACK for both FINs anyway. This extra invalid data
    // does not change that.
    //
    // Neither Linux nor gVisor do anything special for these segments.

    let logical_len = segment.len(payload_len);
    let &SegmentHeader { seq, ack, wnd, control, options: _, push: _ } = segment;

    // From RFC 9293:
    //   If the ACK control bit is set, this field contains the value of the
    //   next sequence number the sender of the segment is expecting to receive.
    //   Once a connection is established, this is always sent.
    let ack = match ack {
        Some(ack) => ack,
        None => {
            return (State::Established(peers.into_peer_pair()), false);
        }
    };

    if !Peer::ack_segment_valid(peers.as_ref(), seq, logical_len, ack) {
        return (State::Established(peers.into_peer_pair()), false);
    }

    match control {
        Some(Control::SYN) => {
            return (State::Established(peers.into_peer_pair()), false);
        }
        // TODO(https://fxbug.dev/546018652): RSTs in synchronized states are
        // validated based on the sequence number only, not the ACK.
        Some(Control::RST) => return (State::Closed, true),
        Some(Control::FIN) | None => {}
    };

    peers.sender = peers.sender.update_sender(seq, logical_len, ack, wnd, control);
    peers.receiver = peers.receiver.update_receiver(ack);

    if peers.sender.fin_state.acked() && peers.receiver.fin_state.acked() {
        // Removing the entry immediately is not expected to break any
        // use-cases. The endpoints are ultimately responsible for
        // respecting the TIME_WAIT state.
        //
        // The NAT entry will be removed as a consequence, but this is only
        // a problem if a server wanted to reopen the connection with the
        // client (but only during TIME_WAIT).
        //
        // TODO(https://fxbug.dev/355200767): Add TimeWait and reopening
        // connections once simultaneous open is supported.
        (State::Closed, true)
    } else {
        (State::Established(peers.into_peer_pair()), true)
    }
}

#[cfg(test)]
mod tests {
    use super::{FinState, Peer, PeerPair, State, SynSent, UpdatePeers};

    use assert_matches::assert_matches;
    use netstack3_base::{
        Control, HandshakeOptions, SegmentHeader, SeqNum, UnscaledWindowSize, WindowScale,
        WindowSize,
    };
    use test_case::test_case;

    use crate::conntrack::ConnectionDirection;

    const ORIGINAL_ISS: SeqNum = SeqNum::new(0);
    const REPLY_ISS: SeqNum = SeqNum::new(8192);
    const ORIGINAL_WND: u16 = 16;
    const REPLY_WND: u16 = 17;
    const ORIGINAL_WS: u8 = 3;
    const REPLY_WS: u8 = 4;
    const ORIGINAL_PAYLOAD_LEN: usize = 12;
    const REPLY_PAYLOAD_LEN: usize = 13;

    impl Peer {
        pub fn arbitrary() -> Peer {
            Peer {
                max_next_seq: SeqNum::new(0),
                window_scale: WindowScale::new(0).unwrap(),
                max_wnd_seq: SeqNum::new(0),
                unacked_data: false,
                max_wnd: WindowSize::new(0).unwrap(),
                fin_state: FinState::NotSent,
            }
        }
    }

    #[test_case(None)]
    #[test_case(Some(Control::FIN))]
    #[test_case(Some(Control::RST))]
    fn syn_sent_original_non_syn_segment(control: Option<Control>) {
        let state = State::SynSent(SynSent {
            iss: ORIGINAL_ISS,
            logical_len: 3,
            advertised_window_scale: None,
            window_size: WindowSize::from_u32(ORIGINAL_WND as u32).unwrap(),
            dir: ConnectionDirection::Original,
        });

        let segment = SegmentHeader {
            seq: ORIGINAL_ISS,
            wnd: UnscaledWindowSize::from(ORIGINAL_WND),
            control,
            ..Default::default()
        };

        let expected_state = state.clone();
        assert_eq!(
            state.update(&segment, ORIGINAL_PAYLOAD_LEN, ConnectionDirection::Original),
            (expected_state, false)
        );
    }

    #[test_case(SegmentHeader {
        // Different from existing.
        seq: ORIGINAL_ISS + 1,
        wnd: UnscaledWindowSize::from(ORIGINAL_WND),
        control: Some(Control::SYN),
        options: HandshakeOptions {
            // Same as existing.
            window_scale: WindowScale::new(ORIGINAL_WS),
            ..Default::default()
        }.into(),
        ..Default::default()
    }; "different ISS")]
    #[test_case(SegmentHeader {
        // Same as existing.
        seq: ORIGINAL_ISS,
        wnd: UnscaledWindowSize::from(ORIGINAL_WND),
        control: Some(Control::SYN),
        options: HandshakeOptions {
            // Different from existing.
            window_scale: WindowScale::new(ORIGINAL_WS + 1),
            ..Default::default()
        }.into(),
        ..Default::default()
    }; "different window scale")]
    #[test_case(SegmentHeader {
        seq: ORIGINAL_ISS,
        // ACK here is invalid.
        ack: Some(SeqNum::new(10)),
        wnd: UnscaledWindowSize::from(ORIGINAL_WND),
        control: Some(Control::SYN),
        options: HandshakeOptions {
            window_scale: WindowScale::new(2),
            ..Default::default()
        }.into(),
        ..Default::default()
    }; "ack not allowed")]
    fn syn_sent_original_syn_not_retransmit(segment: SegmentHeader) {
        let state = State::SynSent(SynSent {
            iss: ORIGINAL_ISS,
            logical_len: ORIGINAL_PAYLOAD_LEN as u32 + 1,
            advertised_window_scale: WindowScale::new(ORIGINAL_WS),
            window_size: WindowSize::from_u32(ORIGINAL_WND as u32).unwrap(),
            dir: ConnectionDirection::Original,
        });

        let expected_state = state.clone();
        assert_eq!(
            state.update(&segment, ORIGINAL_PAYLOAD_LEN, ConnectionDirection::Original),
            (expected_state, false)
        );
    }

    #[test]
    fn syn_sent_original_syn_retransmit() {
        let state = State::SynSent(SynSent {
            iss: ORIGINAL_ISS,
            logical_len: ORIGINAL_PAYLOAD_LEN as u32 + 1,
            advertised_window_scale: WindowScale::new(ORIGINAL_WS),
            window_size: WindowSize::from_u32(ORIGINAL_WND as u32).unwrap(),
            dir: ConnectionDirection::Original,
        });

        let segment = SegmentHeader {
            seq: ORIGINAL_ISS,
            wnd: UnscaledWindowSize::from(ORIGINAL_WND + 10),
            control: Some(Control::SYN),
            options: HandshakeOptions {
                window_scale: WindowScale::new(ORIGINAL_WS),
                ..Default::default()
            }
            .into(),
            ..Default::default()
        };

        let result = assert_matches!(
            state.update(
                &segment,
                ORIGINAL_PAYLOAD_LEN + 10,
                ConnectionDirection::Original
            ),
            (State::SynSent(s), true) => s
        );

        assert_eq!(
            result,
            SynSent {
                iss: ORIGINAL_ISS,
                logical_len: ORIGINAL_PAYLOAD_LEN as u32 + 10 + 1,
                advertised_window_scale: WindowScale::new(ORIGINAL_WS),
                window_size: WindowSize::from_u32(ORIGINAL_WND as u32 + 10).unwrap(),
                dir: ConnectionDirection::Original
            }
        )
    }

    #[test_case(None)]
    #[test_case(Some(Control::FIN))]
    fn syn_sent_reply_non_syn_segment(control: Option<Control>) {
        let state = State::SynSent(SynSent {
            iss: ORIGINAL_ISS,
            logical_len: ORIGINAL_PAYLOAD_LEN as u32 + 1,
            advertised_window_scale: None,
            window_size: WindowSize::from_u32(ORIGINAL_WND as u32).unwrap(),
            dir: ConnectionDirection::Original,
        });

        let segment = SegmentHeader {
            seq: ORIGINAL_ISS,
            wnd: UnscaledWindowSize::from(ORIGINAL_WND),
            control,
            ..Default::default()
        };

        let expected_state = state.clone();
        assert_eq!(
            state.update(&segment, REPLY_PAYLOAD_LEN, ConnectionDirection::Reply),
            (expected_state, false)
        );
    }

    #[test_case(ORIGINAL_ISS, None; "small invalid")]
    #[test_case(
        ORIGINAL_ISS + 1, Some(State::Closed);
        "smallest valid"
    )]
    #[test_case(
        ORIGINAL_ISS + ORIGINAL_PAYLOAD_LEN as u32 + 1,
        Some(State::Closed);
        "largest valid"
    )]
    #[test_case(
        ORIGINAL_ISS + ORIGINAL_PAYLOAD_LEN as u32 + 2,
        None;
        "large invalid"
    )]
    fn syn_sent_reply_rst_segment(ack: SeqNum, new_state: Option<State>) {
        let state = State::SynSent(SynSent {
            iss: ORIGINAL_ISS,
            logical_len: ORIGINAL_PAYLOAD_LEN as u32 + 1,
            advertised_window_scale: None,
            window_size: WindowSize::from_u32(ORIGINAL_WND as u32).unwrap(),
            dir: ConnectionDirection::Original,
        });

        let segment = SegmentHeader {
            seq: ORIGINAL_ISS,
            ack: Some(ack),
            wnd: UnscaledWindowSize::from(ORIGINAL_WND),
            control: Some(Control::RST),
            ..Default::default()
        };

        let (expected_state, valid) = match new_state {
            Some(state) => (state, true),
            None => (state.clone(), false),
        };

        assert_eq!(
            state.update(&segment, /*payload_len*/ 0, ConnectionDirection::Reply),
            (expected_state, valid)
        );
    }

    #[test]
    fn syn_sent_reply_simultaneous_open() {
        let state = State::SynSent(SynSent {
            iss: ORIGINAL_ISS,
            logical_len: ORIGINAL_PAYLOAD_LEN as u32 + 1,
            advertised_window_scale: WindowScale::new(ORIGINAL_WS),
            window_size: WindowSize::from_u32(ORIGINAL_WND as u32).unwrap(),
            dir: ConnectionDirection::Original,
        });

        let segment = SegmentHeader {
            seq: ORIGINAL_ISS,
            wnd: UnscaledWindowSize::from(ORIGINAL_WND + 10),
            control: Some(Control::SYN),
            ..Default::default()
        };

        assert_eq!(
            state.update(&segment, /*payload_len*/ 0, ConnectionDirection::Reply),
            (State::Untracked, true)
        );
    }

    #[test_case(ORIGINAL_ISS; "too low")]
    #[test_case(ORIGINAL_ISS + ORIGINAL_PAYLOAD_LEN + 2; "too high")]
    fn syn_sent_reply_syn_ack_not_in_range(ack: SeqNum) {
        let state = State::SynSent(SynSent {
            iss: ORIGINAL_ISS,
            logical_len: ORIGINAL_PAYLOAD_LEN as u32 + 1,
            advertised_window_scale: None,
            window_size: WindowSize::from_u32(ORIGINAL_WND as u32).unwrap(),
            dir: ConnectionDirection::Original,
        });

        let segment = SegmentHeader {
            seq: REPLY_ISS,
            ack: Some(ack),
            wnd: UnscaledWindowSize::from(REPLY_WND),
            control: Some(Control::SYN),
            ..Default::default()
        };

        let expected_state = state.clone();
        assert_eq!(
            state.update(&segment, REPLY_PAYLOAD_LEN, ConnectionDirection::Reply),
            (expected_state, false)
        );
    }

    #[test_case(None)]
    #[test_case(Some(WindowScale::new(REPLY_WS).unwrap()))]
    fn syn_sent_reply_syn_ack(reply_window_scale: Option<WindowScale>) {
        let state = State::SynSent(SynSent {
            iss: ORIGINAL_ISS,
            logical_len: ORIGINAL_PAYLOAD_LEN as u32 + 1,
            advertised_window_scale: WindowScale::new(ORIGINAL_WS),
            window_size: WindowSize::from_u32(ORIGINAL_WND as u32).unwrap(),
            dir: ConnectionDirection::Original,
        });

        let segment = SegmentHeader {
            seq: REPLY_ISS,
            ack: Some(ORIGINAL_ISS + 1),
            wnd: UnscaledWindowSize::from(REPLY_WND),
            control: Some(Control::SYN),
            options: HandshakeOptions { window_scale: reply_window_scale, ..Default::default() }
                .into(),
            ..Default::default()
        };

        let new_state = assert_matches!(
            state.update(
                &segment,
                REPLY_PAYLOAD_LEN,
                ConnectionDirection::Reply
            ),
            (State::Established(s), true) => s
        );

        let (original_window_scale, reply_window_scale) = match reply_window_scale {
            Some(s) => (WindowScale::new(ORIGINAL_WS).unwrap(), s),
            None => (WindowScale::ZERO, WindowScale::ZERO),
        };

        assert_eq!(
            new_state,
            PeerPair {
                original: Peer {
                    window_scale: original_window_scale,
                    max_wnd: WindowSize::from_u32(ORIGINAL_WND as u32).unwrap(),
                    max_wnd_seq: REPLY_ISS + ORIGINAL_WND as u32,
                    max_next_seq: ORIGINAL_ISS + ORIGINAL_PAYLOAD_LEN + 1,
                    unacked_data: true,
                    fin_state: FinState::NotSent,
                },
                reply: Peer {
                    window_scale: reply_window_scale,
                    max_wnd: WindowSize::from_u32(REPLY_WND as u32).unwrap(),
                    max_wnd_seq: ORIGINAL_ISS + 1 + REPLY_WND as u32,
                    max_next_seq: REPLY_ISS + REPLY_PAYLOAD_LEN + 1,
                    unacked_data: true,
                    fin_state: FinState::NotSent,
                }
            }
        );
    }

    #[test_case(FinState::NotSent, SeqNum::new(9) => FinState::Sent(SeqNum::new(9)))]
    #[test_case(FinState::Sent(SeqNum::new(9)), SeqNum::new(8) => FinState::Sent(SeqNum::new(8)))]
    #[test_case(FinState::Sent(SeqNum::new(9)), SeqNum::new(9) => FinState::Sent(SeqNum::new(9)))]
    #[test_case(FinState::Sent(SeqNum::new(9)), SeqNum::new(10) => FinState::Sent(SeqNum::new(9)))]
    #[test_case(FinState::Acked, SeqNum::new(9) => FinState::Acked)]
    fn fin_state_update_fin_sent(fin_state: FinState, seq: SeqNum) -> FinState {
        fin_state.update_fin_sent(seq)
    }

    #[test_case(FinState::NotSent, SeqNum::new(10) => FinState::NotSent)]
    #[test_case(FinState::Sent(SeqNum::new(9)), SeqNum::new(9) => FinState::Sent(SeqNum::new(9)))]
    #[test_case(FinState::Sent(SeqNum::new(9)), SeqNum::new(10) => FinState::Acked)]
    #[test_case(FinState::Acked, SeqNum::new(10) => FinState::Acked)]
    fn fin_state_update_ack_received(fin_state: FinState, ack: SeqNum) -> FinState {
        fin_state.update_ack_received(ack)
    }

    const RECV_MAX_NEXT_SEQ: SeqNum = SeqNum::new(66_001);
    const RECV_MAX_WND_SEQ: SeqNum = SeqNum::new(1424);

    #[test_case(SeqNum::new(424), 200, SeqNum::new(1) => true; "success low seq/ack")]
    #[test_case(RECV_MAX_WND_SEQ, 0, RECV_MAX_NEXT_SEQ => true; "success high seq/ack")]
    #[test_case(RECV_MAX_WND_SEQ + 1, 0, SeqNum::new(1) => false; "bad equation I")]
    #[test_case(SeqNum::new(424), 199, SeqNum::new(1) => false; "bad equation II")]
    #[test_case(SeqNum::new(424), 200, RECV_MAX_NEXT_SEQ + 1 => false; "bad equation III")]
    #[test_case(SeqNum::new(424), 200, SeqNum::new(0) => false; "bad equation IV")]
    fn ack_segment_valid_test(seq: SeqNum, len: u32, ack: SeqNum) -> bool {
        let peers = UpdatePeers {
            sender: Peer { max_next_seq: SeqNum::new(1024), ..Peer::arbitrary() },

            // MAXACKWINDOW is going to be 66000 due to window shift of 0.
            receiver: Peer {
                window_scale: WindowScale::new(0).unwrap(),
                max_wnd: WindowSize::new(400).unwrap(),
                max_next_seq: RECV_MAX_NEXT_SEQ,
                max_wnd_seq: RECV_MAX_WND_SEQ,
                ..Peer::arbitrary()
            },
            // The direction doesn't matter.
            dir: ConnectionDirection::Original,
        };

        Peer::ack_segment_valid(peers.as_ref(), seq, len, ack)
    }

    struct PeerUpdateSenderArgs {
        seq: SeqNum,
        len: u32,
        ack: SeqNum,
        wnd: UnscaledWindowSize,
        control: Option<Control>,
    }

    #[test_case(
        Peer {
            window_scale: WindowScale::new(3).unwrap(),
            max_wnd: WindowSize::new(16).unwrap(),
            max_wnd_seq: SeqNum::new(127),
            max_next_seq: SeqNum::new(1024),
            unacked_data: false,
            fin_state: FinState::NotSent,
        },
        PeerUpdateSenderArgs {
            seq: SeqNum::new(1025),
            len: 10,
            ack: SeqNum::new(100),
            wnd: UnscaledWindowSize::from_u32(4),
            control: None,
        } => Peer {
            window_scale: WindowScale::new(3).unwrap(),
            max_wnd: WindowSize::new(32).unwrap(),
            max_wnd_seq: SeqNum::new(132),
            max_next_seq: SeqNum::new(1035),
            unacked_data: true,
            fin_state: FinState::NotSent,
        }; "packet larger"
    )]
    #[test_case(
        Peer {
            window_scale: WindowScale::new(3).unwrap(),
            max_wnd: WindowSize::new(16).unwrap(),
            max_wnd_seq: SeqNum::new(127),
            max_next_seq: SeqNum::new(1024),
            unacked_data: false,
            fin_state: FinState::NotSent,
        },
        PeerUpdateSenderArgs {
            seq: SeqNum::new(1000),
            len: 10,
            ack: SeqNum::new(0),
            wnd: UnscaledWindowSize::from_u32(0),
            control: None,
        } => Peer {
            window_scale: WindowScale::new(3).unwrap(),
            max_wnd: WindowSize::new(16).unwrap(),
            max_wnd_seq: SeqNum::new(127),
            max_next_seq: SeqNum::new(1024),
            unacked_data: false,
            fin_state: FinState::NotSent,
        }; "packet smaller"
    )]
    #[test_case(
        Peer {
            window_scale: WindowScale::new(3).unwrap(),
            max_wnd: WindowSize::new(16).unwrap(),
            max_wnd_seq: SeqNum::new(127),
            max_next_seq: SeqNum::new(1024),
            unacked_data: false,
            fin_state: FinState::NotSent,
        },
        PeerUpdateSenderArgs {
            seq: SeqNum::new(1000),
            len: 10,
            ack: SeqNum::new(0),
            wnd: UnscaledWindowSize::from_u32(0),
            control: Some(Control::FIN),
        } => Peer {
            window_scale: WindowScale::new(3).unwrap(),
            max_wnd: WindowSize::new(16).unwrap(),
            max_wnd_seq: SeqNum::new(127),
            max_next_seq: SeqNum::new(1024),
            unacked_data: false,
            fin_state: FinState::Sent(SeqNum::new(1000 + 9)),
        }; "fin sent"
    )]
    #[test_case(
        Peer {
            window_scale: WindowScale::new(3).unwrap(),
            max_wnd: WindowSize::new(16).unwrap(),
            max_wnd_seq: SeqNum::new(127),
            max_next_seq: SeqNum::new(1024),
            unacked_data: false,
            fin_state: FinState::NotSent,
        },
        PeerUpdateSenderArgs {
            seq: SeqNum::new(1000),
            len: 10,
            ack: SeqNum::new(0),
            wnd: UnscaledWindowSize::from_u32(4),
            control: Some(Control::SYN),
        } => Peer {
            window_scale: WindowScale::new(3).unwrap(),
            max_wnd: WindowSize::new(16).unwrap(),
            max_wnd_seq: SeqNum::new(127),
            max_next_seq: SeqNum::new(1024),
            unacked_data: false,
            fin_state: FinState::NotSent,
        }; "syn ack ignores window_scale"
    )]
    fn peer_update_sender_test(peer: Peer, args: PeerUpdateSenderArgs) -> Peer {
        peer.update_sender(args.seq, args.len, args.ack, args.wnd, args.control)
    }

    #[test_case(
        Peer { max_next_seq: SeqNum::new(1024), ..Peer::arbitrary() },
        SeqNum::new(1024) => Peer { max_next_seq: SeqNum::new(1024), ..Peer::arbitrary() };
        "unset unacked data"
    )]
    #[test_case(
        Peer { max_next_seq: SeqNum::new(1024), ..Peer::arbitrary() },
        SeqNum::new(1023) => Peer { max_next_seq: SeqNum::new(1024), ..Peer::arbitrary() };
        "don't unset unacked data"
    )]
    #[test_case(
        Peer { fin_state: FinState::Sent(SeqNum::new(9)), ..Peer::arbitrary() },
        SeqNum::new(10) => Peer { fin_state: FinState::Acked, ..Peer::arbitrary() };
        "update fin state"
    )]
    fn peer_update_receiver_test(peer: Peer, ack: SeqNum) -> Peer {
        peer.update_receiver(ack)
    }

    // This is mostly to ensure that we don't accidentally overflow a u32, since
    // it's such a basic calculation.
    #[test]
    fn peer_max_ack_window() {
        let max_peer = Peer { window_scale: WindowScale::MAX, ..Peer::arbitrary() };
        let min_peer = Peer { window_scale: WindowScale::new(0).unwrap(), ..Peer::arbitrary() };

        assert_eq!(max_peer.max_ack_window(), 1_081_344_000u32);
        assert_eq!(min_peer.max_ack_window(), 66_000u32);
    }

    struct StateUpdateTestArgs {
        segment: SegmentHeader,
        payload_len: usize,
        dir: ConnectionDirection,
        expected: Option<State>,
    }

    #[test_case(
        StateUpdateTestArgs {
            segment: SegmentHeader {
                seq: SeqNum::new(1400),
                ack: Some(SeqNum::new(66_001)),
                wnd: UnscaledWindowSize::from(10),
                ..Default::default()
            },
            payload_len: 24,
            dir: ConnectionDirection::Original,
            expected: Some(State::Established(PeerPair {
                original: Peer {
                    window_scale: WindowScale::new(2).unwrap(),
                    max_wnd: WindowSize::new(40).unwrap(),
                    max_wnd_seq: SeqNum::new(70_000),
                    max_next_seq: SeqNum::new(1424),
                    // This is becoming true because `segment.seq > original.max_next_seq`.
                    unacked_data: true,
                    fin_state: FinState::NotSent,
                },
                reply: Peer {
                    window_scale: WindowScale::new(0).unwrap(),
                    max_wnd: WindowSize::new(400).unwrap(),
                    max_wnd_seq: SeqNum::new(1424),
                    max_next_seq: SeqNum::new(66_001),
                    // This is becoming true because `segment.ack == reply.max_next_seq`.
                    unacked_data: false,
                    fin_state: FinState::NotSent,
                },
            })),
        }; "update original"
    )]
    #[test_case(
        StateUpdateTestArgs {
            segment: SegmentHeader {
                seq: SeqNum::new(66_100),
                ack: Some(SeqNum::new(1024)),
                wnd: UnscaledWindowSize::from(10),
                control: Some(Control::FIN),
                ..Default::default()
            },
            payload_len: 0,
            dir: ConnectionDirection::Reply,
            expected: Some(State::Established(PeerPair {
                original: Peer {
                    window_scale: WindowScale::new(2).unwrap(),
                    max_wnd: WindowSize::new(0).unwrap(),
                    max_wnd_seq: SeqNum::new(70_000),
                    max_next_seq: SeqNum::new(1024),
                    unacked_data: false,
                    fin_state: FinState::NotSent,
                },
                reply: Peer {
                    window_scale: WindowScale::new(0).unwrap(),
                    max_wnd: WindowSize::new(400).unwrap(),
                    max_wnd_seq: SeqNum::new(1424),
                    max_next_seq: SeqNum::new(66_101),
                    unacked_data: true,
                    fin_state: FinState::Sent(SeqNum::new(66_100)),
                },
            })),
        }; "closing"
    )]
    #[test_case(
        StateUpdateTestArgs {
            segment: SegmentHeader {
                seq: SeqNum::new(1400),
                wnd: UnscaledWindowSize::from(10),
                ..Default::default()
            },
            payload_len: 24,
            dir: ConnectionDirection::Original,
            expected: None,
        }; "missing ack"
    )]
    #[test_case(
        StateUpdateTestArgs {
            segment: SegmentHeader {
                // Too low. Doesn't meet equation II.
                seq: SeqNum::new(0),
                ack: Some(SeqNum::new(66_001)),
                wnd: UnscaledWindowSize::from(10),
                ..Default::default()
            },
            payload_len: 24,
            dir: ConnectionDirection::Original,
            expected: None,
        }; "invalid equation bounds"
    )]
    #[test_case(
        StateUpdateTestArgs {
            segment: SegmentHeader {
                seq: SeqNum::new(1400),
                ack: Some(SeqNum::new(66_001)),
                wnd: UnscaledWindowSize::from(10),
                control: Some(Control::SYN),
                ..Default::default()
            },
            payload_len: 24,
            dir: ConnectionDirection::Original,
            expected: None,
        }; "SYN not allowed"
    )]
    #[test_case(
        StateUpdateTestArgs {
            segment: SegmentHeader {
                seq: SeqNum::new(1400),
                // Fails equation III.
                ack: Some(SeqNum::new(100_000)),
                wnd: UnscaledWindowSize::from(10),
                ..Default::default()
            },
            payload_len: 24,
            dir: ConnectionDirection::Original,
            expected: None,
        }; "invalid ack"
    )]
    #[test_case(
        StateUpdateTestArgs {
            segment: SegmentHeader {
                seq: SeqNum::new(1400),
                ack: Some(SeqNum::new(66_001)),
                wnd: UnscaledWindowSize::from(10),
                control: Some(Control::RST),
                ..Default::default()
            },
            payload_len: 24,
            dir: ConnectionDirection::Original,
            expected: Some(State::Closed),
        }; "rst"
    )]
    fn established_test(args: StateUpdateTestArgs) {
        let state = State::Established(PeerPair {
            original: Peer {
                window_scale: WindowScale::new(2).unwrap(),
                max_wnd: WindowSize::new(0).unwrap(),
                max_wnd_seq: SeqNum::new(70_000),
                max_next_seq: SeqNum::new(1024),
                unacked_data: false,
                fin_state: FinState::NotSent,
            },
            reply: Peer {
                window_scale: WindowScale::new(0).unwrap(),
                max_wnd: WindowSize::new(400).unwrap(),
                max_wnd_seq: SeqNum::new(1424),
                max_next_seq: SeqNum::new(66_001),
                unacked_data: true,
                fin_state: FinState::NotSent,
            },
        });

        let (new_state, valid) = match args.expected {
            Some(new_state) => (new_state, true),
            None => (state.clone(), false),
        };

        assert_eq!(state.update(&args.segment, args.payload_len, args.dir), (new_state, valid));
    }

    #[test]
    fn closing_complete_test() {
        let state = State::Established(PeerPair {
            original: Peer {
                window_scale: WindowScale::new(2).unwrap(),
                max_wnd: WindowSize::new(0).unwrap(),
                max_wnd_seq: SeqNum::new(70_000),
                max_next_seq: SeqNum::new(1024),
                unacked_data: true,
                fin_state: FinState::Sent(SeqNum::new(1023)),
            },
            reply: Peer {
                window_scale: WindowScale::new(0).unwrap(),
                max_wnd: WindowSize::new(400).unwrap(),
                max_wnd_seq: SeqNum::new(1424),
                max_next_seq: SeqNum::new(66_001),
                unacked_data: false,
                fin_state: FinState::Acked,
            },
        });

        let segment = SegmentHeader {
            seq: SeqNum::new(66_100),
            ack: Some(SeqNum::new(1024)),
            wnd: UnscaledWindowSize::from(10),
            ..Default::default()
        };

        assert_matches!(
            state.update(&segment, /* payload_len */ 0, ConnectionDirection::Reply),
            (State::Closed, true)
        );
    }
}
