// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/driver/mmio/cpp/mmio-buffer.h>
#include <lib/driver/mmio/cpp/mmio.h>
#include <lib/driver/mmio/testing/cpp/test-helper.h>
#include <lib/fake-bti/bti.h>
#include <lib/zx/vmo.h>
#include <zircon/assert.h>
#include <zircon/errors.h>
#include <zircon/syscalls.h>
#include <zircon/types.h>

#include <limits>
#include <optional>

#include <zxtest/zxtest.h>

namespace {

zx::vmo CreateVmoWithPolicy(size_t size, std::optional<uint32_t> cache_policy) {
  zx::vmo vmo = {};
  zx_status_t status = zx::vmo::create(size, /*options=*/0, &vmo);
  ZX_ASSERT_MSG((status == ZX_OK), "creating vmo failed: %s", zx_status_get_string(status));
  if (cache_policy.has_value()) {
    status = vmo.set_cache_policy(*cache_policy);
    ZX_ASSERT_MSG((status == ZX_OK), "setting vmo cache policy failed: %s",
                  zx_status_get_string(status));
  }
  return vmo;
}

zx::vmo CreateVmo(size_t size) { return CreateVmoWithPolicy(size, std::nullopt); }

zx::vmo DuplicateVmo(const zx::vmo& vmo) {
  zx::vmo out_vmo = {};
  zx_status_t status = vmo.duplicate(ZX_RIGHT_SAME_RIGHTS, &out_vmo);
  ZX_ASSERT_MSG((status == ZX_OK), "duplicating vmo failed: %s", zx_status_get_string(status));
  return out_vmo;
}

size_t VmoNumMappings(const zx::vmo& vmo) {
  zx_info_vmo_t info;
  zx_status_t status = vmo.get_info(ZX_INFO_VMO, &info, sizeof(info), nullptr, nullptr);
  ZX_ASSERT_MSG((status == ZX_OK), "getting vmo info failed: %s", zx_status_get_string(status));
  return info.num_mappings;
}

size_t PinnedVmoCount(const zx::bti& bti) {
  size_t count = 0;
  zx_status_t status = fake_bti_get_pinned_vmos(bti.get(), nullptr, 0, &count);
  ZX_ASSERT_MSG((status == ZX_OK), "getting pinned vmos count failed: %s",
                zx_status_get_string(status));
  return count;
}

TEST(MmioBuffer, CInit) {
  const size_t vmo_sz = zx_system_get_page_size();
  zx::vmo vmo = CreateVmo(vmo_sz);
  mmio_buffer_t mb = {};

  // |buffer| is invalid.
  ASSERT_EQ(ZX_ERR_INVALID_ARGS, mmio_buffer_init(nullptr, 0, vmo_sz, DuplicateVmo(vmo).get(),
                                                  ZX_CACHE_POLICY_UNCACHED));
  // |offset is invalid.
  ASSERT_EQ(ZX_ERR_OUT_OF_RANGE,
            mmio_buffer_init(&mb, -1, vmo_sz, DuplicateVmo(vmo).get(), ZX_CACHE_POLICY_UNCACHED));
  ASSERT_EQ(ZX_ERR_OUT_OF_RANGE, mmio_buffer_init(&mb, vmo_sz + 1, vmo_sz, DuplicateVmo(vmo).get(),
                                                  ZX_CACHE_POLICY_UNCACHED));
  // |size| is invalid
  ASSERT_EQ(ZX_ERR_INVALID_ARGS,
            mmio_buffer_init(&mb, 0, 0, DuplicateVmo(vmo).get(), ZX_CACHE_POLICY_UNCACHED));
  ASSERT_EQ(ZX_ERR_OUT_OF_RANGE, mmio_buffer_init(&mb, 0, vmo_sz + 1, DuplicateVmo(vmo).get(),
                                                  ZX_CACHE_POLICY_UNCACHED));
  // |size| + |offset| are collectively invalid.
  ASSERT_EQ(ZX_ERR_OUT_OF_RANGE,
            mmio_buffer_init(&mb, vmo_sz + 1 / 2, vmo_sz / 2, DuplicateVmo(vmo).get(),
                             ZX_CACHE_POLICY_UNCACHED));

  // |vmo| is invalid
  ASSERT_EQ(ZX_ERR_BAD_HANDLE,
            mmio_buffer_init(&mb, 0, vmo_sz, ZX_HANDLE_INVALID, ZX_CACHE_POLICY_UNCACHED));
  // |cache_policy| is invalid.
  ASSERT_EQ(ZX_ERR_INVALID_ARGS,
            mmio_buffer_init(&mb, 0, vmo_sz, DuplicateVmo(vmo).get(), ZX_CACHE_POLICY_MASK + 1));

  ASSERT_OK(mmio_buffer_init(&mb, 0, vmo_sz, DuplicateVmo(vmo).get(), ZX_CACHE_POLICY_UNCACHED));
  mmio_buffer_release(&mb);
  ASSERT_EQ(mb.vmo, ZX_HANDLE_INVALID);
}

TEST(MmioBuffer, UnalignedOffsetInitRelease) {
  const size_t vmo_sz = zx_system_get_page_size();
  zx::vmo vmo = CreateVmo(vmo_sz);
  mmio_buffer_t mb = {};

  // Unaligned offset.
  ASSERT_OK(mmio_buffer_init(&mb, vmo_sz / 2, vmo_sz / 2, DuplicateVmo(vmo).get(),
                             ZX_CACHE_POLICY_UNCACHED));

  // Ensure the map was successful.
  ASSERT_EQ(1, VmoNumMappings(vmo));

  mmio_buffer_release(&mb);
  ASSERT_EQ(ZX_HANDLE_INVALID, mb.vmo);

  // Ensure the unmap was successful.
  ASSERT_EQ(0, VmoNumMappings(vmo));
}

TEST(MmioBuffer, UnalignedSizeInitRelease) {
  const size_t vmo_sz = zx_system_get_page_size();
  zx::vmo vmo = CreateVmo(vmo_sz);
  mmio_buffer_t mb = {};

  // Unaligned size.
  ASSERT_OK(
      mmio_buffer_init(&mb, 0, vmo_sz / 2, DuplicateVmo(vmo).get(), ZX_CACHE_POLICY_UNCACHED));

  // Ensure the map was successful.
  ASSERT_EQ(1, VmoNumMappings(vmo));

  mmio_buffer_release(&mb);
  ASSERT_EQ(ZX_HANDLE_INVALID, mb.vmo);

  // Ensure the unmap was successful.
  ASSERT_EQ(0, VmoNumMappings(vmo));
}

TEST(MmioBuffer, CppLifecycle) {
  const size_t vmo_sz = zx_system_get_page_size();
  zx::vmo vmo = CreateVmo(vmo_sz);
  MMIO_PTR volatile uint8_t* ptr = nullptr;
  {
    zx::result<fdf::MmioBuffer> mmio =
        fdf::MmioBuffer::Create(0, vmo_sz, std::move(vmo), ZX_CACHE_POLICY_UNCACHED_DEVICE);
    ASSERT_OK(mmio.status_value());
    ptr = reinterpret_cast<MMIO_PTR volatile uint8_t*>(mmio->get());
    // This write should succeed.
    MmioWrite8(0xA5, ptr);
  }

  // This should fault because the dtor of MmioBuffer unmapped the range.
  ASSERT_DEATH(([&ptr]() { MmioWrite8(0xA5, ptr); }));
}

TEST(MmioBuffer, MoveAssignment) {
  const size_t vmo_sz = zx_system_get_page_size();
  MMIO_PTR volatile uint8_t* ptr1 = nullptr;
  MMIO_PTR volatile uint8_t* ptr2 = nullptr;

  {
    zx::vmo vmo1 = CreateVmo(vmo_sz);
    zx::vmo vmo2 = CreateVmo(vmo_sz);
    auto mmio1 =
        fdf::MmioBuffer::Create(0, vmo_sz, std::move(vmo1), ZX_CACHE_POLICY_UNCACHED_DEVICE);
    auto mmio2 =
        fdf::MmioBuffer::Create(0, vmo_sz, std::move(vmo2), ZX_CACHE_POLICY_UNCACHED_DEVICE);
    ASSERT_OK(mmio1.status_value());
    ASSERT_OK(mmio2.status_value());

    ptr1 = reinterpret_cast<MMIO_PTR volatile uint8_t*>(mmio1->get());
    ptr2 = reinterpret_cast<MMIO_PTR volatile uint8_t*>(mmio2->get());

    // Move assign mmio2 into mmio1. mmio1's previous mapping should be unmapped.
    *mmio1 = std::move(*mmio2);

    // ptr2 should still be valid.
    MmioWrite8(0x5A, ptr2);
    EXPECT_EQ(0x5A, MmioRead8(ptr2));
  }

  // Both ptr1 and ptr2 should fault now after destruction.
  ASSERT_DEATH(([&ptr1]() { MmioWrite8(0xA5, ptr1); }));
  ASSERT_DEATH(([&ptr2]() { MmioWrite8(0xA5, ptr2); }));
}

TEST(MmioBuffer, AlreadyMapped) {
  const size_t vmo_sz = zx_system_get_page_size();
  zx::vmo vmo = CreateVmo(vmo_sz);
  mmio_buffer_t mb1 = {};
  mmio_buffer_t mb2 = {};

  ASSERT_OK(mmio_buffer_init(&mb1, 0, vmo_sz, DuplicateVmo(vmo).get(), ZX_CACHE_POLICY_UNCACHED));
  // A second mapping with a different cache policy should fail.
  ASSERT_EQ(ZX_ERR_BAD_STATE,
            mmio_buffer_init(&mb2, 0, vmo_sz, DuplicateVmo(vmo).get(), ZX_CACHE_POLICY_CACHED));
  // The same cache policy should be fine in a second mmio_buffer.
  ASSERT_EQ(ZX_OK,
            mmio_buffer_init(&mb2, 0, vmo_sz, DuplicateVmo(vmo).get(), ZX_CACHE_POLICY_UNCACHED));
  mmio_buffer_release(&mb1);
  mmio_buffer_release(&mb2);
}

TEST(MmioBuffer, AlreadySetVmoCachePolicy) {
  const size_t vmo_sz = zx_system_get_page_size();
  uint32_t policy = ZX_CACHE_POLICY_UNCACHED_DEVICE;
  zx::vmo vmo = CreateVmoWithPolicy(vmo_sz, policy);
  mmio_buffer_t mb1 = {};
  mmio_buffer_t mb2 = {};

  // Since the VMO isn't mapped yet the mmio_buffer_t policy can differ.
  ASSERT_OK(mmio_buffer_init(&mb1, 0, vmo_sz, DuplicateVmo(vmo).get(), ZX_CACHE_POLICY_UNCACHED));
  // Trying to map with another policy will fail.
  ASSERT_EQ(ZX_ERR_BAD_STATE, mmio_buffer_init(&mb2, 0, vmo_sz, DuplicateVmo(vmo).get(), policy));
  // A second mmio_buffer_t with the existing policy will succeed.
  ASSERT_OK(mmio_buffer_init(&mb2, 0, vmo_sz, DuplicateVmo(vmo).get(), ZX_CACHE_POLICY_UNCACHED));
  mmio_buffer_release(&mb1);
  mmio_buffer_release(&mb2);
}

TEST(MmioBuffer, TestMmioBuffer) {
  ASSERT_DEATH([]() { fdf_testing::CreateMmioBuffer(0); });
  ASSERT_DEATH([]() { fdf_testing::CreateMmioBuffer(std::numeric_limits<size_t>::max()); });

  size_t size = zx_system_get_page_size();
  auto buffer = fdf_testing::CreateMmioBuffer(size);
  ASSERT_EQ(size, buffer.get_size());

  auto view = buffer.View(0);
  uint32_t test_val = 0xABCD;
  zx_off_t offset = 0x60;

  buffer.Write32(test_val, offset);
  EXPECT_EQ(view.Read32(offset), test_val);
}

TEST(MmioBuffer, TestMmioBufferWithVmo) {
  zx::vmo vmo;
  size_t size = zx_system_get_page_size();
  ASSERT_OK(zx::vmo::create(size, 0, &vmo));

  ASSERT_DEATH([]() { fdf_testing::CreateMmioBuffer(zx::vmo(ZX_HANDLE_INVALID)); });

  auto buffer = fdf_testing::CreateMmioBuffer(std::move(vmo));
  ASSERT_EQ(size, buffer.get_size());

  auto view = buffer.View(0);
  uint32_t test_val = 0xABCD;
  zx_off_t offset = 0x60;

  buffer.Write32(test_val, offset);
  EXPECT_EQ(view.Read32(offset), test_val);
}

TEST(MmioPinnedBuffer, MoveAssignment) {
  zx::bti bti;
  ASSERT_OK(fake_bti_create(bti.reset_and_get_address()));

  const size_t page_size = zx_system_get_page_size();
  zx::vmo vmo = CreateVmo(page_size);
  zx::result<fdf::MmioBuffer> buffer =
      fdf::MmioBuffer::Create(0, page_size, std::move(vmo), ZX_CACHE_POLICY_UNCACHED_DEVICE);
  ASSERT_OK(buffer.status_value());

  zx::result<fdf::MmioPinnedBuffer> pinned1 = buffer->Pin(bti);
  ASSERT_OK(pinned1.status_value());
  EXPECT_EQ(1u, PinnedVmoCount(bti));
  EXPECT_EQ(pinned1->get_paddr(), FAKE_BTI_PHYS_ADDR);

  // Self-move-assignment for MmioPinnedBuffer: state remains valid.
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wself-move"
  *pinned1 = std::move(*pinned1);
#pragma clang diagnostic pop
  EXPECT_EQ(pinned1->get_paddr(), FAKE_BTI_PHYS_ADDR);
  EXPECT_EQ(1u, PinnedVmoCount(bti));

  zx::result<fdf::MmioPinnedBuffer> pinned2 = buffer->Pin(bti);
  ASSERT_OK(pinned2.status_value());
  EXPECT_EQ(2u, PinnedVmoCount(bti));

  // Move-assigning one pinned buffer over another unpins the destination.
  *pinned1 = std::move(*pinned2);
  EXPECT_EQ(1u, PinnedVmoCount(bti));

  pinned1->reset();
  EXPECT_EQ(0u, PinnedVmoCount(bti));
}

TEST(MmioBuffer, SetMmioBufferOps) {
  size_t size = zx_system_get_page_size();
  auto buffer = fdf_testing::CreateMmioBuffer(size);
  EXPECT_EQ(fdf::MmioBuffer::GetDefaultOps(), &fdf::internal::kDefaultOps);

  MMIO_PTR void* expected_vaddr = buffer.get();
  size_t expected_size = buffer.get_size();

  bool read_called = false;
  static constexpr fdf::MmioBufferOps kCustomOps = {
      .Read8 = [](const void* ctx, const fdf::MmioBuffer& mmio, zx_off_t offs) -> uint8_t {
        *static_cast<bool*>(const_cast<void*>(ctx)) = true;
        return 0x42;
      },
      .Read16 = nullptr,
      .Read32 = nullptr,
      .Read64 = nullptr,
      .ReadBuffer = nullptr,
      .Write8 = nullptr,
      .Write16 = nullptr,
      .Write32 = nullptr,
      .Write64 = nullptr,
      .WriteBuffer = nullptr,
  };

  buffer = fdf::SetMmioBufferOps(std::move(buffer), &kCustomOps, &read_called);
  EXPECT_EQ(expected_vaddr, buffer.get());
  EXPECT_EQ(expected_size, buffer.get_size());
  EXPECT_EQ(0x42, buffer.Read8(0));
  EXPECT_TRUE(read_called);

  // Test resetting back to default ops.
  buffer = fdf::SetMmioBufferOps(std::move(buffer));
  buffer.Write8(0x77, 0);
  EXPECT_EQ(0x77, buffer.Read8(0));

  {
    zx::vmo vmo = CreateVmo(size);
    zx::vmo vmo_dup = DuplicateVmo(vmo);
    auto src = fdf_testing::CreateMmioBuffer(std::move(vmo));
    MMIO_PTR void* orig_vaddr = src.get();
    auto swapped = fdf::SetMmioBufferOps(std::move(src), fdf::MmioBuffer::GetDefaultOps());
    EXPECT_EQ(nullptr, src.get());
    EXPECT_EQ(orig_vaddr, swapped.get());
    EXPECT_EQ(1u, VmoNumMappings(vmo_dup));
  }

  // A null ops pointer also selects the default ops.
  buffer = fdf::SetMmioBufferOps(std::move(buffer), &kCustomOps, &read_called);
  buffer = fdf::SetMmioBufferOps(std::move(buffer), nullptr);
  read_called = false;
  buffer.Write8(0x66, 0);
  EXPECT_EQ(0x66, buffer.Read8(0));
  EXPECT_FALSE(read_called);
}

}  // namespace
