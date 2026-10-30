// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/zxdump/task.h>
#include <lib/zxdump/types.h>

#include <cstddef>
#include <span>
#include <vector>

#include <gtest/gtest.h>

#include "core.h"
#include "dump-file.h"
#include "test-file.h"
#include "test-tool-process.h"

namespace {

using namespace std::literals;

TEST(ZxdumpTests, DumpFileBadOpen) {
  auto result = zxdump::internal::DumpFile::Open({});
  EXPECT_TRUE(result.is_error());
}

TEST(ZxdumpTests, DumpFileMmap) {
  const std::string test_data = "test data";

  zxdump::testing::TestFile test_file;
  fputs(test_data.c_str(), test_file.stdio());

  auto file = zxdump::internal::DumpFile::Open(test_file.RewoundFd(), true);
  ASSERT_TRUE(file.is_ok()) << file.error_value();

  EXPECT_EQ(file->size(), test_data.size());
  EXPECT_EQ(file->size_bytes(), test_data.size());

  auto bytes = file->ReadEphemeral({0, test_data.size()});
  ASSERT_TRUE(bytes.is_ok()) << bytes.error_value();
  std::string_view str{
      reinterpret_cast<const char*>(bytes->data()),
      bytes->size(),
  };
  EXPECT_EQ(str, test_data);
}

TEST(ZxdumpTests, DumpFileStdio) {
  const std::string test_data = "test data";

  zxdump::testing::TestFile test_file;
  fputs(test_data.c_str(), test_file.stdio());

  auto file = zxdump::internal::DumpFile::Open(test_file.RewoundFd(), false);
  ASSERT_TRUE(file.is_ok()) << file.error_value();

  EXPECT_EQ(file->size(), test_data.size());
  EXPECT_EQ(file->size_bytes(), test_data.size());

  auto bytes = file->ReadEphemeral({0, test_data.size()});
  ASSERT_TRUE(bytes.is_ok()) << bytes.error_value();
  std::string_view str{
      reinterpret_cast<const char*>(bytes->data()),
      bytes->size(),
  };
  EXPECT_EQ(str, test_data);
}

TEST(ZxdumpTests, DumpFilePipe) {
  const std::string test_data = "test data";

  int pipefd[2];
  ASSERT_EQ(pipe(pipefd), 0) << strerror(errno);
  fbl::unique_fd in(pipefd[0]);

  {
    FILE* outf = fdopen(pipefd[1], "w");
    ASSERT_TRUE(outf) << strerror(errno);
    fputs(test_data.c_str(), outf);
    fclose(outf);
  }

  auto file = zxdump::internal::DumpFile::Open(std::move(in));
  ASSERT_TRUE(file.is_ok()) << file.error_value();

  EXPECT_EQ(file->size(), std::numeric_limits<size_t>::max());
  EXPECT_EQ(file->size_bytes(), std::numeric_limits<size_t>::max());

  // Even though it's not seekable, we can re-read the last-read chunk.
  for (int i = 0; i < 100; ++i) {
    auto bytes = file->ReadEphemeral({0, test_data.size()});
    ASSERT_TRUE(bytes.is_ok()) << bytes.error_value();
    std::string_view str{
        reinterpret_cast<const char*>(bytes->data()),
        bytes->size(),
    };
    EXPECT_EQ(str, test_data);
  }
}

TEST(ZxdumpTests, DumpFileZstd) {
  constexpr std::string_view kTestData = "test data";

  // Write a compressed file with the zstd tool.
  zxdump::testing::TestToolProcess zstd;
  ASSERT_NO_FATAL_FAILURE(zstd.Init());
  zxdump::testing::TestToolProcess::File& zstd_file =
      zstd.MakeFile("dump-file-zstd", zxdump::testing::TestToolProcess::File::kZstdSuffix);
  std::vector<std::string> args({"-q"s, "-o"s, zstd_file.name()});
  ASSERT_NO_FATAL_FAILURE(zstd.Start("zstd"s, args));
  ASSERT_NO_FATAL_FAILURE(zstd.SendStdin(std::string(kTestData)));
  int exit_status;
  ASSERT_NO_FATAL_FAILURE(zstd.Finish(exit_status));
  EXPECT_EQ(exit_status, EXIT_SUCCESS);

  auto file = zxdump::internal::DumpFile::Open(zstd_file.OpenOutput());
  ASSERT_TRUE(file.is_ok()) << zstd.FilePathForRunner(zstd_file) << ": " << file.error_value();

  auto header = file->ReadEphemeral({0, zxdump::internal::kHeaderProbeSize});
  ASSERT_TRUE(header.is_ok()) << header.error_value();

  ASSERT_FALSE(header->empty());
  ASSERT_TRUE(zxdump::internal::DumpFile::IsCompressed(*header));

  auto decompressed = file->Decompress({0, file->size()}, *header);
  ASSERT_TRUE(decompressed.is_ok()) << decompressed.error_value();

  // The reported size is not really meaningful, since it's streaming input.
  // But it's guaranteed to be nonzero.
  EXPECT_GT(decompressed->size(), 0u);
  EXPECT_GT(decompressed->size_bytes(), 0u);

  auto bytes = decompressed->ReadEphemeral({0, kTestData.size()});
  ASSERT_TRUE(bytes.is_ok()) << bytes.error_value();
  std::string_view str{
      reinterpret_cast<const char*>(bytes->data()),
      bytes->size(),
  };
  EXPECT_EQ(str, kTestData);
}

#if !defined(__Fuchsia__) || defined(__aarch64__)
void AppendNote(std::vector<std::byte>& notes, std::string_view name, uint32_t type,
                zxdump::ByteView desc) {
  zxdump::Elf::Nhdr nhdr = {
      .namesz = static_cast<uint32_t>(name.size() + 1),
      .descsz = static_cast<uint32_t>(desc.size()),
      .type = type,
  };
  const auto* nhdr_bytes = reinterpret_cast<const std::byte*>(&nhdr);
  notes.insert(notes.end(), nhdr_bytes, nhdr_bytes + sizeof(nhdr));
  const auto* name_bytes = reinterpret_cast<const std::byte*>(name.data());
  notes.insert(notes.end(), name_bytes, name_bytes + name.size());
  notes.push_back(std::byte{0});
  notes.resize(zxdump::NoteAlign(notes.size()), std::byte{0});
  notes.insert(notes.end(), desc.begin(), desc.end());
  notes.resize(zxdump::NoteAlign(notes.size()), std::byte{0});
}

void WriteSyntheticAarch64Core(FILE* out, const zx_arm64_thread_state_sve_regs_t* sve_regs) {
  constexpr zx_koid_t kProcessKoid = 1000;
  constexpr zx_koid_t kThreadKoid = 1001;

  std::vector<std::byte> notes;
  const zx_info_handle_basic_t process_basic = {
      .koid = kProcessKoid,
      .type = ZX_OBJ_TYPE_PROCESS,
  };
  AppendNote(notes, zxdump::kProcessInfoNoteName, ZX_INFO_HANDLE_BASIC,
             std::as_bytes(std::span{&process_basic, 1}));
  AppendNote(notes, zxdump::kProcessInfoNoteName, ZX_INFO_PROCESS_THREADS,
             std::as_bytes(std::span{&kThreadKoid, 1}));

  const zx_info_handle_basic_t thread_basic = {
      .koid = kThreadKoid,
      .type = ZX_OBJ_TYPE_THREAD,
  };
  AppendNote(notes, zxdump::kThreadInfoNoteName, ZX_INFO_HANDLE_BASIC,
             std::as_bytes(std::span{&thread_basic, 1}));
  if (sve_regs != nullptr) {
    AppendNote(notes, zxdump::kThreadStateNoteName, ZX_THREAD_STATE_ARM64_SVE,
               std::as_bytes(std::span{sve_regs, 1}));
  }

  const zxdump::Elf::Ehdr ehdr = {
      .magic = zxdump::Elf::Ehdr::kMagic,
      .elfclass = elfldltl::ElfClass::k64,
      .elfdata = elfldltl::ElfData::k2Lsb,
      .ident_version = elfldltl::ElfVersion::kCurrent,
      .type = elfldltl::ElfType::kCore,
      .machine = elfldltl::ElfMachine::kAarch64,
      .version = elfldltl::ElfVersion::kCurrent,
      .phoff = sizeof(ehdr),
      .ehsize = sizeof(ehdr),
      .phentsize = sizeof(zxdump::Elf::Phdr),
      .phnum = 1,
  };

  const zxdump::Elf::Phdr phdr = {
      .type = elfldltl::ElfPhdrType::kNote,
      .offset = sizeof(ehdr) + sizeof(phdr),
      .filesz = notes.size(),
      .align = zxdump::NoteAlign(),
  };

  ASSERT_EQ(fwrite(&ehdr, sizeof(ehdr), 1, out), 1u);
  ASSERT_EQ(fwrite(&phdr, sizeof(phdr), 1, out), 1u);
  ASSERT_EQ(fwrite(notes.data(), 1, notes.size(), out), notes.size());
  ASSERT_EQ(fflush(out), 0);
}

TEST(ZxdumpTests, ReadArm64SveRegsNote) {
  zx_arm64_thread_state_sve_regs_t expected = {};
  expected.ffr = 0x00ff;
  for (size_t i = 0; i < std::size(expected.p); ++i) {
    expected.p[i] = static_cast<uint16_t>(0x1100u + i);
  }

  {
    zxdump::testing::TestFile test_file;
    ASSERT_NO_FATAL_FAILURE(WriteSyntheticAarch64Core(test_file.stdio(), &expected));

    zxdump::TaskHolder holder;
    auto read_result = holder.Insert(test_file.RewoundFd(), false);
    ASSERT_TRUE(read_result.is_ok()) << read_result.error_value();

    auto proc_result = holder.root_job().find(1000);
    ASSERT_TRUE(proc_result.is_ok()) << proc_result.error_value();
    ASSERT_EQ(proc_result->get().type(), ZX_OBJ_TYPE_PROCESS);
    auto& process = static_cast<zxdump::Process&>(proc_result->get());

    auto thread_result = process.get_child(1001);
    ASSERT_TRUE(thread_result.is_ok()) << thread_result.error_value();
    zxdump::Thread& thread = *thread_result;

    auto state_result = thread.read_state<zx_arm64_thread_state_sve_regs_t>();
    ASSERT_TRUE(state_result.is_ok()) << state_result.error_value();
    EXPECT_EQ(state_result->ffr, expected.ffr);
    for (size_t i = 0; i < std::size(expected.p); ++i) {
      EXPECT_EQ(state_result->p[i], expected.p[i]) << "p[" << i << "]";
    }
  }

  {
    zxdump::testing::TestFile test_file;
    ASSERT_NO_FATAL_FAILURE(WriteSyntheticAarch64Core(test_file.stdio(), nullptr));

    zxdump::TaskHolder holder;
    auto read_result = holder.Insert(test_file.RewoundFd(), false);
    ASSERT_TRUE(read_result.is_ok()) << read_result.error_value();

    auto proc_result = holder.root_job().find(1000);
    ASSERT_TRUE(proc_result.is_ok()) << proc_result.error_value();
    ASSERT_EQ(proc_result->get().type(), ZX_OBJ_TYPE_PROCESS);
    auto& process = static_cast<zxdump::Process&>(proc_result->get());

    auto thread_result = process.get_child(1001);
    ASSERT_TRUE(thread_result.is_ok()) << thread_result.error_value();
    zxdump::Thread& thread = *thread_result;

    auto state_result = thread.read_state<zx_arm64_thread_state_sve_regs_t>();
    ASSERT_TRUE(state_result.is_error());
    EXPECT_EQ(state_result.error_value().status_, ZX_ERR_NOT_SUPPORTED);
  }
}
#endif

}  // namespace
