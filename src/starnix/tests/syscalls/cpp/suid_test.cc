// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fcntl.h>
#include <grp.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

#include <filesystem>
#include <optional>

#include <linux/capability.h>

#include "src/lib/files/directory.h"
#include "src/lib/files/file.h"
#include "src/lib/files/path.h"
#include "src/starnix/tests/syscalls/cpp/capabilities_helper.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

namespace {

constexpr int kOutputFd = 100;

constexpr uid_t kUser1Uid = 65533;
constexpr gid_t kUser1Gid = 65534;
constexpr uid_t kRootUid = 0;
constexpr gid_t kRootGid = 0;

std::string GetCredsBinaryPath() {
  return test_helper::GetTestResourcePath("print_uid_gid_exec_child");
}

bool change_ids(uid_t user, gid_t group) {
  return (setresgid(group, group, group) == 0) && (setresuid(user, user, user) == 0);
}

struct ExecCreds {
  uid_t ruid = 0;
  uid_t euid = 0;
  uid_t suid = 0;
  gid_t rgid = 0;
  gid_t egid = 0;
  gid_t sgid = 0;
};

void ReadExecCreds(int fd, ExecCreds *out) {
  ASSERT_THAT(lseek(fd, 0, SEEK_SET), SyscallSucceeds());
  std::string content;
  ASSERT_TRUE(files::ReadFileDescriptorToString(fd, &content));
  ASSERT_EQ(sscanf(content.c_str(), "ruid: %u euid: %u suid: %u\nrgid: %u egid: %u sgid: %u\n",
                   &out->ruid, &out->euid, &out->suid, &out->rgid, &out->egid, &out->sgid),
            6)
      << "Unexpected credentials output: " << content;
}

void ExecAsUser1AndReadCreds(const std::string &path, ExecCreds *creds) {
  fbl::unique_fd fd(SAFE_SYSCALL(test_helper::MemFdCreate("creds", O_RDWR)));
  ASSERT_TRUE(fd.is_valid());

  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([&]() {
    EXPECT_THAT(fcntl(kOutputFd, F_GETFD), SyscallFailsWithErrno(EBADF));
    ASSERT_THAT(dup2(fd.get(), kOutputFd), SyscallSucceeds());

    ASSERT_THAT(setgroups(0, nullptr), SyscallSucceeds());
    ASSERT_TRUE(change_ids(kUser1Uid, kUser1Gid));

    char *const argv[] = {const_cast<char *>(path.c_str()), nullptr};
    ASSERT_THAT(execve(path.c_str(), argv, nullptr), SyscallSucceeds());
  });
  ASSERT_TRUE(helper.WaitForChildren());

  ASSERT_NO_FATAL_FAILURE(ReadExecCreds(fd.get(), creds));
}

}  // namespace

class SuidTestBase : public ::testing::Test {
 protected:
  void SetUp() override {
    if (!test_helper::HasSysAdmin()) {
      GTEST_SKIP() << "Not running with sysadmin capabilities, skipping.";
    }
    mounted_ = ASSERT_RESULT_SUCCESS_AND_RETURN(
        test_helper::ScopedMount::Mount("", temp_dir_.path(), "tmpfs", mount_flags(), nullptr));

    constexpr int kDirPerms = S_IRWXU | S_IXGRP | S_IXOTH;
    ASSERT_THAT(chmod(temp_dir_.path().c_str(), kDirPerms), SyscallSucceeds());
  }

  virtual unsigned long mount_flags() const { return 0; }

  std::string StageCredsBinary(const std::string &name, mode_t mode, uid_t uid = kRootUid,
                               gid_t gid = kRootGid) {
    std::string path = files::JoinPath(temp_dir_.path(), name);
    std::error_code ec;
    EXPECT_TRUE(std::filesystem::copy_file(GetCredsBinaryPath(), path, ec)) << ec.message();
    EXPECT_THAT(chown(path.c_str(), uid, gid), SyscallSucceeds());
    EXPECT_THAT(chmod(path.c_str(), mode), SyscallSucceeds());
    return path;
  }

  std::string StageScript(const std::string &name, const std::string &interpreter_path, mode_t mode,
                          uid_t uid = kRootUid, gid_t gid = kRootGid) {
    std::string path = files::JoinPath(temp_dir_.path(), name);
    EXPECT_TRUE(files::WriteFile(path, "#!" + interpreter_path + "\n"));
    EXPECT_THAT(chown(path.c_str(), uid, gid), SyscallSucceeds());
    EXPECT_THAT(chmod(path.c_str(), mode), SyscallSucceeds());
    return path;
  }

  test_helper::ScopedTempDir temp_dir_;
  test_helper::ScopedMount mounted_;
};

class SuidTest : public SuidTestBase, public ::testing::WithParamInterface<unsigned long> {
 public:
  unsigned long mount_flags() const override { return GetParam(); }

  std::optional<std::string> MountTmpFs(const std::string &temp_dir) {
    std::string temp = temp_dir + "/tmp";
    EXPECT_THAT(mkdir(temp.c_str(), S_IRWXU), SyscallSucceeds());

    int res = mount(nullptr, temp.c_str(), "tmpfs", mount_flags(), "");
    EXPECT_EQ(res, 0) << "mount: " << std::strerror(errno);

    if (res != 0) {
      return std::nullopt;
    }

    return temp;
  }
};

TEST_P(SuidTest, SuidBinaryBecomesRoot) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Not running with sysadmin capabilities, skipping.";
  }
  std::string creds_binary = GetCredsBinaryPath();

  test_helper::ScopedTempDir temp_dir;
  auto mounted = MountTmpFs(temp_dir.path());
  ASSERT_TRUE(mounted.has_value()) << "failed to mount fs";
  auto mount_path = mounted.value();

  // Directory Permissions: owner can do everything, user and other can search.
  constexpr int kDirPerms = S_IRWXU | S_IXGRP | S_IXOTH;

  SAFE_SYSCALL(chmod(mount_path.c_str(), kDirPerms));
  SAFE_SYSCALL(chmod(temp_dir.path().c_str(), kDirPerms));

  std::string suid_binary_path = files::JoinPath(mount_path, "suid_binary_becomes_root");
  std::filesystem::copy_file(creds_binary, suid_binary_path);

  // File permissions: A set-user-ID binary that can be executed by Others.
  constexpr int kBinaryPerms = S_ISUID | S_IXOTH;
  SAFE_SYSCALL(chown(suid_binary_path.c_str(), kRootUid, kRootGid));
  SAFE_SYSCALL(chmod(suid_binary_path.c_str(), kBinaryPerms));

  // Binary will output a string to this memfd.
  int fd = SAFE_SYSCALL(test_helper::MemFdCreate("creds", O_RDWR));
  pid_t pid = SAFE_SYSCALL(fork());
  if (pid == 0) {
    ASSERT_TRUE(fcntl(kOutputFd, F_GETFD) == -1 && errno == EBADF);
    SAFE_SYSCALL(dup2(fd, kOutputFd));

    SAFE_SYSCALL(setgroups(0, nullptr));  // drop all supplementary groups.
    ASSERT_TRUE(change_ids(kUser1Uid, kUser1Gid));

    char *const argv[] = {const_cast<char *>(suid_binary_path.c_str()), nullptr};

    SAFE_SYSCALL(execve(suid_binary_path.c_str(), argv, nullptr));
    _exit(EXIT_FAILURE);
  }

  int status = 0;
  SAFE_SYSCALL(waitpid(pid, &status, 0));
  EXPECT_TRUE(WIFEXITED(status) && WEXITSTATUS(status) == 0);

  SAFE_SYSCALL(lseek(fd, 0, SEEK_SET));
  FILE *fp = fdopen(fd, "r");

  {
    uid_t ruid, euid, suid;
    EXPECT_EQ(fscanf(fp, "ruid: %u euid: %u suid: %u\n", &ruid, &euid, &suid), 3);
    EXPECT_EQ(ruid, kUser1Uid);
    if (mount_flags() & MS_NOSUID) {
      EXPECT_EQ(euid, kUser1Uid);
      EXPECT_EQ(suid, kUser1Uid);
    } else {
      EXPECT_EQ(euid, kRootUid);
      EXPECT_EQ(suid, kRootUid);
    }
  }

  {
    gid_t rgid, egid, sgid;
    EXPECT_EQ(fscanf(fp, "rgid: %u egid: %u sgid: %u\n", &rgid, &egid, &sgid), 3);
    EXPECT_EQ(rgid, kUser1Gid);
    EXPECT_EQ(egid, kUser1Gid);
    EXPECT_EQ(sgid, kUser1Gid);
  }

  fclose(fp);
}

TEST_P(SuidTest, CloneFsSuppressesSuidAndSgid) {
  std::string suid_binary_path =
      StageCredsBinary("suid_sgid_binary", S_ISUID | S_ISGID | S_IXGRP | S_IXOTH);

  fbl::unique_fd fd(SAFE_SYSCALL(test_helper::MemFdCreate("creds", O_RDWR)));
  ASSERT_TRUE(fd.is_valid());

  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([&]() {
    EXPECT_THAT(fcntl(kOutputFd, F_GETFD), SyscallFailsWithErrno(EBADF));
    ASSERT_THAT(dup2(fd.get(), kOutputFd), SyscallSucceeds());

    ASSERT_THAT(setgroups(0, nullptr), SyscallSucceeds());
    ASSERT_TRUE(change_ids(kUser1Uid, kUser1Gid));

    int pipe_fds[2];
    ASSERT_THAT(pipe(pipe_fds), SyscallSucceeds());
    fbl::unique_fd read_end(pipe_fds[0]);
    fbl::unique_fd write_end(pipe_fds[1]);
    ASSERT_THAT(fcntl(write_end.get(), F_SETFD, FD_CLOEXEC), SyscallSucceeds());

    test_helper::CloneHelper clone_helper;
    clone_helper.runInClonedChild(CLONE_FS | SIGCHLD, [&]() {
      write_end.reset();
      char c;
      EXPECT_THAT(HANDLE_EINTR(read(read_end.get(), &c, 1)), SyscallSucceedsWithValue(0));
    });

    read_end.reset();
    // Keep write_end open until execve(2) closes FD_CLOEXEC descriptors (after
    // bprm_creds_from_file) so the peer process shares the filesystem context
    // during credential calculation.
    std::ignore = write_end.release();

    char *const argv[] = {const_cast<char *>(suid_binary_path.c_str()), nullptr};
    ASSERT_THAT(execve(suid_binary_path.c_str(), argv, nullptr), SyscallSucceeds());
  });
  ASSERT_TRUE(helper.WaitForChildren());

  ExecCreds creds;
  ASSERT_NO_FATAL_FAILURE(ReadExecCreds(fd.get(), &creds));
  EXPECT_EQ(creds.ruid, kUser1Uid);
  EXPECT_EQ(creds.euid, kUser1Uid);
  EXPECT_EQ(creds.suid, kUser1Uid);
  EXPECT_EQ(creds.rgid, kUser1Gid);
  EXPECT_EQ(creds.egid, kUser1Gid);
  EXPECT_EQ(creds.sgid, kUser1Gid);
}

TEST_P(SuidTest, FileModificationsRemoveSuid) {
  if (!test_helper::HasSysAdmin() || !test_helper::HasCapability(CAP_FSETID)) {
    GTEST_SKIP() << "Not running with sysadmin capabilities, skipping.";
  }
  test_helper::ScopedTempDir temp_dir;
  auto mounted = MountTmpFs(temp_dir.path());
  ASSERT_TRUE(mounted.has_value()) << "failed to mount fs";
  auto mount_path = mounted.value();

  test_helper::ForkHelper helper;

  // We will drop capabilities, so let's do that inside a new process.
  helper.RunInForkedProcess([&] {
    std::string test_suid_file = files::JoinPath(mount_path, "file_modification_drops_suid");
    int fd = SAFE_SYSCALL(creat(test_suid_file.c_str(), S_ISUID | S_ISGID | S_IRWXU));

    struct stat file_stat;
    SAFE_SYSCALL(fstat(fd, &file_stat));
    EXPECT_NE((file_stat.st_mode & S_ISUID), 0U);

    uint8_t data = 'x';

    // Because we have CAP_FSETID, modifying the file doesn't remove the
    // set-user-ID bit.
    SAFE_SYSCALL(write(fd, &data, sizeof(data)));
    SAFE_SYSCALL(fstat(fd, &file_stat));
    EXPECT_NE((file_stat.st_mode & S_ISUID), 0U);

    // After dropping CAP_FSETID, modifications to the file should remove the
    // set-user-ID bit.
    test_helper::UnsetCapability(CAP_FSETID);
    SAFE_SYSCALL(write(fd, &data, sizeof(data)));
    SAFE_SYSCALL(fstat(fd, &file_stat));
    EXPECT_EQ((file_stat.st_mode & S_ISUID), 0U);

    // Setting the file as set-user-ID again works.
    SAFE_SYSCALL(fchmod(fd, S_ISUID | S_IRWXU));
    SAFE_SYSCALL(fstat(fd, &file_stat));
    EXPECT_NE((file_stat.st_mode & S_ISUID), 0U);

    close(fd);

    // But can be removed again by truncating the file.
    SAFE_SYSCALL(truncate(test_suid_file.c_str(), 0));
    SAFE_SYSCALL(stat(test_suid_file.c_str(), &file_stat));
    EXPECT_EQ((file_stat.st_mode & S_ISUID), 0U);

    SAFE_SYSCALL(unlink(test_suid_file.c_str()));
  });
}

TEST_P(SuidTest, OwnershipChangesRemoveSuid) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Not running with sysadmin capabilities, skipping.";
  }

  test_helper::ScopedTempDir temp_dir;
  auto mounted = MountTmpFs(temp_dir.path());
  ASSERT_TRUE(mounted.has_value()) << "failed to mount fs";
  auto mount_path = mounted.value();

  std::string test_suid_file = files::JoinPath(mount_path, "ownership_change_drops_suid");
  int fd = SAFE_SYSCALL(creat(test_suid_file.c_str(), S_ISUID | S_IRWXU));

  struct stat file_stat;
  SAFE_SYSCALL(fstat(fd, &file_stat));
  EXPECT_NE((file_stat.st_mode & S_ISUID), 0U);

  SAFE_SYSCALL(fchown(fd, kUser1Uid, kRootGid));
  SAFE_SYSCALL(fstat(fd, &file_stat));
  EXPECT_EQ((file_stat.st_mode & S_ISUID), 0U);
  close(fd);
  SAFE_SYSCALL(unlink(test_suid_file.c_str()));
}

// Per execve(2): "Linux ignores the set-user-ID and set-group-ID bits on scripts."
TEST_F(SuidTestBase, SuidAndSgidOnScriptAreIgnored) {
  std::string interpreter_path =
      StageCredsBinary("interpreter", S_IRWXU | S_IRGRP | S_IXGRP | S_IROTH | S_IXOTH);
  std::string script_path =
      StageScript("suid_sgid_script", interpreter_path,
                  S_ISUID | S_ISGID | S_IRWXU | S_IRGRP | S_IXGRP | S_IROTH | S_IXOTH);

  ExecCreds creds;
  ASSERT_NO_FATAL_FAILURE(ExecAsUser1AndReadCreds(script_path, &creds));
  EXPECT_EQ(creds.ruid, kUser1Uid);
  EXPECT_EQ(creds.euid, kUser1Uid);
  EXPECT_EQ(creds.suid, kUser1Uid);
  EXPECT_EQ(creds.rgid, kUser1Gid);
  EXPECT_EQ(creds.egid, kUser1Gid);
  EXPECT_EQ(creds.sgid, kUser1Gid);
}

// Executing a non-SUID/SGID `#!` script whose interpreter has S_ISUID and
// S_ISGID set elevates effective and saved-set IDs to the interpreter's owner
// and group (unless the interpreter's mount has MS_NOSUID).
TEST_P(SuidTest, SuidAndSgidOnScriptInterpreterAreHonored) {
  // Make the interpreter execute-only (no read bits for group/other) with SUID and SGID set.
  std::string interpreter_path =
      StageCredsBinary("suid_sgid_interpreter", S_ISUID | S_ISGID | S_IXGRP | S_IXOTH);
  std::string script_path =
      StageScript("script", interpreter_path, S_IRWXU | S_IRGRP | S_IXGRP | S_IROTH | S_IXOTH,
                  kUser1Uid, kUser1Gid);

  ExecCreds creds;
  ASSERT_NO_FATAL_FAILURE(ExecAsUser1AndReadCreds(script_path, &creds));
  EXPECT_EQ(creds.ruid, kUser1Uid);
  EXPECT_EQ(creds.rgid, kUser1Gid);
  if (mount_flags() & MS_NOSUID) {
    EXPECT_EQ(creds.euid, kUser1Uid);
    EXPECT_EQ(creds.suid, kUser1Uid);
    EXPECT_EQ(creds.egid, kUser1Gid);
    EXPECT_EQ(creds.sgid, kUser1Gid);
  } else {
    EXPECT_EQ(creds.euid, kRootUid);
    EXPECT_EQ(creds.suid, kRootUid);
    EXPECT_EQ(creds.egid, kRootGid);
    EXPECT_EQ(creds.sgid, kRootGid);
  }
}

TEST_F(SuidTestBase, SuidBinaryWithoutExecPermissionFails) {
  // SUID/SGID binary with no execute bits at all.
  std::string no_exec_binary =
      StageCredsBinary("no_exec_suid_binary", S_ISUID | S_ISGID | S_IRUSR | S_IRGRP | S_IROTH);

  // SUID/SGID binary executable only by root (owner).
  std::string owner_only_binary =
      StageCredsBinary("owner_only_suid_binary", S_ISUID | S_ISGID | S_IRWXU | S_IRGRP | S_IROTH);

  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([&]() {
    ASSERT_THAT(setgroups(0, nullptr), SyscallSucceeds());
    ASSERT_TRUE(change_ids(kUser1Uid, kUser1Gid));

    char *const argv_no_exec[] = {const_cast<char *>(no_exec_binary.c_str()), nullptr};
    EXPECT_THAT(execve(no_exec_binary.c_str(), argv_no_exec, nullptr),
                SyscallFailsWithErrno(EACCES));

    char *const argv_owner_only[] = {const_cast<char *>(owner_only_binary.c_str()), nullptr};
    EXPECT_THAT(execve(owner_only_binary.c_str(), argv_owner_only, nullptr),
                SyscallFailsWithErrno(EACCES));
  });
  ASSERT_TRUE(helper.WaitForChildren());
}

TEST_F(SuidTestBase, SuidScriptWithoutExecPermissionFails) {
  std::string interpreter_path = StageCredsBinary(
      "suid_interpreter", S_ISUID | S_ISGID | S_IRWXU | S_IRGRP | S_IXGRP | S_IROTH | S_IXOTH);

  // Script has SUID/SGID and points to an executable SUID/SGID interpreter, but the script itself
  // is not executable by kUser1Uid.
  std::string script_path = StageScript("non_exec_suid_script", interpreter_path,
                                        S_ISUID | S_ISGID | S_IRWXU | S_IRGRP | S_IROTH);

  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([&]() {
    ASSERT_THAT(setgroups(0, nullptr), SyscallSucceeds());
    ASSERT_TRUE(change_ids(kUser1Uid, kUser1Gid));

    char *const argv[] = {const_cast<char *>(script_path.c_str()), nullptr};
    EXPECT_THAT(execve(script_path.c_str(), argv, nullptr), SyscallFailsWithErrno(EACCES));
  });
  ASSERT_TRUE(helper.WaitForChildren());
}

// Because S_ISUID and S_ISGID on a `#!` script are ignored, executing a SUID-root
// script as an unprivileged user does not grant permission to execute a root-only
// (0700) or non-executable (0444) interpreter.
TEST_F(SuidTestBase, SuidScriptCannotExecInaccessibleInterpreter) {
  std::string non_exec_interp =
      StageCredsBinary("non_exec_interp", S_ISUID | S_ISGID | S_IRUSR | S_IRGRP | S_IROTH);
  std::string root_only_interp = StageCredsBinary("root_only_interp", S_ISUID | S_ISGID | S_IRWXU);

  std::string script_non_exec =
      StageScript("script_non_exec_interp", non_exec_interp,
                  S_ISUID | S_ISGID | S_IRWXU | S_IRGRP | S_IXGRP | S_IROTH | S_IXOTH);
  std::string script_root_only =
      StageScript("script_root_only_interp", root_only_interp,
                  S_ISUID | S_ISGID | S_IRWXU | S_IRGRP | S_IXGRP | S_IROTH | S_IXOTH);

  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([&]() {
    ASSERT_THAT(setgroups(0, nullptr), SyscallSucceeds());
    ASSERT_TRUE(change_ids(kUser1Uid, kUser1Gid));

    char *const argv1[] = {const_cast<char *>(script_non_exec.c_str()), nullptr};
    EXPECT_THAT(execve(script_non_exec.c_str(), argv1, nullptr), SyscallFailsWithErrno(EACCES));

    char *const argv2[] = {const_cast<char *>(script_root_only.c_str()), nullptr};
    EXPECT_THAT(execve(script_root_only.c_str(), argv2, nullptr), SyscallFailsWithErrno(EACCES));
  });
  ASSERT_TRUE(helper.WaitForChildren());
}

INSTANTIATE_TEST_SUITE_P(SuidTest, SuidTest, ::testing::Values(0, MS_NOSUID));
