// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <zircon/compiler.h>

#include <ktl/algorithm.h>
#include <ktl/span.h>
#include <ktl/string_view.h>
#include <phys/linuxboot-embed.h>
#include <pretty/hexdump.h>

#include "test-main.h"

namespace {

constexpr ktl::string_view kExpectedImage =
    "Hello, world!\n\0\0"  // two padding bytes to be a multiple of 4
    "Lorem ipsum\n"sv;

constexpr size_t kMaxBytes = 64;
static_assert(kMaxBytes > kExpectedImage.size());

}  // namespace

extern "C" __LOCAL uint64_t __executable_start[];  // NOLINT(bugprone-reserved-identifier)

int TestMain(void* bootloader_data, ktl::optional<EarlyBootZbi> zbi, arch::EarlyTicks) {
  Symbolize symbolize("linuxboot-embed-hello-world-test");

  const size_t link_size = GetLinuxbootLinkMemorySize();
  const size_t header_size = GetLinuxbootHeaderMemorySize();
  printf("%s: link size %#zx vs header size %#zx\n", symbolize.name(), link_size, header_size);
  hexdump(__executable_start, kMaxBytes);

  ktl::span image = GetLinuxbootEmbeddedImage();
  printf("%s: embedded image of %#zx bytes at %p\n", symbolize.name(), image.size_bytes(),
         image.data());

  // Cap it at a reasonable size before dumping.
  image = image.subspan(0, ktl::min(image.size_bytes(), kMaxBytes));
  hexdump(image.data(), image.size_bytes());

  ktl::string_view image_str{
      reinterpret_cast<const char*>(image.data()),
      image.size_bytes(),
  };
  printf("%s: image string of %#zx bytes at %p\n", symbolize.name(),  //
         image_str.size(), image_str.data());

  printf("%s: reference string of %#zx bytes at %p\n", symbolize.name(),  //
         kExpectedImage.size(), kExpectedImage.data());
  hexdump8(kExpectedImage.data(), kExpectedImage.size());

  return image_str == kExpectedImage ? 0 : 1;
}
