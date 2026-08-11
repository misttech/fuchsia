// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/unittest/unittest.h>

#include <arch/x86/fake_msr_access.h>
#include <arch/x86/pv.h>
#include <ktl/iterator.h>
#include <ktl/unique_ptr.h>
#include <vm/physmap.h>

#include <ktl/enforce.h>

namespace {

bool TestPvEoi() {
  BEGIN_TEST;

  // Can't signal if not enabled.
  {
    PvEoi pv;
    pv.Init();
    EXPECT_FALSE(pv.Eoi());
  }

  // Enable, signal, then disable.
  {
    FakeMsrAccess msr{};
    msr.msrs_[0] = {X86_MSR_KVM_PV_EOI_EN, 0U};

    PvEoi pv;
    pv.Init();
    pv.Enable(&msr);

    // Find the PvEoi's state via MSR value.
    uint64_t pa = msr.msrs_[0].value;
    EXPECT_TRUE(pa & X86_MSR_KVM_PV_EOI_EN_ENABLE);
    pa &= ~X86_MSR_KVM_PV_EOI_EN_ENABLE;
    auto state = reinterpret_cast<uint64_t*>(paddr_to_physmap(pa));

    EXPECT_EQ(*state, 0U);

    EXPECT_FALSE(pv.Eoi());
    EXPECT_EQ(*state, 0U);

    *state = 1;
    EXPECT_TRUE(pv.Eoi());
    EXPECT_EQ(*state, 0U);

    pv.Disable(&msr);
    EXPECT_EQ(msr.msrs_[0].value, 0U);
  }

  END_TEST;
}

}  // namespace

UNITTEST_START_TESTCASE(pc_interrupt_tests)
UNITTEST("PvEoi", TestPvEoi)
UNITTEST_END_TESTCASE(pc_interrupt_tests, "pc_interrupt", "Tests for external interrupts")
