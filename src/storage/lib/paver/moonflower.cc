// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/storage/lib/paver/moonflower.h"

#include <fidl/fuchsia.storage.partitions/cpp/wire_types.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/fit/defer.h>
#include <lib/zx/result.h>

#include <algorithm>
#include <iterator>
#include <string>

#include <gpt/gpt.h>
#include <hwreg/bitfields.h>

#include "src/lib/uuid/uuid.h"
#include "src/storage/lib/paver/pave-logging.h"
#include "src/storage/lib/paver/system_shutdown_state.h"
#include "src/storage/lib/paver/utils.h"
#include "src/storage/lib/paver/validation.h"

namespace paver {
namespace {

using fuchsia_system_state::SystemPowerState;
using uuid::Uuid;

}  // namespace

zx::result<std::unique_ptr<DevicePartitioner>> MoonflowerPartitioner::Initialize(
    const BlockDevices& devices, fidl::UnownedClientEnd<fuchsia_io::Directory> svc_root,
    fidl::ClientEnd<fuchsia_device::Controller> block_device) {
  if (IsBoard(svc_root, "kola").is_error() && IsBoard(svc_root, "sorrel").is_error() &&
      IsBoard(svc_root, "lilac").is_error()) {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  auto gpt = GptDevicePartitioner::InitializeGpt(devices, svc_root, std::move(block_device));
  if (gpt.is_error()) {
    return gpt.take_error();
  }
  if (gpt->initialize_partition_tables) {
    LOG("Found GPT but it was missing expected partitions.  The device should be re-initialized "
        "via fastboot.\n");
    return zx::error(ZX_ERR_BAD_STATE);
  }

  auto partitioner = WrapUnique(new MoonflowerPartitioner(std::move(gpt->gpt)));

  LOG("Successfully initialized Moonflower Device Partitioner\n");
  return zx::ok(std::move(partitioner));
}

const paver::BlockDevices& MoonflowerPartitioner::Devices() const { return gpt_->devices(); }

fidl::UnownedClientEnd<fuchsia_io::Directory> MoonflowerPartitioner::SvcRoot() const {
  return gpt_->svc_root();
}

bool MoonflowerPartitioner::SupportsPartition(const PartitionSpec& spec) const {
  // We use the kBootloader spec here to allow paving arbitrary images to any partition named in the
  // `content_type`. This is a bit of a misuse of the spec, these images aren't really bootloaders,
  // but the bootloader paving API is the only one that supports arbitrary content type like this so
  // we use it for this purpose so that we have the flexibility to add images later on without
  // having to worry about updating the paver first.
  //
  // TODO(b/436253787): consider adding a paver API to support this use case more natually.
  if (spec.partition == Partition::kBootloaderA || spec.partition == Partition::kBootloaderB) {
    // Do not check if the partition actually exists here. For bootloaders, `SupportsPartition()`
    // returning false results in a non-fatal skip of the image, intended to support soft-transition
    // of new OTA files by being able to add new images before the paver may support them.
    //
    // In this case where we allow writing to any partition, it would be too easy to accidentally
    // omit a partition e.g. if there were a typo in the partition name. Instead, we report that
    // we support the image here, then if the partition doesn't exist we will error out later when
    // writing it. This will result in a OTA failure instead of silently skipping the image.
    //
    // The downside is if we do need to modify the GPT later we'll have to use some other transition
    // mechanism e.g. stepping-stone OTAs or re-flashing each device, since the paver will fail an
    // OTA if a given partition does not yet exist on-device.
    return !spec.content_type.empty();
  }

  constexpr PartitionSpec non_bootloader_specs[] = {
      PartitionSpec(paver::Partition::kZirconA),
      PartitionSpec(paver::Partition::kZirconB),
      PartitionSpec(paver::Partition::kFuchsiaVolumeManager),
      PartitionSpec(paver::Partition::kFuchsiaVolumeManager, kOpaqueVolumeContentType),
  };
  return std::any_of(std::cbegin(non_bootloader_specs), std::cend(non_bootloader_specs),
                     [&](const PartitionSpec& supported) { return SpecMatches(spec, supported); });
}

zx::result<std::string> MoonflowerPartitioner::PartitionNameForSpec(
    const PartitionSpec& spec) const {
  if (!SupportsPartition(spec)) {
    ERROR("Unsupported partition %s\n", spec.ToString().c_str());
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  std::string part_name;
  switch (spec.partition) {
    case Partition::kBootloaderA:
    case Partition::kBootloaderB:
      // Normally the `content_type` is the partition name, but we've also used "recovery_zbi" to
      // map to the "vendor_boot" partition so we support that as well.
      if (spec.content_type == "recovery_zbi") {
        part_name = "vendor_boot";
      } else {
        part_name = spec.content_type;
      }
      // Only support slotted A/B partitions here. OTA'ing a non-A/B partition is very risky since
      // any failure could result in a bricked device, we do not support it on moonflower.
      part_name += spec.partition == Partition::kBootloaderA ? "_a" : "_b";
      break;
    case Partition::kZirconA:
      part_name = "boot_a";
      break;
    case Partition::kZirconB:
      part_name = "boot_b";
      break;
    case Partition::kFuchsiaVolumeManager:
      part_name = "super";
      break;
    default:
      ERROR("Moonflower partitioner cannot find unknown partition type\n");
      return zx::error(ZX_ERR_NOT_SUPPORTED);
  }
  return zx::ok(std::move(part_name));
}

zx::result<std::unique_ptr<PartitionClient>> MoonflowerPartitioner::FindPartition(
    const PartitionSpec& spec) const {
  return FindGptPartition(spec);
}

zx::result<std::unique_ptr<BlockPartitionClient>> MoonflowerPartitioner::FindGptPartition(
    const PartitionSpec& spec) const {
  zx::result name = PartitionNameForSpec(spec);
  if (name.is_error()) {
    return name.take_error();
  }
  return gpt_->FindPartition(
      [&](const GptPartitionMetadata& part) { return FilterByName(part, *name); });
}

zx::result<std::vector<std::unique_ptr<BlockPartitionClient>>>
MoonflowerPartitioner::FindAllPartitions(FilterCallback filter) const {
  return gpt_->FindAllPartitions(std::move(filter));
}

zx::result<FindPartitionDetailsResult> MoonflowerPartitioner::FindPartitionDetails(
    const PartitionSpec& spec) const {
  zx::result name = PartitionNameForSpec(spec);
  if (name.is_error()) {
    return name.take_error();
  }
  return gpt_->FindPartitionDetails(
      [&](const GptPartitionMetadata& part) { return FilterByName(part, *name); });
}

zx::result<> MoonflowerPartitioner::WipeFvm() const { return gpt_->WipeFvm(); }

zx::result<> MoonflowerPartitioner::ResetPartitionTables() const {
  ERROR("Initialising partition tables is not supported for a Moonflower device\n");
  return zx::error(ZX_ERR_NOT_SUPPORTED);
}

zx::result<> MoonflowerPartitioner::ValidatePayload(const PartitionSpec& spec,
                                                    std::span<const uint8_t> data) const {
  if (!SupportsPartition(spec)) {
    ERROR("Unsupported partition %s\n", spec.ToString().c_str());
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  if (IsZirconPartitionSpec(spec)) {
    if (!IsValidAndroidKernel(data)) {
      return zx::error(ZX_ERR_BAD_STATE);
    }
  }

  return zx::ok();
}

zx::result<std::unique_ptr<DevicePartitioner>> MoonflowerPartitionerFactory::New(
    const BlockDevices& devices, fidl::UnownedClientEnd<fuchsia_io::Directory> svc_root, Arch arch,
    std::shared_ptr<Context> context, fidl::ClientEnd<fuchsia_device::Controller> block_device) {
  return MoonflowerPartitioner::Initialize(devices, svc_root, std::move(block_device));
}

class MoonflowerAbrClient : public abr::Client {
 public:
  static zx::result<std::unique_ptr<MoonflowerAbrClient>> Create(
      const MoonflowerPartitioner* partitioner) {
    zx::result zircon_a = partitioner->FindGptPartition(PartitionSpec(Partition::kZirconA));
    if (zircon_a.is_error()) {
      ERROR("Failed to find Zircon A partition\n");
      return zircon_a.take_error();
    }

    zx::result zircon_b = partitioner->FindGptPartition(PartitionSpec(Partition::kZirconB));
    if (zircon_b.is_error()) {
      ERROR("Failed to find Zircon B partition\n");
      return zircon_b.take_error();
    }

    auto [client, server] =
        fidl::Endpoints<fuchsia_storage_partitions::PartitionsManager>::Create();
    zx::result result = component::ConnectAt(partitioner->SvcRoot(), std::move(server));
    if (result.is_error()) {
      ERROR("Failed to connect to PartitionsManager: %s\n", result.status_string());
      return result.take_error();
    }

    return zx::ok(new MoonflowerAbrClient(partitioner, std::move(zircon_a.value()),
                                          std::move(zircon_b.value()), std::move(client)));
  }

  zx::result<> GetPartitionFlags(uint64_t* a_flags, uint64_t* b_flags) {
    if (pending_zircon_a_flags_) {
      *a_flags = *pending_zircon_a_flags_;
    } else {
      zx::result a = zircon_a_->GetMetadata();
      if (a.is_error()) {
        return a.take_error();
      }
      *a_flags = a->flags;
    }
    if (pending_zircon_b_flags_) {
      *b_flags = *pending_zircon_b_flags_;
    } else {
      zx::result b = zircon_b_->GetMetadata();
      if (b.is_error()) {
        return b.take_error();
      }
      *b_flags = b->flags;
    }
    return zx::ok();
  }

  zx::result<> SetPartitionFlags(uint64_t a_flags, uint64_t b_flags) {
    zx::result result = UpdatePartitionMetadata(*zircon_a_, a_flags, {});
    if (result.is_error()) {
      return result.take_error();
    }
    result = UpdatePartitionMetadata(*zircon_b_, b_flags, {});
    if (result.is_error()) {
      return result.take_error();
    }
    pending_zircon_a_flags_ = a_flags;
    pending_zircon_b_flags_ = b_flags;
    return zx::ok();
  }

  zx::result<> SwapAbPartitionTypeGuids(bool new_slot_is_b) {
    zx::result a_partitions = partitioner_->FindAllPartitions(
        [](const GptPartitionMetadata& metadata) -> bool { return metadata.name.ends_with("_a"); });
    if (a_partitions.is_error()) {
      ERROR("Failed to find a partitions:%s \n", a_partitions.status_string());
      return a_partitions.take_error();
    }
    zx::result b_partitions = partitioner_->FindAllPartitions(
        [](const GptPartitionMetadata& metadata) -> bool { return metadata.name.ends_with("_b"); });
    if (b_partitions.is_error()) {
      ERROR("Failed to find b partitions:%s \n", b_partitions.status_string());
      return b_partitions.take_error();
    }
    if (a_partitions->size() != b_partitions->size()) {
      ERROR("Unexpectedly found %zu a partitions and %zu b partitions\n", a_partitions->size(),
            b_partitions->size());
      return zx::error(ZX_ERR_BAD_STATE);
    }

    struct Partition {
      std::unique_ptr<BlockPartitionClient> client;
      PartitionMetadata metadata;
    };
    auto create_partition_map = [](std::vector<std::unique_ptr<BlockPartitionClient>> partitions)
        -> zx::result<std::unordered_map<std::string, Partition>> {
      std::unordered_map<std::string, Partition> partitions_map;
      for (auto& part : partitions) {
        zx::result metadata = part->GetMetadata();
        if (metadata.is_error()) {
          ERROR("Failed to get metadata: %s\n", metadata.status_string());
          return metadata.take_error();
        }
        ZX_DEBUG_ASSERT(metadata->name.size() >= 2);
        std::string base_name = metadata->name.substr(0, metadata->name.size() - 2);
        partitions_map[base_name] = Partition{
            .client = std::move(part),
            .metadata = std::move(*metadata),
        };
      }
      return zx::ok(std::move(partitions_map));
    };
    zx::result a_partitions_map = create_partition_map(std::move(*a_partitions));
    if (a_partitions_map.is_error()) {
      return a_partitions_map.take_error();
    }
    zx::result b_partitions_map = create_partition_map(std::move(*b_partitions));
    if (b_partitions_map.is_error()) {
      return b_partitions_map.take_error();
    }

    const std::unordered_map<std::string, Partition>& new_partitions =
        new_slot_is_b ? *b_partitions_map : *a_partitions_map;
    const std::unordered_map<std::string, Partition>& old_partitions =
        new_slot_is_b ? *a_partitions_map : *b_partitions_map;

    auto iter = new_partitions.find("boot");
    if (iter == new_partitions.end()) {
      ERROR("Failed to find the boot partition.\n");
      return zx::error(ZX_ERR_BAD_STATE);
    }
    const Uuid& inactive_type_guid = iter->second.metadata.type_guid;

    // Check that all of the new partitions have the same type GUID (inactive_type_guid) and have a
    // corresponding old partition, and then swap the type GUIDs.
    for (const auto& [part_name, new_part] : new_partitions) {
      auto old_part = old_partitions.find(part_name);
      if (old_part == old_partitions.end()) {
        ERROR("Failed to find corresponding %s partition.\n", part_name.c_str());
        return zx::error(ZX_ERR_BAD_STATE);
      }
      if (new_part.metadata.type_guid != inactive_type_guid) {
        // Make note if the device has mixed partition type GUID assignment
        // (https://fxbug.dev/397766186).
        ERROR("%s partition has type GUID %s (expected %s)\n", part_name.c_str(),
              new_part.metadata.type_guid.ToString().c_str(),
              inactive_type_guid.ToString().c_str());
        continue;
      }
      const Uuid& active_type_guid = old_part->second.metadata.type_guid;

      zx::result result = UpdatePartitionMetadata(*new_part.client, {}, active_type_guid);
      if (result.is_error()) {
        ERROR("Failed to update type GUID: %s\n", result.status_string());
        return result.take_error();
      }
      result = UpdatePartitionMetadata(*old_part->second.client, {}, inactive_type_guid);
      if (result.is_error()) {
        ERROR("Failed to update type GUID: %s\n", result.status_string());
        return result.take_error();
      }
    }
    return zx::ok();
  }

  void Discard() {
    transaction_.reset();
    pending_zircon_a_flags_.reset();
    pending_zircon_b_flags_.reset();
  }

  zx::result<> Commit() {
    if (transaction_.is_valid()) {
      fidl::WireResult result = partitions_manager_->CommitTransaction(std::move(transaction_));
      if (!result.ok()) {
        ERROR("Failed to commit transaction: %s\n", result.status_string());
        return zx::error(result.status());
      }
    }
    Discard();
    return zx::ok();
  }

  zx::result<> Flush() override { return Commit(); }

 private:
  MoonflowerAbrClient(
      const MoonflowerPartitioner* partitioner, std::unique_ptr<BlockPartitionClient> zircon_a,
      std::unique_ptr<BlockPartitionClient> zircon_b,
      fidl::ClientEnd<fuchsia_storage_partitions::PartitionsManager> partitions_manager)
      : abr::Client(/*custom=*/true),
        partitioner_(partitioner),
        zircon_a_(std::move(zircon_a)),
        zircon_b_(std::move(zircon_b)),
        partitions_manager_(std::move(partitions_manager)) {}

  zx::result<> UpdatePartitionMetadata(PartitionClient& client, std::optional<uint64_t> flags,
                                       std::optional<Uuid> type_guid) {
    zx::result partition = client.connector()->PartitionManagement();
    if (partition.is_error()) {
      return partition.take_error();
    }
    fidl::Arena arena;
    auto request = fuchsia_storage_partitions::wire::PartitionUpdateMetadataRequest::Builder(arena);
    zx::result transaction = GetTransactionToken();
    if (transaction.is_error()) {
      return transaction.take_error();
    }
    request.transaction(std::move(*transaction));
    if (flags) {
      request.flags(*flags);
    }
    fuchsia_hardware_block_partition::wire::Guid type;
    if (type_guid) {
      ZX_DEBUG_ASSERT(uuid::kUuidSize == type.value.size());
      memcpy(type.value.data(), type_guid->bytes(), uuid::kUuidSize);
      request.type_guid(type);
    }
    fidl::WireResult result = fidl::WireCall<fuchsia_storage_partitions::Partition>(*partition)
                                  ->UpdateMetadata(request.Build());
    if (!result.ok()) {
      ERROR("Failed to update metadata: %s\n", result.status_string());
      return zx::error(result.status());
    }
    return zx::ok();
  }

  zx::result<zx::eventpair> GetTransactionToken() {
    if (!transaction_.is_valid()) {
      fidl::WireResult result = partitions_manager_->CreateTransaction();
      if (result->is_error()) {
        ERROR("Failed to create A/B transaction: %s\n", zx_status_get_string(result.status()));
        return zx::error(result.status());
      }
      transaction_ = std::move(result->value()->transaction);
    }
    zx::eventpair transaction;
    if (zx_status_t status = transaction_.duplicate(ZX_RIGHT_SAME_RIGHTS, &transaction);
        status != ZX_OK) {
      return zx::error(status);
    }
    return zx::ok(std::move(transaction));
  }

  zx::result<> Read(uint8_t* buffer, size_t size) override {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  zx::result<> Write(const uint8_t* buffer, size_t size) override {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  zx::result<> ReadCustom(AbrSlotData* a, AbrSlotData* b, uint8_t* one_shot_recovery) override {
    uint64_t a_flags, b_flags;
    zx::result result = GetPartitionFlags(&a_flags, &b_flags);
    if (result.is_error()) {
      return result.take_error();
    }
    *a = ToFuchsia(a_flags);
    *b = ToFuchsia(b_flags);

    // TODO(b/348034903): Consider checking that the higher-priority active slot has the active
    // partition type GUIDs.

    *one_shot_recovery = 0;  // not supported
    return zx::ok();
  }

  zx::result<> WriteCustom(const AbrSlotData* a, const AbrSlotData* b,
                           uint8_t one_shot_recovery) override {
    bool slot_switch = DetectSlotSwitch(a, b);
    bool new_slot_is_b = false;
    if (slot_switch) {
      new_slot_is_b = true;
    } else {
      slot_switch = DetectSlotSwitch(b, a);
    }

    uint64_t a_flags, b_flags;
    zx::result result = GetPartitionFlags(&a_flags, &b_flags);
    if (result.is_error()) {
      return result.take_error();
    }
    a_flags = ToMoonflower(*a, *b, a_flags);
    b_flags = ToMoonflower(*b, *a, b_flags);

    auto discard_changes = fit::defer([&]() { Discard(); });

    // SetActiveAndUnbootable calls back into ReadCustom to read the flags, so we need to set them
    // here, even though we'll update them and call SetPartitionFlags again after.
    result = SetPartitionFlags(a_flags, b_flags);
    if (result.is_error()) {
      return result.take_error();
    }
    zx::result<uint64_t> new_a_flags = SetActiveAndUnbootable(kAbrSlotIndexA, a_flags);
    if (new_a_flags.is_error()) {
      return new_a_flags.take_error();
    }
    zx::result<uint64_t> new_b_flags = SetActiveAndUnbootable(kAbrSlotIndexB, b_flags);
    if (new_b_flags.is_error()) {
      return new_b_flags.take_error();
    }

    result = SetPartitionFlags(*new_a_flags, *new_b_flags);
    if (result.is_error()) {
      return result.take_error();
    }

    if (slot_switch) {
      zx::result result = SwapAbPartitionTypeGuids(new_slot_is_b);
      if (result.is_error()) {
        return result.take_error();
      }
      LOG("Switching active slot from %s\n", new_slot_is_b ? "A to B" : "B to A");
    }
    discard_changes.cancel();
    return zx::ok();
  }

  struct GptEntryAttributes {
    static constexpr uint8_t kMoonflowerMaxPriority = 3;

    explicit GptEntryAttributes(uint64_t flags) : flags(flags) {}

    uint64_t flags;
    DEF_SUBFIELD(flags, 49, 48, priority);
    DEF_SUBBIT(flags, 50, active);
    DEF_SUBFIELD(flags, 53, 51, retry_count);
    DEF_SUBBIT(flags, 54, boot_success);
    DEF_SUBBIT(flags, 55, unbootable);
  };

  static bool DetectSlotSwitch(const AbrSlotData* old_slot, const AbrSlotData* new_slot) {
    return new_slot->priority == kAbrMaxPriority &&
           new_slot->tries_remaining == kAbrMaxTriesRemaining && !new_slot->successful_boot &&
           old_slot->priority < kAbrMaxPriority && old_slot->successful_boot;
  }

  static uint64_t ToMoonflower(const AbrSlotData& current, const AbrSlotData& alternative,
                               uint64_t gpt_entry_flags) {
    // The priority field in Moonflower is only 2 bits wide (max value 3). Normalize
    // AbrSlotData::priority while maintaining the slots' relative priority.
    const uint8_t moonflower_priority = current.priority >= alternative.priority
                                            ? GptEntryAttributes::kMoonflowerMaxPriority
                                            : GptEntryAttributes::kMoonflowerMaxPriority - 1;

    GptEntryAttributes attributes(gpt_entry_flags);
    attributes.set_priority(moonflower_priority)
        .set_retry_count(current.tries_remaining)  // Both fields are 3 bits wide.
        .set_boot_success(current.successful_boot);
    return attributes.flags;
  }

  zx::result<uint64_t> SetActiveAndUnbootable(AbrSlotIndex slot_index, uint64_t gpt_entry_flags) {
    // Use GetSlotInfo's logic of determining is_active/is_bootable.
    zx::result<AbrSlotInfo> slot_info = GetSlotInfo(slot_index);
    if (slot_info.is_error()) {
      ERROR("Failed to get info for slot %s: %s\n",
            slot_index == kAbrSlotIndexA ? "A" : (slot_index == kAbrSlotIndexB ? "B" : "R"),
            slot_info.status_string());
      return slot_info.take_error();
    }

    GptEntryAttributes attributes(gpt_entry_flags);
    attributes.set_active(slot_info->is_active).set_unbootable(!slot_info->is_bootable);
    return zx::ok(attributes.flags);
  }

  static AbrSlotData ToFuchsia(uint64_t gpt_entry_flags) {
    const GptEntryAttributes attributes(gpt_entry_flags);

    AbrSlotData abr_slot_data = {};
    abr_slot_data.priority = static_cast<uint8_t>(attributes.priority());
    // Both fields are 3 bits wide.
    abr_slot_data.tries_remaining = static_cast<uint8_t>(attributes.retry_count());
    abr_slot_data.successful_boot = static_cast<uint8_t>(attributes.boot_success());
    return abr_slot_data;
  }

  const MoonflowerPartitioner* partitioner_;
  std::unique_ptr<BlockPartitionClient> zircon_a_;
  std::unique_ptr<BlockPartitionClient> zircon_b_;
  fidl::WireSyncClient<fuchsia_storage_partitions::PartitionsManager> partitions_manager_;
  zx::eventpair transaction_;
  std::optional<uint64_t> pending_zircon_a_flags_;
  std::optional<uint64_t> pending_zircon_b_flags_;
};

zx::result<std::unique_ptr<abr::Client>> MoonflowerPartitioner::CreateAbrClient() const {
  // A/B management on moonflower requires storage host APIs for GPT manipulation.
  if (!gpt_->devices().IsStorageHost()) {
    ERROR(
        "Moonflower A/B slots requires the product assembly to be configured with"
        " `storage_host_enabled` set to true in the `storage` configuration");
    ERROR("This is the default for moonflower, it is likely you have locally disabled it");
    ERROR("This device will need to be updated via fastboot instead");
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  zx::result result = MoonflowerAbrClient::Create(this);
  if (result.is_error()) {
    ERROR("Failed to create MoonflowerAbrClient: %s\n", result.status_string());
  }
  return result;
}

}  // namespace paver
