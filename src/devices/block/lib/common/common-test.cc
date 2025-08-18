// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/block/lib/common/include/common.h"

#include <lib/driver/testing/cpp/scoped_global_logger.h>

#include <zxtest/zxtest.h>

namespace block {

class TestWithLogger : public zxtest::Test {
 public:
 protected:
  fdf_testing::ScopedGlobalLogger logger_;
};

TEST_F(TestWithLogger, CheckIoRangeTest) {
  block_read_write rw;

  rw = {
      .length = 0,
      .offset_dev = 10,
  };
  EXPECT_EQ(CheckIoRange(rw, 100, logger_.logger()), ZX_ERR_OUT_OF_RANGE);

  rw = {
      .length = 11,
      .offset_dev = 90,
  };
  EXPECT_EQ(CheckIoRange(rw, 100, logger_.logger()), ZX_ERR_OUT_OF_RANGE);

  rw = {
      .length = 1,
      .offset_dev = 100,
  };
  EXPECT_EQ(CheckIoRange(rw, 100, logger_.logger()), ZX_ERR_OUT_OF_RANGE);

  rw = {
      .length = 2,
      .offset_dev = 99,
  };
  EXPECT_EQ(CheckIoRange(rw, 100, logger_.logger()), ZX_ERR_OUT_OF_RANGE);

  rw = {
      .length = 101,
      .offset_dev = 0,
  };
  EXPECT_EQ(CheckIoRange(rw, 100, logger_.logger()), ZX_ERR_OUT_OF_RANGE);

  rw = {
      .length = 1,
      .offset_dev = 0,
  };
  EXPECT_OK(CheckIoRange(rw, 100, logger_.logger()));

  rw = {
      .length = 1,
      .offset_dev = 99,
  };
  EXPECT_OK(CheckIoRange(rw, 100, logger_.logger()));

  rw = {
      .length = 100,
      .offset_dev = 0,
  };
  EXPECT_OK(CheckIoRange(rw, 100, logger_.logger()));
}

TEST_F(TestWithLogger, CheckIoRangeMaxTransferTest) {
  block_trim trim;

  trim = {
      .length = 26,
      .offset_dev = 0,
  };
  EXPECT_EQ(CheckIoRange(trim, 100, 25, logger_.logger()), ZX_ERR_OUT_OF_RANGE);

  trim = {
      .length = 2,
      .offset_dev = 99,
  };
  EXPECT_EQ(CheckIoRange(trim, 100, 25, logger_.logger()), ZX_ERR_OUT_OF_RANGE);

  trim = {
      .length = 25,
      .offset_dev = 0,
  };
  EXPECT_OK(CheckIoRange(trim, 100, 25, logger_.logger()));
}

TEST_F(TestWithLogger, CheckFlushValidTest) {
  block_read_write rw;

  rw = {
      .vmo = 1,
      .length = 0,
      .offset_dev = 0,
      .offset_vmo = 0,
  };
  EXPECT_EQ(CheckFlushValid(rw, logger_.logger()), ZX_ERR_INVALID_ARGS);

  rw = {
      .vmo = ZX_HANDLE_INVALID,
      .length = 2,
      .offset_dev = 0,
      .offset_vmo = 0,
  };
  EXPECT_EQ(CheckFlushValid(rw, logger_.logger()), ZX_ERR_INVALID_ARGS);

  rw = {
      .vmo = ZX_HANDLE_INVALID,
      .length = 0,
      .offset_dev = 3,
      .offset_vmo = 0,
  };
  EXPECT_EQ(CheckFlushValid(rw, logger_.logger()), ZX_ERR_INVALID_ARGS);

  rw = {
      .vmo = ZX_HANDLE_INVALID,
      .length = 0,
      .offset_dev = 0,
      .offset_vmo = 4,
  };
  EXPECT_EQ(CheckFlushValid(rw, logger_.logger()), ZX_ERR_INVALID_ARGS);

  rw = {
      .vmo = ZX_HANDLE_INVALID,
      .length = 0,
      .offset_dev = 0,
      .offset_vmo = 0,
  };
  EXPECT_OK(CheckFlushValid(rw, logger_.logger()));
}

TEST(EndianTest, BigEndian24Test) {
  uint8_t memory[3] = {};
  EXPECT_OK(WriteToBigEndian24(0x654321, memory));
  EXPECT_EQ(memory[0], 0x65);  // MSB
  EXPECT_EQ(memory[1], 0x43);
  EXPECT_EQ(memory[2], 0x21);  // LSB

  EXPECT_EQ(WriteToBigEndian24(0x1000000, memory), ZX_ERR_OUT_OF_RANGE);

  EXPECT_EQ(ReadFromBigEndian24(memory), 0x654321);
}

TEST(EndianTest, LittleEndian24Test) {
  uint8_t memory[3] = {};
  EXPECT_OK(WriteToLittleEndian24(0x654321, memory));
  EXPECT_EQ(memory[0], 0x21);  // LSB
  EXPECT_EQ(memory[1], 0x43);
  EXPECT_EQ(memory[2], 0x65);  // MSB

  EXPECT_EQ(WriteToLittleEndian24(0x1000000, memory), ZX_ERR_OUT_OF_RANGE);

  EXPECT_EQ(ReadFromLittleEndian24(memory), 0x654321);
}

}  // namespace block
