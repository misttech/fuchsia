// Copyright 2022 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_DEBUG_COMPRESSOR_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_DEBUG_COMPRESSOR_H_

#include <zircon/types.h>

#include <object/opaque_storage.h>
#include <vm/vm_constants.h>

struct vm_page;
using vm_page_t = struct vm_page;
class VmCowPages;

// A debug compressor that can be given references to pages in VMOs and will randomly compress a
// subset of them. The compression will be performed in a difference Zircon thread, so the pages
// can be given with arbitrary locks held.
class VmDebugCompressor {
 public:
  VmDebugCompressor();
  ~VmDebugCompressor();

  // Initializes the debug compressor. This method may acquire Mutexes, and so must not be called
  // with any spinlocks held. Other methods should not be called before |Init| is called and returns
  // ZX_OK.
  zx_status_t Init();

  // Adds the specified |page| at |offset| in |object| to the debug compressor as a candidate for
  // compression.  The |page| and |object| must remain valid until |Add| returns. This implies that
  // |object| lock must be held, however this cannot be stated with an TA_REQ statement since
  // VmCowPages is not declared yet.
  void Add(vm_page_t* page, VmCowPages* object, uint64_t offset);

  // Pauses the debug compressor such that all future |Add| calls will be ignored. It is an error to
  // call |Pause| twice without calling |Resume| in between. Pause might acquire arbitrary VMO and
  // other locks and should not be called with other locks held.
  void Pause();

  // Resumes from a |Pause|, causing calls to |Add| to no longer be ignored. It is an error to call
  // |Resume| except after having called |Pause|.
  void Resume();

 private:
  OpaqueStorage<kVmDebugCompressorStorageSize, kVmDebugCompressorStorageAlign> storage_;
};

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_DEBUG_COMPRESSOR_H_
