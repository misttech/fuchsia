// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_PMM_CHECKER_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_PMM_CHECKER_H_

#include <lib/boot-options/boot-options.h>
#include <lib/page/size.h>
#include <stdint.h>
#include <stdio.h>

#include <ktl/atomic.h>
#include <ktl/optional.h>
#include <vm/page.h>

// |PmmChecker| is used to detect memory corruption.  It is logically part of |PmmNode|.
//
// Usage is as follows:
//
//   PmmChecker checker;
//
//   // Check only the first 16 bytes of each page.
//   checker.SetFillSize(16);
//
//   // For all free pages...
//   for (...) {
//     checker.FillPattern(page);
//   }
//
//   // Now that all free pages have been filled with a pattern, we can arm the checker.
//   checker.Arm();
//   ...
//   checker.AssertPattern(page);
//
class PmmChecker {
 public:
  // See //zircon/kernel/lib/boot-options/include/lib/boot-options/options.inc
  // for default value.
  static constexpr CheckFailAction kDefaultAction = BootOptions{}.pmm_checker_action;
  // kDefaultAction as an integer, which bindgen can turn into a Rust constant.
  static constexpr uint8_t kDefaultActionRaw = static_cast<uint8_t>(kDefaultAction);

  // Returns true if |fill_size| is a valid value.  Valid values are mutliples of 8 between 8 and
  // kPageSize, inclusive.
  static bool IsValidFillSize(size_t fill_size);

  // Returns true if armed.
  bool IsArmed() const;

  void PrintStatus(FILE* f) const;

 private:
  // Only accessed by Rust after construction.
  [[maybe_unused]] size_t fill_size_ = kPageSize;
  [[maybe_unused]] CheckFailAction action_ = kDefaultAction;
  [[maybe_unused]] bool armed_ = false;
};

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_PMM_CHECKER_H_
