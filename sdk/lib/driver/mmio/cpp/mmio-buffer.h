// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_DRIVER_MMIO_CPP_MMIO_BUFFER_H_
#define LIB_DRIVER_MMIO_CPP_MMIO_BUFFER_H_

#ifdef __cplusplus

#include <lib/driver/mmio/cpp/mmio-ops.h>
#include <lib/driver/mmio/cpp/mmio-pinned-buffer.h>
#include <lib/mmio-ptr/mmio-ptr.h>
#include <lib/zx/bti.h>
#include <lib/zx/pmt.h>
#include <lib/zx/resource.h>
#include <lib/zx/result.h>
#include <lib/zx/vmar.h>
#include <lib/zx/vmo.h>
#include <stdint.h>
#include <zircon/assert.h>
#include <zircon/compiler.h>
#include <zircon/process.h>
#include <zircon/syscalls.h>

#include <optional>
#include <utility>

namespace fdf {

// Forward declaration.
class MmioView;

// MmioBuffer represents and manages a mapped MMIO region.
class MmioBuffer {
  friend class MmioView;
  friend MmioBuffer SetMmioBufferOps(MmioBuffer&& buffer, const MmioBufferOps* ops,
                                     const void* ctx);

 public:
  MmioBuffer(const MmioBuffer&) = delete;
  MmioBuffer& operator=(const MmioBuffer&) = delete;

  // Returns the default MmioBufferOps instance.
  static const MmioBufferOps* GetDefaultOps();

  // Returns an MmioBufferOps table whose writes omit the trailing hardware memory barrier, for
  // drivers that order accesses themselves.
  static const MmioBufferOps* GetUnsynchronizedOps();

  // Adopts an existing mapping of |vmo| at |vaddr|. The buffer takes ownership of the mapping and
  // unmaps it from the root VMAR on destruction or reset(). If |vmo| is invalid, the buffer does
  // not own the mapping and never unmaps it. When |ops| is null, the default ops are used and |ctx|
  // is ignored.
  MmioBuffer(zx::vmo vmo, MMIO_PTR void* vaddr, zx_off_t offset, size_t size,
             const MmioBufferOps* ops = GetDefaultOps(), const void* ctx = nullptr)
      : vmo_(std::move(vmo)),
        vaddr_(vaddr),
        offset_(offset),
        size_(size),
        ops_(ops ? ops : GetDefaultOps()),
        ctx_(ctx) {
    ZX_ASSERT(vaddr_ != nullptr);
  }

  virtual ~MmioBuffer() { reset(); }

  MmioBuffer(MmioBuffer&& other) { transfer(std::move(other)); }

  MmioBuffer& operator=(MmioBuffer&& other) {
    if (this != &other) {
      reset();
      transfer(std::move(other));
    }
    return *this;
  }

  // Maps |size| bytes of |vmo| starting at |offset| into the root VMAR. |offset| is the offset from
  // the beginning of |vmo| where the mmio region begins. |size| specifies the size of the mmio
  // region. |offset| + |size| must be less than or equal to the size of |vmo|.
  static zx::result<MmioBuffer> Create(zx_off_t offset, size_t size, zx::vmo vmo,
                                       uint32_t cache_policy) {
    zx_info_vmo_t info = {};
    zx_status_t status = vmo.get_info(ZX_INFO_VMO, &info, sizeof(info), nullptr, nullptr);
    if (status != ZX_OK) {
      return zx::error(status);
    }

    // |zx_vmo_set_cache_policy| will always return an error if it encounters a
    // VMO that has already been mapped. To enable tests where a VMO may be mapped
    // and modified already by a test fixture we only set the cache policy of a
    // provided VMO if the requested cache policy does not match the VMO's current
    // cache policy.
    if (info.cache_policy != cache_policy) {
      status = vmo.set_cache_policy(cache_policy);
      if (status != ZX_OK) {
        return zx::error(status);
      }
    }

    uint64_t result = 0;
    if (add_overflow(offset, size, &result) || result > info.size_bytes) {
      return zx::error(ZX_ERR_OUT_OF_RANGE);
    }
    if (size == 0) {
      return zx::error(ZX_ERR_INVALID_ARGS);
    }

    uintptr_t vaddr;
    const PageAlignedRange range = GetPageAlignedRange(offset, size);

    status = zx::vmar::root_self()->map(ZX_VM_PERM_READ | ZX_VM_PERM_WRITE | ZX_VM_MAP_RANGE, 0,
                                        vmo, range.vmo_offset, range.vmo_size, &vaddr);
    if (status != ZX_OK) {
      return zx::error(status);
    }

    return zx::ok(MmioBuffer(
        std::move(vmo), reinterpret_cast<MMIO_PTR void*>(vaddr + range.page_offset), offset, size));
  }

  void reset() {
    if (vaddr_ != nullptr && vmo_.is_valid()) {
      const PageAlignedRange range = GetPageAlignedRange(offset_, size_);
      zx::vmar::root_self()->unmap(reinterpret_cast<uintptr_t>(vaddr_) - range.page_offset,
                                   range.vmo_size);
      vmo_.reset();
    }
    unowned_vmo_ = zx::unowned_vmo();
    vaddr_ = nullptr;
    offset_ = 0;
    size_ = 0;
  }

  MMIO_PTR void* get() const { return vaddr_; }
  zx_off_t get_offset() const { return offset_; }
  size_t get_size() const { return size_; }
  zx::unowned_vmo get_vmo() const {
    if (vmo_.is_valid()) {
      return zx::unowned_vmo(vmo_);
    }
    return zx::unowned_vmo(unowned_vmo_);
  }

  zx_status_t Pin(const zx::bti& bti, std::optional<MmioPinnedBuffer>* pinned_buffer) {
    auto result = Pin(bti);
    if (result.is_ok()) {
      *pinned_buffer = std::move(result.value());
    }
    return result.status_value();
  }

  zx::result<MmioPinnedBuffer> Pin(const zx::bti& bti) {
    zx_paddr_t paddr;
    zx::pmt pmt;
    const uint32_t options = ZX_BTI_PERM_WRITE | ZX_BTI_PERM_READ | ZX_BTI_CONTIGUOUS;
    const PageAlignedRange range = GetPageAlignedRange(offset_, size_);

    zx_status_t status =
        bti.pin(options, *get_vmo(), range.vmo_offset, range.vmo_size, &paddr, 1, &pmt);
    if (status != ZX_OK) {
      return zx::error(status);
    }
    return zx::ok(MmioPinnedBuffer(paddr + range.page_offset, std::move(pmt)));
  }

  // Provides a slice view into the mmio.
  // The returned slice object must not outlive this object.
  MmioView View(zx_off_t off) const;
  MmioView View(zx_off_t off, size_t size) const;

  uint32_t ReadMasked32(uint32_t mask, zx_off_t offs) const {
    return ReadMasked<uint32_t>(mask, offs);
  }

  void ModifyBits32(uint32_t bits, uint32_t mask, zx_off_t offs) const {
    ModifyBits<uint32_t>(bits, mask, offs);
  }

  void ModifyBits32(uint32_t val, uint32_t start, uint32_t width, zx_off_t offs) const {
    ModifyBits<uint32_t>(val, start, width, offs);
  }

  void SetBits32(uint32_t bits, zx_off_t offs) const { SetBits<uint32_t>(bits, offs); }

  void ClearBits32(uint32_t bits, zx_off_t offs) const { ClearBits<uint32_t>(bits, offs); }

  void CopyFrom32(const MmioBuffer& source, zx_off_t source_offs, zx_off_t dest_offs,
                  size_t count) const {
    CopyFrom<uint32_t>(source, source_offs, dest_offs, count);
  }

  template <typename T>
  T Read(zx_off_t offs) const {
    if constexpr (sizeof(T) == sizeof(uint8_t)) {
      return Read8(offs);
    } else if constexpr (sizeof(T) == sizeof(uint16_t)) {
      return Read16(offs);
    } else if constexpr (sizeof(T) == sizeof(uint32_t)) {
      return Read32(offs);
    } else if constexpr (sizeof(T) == sizeof(uint64_t)) {
      return Read64(offs);
    } else {
      static_assert(false);
    }
  }

  template <typename T>
  T ReadMasked(T mask, zx_off_t offs) const {
    return (Read<T>(offs) & mask);
  }

  template <typename T>
  void CopyFrom(const MmioBuffer& source, zx_off_t source_offs, zx_off_t dest_offs,
                size_t count) const {
    for (size_t i = 0; i < count; i++) {
      T val = source.Read<T>(source_offs);
      Write<T>(val, dest_offs);
      source_offs = source_offs + sizeof(T);
      dest_offs = dest_offs + sizeof(T);
    }
  }

  template <typename T>
  void Write(T val, zx_off_t offs) const {
    if constexpr (sizeof(T) == sizeof(uint8_t)) {
      Write8(val, offs);
    } else if constexpr (sizeof(T) == sizeof(uint16_t)) {
      Write16(val, offs);
    } else if constexpr (sizeof(T) == sizeof(uint32_t)) {
      Write32(val, offs);
    } else if constexpr (sizeof(T) == sizeof(uint64_t)) {
      Write64(val, offs);
    } else {
      static_assert(false);
    }
  }

  template <typename T>
  void ModifyBits(T bits, T mask, zx_off_t offs) const {
    T val = Read<T>(offs);
    Write<T>(static_cast<T>((val & ~mask) | (bits & mask)), offs);
  }

  template <typename T>
  void SetBits(T bits, zx_off_t offs) const {
    ModifyBits<T>(bits, bits, offs);
  }

  template <typename T>
  void ClearBits(T bits, zx_off_t offs) const {
    ModifyBits<T>(0, bits, offs);
  }

  template <typename T>
  T GetBits(size_t shift, size_t count, zx_off_t offs) const {
    T mask = static_cast<T>(((static_cast<T>(1) << count) - 1) << shift);
    T val = Read<T>(offs);
    return static_cast<T>((val & mask) >> shift);
  }

  template <typename T>
  T GetBit(size_t shift, zx_off_t offs) const {
    return GetBits<T>(shift, 1, offs);
  }

  template <typename T>
  void ModifyBits(T bits, size_t shift, size_t count, zx_off_t offs) const {
    T mask = static_cast<T>(((static_cast<T>(1) << count) - 1) << shift);
    T val = Read<T>(offs);
    Write<T>(static_cast<T>((val & ~mask) | ((bits << shift) & mask)), offs);
  }

  template <typename T>
  void ModifyBit(bool val, size_t shift, zx_off_t offs) const {
    ModifyBits<T>(val, shift, 1, offs);
  }

  template <typename T>
  void SetBit(size_t shift, zx_off_t offs) const {
    ModifyBit<T>(true, shift, offs);
  }

  template <typename T>
  void ClearBit(size_t shift, zx_off_t offs) const {
    ModifyBit<T>(false, shift, offs);
  }

  uint8_t Read8(zx_off_t offs) const { return ops_->Read8(ctx_, *this, offs); }
  uint16_t Read16(zx_off_t offs) const { return ops_->Read16(ctx_, *this, offs); }
  uint32_t Read32(zx_off_t offs) const { return ops_->Read32(ctx_, *this, offs); }
  uint64_t Read64(zx_off_t offs) const { return ops_->Read64(ctx_, *this, offs); }

  // Read `size` bytes from the MmioBuffer into `buffer`. There are no access width guarantees
  // when using this operation and must only be used with devices where arbitrary access widths are
  // supported.
  void ReadBuffer(zx_off_t offs, void* buffer, size_t size) const {
    ops_->ReadBuffer(ctx_, *this, offs, buffer, size);
  }

  void Write8(uint8_t val, zx_off_t offs) const { ops_->Write8(ctx_, *this, val, offs); }
  void Write16(uint16_t val, zx_off_t offs) const { ops_->Write16(ctx_, *this, val, offs); }
  void Write32(uint32_t val, zx_off_t offs) const { ops_->Write32(ctx_, *this, val, offs); }
  void Write64(uint64_t val, zx_off_t offs) const { ops_->Write64(ctx_, *this, val, offs); }

  // Write `size` bytes from `buffer` into the MmioBuffer. There are no access width guarantees
  // when using this operation and must only be used with devices where arbitrary access widths are
  // supported.
  void WriteBuffer(zx_off_t offs, const void* buffer, size_t size) const {
    ops_->WriteBuffer(ctx_, *this, offs, buffer, size);
  }

 protected:
  const MmioBufferOps* ops() const { return ops_; }
  const void* ctx() const { return ctx_; }
  zx::unowned_vmo& unowned_vmo() { return unowned_vmo_; }

 private:
  struct PageAlignedRange {
    size_t vmo_offset;
    size_t page_offset;
    size_t vmo_size;
  };
  static PageAlignedRange GetPageAlignedRange(zx_off_t offset, size_t size) {
    const size_t page_size = zx_system_get_page_size();
    const size_t vmo_offset = offset - (offset % page_size);
    const size_t page_offset = offset - vmo_offset;
    const size_t vmo_size = ((size + page_offset + page_size - 1) / page_size) * page_size;
    return {.vmo_offset = vmo_offset, .page_offset = page_offset, .vmo_size = vmo_size};
  }

  void transfer(MmioBuffer&& other) {
    vmo_ = std::move(other.vmo_);
    unowned_vmo_ = other.unowned_vmo_;
    vaddr_ = other.vaddr_;
    offset_ = other.offset_;
    size_ = other.size_;
    ops_ = other.ops_;
    ctx_ = other.ctx_;
    other.unowned_vmo_ = zx::unowned_vmo();
    other.vaddr_ = nullptr;
    other.offset_ = 0;
    other.size_ = 0;
  }

  zx::vmo vmo_;
  zx::unowned_vmo unowned_vmo_;
  MMIO_PTR void* vaddr_ = nullptr;
  zx_off_t offset_ = 0;
  size_t size_ = 0;
  const MmioBufferOps* ops_ = nullptr;
  const void* ctx_ = nullptr;
};

// Returns the MmioBuffer with its mapping retained but using the provided MmioBufferOps and
// context. Use this to mock out or observe MmioBuffer IO operations. A null |ops| selects the
// default ops. Meant for owning buffers; passing an MmioView slices it into a non-owning
// MmioBuffer.
inline MmioBuffer SetMmioBufferOps(MmioBuffer&& buffer,
                                   const MmioBufferOps* ops = MmioBuffer::GetDefaultOps(),
                                   const void* ctx = nullptr) {
  buffer.ops_ = ops ? ops : MmioBuffer::GetDefaultOps();
  buffer.ctx_ = ctx;
  return std::move(buffer);
}

}  // namespace fdf

#include <lib/driver/mmio/cpp/mmio-internal.h>

#endif  // __cplusplus

#endif  // LIB_DRIVER_MMIO_CPP_MMIO_BUFFER_H_
