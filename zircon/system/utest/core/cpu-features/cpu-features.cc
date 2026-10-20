// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <zircon/features.h>
#include <zircon/syscalls.h>

#include <array>

#include <zxtest/zxtest.h>

namespace {

#ifdef __aarch64__

TEST(CpuFeatureTests, ArmMops) {
  uint32_t features = 0;
  ASSERT_OK(zx_system_get_features(ZX_FEATURE_KIND_CPU, &features));

  if ((features & ZX_ARM64_FEATURE_ISA_MOPS) == 0) {
    ZXTEST_SKIP("FEAT_MOPS not detected");
    return;
  }

  uint64_t mem = 0xaaaaaaaaaaaaaaaa;
  uint64_t* dst = &mem;
  uint64_t count = sizeof(mem);
  uint64_t byte = 0xbb;
  __asm__(
      R"""(
      .arch_extension mops
      setp [%[dst]]!, %[count]!, %[byte]
      setm [%[dst]]!, %[count]!, %[byte]
      sete [%[dst]]!, %[count]!, %[byte]
      )"""
      // The memory output operand tells the compiler that the memory will be
      // touched, so it can't presume it knows the value.  It also being an
      // input via `+` tells the compiler that the initialization store is not
      // dead, so we're sure the memory had the old value before the SETP.
      : "+m"(*dst), [dst] "+r"(dst), [count] "+r"(count)
      : [byte] "r"(byte)
      : "cc");
  EXPECT_EQ(mem, 0xbbbbbbbbbbbbbbbb);
}

TEST(CpuFeatureTests, ArmMopsException) {
  uint32_t features = 0;
  ASSERT_OK(zx_system_get_features(ZX_FEATURE_KIND_CPU, &features));

  if ((features & ZX_ARM64_FEATURE_ISA_MOPS) == 0) {
    ZXTEST_SKIP("FEAT_MOPS not detected");
    return;
  }

  // The prologue insn sets or clears the C flag to indicate its algorithm.
  // The main body insn checks that C matches its algorithm, but is only
  // required to check when it has work to do.  There's no guarantee how much
  // the prologue insn can do by itself, but a very large count (greater than
  // page size) effectively ensures that the main body will have work to do and
  // so will be required to check C and raise the exception.  There's no
  // guarantee whether C will be set or cleared, but it must be left as is for
  // the main body insn.  So simply inverting C between the two instructions
  // should produce the exception.  It's technically not quite kosher to use
  // the instructions in any way other than precisely in the adjacent sequence
  // of three with no intervening instructions.  But this probably has the
  // intended effect on real CPUs as well as emulators, and is certainly easier
  // than rigging things up to single-step and invert the flag via exception
  // return without breaking the prescribed adjacency of the instructions.
  constexpr auto bad_mops = [] {
    std::array<char, 0x20000> mem;
    memset(&mem, 0xaa, sizeof(mem));
    auto* dst = &mem;
    uint64_t count = sizeof(mem);
    uint64_t byte = 0xbb;
    uint64_t scratch;
    __asm__ volatile(
        R"""(
        .arch_extension mops
        setp [%[dst]]!, %[count]!, %[byte]
        mrs %[flags], nzcv
        eor %[flags], %[flags], %[c]
        msr nzcv, %[flags]
        setm [%[dst]]!, %[count]!, %[byte]
        )"""
        : "+m"(*dst), [dst] "+r"(dst), [count] "+r"(count), [flags] "=&r"(scratch)
        : [byte] "r"(byte), [c] "i"(1 << 29)
        : "cc");
  };
  ASSERT_DEATH(bad_mops);
}

TEST(CpuFeatureTests, ArmSve) {
  uint32_t features = 0;
  ASSERT_OK(zx_system_get_features(ZX_FEATURE_KIND_CPU, &features));

  if ((features & ZX_ARM64_FEATURE_ISA_SVE) == 0) {
    ASSERT_DEATH([] {
      uint64_t vl = 0;
      __asm__ volatile(
          R"""(
          .arch_extension sve
          rdvl %[vl], #1
          .arch_extension nosve
          )"""
          : [vl] "=&r"(vl));
    });
    return;
  }

  uint64_t vl = 0;
  std::array<uint8_t, 16> out{};
  uint8_t* out_ptr = out.data();
  __asm__ volatile(
      R"""(
      .arch_extension sve
      rdvl %[vl], #1
      ptrue p0.b
      index z0.b, #1, #1
      st1b z0.b, p0, [%[out_ptr]]
      .arch_extension nosve
      )"""
      : [vl] "=&r"(vl), "=m"(out)
      : [out_ptr] "r"(out_ptr)
      : "z0", "p0");

  EXPECT_EQ(vl, 16u);
  for (size_t i = 0; i < out.size(); ++i) {
    EXPECT_EQ(out[i], static_cast<uint8_t>(i + 1));
  }
}

TEST(CpuFeatureTests, ArmSveSubfeatures) {
  uint32_t cpu_features = 0;
  ASSERT_OK(zx_system_get_features(ZX_FEATURE_KIND_CPU, &cpu_features));

  uint32_t sve_features = 0xffffffff;
  ASSERT_OK(zx_system_get_features(ZX_FEATURE_KIND_CPU_SVE, &sve_features));

  if ((cpu_features & ZX_ARM64_FEATURE_ISA_SVE) == 0) {
    EXPECT_EQ(sve_features, 0u);
    ZXTEST_SKIP("FEAT_SVE not detected");
    return;
  }

  constexpr uint32_t kSve2Subfeatures =
      ZX_ARM64_FEATURE_ISA_SVEAES | ZX_ARM64_FEATURE_ISA_SVEPMULL |
      ZX_ARM64_FEATURE_ISA_SVEBITPERM | ZX_ARM64_FEATURE_ISA_SVESHA3 | ZX_ARM64_FEATURE_ISA_SVESM4;
  constexpr uint32_t kKnownSveFeatures =
      ZX_ARM64_FEATURE_ISA_SVEI8MM | ZX_ARM64_FEATURE_ISA_SVEF32MM | ZX_ARM64_FEATURE_ISA_SVEF64MM |
      ZX_ARM64_FEATURE_ISA_SVEBF16 | ZX_ARM64_FEATURE_ISA_SVE2 | kSve2Subfeatures;
  EXPECT_EQ(sve_features & ~kKnownSveFeatures, 0u);

  if ((sve_features & ZX_ARM64_FEATURE_ISA_SVEPMULL) != 0) {
    EXPECT_NE(sve_features & ZX_ARM64_FEATURE_ISA_SVEAES, 0u);
  }
  if ((sve_features & kSve2Subfeatures) != 0) {
    EXPECT_NE(sve_features & ZX_ARM64_FEATURE_ISA_SVE2, 0u);
  }

  // Each subfeature that is reported must execute correctly, and each one that
  // is not reported must trap (ASSERT_DEATH). ZX_FEATURE_KIND_CPU_SVE is the
  // intersection of the subfeatures of all CPUs, so the negative checks assume
  // that SVE subfeatures are symmetric across CPUs: on an asymmetric system the
  // instruction could execute successfully on the CPU running the test even
  // though the subfeature is not reported system-wide.
  if ((sve_features & ZX_ARM64_FEATURE_ISA_SVEI8MM) != 0) {
    uint32_t res = 0;
    __asm__ volatile(
        R"""(
        .arch_extension sve
        .arch_extension i8mm
        mov z0.s, #0
        mov z1.b, #1
        mov z2.b, #2
        smmla z0.s, z1.b, z2.b
        umov %w[res], v0.s[0]
        .arch_extension noi8mm
        .arch_extension nosve
        )"""
        : [res] "=r"(res)
        :
        : "z0", "z1", "z2");
    EXPECT_EQ(res, 16u);
  } else {
    ASSERT_DEATH([] {
      __asm__ volatile(
          R"""(
          .arch_extension sve
          .arch_extension i8mm
          smmla z0.s, z1.b, z2.b
          .arch_extension noi8mm
          .arch_extension nosve
          )"""
          :
          :
          : "z0", "z1", "z2");
    });
  }

  if ((sve_features & ZX_ARM64_FEATURE_ISA_SVEF32MM) != 0) {
    uint32_t res = 0;
    __asm__ volatile(
        R"""(
        .arch_extension sve
        .arch_extension f32mm
        mov z0.s, #0
        fmov z1.s, #1.0
        fmov z2.s, #2.0
        fmmla z0.s, z1.s, z2.s
        umov %w[res], v0.s[0]
        .arch_extension nof32mm
        .arch_extension nosve
        )"""
        : [res] "=r"(res)
        :
        : "z0", "z1", "z2");
    EXPECT_EQ(res, 0x40800000u);
  } else {
    ASSERT_DEATH([] {
      __asm__ volatile(
          R"""(
          .arch_extension sve
          .arch_extension f32mm
          fmmla z0.s, z1.s, z2.s
          .arch_extension nof32mm
          .arch_extension nosve
          )"""
          :
          :
          : "z0", "z1", "z2");
    });
  }

  // FEAT_F64MM instructions require CurrentVL >= 256 bits and are UNDEFINED at
  // 128-bit vector length, so ZX_ARM64_FEATURE_ISA_SVEF64MM is never reported
  // while the kernel configures a 128-bit vector length. (Note: QEMU TCG
  // translates FMMLA_d without checking CurrentVL < 256, executing as a no-op
  // rather than trapping.)
  EXPECT_EQ(sve_features & ZX_ARM64_FEATURE_ISA_SVEF64MM, 0u);

  if ((sve_features & ZX_ARM64_FEATURE_ISA_SVEBF16) != 0) {
    uint32_t res = 0;
    __asm__ volatile(
        R"""(
        .arch_extension sve
        .arch_extension bf16
        mov z0.s, #0
        mov z1.h, #0x4000
        mov z2.h, #0x4000
        bfdot z0.s, z1.h, z2.h
        umov %w[res], v0.s[0]
        .arch_extension nobf16
        .arch_extension nosve
        )"""
        : [res] "=r"(res)
        :
        : "z0", "z1", "z2");
    EXPECT_EQ(res, 0x41000000u);
  } else {
    ASSERT_DEATH([] {
      __asm__ volatile(
          R"""(
          .arch_extension sve
          .arch_extension bf16
          bfdot z0.s, z1.h, z2.h
          .arch_extension nobf16
          .arch_extension nosve
          )"""
          :
          :
          : "z0", "z1", "z2");
    });
  }

  if ((sve_features & ZX_ARM64_FEATURE_ISA_SVE2) != 0) {
    uint32_t res = 0;
    __asm__ volatile(
        R"""(
        .arch_extension sve2
        mov z0.b, #1
        mov z1.b, #2
        cadd z0.b, z0.b, z1.b, #90
        umov %w[res], v0.h[0]
        .arch_extension nosve2
        .arch_extension nosve
        )"""
        : [res] "=r"(res)
        :
        : "z0", "z1");
    EXPECT_EQ(res, 0x03ffu);
  } else {
    ASSERT_DEATH([] {
      __asm__ volatile(
          R"""(
          .arch_extension sve2
          cadd z0.b, z0.b, z1.b, #90
          .arch_extension nosve2
          .arch_extension nosve
          )"""
          :
          :
          : "z0", "z1");
    });
  }

  if ((sve_features & ZX_ARM64_FEATURE_ISA_SVEAES) != 0) {
    uint32_t res = 0;
    __asm__ volatile(
        R"""(
        .arch_extension sve2-aes
        mov z0.b, #0
        mov z1.b, #0
        aese z0.b, z0.b, z1.b
        umov %w[res], v0.b[0]
        .arch_extension nosve2-aes
        .arch_extension nosve2
        .arch_extension nosve
        )"""
        : [res] "=r"(res)
        :
        : "z0", "z1");
    EXPECT_EQ(res, 0x63u);
  } else {
    ASSERT_DEATH([] {
      __asm__ volatile(
          R"""(
          .arch_extension sve2-aes
          aese z0.b, z0.b, z1.b
          .arch_extension nosve2-aes
          .arch_extension nosve2
          .arch_extension nosve
          )"""
          :
          :
          : "z0", "z1");
    });
  }

  if ((sve_features & ZX_ARM64_FEATURE_ISA_SVEPMULL) != 0) {
    uint64_t res = 0;
    __asm__ volatile(
        R"""(
        .arch_extension sve2-aes
        mov z1.d, #3
        mov z2.d, #5
        pmullb z0.q, z1.d, z2.d
        fmov %[res], d0
        .arch_extension nosve2-aes
        .arch_extension nosve2
        .arch_extension nosve
        )"""
        : [res] "=r"(res)
        :
        : "z0", "z1", "z2");
    EXPECT_EQ(res, 15u);
  } else {
    ASSERT_DEATH([] {
      __asm__ volatile(
          R"""(
          .arch_extension sve2-aes
          pmullb z0.q, z1.d, z2.d
          .arch_extension nosve2-aes
          .arch_extension nosve2
          .arch_extension nosve
          )"""
          :
          :
          : "z0", "z1", "z2");
    });
  }

  if ((sve_features & ZX_ARM64_FEATURE_ISA_SVEBITPERM) != 0) {
    uint32_t res = 0;
    __asm__ volatile(
        R"""(
        .arch_extension sve2-bitperm
        mov z1.b, #0b1010
        mov z2.b, #0b1100
        bext z0.b, z1.b, z2.b
        umov %w[res], v0.b[0]
        .arch_extension nosve2-bitperm
        .arch_extension nosve2
        .arch_extension nosve
        )"""
        : [res] "=r"(res)
        :
        : "z0", "z1", "z2");
    EXPECT_EQ(res, 0b10u);
  } else {
    ASSERT_DEATH([] {
      __asm__ volatile(
          R"""(
          .arch_extension sve2-bitperm
          bext z0.b, z1.b, z2.b
          .arch_extension nosve2-bitperm
          .arch_extension nosve2
          .arch_extension nosve
          )"""
          :
          :
          : "z0", "z1", "z2");
    });
  }

  if ((sve_features & ZX_ARM64_FEATURE_ISA_SVESHA3) != 0) {
    uint64_t res = 0;
    __asm__ volatile(
        R"""(
        .arch_extension sve2-sha3
        mov z1.d, #1
        mov z2.d, #2
        rax1 z0.d, z1.d, z2.d
        fmov %[res], d0
        .arch_extension nosve2-sha3
        .arch_extension nosve2
        .arch_extension nosve
        )"""
        : [res] "=r"(res)
        :
        : "z0", "z1", "z2");
    EXPECT_EQ(res, 5u);
  } else {
    ASSERT_DEATH([] {
      __asm__ volatile(
          R"""(
          .arch_extension sve2-sha3
          rax1 z0.d, z1.d, z2.d
          .arch_extension nosve2-sha3
          .arch_extension nosve2
          .arch_extension nosve
          )"""
          :
          :
          : "z0", "z1", "z2");
    });
  }

  if ((sve_features & ZX_ARM64_FEATURE_ISA_SVESM4) != 0) {
    uint32_t res = 0;
    __asm__ volatile(
        R"""(
        .arch_extension sve2-sm4
        mov z0.s, #0
        mov z1.s, #0
        sm4e z0.s, z0.s, z1.s
        umov %w[res], v0.s[0]
        .arch_extension nosve2-sm4
        .arch_extension nosve2
        .arch_extension nosve
        )"""
        : [res] "=r"(res)
        :
        : "z0", "z1");
    EXPECT_EQ(res, 0x5b5b5b5bu);
  } else {
    ASSERT_DEATH([] {
      __asm__ volatile(
          R"""(
          .arch_extension sve2-sm4
          sm4e z0.s, z0.s, z1.s
          .arch_extension nosve2-sm4
          .arch_extension nosve2
          .arch_extension nosve
          )"""
          :
          :
          : "z0", "z1");
    });
  }
}

#else

TEST(CpuFeatureTests, NonArmSveFeaturesAreZero) {
  uint32_t sve_features = 0xffffffff;
  ASSERT_OK(zx_system_get_features(ZX_FEATURE_KIND_CPU_SVE, &sve_features));
  EXPECT_EQ(sve_features, 0u);
}

#endif

}  // namespace
