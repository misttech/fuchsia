// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
#include <elf.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <string>
#include <vector>

// Must not receive SHOULD_NOT_BE_DEFINED
#if defined(SHOULD_NOT_BE_DEFINED)
#error "Unexpected SHOULD_NOT_BE_DEFINED definition"
#endif

// Should have received CFLAGS
#if !defined(CFLAGS)
#error "Missing CFLAGS definition"
#elif CFLAGS != 1
#error "Invalid CFLAGS definition"
#endif

extern int fx_cc_library_for_test();

namespace {

std::string FindFilePath(const std::string& path) {
  FILE* f = fopen(path.c_str(), "rb");
  if (f) {
    fclose(f);
    return path;
  }
  const char* runfiles_dir = getenv("RUNFILES_DIR");
  if (runfiles_dir) {
    std::string rel_path = path;
    size_t bin_pos = rel_path.find("/bin/");
    if (bin_pos != std::string::npos) {
      rel_path = rel_path.substr(bin_pos + 5);
    }
    for (const char* prefix : {"/", "/_main/"}) {
      std::string candidate = std::string(runfiles_dir) + prefix + rel_path;
      f = fopen(candidate.c_str(), "rb");
      if (f) {
        fclose(f);
        return candidate;
      }
    }
  }
  return path;
}

// Reads the ELF64 dynamic section of `path` and returns its DT_SONAME string,
// or an empty string if not found or on error.
std::string GetElfSoname(const char* path) {
  std::string actual_path = FindFilePath(path);
  FILE* f = fopen(actual_path.c_str(), "rb");
  if (!f) {
    fprintf(stderr, "Failed to open %s (resolved from %s)\n", actual_path.c_str(), path);
    return "";
  }
  fseek(f, 0, SEEK_END);
  long size = ftell(f);
  fseek(f, 0, SEEK_SET);
  if (size < static_cast<long>(sizeof(Elf64_Ehdr))) {
    fclose(f);
    return "";
  }
  std::vector<uint8_t> buf(static_cast<size_t>(size));
  if (fread(buf.data(), 1, buf.size(), f) != buf.size()) {
    fclose(f);
    return "";
  }
  fclose(f);

  const auto* ehdr = reinterpret_cast<const Elf64_Ehdr*>(buf.data());
  if (memcmp(ehdr->e_ident, ELFMAG, SELFMAG) != 0) {
    fprintf(stderr, "Not an ELF file: %s\n", path);
    return "";
  }

  const Elf64_Shdr* dynsym_shdr = nullptr;
  for (uint16_t i = 0; i < ehdr->e_shnum; ++i) {
    const auto* shdr =
        reinterpret_cast<const Elf64_Shdr*>(buf.data() + ehdr->e_shoff + i * ehdr->e_shentsize);
    if (shdr->sh_type == SHT_DYNAMIC) {
      dynsym_shdr = shdr;
      break;
    }
  }
  if (!dynsym_shdr || dynsym_shdr->sh_link >= ehdr->e_shnum) {
    fprintf(stderr, "No SHT_DYNAMIC section in %s\n", path);
    return "";
  }

  const auto* dynstr_shdr = reinterpret_cast<const Elf64_Shdr*>(
      buf.data() + ehdr->e_shoff + dynsym_shdr->sh_link * ehdr->e_shentsize);
  const char* dynstr = reinterpret_cast<const char*>(buf.data() + dynstr_shdr->sh_offset);

  size_t count = dynsym_shdr->sh_size / sizeof(Elf64_Dyn);
  const auto* dyns = reinterpret_cast<const Elf64_Dyn*>(buf.data() + dynsym_shdr->sh_offset);
  for (size_t i = 0; i < count; ++i) {
    if (dyns[i].d_tag == DT_SONAME) {
      return std::string(dynstr + dyns[i].d_un.d_val);
    }
  }
  return "";
}

}  // namespace

int main(int argc, char** argv) {
  int ret = fx_cc_library_for_test();
  int expected = 42;
  if (ret != expected) {
    fprintf(stderr, "Unexpected value from fx_cc_library_for_test(): %d, expected %d\n", ret,
            expected);
    return 1;
  }

  for (int i = 1; i < argc; ++i) {
    std::string arg(argv[i]);
    size_t eq = arg.find('=');
    if (eq == std::string::npos) {
      fprintf(stderr, "Expected <path>=<expected_soname> argument, got: %s\n", argv[i]);
      return 1;
    }
    std::string path = arg.substr(0, eq);
    std::string expected_soname = arg.substr(eq + 1);
    std::string actual_soname = GetElfSoname(path.c_str());
    if (actual_soname != expected_soname) {
      fprintf(stderr, "DT_SONAME mismatch for %s: got '%s', expected '%s'\n", path.c_str(),
              actual_soname.c_str(), expected_soname.c_str());
      return 1;
    }
  }
  return 0;
}
