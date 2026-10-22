// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fcntl.h>
#include <lib/fit/defer.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

#include <atomic>
#include <string>
#include <thread>

#include <fbl/unique_fd.h>
#include <gtest/gtest.h>

#include "src/lib/files/file.h"
#include "src/starnix/tests/selinux/userspace/util.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

extern std::string DoPrePolicyLoadWork() { return "mprotect_policy"; }

namespace {

constexpr int kReadIndex = 0;
constexpr int kWriteIndex = 1;
constexpr char kExecmodNoTestLabel[] = "test_u:test_r:mprotect_execmod_no_test_t:s0";

void *GetCurrentStackPage() {
  const long pagesize = sysconf(_SC_PAGESIZE);
  int stack_variable;
  return reinterpret_cast<void *>(((unsigned long)&stack_variable) & ~(pagesize - 1));
}

fbl::unique_fd CreateExecmodTestFile(const test_helper::ScopedTempDir &temp_dir, size_t min_size) {
  std::string file_path = temp_dir.path() + "/test_file";
  if (!files::WriteFile(file_path, "hello")) {
    return fbl::unique_fd();
  }
  fbl::unique_fd fd(open(file_path.c_str(), O_RDWR));
  if (fd.is_valid() && min_size > 5) {
    SAFE_SYSCALL(ftruncate(fd.get(), static_cast<off_t>(min_size)));
  }
  return fd;
}

}  // namespace

/// Check that `execmem` allows making a MAP_STACK mapping executable.
TEST(MProtectTest, ExecMemWorksForMapStack) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  long pagesize = sysconf(_SC_PAGESIZE);
  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:mprotect_execmem_test_t:s0", [&] {
    auto mapping = test_helper::ScopedMMap::MMap(nullptr, pagesize, PROT_NONE,
                                                 MAP_STACK | MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_TRUE(mapping.is_ok()) << mapping.error_value();
    auto addr = mapping->mapping();
    auto result = mprotect(addr, pagesize, PROT_EXEC);
#if defined(__riscv)
    // TODO(https://fxbug.dev/418975186): Fix mprotect returning -1 on RISC-V
    EXPECT_EQ(result, -1);
#else
    EXPECT_EQ(result, 0);
#endif
  }));
}

/// Check that `execmem` allows the initial thread to make a child thread's stack
/// executable.
TEST(MProtectTest, ExecStackOfChildThread) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:mprotect_execmem_test_t:s0", [&] {
    // Create the thread that will return the address of its stack
    struct ThreadArgs {
      // The pipe to notify the parent thread that the child has set the stack
      int stack_was_set_fd[2] = {-1, -1};
      // The pipe to notify the child thread that it can exit
      int child_can_exit_fd[2] = {-1, -1};
      void *stack = nullptr;
    };

    ThreadArgs thread_args;
    ASSERT_THAT(pipe(thread_args.stack_was_set_fd), SyscallSucceeds());
    auto cleanup_stack_was_set = fit::defer([&]() {
      SAFE_SYSCALL(close(thread_args.stack_was_set_fd[0]));
      SAFE_SYSCALL(close(thread_args.stack_was_set_fd[1]));
    });
    ASSERT_THAT(pipe(thread_args.child_can_exit_fd), SyscallSucceeds());
    auto cleanup_child_can_exit = fit::defer([&]() {
      SAFE_SYSCALL(close(thread_args.child_can_exit_fd[0]));
      SAFE_SYSCALL(close(thread_args.child_can_exit_fd[1]));
    });

    pthread_t thread;
    auto thread_lambda = [](void *ptr) -> void * {
      ThreadArgs *args = reinterpret_cast<ThreadArgs *>(ptr);
      args->stack = GetCurrentStackPage();

      // Signal to the parent that the stack address is set
      char ready_signal = 'R';
      SAFE_SYSCALL(write(args->stack_was_set_fd[kWriteIndex], &ready_signal, sizeof(ready_signal)));

      // Wait for the parent to signal that it's done with mprotect
      char exit_signal = 0;
      SAFE_SYSCALL(read(args->child_can_exit_fd[kReadIndex], &exit_signal, sizeof(exit_signal)));
      EXPECT_EQ(exit_signal, 'E');
      return nullptr;
    };

    ASSERT_EQ(pthread_create(&thread, nullptr, +thread_lambda, &thread_args), 0);
    auto join_thread = fit::defer([&]() {
      char exit_signal = 'E';
      SAFE_SYSCALL(
          write(thread_args.child_can_exit_fd[kWriteIndex], &exit_signal, sizeof(exit_signal)));
      pthread_join(thread, nullptr);
    });

    // Wait until the child thread has set the stack and signaled us
    unsigned char ready_ack = 0;
    SAFE_SYSCALL(read(thread_args.stack_was_set_fd[kReadIndex], &ready_ack, sizeof(ready_ack)));
    ASSERT_EQ(ready_ack, 'R');

    long pagesize = sysconf(_SC_PAGESIZE);
    int result = mprotect(thread_args.stack, pagesize, PROT_READ | PROT_WRITE | PROT_EXEC);
    EXPECT_THAT(result, SyscallSucceeds());
  }));
}

#if defined(__x86_64__)  // This test relies on x86 assembly

/// Check that changing the stackpointer before calling `mprotect` allows
/// bypassing the `execstack` permission.
TEST(MProtectTest, ExecStackAfterModifyingStackpointer) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:mprotect_execmem_test_t:s0", [&] {
    pthread_t thread;
    auto thread_lambda = [](void *ptr) -> void * {
      // Reserve an area of memory that the stack pointer will be made to point to
      const long pagesize = sysconf(_SC_PAGESIZE);
      auto mapping = test_helper::ScopedMMap::MMap(nullptr, pagesize, PROT_NONE,
                                                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
      EXPECT_TRUE(mapping.is_ok()) << mapping.error_value();
      void *temporary_stackpointer_value = mapping->mapping();

      // Get the pointer to the stack
      void *stack = GetCurrentStackPage();

      int prot = PROT_READ | PROT_WRITE | PROT_EXEC;
      int result = -1;

      // Temporarily change the stack pointer before calling `mprotect`. Pseudo code equivalent:
      //   %%rbx = %%rsp
      //   %%rsp = temporary_stackpointer_value
      //   result = mprotect(stack, pagesize, prot)
      //   %%rsp = %%rbx
      asm volatile(
          "movq %1, %%rdi\n"     // arg1: addr (rdi)
          "movq %2, %%rsi\n"     // arg2: len (rsi)
          "movl %3, %%edx\n"     // arg3: prot (rdx)
          "movl %4, %%eax\n"     // syscall number (eax)
          "movq %%rsp, %%rbx\n"  // store the stack pointer into rbx
          "movq %5, %%rsp\n"     // set the stack pointer to point to the temporary mapping
          "syscall\n"            // execute syscall
          "movq %%rbx, %%rsp\n"  // restore the stack pointer
          "movl %%eax, %0\n"     // move return value from eax to result
          : "=r"(result)
          : "r"(stack), "r"(pagesize), "r"(prot), "i"(__NR_mprotect),
            "r"(temporary_stackpointer_value)
          : "rdi", "rsi", "rdx", "rax", "memory", "cc"  // Clobbered registers
      );
      EXPECT_EQ(result, 0);
      return nullptr;
    };
    pthread_create(&thread, nullptr, +thread_lambda, nullptr);
    pthread_join(thread, nullptr);
  }));
}

#endif  // defined(__x86_64__)

/// Check that with `execmem` a signal handler can make executable the stack of its thread, but that
/// it can't make executable its own stack.
TEST(MProtectTest, ExecStackInSignal) {
  auto enforce = ScopedEnforcement::SetEnforcing();

  // Static variables used by the signal handler to communicate
  // with the outside world.
  static int fd[2];
  static void *initial_stack = nullptr;
  static int mprotect_signal_stack_result = -2;
  static int mprotect_initial_stack_result = -2;

  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:mprotect_execmem_test_t:s0", [&] {
    // Create a child thread, because the initial thread of the process is special and doesn't
    // rely on the stack pointer to determine whether execstack is required.
    pthread_t thread;
    auto thread_lambda = [](void *ptr) -> void * {
      initial_stack = GetCurrentStackPage();
      if (pipe(fd) != 0) {
        ADD_FAILURE() << "pipe failed: " << strerror(errno);
        return nullptr;
      }
      auto cleanup = fit::defer([&]() {
        SAFE_SYSCALL(close(fd[0]));
        SAFE_SYSCALL(close(fd[1]));
      });

      // Configure the stack to be used by the signals
      stack_t ss;
      memset(&ss, 0, sizeof(ss));
      auto mapping = test_helper::ScopedMMap::MMap(nullptr, SIGSTKSZ, PROT_READ | PROT_WRITE,
                                                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
      EXPECT_TRUE(mapping.is_ok()) << mapping.error_value();
      ss.ss_sp = mapping->mapping();
      ss.ss_size = SIGSTKSZ;
      ss.ss_flags = 0;
      SAFE_SYSCALL(sigaltstack(&ss, nullptr) == -1);

      // Configure the signal handler
      auto signal_handler = [](int signum) -> void {
        void *signal_stack = GetCurrentStackPage();
        long pagesize = sysconf(_SC_PAGESIZE);
        int prot = PROT_READ | PROT_WRITE | PROT_EXEC;
        mprotect_signal_stack_result = mprotect(signal_stack, pagesize, prot);
        mprotect_initial_stack_result = mprotect(initial_stack, pagesize, prot);
        char c = 1;
        write(fd[kWriteIndex], &c, 1);
      };
      struct sigaction sa;
      memset(&sa, 0, sizeof(sa));
      sa.sa_handler = +signal_handler;
      sa.sa_flags = SA_ONSTACK;
      SAFE_SYSCALL(sigaction(SIGUSR1, &sa, nullptr));

      // Send the signal
      pthread_kill(pthread_self(), SIGUSR1);

      // Wait for the signal handler to notify us that it has finished writing the results
      char boolean_result;
      SAFE_SYSCALL(read(fd[kReadIndex], &boolean_result, sizeof(boolean_result)));

      // Check the results
      EXPECT_EQ(mprotect_signal_stack_result, -1);
      EXPECT_EQ(mprotect_initial_stack_result, 0);

      return nullptr;
    };
    pthread_create(&thread, nullptr, +thread_lambda, nullptr);
    pthread_join(thread, nullptr);
  }));
}

struct ExecmodTestParam {
  // Whether the test is executed with the execmod permission.
  bool has_execmod_perm = false;
  // Whether the mapping is private or shared.
  bool private_mapping = false;
  // Whether the test should modify the mapped memory.
  bool modify_mapping = false;
  // Whether mprotect is expected to succeed or not.
  bool expect_success = false;
  std::string TestSuffixGenerator() const {
    std::string suffix;
    if (has_execmod_perm) {
      suffix += "ExecmodYes_";
    } else {
      suffix += "ExecmodNo_";
    }
    if (private_mapping) {
      suffix += "PrivateMapping_";
    } else {
      suffix += "SharedMapping_";
    }
    if (modify_mapping) {
      suffix += "Modification_";
    } else {
      suffix += "NoModification_";
    }
    if (expect_success) {
      suffix += "MprotectSuccess";
    } else {
      suffix += "MprotectFailure";
    }
    return suffix;
  }
};

class MProtectExecmod : public testing::TestWithParam<ExecmodTestParam> {};

// Checks that making executable a mapping of a file requires the
// `execmod` permission if the mapping was private and was modified.
TEST_P(MProtectExecmod, ExecMod) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  const auto param = MProtectExecmod::GetParam();
  auto label =
      param.has_execmod_perm ? "test_u:test_r:mprotect_execmod_yes_test_t:s0" : kExecmodNoTestLabel;
  ASSERT_TRUE(RunSubprocessAs(label, [&] {
    // Create a temporary file.
    const long pagesize = sysconf(_SC_PAGESIZE);
    test_helper::ScopedTempDir temp_dir;
    fbl::unique_fd fd = CreateExecmodTestFile(temp_dir, pagesize);
    ASSERT_TRUE(fd.is_valid());

    // Creates a mapping of the file, either private or shared.
    auto flags = param.private_mapping ? MAP_PRIVATE : MAP_SHARED;
    auto mapping = test_helper::ScopedMMap::MMap(
        /* addr = */ nullptr, pagesize, PROT_READ | PROT_WRITE, flags, fd.get(), /* offset = */ 0);
    ASSERT_TRUE(mapping.is_ok()) << mapping.error_value();

    // Modify the mapping.
    if (param.modify_mapping) {
      static_cast<char *>(mapping->mapping())[0] = 'j';
    }

    // Try to make the mapping executable.
    int result = mprotect(mapping->mapping(), pagesize, PROT_READ | PROT_EXEC);
    if (param.expect_success) {
      EXPECT_THAT(result, SyscallSucceeds());
    } else {
      EXPECT_THAT(result, SyscallFailsWithErrno(EACCES));
    }
  }));
}

const auto kExecmodSuccessFailureValues = ::testing::Values(
    // `execmod` required if a private mapping is modified.
    ExecmodTestParam{.has_execmod_perm = false,
                     .private_mapping = true,
                     .modify_mapping = true,
                     .expect_success = false},
    ExecmodTestParam{.has_execmod_perm = true,
                     .private_mapping = true,
                     .modify_mapping = true,
                     .expect_success = true},
    // `execmod` not required if the mapping is not private.
    ExecmodTestParam{.has_execmod_perm = false,
                     .private_mapping = false,
                     .modify_mapping = true,
                     .expect_success = true},
    // `execmod` not required if the mapping is not modified.
    ExecmodTestParam{.has_execmod_perm = false,
                     .private_mapping = true,
                     .modify_mapping = false,
                     .expect_success = true});
INSTANTIATE_TEST_SUITE_P(MProtectExecmod, MProtectExecmod, kExecmodSuccessFailureValues,
                         [](const testing::TestParamInfo<ExecmodTestParam> &info) {
                           return info.param.TestSuffixGenerator();
                         });

// Checks that `fork()` preserves the copy-on-write state of a private file mapping in the child
// process: making an unmodified mapping executable in the child succeeds without `execmod`, while
// making a modified mapping executable in the child requires `execmod`.
TEST(ExecModTest, ForkPreservesCowModifiedInChild) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  ASSERT_TRUE(RunSubprocessAs(kExecmodNoTestLabel, [&] {
    const long pagesize = sysconf(_SC_PAGESIZE);
    test_helper::ScopedTempDir temp_dir;
    fbl::unique_fd fd = CreateExecmodTestFile(temp_dir, pagesize);
    ASSERT_TRUE(fd.is_valid());

    for (bool modify_mapping : {false, true}) {
      auto mapping = test_helper::ScopedMMap::MMap(
          /* addr = */ nullptr, pagesize, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd.get(),
          /* offset = */ 0);
      ASSERT_TRUE(mapping.is_ok()) << mapping.error_value();

      if (modify_mapping) {
        static_cast<char *>(mapping->mapping())[0] = 'j';
      }

      test_helper::ForkHelper helper;
      helper.RunInForkedProcess([&] {
        int result = mprotect(mapping->mapping(), pagesize, PROT_READ | PROT_EXEC);
        if (modify_mapping) {
          EXPECT_THAT(result, SyscallFailsWithErrno(EACCES));
        } else {
          EXPECT_THAT(result, SyscallSucceeds());
        }
      });
      EXPECT_TRUE(helper.WaitForChildren());
    }
  }));
}

// Checks that `fork()` preserves the copy-on-write state of a private file mapping in the parent
// process while a child process shares the pages.
TEST(ExecModTest, ForkPreservesCowModifiedInParent) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  ASSERT_TRUE(RunSubprocessAs(kExecmodNoTestLabel, [&] {
    const long pagesize = sysconf(_SC_PAGESIZE);
    test_helper::ScopedTempDir temp_dir;
    fbl::unique_fd fd = CreateExecmodTestFile(temp_dir, pagesize);
    ASSERT_TRUE(fd.is_valid());

    for (bool modify_mapping : {false, true}) {
      auto mapping = test_helper::ScopedMMap::MMap(
          /* addr = */ nullptr, pagesize, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd.get(),
          /* offset = */ 0);
      ASSERT_TRUE(mapping.is_ok()) << mapping.error_value();

      if (modify_mapping) {
        static_cast<char *>(mapping->mapping())[0] = 'j';
      }

      test_helper::EventFdSem sem(0);
      test_helper::ForkHelper helper;
      helper.RunInForkedProcess([&] { EXPECT_THAT(sem.Wait(), SyscallSucceeds()); });

      int result = mprotect(mapping->mapping(), pagesize, PROT_READ | PROT_EXEC);
      if (modify_mapping) {
        EXPECT_THAT(result, SyscallFailsWithErrno(EACCES));
      } else {
        EXPECT_THAT(result, SyscallSucceeds());
      }

      EXPECT_THAT(sem.Notify(1), SyscallSucceeds());
      EXPECT_TRUE(helper.WaitForChildren());
    }
  }));
}

// Checks that shrinking and regrowing a private file mapping in-place with `mremap()` preserves
// its copy-on-write state on the regrown page.
TEST(ExecModTest, ShrinkAndRegrowPreservesCowModified) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  ASSERT_TRUE(RunSubprocessAs(kExecmodNoTestLabel, [&] {
    const long pagesize = sysconf(_SC_PAGESIZE);
    test_helper::ScopedTempDir temp_dir;
    fbl::unique_fd fd = CreateExecmodTestFile(temp_dir, pagesize * 2);
    ASSERT_TRUE(fd.is_valid());

    for (bool modify_mapping : {false, true}) {
      size_t map_len = pagesize * 2;
      void *map_addr = mmap(/* addr = */ nullptr, map_len, PROT_READ | PROT_WRITE, MAP_PRIVATE,
                            fd.get(), /* offset = */ 0);
      ASSERT_NE(map_addr, MAP_FAILED) << strerror(errno);
      auto cleanup_map = fit::defer([&] { SAFE_SYSCALL(munmap(map_addr, map_len)); });

      // Unmap the second page so the mapping can later be grown in-place by `mremap`.
      SAFE_SYSCALL(munmap(static_cast<char *>(map_addr) + pagesize, pagesize));
      map_len = pagesize;

      if (modify_mapping) {
        static_cast<char *>(map_addr)[0] = 'j';
      }

      // Grow the mapping in-place back to 2 pages.
      void *grown = mremap(map_addr, pagesize, pagesize * 2, /* flags = */ 0);
      ASSERT_EQ(grown, map_addr) << strerror(errno);
      map_len = pagesize * 2;

      int result =
          mprotect(static_cast<char *>(map_addr) + pagesize, pagesize, PROT_READ | PROT_EXEC);
      if (modify_mapping) {
        EXPECT_THAT(result, SyscallFailsWithErrno(EACCES));
      } else {
        EXPECT_THAT(result, SyscallSucceeds());
      }
    }
  }));
}

// Checks that moving a private file mapping to a new address with `mremap()` preserves its
// copy-on-write state.
TEST(ExecModTest, MovePreservesCowModified) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  ASSERT_TRUE(RunSubprocessAs(kExecmodNoTestLabel, [&] {
    const long pagesize = sysconf(_SC_PAGESIZE);
    test_helper::ScopedTempDir temp_dir;
    fbl::unique_fd fd = CreateExecmodTestFile(temp_dir, pagesize);
    ASSERT_TRUE(fd.is_valid());

    for (bool modify_mapping : {false, true}) {
      void *map_addr = mmap(/* addr = */ nullptr, pagesize, PROT_READ | PROT_WRITE, MAP_PRIVATE,
                            fd.get(), /* offset = */ 0);
      ASSERT_NE(map_addr, MAP_FAILED) << strerror(errno);
      auto cleanup_map = fit::defer([&] { SAFE_SYSCALL(munmap(map_addr, pagesize)); });

      if (modify_mapping) {
        static_cast<char *>(map_addr)[0] = 'j';
      }

      void *dst_addr = mmap(/* addr = */ nullptr, pagesize, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS,
                            /* fd = */ -1, /* offset = */ 0);
      ASSERT_NE(dst_addr, MAP_FAILED) << strerror(errno);
      auto cleanup_dst = fit::defer([&] { SAFE_SYSCALL(munmap(dst_addr, pagesize)); });

      void *moved = mremap(map_addr, pagesize, pagesize, MREMAP_MAYMOVE | MREMAP_FIXED, dst_addr);
      ASSERT_EQ(moved, dst_addr) << strerror(errno);
      cleanup_dst.cancel();
      map_addr = dst_addr;

      int result = mprotect(map_addr, pagesize, PROT_READ | PROT_EXEC);
      if (modify_mapping) {
        EXPECT_THAT(result, SyscallFailsWithErrno(EACCES));
      } else {
        EXPECT_THAT(result, SyscallSucceeds());
      }
    }
  }));
}

// Checks that a modified private file mapping that is shrunk and regrown in-place with `mremap()`
// still requires `execmod` in a child process after `fork()`.
TEST(ExecModTest, ShrinkAndRegrowPreservesCowModifiedAcrossFork) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  ASSERT_TRUE(RunSubprocessAs(kExecmodNoTestLabel, [&] {
    const long pagesize = sysconf(_SC_PAGESIZE);
    test_helper::ScopedTempDir temp_dir;
    fbl::unique_fd fd = CreateExecmodTestFile(temp_dir, pagesize * 2);
    ASSERT_TRUE(fd.is_valid());

    size_t map_len = pagesize * 2;
    void *map_addr = mmap(/* addr = */ nullptr, map_len, PROT_READ | PROT_WRITE, MAP_PRIVATE,
                          fd.get(), /* offset = */ 0);
    ASSERT_NE(map_addr, MAP_FAILED) << strerror(errno);
    auto cleanup_map = fit::defer([&] { SAFE_SYSCALL(munmap(map_addr, map_len)); });

    SAFE_SYSCALL(munmap(static_cast<char *>(map_addr) + pagesize, pagesize));
    map_len = pagesize;

    static_cast<char *>(map_addr)[0] = 'j';

    void *grown = mremap(map_addr, pagesize, pagesize * 2, /* flags = */ 0);
    ASSERT_EQ(grown, map_addr) << strerror(errno);
    map_len = pagesize * 2;

    test_helper::ForkHelper helper;
    helper.RunInForkedProcess([&] {
      EXPECT_THAT(
          mprotect(static_cast<char *>(map_addr) + pagesize, pagesize, PROT_READ | PROT_EXEC),
          SyscallFailsWithErrno(EACCES));
    });
    EXPECT_TRUE(helper.WaitForChildren());
  }));
}

// Checks that a modified private file mapping moved with `mremap()` still requires `execmod` in a
// child process after `fork()`.
TEST(ExecModTest, MovePreservesCowModifiedAcrossFork) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  ASSERT_TRUE(RunSubprocessAs(kExecmodNoTestLabel, [&] {
    const long pagesize = sysconf(_SC_PAGESIZE);
    test_helper::ScopedTempDir temp_dir;
    fbl::unique_fd fd = CreateExecmodTestFile(temp_dir, pagesize);
    ASSERT_TRUE(fd.is_valid());

    void *map_addr = mmap(/* addr = */ nullptr, pagesize, PROT_READ | PROT_WRITE, MAP_PRIVATE,
                          fd.get(), /* offset = */ 0);
    ASSERT_NE(map_addr, MAP_FAILED) << strerror(errno);
    auto cleanup_map = fit::defer([&] { SAFE_SYSCALL(munmap(map_addr, pagesize)); });

    static_cast<char *>(map_addr)[0] = 'j';

    void *dst_addr = mmap(/* addr = */ nullptr, pagesize, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS,
                          /* fd = */ -1, /* offset = */ 0);
    ASSERT_NE(dst_addr, MAP_FAILED) << strerror(errno);
    auto cleanup_dst = fit::defer([&] { SAFE_SYSCALL(munmap(dst_addr, pagesize)); });

    void *moved = mremap(map_addr, pagesize, pagesize, MREMAP_MAYMOVE | MREMAP_FIXED, dst_addr);
    ASSERT_EQ(moved, dst_addr) << strerror(errno);
    cleanup_dst.cancel();
    map_addr = dst_addr;

    test_helper::ForkHelper helper;
    helper.RunInForkedProcess([&] {
      EXPECT_THAT(mprotect(map_addr, pagesize, PROT_READ | PROT_EXEC),
                  SyscallFailsWithErrno(EACCES));
    });
    EXPECT_TRUE(helper.WaitForChildren());
  }));
}

// Checks that `execmod` is not required on a second private mapping of the same file when only
// the first private mapping was modified.
TEST(ExecModTest, SecondMappingIsNotCowModified) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  ASSERT_TRUE(RunSubprocessAs(kExecmodNoTestLabel, [&] {
    const long pagesize = sysconf(_SC_PAGESIZE);
    test_helper::ScopedTempDir temp_dir;
    fbl::unique_fd fd = CreateExecmodTestFile(temp_dir, pagesize);
    ASSERT_TRUE(fd.is_valid());

    auto first_mapping = test_helper::ScopedMMap::MMap(
        /* addr = */ nullptr, pagesize, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd.get(),
        /* offset = */ 0);
    ASSERT_TRUE(first_mapping.is_ok()) << first_mapping.error_value();
    static_cast<char *>(first_mapping->mapping())[0] = 'j';

    auto second_mapping = test_helper::ScopedMMap::MMap(
        /* addr = */ nullptr, pagesize, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd.get(),
        /* offset = */ 0);
    ASSERT_TRUE(second_mapping.is_ok()) << second_mapping.error_value();

    EXPECT_THAT(mprotect(second_mapping->mapping(), pagesize, PROT_READ | PROT_EXEC),
                SyscallSucceeds());
  }));
}

// While a writable private file mapping is made executable, writes to it are temporarily blocked,
// so that none can land between the `execmod` check and the protection change. This race itself
// can't be tested deterministically. Instead, this checks that a thread that keeps writing to the
// mapping is unaffected by the blocking: its write faults are retried rather than reported as
// SIGSEGV, and write access is restored after the attempt is denied. The mapping was written
// before the attempt, so the denial itself is deterministic.
TEST(MProtectTest, ExecmodWithConcurrentWrites) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:mprotect_execmod_no_test_t:s0", [&] {
    test_helper::ScopedTempDir temp_dir;
    std::string file_path = temp_dir.path() + "/test_file";
    ASSERT_TRUE(files::WriteFile(file_path, "hello"));
    auto fd = fbl::unique_fd(open(file_path.c_str(), O_RDWR));
    ASSERT_TRUE(fd.is_valid());

    long pagesize = sysconf(_SC_PAGESIZE);
    auto mapping = test_helper::ScopedMMap::MMap(nullptr, pagesize, PROT_READ | PROT_WRITE,
                                                 MAP_PRIVATE, fd.get(), 0);
    ASSERT_TRUE(mapping.is_ok()) << mapping.error_value();
    volatile char *page = static_cast<volatile char *>(mapping->mapping());

    // Keep writing to the mapping from another thread.
    std::atomic<bool> stop = false;
    std::atomic<uint64_t> writes = 0;
    std::thread writer([&] {
      while (!stop) {
        page[1] = 'j';
        writes.fetch_add(1);
      }
    });
    auto stop_writer = fit::defer([&] {
      stop = true;
      writer.join();
    });
    // Returns whether at least `count` writes were made within 10 seconds.
    auto wait_for_writes = [&](uint64_t count) {
      for (int i = 0; i < 10000 && writes < count; i++) {
        usleep(/*usec=*/1000);
      }
      return writes >= count;
    };
    ASSERT_TRUE(wait_for_writes(/*count=*/1));

    // The mapping was modified, so making it executable requires `execmod`.
    EXPECT_THAT(mprotect(mapping->mapping(), pagesize, PROT_READ | PROT_EXEC),
                SyscallFailsWithErrno(EACCES));

    // The mapping is still writable.
    EXPECT_TRUE(wait_for_writes(/*count=*/writes + 1));
  }));
}

class MProtectExecstack : public testing::TestWithParam<std::pair<const char *, bool>> {};

// Test making the stack of the initial thread executable from *another* thread.
// This works with `execstack`, but does not work with `execmem`.
TEST_P(MProtectExecstack, MakeInitialStackExecFromOtherThread) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  const auto [label, expect_success] = MProtectExecstack::GetParam();
  ASSERT_TRUE(RunSubprocessAs(label, [&] {
    struct ThreadArgs {
      // Stores a pointer to the stack of the initial thread
      void *stack = nullptr;
      // Stores the result of the call to the `mprotect` syscall
      std::atomic<int> mprotect_result = 0;
    };
    ThreadArgs thread_args;
    thread_args.stack = GetCurrentStackPage();
    pthread_t thread;
    auto thread_lambda = [](void *ptr) -> void * {
      // The child thread will call mprotect on the stack of the initial thread.
      ThreadArgs *args = reinterpret_cast<ThreadArgs *>(ptr);
      long pagesize = sysconf(_SC_PAGESIZE);
      args->mprotect_result = mprotect(args->stack, pagesize, PROT_READ | PROT_WRITE | PROT_EXEC);
      return nullptr;
    };
    pthread_create(&thread, nullptr, +thread_lambda, &thread_args);
    pthread_join(thread, nullptr);
    if (expect_success) {
      EXPECT_EQ(thread_args.mprotect_result, 0);
    } else {
      EXPECT_EQ(thread_args.mprotect_result, -1);
    }
  }));
}

const auto kExecstackSuccessFailureValues =
    ::testing::Values(std::make_pair("test_u:test_r:mprotect_execstack_test_t:s0", true),
                      std::make_pair("test_u:test_r:mprotect_execmem_test_t:s0", false));
INSTANTIATE_TEST_SUITE_P(MProtectExecstack, MProtectExecstack, kExecstackSuccessFailureValues);
