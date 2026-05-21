// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/firmware/paver/iris.h"

#include <fidl/fuchsia.storage.partitions/cpp/wire_types.h>
#include <lib/abr/abr.h>
#include <lib/component/incoming/cpp/clone.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/fdio/directory.h>
#include <lib/fit/defer.h>
#include <lib/zx/result.h>
#include <lib/zx/vmo.h>
#include <zircon/hw/gpt.h>
#include <zircon/status.h>

#include <algorithm>
#include <iterator>
#include <set>
#include <string>

#include <fbl/unique_fd.h>
#include <gpt/gpt.h>

#include "src/firmware/paver/block-devices.h"
#include "src/firmware/paver/boot_control_definition.h"
#include "src/firmware/paver/gpt.h"
#include "src/firmware/paver/libboot_control.h"
#include "src/firmware/paver/partition-client.h"
#include "src/firmware/paver/pave-logging.h"
#include "src/firmware/paver/utils.h"
#include "src/firmware/paver/validation.h"
#include "src/lib/uuid/uuid.h"

namespace paver {

const std::set<std::string> kSupportedBoards{
    "iris",
};

zx::result<std::unique_ptr<DevicePartitioner>> IrisPartitioner::Initialize(
    const BlockDevices& devices, fidl::UnownedClientEnd<fuchsia_io::Directory> svc_root,
    const PaverConfig& config) {
  zx::result<std::string> board_name = GetBoardName(svc_root);
  if (board_name.is_error()) {
    return board_name.take_error();
  }

  if (!kSupportedBoards.contains(board_name.value())) {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  auto gpt = GptDevicePartitioner::InitializeGpt(devices, svc_root, config);
  if (gpt.is_error()) {
    return gpt.take_error();
  }
  if (gpt->initialize_partition_tables) {
    LOG("Found GPT but it was missing expected partitions. The device should be re-initialized via fastboot.\n");
    return zx::error(ZX_ERR_BAD_STATE);
  }

  auto partitioner =
      WrapUnique(new IrisPartitioner(devices.Duplicate(), component::MaybeClone(svc_root)));

  LOG("Successfully initialized Iris Device Partitioner\n");
  return zx::ok(std::move(partitioner));
}

bool IrisPartitioner::SupportsPartition(const PartitionSpec& spec) const {
  if (spec.partition == Partition::kBootloaderA || spec.partition == Partition::kBootloaderB) {
    // TODO(b/515134439): Support recover_zbi once recovery image is supported.
    return !spec.content_type.empty() && spec.content_type != "recovery_zbi";
  }

  constexpr PartitionSpec non_bootloader_specs[] = {
      PartitionSpec(paver::Partition::kZirconA),
      PartitionSpec(paver::Partition::kZirconB),
      PartitionSpec(paver::Partition::kVbMetaA),
      PartitionSpec(paver::Partition::kVbMetaB),
      PartitionSpec(paver::Partition::kAbrMeta),
      PartitionSpec(paver::Partition::kFuchsiaVolumeManager),
      PartitionSpec(paver::Partition::kFuchsiaVolumeManager, kOpaqueVolumeContentType),
  };
  return std::any_of(std::cbegin(non_bootloader_specs), std::cend(non_bootloader_specs),
                     [&](const PartitionSpec& supported) { return SpecMatches(spec, supported); });
}

zx::result<std::unique_ptr<PartitionClient>> IrisPartitioner::FindPartition(
    const PartitionSpec& spec) const {
  if (!SupportsPartition(spec)) {
    ERROR("Unsupported partition %s\n", spec.ToString().c_str());
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  auto gpt = GptDevicePartitioner::InitializeGpt(devices_, svc_root_, PaverConfig{});
  if (gpt.is_error()) {
    return gpt.take_error();
  }

  std::vector<std::string> part_names;
  switch (spec.partition) {
    case Partition::kBootloaderA:
    case Partition::kBootloaderB:
      part_names.emplace_back(spec.content_type);
      part_names.back() += spec.partition == Partition::kBootloaderA ? "_a" : "_b";
      break;
    case Partition::kZirconA:
      part_names.emplace_back("boot_a");
      break;
    case Partition::kZirconB:
      part_names.emplace_back("boot_b");
      break;
    case Partition::kVbMetaA:
      part_names.emplace_back("vbmeta_a");
      break;
    case Partition::kVbMetaB:
      part_names.emplace_back("vbmeta_b");
      break;
    case Partition::kAbrMeta:
      part_names.emplace_back("devinfo");
      break;
    case Partition::kFuchsiaVolumeManager:
      part_names.emplace_back("super");
      break;
    default:
      ERROR("Iris partitioner cannot find unknown partition type\n");
      return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  for (const auto& part_name : part_names) {
    if (auto client = gpt->gpt->FindPartition([&part_name](const GptPartitionMetadata& part) {
          return FilterByName(part, part_name);
        });
        client.is_ok()) {
      return client;
    }
  }

  if (spec.partition == Partition::kAbrMeta) {
    return OpenAbrBlockDevice();
  }

  return zx::error(ZX_ERR_NOT_FOUND);
}

zx::result<std::unique_ptr<PartitionClient>> IrisPartitioner::OpenAbrBlockDevice() const {
  // TODO(b/512994030): Switch to an explicit synchronous API.
  auto part_connector =
      OpenBlockPartition(devices_, std::nullopt, std::nullopt, "devinfo", ZX_SEC(5));
  if (part_connector.is_error()) {
    ERROR("Failed to open block partition via OpenBlockPartition: %s\n",
          part_connector.status_string());
    return part_connector.take_error();
  }
  auto client = BlockPartitionClient::Create(std::move(part_connector.value()));
  if (client.is_error()) {
    ERROR("Failed to create BlockPartitionClient via OpenBlockPartition: %s\n",
          client.status_string());
    return client.take_error();
  }
  LOG("Successfully found ABRmeta on raw block device via OpenBlockPartition\n");
  return zx::ok(std::move(client.value()));
}

zx::result<> IrisPartitioner::ResetPartitionTables() const {
  return zx::error(ZX_ERR_NOT_SUPPORTED);
}

zx::result<> IrisPartitioner::ValidatePayload(const PartitionSpec& spec,
                                              std::span<const uint8_t> data) const {
  if (!SupportsPartition(spec)) {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }
  return zx::ok();
}

const paver::BlockDevices& IrisPartitioner::Devices() const { return devices_; }

fidl::UnownedClientEnd<fuchsia_io::Directory> IrisPartitioner::SvcRoot() const {
  return svc_root_.borrow();
}

zx::result<std::unique_ptr<DevicePartitioner>> IrisPartitionerFactory::New(
    const paver::BlockDevices& devices, fidl::UnownedClientEnd<fuchsia_io::Directory> svc_root,
    const PaverConfig& config, std::shared_ptr<Context> context) {
  return IrisPartitioner::Initialize(devices, svc_root, config);
}

namespace {

using uuid::Uuid;

class IrisAbrClient : public abr::Client {
 public:
  static zx::result<std::unique_ptr<abr::Client>> Create(
      std::unique_ptr<paver::PartitionClient> partition) {
    zx::vmo vmo;
    if (auto status = zx::make_result(zx::vmo::create(kIrisDevinfoSize, 0, &vmo));
        status.is_error()) {
      ERROR("Failed to create vmo\n");
      return status.take_error();
    }
    return zx::ok(new IrisAbrClient(std::move(partition), std::move(vmo)));
  }

 private:
  IrisAbrClient(std::unique_ptr<paver::PartitionClient> partition, zx::vmo vmo)
      : Client(/*custom = */ true), partition_(std::move(partition)), vmo_(std::move(vmo)) {}

  zx::result<> Read(uint8_t* buffer, size_t size) override {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  zx::result<> Write(const uint8_t* buffer, size_t size) override {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  iris_devinfo_ab_slot_data_t ToIris(const AbrSlotData& src, bool active,
                                     const iris_devinfo_ab_slot_data_t& disk) {
    iris_devinfo_ab_slot_data_t slot_metadata = disk;
    slot_metadata.set_successful(src.successful_boot);
    // ABR retry count is only in effect when not successful otherwise it's cleared to 0.
    if (!slot_metadata.successful()) {
      slot_metadata.retry_count = std::min<uint8_t>(src.tries_remaining, 3);
    }
    slot_metadata.set_unbootable(AbrGetNormalizedPriority(&src) == 0 ? 1 : 0);
    slot_metadata.set_active(active ? 1 : 0);
    return slot_metadata;
  }

  AbrSlotData ToFuchsia(const iris_devinfo_ab_slot_data_t& slot_metadata) {
    AbrSlotData abr_slot_data = {};
    abr_slot_data.successful_boot = slot_metadata.successful();
    abr_slot_data.tries_remaining = slot_metadata.retry_count;
    abr_slot_data.priority = slot_metadata.active() ? 2 : 1;
    if (abr_slot_data.successful_boot) {
      // ABR requires that successful boot must have a retry of 0.
      abr_slot_data.tries_remaining = 0;
    } else if (slot_metadata.unbootable()) {
      abr_slot_data.priority = 0;
    }
    return abr_slot_data;
  }

  zx::result<> ReadCustom(AbrSlotData* a, AbrSlotData* b, uint8_t* one_shot_recovery) override {
    auto devinfo_res = ReadDevinfoAb();
    if (devinfo_res.is_error()) {
      return devinfo_res.take_error();
    }
    *a = ToFuchsia(devinfo_res->slots[0]);
    *b = ToFuchsia(devinfo_res->slots[1]);
    *one_shot_recovery = 0;
    return zx::ok();
  }

  zx::result<> WriteCustom(const AbrSlotData* a, const AbrSlotData* b,
                           uint8_t one_shot_recovery) override {
    auto devinfo_res = ReadDevinfoAb();
    if (devinfo_res.is_error()) {
      return devinfo_res.take_error();
    }
    iris_devinfo_ab_data_t ab_data = *devinfo_res;

    bool b_active = AbrGetActiveSlotFromData(a, b) == kAbrSlotIndexB;
    // In case of both slot unbootable, default to active a slot.
    bool a_active = !b_active;
    ab_data.slots[0] = ToIris(*a, a_active, ab_data.slots[0]);
    ab_data.slots[1] = ToIris(*b, b_active, ab_data.slots[1]);

    // Not supported on Iris
    (void)one_shot_recovery;

    if (auto status =
            zx::make_result(vmo_.write(&ab_data, kIrisAbrMetadataOffset, sizeof(ab_data)));
        status.is_error()) {
      ERROR("Failed to write ab_data to vmo\n");
      return status;
    }

    if (auto status = partition_->Write(vmo_, kIrisDevinfoSize); status.is_error()) {
      ERROR("Failed to write to partition\n");
      return status.take_error();
    }
    return partition_->Flush();
  }

  zx::result<iris_devinfo_ab_data_t> ReadDevinfoAb() const {
    if (auto status = partition_->Read(vmo_, kIrisDevinfoSize); status.is_error()) {
      ERROR("Failed to read from partition\n");
      return status.take_error();
    }

    uint32_t magic = 0;
    if (auto status = zx::make_result(vmo_.read(&magic, 0, sizeof(magic))); status.is_error()) {
      ERROR("Failed to read magic from vmo\n");
      return status.take_error();
    }

    // TODO(b/512994030): Remove this check once we have a way to initialize the devinfo partition.
    if (magic != IRIS_DEVINFO_MAGIC) {
      ERROR("Invalid devinfo magic: 0x%08x\n", magic);
      return zx::error(ZX_ERR_BAD_STATE);
    }

    iris_devinfo_ab_data_t ab_data;
    if (auto status = zx::make_result(vmo_.read(&ab_data, kIrisAbrMetadataOffset, sizeof(ab_data)));
        status.is_error()) {
      ERROR("Failed to read ab_data from vmo\n");
      return status.take_error();
    }

    return zx::ok(ab_data);
  }

  zx::result<> Flush() override { return zx::ok(); }

  std::unique_ptr<paver::PartitionClient> partition_;
  zx::vmo vmo_;
};

}  // namespace

zx::result<std::unique_ptr<abr::Client>> IrisPartitioner::CreateAbrClient() const {
  zx::result partition = FindPartition(paver::PartitionSpec(paver::Partition::kAbrMeta));
  if (partition.is_error()) {
    ERROR("Failed to find abr partition\n");
    return partition.take_error();
  }

  return IrisAbrClient::Create(std::move(partition.value()));
}

}  // namespace paver
