// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_DRIVER_MMIO_CPP_MMIO_INTERNAL_H_
#define LIB_DRIVER_MMIO_CPP_MMIO_INTERNAL_H_

// This header is included by mmio-buffer.h and is not meant to be included directly.
#include <lib/mmio-ptr/mmio-ptr.h>
#include <stddef.h>
#include <stdint.h>
#include <zircon/assert.h>
#include <zircon/types.h>

#include "mmio-ops.h"

// Tests may predefine mmio_hw_mb() to observe barriers. This header only #undefs the macro
// if it defined it. The override is valid only in a single-translation-unit test binary.
#ifndef mmio_hw_mb
#define FDF_DEFINED_MMIO_HW_MB 1
#if defined(__aarch64__)
#define mmio_hw_mb() __asm__ volatile("dmb sy" : : : "memory")
#elif defined(__x86_64__)
#define mmio_hw_mb() __asm__ volatile("mfence" ::: "memory")
#elif defined(__riscv)
#define mmio_hw_mb() __asm__ volatile("fence iorw,iorw" ::: "memory")
#endif
#endif

namespace fdf::internal {

template <typename T>
inline MMIO_PTR volatile T* GetAddr(const void* ctx, const MmioBuffer& mmio, zx_off_t offs,
                                    size_t access_size = sizeof(T)) {
  ZX_ASSERT(offs <= mmio.get_size() && access_size <= mmio.get_size() - offs);
  const uintptr_t ptr = reinterpret_cast<uintptr_t>(mmio.get());
  ZX_ASSERT(ptr);
  return reinterpret_cast<MMIO_PTR T*>(ptr + offs);
}

inline uint8_t Read8(const void* ctx, const MmioBuffer& mmio, zx_off_t offs) {
  return MmioRead8(GetAddr<uint8_t>(ctx, mmio, offs));
}

inline uint16_t Read16(const void* ctx, const MmioBuffer& mmio, zx_off_t offs) {
  return MmioRead16(GetAddr<uint16_t>(ctx, mmio, offs));
}

inline uint32_t Read32(const void* ctx, const MmioBuffer& mmio, zx_off_t offs) {
  return MmioRead32(GetAddr<uint32_t>(ctx, mmio, offs));
}

inline uint64_t Read64(const void* ctx, const MmioBuffer& mmio, zx_off_t offs) {
  return MmioRead64(GetAddr<uint64_t>(ctx, mmio, offs));
}

inline void ReadBuffer(const void* ctx, const MmioBuffer& mmio, zx_off_t offs, void* buffer,
                       size_t size) {
  MmioReadBuffer(buffer, GetAddr<uint8_t>(ctx, mmio, offs, size), size);
}

inline void Write8Unsynchronized(const void* ctx, const MmioBuffer& mmio, uint8_t val,
                                 zx_off_t offs) {
  MmioWrite8(val, GetAddr<uint8_t>(ctx, mmio, offs));
}

inline void Write8(const void* ctx, const MmioBuffer& mmio, uint8_t val, zx_off_t offs) {
  Write8Unsynchronized(ctx, mmio, val, offs);
  mmio_hw_mb();
}

inline void Write16Unsynchronized(const void* ctx, const MmioBuffer& mmio, uint16_t val,
                                  zx_off_t offs) {
  MmioWrite16(val, GetAddr<uint16_t>(ctx, mmio, offs));
}

inline void Write16(const void* ctx, const MmioBuffer& mmio, uint16_t val, zx_off_t offs) {
  Write16Unsynchronized(ctx, mmio, val, offs);
  mmio_hw_mb();
}

inline void Write32Unsynchronized(const void* ctx, const MmioBuffer& mmio, uint32_t val,
                                  zx_off_t offs) {
  MmioWrite32(val, GetAddr<uint32_t>(ctx, mmio, offs));
}

inline void Write32(const void* ctx, const MmioBuffer& mmio, uint32_t val, zx_off_t offs) {
  Write32Unsynchronized(ctx, mmio, val, offs);
  mmio_hw_mb();
}

inline void Write64Unsynchronized(const void* ctx, const MmioBuffer& mmio, uint64_t val,
                                  zx_off_t offs) {
  MmioWrite64(val, GetAddr<uint64_t>(ctx, mmio, offs));
}

inline void Write64(const void* ctx, const MmioBuffer& mmio, uint64_t val, zx_off_t offs) {
  Write64Unsynchronized(ctx, mmio, val, offs);
  mmio_hw_mb();
}

inline void WriteBufferUnsynchronized(const void* ctx, const MmioBuffer& mmio, zx_off_t offs,
                                      const void* buffer, size_t size) {
  MmioWriteBuffer(GetAddr<uint8_t>(ctx, mmio, offs, size), buffer, size);
}

inline void WriteBuffer(const void* ctx, const MmioBuffer& mmio, zx_off_t offs, const void* buffer,
                        size_t size) {
  WriteBufferUnsynchronized(ctx, mmio, offs, buffer, size);
  mmio_hw_mb();
}

inline constexpr MmioBufferOps kUnsynchronizedOps = {
    .Read8 = Read8,
    .Read16 = Read16,
    .Read32 = Read32,
    .Read64 = Read64,
    .ReadBuffer = ReadBuffer,
    .Write8 = Write8Unsynchronized,
    .Write16 = Write16Unsynchronized,
    .Write32 = Write32Unsynchronized,
    .Write64 = Write64Unsynchronized,
    .WriteBuffer = WriteBufferUnsynchronized,
};

inline constexpr MmioBufferOps kDefaultOps = [] {
  MmioBufferOps ops = kUnsynchronizedOps;
  ops.Write8 = Write8;
  ops.Write16 = Write16;
  ops.Write32 = Write32;
  ops.Write64 = Write64;
  ops.WriteBuffer = WriteBuffer;
  return ops;
}();

#ifdef FDF_DEFINED_MMIO_HW_MB
#undef mmio_hw_mb
#undef FDF_DEFINED_MMIO_HW_MB
#endif

}  // namespace fdf::internal

namespace fdf {
inline const MmioBufferOps* MmioBuffer::GetDefaultOps() { return &internal::kDefaultOps; }
inline const MmioBufferOps* MmioBuffer::GetUnsynchronizedOps() {
  return &internal::kUnsynchronizedOps;
}
}  // namespace fdf

#endif  // LIB_DRIVER_MMIO_CPP_MMIO_INTERNAL_H_
