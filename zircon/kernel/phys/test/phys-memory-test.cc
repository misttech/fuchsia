// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/memalloc/range.h>
#include <stdio.h>
#include <string.h>
#include <zircon/assert.h>

#include <fbl/algorithm.h>
#include <ktl/byte.h>
#include <ktl/span.h>
#include <phys/address-space.h>
#include <phys/allocation.h>
#include <phys/new.h>

#include "test-main.h"

#include <ktl/enforce.h>

namespace {

constexpr uint64_t kMiB = 1024 * 1024;

// Allocate and overwrite all RAM from the given memalloc::Pool.
//
// Return the number of bytes that were in the allocator.
size_t AllocateAndOverwriteFreeMemory() {
  size_t bytes_allocated = 0;

  // To avoid having to call into the allocator too many times, we start
  // trying to do large allocations, and gradually ask for less and less
  // memory as the larger allocations fail.
  size_t allocation_size = kMiB;  // start with 1MiB allocations.
  int iter = 0;
  while (allocation_size > 0) {
    if (iter % 200 == 0) {
      printf("Iteration %d: Allocation size %#zx bytes.\n", iter, allocation_size);
    }
    iter++;
    // Allocate some memory.
    fbl::AllocChecker ac;
    auto result = Allocation::New(ac, memalloc::Type::kZbiTestPayload, allocation_size);
    if (!ac.check()) {
      printf("Iteration %d: Allocation size reduced to %#zx bytes.\n", iter, allocation_size / 2);
      allocation_size /= 2;
      continue;
    }
    if (!result.get()) {
      break;
    }

    bytes_allocated += allocation_size;

    // Overwrite the memory.
    memset(result.get(), 0x33, allocation_size);

    // Leak this allocation.
    (void)result.release();
  }

  return bytes_allocated;
}

void LogMemory() {
#if !EFI
  Allocation::GetPool().PrintMemoryRanges(ProgramName());
#endif
}

}  // namespace

int TestMain(void* bootloader_data, ktl::optional<EarlyBootZbi> zbi, arch::EarlyTicks ticks) {
  Symbolize symbolize("phys-memory-test");

#if defined(__riscv)
  // TODO(https://fxbug.dev/571663247): Re-enable on riscv64.
  //
  // This test overwrites all of RAM with memset.  Under QEMU TCG, a hot loop
  // whose code straddles a 4 KiB page boundary runs ~8-11x slower because
  // TCG can't directly chain translation blocks across guest pages.  Whether
  // memset's store loop straddles a page depends on the size of all the code
  // linked before it, so unrelated changes can push this test from ~12s to
  // ~95s and past its timeout.
  constexpr bool kSkipTest = true;
#else
  constexpr bool kSkipTest = false;
#endif
  if (kSkipTest) {
    printf("phys-memory-test: SKIPPED on this architecture.\n");
    return 0;
  }

  printf("Initializing memory...\n");

  // Initialize memory for allocation/free.
  AddressSpace aspace;
  InitMemory(bootloader_data, ktl::move(zbi), &aspace);

  LogMemory();

  printf("Testing memory allocation...\n");

  fbl::AllocChecker ac;
  int* x = new (gPhysNew<memalloc::Type::kZbiTestPayload>, ac) int{17};
  ZX_ASSERT(ac.check());
  ZX_ASSERT(x != nullptr);
  ZX_ASSERT(*x == 17);

  size_t bytes_allocated = AllocateAndOverwriteFreeMemory();
  if (bytes_allocated == 0) {
    printf("FAIL: Could not allocate any memory.\n");
    LogMemory();
    return 1;
  }

  printf("Successfully allocated %zu bytes of memory.\n", bytes_allocated);
  LogMemory();
  return 0;
}
