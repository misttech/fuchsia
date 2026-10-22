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
#include <ktl/tuple.h>
#include <ktl/variant.h>
#include <vm/compressor.h>
#include <vm/page.h>
#include <vm/vm_page_list.h>

// Defines an allocator like interface for adding, removing and accessing compressed data.
// Instances of this class may be accessed concurrently.
class VmCompressedStorage : public fbl::RefCounted<VmCompressedStorage> {
 public:
  VmCompressedStorage() = default;
  virtual ~VmCompressedStorage() = default;

  using CompressedRef = VmPageOrMarker::ReferenceValue;

  // Attempts to store the data in |page| of size |len|. The data is assumed to start at offset 0 in
  // the page, and |len| cannot exceed kPageSize. The |page| becomes owned by this
  // VmCompressedStorage instance, although ownership may be returned via the return result.
  //
  // The return value is an optional reference to compressed data, as well as an optional vm_page_t,
  // with a nullptr for the vm_page_t being used to indicate absence. If a vm_page_t is returned the
  // caller owns it, and is responsible for freeing it.
  // If the data is stored successfully then a valid CompressedRef will be returned and will be
  // retained until that reference is passed to |Free|. Otherwise a nullopt is returned.
  //
  // Regardless of the success or failure of the storage, a vm_page_t might be returned, and if one
  // is returned it may or may not be the same as the |page| passed in. The returned vm_page_t has
  // undefined content and is now freely owned by the caller and can be used as the caller likes, or
  // returned to the pmm.
  // The expected usage of this is that a compressor puts data in a buffer page and passes it here
  // as |page|, then takes any returned vm_page_t as its next buffer page, or if none was returned
  // allocates a new one. This provides the storage implementation with the freedom to either copy
  // the data out of |page| and return it, or take the page to add to its storage instead of going
  // to the PMM (and then returning a nullptr).
  virtual ktl::pair<ktl::optional<CompressedRef>, vm_page_t*> Store(vm_page_t* page,
                                                                    size_t len) = 0;

  // Free the data referenced by a |CompressedRef| that was returned from |Store|. After calling
  // this the reference is no longer valid and must not be passed to |CompressedData|, and any
  // previous result of |CompressedData| must not be used.
  virtual void Free(CompressedRef ref) = 0;

  // Retrieve a reference to original data that was stored. The metadata and length of the data are
  // also returned, alleviating the need to retain them separately.
  //
  // The return address remains valid as long as this specific reference is not freed, and otherwise
  // any other calls to |Store| or |Free| do not invalidate.
  // TODO(https://fxbug.dev/42138396): Restrict this if de-fragmentation of the compressed storage
  // becomes necessary.
  //
  // No guarantee on alignment of the data is provided, and callers must tolerate arbitrary byte
  // alignment.
  // TODO(https://fxbug.dev/42138396): Consider providing an alignment guarantee if needed by
  // decompressors.
  virtual ktl::tuple<const void*, uint32_t, size_t> CompressedData(CompressedRef ref) const = 0;

  // Retrieve the metadata for the original page referred to by ref.
  virtual uint32_t GetMetadata(CompressedRef ref) = 0;

  // Set the metadata for the original page referred to by ref.
  virtual void SetMetadata(CompressedRef ref, uint32_t metadata) = 0;

  // Perform an information dump of the internal state to the debuglog.
  virtual void Dump() const = 0;

  struct MemoryUsage {
    uint64_t uncompressed_content_bytes = 0;
    uint64_t compressed_storage_bytes = 0;
    uint64_t compressed_storage_used_bytes = 0;
  };
  virtual MemoryUsage GetMemoryUsage() const = 0;
};

// Defines the interface for different compression algorithms.
// Instances of this class may be accessed concurrently.
class VmCompressionStrategy : public fbl::RefCounted<VmCompressionStrategy> {
 public:
  VmCompressionStrategy() = default;
  virtual ~VmCompressionStrategy() = default;

  // Attempt to compress the data at |src| into |dst|. The input data is assumed to be kPageSize in
  // length, and the amount of output data can be constrained with |dst_limit|. |Compress| will
  // never write more than |dst_limit| to |dst|.
  //
  // The return value is one of:
  //  size_t: Compression was successful and this value is <= |dst_limit| and represents the length
  //          of data stored to |dst|.
  //  ZeroTag: The input data was noticed to be all zeroes.
  //  FailTag: The input data could not be compressed within |dst_limit|.
  //
  // No guarantee on the alignment of |src| or |dst| is provided, and the caller is free to provide
  // pointers with arbitrary byte alignment.
  // TODO(https://fxbug.dev/42138396): Consider requiring an alignment if needed by compressors.
  using FailTag = VmCompressor::FailTag;
  using ZeroTag = VmCompressor::ZeroTag;
  using CompressResult = ktl::variant<size_t, ZeroTag, FailTag>;
  virtual CompressResult Compress(const void* src, void* dst, size_t dst_limit) = 0;

  // Decompress the data at |src| into |dst|. The input is assumed to have been produced by
  // |Compress| and this method cannot fail. Similar to the input of |Compress| being assumed to be
  // a page size, the |dst| output here is assumed to be of page size and will be exactly filled.
  //
  // No guarantee on the alignment of |src| or |dst| is provided, and the caller is free to provide
  // pointers with arbitrary byte alignment.
  // TODO(https://fxbug.dev/42138396): Consider requiring an alignment if needed by decompressors.
  virtual void Decompress(const void* src, size_t src_len, void* dst) = 0;

  // Perform an information dump of the internal state to the debuglog.
  virtual void Dump() const = 0;
};

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

  // Constructs a compression manager using the given storage and compression strategies. The
  // |compression_threshold| is the number of bytes above which a compression is considered to have
  // failed and should not be considered worth storing.
  // TODO(https://fxbug.dev/42138396): Limit total amount of pages stored.
  static fbl::RefPtr<VmCompression> Create(fbl::RefPtr<VmCompressedStorage> storage,
                                           fbl::RefPtr<VmCompressionStrategy> strategy,
                                           size_t compression_threshold);

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
