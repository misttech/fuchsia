// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_PHYS_INCLUDE_PHYS_SYMBOLIZE_H_
#define ZIRCON_KERNEL_PHYS_INCLUDE_PHYS_SYMBOLIZE_H_

#include <lib/arch/backtrace.h>
#include <lib/elfldltl/note.h>
#include <lib/elfldltl/preallocated-vector.h>
#include <lib/symbolizer-markup/writer.h>
#include <stdint.h>
#include <stdio.h>
#include <zircon/assert.h>

#include <ktl/algorithm.h>
#include <ktl/byte.h>
#include <ktl/declval.h>
#include <ktl/optional.h>
#include <ktl/span.h>
#include <ktl/string_view.h>
#include <phys/main.h>
#include <phys/stack.h>
#include <phys/stdio.h>

// FileMarkupWriter{{stdout}} et al can be used with SymbolizerContext et al.
using FileMarkupWriter = symbolizer_markup::Writer<CallableFile>;

class ElfImage;
struct PhysExceptionState;
class Symbolize;

// The Symbolize instance registered by MainSymbolize.
extern class Symbolize* gSymbolize;

class Symbolize {
 public:
  template <class BootStackType>
  struct Stack {
    BootStackType& boot_stack;
    std::string_view name;
  };

  struct IsOnStackFunction {
    bool operator()(const void* ptr) const {
      return gSymbolize && gSymbolize->IsOnStack(reinterpret_cast<uintptr_t>(ptr));
    }
  };
  using FramePointerBacktrace = arch::FramePointerBacktrace<IsOnStackFunction>;

  using ModuleList = elfldltl::PreallocatedVector<const ElfImage*>;

  Symbolize() = delete;
  Symbolize(const Symbolize&) = delete;

  explicit Symbolize(const char* name, FILE* f = stdout)
      : name_{name}, output_{f}, writer_{{output_}} {}

  const char* name() const { return name_; }

  auto modules() const { return modules_.as_span(); }

  // The running phys executable's own image.
  const ElfImage& self() const { return *self_; }

  // Return the ELF build ID note for the currently executing module.
  elfldltl::ElfNote build_id() const;

  void set_stacks(ktl::span<const Stack<BootStack>> stacks) { stacks_ = stacks; }

  void set_shadow_call_stacks(ktl::span<const Stack<BootShadowCallStack>> stacks) {
    shadow_call_stacks_ = stacks;
  }

  bool IsOnStack(uintptr_t sp) const;

  arch::ShadowCallStackBacktrace GetShadowCallStackBacktrace(
      uintptr_t scsp = arch::GetShadowCallStackPointer()) const;

  FramePointerBacktrace GetFramePointerBacktrace(
      const arch::CallFrame* fp = static_cast<arch::CallFrame*>(__builtin_frame_address(0))) const {
    return FramePointerBacktrace::BackTrace(fp);
  }

  // Print the contextual markup elements describing each loaded module.
  void ContextAlways(FILE* log = nullptr);

  // Same, but idempotent: the first call prints and others do nothing.
  void Context();

  // Print one module's context if Context() has already printed.
  void ModuleContext(const ElfImage& loaded, unsigned int module_id, bool physical = false);

  // Adds the new module to the list.  If Context() has run, then emit context
  // for the new module.
  void OnLoad(const ElfImage& loaded);

  // Print the presentation markup element for one frame of a backtrace.
  void BackTraceFrame(unsigned int n, uintptr_t pc, bool interrupt = false);

  // Print a backtrace, ensuring context has been printed beforehand.
  // This takes any container of uintptr_t, so FramePointer works.
  template <typename T>
  PHYS_SINGLETHREAD void BackTrace(const T& pcs, unsigned int n, unsigned int max) {
    Context();
    for (uintptr_t pc : pcs) {
      if (max != 0 && n >= max) {
        writer_.Prefix(name_)
            .Literal("Backtrace truncated before frame #")
            .DecimalDigits(n)
            .Newline();
        break;
      }
      BackTraceFrame(n++, pc);
    }
  }

  // Print both flavors of backtrace together.  If the optional interrupt_pc
  // argument is supplied, then it's inserted as frame 0 and marked as exact PC
  // (whereas all frame-pointer and shadow-call-stack frames are marked as RA).
  PHYS_SINGLETHREAD void PrintBacktraces(const FramePointerBacktrace& frame_pointers,
                                         const arch::ShadowCallStackBacktrace& shadow_call_stack,
                                         ktl::optional<uintptr_t> interrupt_pc = ktl::nullopt);

  // Print the trigger markup element for a dumpfile.
  void DumpFile(ktl::string_view announce, size_t size_bytes, ktl::string_view sink_name,
                ktl::string_view vmo_name, ktl::string_view vmo_name_suffix = "");

  // Dump some stack up to the SP.
  PHYS_SINGLETHREAD void PrintStack(uintptr_t sp,
                                    ktl::optional<size_t> max_size_bytes = ktl::nullopt);

  // Print out register values.
  PHYS_SINGLETHREAD void PrintRegisters(const PhysExceptionState& regs);

  // Print out useful details at an exception.
  PHYS_SINGLETHREAD void PrintException(uint64_t vector, const char* vector_name,
                                        const PhysExceptionState& regs);

  // The arguments must be a subrange of the self image's final segment's
  // runtime vaddr bounds (i.e. a range of data + bss addresses).  That tail
  // should no longer be considered part of the segment's or module's bounds.
  void PruneFromSelf(uint64_t start, uint64_t end);

 protected:
  // This reads the existing modules to the new storage and then
  // replaces it as the storage to be used by future OnLoad calls.
  void ReplaceModulesStorage(ModuleList modules);

  const ElfImage* self_ = nullptr;

 private:
  [[gnu::format(printf, 2, 3)]] void Printf(const char* fmt, ...);

  void AddModule(const ElfImage* module);

  const char* name_;
  FILE* output_;
  ModuleList modules_;
  ktl::span<const Stack<BootStack>> stacks_;
  ktl::span<const Stack<BootShadowCallStack>> shadow_call_stacks_;
  FileMarkupWriter writer_;
  bool context_done_ = false;
};

// MainSymbolize represents the singleton Symbolize instance to be used by the
// current program. On construction, it regsters itself as `gSymbolize` and
// emits symbolization markup context.
class MainSymbolize : public Symbolize {
 public:
  explicit MainSymbolize(const char* name, FILE* f = stdout);

  // Registers the running executable's own image as the first module.
  void set_self(const ElfImage& self);

  // Provide storage for the module list so that additional modules (e.g. the
  // kernel) can be loaded and symbolized.
  void EnableModuleLoading(ModuleList modules) { ReplaceModulesStorage(ktl::move(modules)); }
};

#endif  // ZIRCON_KERNEL_PHYS_INCLUDE_PHYS_SYMBOLIZE_H_
