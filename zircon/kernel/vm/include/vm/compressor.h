// Copyright 2022 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_COMPRESSOR_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_COMPRESSOR_H_

#include <ktl/variant.h>
#include <vm/vm_page_list.h>

// The VmCompressor represents a single instance of a compression operation that can be performed
// with a minimal locking protocol between a VMO page owner, and the compressor. There are two
// different actors that may access and interact with the compressor:
//  1. The owner of the VmCompressor instance.
//  2. The holder of the lock to the VMO that owns the temporary reference from this VmCompressor.
//
// The temporary reference exists to allow for the, hopefully unlikely, scenario where another
// thread wants access to the page whilst compression is in progress, and it works as follows:
//
//  * With the VMO lock held, a vm_page_t in a page list is replaced with the temporary reference
//     of the VmCompressor.
//
// At this point the VmCompressor owns the vm_page_t, but the VMO owns the temporary reference. From
// here any reference to a VMO or the VMO lock refers to the owner of the temporary reference.
//
//  * With the VMO lock *not* held the data in the page is compressed resulting in success or
//   failure.
//
// During this process the VMO lock was not held, allowing another thread to lookup this page and
// find the temporary reference. If the temporary reference is found it can treat it like any other
// compressed reference and decompress it. Decompression of the temporary reference does not have
// to synchronize on the compression attempt, rather it can just copy page (since the compressor
// does not modify it).
//
// Decompression is free to read the page, knowing it cannot be changed, due to holding the VMO
// lock. This is because for the original thread to progress after performing the compress step it
// must.
//
//  * Acquire the VMO lock, see if the temporary reference is still there, and resolve any success
//    or failure of the compression.
//
// In this case if the temporary reference is still there then it is replaced with the compression
// result (either a compress reference if it succeed, or the original page again if it failed).
//
// As we are holding the VMO lock we know that either the temporary reference is there, and we hold
// the lock, so no one else can be using it, or the temporary reference is not there and so it must
// have been returned, and so no on else can be using it. Therefore the compressor can be reset.
//
// In order to know that the temporary reference is no longer in use we require that it not move
// between slots in the page list (or between different page lists). This represents the one time
// where a compressed reference must be eagerly decompressed even if the data is not being accessed.
//
// This process of using a temporary reference that can resolve to performing a parallel copy
// provides some important properties:
//  1. Requesting a page blocks either on decompression, or a memcpy, but never compression.
//  2. Compression is performed on an owned page, and not on a page still in the VmPageList,
//     ensuring that the compression algorithm does not have to tolerate mutations during
//     compression.
//
//
// The VmCompressor also helps to manage the update of metadata associated with the vm_page_t being
// compressed. The owner of the VmCompressor provides the current value of this metadata to |Start|
// along with the vm_page_t.
//
// The holder of the lock to the VMO that owns the temporary reference may update this metadata at
// any time after |Start| is called and prior to |TakeCompressionResult| being called.
//
// Only the lock holder may call |TakeCompressionResult|, so at this time the VmCompressor will
// ensure that the final compressed reference points to the updated metadata (when compression
// succeeds) or the updated metadata is returned to the caller (when compression fails).
//
// The updated metadata is also returned to the caller when decompressing a page. In both the cases
// of compression failure or active decompression, it is the caller's responsibiliy to handle the
// updated metadata in whatever way it deems appropriate. The meaning of the metadata is opaque to
// the VmCompressor.
class VmCompressor {
 public:
  VmCompressor() = delete;
  ~VmCompressor() = delete;
  VmCompressor(const VmCompressor&) = delete;
  VmCompressor& operator=(const VmCompressor&) = delete;

  // Convenience struct to contain both a page and its associated metadata.
  struct PageAndMetadata {
    vm_page_t* page = nullptr;
    uint32_t metadata = 0;
  };

  // Possible results of a compression attempt.  See |TakeCompressionResult| for details.
  using CompressedRef = VmPageOrMarker::ReferenceValue;
  struct FailTag {
    PageAndMetadata src_page;
  };
  struct ZeroTag {};
  using CompressResult = ktl::variant<CompressedRef, ZeroTag, FailTag>;

  // Arms the compressor, ensuring the backup page is allocated. This must be called prior to
  // |Start|.
  zx_status_t Arm();

  // Start the compression process. Gives ownership of the page to the compressor, and returns the
  // temporary compression reference that should be installed in the page list in its place. |Arm|
  // must be called prior to calling |Start|.
  //
  // The caller may update the metadata associated with the page at any time after calling |Start|
  // and prior to calling |TakeCompressionResult|.
  CompressedRef Start(PageAndMetadata src_page);

  // Perform compression. |Start| must have been called prior, and this may be called without any
  // other locks held.
  void Compress();

  // Takes the result of the prior compression attempt and ensures that page metadata is up-to-date.
  // This function may only be called by the holder of the VMO lock, and |Compress| must be called
  // prior to calling |TakeCompressionResult|.
  //
  // The caller must ensure that there was a prior compression attempt, and thus a result to take.
  //
  // This returns one of:
  //  CompressedRef - Compression was successful and the provided reference can be passed to
  //                  |Decompress| or |Free|.
  //  ZeroTag - Input was the zero page. The compressor is not required to detect zero pages, and
  //            the absence of this value should not be used to assume the input was not zero.
  //  FailTag - Input could not be compressed or stored. The FailTag contains the original source
  //            page provided to |Start| along with the most up-to-date metadata for that page (as
  //            the metadata may have been altered while compression was in-progress).
  //
  // If compression succeeded, the returned |CompressedRef| will refer to the metadata for the
  // compressed page, including any metadata updates the caller made after calling |Start| but
  // before calling |TakeCompressionResult|.
  // If compression failed, the returned |FailTag| will contain the original source page and the
  // up-to-date metadata.
  CompressResult TakeCompressionResult();

  // Indicates that the temporary reference is no longer in use, and the compressor is now able to
  // be re-armed.
  void Finalize();

  // Convenience method to free unused compressed references returned from |Compress|. A reference
  // from |Compress| might be unused if, after reacquiring locks, a race has occurred and the
  // compression result is stale.
  void Free(CompressedRef ref);

  // Return the temporary reference, indicating it is no longer in use.
  void ReturnTempReference(CompressedRef ref);

  // Tests whether the supplied reference is the temporary reference from this compressor.
  bool IsTempReference(CompressedRef ref) const;

  // Returns whether the temporary reference is currently in use or not. This is different IsIdle,
  // as compression could be in progress, but the temporary reference already returned due to a
  // race.
  //
  // Calling this has the same locking requirements as the |spare_page_| member, whether either the
  // compressor needs to be idle, or the VMO lock must be held.
  bool IsTempReferenceInUse() const;
};

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_COMPRESSOR_H_
