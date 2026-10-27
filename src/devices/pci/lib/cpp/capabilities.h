// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
#ifndef SRC_DEVICES_PCI_LIB_CPP_CAPABILITIES_H_
#define SRC_DEVICES_PCI_LIB_CPP_CAPABILITIES_H_

#include <lib/pci/constants.h>

#include <memory>
#include <optional>
#include <utility>
#include <vector>

#include <hwreg/bitfields.h>

#include "src/devices/pci/lib/cpp/config.h"

namespace pci {

// General PCI/PCIe capability classes. Final calculated address
// for config corresponds to cfg's base plus cap's base along with
// the specific register's offset.
class Capability {
 public:
  using BaseClass = Capability;
  using RegType = uint8_t;

  // PCI Code and ID Assignment Specification Revision 1.9 section 2.
  enum class Id : RegType {
    /* 0x00 */ kNull = 0,
    /* 0x01 */ kPciPowerManagement,
    /* 0x02 */ kAgp,
    /* 0x03 */ kVitalProductData,
    /* 0x04 */ kSlotIdentification,
    /* 0x05 */ kMsi,
    /* 0x06 */ kCompactPciHotSwap,
    /* 0x07 */ kPciX,
    /* 0x08 */ kHyperTransport,
    /* 0x09 */ kVendor,
    /* 0x0a */ kDebugPort,
    /* 0x0b */ kCompactPciCrc,
    /* 0x0c */ kPciHotplug,
    /* 0x0d */ kPciBridgeSubsystemVendorId,
    /* 0x0e */ kAgp8x,
    /* 0x0f */ kSecureDevice,
    /* 0x10 */ kPciExpress,
    /* 0x11 */ kMsiX,
    /* 0x12 */ kSataDataNdxCfg,
    /* 0x13 */ kAdvancedFeatures,
    /* 0x14 */ kEnhancedAllocation,
    /* 0x15 */ kFlatteningPortalBridge,
  };

  Capability(uint8_t id, uint8_t base, const char* addr = nullptr)
      : id_(id), base_(base), addr_(addr) {}
  virtual ~Capability() = default;
  [[nodiscard]] uint8_t id() const { return id_; }
  [[nodiscard]] uint8_t base() const { return base_; }
  const char* addr() const { return addr_; }

 private:
  uint8_t id_;
  uint8_t base_;
  const char* addr_;
};
using CapabilityList = std::vector<std::unique_ptr<Capability>>;
static_assert(std::to_underlying(Capability::Id::kFlatteningPortalBridge) == 0x15);

struct CapabilityHdr {
  uint8_t id;
  uint8_t ptr;
};

inline bool ReadCapability(const Config& cfg, uint8_t offset, CapabilityHdr* header) {
  offset &= kCapabilityPtrMask;
  // Reject a capability starting in the last DWORD as a guard against overrunning config space.
  if (offset == 0 || offset == kCapPtrMaxValid) {
    return false;
  }

  // Read the id (at offset + 0x0) and pointer to the next cap (at offset + 1).
  // The lower two bits must be masked off per PCI Local Bus Spec 6.7.
  // In the case of PCIe, the ptr field also contains the revision number of
  // the capability and that is handled by the extended capability parsing
  // code.
  header->id = cfg.Read(PciReg8(offset));
  header->ptr = cfg.Read(PciReg8(static_cast<uint16_t>(offset + 1u))) & kCapabilityPtrMask;
  return true;
}

inline std::optional<uint8_t> FindCapability(const Config& cfg, Capability::Id id) {
  if (!config::Status.ReadFrom(&cfg).has_capabilities_list()) {
    return std::nullopt;
  }
  CapabilityHdr hdr{};
  uint8_t cap_offset =
      static_cast<uint8_t>(cfg.Read(Config::kCapabilitiesPtr) & kCapabilityPtrMask);
  // Bounded by kMaxStdCapabilities (the maximum number of 4-byte aligned standard capabilities
  // in 192 bytes) to guard against capability pointer cycles.
  for (size_t i = 0; i < kMaxStdCapabilities && cap_offset != 0; ++i) {
    if (cap_offset < kCapPtrMinValid || cap_offset > kCapPtrMaxValid) {
      return std::nullopt;
    }
    if (!ReadCapability(cfg, cap_offset, &hdr)) {
      return std::nullopt;
    }
    if (hdr.id == std::to_underlying(id)) {
      return cap_offset;
    }
    cap_offset = hdr.ptr;
  }
  return std::nullopt;
}

// General PCIe Extended capability classes. Final calculated address
// for capability register corresponds to cfg's base plus cap's base along with
// the specific register's offset.
class ExtCapability {
 public:
  using BaseClass = ExtCapability;
  using RegType = uint16_t;

  // PCI Code and ID Assignment Specification Revision 1.9 section 3.
  // Extended capability IDs are a 16-bit field in the extended capability header.
  enum class Id : RegType {  // NOLINT(performance-enum-size)
    /* 0x00 */ kNull = 0,
    /* 0x01 */ kAdvancedErrorReporting,
    /* 0x02 */ kVirtualChannelNoMFVC,
    /* 0x03 */ kDeviceSerialNumber,
    /* 0x04 */ kPowerBudgeting,
    /* 0x05 */ kRootComplexLinkDeclaration,
    /* 0x06 */ kRootComplexInternalLinkControl,
    /* 0x07 */ kRootComplexEventCollectorEndpointAssociation,
    /* 0x08 */ kMultiFunctionVirtualChannel,
    /* 0x09 */ kVirtualChannel,
    /* 0x0a */ kRCRB,
    /* 0x0b */ kVendor,
    /* 0x0c */ kCAC,
    /* 0x0d */ kACS,
    /* 0x0e */ kARI,
    /* 0x0f */ kATS,
    /* 0x10 */ kSR_IOV,
    /* 0x11 */ kMR_IOV,
    /* 0x12 */ kMulticast,
    /* 0x13 */ kPRI,
    /* 0x14 */ kEnhancedAllocation,
    /* 0x15 */ kResizableBAR,
    /* 0x16 */ kDynamicPowerAllocation,
    /* 0x17 */ kTPHRequester,
    /* 0x18 */ kLatencyToleranceReporting,
    /* 0x19 */ kSecondaryPCIExpress,
    /* 0x1a */ kPMUX,
    /* 0x1b */ kPASID,
    /* 0x1c */ kLNR,
    /* 0x1d */ kDPC,
    /* 0x1e */ kL1PMSubstates,
    /* 0x1f */ kPrecisionTimeMeasurement,
    /* 0x20 */ kMPCIe,
    /* 0x21 */ kFRSQueueing,
    /* 0x22 */ kReadinessTimeReporting,
    /* 0x23 */ kDesignatedVendor,
    /* 0x24 */ kVFResizableBAR,
    /* 0x25 */ kDataLinkFeature,
    /* 0x26 */ kPhysicalLayer16,
    /* 0x27 */ kLaneMarginingAtReceiver,
    /* 0x28 */ kHierarchyId,
    /* 0x29 */ kNativePCIeEnclosure,
    /* 0x2a */ kPhysicalLayer32,
    /* 0x2b */ kAlternateProtocol,
    /* 0x2c */ kSystemFirmwareIntermediary,
  };

  ExtCapability(uint16_t id, uint8_t version, uint16_t base)
      : id_(id), base_(base), version_(version) {}
  virtual ~ExtCapability() = default;
  [[nodiscard]] uint16_t id() const { return id_; }
  [[nodiscard]] uint16_t base() const { return base_; }
  [[nodiscard]] uint8_t version() const { return version_; }

 private:
  uint16_t id_;
  uint16_t base_;
  uint8_t version_;
};
using ExtCapabilityList = std::vector<std::unique_ptr<ExtCapability>>;
static_assert(std::to_underlying(ExtCapability::Id::kSystemFirmwareIntermediary) == 0x2c);

// PCI Express Base Spec 7.6
struct ExtCapabilityIdReg : public hwreg::RegisterBase<ExtCapabilityIdReg, uint32_t> {
  DEF_FIELD(31, 20, offset);
  DEF_FIELD(19, 16, version);
  DEF_FIELD(15, 0, id);

  static auto Get(uint16_t offset) { return hwreg::RegisterAddr<ExtCapabilityIdReg>(offset); }
};

struct ExtCapabilityHdr {
  uint16_t id;
  uint16_t ptr;
  uint8_t version;
};

inline bool ReadExtCapability(const Config& cfg, uint16_t offset, ExtCapabilityHdr* header) {
  // Reject an extended capability starting in the last DWORD as a guard against overrunning
  // extended config space.
  if (offset == 0 || (offset & kExtCapabilityPtrMask) == kPcieExtCapPtrMaxValid) {
    return false;
  }

  auto reg = ExtCapabilityIdReg::Get(offset).ReadFrom(&cfg);
  uint16_t id = static_cast<uint16_t>(reg.id());
  if (id == kInvalidExtCapabilityId) {
    return false;
  }

  // Extended capabilities start with a 16 bit id, followed by a 4 bit version
  // and a 12 bit pointer to the next offset. Like standard capabilities, the bottom
  // 2 bits off the offset must be masked off.
  header->id = id;
  header->ptr = static_cast<uint16_t>(reg.offset() & kExtCapabilityPtrMask);
  header->version = static_cast<uint8_t>(reg.version());
  return true;
}

}  // namespace pci

#endif  // SRC_DEVICES_PCI_LIB_CPP_CAPABILITIES_H_
