// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/bringup/bin/critical-services/storage-watchdog/wedge_dump.h"

#include <elf-search.h>
#include <inttypes.h>
#include <lib/zx/clock.h>
#include <lib/zx/process.h>
#include <lib/zx/suspend_token.h>
#include <lib/zx/thread.h>
#include <stdio.h>
#include <zircon/process.h>
#include <zircon/status.h>
#include <zircon/syscalls.h>
#include <zircon/syscalls/debug.h>
#include <zircon/syscalls/object.h>

#include <algorithm>
#include <atomic>
#include <condition_variable>
#include <cstring>
#include <functional>
#include <memory>
#include <mutex>
#include <optional>
#include <print>
#include <span>
#include <string>
#include <thread>
#include <unordered_set>
#include <vector>

#include <task-utils/walker.h>

#include "src/bringup/bin/critical-services/storage-watchdog/archivist_snapshot.h"
#include "src/bringup/bin/critical-services/storage-watchdog/diagnostics_text.h"

// Zircon C headers define ZX_RIGHT_*, ZX_INFO_*, ZX_VM_*, and ZX_THREAD_* with signed (1 << N).
// NOLINTBEGIN(bugprone-signed-bitwise)
namespace storage_watchdog {
namespace {

constexpr char kTag[] = "STORAGE WATCHDOG";
constexpr zx::duration kDeadline = zx::sec(60);
constexpr zx::duration kSuspendTimeout = zx::msec(100);
constexpr zx::duration kModuleTimeout = zx::sec(2);
constexpr size_t kMaxFrames = 48;
constexpr size_t kMaxStarnixThreads = 64;
// stdout goes to the debuglog, one record (40 byte header + text) per line. The crashlog keeps
// only the newest 128 KiB of the debuglog, so the dump must stay well below that, or its end
// would evict its start. Stacks come first and stop at kMaxStackDebuglogBytes. Logs and Inspect
// get what's left of kMaxDebuglogBytes, which leaves room for what the kernel and other
// processes log until the reboot.
constexpr size_t kMaxStackDebuglogBytes = 80ULL * 1024;
constexpr size_t kMaxDebuglogBytes = 112ULL * 1024;
constexpr zx::duration kArchivistTimeout = zx::sec(10);

size_t debuglog_used = 0;

void Print(std::string_view text) {
  fwrite(text.data(), 1, text.size(), stdout);
  debuglog_used += text.size() + (kDebuglogRecordHeader * std::ranges::count(text, '\n'));
}

template <typename T>
std::vector<T> GetInfo(const zx::process& process, uint32_t topic) {
  std::vector<T> items;
  size_t actual = 0, avail = 0;
  for (int attempt = 0; attempt < 3; attempt++) {
    if (process.get_info(topic, items.data(), items.size() * sizeof(T), &actual, &avail) != ZX_OK) {
      return {};
    }
    if (actual == avail) {
      break;
    }
    items.resize(avail + 8);
  }
  items.resize(actual);
  return items;
}

std::string GetName(zx_handle_t handle) {
  char name[ZX_MAX_NAME_LEN] = {};
  zx_object_get_property(handle, ZX_PROP_NAME, name, sizeof(name));
  return name;
}

const char* StateName(zx_thread_state_t state) {
  switch (state) {
    case ZX_THREAD_STATE_RUNNING:
      return "running";
    case ZX_THREAD_STATE_SUSPENDED:
      return "suspended";
    case ZX_THREAD_STATE_BLOCKED_EXCEPTION:
      return "exception";
    case ZX_THREAD_STATE_BLOCKED_SLEEPING:
      return "sleeping";
    case ZX_THREAD_STATE_BLOCKED_FUTEX:
      return "futex";
    case ZX_THREAD_STATE_BLOCKED_PORT:
      return "port";
    case ZX_THREAD_STATE_BLOCKED_CHANNEL:
      return "channel";
    case ZX_THREAD_STATE_BLOCKED_WAIT_ONE:
    case ZX_THREAD_STATE_BLOCKED_WAIT_MANY:
      return "wait";
    case ZX_THREAD_STATE_BLOCKED_INTERRUPT:
      return "interrupt";
    case ZX_THREAD_STATE_BLOCKED_PAGER:
      return "pager";
    default:
      return "other";
  }
}

struct Thread {
  zx_koid_t koid;
  zx::thread handle;
  std::string name;
  zx_thread_state_t state;
};

struct Process {
  zx_koid_t koid;
  zx::process handle;
  std::string name;
  std::vector<Thread> threads;
  // Whether it runs code from a package blob rather than bootfs.
  bool pager_backed_code = false;
};

// Picks the threads worth a backtrace: all of fxfs, any thread blocked on a pager, and the non-idle
// threads of driver hosts (incl. the storage drivers) and Starnix.
class ThreadPicker final : public TaskEnumerator {
 public:
  std::vector<Process> TakeProcesses() { return std::move(processes_); }

  zx_status_t OnProcess(int, zx_handle_t process, zx_koid_t koid, zx_koid_t) override {
    // Never suspend our own threads: one of them might hold a lock that the dump needs.
    if (koid == self_koid_) {
      return ZX_OK;
    }
    Process& entry = processes_.emplace_back(Process{.koid = koid, .name = GetName(process)});
    zx_handle_duplicate(process, ZX_RIGHT_SAME_RIGHTS, entry.handle.reset_and_get_address());
    return ZX_OK;
  }

  zx_status_t OnThread(int, zx_handle_t thread, zx_koid_t koid, zx_koid_t parent) override {
    if (processes_.empty() || processes_.back().koid != parent) {
      return ZX_OK;
    }
    Process& process = processes_.back();
    zx_info_thread_t info = {};
    zx_object_get_info(thread, ZX_INFO_THREAD, &info, sizeof(info), nullptr, nullptr);
    std::string name = GetName(thread);
    auto has = [](std::string_view s, std::string_view part) { return s.contains(part); };
    const zx_thread_state_t state = info.state;
    const bool busy = state != ZX_THREAD_STATE_BLOCKED_PORT;
    const bool picked =
        state == ZX_THREAD_STATE_BLOCKED_PAGER || has(process.name, "fxfs") ||
        (busy && (has(process.name, "driver_host") || has(process.name, "starnix_runner"))) ||
        (has(process.name, "starnix_kernel") &&
         (has(name, "crypt") || (busy && state != ZX_THREAD_STATE_BLOCKED_FUTEX &&
                                 process.threads.size() < kMaxStarnixThreads)));
    if (picked) {
      Thread& entry = process.threads.emplace_back(
          Thread{.koid = koid, .name = std::move(name), .state = state});
      zx_handle_duplicate(thread, ZX_RIGHT_SAME_RIGHTS, entry.handle.reset_and_get_address());
    }
    return ZX_OK;
  }

 protected:
  bool has_on_process() const override { return true; }
  bool has_on_thread() const override { return true; }

 private:
  std::vector<Process> processes_;
  const zx_koid_t self_koid_ = [] {
    zx_info_handle_basic_t info = {};
    zx::process::self()->get_info(ZX_INFO_HANDLE_BASIC, &info, sizeof(info), nullptr, nullptr);
    return info.koid;
  }();
};

// A process's mappings, and which of them are pager-backed (package blobs and file mappings,
// whose pages may have to come from the stuck storage stack).
class Mappings {
 public:
  explicit Mappings(const zx::process& process)
      : maps_(GetInfo<zx_info_maps_t>(process, ZX_INFO_PROCESS_MAPS)) {
    for (const zx_info_vmo_t& vmo : GetInfo<zx_info_vmo_t>(process, ZX_INFO_PROCESS_VMOS)) {
      if (vmo.flags & ZX_INFO_VMO_PAGER_BACKED) {
        paged_vmos_.insert(vmo.koid);
      }
    }
  }

  // Whether any executable mapping is pager-backed, i.e. the process runs code from a package
  // blob rather than from bootfs.
  bool any_pager_backed_code() const {
    return std::ranges::any_of(maps_, [this](const zx_info_maps_t& map) {
      return map.type == ZX_INFO_MAPS_TYPE_MAPPING &&
             (map.u.mapping.mmu_flags & ZX_VM_PERM_EXECUTE) &&
             paged_vmos_.contains(map.u.mapping.vmo_koid);
    });
  }

  // The bounds of the writable, non-pager-backed mapping holding |addr|, which is what a stack or
  // shadow call stack is, or {0, 0}. Reading only from these never faults on a pager.
  std::pair<uint64_t, uint64_t> SafeRange(uint64_t addr) const {
    for (const zx_info_maps_t& map : maps_) {
      if (map.type == ZX_INFO_MAPS_TYPE_MAPPING && addr - map.base < map.size) {
        if ((map.u.mapping.mmu_flags & ZX_VM_PERM_WRITE) &&
            !paged_vmos_.contains(map.u.mapping.vmo_koid)) {
          return {map.base, map.base + map.size};
        }
        break;
      }
    }
    return {0, 0};
  }

  // For an executable, non-pager-backed (bootfs) mapping holding |pc|: the mapping of the same VMO
  // at offset 0, where that module's ELF headers are. Otherwise nullptr.
  const zx_info_maps_t* BootfsModuleHeaders(uint64_t pc) const {
    const zx_info_maps_t* code = nullptr;
    for (const zx_info_maps_t& map : maps_) {
      if (map.type == ZX_INFO_MAPS_TYPE_MAPPING && pc - map.base < map.size) {
        code = &map;
        break;
      }
    }
    if (!code || !(code->u.mapping.mmu_flags & ZX_VM_PERM_EXECUTE) ||
        paged_vmos_.contains(code->u.mapping.vmo_koid)) {
      return nullptr;
    }
    for (const zx_info_maps_t& map : maps_) {
      if (map.type == ZX_INFO_MAPS_TYPE_MAPPING &&
          map.u.mapping.vmo_koid == code->u.mapping.vmo_koid && map.u.mapping.vmo_offset == 0) {
        return &map;
      }
    }
    return nullptr;
  }

 private:
  std::vector<zx_info_maps_t> maps_;
  std::unordered_set<zx_koid_t> paged_vmos_;
};

// Returns the PC and return addresses of a stopped thread, read only from its shadow call stack
// or, failing that, its frame pointer chain. src/lib/unwinder isn't usable here: it needs the
// module memory of every PC (for CFI and to validate PCs), and package modules are pager-backed,
// so with storage stuck it would hang on the first frame in one.
std::vector<uint64_t> UnwindFromStack(const zx::process& process, const Mappings& maps,
                                      const zx_thread_state_general_regs_t& regs) {
#ifdef __aarch64__
  const uint64_t pc = regs.pc, sp = regs.sp, fp = regs.r[29], lr = regs.lr, scs = regs.r[18];
#elifdef __x86_64__
  const uint64_t pc = regs.rip, sp = regs.rsp, fp = regs.rbp, lr = 0, scs = 0;
#else
  const uint64_t pc = regs.pc, sp = 0, fp = 0, lr = 0, scs = 0;
#endif
  std::vector<uint64_t> return_addresses;
  // x18 points just past the newest entry, and the oldest entry is preceded by a zero.
  if (auto [start, end] = maps.SafeRange(scs - 8); end != 0 && scs % 8 == 0) {
    uint64_t entries[kMaxFrames];
    const uint64_t from = scs - std::min<uint64_t>(scs - start, sizeof(entries));
    size_t actual = 0;
    if (process.read_memory(from, entries, scs - from, &actual) == ZX_OK) {
      for (size_t i = actual / 8; i > 0 && entries[i - 1] != 0; i--) {
        return_addresses.push_back(entries[i - 1]);
      }
    }
  }
  if (return_addresses.empty()) {
    // Frame records are {previous fp, return address}, at increasing addresses.
    auto [start, end] = maps.SafeRange(sp);
    uint64_t frame = fp;
    while (return_addresses.size() < kMaxFrames && frame % 8 == 0 && frame >= start &&
           frame + 16 <= end) {
      uint64_t record[2];
      size_t actual = 0;
      if (process.read_memory(frame, record, sizeof(record), &actual) != ZX_OK ||
          actual != sizeof(record) || record[1] == 0) {
        break;
      }
      return_addresses.push_back(record[1]);
      if (record[0] <= frame) {
        break;
      }
      frame = record[0];
    }
  }

  std::vector<uint64_t> pcs = {pc};
  // A leaf function doesn't push the link register.
  if (lr != 0 && (return_addresses.empty() || return_addresses.front() != lr)) {
    pcs.push_back(lr);
  }
  pcs.insert(pcs.end(), return_addresses.begin(), return_addresses.end());
  return pcs;
}

// Suspends |thread| unless it's stopped in an exception, and unwinds it.
std::optional<std::vector<uint64_t>> SampleThread(const zx::process& process, const Mappings& maps,
                                                  const Thread& thread) {
  zx::suspend_token token;
  if (thread.state != ZX_THREAD_STATE_BLOCKED_EXCEPTION) {
    zx_signals_t observed = 0;
    if (thread.handle.suspend(&token) != ZX_OK ||
        thread.handle.wait_one(ZX_THREAD_SUSPENDED | ZX_THREAD_TERMINATED,
                               zx::deadline_after(kSuspendTimeout), &observed) != ZX_OK ||
        !(observed & ZX_THREAD_SUSPENDED)) {
      return std::nullopt;
    }
  }
  zx_thread_state_general_regs_t regs;
  if (thread.handle.read_state(ZX_THREAD_STATE_GENERAL_REGS, &regs, sizeof(regs)) != ZX_OK) {
    return std::nullopt;
  }
  return UnwindFromStack(process, maps, regs);
}

// Formats the {{{module}}} and {{{mmap}}} markup for one module loaded at |bias|.
std::string ModuleMarkup(unsigned int id, std::string_view name, std::span<const uint8_t> build_id,
                         uint64_t bias, std::span<const Elf64_Phdr> phdrs) {
  const uint64_t page_size = zx_system_get_page_size();
  char buf[160];
  std::string text = "{{{module:" + std::to_string(id) + ":" + std::string(name) + ":elf:";
  for (uint8_t byte : build_id) {
    snprintf(buf, sizeof(buf), "%02x", byte);
    text += buf;
  }
  text += "}}}\n";
  for (const Elf64_Phdr& phdr : phdrs) {
    if (phdr.p_type == PT_LOAD) {
      const uint64_t start = phdr.p_vaddr & -page_size;
      const uint64_t end = (phdr.p_vaddr + phdr.p_memsz + page_size - 1) & -page_size;
      snprintf(buf, sizeof(buf),
               "{{{mmap:0x%" PRIx64 ":0x%" PRIx64 ":load:%u:%s%s%s:0x%" PRIx64 "}}}\n",
               bias + start, end - start, id, (phdr.p_flags & PF_R) ? "r" : "",
               (phdr.p_flags & PF_W) ? "w" : "", (phdr.p_flags & PF_X) ? "x" : "", start);
      text += buf;
    }
  }
  return text;
}

// The module markup for |pcs| in a process whose code is all from bootfs. It reads ELF headers
// only from the bootfs mapping that holds them, so it never waits on a pager. elf_search would
// also read the start of every other read-only mapping, and fxfs maps pager-backed VMOs whose
// pager is the storage driver, so with storage stuck that blocks.
std::string BootfsModuleContext(const zx::process& process, const Mappings& maps,
                                std::span<const uint64_t> pcs) {
  constexpr size_t kMaxPhdrs = 32;
  constexpr size_t kMaxNotes = 1024;
  const uint64_t page_size = zx_system_get_page_size();
  std::string text;
  std::vector<zx_koid_t> seen;
  unsigned int id = 0;
  for (uint64_t pc : pcs) {
    const zx_info_maps_t* headers = maps.BootfsModuleHeaders(pc);
    if (!headers || std::ranges::contains(seen, headers->u.mapping.vmo_koid)) {
      continue;
    }
    seen.push_back(headers->u.mapping.vmo_koid);
    auto read = [&](uint64_t addr, void* buf, size_t size) {
      size_t actual = 0;
      return addr - headers->base < headers->size && size <= headers->base + headers->size - addr &&
             process.read_memory(addr, buf, size, &actual) == ZX_OK && actual == size;
    };
    Elf64_Ehdr ehdr;
    if (!read(headers->base, &ehdr, sizeof(ehdr)) || memcmp(ehdr.e_ident, ELFMAG, SELFMAG) != 0 ||
        ehdr.e_ident[EI_CLASS] != ELFCLASS64 || ehdr.e_phentsize != sizeof(Elf64_Phdr) ||
        ehdr.e_phnum == 0 || ehdr.e_phnum > kMaxPhdrs) {
      continue;
    }
    Elf64_Phdr phdr_buf[kMaxPhdrs];
    if (!read(headers->base + ehdr.e_phoff, phdr_buf, ehdr.e_phnum * sizeof(Elf64_Phdr))) {
      continue;
    }
    const std::span<const Elf64_Phdr> phdrs(phdr_buf, ehdr.e_phnum);
    auto first_load = std::ranges::find(phdrs, PT_LOAD, &Elf64_Phdr::p_type);
    if (first_load == phdrs.end()) {
      continue;
    }
    const uint64_t bias = headers->base - (first_load->p_vaddr & -page_size);
    std::vector<uint8_t> build_id;
    for (const Elf64_Phdr& phdr : phdrs) {
      uint8_t notes[kMaxNotes];
      if (phdr.p_type != PT_NOTE || phdr.p_filesz > sizeof(notes) ||
          !read(bias + phdr.p_vaddr, notes, phdr.p_filesz)) {
        continue;
      }
      for (size_t offset = 0; offset + sizeof(Elf64_Nhdr) <= phdr.p_filesz;) {
        Elf64_Nhdr nhdr;
        memcpy(&nhdr, notes + offset, sizeof(nhdr));
        const size_t name = offset + sizeof(nhdr);
        const size_t desc = name + ((nhdr.n_namesz + 3) & ~size_t{3});
        const size_t next = desc + ((nhdr.n_descsz + 3) & ~size_t{3});
        if (next > phdr.p_filesz) {
          break;
        }
        if (nhdr.n_type == NT_GNU_BUILD_ID && nhdr.n_namesz == sizeof("GNU") &&
            memcmp(notes + name, "GNU", sizeof("GNU")) == 0) {
          build_id.assign(notes + desc, notes + desc + nhdr.n_descsz);
        }
        offset = next;
      }
    }
    if (!build_id.empty()) {
      text += ModuleMarkup(id++, headers->name, build_id, bias, phdrs);
    }
  }
  return text;
}

// Formats the {{{module}}} and {{{mmap}}} markup for the modules that contain |pcs|. elf_search
// reads ELF headers from process memory, which for package modules is pager-backed, so this runs
// on a thread that's abandoned if it doesn't finish within kModuleTimeout.
std::optional<std::string> ModuleContext(const zx::process& process, std::vector<uint64_t> pcs) {
  struct Result {
    std::mutex mutex;
    std::condition_variable cv;
    std::optional<std::string> text;
  };
  auto result = std::make_shared<Result>();
  zx::process dup;
  if (process.duplicate(ZX_RIGHT_SAME_RIGHTS, &dup) != ZX_OK) {
    return std::nullopt;
  }
  std::thread([result, process = std::move(dup), pcs = std::move(pcs)] {
    std::string text;
    unsigned int id = 0;
    elf_search::ForEachModule(process, [&](const elf_search::ModuleInfo& info) {
      auto contains = [&info](uint64_t pc) {
        return std::ranges::any_of(info.phdrs, [&](const Elf64_Phdr& phdr) {
          return phdr.p_type == PT_LOAD && pc - (info.vaddr + phdr.p_vaddr) < phdr.p_memsz;
        });
      };
      if (std::ranges::none_of(pcs, contains)) {
        return;
      }
      text += ModuleMarkup(id++, info.name, info.build_id, info.vaddr, info.phdrs);
    });
    std::scoped_lock lock(result->mutex);
    result->text = std::move(text);
    result->cv.notify_all();
  }).detach();

  std::unique_lock lock(result->mutex);
  result->cv.wait_for(lock, std::chrono::nanoseconds(kModuleTimeout.get()),
                      [&] { return result->text.has_value(); });
  return result->text;
}

// Prints each distinct stack of |process|'s picked threads once, and appends its summary line to
// |summary|. Returns false if reading module headers timed out.
bool DumpProcess(const Process& process, bool skip_pager_backed_modules, std::string& summary) {
  const Mappings maps(process.handle);
  struct Stack {
    std::vector<uint64_t> pcs;
    size_t threads = 0;
    std::string owners;
  };
  std::vector<Stack> stacks;
  std::vector<uint64_t> all_pcs;
  size_t skipped = 0;
  for (const Thread& thread : process.threads) {
    std::optional pcs = SampleThread(process.handle, maps, thread);
    if (!pcs) {
      skipped++;
      continue;
    }
    auto it = std::ranges::find(stacks, *pcs, &Stack::pcs);
    if (it == stacks.end()) {
      all_pcs.insert(all_pcs.end(), pcs->begin(), pcs->end());
      it = stacks.insert(stacks.end(), Stack{.pcs = std::move(*pcs)});
    }
    // Keep the line within one debuglog record.
    if (++it->threads, it->owners.size() < 120) {
      char owner[64];
      snprintf(owner, sizeof(owner), " %s(%" PRIu64 ",%s)", thread.name.c_str(), thread.koid,
               StateName(thread.state));
      it->owners += owner;
    }
  }

  std::optional<std::string> modules;
  if (!stacks.empty() && !process.pager_backed_code) {
    modules = BootfsModuleContext(process.handle, maps, all_pcs);
  } else if (!stacks.empty() && !skip_pager_backed_modules) {
    modules = ModuleContext(process.handle, std::move(all_pcs));
  }

  std::string text = "{{{reset}}}\n";
  char line[256];
  snprintf(line, sizeof(line), "%s: %s (%" PRIu64 "): %zu threads, %zu skipped, %zu stacks%s\n",
           kTag, process.name.c_str(), process.koid, process.threads.size(), skipped, stacks.size(),
           modules ? "" : "; no module info (pager stuck?)");
  text += line;
  summary += line;
  if (modules) {
    text += *modules;
  }
  for (const Stack& stack : stacks) {
    snprintf(line, sizeof(line), "%s: %zu threads:%s\n", kTag, stack.threads, stack.owners.c_str());
    text += line;
    for (size_t i = 0; i < stack.pcs.size(); i++) {
      snprintf(line, sizeof(line), "{{{bt:%zu:0x%" PRIx64 ":%s}}}\n", i, stack.pcs[i],
               i == 0 ? "pc" : "ra");
      text += line;
    }
  }
  Print(text);
  return modules.has_value() || stacks.empty() || !process.pager_backed_code;
}

// Dies as a process critical to the root job, or kills the root job if that fails. Either way
// the kernel reboots with USERSPACE ROOT JOB TERMINATION.
[[noreturn]] void Reboot(const zx::job& root_job, const char* why) {
  static std::atomic_flag rebooting;
  if (!rebooting.test_and_set()) {
    std::println("{}: {}; terminating the root job", kTag, why);
    fflush(stdout);
    // The crashlog names the critical process, and feedback turns that into the crash signature.
    constexpr char kName[] = "storage-watchdog";
    zx::process::self()->set_property(ZX_PROP_NAME, kName, sizeof(kName) - 1);
    zx_status_t status = zx_job_set_critical(
        root_job.get(), ZX_JOB_CRITICAL_PROCESS_RETCODE_NONZERO, zx_process_self());
    if (status == ZX_OK) {
      zx_process_exit(1);
    }
    std::println("{}: zx_job_set_critical failed: {}; killing the root job", kTag,
                 zx_status_get_string(status));
    fflush(stdout);
    root_job.kill();
  }
  while (true) {
    zx::nanosleep(zx::time::infinite());
  }
}

}  // namespace

bool HasPagerBackedCode(const zx::process& process) {
  return Mappings(process).any_pager_backed_code();
}

void DumpWedgeDiagnosticsAndReboot(std::string_view reason, zx::job root_job) {
  const zx::time start = zx::clock::get_monotonic();
  auto job = std::make_shared<zx::job>(std::move(root_job));
  std::thread([job, deadline = start + kDeadline] {
    zx::nanosleep(deadline);
    Reboot(*job, "wedge dump deadline reached");
  }).detach();

  std::println("{}: ==== WEDGE DUMP ==== storage stall: {}", kTag, reason);
  ThreadPicker picker;
  picker.WalkJobTree(job->get());
  std::vector<Process> processes = picker.TakeProcesses();
  std::erase_if(processes, [](const Process& p) { return p.threads.empty(); });
  // Processes running bootfs code first: their module headers can always be read.
  for (Process& process : processes) {
    process.pager_backed_code = HasPagerBackedCode(process.handle);
  }
  std::ranges::stable_partition(processes, std::not_fn(&Process::pager_backed_code));

  bool pager_stuck = false;
  std::string summary;
  for (size_t i = 0; i < processes.size(); i++) {
    if (debuglog_used > kMaxStackDebuglogBytes) {
      std::println("{}: out of debuglog space, skipped {} processes", kTag, processes.size() - i);
      break;
    }
    pager_stuck |= !DumpProcess(processes[i], pager_stuck, summary);
  }

  // Then the log tail and storage Inspect data, as much as fits. Inspect is small, so it gets
  // room first, and the log tail drops its oldest lines to fit in the rest.
  ArchivistSnapshots snapshots = TakeArchivistSnapshots(zx::deadline_after(kArchivistTimeout));
  // Leave room for the two snapshot summary lines and the trailer below.
  const size_t reserved = (3 * (kMaxLineLength + kDebuglogRecordHeader)) + summary.size() +
                          (kDebuglogRecordHeader * std::ranges::count(summary, '\n'));
  size_t room = kMaxDebuglogBytes - std::min(kMaxDebuglogBytes, debuglog_used + reserved);
  std::span<const std::string> inspect = snapshots.inspect.lines;
  while (DebuglogCost(inspect) > room) {
    inspect = inspect.first(inspect.size() - 1);
  }
  room -= DebuglogCost(inspect);
  std::span<const std::string> logs = NewestWithinBudget(snapshots.logs.lines, room);
  for (auto [snapshot, lines] : {std::pair{&snapshots.logs, logs}, {&snapshots.inspect, inspect}}) {
    std::string text = std::string(kTag) + ": " + snapshot->summary + ", printing " +
                       std::to_string(lines.size()) + "\n";
    for (const std::string& line : lines) {
      text += line + "\n";
    }
    Print(text);
  }

  // The kernel prints a whole address space to the debuglog for every page fault that times out,
  // which can push the start of the dump out of the crashlog. So repeat the reason and the process
  // summaries at the end, where they're the last thing to go.
  Print(std::string(kTag) + ": ==== WEDGE DUMP END ==== storage stall: " + std::string(reason) +
        "\n" + summary);

  char why[64];
  snprintf(why, sizeof(why), "wedge dump done in %.1f s",
           static_cast<double>((zx::clock::get_monotonic() - start).to_msecs()) / 1000);
  Reboot(*job, why);
}

}  // namespace storage_watchdog
// NOLINTEND(bugprone-signed-bitwise)
