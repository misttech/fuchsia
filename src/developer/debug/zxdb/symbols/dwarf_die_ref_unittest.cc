// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/symbols/dwarf_die_ref.h"

#include <sstream>

#include <gtest/gtest.h>

namespace zxdb {

TEST(DwarfDieRef, Basics) {
  DwarfDieRef empty;
  EXPECT_FALSE(empty.is_valid());
  EXPECT_FALSE(static_cast<bool>(empty));

  DwarfDieRef zero_ref = DwarfDieRef::Main(0);
  EXPECT_TRUE(zero_ref.is_valid());
  EXPECT_TRUE(static_cast<bool>(zero_ref));
  EXPECT_EQ(0u, zero_ref.offset());

  DwarfDieRef main_ref = DwarfDieRef::Main(0x1234);
  EXPECT_TRUE(main_ref.is_valid());
  EXPECT_TRUE(static_cast<bool>(main_ref));
  EXPECT_EQ(0x1234u, main_ref.offset());
}

TEST(DwarfDieRef, Comparison) {
  DwarfDieRef empty;
  DwarfDieRef zero = DwarfDieRef::Main(0);
  DwarfDieRef main1 = DwarfDieRef::Main(0x10);
  DwarfDieRef main2 = DwarfDieRef::Main(0x20);

  EXPECT_LT(empty, zero);
  EXPECT_LT(zero, main1);
  EXPECT_LT(main1, main2);
  EXPECT_EQ(main1, DwarfDieRef::Main(0x10));
  EXPECT_NE(main1, empty);
  EXPECT_NE(main1, main2);
}

TEST(DwarfDieRef, Math) {
  DwarfDieRef main_ref = DwarfDieRef::Main(0x10);
  DwarfDieRef added_main = main_ref + 0x10;
  EXPECT_TRUE(added_main.is_valid());
  EXPECT_EQ(0x20u, added_main.offset());

  DwarfDieRef empty;
  DwarfDieRef added_empty = empty + 0x10;
  EXPECT_FALSE(added_empty.is_valid());
}

TEST(DwarfDieRef, Output) {
  std::stringstream ss_empty;
  ss_empty << DwarfDieRef();
  EXPECT_EQ("<invalid>", ss_empty.str());

  std::stringstream ss_main;
  ss_main << DwarfDieRef::Main(0x1234);
  EXPECT_EQ("offset 0x1234", ss_main.str());
}

}  // namespace zxdb
