// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/page/size.h>
#include <lib/unittest/unittest.h>

#include <object/job_dispatcher.h>
#include <object/process_dispatcher.h>
#include <object/vm_address_region_dispatcher.h>
#include <vm/vm_address_region.h>
#include <vm/vm_object_paged.h>

namespace {

bool TestGetAttributedMemoryUnstartedProcess() {
  BEGIN_TEST;

  KernelHandle<JobDispatcher> job;
  zx_rights_t job_rights;
  ASSERT_EQ(JobDispatcher::Create(0, GetRootJobDispatcher(), &job, &job_rights), ZX_OK);

  KernelHandle<ProcessDispatcher> process;
  KernelHandle<VmAddressRegionDispatcher> vmar;
  zx_rights_t process_rights = 0;
  zx_rights_t vmar_rights = 0;
  ASSERT_EQ(ProcessDispatcher::Create(job.dispatcher(), "unstarted-proc", 0u, &process,
                                      &process_rights, &vmar, &vmar_rights),
            ZX_OK);

  EXPECT_EQ(process.dispatcher()->state(), ProcessDispatcher::State::INITIAL);
  EXPECT_EQ(process.dispatcher()->GetAttributedMemory().uncompressed_bytes, 0u);

  fbl::RefPtr<VmObjectPaged> vmo;
  ASSERT_EQ(VmObjectPaged::Create(0u, 0u, kPageSize, &vmo), ZX_OK);
  ASSERT_EQ(vmo->CommitRange(0, kPageSize), ZX_OK);

  constexpr uint kArchFlags = uint{ARCH_MMU_FLAG_PERM_READ} | uint{ARCH_MMU_FLAG_PERM_WRITE} |
                              uint{ARCH_MMU_FLAG_PERM_USER};
  fbl::RefPtr<VmAddressRegion> root_vmar = process.dispatcher()->normal_aspace()->RootVmar();
  ASSERT_NONNULL(root_vmar);
  auto mapping = root_vmar->CreateVmMapping(0, kPageSize, 0, 0, vmo, 0, kArchFlags, "test-mapping");
  ASSERT_TRUE(mapping.is_ok());

  // While the process is still in State::INITIAL, its mapped VMO pages should be attributed.
  EXPECT_EQ(process.dispatcher()->state(), ProcessDispatcher::State::INITIAL);
  VmObject::AttributionCounts counts = process.dispatcher()->GetAttributedMemory();
  EXPECT_EQ(counts.uncompressed_bytes, kPageSize);
  EXPECT_EQ(counts.private_uncompressed_bytes, kPageSize);

  // Once the process transitions to State::DEAD, attributed memory should be zero.
  process.dispatcher()->Kill(0);
  EXPECT_EQ(process.dispatcher()->state(), ProcessDispatcher::State::DEAD);
  EXPECT_EQ(process.dispatcher()->GetAttributedMemory().uncompressed_bytes, 0u);

  job.dispatcher()->Kill(0);
  END_TEST;
}

}  // namespace

UNITTEST_START_TESTCASE(process_dispatcher_tests)
UNITTEST("GetAttributedMemoryUnstartedProcess", TestGetAttributedMemoryUnstartedProcess)
UNITTEST_END_TESTCASE(process_dispatcher_tests, "process_dispatcher_tests",
                      "ProcessDispatcher tests")
