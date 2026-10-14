// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <cstddef>
#include <cstdint>

#include <efi/types.h>
#include <gbl/gbl_efi_protocols.h>
#include <gbl/uefi/gbl_protocol_utils.h>
#include <gbl/uefi/protocols/gbl_efi_avb_protocol.h>
#include <phys/efi/main.h>

namespace {

EfiStatus ReadPartitionAttributes(struct GblEfiAvbProtocol* self,
                                  /* in-out */ size_t* num_partitions,
                                  /* in-out */ GblEfiAvbPartitionAttributes* partitions) {
  if (!num_partitions) {
    return EFI_INVALID_PARAMETER;
  }
  *num_partitions = 0;
  return EFI_SUCCESS;
}

EfiStatus ReadDeviceStatus(struct GblEfiAvbProtocol* self,
                           /* out */ GblEfiAvbDeviceStatus* status_flags) {
  if (!status_flags) {
    return EFI_INVALID_PARAMETER;
  }

  // TODO(https://fxbug.dev/573312423): Support full device lock state.
  *status_flags = GBL_EFI_AVB_DEVICE_STATUS_UNLOCKED;
  return EFI_SUCCESS;
}

EfiStatus ValidateVbmetaPublicKey(struct GblEfiAvbProtocol* self,
                                  /* in */ size_t public_key_length,
                                  /* in */ const uint8_t* public_key_data,
                                  /* in */ size_t public_key_metadata_length,
                                  /* in */ const uint8_t* public_key_metadata,
                                  /* out */ GblEfiAvbKeyValidationStatus* validation_status) {
  if (!validation_status) {
    return EFI_INVALID_PARAMETER;
  }
  return EFI_UNSUPPORTED;
}

EfiStatus ReadRollbackIndex(struct GblEfiAvbProtocol* self,
                            /* in */ size_t index_location,
                            /* out */ uint64_t* rollback_index) {
  if (!rollback_index) {
    return EFI_INVALID_PARAMETER;
  }
  *rollback_index = 0;
  return EFI_SUCCESS;
}

EfiStatus WriteRollbackIndex(struct GblEfiAvbProtocol* self,
                             /* in */ size_t index_location,
                             /* in */ uint64_t rollback_index) {
  return EFI_SUCCESS;
}

EfiStatus ReadPersistentValue(struct GblEfiAvbProtocol* self,
                              /* in */ const EfiChar8* name,
                              /* in-out */ size_t* value_size,
                              /* out */ uint8_t* value) {
  if (!name || !value_size) {
    return EFI_INVALID_PARAMETER;
  }
  return EFI_UNSUPPORTED;
}

EfiStatus WritePersistentValue(struct GblEfiAvbProtocol* self,
                               /* in */ const EfiChar8* name,
                               /* in */ size_t value_size,
                               /* in */ const uint8_t* value) {
  if (!name) {
    return EFI_INVALID_PARAMETER;
  }
  return EFI_UNSUPPORTED;
}

EfiStatus HandleVerificationResult(struct GblEfiAvbProtocol* self,
                                   /* in */ const GblEfiAvbVerificationResult* result) {
  if (!result) {
    return EFI_INVALID_PARAMETER;
  }
  return EFI_SUCCESS;
}

EfiStatus WriteLockState(struct GblEfiAvbProtocol* self,
                         /* in */ GblEfiAvbLockType type,
                         /* in */ GblEfiAvbLockState state) {
  return EFI_UNSUPPORTED;
}

EfiStatus FactoryDataReset(struct GblEfiAvbProtocol* self) { return EFI_UNSUPPORTED; }

GblEfiAvbProtocol protocol = {
    .revision = GBL_EFI_AVB_PROTOCOL_REVISION,
    .read_partition_attributes = ReadPartitionAttributes,
    .read_device_status = ReadDeviceStatus,
    .validate_vbmeta_public_key = ValidateVbmetaPublicKey,
    .read_rollback_index = ReadRollbackIndex,
    .write_rollback_index = WriteRollbackIndex,
    .read_persistent_value = ReadPersistentValue,
    .write_persistent_value = WritePersistentValue,
    .handle_verification_result = HandleVerificationResult,
    .write_lock_state = WriteLockState,
    .factory_data_reset = FactoryDataReset,
};

efi_guid guid = GBL_EFI_AVB_PROTOCOL_GUID;

}  // namespace

namespace gigaboot {

efi_status InstallGblEfiAvbProtocol() {
  efi_handle out_handle = nullptr;
  return gEfiSystemTable->BootServices->InstallMultipleProtocolInterfaces(&out_handle, &guid,
                                                                          &protocol, NULL);
}

}  // namespace gigaboot
