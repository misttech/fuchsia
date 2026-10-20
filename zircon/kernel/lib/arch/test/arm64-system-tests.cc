// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/arch/arm64/feature.h>
#include <lib/arch/arm64/memory.h>
#include <lib/arch/arm64/system.h>

#include <algorithm>
#include <utility>
#include <variant>
#include <vector>

#include <gtest/gtest.h>

namespace {

bool ExpectAttrsConsistent(arch::ArmMemoryAttrIndirectionRegister reg) {
  EXPECT_EQ(reg.attr0(), reg.GetAttributeValue(0));
  EXPECT_EQ(reg.attr1(), reg.GetAttributeValue(1));
  EXPECT_EQ(reg.attr2(), reg.GetAttributeValue(2));
  EXPECT_EQ(reg.attr3(), reg.GetAttributeValue(3));
  EXPECT_EQ(reg.attr4(), reg.GetAttributeValue(4));
  EXPECT_EQ(reg.attr5(), reg.GetAttributeValue(5));
  EXPECT_EQ(reg.attr6(), reg.GetAttributeValue(6));
  EXPECT_EQ(reg.attr7(), reg.GetAttributeValue(7));
  return true;
}

TEST(Arm64System, MairGetSetAttribute) {
  arch::ArmMemoryAttrIndirectionRegister val{};

  // Ensure everything consistent to begin with.
  EXPECT_EQ(val.reg_value(), 0u);
  ExpectAttrsConsistent(val);

  // Set some attributes by the setters. Ensure everything is consistent.
  val.set_attr0(0b1111'1111);
  ExpectAttrsConsistent(val);
  val.set_attr3(0b0000'0100);
  ExpectAttrsConsistent(val);
  val.set_attr7(0b0100'0100);
  ExpectAttrsConsistent(val);

  // Set some attributes using SetAttribute. Ensure everything is consistent.
  val.SetAttributeValue(0, 0b0100'0100);
  ExpectAttrsConsistent(val);
  val.SetAttributeValue(3, 0b1111'1111);
  ExpectAttrsConsistent(val);
  val.SetAttributeValue(7, 0b0000'0100);
  ExpectAttrsConsistent(val);
}

TEST(Arm64System, MairAttributeTranslation) {
  constexpr struct {
    const char* name;
    std::optional<arch::ArmMairAttribute> attr;
    uint8_t value;
  } kTestCases[] = {
      {
          .name = "nGnRnE",
          .attr = arch::ArmDeviceMemory::kNonGatheringNonReorderingNoEarlyAck,
          .value = 0b0000'0000,
      },
      {
          .name = "nGnRE",
          .attr = arch::ArmDeviceMemory::kNonGatheringNonReorderingEarlyAck,
          .value = 0b0000'0100,
      },
      {
          .name = "nGRE",
          .attr = arch::ArmDeviceMemory::kNonGatheringReorderingEarlyAck,
          .value = 0b0000'1000,
      },
      {
          .name = "GRE",
          .attr = arch::ArmDeviceMemory::kGatheringReorderingEarlyAck,
          .value = 0b0000'1100,
      },
      {
          .name = "inner/outer kNonCacheable",
          .attr =
              arch::ArmMairNormalAttribute{
                  .inner = arch::ArmCacheabilityAttribute::kNonCacheable,
                  .outer = arch::ArmCacheabilityAttribute::kNonCacheable,
              },
          .value = 0b0100'0100,
      },
      {
          .name = "inner/outer kWriteBackReadWriteAllocate",
          .attr =
              arch::ArmMairNormalAttribute{
                  .inner = arch::ArmCacheabilityAttribute::kWriteBackReadWriteAllocate,
                  .outer = arch::ArmCacheabilityAttribute::kWriteBackReadWriteAllocate,
              },
          .value = 0b1111'1111,
      },
      {
          .name = "inner/outer kWriteThroughReadAllocate",
          .attr =
              arch::ArmMairNormalAttribute{
                  .inner = arch::ArmCacheabilityAttribute::kWriteThroughReadAllocate,
                  .outer = arch::ArmCacheabilityAttribute::kWriteThroughReadAllocate,
              },
          .value = 0b1010'1010,
      },
      {
          .name = "inner/outer kWriteBackReadAllocate",
          .attr =
              arch::ArmMairNormalAttribute{
                  .inner = arch::ArmCacheabilityAttribute::kWriteBackReadAllocate,
                  .outer = arch::ArmCacheabilityAttribute::kWriteBackReadAllocate,
              },
          .value = 0b1110'1110,
      },
      {
          .name = "inner kNonCacheable; outer kWriteBackReadAllocate",
          .attr =
              arch::ArmMairNormalAttribute{
                  .inner = arch::ArmCacheabilityAttribute::kNonCacheable,
                  .outer = arch::ArmCacheabilityAttribute::kWriteBackReadAllocate,
              },
          .value = 0b1110'0100,
      },
      {
          .name = "inner kWriteThroughReadAllocate; outer kWriteBackReadWriteAllocate",
          .attr =
              arch::ArmMairNormalAttribute{
                  .inner = arch::ArmCacheabilityAttribute::kWriteThroughReadAllocate,
                  .outer = arch::ArmCacheabilityAttribute::kWriteBackReadWriteAllocate,
              },
          .value = 0b1111'1010,
      },
      {
          .name = "0b1111'0000: invalid",
          .attr = std::nullopt,
          .value = 0b1111'0000,
      },
      {
          .name = "0b1111'1011: valid software; invalid hardware",
          .attr = std::nullopt,
          .value = 0b1111'1011,  // Inner write-through read *and write* allocating.
      },
  };

  for (const auto& test : kTestCases) {
    EXPECT_EQ(test.attr, arch::ArmMemoryAttrIndirectionRegister::AttributeFromValue(test.value))
        << test.name;
    if (test.attr) {
      EXPECT_EQ(test.value, arch::ArmMemoryAttrIndirectionRegister::AttributeToValue(*test.attr))
          << test.name;
    }
  }
}

TEST(Arm64System, ZcrEl1) {
  arch::ArmZcrEl1 zcr{};
  EXPECT_EQ(zcr.reg_value(), 0u);

  zcr.set_len(0xf);
  EXPECT_EQ(zcr.reg_value(), 0xfu);
  EXPECT_EQ(zcr.len(), 0xfu);

  zcr.set_impdef(0x1f);
  EXPECT_EQ(zcr.reg_value(), 0x1ffu);
  EXPECT_EQ(zcr.impdef(), 0x1fu);
}

TEST(Arm64System, CpacrEl1) {
  arch::ArmCpacrEl1 cpacr{};
  EXPECT_EQ(cpacr.reg_value(), 0u);

  cpacr.set_zen(arch::ArmCpacrEl1::Trap::kTrapNone);
  EXPECT_EQ(cpacr.reg_value(), 3ULL << 16);
  EXPECT_EQ(cpacr.zen(), arch::ArmCpacrEl1::Trap::kTrapNone);

  cpacr.set_fpen(arch::ArmCpacrEl1::Trap::kTrapEl0);
  EXPECT_EQ(cpacr.reg_value(), (1ULL << 20) | (3ULL << 16));
  EXPECT_EQ(cpacr.fpen(), arch::ArmCpacrEl1::Trap::kTrapEl0);

  cpacr.set_smen(arch::ArmCpacrEl1::Trap::kTrapAllAlt);
  EXPECT_EQ(cpacr.reg_value(), (2ULL << 24) | (1ULL << 20) | (3ULL << 16));
  EXPECT_EQ(cpacr.smen(), arch::ArmCpacrEl1::Trap::kTrapAllAlt);
}

TEST(Arm64System, IdAa64Zfr0El1) {
  arch::ArmIdAa64Pfr0El1 pfr0{};
  EXPECT_EQ(pfr0.sve(), arch::ArmIdAa64Pfr0El1::Sve::kNone);
  pfr0.set_reg_value(1ULL << 32);
  EXPECT_EQ(pfr0.sve(), arch::ArmIdAa64Pfr0El1::Sve::kSve);

  arch::ArmIdAa64Zfr0El1 zfr0{};
  EXPECT_EQ(zfr0.reg_value(), 0u);
  EXPECT_EQ(zfr0.sve_ver(), arch::ArmIdAa64Zfr0El1::SveVer::kSve);
  EXPECT_EQ(zfr0.aes(), arch::ArmIdAa64Zfr0El1::Aes::kNone);
  EXPECT_EQ(zfr0.elt_perm(), arch::ArmIdAa64Zfr0El1::EltPerm::kNone);
  EXPECT_EQ(zfr0.bit_perm(), arch::ArmIdAa64Zfr0El1::BitPerm::kNone);
  EXPECT_EQ(zfr0.bf16(), arch::ArmIdAa64Zfr0El1::Bf16::kNone);
  EXPECT_EQ(zfr0.b16b16(), arch::ArmIdAa64Zfr0El1::B16b16::kNone);
  EXPECT_EQ(zfr0.sha3(), arch::ArmIdAa64Zfr0El1::Sha3::kNone);
  EXPECT_EQ(zfr0.sm4(), arch::ArmIdAa64Zfr0El1::Sm4::kNone);
  EXPECT_EQ(zfr0.i8mm(), arch::ArmIdAa64Zfr0El1::I8mm::kNone);
  EXPECT_EQ(zfr0.f16mm(), arch::ArmIdAa64Zfr0El1::F16mm::kNone);
  EXPECT_EQ(zfr0.f32mm(), arch::ArmIdAa64Zfr0El1::F32mm::kNone);
  EXPECT_EQ(zfr0.f64mm(), arch::ArmIdAa64Zfr0El1::F64mm::kNone);

  zfr0.set_sve_ver(arch::ArmIdAa64Zfr0El1::SveVer::kSve2)
      .set_aes(arch::ArmIdAa64Zfr0El1::Aes::kPmull)
      .set_elt_perm(arch::ArmIdAa64Zfr0El1::EltPerm::kEltPerm)
      .set_bit_perm(arch::ArmIdAa64Zfr0El1::BitPerm::kBitPerm)
      .set_bf16(arch::ArmIdAa64Zfr0El1::Bf16::kEbf16)
      .set_b16b16(arch::ArmIdAa64Zfr0El1::B16b16::kB16b16)
      .set_sha3(arch::ArmIdAa64Zfr0El1::Sha3::kSha3)
      .set_sm4(arch::ArmIdAa64Zfr0El1::Sm4::kSm4)
      .set_i8mm(arch::ArmIdAa64Zfr0El1::I8mm::kI8mm)
      .set_f16mm(arch::ArmIdAa64Zfr0El1::F16mm::kF16mm)
      .set_f32mm(arch::ArmIdAa64Zfr0El1::F32mm::kF32mm)
      .set_f64mm(arch::ArmIdAa64Zfr0El1::F64mm::kF64mm);

  constexpr uint64_t kExpected = (1ULL << 56) | (1ULL << 52) | (1ULL << 48) | (1ULL << 44) |
                                 (1ULL << 40) | (1ULL << 32) | (1ULL << 24) | (2ULL << 20) |
                                 (1ULL << 16) | (1ULL << 12) | (2ULL << 4) | 1ULL;
  EXPECT_EQ(zfr0.reg_value(), kExpected);
  EXPECT_EQ(zfr0.sve_ver(), arch::ArmIdAa64Zfr0El1::SveVer::kSve2);
  EXPECT_EQ(zfr0.aes(), arch::ArmIdAa64Zfr0El1::Aes::kPmull);
  EXPECT_EQ(zfr0.elt_perm(), arch::ArmIdAa64Zfr0El1::EltPerm::kEltPerm);
  EXPECT_EQ(zfr0.bit_perm(), arch::ArmIdAa64Zfr0El1::BitPerm::kBitPerm);
  EXPECT_EQ(zfr0.bf16(), arch::ArmIdAa64Zfr0El1::Bf16::kEbf16);
  EXPECT_EQ(zfr0.b16b16(), arch::ArmIdAa64Zfr0El1::B16b16::kB16b16);
  EXPECT_EQ(zfr0.sha3(), arch::ArmIdAa64Zfr0El1::Sha3::kSha3);
  EXPECT_EQ(zfr0.sm4(), arch::ArmIdAa64Zfr0El1::Sm4::kSm4);
  EXPECT_EQ(zfr0.i8mm(), arch::ArmIdAa64Zfr0El1::I8mm::kI8mm);
  EXPECT_EQ(zfr0.f16mm(), arch::ArmIdAa64Zfr0El1::F16mm::kF16mm);
  EXPECT_EQ(zfr0.f32mm(), arch::ArmIdAa64Zfr0El1::F32mm::kF32mm);
  EXPECT_EQ(zfr0.f64mm(), arch::ArmIdAa64Zfr0El1::F64mm::kF64mm);
}

TEST(Arm64System, IdAa64Zfr0El1Enumerators) {
  using Zfr0 = arch::ArmIdAa64Zfr0El1;

  // Enumerator encodings per the Arm ARM description of ID_AA64ZFR0_EL1.
  static_assert(static_cast<uint8_t>(Zfr0::SveVer::kSve) == 0b0000);
  static_assert(static_cast<uint8_t>(Zfr0::SveVer::kSve2) == 0b0001);
  static_assert(static_cast<uint8_t>(Zfr0::SveVer::kSve2p1) == 0b0010);
  static_assert(static_cast<uint8_t>(Zfr0::SveVer::kSve2p2) == 0b0011);
  static_assert(static_cast<uint8_t>(Zfr0::Aes::kNone) == 0b0000);
  static_assert(static_cast<uint8_t>(Zfr0::Aes::kAes) == 0b0001);
  static_assert(static_cast<uint8_t>(Zfr0::Aes::kPmull) == 0b0010);
  static_assert(static_cast<uint8_t>(Zfr0::Aes::kAes2) == 0b0011);
  static_assert(static_cast<uint8_t>(Zfr0::B16b16::kNone) == 0b0000);
  static_assert(static_cast<uint8_t>(Zfr0::B16b16::kB16b16) == 0b0001);
  static_assert(static_cast<uint8_t>(Zfr0::B16b16::kBfscale) == 0b0010);
  static_assert(static_cast<uint8_t>(Zfr0::Bf16::kBf16) == 0b0001);
  static_assert(static_cast<uint8_t>(Zfr0::Bf16::kEbf16) == 0b0010);
  static_assert(static_cast<uint8_t>(Zfr0::F16mm::kNone) == 0b0000);
  static_assert(static_cast<uint8_t>(Zfr0::F16mm::kF16mm) == 0b0001);

  // Decoding raw register values: every enumerator of the multi-valued fields
  // is recognized, and each field only sees its own bits.
  Zfr0 zfr0{};
  for (auto [raw, sve_ver] : {
           std::pair{0b0000ULL, Zfr0::SveVer::kSve},
           std::pair{0b0001ULL, Zfr0::SveVer::kSve2},
           std::pair{0b0010ULL, Zfr0::SveVer::kSve2p1},
           std::pair{0b0011ULL, Zfr0::SveVer::kSve2p2},
       }) {
    zfr0.set_reg_value(raw);
    EXPECT_EQ(zfr0.sve_ver(), sve_ver) << raw;
    EXPECT_EQ(zfr0.aes(), Zfr0::Aes::kNone) << raw;
  }
  for (auto [raw, aes] : {
           std::pair{0b0001ULL << 4, Zfr0::Aes::kAes},
           std::pair{0b0010ULL << 4, Zfr0::Aes::kPmull},
           std::pair{0b0011ULL << 4, Zfr0::Aes::kAes2},
       }) {
    zfr0.set_reg_value(raw);
    EXPECT_EQ(zfr0.aes(), aes) << raw;
    EXPECT_EQ(zfr0.sve_ver(), Zfr0::SveVer::kSve) << raw;
    EXPECT_EQ(zfr0.elt_perm(), Zfr0::EltPerm::kNone) << raw;
  }
  for (auto [raw, bf16] : {
           std::pair{0b0001ULL << 20, Zfr0::Bf16::kBf16},
           std::pair{0b0010ULL << 20, Zfr0::Bf16::kEbf16},
       }) {
    zfr0.set_reg_value(raw);
    EXPECT_EQ(zfr0.bf16(), bf16) << raw;
    EXPECT_EQ(zfr0.bit_perm(), Zfr0::BitPerm::kNone) << raw;
    EXPECT_EQ(zfr0.b16b16(), Zfr0::B16b16::kNone) << raw;
  }
  for (auto [raw, b16b16] : {
           std::pair{0b0001ULL << 24, Zfr0::B16b16::kB16b16},
           std::pair{0b0010ULL << 24, Zfr0::B16b16::kBfscale},
       }) {
    zfr0.set_reg_value(raw);
    EXPECT_EQ(zfr0.b16b16(), b16b16) << raw;
    EXPECT_EQ(zfr0.bf16(), Zfr0::Bf16::kNone) << raw;
    EXPECT_EQ(zfr0.sha3(), Zfr0::Sha3::kNone) << raw;
  }

  // F16MM occupies bits [51:48], between I8MM [47:44] and F32MM [55:52].
  zfr0.set_reg_value(1ULL << 48);
  EXPECT_EQ(zfr0.f16mm(), Zfr0::F16mm::kF16mm);
  EXPECT_EQ(zfr0.i8mm(), Zfr0::I8mm::kNone);
  EXPECT_EQ(zfr0.f32mm(), Zfr0::F32mm::kNone);
  EXPECT_EQ(zfr0.f64mm(), Zfr0::F64mm::kNone);
  zfr0.set_reg_value((1ULL << 52) | (1ULL << 44));
  EXPECT_EQ(zfr0.f16mm(), Zfr0::F16mm::kNone);
  EXPECT_EQ(zfr0.i8mm(), Zfr0::I8mm::kI8mm);
  EXPECT_EQ(zfr0.f32mm(), Zfr0::F32mm::kF32mm);

  // The RES0 ranges ([63:60], [39:36], [31:28], and [11:8]) do not alias any
  // field.
  constexpr uint64_t kRes0Mask = (0xfULL << 60) | (0xfULL << 36) | (0xfULL << 28) | (0xfULL << 8);
  zfr0.set_reg_value(kRes0Mask);
  EXPECT_EQ(zfr0.sve_ver(), Zfr0::SveVer::kSve);
  EXPECT_EQ(zfr0.aes(), Zfr0::Aes::kNone);
  EXPECT_EQ(zfr0.elt_perm(), Zfr0::EltPerm::kNone);
  EXPECT_EQ(zfr0.bit_perm(), Zfr0::BitPerm::kNone);
  EXPECT_EQ(zfr0.bf16(), Zfr0::Bf16::kNone);
  EXPECT_EQ(zfr0.b16b16(), Zfr0::B16b16::kNone);
  EXPECT_EQ(zfr0.sha3(), Zfr0::Sha3::kNone);
  EXPECT_EQ(zfr0.sm4(), Zfr0::Sm4::kNone);
  EXPECT_EQ(zfr0.i8mm(), Zfr0::I8mm::kNone);
  EXPECT_EQ(zfr0.f16mm(), Zfr0::F16mm::kNone);
  EXPECT_EQ(zfr0.f32mm(), Zfr0::F32mm::kNone);
  EXPECT_EQ(zfr0.f64mm(), Zfr0::F64mm::kNone);

  // Encoding the highest enumerator of every field.
  Zfr0 max{};
  max.set_sve_ver(Zfr0::SveVer::kSve2p2)
      .set_aes(Zfr0::Aes::kAes2)
      .set_elt_perm(Zfr0::EltPerm::kEltPerm)
      .set_bit_perm(Zfr0::BitPerm::kBitPerm)
      .set_bf16(Zfr0::Bf16::kEbf16)
      .set_b16b16(Zfr0::B16b16::kBfscale)
      .set_sha3(Zfr0::Sha3::kSha3)
      .set_sm4(Zfr0::Sm4::kSm4)
      .set_i8mm(Zfr0::I8mm::kI8mm)
      .set_f16mm(Zfr0::F16mm::kF16mm)
      .set_f32mm(Zfr0::F32mm::kF32mm)
      .set_f64mm(Zfr0::F64mm::kF64mm);
  constexpr uint64_t kExpectedMax = (1ULL << 56) | (1ULL << 52) | (1ULL << 48) | (1ULL << 44) |
                                    (1ULL << 40) | (1ULL << 32) | (2ULL << 24) | (2ULL << 20) |
                                    (1ULL << 16) | (1ULL << 12) | (3ULL << 4) | 3ULL;
  EXPECT_EQ(max.reg_value(), kExpectedMax);
  EXPECT_EQ(max.reg_value() & kRes0Mask, 0u);
  EXPECT_EQ(max.sve_ver(), Zfr0::SveVer::kSve2p2);
  EXPECT_EQ(max.aes(), Zfr0::Aes::kAes2);
  EXPECT_EQ(max.b16b16(), Zfr0::B16b16::kBfscale);
  EXPECT_EQ(max.bf16(), Zfr0::Bf16::kEbf16);
  EXPECT_EQ(max.f16mm(), Zfr0::F16mm::kF16mm);

  // Decoding the same raw value yields the same fields.
  Zfr0 decoded{};
  decoded.set_reg_value(kExpectedMax);
  EXPECT_EQ(decoded.sve_ver(), Zfr0::SveVer::kSve2p2);
  EXPECT_EQ(decoded.aes(), Zfr0::Aes::kAes2);
  EXPECT_EQ(decoded.elt_perm(), Zfr0::EltPerm::kEltPerm);
  EXPECT_EQ(decoded.bit_perm(), Zfr0::BitPerm::kBitPerm);
  EXPECT_EQ(decoded.bf16(), Zfr0::Bf16::kEbf16);
  EXPECT_EQ(decoded.b16b16(), Zfr0::B16b16::kBfscale);
  EXPECT_EQ(decoded.sha3(), Zfr0::Sha3::kSha3);
  EXPECT_EQ(decoded.sm4(), Zfr0::Sm4::kSm4);
  EXPECT_EQ(decoded.i8mm(), Zfr0::I8mm::kI8mm);
  EXPECT_EQ(decoded.f16mm(), Zfr0::F16mm::kF16mm);
  EXPECT_EQ(decoded.f32mm(), Zfr0::F32mm::kF32mm);
  EXPECT_EQ(decoded.f64mm(), Zfr0::F64mm::kF64mm);

  // Fields can be changed individually without disturbing their neighbours.
  decoded.set_aes(Zfr0::Aes::kAes).set_sve_ver(Zfr0::SveVer::kSve2p1);
  EXPECT_EQ(decoded.reg_value(), (kExpectedMax & ~0xffULL) | (1ULL << 4) | 2ULL);
  EXPECT_EQ(decoded.aes(), Zfr0::Aes::kAes);
  EXPECT_EQ(decoded.sve_ver(), Zfr0::SveVer::kSve2p1);
  EXPECT_EQ(decoded.elt_perm(), Zfr0::EltPerm::kEltPerm);
  decoded.set_b16b16(Zfr0::B16b16::kB16b16).set_f16mm(Zfr0::F16mm::kNone);
  EXPECT_EQ(decoded.reg_value(), (kExpectedMax & ~((0xfULL << 48) | (0xfULL << 24) | 0xffULL)) |
                                     (1ULL << 24) | (1ULL << 4) | 2ULL);
  EXPECT_EQ(decoded.b16b16(), Zfr0::B16b16::kB16b16);
  EXPECT_EQ(decoded.bf16(), Zfr0::Bf16::kEbf16);
  EXPECT_EQ(decoded.f16mm(), Zfr0::F16mm::kNone);
  EXPECT_EQ(decoded.i8mm(), Zfr0::I8mm::kI8mm);
  EXPECT_EQ(decoded.f32mm(), Zfr0::F32mm::kF32mm);
}

}  // namespace
