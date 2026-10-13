// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_MSI_INTERRUPT_DISPATCHER_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_MSI_INTERRUPT_DISPATCHER_H_

#include <sys/types.h>
#include <zircon/types.h>

#include <cstddef>

#include <dev/interrupt.h>
#include <fbl/ref_ptr.h>
#include <object/handle.h>
#include <object/interrupt_dispatcher.h>
#include <object/msi_dispatcher.h>
#include <object/vm_object_dispatcher.h>
#include <vm/vm_address_region.h>
#include <vm/vm_aspace.h>

// Specify that we should create an MSIX backed interrupt and the vmo passed
// to zx_msi_create contains the table entries, not the device's MSI capability.
#define ZX_MSI_VALID_OPTIONS (ZX_MSI_MODE_MSI_X)

extern "C" zx_status_t rust_msi_interrupt_dispatcher_create(
    const fbl::RefPtr<MsiAllocation>* alloc, uint32_t msi_id, const fbl::RefPtr<VmObject>* vmo,
    zx_off_t cap_offset, uint32_t options, zx_rights_t* out_rights,
    KernelHandle<InterruptDispatcher>* out_interrupt,
    void (*register_int_fn)(const msi_block_t*, uint, interrupt_handler_t));

// The common interface for all MSI related interrupt handling. This encompasses MSI and MSI-X.
class MsiInterruptDispatcher final {
 public:
  using RegisterIntFn = void (*)(const msi_block_t*, uint, interrupt_handler_t);

  static zx_status_t Create(fbl::RefPtr<MsiAllocation> alloc, uint32_t msi_id,
                            const fbl::RefPtr<VmObject>& vmo, zx_off_t cap_offset, uint32_t options,
                            zx_rights_t* out_rights,
                            KernelHandle<InterruptDispatcher>* out_interrupt,
                            RegisterIntFn register_int_fn = msi_register_handler) {
    return rust_msi_interrupt_dispatcher_create(&alloc, msi_id, &vmo, cap_offset, options,
                                                out_rights, out_interrupt, register_int_fn);
  }
};

// Message Signaled Interrupts --
// This derived interrupt dispatcher handles operation of Messaged Signaled
// Interrupts (MSIs) and their associated interactions with userspace drivers.
// MSIs are allocated at the platform interrupt controller in contiguous blocks
// and then assigned as a group to a given PCI device. A PCI device may support
// 1 or more interrupts, and may or may not support masking of individual
// vectors. Operation of the MSI functionality is largely controlled in the
// device's capability space via its MSI Capability Structure. This includes
// enabling MSI, configuring vectors, and masking/unmasking vectors. To reduce
// interrupt latency all masking and unmasking at interrupt time is handled by
// this dispatcher, but all configuration is (will be: https://fxbug.dev/42108122) handled by the
// userspace PCI Bus Driver. To facilitate safe interactions between the two,
// all access to MSI configuration registers are synchronized via the MSI
// allocation lock. Userspace will rarely be accessing this outside of
// initialization so the performance overhead is minimal due to a lack of
// congestion and interrupts all being handled by the bootstrap CPU.
// Since the dispatcher only needs access to the Id register and Mask Bits register
// we are fortunately able to ignore the different formats due to the 32 bit and 64
// bit mask registers lining up.
// PCI Local Bus Specification rev 3.0 figure 6-9.
const uint8_t kMsiCapabilityId = (0x5);
const uint16_t kMsi64bitSupported = (1u << 7);
const uint16_t kMsiPvmSupported = (1u << 8);
struct MsiCapability {
  uint8_t id;
  uint8_t reserved0_;  // Next pointer
  uint16_t control;
  uint32_t reserved1_[2];  // For 32 bit this is Address, Data, and a reserved field.
                           // For 64 bit this is Address and Address Upper.
  uint32_t mask_bits_32;   // For 64 bit this is Data and a reserved field.
  uint32_t mask_bits_64;
  uint32_t reserved2_;  // Pending Bits
};
static_assert(offsetof(MsiCapability, mask_bits_32) == 0x0C);
static_assert(offsetof(MsiCapability, mask_bits_64) == 0x10);
static_assert(sizeof(MsiCapability) == 24);

// For MSI-X, the kernel only needs to interact with a given table entry for a specific vector.
// Furthermore, since each vector has its own entry and MsiInterruptDispatchers hold a reference to
// their allocation there is no need to lock any of the accesses. If the PCI bus driver wishes
// to disable MSI-X on the device function then it can do so with the function level disable
// in the capability before tearing down any interrupts.
//
// MSI-X table entries are covered in the PCI Local Bus Spec v3.0 section 6.8.2.7
struct MsixTableEntry {
  uint32_t msg_addr;
  uint32_t msg_upper_addr;
  uint32_t msg_data;
  uint32_t vector_control;
};

#define kMsixVectorControlMaskBit 0

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_MSI_INTERRUPT_DISPATCHER_H_
