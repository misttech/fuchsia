// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <sys/auxv.h>

#include <array>
#include <cstdint>
#include <fstream>
#include <optional>
#include <ostream>
#include <sstream>
#include <string>
#include <string_view>

#include "gtest/gtest.h"

namespace {

#if defined(__arm__)
// The Linux sysroot headers predate these 32-bit ARM AT_HWCAP capability bits
// (`arch/arm/include/uapi/asm/hwcap.h`).
constexpr unsigned long kArch32HwcapFphp = 1UL << 22;
constexpr unsigned long kArch32HwcapAsimdhp = 1UL << 23;
constexpr unsigned long kArch32HwcapAsimdfhm = 1UL << 25;

constexpr std::array<std::string_view, 28> kHwcapStr = {
    "swp",      "half", "thumb",   "26bit",   "fastmult", "fpa",       "vfp",
    "edsp",     "java", "iwmmxt",  "crunch",  "thumbee",  "neon",      "vfpv3",
    "vfpv3d16", "tls",  "vfpv4",   "idiva",   "idivt",    "vfpd32",    "lpae",
    "evtstrm",  "fphp", "asimdhp", "asimddp", "asimdfhm", "asimdbf16", "i8mm",
};

constexpr std::array<std::string_view, 7> kHwcap2Str = {
    "aes", "pmull", "sha1", "sha2", "crc32", "sb", "ssbs",
};
#elif defined(__aarch64__)
// The Linux sysroot headers (`bits/hwcap.h`) predate these AArch64 AT_HWCAP/AT_HWCAP2 capability
// bits (`arch/arm64/include/uapi/asm/hwcap.h`).
constexpr unsigned long kHwcapFp = 1UL << 0;
constexpr unsigned long kHwcapAsimd = 1UL << 1;
constexpr unsigned long kHwcapFphp = 1UL << 9;
constexpr unsigned long kHwcapAsimdhp = 1UL << 10;
constexpr unsigned long kHwcapSve = 1UL << 22;
constexpr unsigned long kHwcap2Sve2 = 1UL << 1;

constexpr std::array<std::string_view, 32> kHwcapStr = {
    "fp",      "asimd", "evtstrm", "aes",   "pmull",    "sha1",   "sha2", "crc32",
    "atomics", "fphp",  "asimdhp", "cpuid", "asimdrdm", "jscvt",  "fcma", "lrcpc",
    "dcpop",   "sha3",  "sm3",     "sm4",   "asimddp",  "sha512", "sve",  "asimdfhm",
    "dit",     "uscat", "ilrcpc",  "flagm", "ssbs",     "sb",     "paca", "pacg",
};

constexpr std::array<std::string_view, 32> kHwcap2Str = {
    "dcpodp",    "sve2",      "sveaes",  "svepmull",  "svebitperm", "svesha3",  "svesm4",
    "flagm2",    "frint",     "svei8mm", "svef32mm",  "svef64mm",   "svebf16",  "i8mm",
    "bf16",      "dgh",       "rng",     "bti",       "mte",        "ecv",      "afp",
    "rpres",     "mte3",      "sme",     "smei16i64", "smef64f64",  "smei8i32", "smef16f32",
    "smeb16f32", "smef32f32", "smefa64", "wfxt",
};

enum class SveVersion : int {
  kDefault = 0,
  kSve = 1,
  kSve2 = 2,
};

std::ostream& operator<<(std::ostream& os, SveVersion version) {
  switch (version) {
    case SveVersion::kDefault:
      return os << "default";
    case SveVersion::kSve:
      return os << "sve";
    case SveVersion::kSve2:
      return os << "sve2";
  }
  return os << "unknown(" << static_cast<int>(version) << ")";
}

__attribute__((target_version("default"))) uint64_t SveVectorBytesOrZero() { return 0; }

__attribute__((target_version("sve"))) uint64_t SveVectorBytesOrZero() {
  uint64_t vl_bytes = 0;
  __asm__("cntb %0" : "=r"(vl_bytes));
  return vl_bytes;
}

__attribute__((target_version("default"))) SveVersion SelectSveOrDefault() {
  return SveVersion::kDefault;
}

__attribute__((target_version("sve"))) SveVersion SelectSveOrDefault() { return SveVersion::kSve; }

__attribute__((target_version("default"))) SveVersion SelectSve2OrDefault() {
  return SveVersion::kDefault;
}

__attribute__((target_version("sve2"))) SveVersion SelectSve2OrDefault() {
  __asm__ volatile("ptrue p0.s\n\thistcnt z0.s, p0/z, z0.s, z0.s" : : : "z0", "p0");
  return SveVersion::kSve2;
}

__attribute__((target_version("default"))) SveVersion SelectBestSveVersion() {
  return SveVersion::kDefault;
}

__attribute__((target_version("sve"))) SveVersion SelectBestSveVersion() {
  uint64_t vl_bytes = 0;
  __asm__ volatile("cntb %0" : "=r"(vl_bytes));
  (void)vl_bytes;
  return SveVersion::kSve;
}

__attribute__((target_version("sve2"))) SveVersion SelectBestSveVersion() {
  __asm__ volatile("ptrue p0.s\n\thistcnt z0.s, p0/z, z0.s, z0.s" : : : "z0", "p0");
  return SveVersion::kSve2;
}
#endif  // defined(__aarch64__)

class CpuinfoTest : public ::testing::Test {};

TEST_F(CpuinfoTest, Cpuinfo) {
  std::fstream cpuinfo("/proc/cpuinfo");
  ASSERT_TRUE(cpuinfo.is_open());

  std::optional<std::string> features_line;
  std::string line;
  while (std::getline(cpuinfo, line)) {
    if (!features_line && line.starts_with("Features\t:")) {
      features_line = line;
    }
  }
  EXPECT_TRUE(cpuinfo.eof());

#if defined(__arm__) || defined(__aarch64__)
  std::stringstream ss;
  ss << "Features\t:";
  for (size_t i = 0; i < kHwcapStr.size(); i++) {
    if (getauxval(AT_HWCAP) & (1UL << i)) {
      ss << " " << kHwcapStr[i];
    }
  }
  for (size_t i = 0; i < kHwcap2Str.size(); i++) {
    if (getauxval(AT_HWCAP2) & (1UL << i)) {
      ss << " " << kHwcap2Str[i];
    }
  }
  const auto expected_features_line = ss.str();

  ASSERT_TRUE(features_line.has_value());
  EXPECT_EQ(*features_line, expected_features_line);
#endif
}

#if defined(__arm__)
TEST_F(CpuinfoTest, Arch32HwcapInvariants) {
  const unsigned long hwcap = getauxval(AT_HWCAP);
  const unsigned long hwcap2 = getauxval(AT_HWCAP2);

  // SVE and SVE2 are AArch64-only (A64 ISA) extensions and do not exist in AArch32 execution state
  // or 32-bit ARM AT_HWCAP/AT_HWCAP2. Verify 32-bit AT_HWCAP/AT_HWCAP2 invariants (including
  // FEAT_FP16 HWCAP_FPHP/HWCAP_ASIMDHP when FEAT_FHM is present) and that /proc/cpuinfo does not
  // expose AArch64-only SVE/SVE2 feature tokens to a 32-bit process.
  EXPECT_EQ(hwcap & ~((1UL << kHwcapStr.size()) - 1), 0UL);
  EXPECT_EQ(hwcap2 & ~((1UL << kHwcap2Str.size()) - 1), 0UL);
  if ((hwcap & kArch32HwcapAsimdfhm) != 0) {
    EXPECT_NE(hwcap & kArch32HwcapFphp, 0UL);
    EXPECT_NE(hwcap & kArch32HwcapAsimdhp, 0UL);
  }

  std::ifstream cpuinfo("/proc/cpuinfo");
  ASSERT_TRUE(cpuinfo.is_open());
  std::string line;
  while (std::getline(cpuinfo, line)) {
    if (line.starts_with("Features\t:")) {
      std::istringstream tokens(line.substr(sizeof("Features\t:") - 1));
      std::string token;
      while (tokens >> token) {
        EXPECT_FALSE(token.starts_with("sve")) << "Unexpected SVE feature in arch32: " << token;
      }
    }
  }
}
#elif defined(__aarch64__)
TEST_F(CpuinfoTest, SveFunctionMultiVersioning) {
  const unsigned long hwcap = getauxval(AT_HWCAP);
  const unsigned long hwcap2 = getauxval(AT_HWCAP2);
  const bool has_sve = (hwcap & kHwcapSve) != 0;
  const bool has_sve2 = (hwcap2 & kHwcap2Sve2) != 0;

  if (has_sve2) {
    EXPECT_TRUE(has_sve) << "HWCAP2_SVE2 must not be reported without HWCAP_SVE";
  }
  if (has_sve) {
    // Per ACLE Function Multi-Versioning (FMV) dependency rules (used by LLVM/compiler-rt), "sve"
    // and "sve2" targets transitively require FEAT_FP (HWCAP_FP) and FEAT_FP16 (HWCAP_FPHP).
    EXPECT_NE(hwcap & kHwcapFp, 0UL);
    EXPECT_NE(hwcap & kHwcapAsimd, 0UL);
    EXPECT_NE(hwcap & kHwcapFphp, 0UL);
    EXPECT_NE(hwcap & kHwcapAsimdhp, 0UL);
  }

  EXPECT_EQ(static_cast<bool>(__builtin_cpu_supports("sve")), has_sve);
  EXPECT_EQ(static_cast<bool>(__builtin_cpu_supports("sve2")), has_sve2);

  if (has_sve) {
    EXPECT_EQ(SelectSveOrDefault(), SveVersion::kSve);
    const uint64_t vl_bytes = SveVectorBytesOrZero();
    EXPECT_GE(vl_bytes, 16u);
    EXPECT_LE(vl_bytes, 256u);
    EXPECT_EQ(vl_bytes % 16, 0u);
  } else {
    EXPECT_EQ(SelectSveOrDefault(), SveVersion::kDefault);
    EXPECT_EQ(SveVectorBytesOrZero(), 0u);
  }

  if (has_sve2) {
    EXPECT_EQ(SelectSve2OrDefault(), SveVersion::kSve2);
    EXPECT_EQ(SelectBestSveVersion(), SveVersion::kSve2);
  } else if (has_sve) {
    EXPECT_EQ(SelectSve2OrDefault(), SveVersion::kDefault);
    EXPECT_EQ(SelectBestSveVersion(), SveVersion::kSve);
  } else {
    EXPECT_EQ(SelectSve2OrDefault(), SveVersion::kDefault);
    EXPECT_EQ(SelectBestSveVersion(), SveVersion::kDefault);
  }
}
#endif  // defined(__aarch64__)

}  // namespace
