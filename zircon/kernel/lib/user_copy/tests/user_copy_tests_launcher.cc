// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/page/size.h>
#include <lib/unittest/unittest.h>
#include <lib/unittest/user_memory.h>
#include <lib/user_copy/user_iovec.h>
#include <lib/user_copy/user_ptr.h>
#include <string.h>

extern "C" {
zx_status_t rust_user_copy_user_out_ptr_write(user_out_ptr<uint32_t> ptr, uint32_t val);
zx_status_t rust_user_copy_user_in_ptr_read(user_in_ptr<const uint32_t> ptr, uint32_t* out_val);
zx_status_t rust_user_copy_user_in_iovec_get_total_capacity(user_in_ptr<const zx_iovec_t> ptr,
                                                            size_t count, size_t* out_capacity);
zx_status_t rust_user_copy_user_in_iovec_for_each(user_in_ptr<const zx_iovec_t> ptr, size_t count,
                                                  size_t* out_product);
zx_status_t rust_user_copy_user_string_view_copy_slice_from_user(user_in_ptr<const uint8_t> ptr,
                                                                 size_t length, uint8_t* dst,
                                                                 size_t dst_len);
zx_status_t rust_user_copy_user_in_ptr_copy_from_user(user_in_ptr<const uint32_t> ptr,
                                                      uint32_t* out_val);
zx_status_t rust_user_copy_user_in_ptr_copy_slice_from_user(user_in_ptr<const uint32_t> ptr,
                                                            uint32_t* dst, size_t dst_len);
zx_status_t rust_user_copy_test_offsets();
}

namespace {

using testing::UserMemory;
constexpr uint32_t kTestValue = 0xDEADBEEF;

bool rust_test_copy_out() {
  BEGIN_TEST;
  auto user = UserMemory::Create(kPageSize);
  ASSERT_EQ(user->CommitAndMap(kPageSize), ZX_OK, "");

  ASSERT_EQ(rust_user_copy_user_out_ptr_write(user->user_out<uint32_t>(), kTestValue), ZX_OK, "");

  uint32_t temp;
  ASSERT_EQ(user->VmoRead(&temp, 0, sizeof(temp)), ZX_OK, "");
  EXPECT_EQ(temp, kTestValue, "");
  END_TEST;
}

bool rust_test_copy_in() {
  BEGIN_TEST;
  auto user = UserMemory::Create(kPageSize);
  ASSERT_EQ(user->CommitAndMap(kPageSize), ZX_OK, "");
  ASSERT_EQ(user->VmoWrite(&kTestValue, 0, sizeof(kTestValue)), ZX_OK, "");

  uint32_t temp = 0;
  ASSERT_EQ(rust_user_copy_user_in_ptr_read(user->user_in<const uint32_t>(), &temp), ZX_OK, "");
  EXPECT_EQ(temp, kTestValue, "");
  END_TEST;
}

bool rust_test_faults() {
  BEGIN_TEST;
  // Null pointer should fail with ZX_ERR_INVALID_ARGS.
  EXPECT_EQ(rust_user_copy_user_out_ptr_write(make_user_out_ptr<uint32_t>(nullptr), kTestValue),
            ZX_ERR_INVALID_ARGS, "");

  uint32_t temp = 0;
  EXPECT_EQ(rust_user_copy_user_in_ptr_read(make_user_in_ptr<const uint32_t>(nullptr), &temp),
            ZX_ERR_INVALID_ARGS, "");
  EXPECT_EQ(
      rust_user_copy_user_in_ptr_copy_from_user(make_user_in_ptr<const uint32_t>(nullptr), &temp),
      ZX_ERR_INVALID_ARGS, "");
  EXPECT_EQ(rust_user_copy_user_in_ptr_copy_slice_from_user(
                make_user_in_ptr<const uint32_t>(nullptr), &temp, 1),
            ZX_ERR_INVALID_ARGS, "");

  // Address outside user range should fail with ZX_ERR_INVALID_ARGS.
  auto bad_out = make_user_out_ptr(reinterpret_cast<uint32_t*>(USER_ASPACE_BASE - 64));
  EXPECT_EQ(rust_user_copy_user_out_ptr_write(bad_out, kTestValue), ZX_ERR_INVALID_ARGS, "");

  auto bad_in = make_user_in_ptr(reinterpret_cast<const uint32_t*>(USER_ASPACE_BASE - 64));
  EXPECT_EQ(rust_user_copy_user_in_ptr_read(bad_in, &temp), ZX_ERR_INVALID_ARGS, "");
  EXPECT_EQ(rust_user_copy_user_in_ptr_copy_from_user(bad_in, &temp), ZX_ERR_INVALID_ARGS, "");
  EXPECT_EQ(rust_user_copy_user_in_ptr_copy_slice_from_user(bad_in, &temp, 1), ZX_ERR_INVALID_ARGS,
            "");
  END_TEST;
}

bool rust_test_iovec_capacity() {
  BEGIN_TEST;
  auto user = UserMemory::Create(kPageSize);
  zx_iovec_t vec[2] = {};
  vec[0].capacity = 348u;
  vec[1].capacity = 58u;
  ASSERT_EQ(user->VmoWrite(vec, 0, sizeof(vec)), ZX_OK, "");

  size_t total_capacity = 0;
  ASSERT_EQ(rust_user_copy_user_in_iovec_get_total_capacity(user->user_in<const zx_iovec_t>(), 2,
                                                            &total_capacity),
            ZX_OK, "");
  EXPECT_EQ(total_capacity, 406u, "");
  END_TEST;
}

bool rust_test_iovec_foreach() {
  BEGIN_TEST;
  auto user = UserMemory::Create(kPageSize);
  zx_iovec_t vec[3] = {};
  vec[0].capacity = 7u;
  vec[1].capacity = 11u;
  vec[2].capacity = 13u;
  ASSERT_EQ(user->VmoWrite(vec, 0, sizeof(vec)), ZX_OK, "");

  size_t product = 0;
  ASSERT_EQ(rust_user_copy_user_in_iovec_for_each(user->user_in<const zx_iovec_t>(), 3, &product),
            ZX_OK, "");
  EXPECT_EQ(product, 2002u, "");
  END_TEST;
}

bool rust_test_string_view() {
  BEGIN_TEST;
  auto user = UserMemory::Create(kPageSize);
  ASSERT_EQ(user->CommitAndMap(kPageSize), ZX_OK, "");
  const char kString[] = "Hello, Fuchsia!";
  ASSERT_EQ(user->VmoWrite(kString, 0, sizeof(kString)), ZX_OK, "");

  uint8_t buf[32] = {};
  ASSERT_EQ(rust_user_copy_user_string_view_copy_slice_from_user(user->user_in<const uint8_t>(),
                                                                 sizeof(kString), buf, sizeof(buf)),
            ZX_OK, "");
  EXPECT_EQ(strcmp(reinterpret_cast<char*>(buf), kString), 0, "");

  // Buffer too small should return ZX_ERR_INVALID_ARGS.
  EXPECT_EQ(rust_user_copy_user_string_view_copy_slice_from_user(user->user_in<const uint8_t>(),
                                                                 sizeof(kString), buf, 5),
            ZX_ERR_INVALID_ARGS, "");
  END_TEST;
}

bool rust_test_copy_from_user() {
  BEGIN_TEST;
  auto user = UserMemory::Create(kPageSize);
  ASSERT_EQ(user->CommitAndMap(kPageSize), ZX_OK, "");
  ASSERT_EQ(user->VmoWrite(&kTestValue, 0, sizeof(kTestValue)), ZX_OK, "");

  uint32_t temp = 0;
  ASSERT_EQ(rust_user_copy_user_in_ptr_copy_from_user(user->user_in<const uint32_t>(), &temp),
            ZX_OK, "");
  EXPECT_EQ(temp, kTestValue, "");
  END_TEST;
}

bool rust_test_copy_slice_from_user() {
  BEGIN_TEST;
  auto user = UserMemory::Create(kPageSize);
  ASSERT_EQ(user->CommitAndMap(kPageSize), ZX_OK, "");
  uint32_t vals[3] = {10, 20, 30};
  ASSERT_EQ(user->VmoWrite(vals, 0, sizeof(vals)), ZX_OK, "");

  uint32_t out[3] = {};
  ASSERT_EQ(
      rust_user_copy_user_in_ptr_copy_slice_from_user(user->user_in<const uint32_t>(), out, 3),
      ZX_OK, "");
  EXPECT_EQ(out[0], 10u, "");
  EXPECT_EQ(out[1], 20u, "");
  EXPECT_EQ(out[2], 30u, "");
  END_TEST;
}

bool rust_test_offsets() {
  BEGIN_TEST;
  EXPECT_EQ(rust_user_copy_test_offsets(), ZX_OK, "");
  END_TEST;
}

}  // namespace

UNITTEST_START_TESTCASE(rust_user_copy_tests)
UNITTEST("CopyOut", rust_test_copy_out)
UNITTEST("CopyIn", rust_test_copy_in)
UNITTEST("CopyFromUser", rust_test_copy_from_user)
UNITTEST("CopySliceFromUser", rust_test_copy_slice_from_user)
UNITTEST("Faults", rust_test_faults)
UNITTEST("IovecCapacity", rust_test_iovec_capacity)
UNITTEST("IovecForeach", rust_test_iovec_foreach)
UNITTEST("StringView", rust_test_string_view)
UNITTEST("Offsets", rust_test_offsets)
UNITTEST_END_TESTCASE(rust_user_copy_tests, "rust_user_copy", "Rust user_copy tests")
