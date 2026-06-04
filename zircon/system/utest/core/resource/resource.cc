// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/standalone-test/standalone.h>
#include <lib/zbi-format/kernel.h>
#include <lib/zbi-format/zbi.h>
#include <lib/zbitl/item.h>
#include <lib/zx/interrupt.h>
#include <lib/zx/pager.h>
#include <lib/zx/port.h>
#include <lib/zx/resource.h>
#include <lib/zx/vmar.h>
#include <lib/zx/vmo.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <zircon/errors.h>
#include <zircon/syscalls.h>
#include <zircon/syscalls/object.h>
#include <zircon/syscalls/port.h>
#include <zircon/syscalls/resource.h>
#include <zircon/types.h>

#include <thread>

#include <zxtest/zxtest.h>

static const size_t mmio_test_size = (zx_system_get_page_size() * 4);
static uint64_t mmio_test_base;

const zx::unowned_resource get_ioport() { return standalone::GetIoportResource(); }

const zx::unowned_resource get_mmio() { return standalone::GetMmioResource(); }

const zx::unowned_resource get_system() { return standalone::GetSystemResource(); }

// Physical memory is reserved during boot and its location varies based on
// system and architecture. What this 'test' does is scan MMIO space looking
// for a valid region to test against, ensuring that the only errors it sees
// are 'ZX_ERR_NOT_FOUND', which indicates that it is missing from the
// region allocator.
//
// TODO(https://fxbug.dev/42107339): Figure out a way to test IRQs in the same manner, without
// hardcoding target-specific IRQ vectors in these tests. That information is
// stored in the kernel and is not exposed to userspace, so we can't simply
// guess/probe valid vectors like we can MMIO and still assume the tests are
// valid.

TEST(Resource, ProbeAddressSpace) {
  zx_status_t status;
  // Scan mmio in chunks until we find a gap that isn't exclusively reserved physical memory.
  uint64_t step = 0x100000000;
  for (uint64_t base = 0; base < UINT64_MAX - step; base += step) {
    zx::resource handle;
    status = zx::resource::create(*get_mmio(), ZX_RSRC_KIND_MMIO, base, mmio_test_size, NULL, 0,
                                  &handle);
    if (status == ZX_OK) {
      mmio_test_base = base;
      break;
    }

    // If ZX_OK wasn't returned, then we should see either ZX_ERR_NOT_FOUND or
    // ZX_ERR_ACCESS_DENIED.
    ASSERT_TRUE(status == ZX_ERR_NOT_FOUND || status == ZX_ERR_ACCESS_DENIED);
  }
}

// This is a basic smoketest for creating resources and verifying the internals
// returned by zx_object_get_info match what the caller passed for creation.
TEST(Resource, BasicActions) {
  zx::resource new_resource;
  zx_info_resource_t info;
  char resource_name[] = "resource";

  // Create a resource and verify the fields are still zero, but the name matches.
  EXPECT_EQ(zx::resource::create(*get_mmio(), ZX_RSRC_KIND_MMIO, mmio_test_base, mmio_test_size,
                                 resource_name, sizeof(resource_name), &new_resource),
            ZX_OK);
  ASSERT_EQ(new_resource.get_info(ZX_INFO_RESOURCE, &info, sizeof(info), NULL, NULL), ZX_OK);
  EXPECT_EQ(info.kind, ZX_RSRC_KIND_MMIO);
  EXPECT_EQ(info.base, mmio_test_base);
  EXPECT_EQ(info.size, mmio_test_size);
  EXPECT_EQ(info.flags, 0u);
  EXPECT_EQ(0, strncmp(resource_name, info.name, ZX_MAX_NAME_LEN));

  // Check that a resource is created with all the parameters passed to the syscall, and use
  // the new resource created for good measure.
  zx::resource mmio;
  uint32_t kind = ZX_RSRC_KIND_MMIO;
  char mmio_name[] = "test_resource_name";
  ASSERT_EQ(zx::resource::create(new_resource, kind, mmio_test_base, mmio_test_size, mmio_name,
                                 sizeof(mmio_name), &mmio),
            ZX_OK);
  ASSERT_EQ(mmio.get_info(ZX_INFO_RESOURCE, &info, sizeof(info), NULL, NULL), ZX_OK);
  EXPECT_EQ(info.kind, kind);
  EXPECT_EQ(info.flags, 0u);
  EXPECT_EQ(info.base, mmio_test_base);
  EXPECT_EQ(info.size, mmio_test_size);
  EXPECT_EQ(0, strncmp(info.name, mmio_name, ZX_MAX_NAME_LEN));
}

// This test covers every path that returns ZX_ERR_INVALID_ARGS from the syscall.
TEST(Resource, InvalidArgs) {
  zx::resource temp;
  zx::resource fail_hnd;
  // test privilege inversion by seeing if an MMIO resource can create other resources.
  EXPECT_EQ(zx::resource::create(*get_mmio(), ZX_RSRC_KIND_MMIO, mmio_test_base, mmio_test_size,
                                 NULL, 0, &temp),
            ZX_OK);
  EXPECT_EQ(zx::resource::create(temp, ZX_RSRC_KIND_ROOT, 0, 0, NULL, 0, &fail_hnd),
            ZX_ERR_ACCESS_DENIED);
  EXPECT_EQ(zx::resource::create(temp, ZX_RSRC_KIND_IRQ, 0, 0, NULL, 0, &fail_hnd),
            ZX_ERR_ACCESS_DENIED);

  // test invalid kind
  EXPECT_EQ(zx::resource::create(*get_mmio(), ZX_RSRC_KIND_COUNT, mmio_test_base, mmio_test_size,
                                 NULL, 0, &temp),
            ZX_ERR_INVALID_ARGS);
  EXPECT_EQ(zx::resource::create(*get_mmio(), ZX_RSRC_KIND_COUNT + 1, mmio_test_base,
                                 mmio_test_size, NULL, 0, &temp),
            ZX_ERR_INVALID_ARGS);

  // test invalid base
  EXPECT_EQ(zx::resource::create(*get_mmio(), ZX_RSRC_KIND_MMIO, UINT64_MAX, 1024, NULL, 0, &temp),
            ZX_ERR_INVALID_ARGS);
  // test invalid size
  EXPECT_EQ(zx::resource::create(*get_mmio(), ZX_RSRC_KIND_MMIO, 1024, UINT64_MAX, NULL, 0, &temp),
            ZX_ERR_INVALID_ARGS);
  // test invalid options
  EXPECT_EQ(zx::resource::create(*get_mmio(), ZX_RSRC_KIND_MMIO | 0xFF0000, mmio_test_base,
                                 mmio_test_size, NULL, 0, &temp),
            ZX_ERR_INVALID_ARGS);
}

TEST(Resource, ExclusiveShared) {
  // Try to create a shared  resource and ensure it blocks an exclusive
  // resource.
  zx::resource mmio_1, mmio_2;
  EXPECT_EQ(zx::resource::create(*get_mmio(), ZX_RSRC_KIND_MMIO | ZX_RSRC_FLAG_EXCLUSIVE,
                                 mmio_test_base, mmio_test_size, NULL, 0, &mmio_1),
            ZX_OK);
  EXPECT_EQ(zx::resource::create(*get_mmio(), ZX_RSRC_KIND_MMIO, mmio_test_base, mmio_test_size,
                                 NULL, 0, &mmio_2),
            ZX_ERR_NOT_FOUND);
}

TEST(Resource, SharedExclusive) {
  // Try to create a shared resource and ensure it blocks an exclusive
  // resource.
  zx::resource mmio_1, mmio_2;
  EXPECT_EQ(zx::resource::create(*get_mmio(), ZX_RSRC_KIND_MMIO, mmio_test_base, mmio_test_size,
                                 NULL, 0, &mmio_1),
            ZX_OK);
  EXPECT_EQ(zx::resource::create(*get_mmio(), ZX_RSRC_KIND_MMIO | ZX_RSRC_FLAG_EXCLUSIVE,
                                 mmio_test_base, mmio_test_size, NULL, 0, &mmio_2),
            ZX_ERR_NOT_FOUND);
}

TEST(Resource, CreateFromRangedRoot) {
  // Try to create an exclusive resource from a ranged resource.
  zx::resource mmio, mmio_dup;
  EXPECT_EQ(zx::resource::create(*get_mmio(), ZX_RSRC_KIND_MMIO | ZX_RSRC_FLAG_EXCLUSIVE,
                                 mmio_test_base, mmio_test_size, NULL, 0, &mmio),
            ZX_OK);
  // Try to duplicate a ranged resource.
  EXPECT_EQ(get_mmio()->duplicate(ZX_RIGHT_SAME_RIGHTS, &mmio_dup), ZX_OK);
}

TEST(Resource, VmoCreation) {
  // Attempt to create a resource and then a vmo using that resource.
  zx::resource mmio;
  zx::vmo vmo;
  ASSERT_EQ(zx::resource::create(*get_mmio(), ZX_RSRC_KIND_MMIO, mmio_test_base, mmio_test_size,
                                 NULL, 0, &mmio),
            ZX_OK);
  EXPECT_EQ(zx_vmo_create_physical(mmio.get(), mmio_test_base, zx_system_get_page_size(),
                                   vmo.reset_and_get_address()),
            ZX_OK);
}

TEST(Resource, VmoCreationSmaller) {
  // Attempt to create a resource smaller than a page and ensure it still expands access to the
  // entire page.
  zx::resource mmio;
  zx::vmo vmo;
  ASSERT_EQ(zx::resource::create(*get_mmio(), ZX_RSRC_KIND_MMIO, mmio_test_base,
                                 zx_system_get_page_size() / 2, NULL, 0, &mmio),
            ZX_OK);
  EXPECT_EQ(zx_vmo_create_physical(mmio.get(), mmio_test_base, zx_system_get_page_size(),
                                   vmo.reset_and_get_address()),
            ZX_OK);
}

TEST(Resource, VmoCreationUnaligned) {
  // Attempt to create an unaligned resource and ensure that the bounds are rounded appropriately
  // to the proper zx_system_get_page_size().
  zx::resource mmio;
  zx::vmo vmo;
  ASSERT_EQ(zx::resource::create(*get_mmio(), ZX_RSRC_KIND_MMIO, mmio_test_base + 0x7800, 0x2000,
                                 NULL, 0, &mmio),
            ZX_OK);
  EXPECT_EQ(zx_vmo_create_physical(mmio.get(), mmio_test_base + 0x7000, 0x2000,
                                   vmo.reset_and_get_address()),
            ZX_OK);
}

// Returns zero on failure.
static zx_rights_t get_vmo_rights(const zx::vmo& vmo) {
  zx_info_handle_basic_t info;
  zx_status_t s =
      zx_object_get_info(vmo.get(), ZX_INFO_HANDLE_BASIC, &info, sizeof(info), nullptr, nullptr);
  if (s != ZX_OK) {
    EXPECT_EQ(s, ZX_OK);  // Poison the test
    return 0;
  }
  return info.rights;
}

TEST(Resource, VmoReplaceAsExecutable) {
  zx::resource vmex;
  zx::vmo vmo, vmo2, vmo3;

  // allocate an object
  ASSERT_EQ(ZX_OK, zx_vmo_create(zx_system_get_page_size(), 0, vmo.reset_and_get_address()));

  // set-exec with valid VMEX resource
  ASSERT_EQ(ZX_OK, zx::resource::create(*get_system(), ZX_RSRC_KIND_SYSTEM,
                                        ZX_RSRC_SYSTEM_VMEX_BASE, 1, NULL, 0, &vmex));
  ASSERT_EQ(ZX_OK, zx_handle_duplicate(vmo.get(), ZX_RIGHT_READ, vmo2.reset_and_get_address()));
  ASSERT_EQ(ZX_OK,
            zx_vmo_replace_as_executable(vmo2.release(), vmex.get(), vmo3.reset_and_get_address()));
  EXPECT_EQ(ZX_RIGHT_READ | ZX_RIGHT_EXECUTE, get_vmo_rights(vmo3));

  // set-exec with ZX_HANDLE_INVALID
  // TODO(mdempsky): Disallow.
  ASSERT_EQ(ZX_OK, zx_handle_duplicate(vmo.get(), ZX_RIGHT_READ, vmo2.reset_and_get_address()));
  ASSERT_EQ(ZX_OK, zx_vmo_replace_as_executable(vmo2.release(), ZX_HANDLE_INVALID,
                                                vmo3.reset_and_get_address()));
  EXPECT_EQ(ZX_RIGHT_READ | ZX_RIGHT_EXECUTE, get_vmo_rights(vmo3));

  // verify invalid handle fails
  ASSERT_EQ(ZX_OK, zx_handle_duplicate(vmo.get(), ZX_RIGHT_READ, vmo2.reset_and_get_address()));
  EXPECT_EQ(ZX_ERR_WRONG_TYPE,
            zx_vmo_replace_as_executable(vmo2.release(), vmo.get(), vmo3.reset_and_get_address()));
}

TEST(Resource, CreateResourceSlice) {
  {
    zx::resource mmio, smaller_mmio;
    ASSERT_EQ(ZX_OK, zx::resource::create(*get_mmio(), ZX_RSRC_KIND_MMIO, mmio_test_base,
                                          zx_system_get_page_size(), NULL, 0, &mmio));
    // A new resource shouldn't be able to create ROOT.
    EXPECT_EQ(ZX_ERR_ACCESS_DENIED,
              zx::resource::create(mmio, ZX_RSRC_KIND_ROOT, mmio_test_base,
                                   zx_system_get_page_size(), NULL, 0, &smaller_mmio));
    // Creating an identically sized resource with the wrong kind should fail.
    EXPECT_EQ(ZX_ERR_ACCESS_DENIED,
              zx::resource::create(mmio, ZX_RSRC_KIND_IRQ, mmio_test_base,
                                   zx_system_get_page_size(), NULL, 0, &smaller_mmio));
    // Creating a resource with a different base and the same size should fail.
    EXPECT_EQ(
        ZX_ERR_ACCESS_DENIED,
        zx::resource::create(mmio, ZX_RSRC_KIND_MMIO, mmio_test_base + zx_system_get_page_size(),
                             zx_system_get_page_size(), NULL, 0, &smaller_mmio));
    // Creating a resource with the same base and a different size should fail.
    EXPECT_EQ(ZX_ERR_ACCESS_DENIED,
              zx::resource::create(mmio, ZX_RSRC_KIND_MMIO, mmio_test_base,
                                   zx_system_get_page_size() + 34u, NULL, 0, &smaller_mmio));
  }
  {
    // Try to make a slice going from exclusive -> shared. This should fail.
    zx::resource mmio, smaller_mmio;
    ASSERT_EQ(ZX_OK,
              zx::resource::create(*get_mmio(), ZX_RSRC_KIND_MMIO | ZX_RSRC_FLAG_EXCLUSIVE,
                                   mmio_test_base, zx_system_get_page_size(), NULL, 0, &mmio));
    EXPECT_EQ(ZX_ERR_INVALID_ARGS,
              zx::resource::create(mmio, ZX_RSRC_KIND_MMIO, mmio_test_base,
                                   zx_system_get_page_size(), NULL, 0, &smaller_mmio));
  }
  {
    // Try to make a slice going from shared -> exclusive. This should fail.
    zx::resource mmio, smaller_mmio;
    ASSERT_EQ(ZX_OK, zx::resource::create(*get_mmio(), ZX_RSRC_KIND_MMIO, mmio_test_base,
                                          zx_system_get_page_size(), NULL, 0, &mmio));
    EXPECT_EQ(ZX_ERR_INVALID_ARGS,
              zx::resource::create(mmio, ZX_RSRC_KIND_MMIO | ZX_RSRC_FLAG_EXCLUSIVE, mmio_test_base,
                                   zx_system_get_page_size(), NULL, 0, &smaller_mmio));
  }
  {
    // Try to make a slice going from exclusive -> exclusive. This should fail.
    zx::resource mmio, smaller_mmio;
    ASSERT_EQ(ZX_OK,
              zx::resource::create(*get_mmio(), ZX_RSRC_KIND_MMIO | ZX_RSRC_FLAG_EXCLUSIVE,
                                   mmio_test_base, zx_system_get_page_size(), NULL, 0, &mmio));
    EXPECT_EQ(ZX_ERR_INVALID_ARGS,
              zx::resource::create(mmio, ZX_RSRC_KIND_MMIO | ZX_RSRC_FLAG_EXCLUSIVE, mmio_test_base,
                                   zx_system_get_page_size(), NULL, 0, &smaller_mmio));
  }
  {
    // Creating a identically sized resource should succeed.
    zx::resource mmio, smaller_mmio;
    ASSERT_EQ(ZX_OK, zx::resource::create(*get_mmio(), ZX_RSRC_KIND_MMIO, mmio_test_base,
                                          zx_system_get_page_size(), NULL, 0, &mmio));
    EXPECT_EQ(ZX_OK, zx::resource::create(mmio, ZX_RSRC_KIND_MMIO, mmio_test_base,
                                          zx_system_get_page_size(), NULL, 0, &smaller_mmio));
  }
  {
    // Creating an smaller resource should succeed.
    zx::vmo vmo;
    zx::resource mmio, smaller_mmio;
    EXPECT_EQ(ZX_OK, zx::resource::create(*get_mmio(), ZX_RSRC_KIND_MMIO, mmio_test_base,
                                          zx_system_get_page_size() * 2, NULL, 0, &mmio));
    // This will succeed at creating an MMIO resource that is a single page size.
    EXPECT_EQ(ZX_OK, zx::resource::create(mmio, ZX_RSRC_KIND_MMIO, mmio_test_base,
                                          zx_system_get_page_size(), NULL, 0, &smaller_mmio));
    // Trying to create a VMO of the original size will fail
    EXPECT_EQ(ZX_ERR_OUT_OF_RANGE,
              zx_vmo_create_physical(smaller_mmio.get(), mmio_test_base,
                                     zx_system_get_page_size() * 2, vmo.reset_and_get_address()));
    // Trying to create VMO that fits in the resource will succeed.
    EXPECT_EQ(ZX_OK,
              zx_vmo_create_physical(smaller_mmio.get(), mmio_test_base, zx_system_get_page_size(),
                                     vmo.reset_and_get_address()));
  }
}

#if defined(__x86_64__)

static inline void outb(uint16_t port, uint8_t data) {
  __asm__ __volatile__("outb %1, %0" : : "dN"(port), "a"(data));
}

TEST(Resource, Ioports) {
  // On x86 create an ioport resource and attempt to have the privilege bits
  // set for the process.
  zx::resource io;
  uint16_t io_base = 0xCF8;
  uint32_t io_size = 8;  // CF8 - CFC (inclusive to 4 bytes each)
  char io_name[] = "ports!";
  ASSERT_EQ(zx::resource::create(*get_ioport(), ZX_RSRC_KIND_IOPORT, io_base, io_size, io_name,
                                 sizeof(io_name), &io),
            ZX_OK);
  EXPECT_EQ(zx_ioports_request(io.get(), io_base, io_size), ZX_OK);

  EXPECT_EQ(zx_ioports_release(io.get(), io_base, io_size), ZX_OK);

  zx::resource one_io;
  char one_io_name[] = "one";
  ASSERT_EQ(zx::resource::create(*get_ioport(), ZX_RSRC_KIND_IOPORT, 0x80, 1, one_io_name,
                                 strlen(one_io_name), &one_io),
            ZX_OK);
  // Ask for the wrong port. Should fail.
  EXPECT_EQ(zx_ioports_request(one_io.get(), io_base, io_size), ZX_ERR_OUT_OF_RANGE);
  // Lets get the right one.
  EXPECT_EQ(zx_ioports_request(one_io.get(), 0x80, 1), ZX_OK);

  outb(/*port=*/0x80, /*data=*/1);  // If we failed to get the port, this will #GP.

  // Try to release the wrong one.
  EXPECT_EQ(zx_ioports_release(one_io.get(), io_base, io_size), ZX_ERR_OUT_OF_RANGE);

  EXPECT_EQ(zx_ioports_release(one_io.get(), 0x80, 1), ZX_OK);
}

TEST(Resource, IoportDeniedByDefault) {
  // Ensure that access without requesting it via resource leads to a general protection fault
  // (#GP).
  ASSERT_DEATH([]() { outb(/*port=*/0x80, /*data=*/1); },
               "I/O port access should be forbidden by default and trigger a crash");
}

// Regression test for https://fxbug.dev/502277149
TEST(Resource, MexecPagerPanic) {
  const zx::unowned_resource system = get_system();
  if (!system->is_valid()) {
    ZXTEST_SKIP("System resource not available");
  }

  // Create a valid-looking ZBI for kernel_vmo
  zx::vmo kernel_vmo;
  ASSERT_OK(zx::vmo::create(8192, 0, &kernel_vmo));

  zbi_header_t header = {
      .type = ZBI_TYPE_CONTAINER,
      .length = static_cast<uint32_t>(sizeof(zbi_header_t) + sizeof(zbi_kernel_t)),
      .extra = ZBI_CONTAINER_MAGIC,
      .flags = ZBI_FLAGS_VERSION,
      .reserved0 = 0,
      .reserved1 = 0,
      .magic = ZBI_ITEM_MAGIC,
      .crc32 = ZBI_ITEM_NO_CRC32,
  };
  zbi_header_t kheader = {
      .type = ZBI_TYPE_KERNEL_X64,
      .length = static_cast<uint32_t>(sizeof(zbi_kernel_t)),
      .extra = 0,
      .flags = ZBI_FLAGS_VERSION,
      .reserved0 = 0,
      .reserved1 = 0,
      .magic = ZBI_ITEM_MAGIC,
      .crc32 = ZBI_ITEM_NO_CRC32,
  };
  zbi_kernel_t kernel = {
      .entry = 0x100,
      .reserve_memory_size = 0,
  };
  ASSERT_OK(kernel_vmo.write(&header, 0, sizeof(header)));
  ASSERT_OK(kernel_vmo.write(&kheader, sizeof(header), sizeof(kheader)));
  ASSERT_OK(kernel_vmo.write(&kernel, sizeof(header) + sizeof(kheader), sizeof(kernel)));

  zx::pager pager;
  zx::port port;
  zx::vmo vmo;
  ASSERT_OK(zx::pager::create(0, &pager));
  ASSERT_OK(zx::port::create(0, &port));
  ASSERT_OK(pager.create_vmo(0, port, 0, zx_system_get_page_size(), &vmo));

  std::thread t([&pager, &port, &vmo]() {
    for (;;) {
      zx_port_packet_t packet;
      zx_status_t status = port.wait(zx::time::infinite(), &packet);
      if (status != ZX_OK)
        break;

      if (packet.type == ZX_PKT_TYPE_USER)
        break;

      if (packet.type == ZX_PKT_TYPE_PAGE_REQUEST &&
          packet.page_request.command == ZX_PAGER_VMO_READ) {
        pager.op_range(ZX_PAGER_OP_FAIL, vmo, packet.page_request.offset,
                       packet.page_request.length, ZX_ERR_IO);
      }
    }
  });

  // Call mexec.
  // This should return an error, but not panic!
  zx_status_t status = zx_system_mexec(system->get(), kernel_vmo.get(), vmo.get());

  EXPECT_NE(status, ZX_OK);

  // Cleanup
  zx_port_packet_t packet = {};
  packet.type = ZX_PKT_TYPE_USER;
  port.queue(&packet);
  t.join();
}

#endif  // defined(__x86_64__)

// Regression test for https://fxbug.dev/503716683
TEST(Resource, MexecZeroSizedVmo) {
  // This test requires the MEXEC resource.
  zx::unowned_resource system_resource = standalone::GetSystemResource();
  if (!system_resource->is_valid()) {
    ZXTEST_SKIP("System resource not available");
  }

  zx::result<zx::resource> mexec_rsrc_result =
      standalone::GetSystemResourceWithBase(system_resource, ZX_RSRC_SYSTEM_MEXEC_BASE);

  if (mexec_rsrc_result.is_error()) {
    ZXTEST_SKIP("MEXEC resource not available or failed to derive");
  }
  zx::resource mexec_resource = std::move(mexec_rsrc_result.value());

  zx::vmo vmo1, vmo2;
  ASSERT_OK(zx::vmo::create(0, 0, &vmo1));
  ASSERT_OK(zx::vmo::create(0, 0, &vmo2));

  // Zero sized kernel or bootimage is never valid.
  EXPECT_EQ(zx_system_mexec(mexec_resource.get(), vmo1.get(), vmo2.get()), ZX_ERR_BAD_STATE);
}

// Regression test for https://fxbug.dev/503703423
TEST(Resource, MexecPayloadTooSmall) {
  // This test requires the MEXEC resource.
  zx::unowned_resource system_resource = get_system();
  if (!system_resource->is_valid()) {
    ZXTEST_SKIP("System resource not available");
  }

  zx::resource mexec_resource;
  zx_status_t status =
      zx::resource::create(*system_resource, ZX_RSRC_KIND_SYSTEM, ZX_RSRC_SYSTEM_MEXEC_BASE, 1,
                           nullptr, 0, &mexec_resource);

  if (status != ZX_OK) {
    ZXTEST_SKIP("MEXEC resource not available or failed to derive");
  }

  // Buffer small enough to cause Extend to fail.
  uint8_t buffer[32];

  status = zx_system_mexec_payload_get(mexec_resource.get(), buffer, sizeof(buffer));

  // If we reach here, it didn't crash!
  EXPECT_NE(ZX_OK, status);
}

// Regression test for https://fxbug.dev/507415559
TEST(Resource, MexecEmptyZbi) {
  // This test requires the MEXEC resource.
  zx::unowned_resource system_resource = get_system();
  if (!system_resource->is_valid()) {
    ZXTEST_SKIP("System resource not available");
  }

  zx::resource mexec_resource;
  if (zx_status_t status =
          zx::resource::create(*system_resource, ZX_RSRC_KIND_SYSTEM, ZX_RSRC_SYSTEM_MEXEC_BASE, 1,
                               nullptr, 0, &mexec_resource);
      status != ZX_OK) {
    ZXTEST_SKIP("MEXEC resource not available");
  }

  zx::vmo kernel_vmo, bootimage_vmo;
  ASSERT_OK(zx::vmo::create(zx_system_get_page_size(), 0, &kernel_vmo));
  ASSERT_OK(zx::vmo::create(zx_system_get_page_size(), 0, &bootimage_vmo));

  // Valid container, but no kernel item.
  const zbi_header_t header = zbitl::ContainerHeader(0);
  ASSERT_OK(kernel_vmo.write(&header, 0, sizeof(header)));

  EXPECT_EQ(ZX_ERR_IO_DATA_INTEGRITY,
            zx_system_mexec(mexec_resource.get(), kernel_vmo.get(), bootimage_vmo.get()));
}

// Regression test for https://fxbug.dev/512234306
// NOTE: If behavior ever regresses, then we expect this
// test to only fail (crash) on KASAN builds of the kernel.
TEST(Resource, MexecBadZbiHeaderLength) {
  zx::unowned_resource system_resource = get_system();
  if (!system_resource->is_valid()) {
    ZXTEST_SKIP("System resource not available");
  }

  zx::resource mexec_resource;
  zx_status_t status =
      zx::resource::create(*system_resource, ZX_RSRC_KIND_SYSTEM, ZX_RSRC_SYSTEM_MEXEC_BASE, 1,
                           nullptr, 0, &mexec_resource);
  if (status != ZX_OK) {
    ZXTEST_SKIP("MEXEC resource not available");
  }

  zbi_header_t container = {
      .type = ZBI_TYPE_CONTAINER,
      .length = 0x40000000,  // 1 GB
      .extra = ZBI_CONTAINER_MAGIC,
      .flags = ZBI_FLAGS_VERSION,
      .reserved0 = 0,
      .reserved1 = 0,
      .magic = ZBI_ITEM_MAGIC,
      .crc32 = ZBI_ITEM_NO_CRC32,
  };

  zbi_header_t item1 = {
      .type = ZBI_TYPE_DISCARD,
      .length = 0x3FFFFF00,  // Jump 1 GB
      .extra = 0,
      .flags = ZBI_FLAGS_VERSION,
      .reserved0 = 0,
      .reserved1 = 0,
      .magic = ZBI_ITEM_MAGIC,
      .crc32 = ZBI_ITEM_NO_CRC32,
  };

  zx::vmo kernel_vmo, bootimage_vmo;
  const size_t vmo_size = sizeof(container) + sizeof(item1);
  ASSERT_OK(zx::vmo::create(vmo_size, 0, &kernel_vmo));
  ASSERT_OK(zx::vmo::create(vmo_size, 0, &bootimage_vmo));

  ASSERT_OK(kernel_vmo.write(&container, 0, sizeof(container)));
  ASSERT_OK(kernel_vmo.write(&item1, sizeof(container), sizeof(item1)));

  status = zx_system_mexec(mexec_resource.get(), kernel_vmo.get(), bootimage_vmo.get());
  EXPECT_NE(ZX_OK, status);
}

#if defined(__x86_64__)

// Regression test for https://fxbug.dev/517585028
TEST(Resource, PcInterruptVectorPoolLeak) {
  zx::unowned_resource irq_res = standalone::GetIrqResource();
  if (!irq_res->is_valid()) {
    ZXTEST_SKIP("IRQ resource not available");
  }
  std::atomic<bool> stop{false};

  // This test spawns two threads repeatedly creating and destroying interrupt dispatcher
  // objects in order to exploit a potential race condition in the pc platform's
  // interrupt manager and leak entries of the vector pool.

  auto repeatedly_create_and_destroy_interrupt = [&]() {
    while (!stop.load()) {
      zx_handle_t h = ZX_HANDLE_INVALID;
      if (zx_interrupt_create(irq_res->get(), 8, ZX_INTERRUPT_MODE_EDGE_HIGH, &h) == ZX_OK) {
        zx_handle_close(h);
      }
    }
  };

  std::thread t1(repeatedly_create_and_destroy_interrupt);
  std::thread t2(repeatedly_create_and_destroy_interrupt);

  zx_nanosleep(zx_deadline_after(ZX_MSEC(50)));
  stop.store(true);
  t1.join();
  t2.join();

  // By this point, if there was a leak, we would expect a crash.
}

#endif  // defined(__x86_64__)
