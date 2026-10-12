// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "lib/linux-initramfs/initramfs.h"

#include <algorithm>
#include <cassert>
#include <cctype>

namespace linux_initramfs {
namespace {

using namespace std::literals;

// cf https://docs.kernel.org/driver-api/early-userspace/buffer-format.html
//
// There is a 6-byte magic string.  Then there are 8-byte fields of ASCII hex.

constexpr std::string_view kMagic = "070701"sv;
constexpr std::string_view kMagicChksum = "070702"sv;
static_assert(kMagic.size() == kMagicChksum.size());

constexpr std::string_view kTrailer = "TRAILER!!!"sv;

enum FieldIndex : size_t {
  kIno,
  kMode,
  kUid,
  kGid,
  kNlink,
  kMtime,
  kFilesize,
  kMaj,
  kMin,
  kRmaj,
  kRmin,
  kNamesize,
  kChksum,
  kFieldCount,
};

constexpr size_t kHeaderSize = kMagic.size() + (kFieldCount * 8);
static_assert(kHeaderSize == Member::kHeaderSize);

bool ValidHeader(std::string_view contents) {
  return contents.size() >= kHeaderSize &&
         (contents.starts_with(kMagic) || contents.starts_with(kMagicChksum)) &&
         std::ranges::all_of(contents
                                 .substr(0, kHeaderSize)  //
                                 .substr(kMagic.size()),
                             isxdigit);
}

// This goes at the end of archives, but is ignored so they can be concatenated.
constexpr bool IsTrailer(const Member& member) {
  return member.contents.empty() && member.name == kTrailer;
}

// Extracts the n-th 8-byte hex field from the header.
// The input data must be valid ASCII hex digits (e.g. ValidHeader() returns
// true on this data).
constexpr uint32_t GetField(std::string_view contents, FieldIndex n) {
  std::string_view field = contents.substr(kMagic.size() + (n * 8), 8);
  assert(field.size() == 8);
  // Avoid std::from_chars(..., 16) here because libc++'s non-base-10
  // implementation uses float arithmetic, which emits soft-float runtime library
  // calls in kernel.phys (-mgeneral-regs-only) builds when not constant-folded.
  uint32_t value = 0;
  for (char c : field) {
    value <<= 4;
    if (c >= '0' && c <= '9') {
      value |= static_cast<uint32_t>(c - '0');
    } else if (c >= 'a' && c <= 'f') {
      value |= static_cast<uint32_t>(c - 'a' + 10);
    } else if (c >= 'A' && c <= 'F') {
      value |= static_cast<uint32_t>(c - 'A' + 10);
    } else {
      assert(false);
    }
  }
  return value;
}

constexpr Member GetMember(std::string_view contents) {
  if (!ValidHeader(contents)) {
    return {};
  }

  const uint32_t filesize = GetField(contents, kFilesize);
  const uint32_t namesize = GetField(contents, kNamesize);
  contents.remove_prefix(kHeaderSize);

  if (contents.size() < namesize || namesize == 0) [[unlikely]] {
    return {};
  }
  // The namesize includes the NUL terminator.
  std::string_view name = contents.substr(0, namesize - 1);
  contents.remove_prefix(namesize);

  // Skip alignment padding after the name.
  const size_t consumed = kHeaderSize + namesize;
  const size_t padding = ((consumed + 3) & -size_t{4}) - consumed;
  if (contents.size() < padding) [[unlikely]] {
    return {};
  }
  contents.remove_prefix(padding);

  if (contents.size() < filesize) [[unlikely]] {
    return {};
  }
  std::string_view file = contents.substr(0, filesize);

  return {.name = name, .contents = std::as_bytes(std::span{file})};
}

}  // namespace

Initramfs::iterator Initramfs::begin() const {
  iterator it;
  it.value_ = GetMember(contents_);

  if (it.value_) {
    it.contents_ = contents_;
    assert(it != end());
    if (IsTrailer(*it)) {
      ++it;
    }
  } else {
    assert(it == end());
  }
  return it;
}

Initramfs::iterator& Initramfs::iterator::operator++() {  // prefix
  do {
    assert(!contents_.empty());
    contents_.remove_prefix(value_.size_bytes());
    // Padding bytes of zero are allowed between members in multiples of 4.
    if (size_t pos = contents_.find_first_not_of('\0');
        pos != std::string_view::npos && pos % 4 == 0) {
      contents_.remove_prefix(pos);
    } else {
      value_ = {};
      break;
    }
    value_ = GetMember(contents_);
    // A special member optionally terminates a single cpio archive.  But
    // archives can be concatenated, so just skip it and keep looking.  It's
    // only relevant to the hard-link tracking, which is not supported here.
  } while (IsTrailer(value_));
  if (!value_) {  // end() state.
    contents_ = {};
  }
  return *this;
}

}  // namespace linux_initramfs
