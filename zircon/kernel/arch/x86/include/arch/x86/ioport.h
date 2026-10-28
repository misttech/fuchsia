// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_IOPORT_H_
#define ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_IOPORT_H_

#include <sys/types.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

#include <bitmap/rle-bitmap.h>
#include <kernel/spinlock.h>
#include <ktl/unique_ptr.h>

class IoBitmap {
 public:
  // Return the IoBitmap associated with the current thread, or nullptr if the
  // thread has no associated IoBitmap (such as idle threads).
  static IoBitmap* GetCurrent();

  ~IoBitmap();

  int SetIoBitmap(uint32_t port, uint32_t len, bool enable);

 private:
  // Task used for updating IO permissions on each CPU.
  static void UpdateTask(void* context);

  friend void x86_set_tss_io_bitmap(IoBitmap& bitmap);
  friend void x86_clear_tss_io_bitmap(IoBitmap& bitmap);

  ktl::unique_ptr<bitmap::RleBitmap> bitmap_;
  DECLARE_SPINLOCK(IoBitmap) lock_;
};

__BEGIN_CDECLS

IoBitmap* cpp_io_bitmap_get_current();
zx_status_t cpp_io_bitmap_set_io_bitmap(IoBitmap* bitmap, uint32_t port, uint32_t len, bool enable);

__END_CDECLS

#endif  // ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_IOPORT_H_
