// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/client/process_impl.h"

#include <gtest/gtest.h>

#include "src/developer/debug/zxdb/client/frame.h"
#include "src/developer/debug/zxdb/client/memory_dump.h"
#include "src/developer/debug/zxdb/client/mock_remote_api.h"
#include "src/developer/debug/zxdb/client/process_observer.h"
#include "src/developer/debug/zxdb/client/remote_api_test.h"
#include "src/developer/debug/zxdb/client/session.h"
#include "src/developer/debug/zxdb/client/target_impl.h"
#include "src/developer/debug/zxdb/client/thread.h"
#include "src/developer/debug/zxdb/client/thread_observer.h"
#include "src/developer/debug/zxdb/symbols/elf_symbol.h"
#include "src/developer/debug/zxdb/symbols/function.h"
#include "src/developer/debug/zxdb/symbols/mock_module_symbols.h"

namespace zxdb {

namespace {

class ProcessSink : public MockRemoteAPI {
 public:
  ProcessSink() = default;
  ~ProcessSink() = default;

  const debug_ipc::ResumeRequest& resume_request() const { return resume_request_; }
  int resume_count() const { return resume_count_; }

  int modules_count() const { return modules_count_; }
  void set_modules_reply(debug_ipc::ModulesReply reply) { modules_reply_ = std::move(reply); }

  void Resume(const debug_ipc::ResumeRequest& request,
              fit::callback<void(const Err&, debug_ipc::ResumeReply)> cb) override {
    resume_count_++;
    resume_request_ = request;
    debug::MessageLoop::Current()->PostTask(
        FROM_HERE, [cb = std::move(cb)]() mutable { cb(Err(), debug_ipc::ResumeReply()); });
  }

  void Modules(const debug_ipc::ModulesRequest& request,
               fit::callback<void(const Err&, debug_ipc::ModulesReply)> cb) override {
    modules_count_++;
    debug::MessageLoop::Current()->PostTask(
        FROM_HERE,
        [cb = std::move(cb), reply = modules_reply_]() mutable { cb(Err(), std::move(reply)); });
  }

  // No-op.
  void Threads(const debug_ipc::ThreadsRequest& request,
               fit::callback<void(const Err&, debug_ipc::ThreadsReply)> cb) override {
    thread_request_made_ = true;
  }

  bool thread_request_made() const { return thread_request_made_; }

 private:
  debug_ipc::ResumeRequest resume_request_;
  int resume_count_ = 0;

  debug_ipc::ModulesReply modules_reply_;
  int modules_count_ = 0;

  bool thread_request_made_ = false;
};

class ProcessImplTest : public RemoteAPITest {
 public:
  ProcessImplTest() = default;
  ~ProcessImplTest() override = default;

  ProcessSink* sink() { return sink_; }

 private:
  std::unique_ptr<RemoteAPI> GetRemoteAPIImpl() override {
    auto sink = std::make_unique<ProcessSink>();
    sink_ = sink.get();
    return sink;
  }

 private:
  ProcessSink* sink_;  // Owned by the session.
};

// Tests that the correct threads are resumed after the modules are loaded.
TEST_F(ProcessImplTest, OnModules) {
  constexpr uint64_t kProcessKoid = 1234;
  Process* process = InjectProcess(kProcessKoid);
  ASSERT_TRUE(process);

  EXPECT_FALSE(sink()->thread_request_made());

  debug_ipc::NotifyModules notify;
  notify.process_koid = kProcessKoid;
  notify.modules.resize(1);
  notify.modules[0].name = "comctl32.dll";
  notify.modules[0].base = 0x7685348234;

  session().DispatchNotifyModules(notify);

  EXPECT_TRUE(sink()->thread_request_made());

  // Should have resumed both of those threads.
  ASSERT_EQ(1, sink()->resume_count());
  const debug_ipc::ResumeRequest& resume = sink()->resume_request();
  EXPECT_EQ(debug_ipc::ResumeRequest::How::kResolveAndContinue, resume.how);
  EXPECT_EQ(1U, resume.ids.size());
  EXPECT_EQ(debug_ipc::ProcessThreadId{.process = kProcessKoid}, resume.ids[0]);
}

TEST_F(ProcessImplTest, GetTLSHelpers) {
  constexpr uint64_t kProcessKoid = 1234;
  constexpr uint64_t kThreadKoid = 5678;
  constexpr char kBuildId[] = "abcd";
  constexpr uint64_t kModuleBase = 0x1000;
  Process* process = InjectProcess(kProcessKoid);
  ASSERT_TRUE(process);
  Thread* thread = InjectThread(kProcessKoid, kThreadKoid);
  ASSERT_TRUE(thread);
  SymbolContext symbol_context(kModuleBase);

  auto module_symbols = fxl::MakeRefCounted<MockModuleSymbols>("libc.so");
  session().system().GetSymbols()->InjectModuleForTesting(kBuildId, module_symbols.get());

  std::vector<debug_ipc::Module> modules;
  debug_ipc::Module module;
  module.name = "libc";
  module.base = kModuleBase;
  module.build_id = kBuildId;
  modules.push_back(module);

  TargetImpl* target = session().system().GetTargetImpls()[0];
  target->process()->OnModules(modules);

  constexpr uint64_t kThrdTAddr = 0x1000;
  constexpr uint64_t kLinkMapTlsModIdAddr = 0x2000;
  constexpr uint64_t kTlsBaseAddr = 0x3000;
  constexpr uint64_t kSymSize = 4;

  ElfSymbolRecord thrd_t_record(ElfSymbolType::kNormal, kThrdTAddr, kSymSize, "zxdb.thrd_t");
  ElfSymbolRecord link_map_tls_modid_record(ElfSymbolType::kNormal, kLinkMapTlsModIdAddr, kSymSize,
                                            "zxdb.link_map_tls_modid");
  ElfSymbolRecord tlsbase_record(ElfSymbolType::kNormal, kTlsBaseAddr, kSymSize, "zxdb.tlsbase");

  auto thrd_t_sym = fxl::MakeRefCounted<ElfSymbol>(module_symbols->GetWeakPtr(), thrd_t_record);
  auto link_map_tls_modid_sym =
      fxl::MakeRefCounted<ElfSymbol>(module_symbols->GetWeakPtr(), link_map_tls_modid_record);
  auto tlsbase_sym = fxl::MakeRefCounted<ElfSymbol>(module_symbols->GetWeakPtr(), tlsbase_record);

  module_symbols->AddSymbolLocations(
      Identifier(IdentifierComponent(SpecialIdentifier::kElf, "zxdb.thrd_t")),
      {Location(kThrdTAddr + kModuleBase, FileLine(), 0, symbol_context, thrd_t_sym)});
  module_symbols->AddSymbolLocations(
      Identifier(IdentifierComponent(SpecialIdentifier::kElf, "zxdb.link_map_tls_modid")),
      {Location(kLinkMapTlsModIdAddr + kModuleBase, FileLine(), 0, symbol_context,
                link_map_tls_modid_sym)});
  module_symbols->AddSymbolLocations(
      Identifier(IdentifierComponent(SpecialIdentifier::kElf, "zxdb.tlsbase")),
      {Location(kTlsBaseAddr + kModuleBase, FileLine(), 0, symbol_context, tlsbase_sym)});

  sink()->AddMemory(kThrdTAddr + kModuleBase, {0x1, 0x2, 0x3, 0x4});
  sink()->AddMemory(kLinkMapTlsModIdAddr + kModuleBase, {0x5, 0x6, 0x7, 0x8});
  sink()->AddMemory(kTlsBaseAddr + kModuleBase, {0x9, 0xa, 0xb, 0xc});

  bool called = false;
  bool called_2 = false;
  Process::TLSHelpers helpers;
  Process::TLSHelpers helpers_2;
  Err err;
  Err err_2;
  process->GetTLSHelpers([&called, &helpers, &err](ErrOr<const Process::TLSHelpers*> got) {
    called = true;

    if (got.has_error()) {
      err = got.err();
    } else {
      helpers = *got.value();
    }
  });

  process->GetTLSHelpers([&called_2, &helpers_2, &err_2](ErrOr<const Process::TLSHelpers*> got) {
    called_2 = true;

    if (got.has_error()) {
      err_2 = got.err();
    } else {
      helpers_2 = *got.value();
    }
  });

  EXPECT_FALSE(called);
  EXPECT_FALSE(called_2);
  loop().RunUntilNoTasks();
  EXPECT_TRUE(called);
  EXPECT_TRUE(called_2);
  ASSERT_FALSE(err.has_error()) << err.msg();
  EXPECT_FALSE(err_2.has_error());

  ASSERT_EQ(helpers.thrd_t.size(), kSymSize);
  ASSERT_EQ(helpers.link_map_tls_modid.size(), kSymSize);
  ASSERT_EQ(helpers.tlsbase.size(), kSymSize);

  EXPECT_EQ(0x1, helpers.thrd_t[0]);
  EXPECT_EQ(0x2, helpers.thrd_t[1]);
  EXPECT_EQ(0x3, helpers.thrd_t[2]);
  EXPECT_EQ(0x4, helpers.thrd_t[3]);

  EXPECT_EQ(0x5, helpers.link_map_tls_modid[0]);
  EXPECT_EQ(0x6, helpers.link_map_tls_modid[1]);
  EXPECT_EQ(0x7, helpers.link_map_tls_modid[2]);
  EXPECT_EQ(0x8, helpers.link_map_tls_modid[3]);

  EXPECT_EQ(0x9, helpers.tlsbase[0]);
  EXPECT_EQ(0xa, helpers.tlsbase[1]);
  EXPECT_EQ(0xb, helpers.tlsbase[2]);
  EXPECT_EQ(0xc, helpers.tlsbase[3]);

  EXPECT_EQ(0x1, helpers_2.thrd_t[0]);
  EXPECT_EQ(0x2, helpers_2.thrd_t[1]);
  EXPECT_EQ(0x3, helpers_2.thrd_t[2]);
  EXPECT_EQ(0x4, helpers_2.thrd_t[3]);

  EXPECT_EQ(0x5, helpers_2.link_map_tls_modid[0]);
  EXPECT_EQ(0x6, helpers_2.link_map_tls_modid[1]);
  EXPECT_EQ(0x7, helpers_2.link_map_tls_modid[2]);
  EXPECT_EQ(0x8, helpers_2.link_map_tls_modid[3]);

  EXPECT_EQ(0x9, helpers_2.tlsbase[0]);
  EXPECT_EQ(0xa, helpers_2.tlsbase[1]);
  EXPECT_EQ(0xb, helpers_2.tlsbase[2]);
  EXPECT_EQ(0xc, helpers_2.tlsbase[3]);

  called = false;
  process->GetTLSHelpers([&called, &helpers, &err](ErrOr<const Process::TLSHelpers*> got) {
    called = true;

    if (got.has_error()) {
      err = got.err();
    } else {
      helpers = *got.value();
    }
  });

  EXPECT_TRUE(called);
  ASSERT_FALSE(err.has_error()) << err.msg();

  ASSERT_EQ(helpers.thrd_t.size(), kSymSize);
  ASSERT_EQ(helpers.link_map_tls_modid.size(), kSymSize);
  ASSERT_EQ(helpers.tlsbase.size(), kSymSize);

  EXPECT_EQ(0x1, helpers.thrd_t[0]);
  EXPECT_EQ(0x2, helpers.thrd_t[1]);
  EXPECT_EQ(0x3, helpers.thrd_t[2]);
  EXPECT_EQ(0x4, helpers.thrd_t[3]);

  EXPECT_EQ(0x5, helpers.link_map_tls_modid[0]);
  EXPECT_EQ(0x6, helpers.link_map_tls_modid[1]);
  EXPECT_EQ(0x7, helpers.link_map_tls_modid[2]);
  EXPECT_EQ(0x8, helpers.link_map_tls_modid[3]);

  EXPECT_EQ(0x9, helpers.tlsbase[0]);
  EXPECT_EQ(0xa, helpers.tlsbase[1]);
  EXPECT_EQ(0xb, helpers.tlsbase[2]);
  EXPECT_EQ(0xc, helpers.tlsbase[3]);
}

TEST_F(ProcessImplTest, ReadMemoryAutomation) {
  constexpr uint64_t kProcessKoid = 1234;
  constexpr uint64_t kThreadKoid = 5678;
  constexpr uint64_t kBlockAddress = 0xdeadbeef;
  constexpr uint64_t kBlockSize = 8;
  Process* process = InjectProcess(kProcessKoid);
  ASSERT_TRUE(process);
  ProcessImpl* process_impl = session().system().ProcessImplFromKoid(process->GetKoid());
  ASSERT_TRUE(process_impl);

  // This creates the memory block to mimic getting a response from an automated breakpoint.
  debug_ipc::MemoryBlock mem_block = {.address = kBlockAddress, .valid = true, .size = kBlockSize};
  mem_block.data = {0, 1, 2, 3, 4, 5, 6, 7};

  debug_ipc::MemoryBlock invalid_mem_block = {
      .address = kBlockAddress + 32, .valid = false, .size = kBlockSize};
  invalid_mem_block.data = {10, 11, 12, 13, 14, 15, 16, 17};

  std::vector<debug_ipc::MemoryBlock> mem_block_vect = {mem_block, invalid_mem_block};
  process_impl->SetMemoryBlocks(kThreadKoid, mem_block_vect);

  // This puts some memory on the mock remote device such that if the read misses the memory block
  // it'll read from this array.
  sink()->AddMemory(kBlockAddress - 4, {100, 101, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111,
                                        112, 113, 114, 115});

  // Attempt to read the memory block. The read request must be for a subset of one of the blocks
  // returned for it to hit the memory block, otherwise it falls through to the remote device.
  process_impl->ReadMemory(
      kBlockAddress, kBlockSize, [](const zxdb::Err& err, zxdb::MemoryDump dump) {
        ASSERT_EQ(dump.blocks()[0].data, std::vector<uint8_t>({0, 1, 2, 3, 4, 5, 6, 7}));
        ASSERT_TRUE(dump.blocks()[0].valid);
      });

  // Read with a lower address, which should fall through to the remote device.
  process_impl->ReadMemory(kBlockAddress - 2, kBlockSize,
                           [](const zxdb::Err& err, zxdb::MemoryDump dump) {
                             ASSERT_EQ(dump.blocks()[0].data, std::vector<uint8_t>({
                                                                  102,
                                                                  103,
                                                                  104,
                                                                  105,
                                                                  106,
                                                                  107,
                                                                  108,
                                                                  109,
                                                              }));
                             ASSERT_TRUE(dump.blocks()[0].valid);
                           });

  // Read with a too large size, which should also fall through to the remote device.
  process_impl->ReadMemory(kBlockAddress, kBlockSize + 2,
                           [](const zxdb::Err& err, zxdb::MemoryDump dump) {
                             ASSERT_EQ(dump.blocks()[0].data, std::vector<uint8_t>({
                                                                  104,
                                                                  105,
                                                                  106,
                                                                  107,
                                                                  108,
                                                                  109,
                                                                  110,
                                                                  111,
                                                                  112,
                                                                  113,
                                                              }));
                             ASSERT_TRUE(dump.blocks()[0].valid);
                           });

  // Read a subset of the block, which should not fall through.
  process_impl->ReadMemory(kBlockAddress + 2, kBlockSize - 4,
                           [](const zxdb::Err& err, zxdb::MemoryDump dump) {
                             ASSERT_EQ(dump.blocks()[0].data, std::vector<uint8_t>({
                                                                  2,
                                                                  3,
                                                                  4,
                                                                  5,
                                                              }));
                             ASSERT_TRUE(dump.blocks()[0].valid);
                           });

  // Read a subset from the invalid block, which should not fall through, and also should not return
  // any data in the blocks.
  process_impl->ReadMemory(kBlockAddress + 32, kBlockSize - 2,
                           [](const zxdb::Err& err, zxdb::MemoryDump dump) {
                             ASSERT_EQ(dump.blocks()[0].data.size(), 0ul);
                             ASSERT_FALSE(dump.blocks()[0].valid);
                           });

  loop().RunUntilNoTasks();
}

TEST_F(ProcessImplTest, ObserverOrder) {
  // An observer that requires the process to be created before any threads are created.
  class EnforceOrderObserver : public ProcessObserver, public ThreadObserver {
   public:
    void DidCreateProcess(Process* process, uint64_t timestamp) override {
      process_created = true;
      last_process = process;
    }
    void DidCreateThread(Thread* thread) override {
      // This is the core requirement: Process should be created before threads.
      EXPECT_TRUE(process_created);
      EXPECT_EQ(last_process, thread->GetProcess());
      thread_created = true;
      last_thread_koid = thread->GetKoid();
    }

    bool process_created = false;
    bool thread_created = false;
    Process* last_process = nullptr;
    uint64_t last_thread_koid = 0;
  };

  EnforceOrderObserver observer;
  session().process_observers().AddObserver(&observer);
  session().thread_observers().AddObserver(&observer);

  constexpr uint64_t kProcessKoid = 1234;
  constexpr uint64_t kThreadKoid = 5678;

  debug_ipc::ProcessRecord record;
  record.process_koid = kProcessKoid;
  record.process_name = "process";
  record.threads.resize(1);
  record.threads[0].id = {.process = kProcessKoid, .thread = kThreadKoid};

  TargetImpl* target = session().system().GetTargetImpls()[0];
  auto process = ProcessImpl::FromPreviousProcess(target, record);

  EXPECT_TRUE(observer.process_created);
  EXPECT_TRUE(observer.thread_created);

  EXPECT_EQ(process.get(), observer.last_process);
  EXPECT_EQ(kThreadKoid, observer.last_thread_koid);

  session().process_observers().RemoveObserver(&observer);
  session().thread_observers().RemoveObserver(&observer);
}

TEST_F(ProcessImplTest, AllThreadsStopped) {
  constexpr uint64_t kProcessKoid = 1234;
  constexpr uint64_t kThreadKoid1 = 5678;
  constexpr uint64_t kThreadKoid2 = 5679;

  Process* process = InjectProcess(kProcessKoid);
  ASSERT_TRUE(process);

  // Process with no threads should return false.
  EXPECT_FALSE(process->AllThreadsStopped());

  Thread* thread1 = InjectThread(kProcessKoid, kThreadKoid1);
  ASSERT_TRUE(thread1);

  // Single running thread should return false.
  EXPECT_FALSE(process->AllThreadsStopped());
  // Ignoring the only thread in the process leaves no stopped threads.
  EXPECT_FALSE(process->AllThreadsStopped(thread1));

  // Stop thread1 on an exception (supports frames).
  debug_ipc::NotifyException stop1;
  stop1.type = debug_ipc::ExceptionType::kSingleStep;
  stop1.thread.id = {.process = kProcessKoid, .thread = kThreadKoid1};
  stop1.thread.state = debug_ipc::ThreadRecord::State::kBlocked;
  stop1.thread.blocked_reason = debug_ipc::ThreadRecord::BlockedReason::kException;
  InjectExceptionWithStack(stop1, {}, true);

  EXPECT_TRUE(process->AllThreadsStopped());
  EXPECT_FALSE(process->AllThreadsStopped(thread1));

  // Add a second running thread.
  Thread* thread2 = InjectThread(kProcessKoid, kThreadKoid2);
  ASSERT_TRUE(thread2);

  // Not all threads are stopped since thread2 is running.
  EXPECT_FALSE(process->AllThreadsStopped());
  // Ignoring running thread2 means all remaining threads (thread1) are stopped.
  EXPECT_TRUE(process->AllThreadsStopped(thread2));
  // Ignoring stopped thread1 leaves running thread2, so returns false.
  EXPECT_FALSE(process->AllThreadsStopped(thread1));

  // Put thread2 into a blocked state that does NOT support frames (e.g., sleeping).
  debug_ipc::NotifyException sleep2;
  sleep2.type = debug_ipc::ExceptionType::kNone;
  sleep2.thread.id = {.process = kProcessKoid, .thread = kThreadKoid2};
  sleep2.thread.state = debug_ipc::ThreadRecord::State::kBlocked;
  sleep2.thread.blocked_reason = debug_ipc::ThreadRecord::BlockedReason::kSleeping;
  InjectExceptionWithStack(sleep2, {}, true);

  EXPECT_FALSE(process->AllThreadsStopped());
  EXPECT_TRUE(process->AllThreadsStopped(thread2));

  // Stop thread2 in a state that supports frames.
  debug_ipc::NotifyException stop2;
  stop2.type = debug_ipc::ExceptionType::kSingleStep;
  stop2.thread.id = {.process = kProcessKoid, .thread = kThreadKoid2};
  stop2.thread.state = debug_ipc::ThreadRecord::State::kBlocked;
  stop2.thread.blocked_reason = debug_ipc::ThreadRecord::BlockedReason::kException;
  InjectExceptionWithStack(stop2, {}, true);

  EXPECT_TRUE(process->AllThreadsStopped());
  EXPECT_TRUE(process->AllThreadsStopped(thread1));
  EXPECT_TRUE(process->AllThreadsStopped(thread2));
}

TEST_F(ProcessImplTest, PauseLoadsModulesWhenEmpty) {
  constexpr uint64_t kProcessKoid = 1234;
  constexpr uint64_t kThreadKoid = 5678;
  constexpr char kBuildId[] = "abcd";
  constexpr uint64_t kModuleBase = 0x1000;
  constexpr uint64_t kFrameIp = 0x1020;

  Process* process = InjectProcess(kProcessKoid);
  ASSERT_TRUE(process);
  ASSERT_TRUE(process->GetThreads().empty());

  auto module_symbols = fxl::MakeRefCounted<MockModuleSymbols>("test_module.so", kBuildId, true);
  auto fn = fxl::MakeRefCounted<Function>(DwarfTag::kSubprogram);
  fn->set_assigned_name("DoLoop");
  module_symbols->AddSymbolLocations(
      kFrameIp, {Location(kFrameIp, FileLine("loop.cc", 10), 0, SymbolContext(kModuleBase), fn)});
  session().system().GetSymbols()->InjectModuleForTesting(kBuildId, module_symbols.get());

  ASSERT_TRUE(process->GetSymbols()->GetStatus().empty());
  EXPECT_EQ(0, sink()->modules_count());

  debug_ipc::ModulesReply modules_reply;
  modules_reply.modules.resize(1);
  modules_reply.modules[0].name = "test_module.so";
  modules_reply.modules[0].base = kModuleBase;
  modules_reply.modules[0].build_id = kBuildId;
  sink()->set_modules_reply(modules_reply);

  debug_ipc::PauseReply pause_reply;
  pause_reply.threads.resize(1);
  pause_reply.threads[0].id = {.process = kProcessKoid, .thread = kThreadKoid};
  pause_reply.threads[0].state = debug_ipc::ThreadRecord::State::kSuspended;
  pause_reply.threads[0].stack_amount = debug_ipc::ThreadRecord::StackAmount::kMinimal;
  pause_reply.threads[0].frames.emplace_back(kFrameIp, 0x2000);
  sink()->set_pause_reply(pause_reply);

  bool paused = false;
  size_t module_count_on_pause = 0;
  bool top_frame_symbolized_on_pause = false;
  process->Pause([&paused, &module_count_on_pause, &top_frame_symbolized_on_pause, process]() {
    paused = true;
    module_count_on_pause = process->GetSymbols()->GetStatus().size();
    if (Thread* thread = process->GetThreadFromKoid(kThreadKoid);
        thread && !thread->GetStack().empty()) {
      top_frame_symbolized_on_pause = thread->GetStack()[0]->GetLocation().is_symbolized();
    }
  });

  EXPECT_FALSE(paused);
  loop().RunUntilNoTasks();

  EXPECT_TRUE(paused);
  EXPECT_EQ(1u, module_count_on_pause);
  EXPECT_TRUE(top_frame_symbolized_on_pause);
  EXPECT_EQ(1, sink()->modules_count());
  Thread* thread = process->GetThreadFromKoid(kThreadKoid);
  ASSERT_TRUE(thread);
  EXPECT_EQ(debug_ipc::ThreadRecord::State::kSuspended, thread->GetState());
  ASSERT_FALSE(thread->GetStack().empty());
  EXPECT_EQ("loop.cc", thread->GetStack()[0]->GetLocation().file_line().file());
  EXPECT_EQ(10, thread->GetStack()[0]->GetLocation().file_line().line());

  auto status = process->GetSymbols()->GetStatus();
  ASSERT_EQ(1u, status.size());
  EXPECT_EQ("test_module.so", status[0].name);
  EXPECT_EQ(kModuleBase, status[0].base);
  EXPECT_EQ(kBuildId, status[0].build_id);

  // A subsequent Pause when modules are already loaded should not issue a duplicate Modules RPC.
  bool paused_again = false;
  process->Pause([&paused_again]() { paused_again = true; });
  loop().RunUntilNoTasks();

  EXPECT_TRUE(paused_again);
  EXPECT_EQ(1, sink()->modules_count());
}

TEST_F(ProcessImplTest, ReadBytesEmptyCacheReturnsError) {
  constexpr uint64_t kProcessKoid = 1234;
  Process* process = InjectProcess(kProcessKoid);
  ASSERT_TRUE(process);
  ProcessImpl* process_impl = session().system().ProcessImplFromKoid(process->GetKoid());
  ASSERT_TRUE(process_impl);

  uint64_t value = 0xdeadbeefULL;
  unwinder::Error err = process_impl->ReadBytes(0x1000, sizeof(value), &value);
  EXPECT_TRUE(err.has_err());
  EXPECT_EQ(0xdeadbeefULL, value);
}

TEST_F(ProcessImplTest, FetchMemoryRangesSkipsCachedMultiFrameUnwind) {
  constexpr uint64_t kProcessKoid = 1234;
  constexpr uint64_t kStackBase = 0x100000;
  constexpr uint32_t kInitialPrefetchSize = 8192;
  constexpr size_t kNumFrames = 15;
  constexpr size_t kRegsPerFrame = 4;
  constexpr uint64_t kFrameStride = 64;

  Process* process = InjectProcess(kProcessKoid);
  ASSERT_TRUE(process);
  ProcessImpl* process_impl = session().system().ProcessImplFromKoid(process->GetKoid());
  ASSERT_TRUE(process_impl);

  // Populate an 8 KiB stack buffer with distinct 64-bit register values for 15 frames.
  std::vector<uint8_t> stack_data(kInitialPrefetchSize, 0);
  for (size_t frame = 0; frame < kNumFrames; ++frame) {
    for (size_t reg = 0; reg < kRegsPerFrame; ++reg) {
      uint64_t expected_val = 0xabcd0000ULL + (frame << 8U) + reg;
      size_t offset = (frame * kFrameStride) + (reg * sizeof(uint64_t));
      memcpy(&stack_data[offset], &expected_val, sizeof(expected_val));
    }
  }
  sink()->AddMemory(kStackBase, std::move(stack_data));
  sink()->ResetReadMemoryMetrics();

  // Perform the initial 8 KiB stack prefetch that AsyncUnwindLoop::Unwind issues at frame 0.
  bool prefetch_done = false;
  process_impl->FetchMemoryRanges({{kStackBase, kInitialPrefetchSize}},
                                  [&prefetch_done]() { prefetch_done = true; });
  loop().RunUntilNoTasks();
  EXPECT_TRUE(prefetch_done);
  EXPECT_EQ(1UL, sink()->read_memory_count());
  EXPECT_EQ(kInitialPrefetchSize, sink()->read_memory_total_bytes());

  // Simulate 15 frames of CfiParser::AsyncStep fetching 4 saved 8-byte registers per frame.
  // Because all 60 register slots fall inside the cached 8 KiB prefetch, zero additional
  // ReadMemory RPCs should be sent over the wire.
  for (size_t frame = 0; frame < kNumFrames; ++frame) {
    uint64_t frame_base = kStackBase + (frame * kFrameStride);
    std::vector<std::pair<uint64_t, uint32_t>> frame_ranges;
    frame_ranges.reserve(kRegsPerFrame);
    for (size_t reg = 0; reg < kRegsPerFrame; ++reg) {
      frame_ranges.emplace_back(frame_base + (reg * sizeof(uint64_t)), sizeof(uint64_t));
    }

    bool step_done = false;
    process_impl->FetchMemoryRanges(std::move(frame_ranges), [&step_done]() { step_done = true; });
    loop().RunUntilNoTasks();
    EXPECT_TRUE(step_done);

    for (size_t reg = 0; reg < kRegsPerFrame; ++reg) {
      uint64_t actual_val = 0;
      unwinder::Error err = process_impl->ReadBytes(frame_base + (reg * sizeof(uint64_t)),
                                                    sizeof(actual_val), &actual_val);
      ASSERT_TRUE(err.ok()) << err.msg();
      EXPECT_EQ(0xabcd0000ULL + (frame << 8U) + reg, actual_val);
    }
  }

  EXPECT_EQ(1UL, sink()->read_memory_count());
  EXPECT_EQ(kInitialPrefetchSize, sink()->read_memory_total_bytes());
}

TEST_F(ProcessImplTest, FetchMemoryRangesCoalescesStarnixRestrictedRegisters) {
  constexpr uint64_t kProcessKoid = 1234;
  constexpr uint64_t kRestrictedStateAddr = 0x500000;
  constexpr size_t kNumRegisters = 33;  // x0..x30, sp, pc in ARM64 restricted_return_loop
  constexpr uint32_t kRegSize = 8;

  Process* process = InjectProcess(kProcessKoid);
  ASSERT_TRUE(process);
  ProcessImpl* process_impl = session().system().ProcessImplFromKoid(process->GetKoid());
  ASSERT_TRUE(process_impl);

  std::vector<uint8_t> state_bytes(kNumRegisters * kRegSize, 0);
  for (size_t i = 0; i < kNumRegisters; ++i) {
    uint64_t val = 0x90000000ULL + i;
    memcpy(&state_bytes[i * kRegSize], &val, sizeof(val));
  }
  sink()->AddMemory(kRestrictedStateAddr, std::move(state_bytes));
  sink()->ResetReadMemoryMetrics();

  // Construct 33 already-sorted adjacent 8-byte ranges, matching CfiParser::AsyncStep iterating
  // over std::map<RegisterID, RegisterLocation> for restricted_return_loop.
  std::vector<std::pair<uint64_t, uint32_t>> ranges;
  ranges.reserve(kNumRegisters);
  for (size_t i = 0; i < kNumRegisters; ++i) {
    ranges.emplace_back(kRestrictedStateAddr + (i * kRegSize), kRegSize);
  }

  bool done = false;
  process_impl->FetchMemoryRanges(std::move(ranges), [&done]() { done = true; });
  loop().RunUntilNoTasks();
  EXPECT_TRUE(done);

  // All 33 adjacent 8-byte ranges must coalesce into a single 264-byte ReadMemory request.
  ASSERT_EQ(1UL, sink()->read_memory_count());
  EXPECT_EQ(kRestrictedStateAddr, sink()->read_memory_requests()[0].address);
  EXPECT_EQ(kNumRegisters * kRegSize, sink()->read_memory_requests()[0].size);
  EXPECT_EQ(kNumRegisters * kRegSize, sink()->read_memory_total_bytes());

  for (size_t i = 0; i < kNumRegisters; ++i) {
    uint64_t val = 0;
    ASSERT_TRUE(
        process_impl->ReadBytes(kRestrictedStateAddr + (i * kRegSize), kRegSize, &val).ok());
    EXPECT_EQ(0x90000000ULL + i, val);
  }
}

TEST_F(ProcessImplTest, FetchMemoryRangesCoalescesOverlappingAndInvalidatesOnContinue) {
  constexpr uint64_t kProcessKoid = 1234;
  constexpr uint64_t kThreadKoid = 5678;

  Process* process = InjectProcess(kProcessKoid);
  ASSERT_TRUE(process);
  Thread* thread = InjectThread(kProcessKoid, kThreadKoid);
  ASSERT_TRUE(thread);
  ProcessImpl* process_impl = session().system().ProcessImplFromKoid(process->GetKoid());
  ASSERT_TRUE(process_impl);

  sink()->AddMemory(0x2000, std::vector<uint8_t>(32, 0x11));
  sink()->AddMemory(0x4000, std::vector<uint8_t>(16, 0x22));
  sink()->ResetReadMemoryMetrics();

  // Pass unsorted, overlapping, adjacent, zero-length, and overflowing ranges across two disjoint
  // regions.
  std::vector<std::pair<uint64_t, uint32_t>> ranges = {
      {0x4008, 8}, {0x2008, 16}, {0x2000, 0}, {std::numeric_limits<uint64_t>::max() - 3, 8},
      {0x4000, 8}, {0x2000, 12}, {0x2018, 8},
  };

  bool done = false;
  process_impl->FetchMemoryRanges(std::move(ranges), [&done]() { done = true; });
  loop().RunUntilNoTasks();
  EXPECT_TRUE(done);

  ASSERT_EQ(2UL, sink()->read_memory_count());
  EXPECT_EQ(0x2000UL, sink()->read_memory_requests()[0].address);
  EXPECT_EQ(32U, sink()->read_memory_requests()[0].size);
  EXPECT_EQ(0x4000UL, sink()->read_memory_requests()[1].address);
  EXPECT_EQ(16U, sink()->read_memory_requests()[1].size);

  uint64_t val = 0;
  EXPECT_TRUE(process_impl->ReadBytes(0x2010, sizeof(val), &val).ok());
  EXPECT_EQ(0x1111111111111111ULL, val);

  // Continuing the process must invalidate the unwinder memory cache.
  process->Continue(false);
  loop().RunUntilNoTasks();

  EXPECT_TRUE(process_impl->ReadBytes(0x2010, sizeof(val), &val).has_err());
}

}  // namespace

}  // namespace zxdb
