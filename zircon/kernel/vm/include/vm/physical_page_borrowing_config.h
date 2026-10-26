// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_PHYSICAL_PAGE_BORROWING_CONFIG_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_PHYSICAL_PAGE_BORROWING_CONFIG_H_

#include <zircon/compiler.h>

#include <kernel/ffi.h>

__BEGIN_CDECLS

void rust_ppb_config_set_borrowing_on_mru_enabled(bool enabled);
void rust_ppb_config_set_loaning_enabled(bool enabled);
bool rust_ppb_config_is_loaning_enabled(void);

__END_CDECLS

// Allow the ppb kernel command to dynamically control whether physical page borrowing is enabled
// or disabled (for pager-backed VMOs only for now).
class PhysicalPageBorrowingConfig {
 public:
  PhysicalPageBorrowingConfig() = default;
  PhysicalPageBorrowingConfig(const PhysicalPageBorrowingConfig& to_copy) = delete;
  PhysicalPageBorrowingConfig(PhysicalPageBorrowingConfig&& to_move) = delete;
  PhysicalPageBorrowingConfig& operator=(const PhysicalPageBorrowingConfig& to_copy) = delete;
  PhysicalPageBorrowingConfig& operator=(PhysicalPageBorrowingConfig&& to_move) = delete;

  static PhysicalPageBorrowingConfig& Get() {
    static PhysicalPageBorrowingConfig config;
    return config;
  }

  // true - allow page borrowing when a page is logically moved to MRU queue
  // false - disallow page borrowing when a page is logically moved to MRU queue
  void set_borrowing_on_mru_enabled(bool enabled) {
    rust_ppb_config_set_borrowing_on_mru_enabled(enabled);
  }

  // true - decommitted contiguous VMO pages will decommit+loan the pages.
  // false - decommit of a contiguous VMO page zeroes instead of decommitting+loaning.
  void set_loaning_enabled(bool enabled) { rust_ppb_config_set_loaning_enabled(enabled); }
  bool is_loaning_enabled() { return rust_ppb_config_is_loaning_enabled(); }
};

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_PHYSICAL_PAGE_BORROWING_CONFIG_H_
