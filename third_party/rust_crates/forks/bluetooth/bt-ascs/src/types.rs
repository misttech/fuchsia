// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use bt_common::packet_encoding::Decodable;
use bt_common::{codable_as_bitmask, decodable_enum};
use thiserror::Error;

use bt_common::core::CodecId;
use bt_common::generic_audio::metadata_ltv::Metadata;

/// Error type
#[derive(Debug, Error)]
pub enum Error {
    #[error("Reserved for Future Use: {0}")]
    ReservedFutureUse(String),
    #[error("Server Only Operation")]
    ServerOnlyOperation,
    #[error("Service is already published")]
    AlreadyPublished,
    #[error("Issue publishing service: {0}")]
    PublishError(bt_gatt::types::Error),
    #[error("Unsupported configuration: {0}")]
    Unsupported(String),
}

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub enum ResponseCode {
    Success { ase_id: AseId },
    UnsupportedOpcode,
    InvalidLength,
    InvalidAseId { value: u8 },
    InvalidAseStateMachineTransition { ase_id: AseId },
    InvalidAseDirection { ase_id: AseId },
    UnsupportedAudioCapablities { ase_id: AseId },
    ConfigurationParameterValue { ase_id: AseId, issue: ResponseIssue, reason: ResponseReason },
    Metadata { ase_id: AseId, issue: ResponseIssue, type_value: u8 },
    InsufficientResources { ase_id: AseId },
    UnspecifiedError { ase_id: AseId },
}

impl ResponseCode {
    fn to_code(&self) -> u8 {
        match self {
            ResponseCode::Success { .. } => 0x00,
            ResponseCode::UnsupportedOpcode => 0x01,
            ResponseCode::InvalidLength => 0x02,
            ResponseCode::InvalidAseId { .. } => 0x03,
            ResponseCode::InvalidAseStateMachineTransition { .. } => 0x04,
            ResponseCode::InvalidAseDirection { .. } => 0x05,
            ResponseCode::UnsupportedAudioCapablities { .. } => 0x06,
            ResponseCode::ConfigurationParameterValue {
                issue: ResponseIssue::Unsupported, ..
            } => 0x07,
            ResponseCode::ConfigurationParameterValue {
                issue: ResponseIssue::Rejected, ..
            } => 0x08,
            ResponseCode::ConfigurationParameterValue { issue: ResponseIssue::Invalid, .. } => 0x09,
            ResponseCode::Metadata { issue: ResponseIssue::Unsupported, .. } => 0x0A,
            ResponseCode::Metadata { issue: ResponseIssue::Rejected, .. } => 0x0B,
            ResponseCode::Metadata { issue: ResponseIssue::Invalid, .. } => 0x0C,
            ResponseCode::InsufficientResources { .. } => 0x0D,
            ResponseCode::UnspecifiedError { .. } => 0x0E,
        }
    }

    fn reason_byte(&self) -> u8 {
        match self {
            ResponseCode::ConfigurationParameterValue { reason, .. } => (*reason).into(),
            ResponseCode::Metadata { type_value, .. } => *type_value,
            _ => 0x00,
        }
    }

    fn ase_id_value(&self) -> u8 {
        match self {
            ResponseCode::UnsupportedOpcode | ResponseCode::InvalidLength => 0x00,
            ResponseCode::InvalidAseId { value } => *value,
            ResponseCode::Success { ase_id }
            | ResponseCode::InvalidAseStateMachineTransition { ase_id }
            | ResponseCode::InvalidAseDirection { ase_id }
            | ResponseCode::UnsupportedAudioCapablities { ase_id }
            | ResponseCode::ConfigurationParameterValue { ase_id, .. }
            | ResponseCode::Metadata { ase_id, .. }
            | ResponseCode::InsufficientResources { ase_id }
            | ResponseCode::UnspecifiedError { ase_id } => (*ase_id).into(),
        }
    }

    pub(crate) fn notify_value(&self) -> Vec<u8> {
        [self.ase_id_value(), self.to_code(), self.reason_byte()].into()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResponseIssue {
    Unsupported,
    Rejected,
    Invalid,
}

decodable_enum! {
#[non_exhaustive]
pub enum ResponseReason<u8, bt_common::packet_encoding::Error, OutOfRange> {
    CodecId = 0x01,
    CodecSpecificConfiguration = 0x02,
    SduInterval = 0x03,
    Framing = 0x04,
    Phy = 0x05,
    MaximumSduSize = 0x06,
    RetransmissionNumber = 0x07,
    MaxTransportLatency = 0x08,
    PresentationDelay = 0x09,
    InvalidAseCisMapping = 0x0A,
}
}

decodable_enum! {

#[derive(Default)]
pub enum AseState<u8, bt_common::packet_encoding::Error, OutOfRange> {
    #[default]
    Idle = 0x00,
    CodecConfigured = 0x01,
    QosConfigured = 0x02,
    Enabling = 0x03,
    Streaming = 0x04,
    Disabling = 0x05,
    Releasing = 0x06,
}
}

/// Audio Stream Endpoint Identifier
/// Exposed by the server in ASE characteristics
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AseId(pub u8);

impl AseId {
    const BYTE_SIZE: usize = 1;
}

impl TryFrom<u8> for AseId {
    type Error = ResponseCode;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        if value == 0 {
            return Err(ResponseCode::InvalidAseId { value });
        }
        Ok(Self(value))
    }
}

impl From<AseId> for u8 {
    fn from(value: AseId) -> Self {
        value.0
    }
}

impl Decodable for AseId {
    type Error = ResponseCode;

    fn decode(buf: &[u8]) -> (core::result::Result<Self, Self::Error>, usize) {
        if buf.len() < 1 {
            return (Err(ResponseCode::InvalidLength), buf.len());
        }
        (buf[0].try_into(), 1)
    }
}

decodable_enum! {
    pub enum AseControlPointOpcode<u8, ResponseCode, UnsupportedOpcode> {
        ConfigCodec = 0x01,
        ConfigQos = 0x02,
        Enable = 0x03,
        ReceiverStartReady = 0x04,
        Disable = 0x05,
        ReceiverStopReady = 0x06,
        UpdateMetadata = 0x07,
        Release = 0x08,
    }
}

/// ASE Control Operations.  These can be initiated by a server or client.
/// Defined in Table 4.6 of ASCS v1.0
/// Marked non-exaustive as the remaining operations are RFU and new operations
/// could arrive but should be rejected if they are not recognized.
/// Some variants already contain responses as decoding errors are detected,
/// i.e. invalid parameters or metadata, which will be delivered after the
/// operation is complete with the results from the rest of the operation.
#[non_exhaustive]
#[derive(Debug, PartialEq, Clone)]
pub enum AseControlOperation {
    ConfigCodec { codec_configurations: Vec<CodecConfiguration>, responses: Vec<ResponseCode> },
    ConfigQos { qos_configurations: Vec<QosConfiguration>, responses: Vec<ResponseCode> },
    Enable { ases_with_metadata: Vec<AseIdWithMetadata>, responses: Vec<ResponseCode> },
    ReceiverStartReady { ases: Vec<AseId> },
    // The only possible error here is InvalidLength
    Disable { ases: Vec<AseId> },
    // The only possible error here is InvalidLength
    ReceiverStopReady { ases: Vec<AseId> },
    UpdateMetadata { ases_with_metadata: Vec<AseIdWithMetadata>, responses: Vec<ResponseCode> },
    // The only possible error here is InvalidLength
    Release { ases: Vec<AseId> },
    // This is only initiated by the server
    Released,
}

impl AseControlOperation {
    const MIN_BYTE_SIZE: usize = 3;

    fn contains_invalid_length(&self) -> bool {
        match self {
            Self::ConfigCodec { responses, .. }
            | Self::ConfigQos { responses, .. }
            | Self::Enable { responses, .. }
            | Self::UpdateMetadata { responses, .. } => {
                responses.contains(&ResponseCode::InvalidLength)
            }
            _ => false,
        }
    }
}

impl TryFrom<AseControlOperation> for u8 {
    type Error = Error;

    fn try_from(value: AseControlOperation) -> Result<Self, Self::Error> {
        match value {
            AseControlOperation::ConfigCodec { .. } => Ok(0x01),
            AseControlOperation::ConfigQos { .. } => Ok(0x02),
            AseControlOperation::Enable { .. } => Ok(0x03),
            AseControlOperation::ReceiverStartReady { .. } => Ok(0x04),
            AseControlOperation::Disable { .. } => Ok(0x05),
            AseControlOperation::ReceiverStopReady { .. } => Ok(0x06),
            AseControlOperation::UpdateMetadata { .. } => Ok(0x07),
            AseControlOperation::Release { .. } => Ok(0x08),
            AseControlOperation::Released => Err(Error::ServerOnlyOperation),
        }
    }
}

fn partition_results<T, E>(collection: Vec<Result<T, E>>) -> (Vec<T>, Vec<E>) {
    let mut oks = Vec::with_capacity(collection.len());
    let mut errs = Vec::with_capacity(collection.len());
    for item in collection {
        match item {
            Ok(x) => oks.push(x),
            Err(e) => errs.push(e),
        }
    }
    (oks, errs)
}

impl TryFrom<Vec<u8>> for AseControlOperation {
    type Error = ResponseCode;

    fn try_from(value: Vec<u8>) -> Result<Self, Self::Error> {
        if value.len() < Self::MIN_BYTE_SIZE {
            return Err(ResponseCode::InvalidLength);
        }
        let operation: AseControlPointOpcode = value[0].try_into()?;
        let number_of_ases = value[1] as usize;
        if number_of_ases < 1 {
            return Err(ResponseCode::InvalidLength);
        }
        let (op, consumed) = match operation {
            AseControlPointOpcode::ConfigCodec => {
                let (results, consumed) =
                    CodecConfiguration::decode_multiple(&value[2..], Some(number_of_ases));
                let (codec_configurations, responses) = partition_results(results);
                (Self::ConfigCodec { codec_configurations, responses }, consumed)
            }
            AseControlPointOpcode::ConfigQos => {
                let (results, consumed) =
                    QosConfiguration::decode_multiple(&value[2..], Some(number_of_ases));
                let (qos_configurations, responses) = partition_results(results);
                (Self::ConfigQos { qos_configurations, responses }, consumed)
            }
            AseControlPointOpcode::Enable => {
                let (results, consumed) =
                    AseIdWithMetadata::decode_multiple(&value[2..], Some(number_of_ases));
                let (ases_with_metadata, responses) = partition_results(results);
                (Self::Enable { ases_with_metadata, responses }, consumed)
            }
            AseControlPointOpcode::ReceiverStartReady => {
                // Only InvalidLength is possible
                let (results, consumed) = AseId::decode_multiple(&value[2..], Some(number_of_ases));
                let ases = results.into_iter().collect::<Result<Vec<_>, ResponseCode>>()?;
                (Self::ReceiverStartReady { ases }, consumed)
            }
            AseControlPointOpcode::Disable => {
                // Only InvalidLength is possible
                let (results, consumed) = AseId::decode_multiple(&value[2..], Some(number_of_ases));
                let ases = results.into_iter().collect::<Result<Vec<_>, ResponseCode>>()?;
                (Self::Disable { ases }, consumed)
            }
            AseControlPointOpcode::ReceiverStopReady => {
                // Only InvalidLength is possible
                let (results, consumed) = AseId::decode_multiple(&value[2..], Some(number_of_ases));
                let ases = results.into_iter().collect::<Result<Vec<_>, ResponseCode>>()?;
                (Self::ReceiverStopReady { ases }, consumed)
            }
            AseControlPointOpcode::UpdateMetadata => {
                let (results, consumed) =
                    AseIdWithMetadata::decode_multiple(&value[2..], Some(number_of_ases));
                let (ases_with_metadata, responses) = partition_results(results);
                (Self::UpdateMetadata { ases_with_metadata, responses }, consumed)
            }
            AseControlPointOpcode::Release => {
                // Only InvalidLength is possible
                let (results, consumed) = AseId::decode_multiple(&value[2..], Some(number_of_ases));
                let ases = results.into_iter().collect::<Result<Vec<_>, ResponseCode>>()?;
                (Self::Release { ases }, consumed)
            }
        };
        // A client-initiated ASE Control operation shall also be defined as an invalid
        // length operation if the total length of all parameters written by the
        // client is not equal to the total length of all fixed parameters plus
        // the length of any variable length parameters for that operation as
        // defined in Section 5.1 through Section 5.8.
        if (consumed + 2) != value.len() {
            return Err(ResponseCode::InvalidLength);
        }
        if op.contains_invalid_length() {
            return Err(ResponseCode::InvalidLength);
        }
        Ok(op)
    }
}

decodable_enum! {
pub enum TargetLatency<u8, bt_common::packet_encoding::Error, OutOfRange> {
    TargetLowLatency = 0x01,
    TargetBalanced = 0x02,
    TargetHighReliability = 0x03,
}
}

impl TargetLatency {
    const BYTE_SIZE: usize = 1;
}

decodable_enum! {
pub enum TargetPhy<u8, bt_common::packet_encoding::Error, OutOfRange> {
    Le1MPhy = 0x01,
    Le2MPhy = 0x02,
    LeCodedPhy = 0x03,
}
}

impl TargetPhy {
    const BYTE_SIZE: usize = 1;
}

decodable_enum! {
    pub enum Phy<u8, bt_common::packet_encoding::Error, OutOfRange> {
        Le1MPhy = 0b0001,
        Le2MPhy = 0b0010,
        LeCodedPhy = 0b0100,
    }
}

codable_as_bitmask!(Phy, u8);

impl Phy {
    const BYTE_SIZE: usize = 1;
}

/// Represents Config Codec parameters for a single ASE. See ASCS v1.0.1 Section
/// 5.2.
#[derive(Debug, Clone, PartialEq)]
pub struct CodecConfiguration {
    pub ase_id: AseId,
    pub target_latency: TargetLatency,
    pub target_phy: TargetPhy,
    pub codec_id: CodecId,
    pub codec_specific_configuration: Vec<u8>,
}

impl CodecConfiguration {
    const MIN_BYTE_SIZE: usize =
        AseId::BYTE_SIZE + TargetLatency::BYTE_SIZE + TargetPhy::BYTE_SIZE + CodecId::BYTE_SIZE + 1;
}

impl Decodable for CodecConfiguration {
    type Error = ResponseCode;

    fn decode(buf: &[u8]) -> (core::result::Result<Self, Self::Error>, usize) {
        if buf.len() < Self::MIN_BYTE_SIZE {
            return (Err(ResponseCode::InvalidLength), buf.len());
        }
        let codec_specific_configuration_len = buf[Self::MIN_BYTE_SIZE - 1] as usize;
        let total_len = codec_specific_configuration_len + Self::MIN_BYTE_SIZE;
        if buf.len() < total_len {
            return (Err(ResponseCode::InvalidLength), buf.len());
        }
        let try_decode_fn = |buf: &[u8]| {
            let ase_id = AseId::try_from(buf[0])?;
            let Ok(target_latency) = TargetLatency::try_from(buf[1]) else {
                // TODO: unclear what to do if the target latency is out of range.
                return Err(ResponseCode::ConfigurationParameterValue {
                    ase_id,
                    issue: ResponseIssue::Invalid,
                    reason: ResponseReason::MaxTransportLatency,
                });
            };
            let Ok(target_phy) = TargetPhy::try_from(buf[2]) else {
                return Err(ResponseCode::ConfigurationParameterValue {
                    ase_id,
                    issue: ResponseIssue::Unsupported,
                    reason: ResponseReason::Phy,
                });
            };
            let Ok(codec_id) = CodecId::decode(&buf[3..]).0 else {
                return Err(ResponseCode::ConfigurationParameterValue {
                    ase_id,
                    issue: ResponseIssue::Invalid,
                    reason: ResponseReason::CodecId,
                });
            };
            let codec_specific_configuration = Vec::from(
                &buf[Self::MIN_BYTE_SIZE..Self::MIN_BYTE_SIZE + codec_specific_configuration_len],
            );
            Ok(Self { ase_id, target_latency, target_phy, codec_id, codec_specific_configuration })
        };
        (try_decode_fn(buf), total_len)
    }
}

/// Represents Config QoS parameters for a single ASE. See ASCS v1.0.1 Section
/// 5.2.
#[derive(Debug, Clone, PartialEq)]
pub struct QosConfiguration {
    pub ase_id: AseId,
    pub cig_id: CigId,
    pub cis_id: CisId,
    pub sdu_interval: SduInterval,
    pub framing: Framing,
    pub phy: Vec<Phy>,
    pub max_sdu: MaxSdu,
    pub retransmission_number: u8,
    pub max_transport_latency: MaxTransportLatency,
    pub presentation_delay: PresentationDelay,
}

impl QosConfiguration {
    const BYTE_SIZE: usize = AseId::BYTE_SIZE
        + CigId::BYTE_SIZE
        + CisId::BYTE_SIZE
        + SduInterval::BYTE_SIZE
        + Framing::BYTE_SIZE
        + Phy::BYTE_SIZE
        + MaxSdu::BYTE_SIZE
        + 1 // retransmission_number
        + MaxTransportLatency::BYTE_SIZE
        + PresentationDelay::BYTE_SIZE;
}

impl Decodable for QosConfiguration {
    type Error = ResponseCode;

    fn decode(buf: &[u8]) -> (core::result::Result<Self, Self::Error>, usize) {
        if buf.len() < QosConfiguration::BYTE_SIZE {
            return (Err(ResponseCode::InvalidLength), buf.len());
        }
        let try_decode_fn = |buf: &[u8]| {
            let ase_id = AseId::try_from(buf[0])?;
            let cig_id =
                CigId::try_from(buf[1]).map_err(|_e| ResponseCode::UnspecifiedError { ase_id })?;
            let cis_id =
                CisId::try_from(buf[2]).map_err(|_e| ResponseCode::UnspecifiedError { ase_id })?;
            let sdu_interval;
            match SduInterval::decode(&buf[3..]) {
                (Ok(interval), _) => {
                    sdu_interval = interval;
                }
                (Err(bt_common::packet_encoding::Error::BufferTooSmall), _) => {
                    return Err(ResponseCode::InvalidLength);
                }
                (Err(bt_common::packet_encoding::Error::OutOfRange), _) => {
                    return Err(ResponseCode::ConfigurationParameterValue {
                        ase_id,
                        issue: ResponseIssue::Invalid,
                        reason: ResponseReason::SduInterval,
                    });
                }
                _ => unreachable!(),
            };
            let Ok(framing) = Framing::try_from(buf[6]) else {
                return Err(ResponseCode::ConfigurationParameterValue {
                    ase_id,
                    issue: ResponseIssue::Invalid,
                    reason: ResponseReason::Framing,
                });
            };
            let phy = Phy::from_bits(buf[7]).collect();
            let max_sdu = [buf[8], buf[9]].try_into().map_err(|_e| {
                ResponseCode::ConfigurationParameterValue {
                    issue: ResponseIssue::Invalid,
                    reason: ResponseReason::MaximumSduSize,
                    ase_id,
                }
            })?;
            let retransmission_number = buf[10];
            let max_transport_latency =
                MaxTransportLatency::decode(&buf[11..]).0.map_err(|e| match e {
                    bt_common::packet_encoding::Error::BufferTooSmall => {
                        ResponseCode::InvalidLength
                    }
                    bt_common::packet_encoding::Error::OutOfRange => {
                        ResponseCode::ConfigurationParameterValue {
                            issue: ResponseIssue::Invalid,
                            reason: ResponseReason::MaxTransportLatency,
                            ase_id,
                        }
                    }
                    _ => unreachable!(),
                })?;
            let presentation_delay = PresentationDelay::decode(&buf[13..]).0?;
            Ok(Self {
                ase_id,
                cig_id,
                cis_id,
                sdu_interval,
                framing,
                phy,
                max_sdu,
                retransmission_number,
                max_transport_latency,
                presentation_delay,
            })
        };
        (try_decode_fn(buf), QosConfiguration::BYTE_SIZE)
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct CigId(u8);

impl CigId {
    const BYTE_SIZE: usize = 1;
}

impl TryFrom<u8> for CigId {
    type Error = bt_common::packet_encoding::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        if value > 0xEF { Err(Self::Error::OutOfRange) } else { Ok(CigId(value)) }
    }
}

impl From<CigId> for u8 {
    fn from(value: CigId) -> Self {
        value.0
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct CisId(u8);

impl CisId {
    const BYTE_SIZE: usize = 1;
}

impl TryFrom<u8> for CisId {
    type Error = bt_common::packet_encoding::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        if value > 0xEF { Err(Self::Error::OutOfRange) } else { Ok(CisId(value)) }
    }
}

impl From<CisId> for u8 {
    fn from(value: CisId) -> Self {
        value.0
    }
}

/// SDU Inteval parameter
/// This value is 24 bits long and little-endian on the wire.
/// It is stored native-endian here.
/// Valid range is [0x0000FF, 0x0FFFFF].
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct SduInterval(u32);

impl SduInterval {
    const BYTE_SIZE: usize = 3;
}

impl Decodable for SduInterval {
    type Error = bt_common::packet_encoding::Error;

    fn decode(buf: &[u8]) -> (core::result::Result<Self, Self::Error>, usize) {
        if buf.len() < Self::BYTE_SIZE {
            return (Err(Self::Error::BufferTooSmall), buf.len());
        }
        let val = u32::from_le_bytes([buf[0], buf[1], buf[2], 0]);
        if val < 0xFF || val > 0x0FFFFF {
            return (Err(Self::Error::OutOfRange), Self::BYTE_SIZE);
        }
        (Ok(SduInterval(val)), Self::BYTE_SIZE)
    }
}

decodable_enum! {
pub enum Framing<u8, bt_common::packet_encoding::Error, OutOfRange> {
    Unframed = 0x00,
    Framed = 0x01,
}
}

impl Framing {
    const BYTE_SIZE: usize = 1;
}

/// Max SDU parameter value.
/// Valid range is 0x0000-0x0FFF
/// Transmitted in little-endian. Stored here in native-endian.
#[derive(Debug, Clone, PartialEq)]
pub struct MaxSdu(u16);

impl TryFrom<[u8; 2]> for MaxSdu {
    type Error = bt_common::packet_encoding::Error;

    fn try_from(value: [u8; 2]) -> Result<Self, Self::Error> {
        let value = u16::from_le_bytes([value[0], value[1]]);
        if value > 0xFFF {
            return Err(Self::Error::OutOfRange);
        }
        Ok(MaxSdu(value))
    }
}

impl MaxSdu {
    const BYTE_SIZE: usize = 2;
}

/// Max Transport Latency
/// Valid range is [0x0005, 0x0FA0].
/// Transmitted in little-endian, Stored in native-endian.
#[derive(Debug, Clone, PartialEq)]
pub struct MaxTransportLatency(u16);

impl Decodable for MaxTransportLatency {
    type Error = bt_common::packet_encoding::Error;

    fn decode(buf: &[u8]) -> (core::result::Result<Self, Self::Error>, usize) {
        if buf.len() < Self::BYTE_SIZE {
            return (Err(Self::Error::BufferTooSmall), buf.len());
        }
        let val = u16::from_le_bytes([buf[0], buf[1]]);
        if val < 0x0005 || val > 0x0FA0 {
            return (Err(Self::Error::OutOfRange), Self::BYTE_SIZE);
        }
        (Ok(MaxTransportLatency(val)), Self::BYTE_SIZE)
    }
}

impl TryFrom<std::time::Duration> for MaxTransportLatency {
    type Error = bt_common::packet_encoding::Error;

    fn try_from(value: std::time::Duration) -> Result<Self, Self::Error> {
        let Ok(milliseconds) = u16::try_from(value.as_millis()) else {
            return Err(Self::Error::OutOfRange);
        };
        if !(0x0005..=0x0FA0).contains(&milliseconds) {
            return Err(Self::Error::OutOfRange);
        }
        Ok(Self(milliseconds))
    }
}

impl MaxTransportLatency {
    const BYTE_SIZE: usize = 2;
}

/// Presentation delay parameter value being requested by the client for an ASE.
/// This value is 24 bits long (0x00FFFFFF max)
/// Transmitted in little-endian, Stored in native-endian.
#[derive(Debug, Clone, PartialEq)]
pub struct PresentationDelay {
    pub microseconds: u32,
}

impl PresentationDelay {
    const BYTE_SIZE: usize = 3;
}

impl Decodable for PresentationDelay {
    type Error = ResponseCode;

    fn decode(buf: &[u8]) -> (core::result::Result<Self, Self::Error>, usize) {
        if buf.len() < Self::BYTE_SIZE {
            return (Err(ResponseCode::InvalidLength), buf.len());
        }
        let microseconds = u32::from_le_bytes([buf[0], buf[1], buf[2], 0]);
        (Ok(PresentationDelay { microseconds }), Self::BYTE_SIZE)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AseIdWithMetadata {
    pub ase_id: AseId,
    pub metadata: Vec<Metadata>,
}

impl AseIdWithMetadata {
    const MIN_BYTE_SIZE: usize = 2;
}

impl Decodable for AseIdWithMetadata {
    type Error = ResponseCode;

    fn decode(buf: &[u8]) -> (core::result::Result<Self, Self::Error>, usize) {
        if buf.len() < Self::MIN_BYTE_SIZE {
            return (Err(ResponseCode::InvalidLength), buf.len());
        }
        let ase_id = match AseId::try_from(buf[0]) {
            Ok(ase_id) => ase_id,
            Err(e) => return (Err(e), buf.len()),
        };
        let metadata_length = buf[1] as usize;
        let total_length = Self::MIN_BYTE_SIZE + metadata_length;
        if buf.len() < total_length {
            return (Err(ResponseCode::InvalidLength), buf.len());
        }
        use bt_common::core::ltv::Error as LtvError;
        use bt_common::core::ltv::LtValue;
        let (metadata_results, consumed) = Metadata::decode_all(&buf[2..2 + metadata_length]);
        if consumed != metadata_length {
            return (Err(ResponseCode::InvalidLength), buf.len());
        }
        let metadata_result: Result<Vec<Metadata>, LtvError<<Metadata as LtValue>::Type>> =
            metadata_results.into_iter().collect();
        let Ok(metadata) = metadata_result else {
            match metadata_result.unwrap_err() {
                LtvError::MissingType => return (Err(ResponseCode::InvalidLength), buf.len()),
                LtvError::MissingData(_) => return (Err(ResponseCode::InvalidLength), buf.len()),
                LtvError::UnrecognizedType(_, type_value) => {
                    return (
                        Err(ResponseCode::Metadata {
                            ase_id,
                            issue: ResponseIssue::Unsupported,
                            type_value,
                        }),
                        total_length,
                    );
                }
                LtvError::LengthOutOfRange(_, t, _) | LtvError::TypeFailedToDecode(t, _) => {
                    return (
                        Err(ResponseCode::Metadata {
                            ase_id,
                            issue: ResponseIssue::Invalid,
                            type_value: t.into(),
                        }),
                        total_length,
                    );
                }
            }
        };
        (Ok(Self { ase_id, metadata }), total_length)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use bt_common::packet_encoding::Encodable;

    use bt_common::core::ltv::LtValue;
    use bt_common::generic_audio::{AudioLocation, codec_configuration};

    #[test]
    fn codec_configuration_roundtrip() {
        let codec_specific_configuration = vec![
            codec_configuration::CodecConfiguration::SamplingFrequency(
                codec_configuration::SamplingFrequency::F48000Hz,
            ),
            codec_configuration::CodecConfiguration::FrameDuration(
                codec_configuration::FrameDuration::TenMs,
            ),
            codec_configuration::CodecConfiguration::AudioChannelAllocation(
                [AudioLocation::FrontLeft].into_iter().collect(),
            ),
            codec_configuration::CodecConfiguration::CodecFramesPerSdu(1),
        ];
        let codec_config_len =
            codec_specific_configuration.iter().fold(0, |a, x| a + x.encoded_len());
        let mut vec = Vec::with_capacity(codec_config_len);
        vec.resize(codec_config_len, 0);
        LtValue::encode_all(codec_specific_configuration.clone().into_iter(), &mut vec[..])
            .unwrap();

        let _codec_config = CodecConfiguration {
            ase_id: AseId(5),
            target_latency: TargetLatency::TargetLowLatency,
            target_phy: TargetPhy::Le1MPhy,
            codec_id: CodecId::Assigned(bt_common::core::CodingFormat::Lc3),
            codec_specific_configuration: vec,
        };
    }

    #[test]
    fn codec_configuration_decode() {
        let encoded = &[
            0x04, 0x02, 0x02, 0x06, 0x00, 0x00, 0x00, 0x00, 0x10, 0x02, 0x01, 0x08, 0x02, 0x02,
            0x01, 0x05, 0x03, 0x01, 0x00, 0x00, 0x00, 0x03, 0x04, 0x78, 0x00,
        ];

        let (codec_config, size) = CodecConfiguration::decode(&encoded[..]);
        let codec_config = codec_config.unwrap();
        assert_eq!(size, encoded.len());
        assert_eq!(codec_config.ase_id, AseId(4));
        assert_eq!(codec_config.target_latency, TargetLatency::TargetBalanced);
        assert_eq!(codec_config.target_phy, TargetPhy::Le2MPhy);
        assert_eq!(codec_config.codec_id, CodecId::Assigned(bt_common::core::CodingFormat::Lc3));
        assert_eq!(codec_config.codec_specific_configuration.len(), 0x10);
    }

    #[test]
    fn ase_control_operation_decode_config_codec() {
        let encoded = &[
            0x01, 0x01, 0x04, 0x02, 0x02, 0x06, 0x00, 0x00, 0x00, 0x00, 0x10, 0x02, 0x01, 0x08,
            0x02, 0x02, 0x01, 0x05, 0x03, 0x01, 0x00, 0x00, 0x00, 0x03, 0x04, 0x78, 0x00,
        ];

        let codec_config = CodecConfiguration::decode(&encoded[2..]).0.unwrap();
        let encoded_vec: Vec<u8> = encoded.into_iter().copied().collect();
        let operation = AseControlOperation::try_from(encoded_vec);

        let Ok(operation) = operation else {
            panic!("Expected decode to work correctly, got {operation:?}");
        };

        assert_eq!(
            operation,
            AseControlOperation::ConfigCodec {
                codec_configurations: vec![codec_config],
                responses: Vec::new()
            }
        );
    }

    #[test]
    fn ase_control_operation_decode_config_codec_some_failures() {
        #[rustfmt::skip]
        let encoded = &[
            0x01, 0x02, // Config Codec, Two ASE_IDs
            // First ASE_ID config
            0x04, 0x02, 0x02, 0x06, 0x00, 0x00, 0x00, 0x00, 0x10, 0x02, 0x01, 0x08,
            0x02, 0x02, 0x01, 0x05, 0x03, 0x01, 0x00, 0x00, 0x00, 0x03, 0x04, 0x78, 0x00,
            // Second ASE_ID config, fails based on invalid parameter value
            0x05, 0x05, 0x02, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];

        let codec_config = CodecConfiguration::decode(&encoded[2..]).0.unwrap();
        let encoded_vec: Vec<u8> = encoded.into_iter().copied().collect();
        let operation = AseControlOperation::try_from(encoded_vec);

        let Ok(operation) = operation else {
            panic!("Expected decode to work correctly, got {operation:?}");
        };

        match operation {
            AseControlOperation::ConfigCodec { codec_configurations, responses } => {
                assert_eq!(codec_configurations, vec![codec_config]);
                assert_eq!(responses.len(), 1);
                assert!(matches!(responses[0], ResponseCode::ConfigurationParameterValue { .. }));
            }
            x => panic!("Expected ConfigCodec, got {x:?}"),
        };
    }

    #[test]
    fn ase_control_operation_decode_invalid_length() {
        #[rustfmt::skip]
        let encoded = &[
            0x01, 0x02, // Config Codec, Two ASE_IDs
            // First ASE_ID config
            0x04, 0x02, 0x02, 0x06, 0x00, 0x00, 0x00, 0x00, 0x10, 0x02, 0x01, 0x08,
            0x02, 0x02, 0x01, 0x05, 0x03, 0x01, 0x00, 0x00, 0x00, 0x03, 0x04, 0x78, 0x00,
            // Second ASE_ID config, fails based on invalid length
            0x05, 0x05, 0x02, 0x06, 0x00, 0x00, 0x00,
        ];

        let encoded_vec: Vec<u8> = encoded.into_iter().copied().collect();
        let operation = AseControlOperation::try_from(encoded_vec);

        assert_eq!(operation, Err(ResponseCode::InvalidLength));
    }

    #[test]
    fn qos_configuration_decode() {
        let encoded = &[
            0x03, // ASE_ID,
            0x00, // CIG_ID,
            0x00, // CIS_ID,
            0x10, 0x27, 0x00, 0x01, 0x02, 0x28, 0x00, 0x02, 0xf4, 0x01, 0x18, 0x00, 0x80,
        ];
        let (qos_config, consumed) = QosConfiguration::decode(&encoded[..]);

        let qos_config = qos_config.unwrap();

        assert_eq!(consumed, encoded.len());
        assert_eq!(qos_config.ase_id, AseId(3));
        assert_eq!(qos_config.cig_id, CigId(0));
        assert_eq!(qos_config.cis_id, CisId(0));
        assert_eq!(qos_config.sdu_interval, SduInterval(10000));
        assert_eq!(qos_config.framing, Framing::Framed);
        assert_eq!(qos_config.phy, vec![Phy::Le2MPhy]);
        assert_eq!(qos_config.max_sdu, MaxSdu(40));
        assert_eq!(qos_config.retransmission_number, 2);
        assert_eq!(qos_config.max_transport_latency, MaxTransportLatency(500));
        assert_eq!(qos_config.presentation_delay, PresentationDelay { microseconds: 8388632 });
    }

    #[test]
    fn aseid_with_metadata_decode() {
        let encoded = &[
            0x03, // ASE_ID
            0x08, // metadata length
            0x04, 0x04, 0x65, 0x6E, 0x67, // Language code
            0x02, 0x03, 0x61, // Program Info
        ];

        let (ase_with_metadata, consumed) = AseIdWithMetadata::decode(&encoded[..]);
        let ase_with_metadata = ase_with_metadata.unwrap();
        assert_eq!(consumed, encoded.len());
        assert_eq!(ase_with_metadata.ase_id, AseId(3));
        assert_eq!(ase_with_metadata.metadata.len(), 2);
    }

    #[test]
    fn ase_control_operation_decode_qos() {
        #[rustfmt::skip]
        let encoded_qos = &[
            0x02, 0x01, // Qos OpCode, 1 number_of_ases
            0x03, // ASE_ID,
            0x00, // CIG_ID,
            0x00, // CIS_ID,
            0x10, 0x27, 0x00, // SduInterval
            0x01, 0x02, 0x28, 0x00, 0x02, 0xf4,
            0x01, 0x18, 0x00, 0x80,
        ];
        assert_eq!(QosConfiguration::BYTE_SIZE, encoded_qos.len() - 2);
        let (qos_config, consumed) = QosConfiguration::decode(&encoded_qos[2..]);
        assert!(qos_config.is_ok());
        assert_eq!(consumed, encoded_qos.len() - 2);
        let encoded_qos: Vec<u8> = encoded_qos.into_iter().copied().collect();
        let operation = AseControlOperation::try_from(encoded_qos);
        let Ok(_operation) = operation else {
            panic!("Expected decode to work correctly, for {operation:?}");
        };

        #[rustfmt::skip]
        let encoded_qos_one_fails = &[
            0x02, 0x03, // Qos OpCode, 3 number_of_ases
            0x03, // ASE_ID,
            0x00, // CIG_ID,
            0x00, // CIS_ID,
            0x10, 0x27, 0x00, // SduInterval
            0x01, 0x02, 0x28, 0x00, 0x02, 0xf4,
            0x01, 0x18, 0x00, 0x80,
            0x00, // ASE_ID, (AseId 0 is not allowed)
            0x00, // CIG_ID,
            0x00, // CIS_ID,
            0x10, 0x27, 0x00, // SduInterval
            0x01, 0x02, 0x28, 0x00, 0x02, 0xf4,
            0x01, 0x18, 0x00, 0x80,
            0x04, // ASE_ID
            0x00, // CIG_ID,
            0x00, // CIS_ID,
            0x10, 0x17, 0x00, // SduInterval
            0x01, 0x02, 0x28, 0x00, 0x02, 0xf4,
            0x01, 0x18, 0x00, 0x80,
        ];

        let encoded_qos_one_fails: Vec<u8> = encoded_qos_one_fails.into_iter().copied().collect();

        match AseControlOperation::try_from(encoded_qos_one_fails) {
            Ok(AseControlOperation::ConfigQos { qos_configurations, responses }) => {
                assert_eq!(qos_configurations.len(), 2);
                assert_eq!(responses.len(), 1);
                assert_eq!(responses[0], ResponseCode::InvalidAseId { value: 0x00 });
            }
            x => panic!("Expected ConfigQos to succeed, got {x:?}"),
        };
    }
}
