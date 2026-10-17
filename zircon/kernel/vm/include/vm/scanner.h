// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_SCANNER_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_SCANNER_H_

#include <sys/types.h>

#include <fbl/macros.h>
#include <vm/evictor.h>
#include <vm/page_queues.h>

// Increase the disable count of the scanner. This may need to block until the scanner finishes any
// current work and so should not be called with other locks held that may conflict with the
// scanner. Generally this is expected to be used by unittests.
void scanner_push_disable_count();

// Decrease the disable count of the scanner, potentially re-enabling the scanner if it reaches
// zero.
void scanner_pop_disable_count();

// Ask the scanner to dump informational state before issuing a panic. The assumption on calling
// this is that the panic is due to eviction/scanner going wrong for some hard to infer reason at
// the panic site, and this method can attempt to provide some additional context in the kernel log.
// This should only be called if a panic is expected as this performs semi-unsafe operations that
// might themselves crash or panic the kernel.
void scanner_debug_dump_state_before_panic();

// AutoVmScannerDisable is an RAII helper for disabling scanning using the
// scanner_push_disable_count()/scanner_pop_disable_count(). Disabling the scanner is useful in test
// code where it is not possible or practical to hold locks to prevent the scanner from taking
// actions.
class AutoVmScannerDisable {
 public:
  AutoVmScannerDisable() { scanner_push_disable_count(); }
  ~AutoVmScannerDisable() { scanner_pop_disable_count(); }

  DISALLOW_COPY_ASSIGN_AND_MOVE(AutoVmScannerDisable);
};

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_SCANNER_H_
