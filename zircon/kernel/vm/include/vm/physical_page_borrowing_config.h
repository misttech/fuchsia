// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_PHYSICAL_PAGE_BORROWING_CONFIG_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_PHYSICAL_PAGE_BORROWING_CONFIG_H_

#include <ktl/atomic.h>

// Allow the ppb kernel command to dynamically control whether physical page borrowing is enabled
// or disabled (for pager-backed VMOs only for now).
class PhysicalPageBorrowingConfig {
 public:
  PhysicalPageBorrowingConfig() = default;
  PhysicalPageBorrowingConfig(const PhysicalPageBorrowingConfig& to_copy) = delete;
  PhysicalPageBorrowingConfig(PhysicalPageBorrowingConfig&& to_move) = delete;
  PhysicalPageBorrowingConfig& operator=(const PhysicalPageBorrowingConfig& to_copy) = delete;
  PhysicalPageBorrowingConfig& operator=(PhysicalPageBorrowingConfig&& to_move) = delete;

  static PhysicalPageBorrowingConfig& Get() { return instance_; }

  // true - allow page borrowing when a page is logically moved to MRU queue
  // false - disallow page borrowing when a page is logically moved to MRU queue
  void set_borrowing_on_mru_enabled(bool enabled) {
    borrowing_on_mru_enabled_.store(enabled, ktl::memory_order_relaxed);
  }
  bool is_borrowing_on_mru_enabled() {
    return borrowing_on_mru_enabled_.load(ktl::memory_order_relaxed);
  }

  // true - decommitted contiguous VMO pages will decommit+loan the pages.
  // false - decommit of a contiguous VMO page zeroes instead of decommitting+loaning.
  void set_loaning_enabled(bool enabled) {
    loaning_enabled_.store(enabled, ktl::memory_order_relaxed);
  }
  bool is_loaning_enabled() { return loaning_enabled_.load(ktl::memory_order_relaxed); }

  // true - loaned pages will be replaced with new page with copied contents.
  // false - loaned pages will be evicted.
  void set_replace_on_unloan_enabled(bool enabled) {
    replace_on_unloan_.store(enabled, ktl::memory_order_relaxed);
  }
  bool is_replace_on_unloan_enabled() { return replace_on_unloan_.load(ktl::memory_order_relaxed); }

 private:
  // Singleton.
  static PhysicalPageBorrowingConfig instance_;

  // Enable page borrowing when a page is logically moved to the MRU queue.  If true, replace an
  // accessed non-loaned page with loaned on access.  If false, this is disabled.
  ktl::atomic<bool> borrowing_on_mru_enabled_ = false;

  // Enable page loaning.  If false, no page loaning will occur.  If true, decommitting pages of a
  // contiguous VMO will loan the pages.  This can be dynamically changed, but changes will only
  // apply to subsequent decommit of contiguous VMO pages.
  ktl::atomic<bool> loaning_enabled_ = false;

  // Enables copy of page contents, instead of eviction, when a loaned page is committed back to its
  // contiguous owner.
  ktl::atomic<bool> replace_on_unloan_ = false;
};

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_PHYSICAL_PAGE_BORROWING_CONFIG_H_
