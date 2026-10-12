// Copyright 2022 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/boot-shim/boot-shim.h>
#include <lib/boot-shim/bootloader-file.h>
#include <lib/boot-shim/item-base.h>
#include <lib/zbitl/image.h>

#include <array>
#include <span>
#include <string>
#include <tuple>
#include <type_traits>
#include <utility>
#include <vector>

#include <zxtest/zxtest.h>

namespace {

struct Base {};

template <typename T>
struct IsIntegralOrDerivedFromBase {
  static constexpr bool value = std::is_integral_v<T> || std::is_base_of_v<Base, T>;
};

using TestShim = boot_shim::BootShim<int, bool, std::string>;
using ExpectedType = std::tuple<int&, bool&>;

constexpr auto to_tuple = [](auto... items) { return std::forward_as_tuple(items...); };

static_assert(std::is_same_v<ExpectedType,
                             decltype(std::declval<TestShim>()
                                          .OnSelectItems<IsIntegralOrDerivedFromBase>(to_tuple))>);

TEST(BootShimTests, OnInvocableItems) {
  TestShim shim("BootShimTests.OnInvocableItems");
  {
    bool saw_bool = false, saw_string = false;
    shim.OnInvocableItems(
        // Not invocable on something not in the items list at all.
        [](Base&) { ADD_FAILURE(); },
        // Invocable on bool item.
        [&saw_bool](bool&) { saw_bool = true; },
        // Invocable on std::string item.
        [&saw_string](std::string&) { saw_string = true; });
    EXPECT_TRUE(saw_bool);
    EXPECT_TRUE(saw_string);
  }
  {
    bool saw_bool = false, saw_string = false;
    std::as_const(shim).OnInvocableItems(
        // Not invocable on something not in the items list at all.
        [](const Base&) { ADD_FAILURE(); },
        // Not invocable on const item.
        [](bool&) { ADD_FAILURE(); },
        // Invocable on bool item.
        [&saw_bool](const bool&) { saw_bool = true; },
        // Invocable on std::string item.
        [&saw_string](const std::string&) { saw_string = true; });
    EXPECT_TRUE(saw_bool);
    EXPECT_TRUE(saw_string);
  }
}

TEST(BootShimTests, BootloaderFileItem) {
  constexpr std::string_view kFile1Content = "hello world";
  constexpr std::string_view kFile2Content = "second file payload";

  // Include an unset slot to verify it contributes 0 bytes and is not appended.
  boot_shim::BootloaderFileItems<3> items;
  items.emplace_back(zbitl::BootloaderFile{
      .name = "first.txt",
      .contents = std::as_bytes(std::span{kFile1Content}),
  });
  items.emplace_back(zbitl::BootloaderFile{
      .name = "fuchsia.zbi",
      .contents = std::as_bytes(std::span{kFile2Content}),
  });

  alignas(ZBI_ALIGNMENT) std::array<std::byte, 512> buffer{};
  zbitl::Image<std::span<std::byte>> image(buffer);
  ASSERT_TRUE(image.clear().is_ok());
  ASSERT_TRUE(
      image.Append(zbi_header_t{.type = ZBI_TYPE_CMDLINE}, std::as_bytes(std::span{"foo=bar"}))
          .is_ok());
  ASSERT_TRUE(items.AppendItems(image).is_ok());

  std::vector<std::pair<std::string_view, std::string_view>> files;
  for (auto [name, contents] : zbitl::BootloaderFiles(image)) {
    files.emplace_back(name, std::string_view{reinterpret_cast<const char*>(contents.data()),
                                              contents.size_bytes()});
  }
  image.ignore_error();

  ASSERT_EQ(files.size(), 2u);
  EXPECT_EQ(files[0].first, "first.txt");
  EXPECT_EQ(files[0].second, kFile1Content);
  EXPECT_EQ(files[1].first, "fuchsia.zbi");
  EXPECT_EQ(files[1].second, kFile2Content);
}

}  // namespace
