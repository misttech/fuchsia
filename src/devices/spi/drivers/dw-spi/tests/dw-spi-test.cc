// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "../dw-spi.h"

#include <lib/driver/mmio/cpp/mmio.h>
#include <lib/zx/interrupt.h>
#include <lib/zx/vmo.h>
#include <zircon/types.h>

#include <gtest/gtest.h>

#include "../registers.h"

namespace dw_spi {

class DwSpiTest : public ::testing::Test {
 protected:
  void SetUp() override {
    // Create a VMO for fake MMIO
    ASSERT_EQ(ZX_OK, zx::vmo::create(4096, 0, &vmo_));

    // Duplicate VMO for MmioBuffer
    zx::vmo vmo_dup;
    ASSERT_EQ(ZX_OK, vmo_.duplicate(ZX_RIGHT_SAME_RIGHTS, &vmo_dup));

    // Create MmioBuffer
    auto mmio_result = fdf::MmioBuffer::Create(0, 4096, std::move(vmo_dup), ZX_CACHE_POLICY_CACHED);
    ASSERT_TRUE(mmio_result.is_ok());

    // Create a virtual interrupt
    ASSERT_EQ(ZX_OK, zx::interrupt::create({}, 0, ZX_INTERRUPT_VIRTUAL, &interrupt_));

    // Duplicate for device
    zx::interrupt interrupt_dup;
    ASSERT_EQ(ZX_OK, interrupt_.duplicate(ZX_RIGHT_SAME_RIGHTS, &interrupt_dup));

    // Create device
    device_ = std::make_unique<DwSpi>(std::move(*mmio_result), std::move(interrupt_dup));
  }

  zx::vmo vmo_;
  zx::interrupt interrupt_;
  std::unique_ptr<DwSpi> device_;
};

TEST_F(DwSpiTest, InitRegisters) {
  device_->InitRegisters();

  // Verify CTRLR0
  // We expect SPI_FRF=0, FRF=0, DFS=7 (8-bit), TMOD=0
  uint32_t ctrlr0;
  ASSERT_EQ(ZX_OK, vmo_.read(&ctrlr0, DW_SPI_CTRLR0, sizeof(ctrlr0)));
  EXPECT_EQ(ctrlr0, 7u);  // DFS=7, others 0

  // Verify SSIENR
  uint32_t ssienr;
  ASSERT_EQ(ZX_OK, vmo_.read(&ssienr, DW_SPI_SSIENR, sizeof(ssienr)));
  EXPECT_EQ(ssienr, 1u);

  // Verify BAUDR
  uint32_t baudr;
  ASSERT_EQ(ZX_OK, vmo_.read(&baudr, DW_SPI_BAUDR, sizeof(baudr)));
  EXPECT_EQ(baudr, 2u);

  // Verify IMR
  uint32_t imr;
  ASSERT_EQ(ZX_OK, vmo_.read(&imr, DW_SPI_IMR, sizeof(imr)));
  EXPECT_EQ(imr, 0u);
}

}  // namespace dw_spi
