// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_CONNECTIVITY_BLUETOOTH_CORE_BT_HOST_PUBLIC_PW_BLUETOOTH_SAPPHIRE_INTERNAL_HOST_ISO_ISO_COMMON_H_
#define SRC_CONNECTIVITY_BLUETOOTH_CORE_BT_HOST_PUBLIC_PW_BLUETOOTH_SAPPHIRE_INTERNAL_HOST_ISO_ISO_COMMON_H_

#include "src/connectivity/bluetooth/core/bt-host/public/pw_bluetooth_sapphire/internal/host/hci-spec/constants.h"
#include "src/connectivity/bluetooth/core/bt-host/public/pw_bluetooth_sapphire/internal/host/hci-spec/protocol.h"

#include <pw_bluetooth/hci_data.emb.h>
#include <pw_bluetooth/hci_events.emb.h>

namespace bt::iso {

// Maximum possible size of an Isochronous data packet.
// See Core Spec v5.4, Volume 4, Part E, Section 5.4.5
constexpr size_t kMaxIsochronousDataPacketSize =
    pw::bluetooth::emboss::IsoDataFrameHeader::MaxSizeInBytes() +
    hci_spec::kMaxIsochronousDataPacketPayloadSize;

// Our internal representation of the parameters returned from the
// HCI_LE_CIS_Established event.
struct CisEstablishedParameters {
  struct CisUnidirectionalParams {
    // The actual transport latency, in microseconds.
    uint32_t transport_latency;

    // The transmitter PHY.
    pw::bluetooth::emboss::IsoPhyType phy;

    uint8_t burst_number;

    // The flush timeout, in multiples of the ISO_Interval for the CIS, for each
    // payload sent.
    uint8_t flush_timeout;

    // Maximum size, in octets, of the payload.
    uint16_t max_pdu_size;
  };

  // The maximum time, in microseconds, for transmission of PDUs of all CISes in
  // a CIG event.
  uint32_t cig_sync_delay;

  // The maximum time, in microseconds, for transmission of PDUs of the
  // specified CIS in a CIG event.
  uint32_t cis_sync_delay;

  // Maximum number of subevents in each CIS event.
  uint8_t max_subevents;

  // The "Iso Interval" is represented in units of 1.25ms.
  // (Core Spec v5.4, Vol 4, Part E, Sec 7.7.65.25)
  static constexpr size_t kIsoIntervalToMicroseconds = 1250;

  // The time between two consecutive CIS anchor points.
  uint16_t iso_interval;

  // Central => Peripheral parameters
  CisUnidirectionalParams c_to_p_params;

  // Peripheral => Central parameters
  CisUnidirectionalParams p_to_c_params;
};

// A convenience class for holding an identifier that uniquely represents a
// CIG/CIS combination.
class CigCisIdentifier {
 public:
  CigCisIdentifier() = delete;
  CigCisIdentifier(hci_spec::CigIdentifier cig_id,
                   hci_spec::CisIdentifier cis_id)
      : cig_id_(cig_id), cis_id_(cis_id) {}

  bool operator==(const CigCisIdentifier other) const {
    return (other.cig_id() == cig_id_) && (other.cis_id() == cis_id());
  }

  hci_spec::CigIdentifier cig_id() const { return cig_id_; }
  hci_spec::CisIdentifier cis_id() const { return cis_id_; }

 private:
  hci_spec::CigIdentifier cig_id_;
  hci_spec::CisIdentifier cis_id_;
};

}  // namespace bt::iso

namespace std {
// Implement hash operator for CigCisIdentifier.
template <>
struct hash<bt::iso::CigCisIdentifier> {
 public:
  std::size_t operator()(const bt::iso::CigCisIdentifier& id) const {
    return ((static_cast<size_t>(id.cig_id()) << 8) | id.cis_id());
  }
};

}  // namespace std

#endif  // SRC_CONNECTIVITY_BLUETOOTH_CORE_BT_HOST_PUBLIC_PW_BLUETOOTH_SAPPHIRE_INTERNAL_HOST_ISO_ISO_COMMON_H_
