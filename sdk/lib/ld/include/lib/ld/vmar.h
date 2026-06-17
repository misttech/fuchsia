// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_LD_VMAR_H_
#define LIB_LD_VMAR_H_

#include <lib/zx/result.h>
#include <lib/zx/vmar.h>
#include <zircon/syscalls.h>

#include <utility>

namespace ld {

// Helper class to manage a VMAR reservation handle with strict ownership and RAII cleanup.
//
// Example usage:
//
//   zx_info_vmar_t info;
//   parent_vmar->get_info(ZX_INFO_VMAR, &info, sizeof(info), nullptr, nullptr);
//
//   ld::VmarReservation reservation;
//   // reserve_info specifies the bounds of the reservation VMAR.
//   if (auto res = reservation.Init(parent_vmar, info, reserve_info);
//       res.is_error()) {
//     return res;
//   }
//
//   // The reservation VMAR is cleanly destroyed automatically when reservation goes out of scope.
//
class VmarReservation {
 public:
  VmarReservation() = default;
  VmarReservation(VmarReservation&&) = default;

  // If we would take ownership of another vmar while this one has a valid vmar, we must explicitly
  // destroy it since the normal dtor of a zx::vmar won't destroy it.
  VmarReservation& operator=(VmarReservation&& other) {
    if (this != &other) {
      if (vmar_) {
        vmar_.destroy();
      }
      vmar_ = std::move(other.vmar_);
    }
    return *this;
  }

  ~VmarReservation() {
    if (vmar_) {
      vmar_.destroy();
    }
  }

  // Initialize the reservation by allocating a child VMAR in the parent covering the specified
  // bounds. The offset is computed from the absolute bases of the parent and the requested child
  // VMAR.
  zx::result<> Init(zx::unowned_vmar parent, zx_info_vmar_t parent_info,
                    zx_info_vmar_t reserve_info) {
    uintptr_t child_addr;
    uintptr_t offset = reserve_info.base - parent_info.base;
    return zx::make_result(
        parent->allocate(ZX_VM_SPECIFIC, offset, reserve_info.len, &vmar_, &child_addr));
  }

  const zx::vmar& vmar() const { return vmar_; }

  explicit operator bool() const { return vmar_.is_valid(); }

 private:
  zx::vmar vmar_;
};

static_assert(std::movable<VmarReservation>);
static_assert(!std::copyable<VmarReservation>);

}  // namespace ld

#endif  // LIB_LD_VMAR_H_
