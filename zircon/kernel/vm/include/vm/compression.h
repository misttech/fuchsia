// Copyright 2022 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_COMPRESSION_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_COMPRESSION_H_

#include <debug.h>
#include <zircon/types.h>

#include <fbl/macros.h>
#include <fbl/ref_counted.h>
#include <fbl/ref_ptr.h>
#include <kernel/ffi.h>
#include <ktl/optional.h>
#include <vm/compressor.h>
#include <vm/page.h>
#include <vm/vm_page_list.h>

// Top level container for performing compression and is responsible for coordinating between the
// provided storage and compression strategies. Each `VmCompression` instance has its own
// `CompressedRef` namespace, and references are not transferable. Aside from testing it is expected
// that there be a single global instance.
//
// This also manages the `VmCompressor` instances that provide the state machine for a VMO to do
// compression. Currently only a single instance is supported limiting to one simultaneous
// compression.
class VmCompression final : public fbl::RefCounted<VmCompression>,
                            public fbl::Recyclable<VmCompression> {
 public:
  void fbl_recycle();

  using CompressedRef = VmPageOrMarker::ReferenceValue;

  // Decompresses and frees the provided reference into |page_dest| and |metadata_dest|. This cannot
  // fail and always produces kPageSize worth of data. After calling this the reference is no longer
  // valid.
  //
  // Note that the temporary reference may be passed into here, however the same locking
  // requirements as |MoveReference| must be observed.
  void Decompress(CompressedRef ref, void* page_dest, uint32_t* metadata_dest);

  // Free the compressed reference without decompressing it.
  //
  // Note that the temporary reference may be passed into here, however the same locking
  // requirements as |MoveReference| must be observed.
  void Free(CompressedRef ref);

  // Must be called if a reference is being moved in or from a VmPageList. Will return a nullopt if
  // the reference is safe to move without being converted to a page. Otherwise returns a vm_page_t
  // along with associated metadata which is now owned by the caller and should be used to replace
  // the reference before moving.
  //
  // For performance reasons the check is performed inline here, with the unlikely case handled by a
  // separate method.
  //
  // This may only be called if the VMO lock for the VMO that the temporary reference was placed
  // into is held.
  //
  // See |VmCompressor| for a full explanation of temporary references and why this is needed.
  using PageAndMetadata = VmCompressor::PageAndMetadata;
  ktl::optional<PageAndMetadata> MoveReference(CompressedRef ref) {
    if (unlikely(IsTempReference(ref))) {
      return MoveTempReference(ref);
    }
    return ktl::nullopt;
  }

  // Returns whether or not the provided reference is a temporary reference.
  //
  // See |VmCompressor| for a full explanation of temporary references.
  bool IsTempReference(const CompressedRef& ref) const {
    return ref.value() == kTempReferenceValue;
  }

  // Retrieve the metadata for the original page referred to by ref.
  //
  // This may only be called if the VMO lock for the VMO that ref was placed into is held.
  uint32_t GetMetadata(CompressedRef ref);

  // Set the metadata for the original page referred to by ref.
  //
  // This may only be called if the VMO lock for the VMO that ref was placed into is held.
  void SetMetadata(CompressedRef ref, uint32_t metadata);

  // An RAII wrapper around holding a locked reference to a VmCompressor.
  class CompressorGuard {
   public:
    ~CompressorGuard();
    CompressorGuard(const CompressorGuard&) = delete;
    CompressorGuard(CompressorGuard&&) = delete;
    CompressorGuard& operator=(const CompressorGuard&) = delete;
    CompressorGuard& operator=(CompressorGuard&&) = delete;

    // Return a reference to the VmCompressor. Reference must not outlive this object.
    VmCompressor& get();

   private:
    explicit CompressorGuard(const VmCompression* compression);
    friend VmCompression;

    [[maybe_unused]] OpaqueStorage<56, 8> storage_{};
  };

  // Retrieve a reference to a VmCompressor, wrapped in the RAII CompressorGuard. Once the
  // compressor is finished with it can be destroyed, which will release it for reuse.
  // This method may block until a compressor becomes available and callers should be prepared for
  // extended wait times.
  // The returned CompressorGuard must not outlive this object.
  CompressorGuard AcquireCompressor();

 private:
  VmCompression() = default;
  ~VmCompression() = default;
  DISALLOW_COPY_ASSIGN_AND_MOVE(VmCompression);

  // Currently only a single VmCompressor instance is supported, so only a single temporary
  // reference is needed.
  // It is the responsibility of each underlying storage to not return this.
  static constexpr uint64_t kTempReferenceValue = UINT32_MAX & ~BIT_MASK(CompressedRef::kAlignBits);

  // Internal helper to operate on the temporary references.
  PageAndMetadata MoveTempReference(CompressedRef ref);
};

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_COMPRESSION_H_
