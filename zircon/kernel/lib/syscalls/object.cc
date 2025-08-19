// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/boot-options/boot-options.h>
#include <lib/heap.h>
#include <lib/kconcurrent/chainlock_transaction.h>
#include <lib/power-management/energy-model.h>
#include <lib/power-management/kernel-registry.h>
#include <lib/stall.h>
#include <lib/syscalls/forward.h>
#include <lib/zircon-internal/macros.h>
#include <lib/zx/result.h>
#include <platform.h>
#include <trace.h>
#include <zircon/errors.h>
#include <zircon/rights.h>
#include <zircon/syscalls-next.h>
#include <zircon/syscalls/iob.h>
#include <zircon/syscalls/object.h>
#include <zircon/syscalls/resource.h>
#include <zircon/time.h>
#include <zircon/types.h>

#include <cstdint>

#include <fbl/alloc_checker.h>
#include <fbl/array.h>
#include <fbl/ref_ptr.h>
#include <kernel/mp.h>
#include <kernel/scheduler.h>
#include <kernel/stats.h>
#include <ktl/algorithm.h>
#include <ktl/iterator.h>
#include <object/bus_transaction_initiator_dispatcher.h>
#include <object/clock_dispatcher.h>
#include <object/diagnostics.h>
#include <object/exception_dispatcher.h>
#include <object/handle.h>
#include <object/interrupt_dispatcher.h>
#include <object/io_buffer_dispatcher.h>
#include <object/job_dispatcher.h>
#include <object/msi_dispatcher.h>
#include <object/process_dispatcher.h>
#include <object/resource.h>
#include <object/resource_dispatcher.h>
#include <object/socket_dispatcher.h>
#include <object/stream_dispatcher.h>
#include <object/thread_dispatcher.h>
#include <object/timer_dispatcher.h>
#include <object/vcpu_dispatcher.h>
#include <object/vm_address_region_dispatcher.h>
#include <object/vm_object_dispatcher.h>
#include <vm/compression.h>
#include <vm/discardable_vmo_tracker.h>
#include <vm/memory_stats.h>
#include <vm/pmm.h>
#include <vm/vm.h>

#include <ktl/enforce.h>

#define LOCAL_TRACE 0

namespace {

// Gathers the koids of a job's descendants.
class SimpleJobEnumerator final : public JobEnumerator {
 public:
  // If |job| is true, only records job koids; otherwise, only
  // records process koids.
  SimpleJobEnumerator(user_out_ptr<zx_koid_t> ptr, size_t max, bool jobs)
      : jobs_(jobs), ptr_(ptr), max_(max) {}

  size_t get_avail() const { return avail_; }
  size_t get_count() const { return count_; }

 private:
  bool OnJob(JobDispatcher* job) override {
    if (!jobs_) {
      return true;
    }
    return RecordKoid(job->get_koid());
  }

  bool OnProcess(ProcessDispatcher* proc) override {
    if (jobs_) {
      return true;
    }
    // Hide any processes that are both still in the INITIAL state, and have a handle count of 0.
    // Such processes have not yet had their zx_process_create call complete yet, and making it
    // visible and allowing handles to be constructed via object_get_child, could spuriously destroy
    // it. Once a process either has a handle, or has left the initial state, handles can freely be
    // constructed since any additional on_zero_handles invocations will be idempotent.
    // TODO(https://fxbug.dev/42175105): Consider whether long term needing to allow multiple
    // on_zero_handles transitions is the correct strategy.
    if (proc->state() == ProcessDispatcher::State::INITIAL && Handle::Count(*proc) == 0) {
      return true;
    }
    return RecordKoid(proc->get_koid());
  }

  bool RecordKoid(zx_koid_t koid) {
    avail_++;
    if (count_ < max_) {
      // TODO: accumulate batches and do fewer user copies.
      if (ptr_.copy_array_to_user(&koid, 1, count_) != ZX_OK) {
        return false;
      }
      count_++;
    }
    return true;
  }

  const bool jobs_;
  const user_out_ptr<zx_koid_t> ptr_;
  const size_t max_;

  size_t count_ = 0;
  size_t avail_ = 0;
};

// Specialize the VmoInfoWriter to work for any T that is a subset of zx_info_vmo_t. This is
// currently true for v1 and v2 (v2 being the current version). Being a subset the full
// zx_info_vmo_t can just be casted and copied.
template <typename T>
class SubsetVmoInfoWriter : public VmoInfoWriter {
 public:
  SubsetVmoInfoWriter(user_out_ptr<T> out) : out_(out) {}
  ~SubsetVmoInfoWriter() = default;
  zx_status_t Write(const zx_info_vmo_t& vmo, size_t offset) override {
    T versioned_vmo = VmoInfoToVersion<T>(vmo);
    return out_.element_offset(offset + base_offset_).copy_to_user(versioned_vmo);
  }
  UserCopyCaptureFaultsResult WriteCaptureFaults(const zx_info_vmo_t& vmo, size_t offset) override {
    T versioned_vmo = VmoInfoToVersion<T>(vmo);
    return out_.element_offset(offset + base_offset_).copy_to_user_capture_faults(versioned_vmo);
  }
  void AddOffset(size_t offset) override { base_offset_ += offset; }

 private:
  static_assert(sizeof(T) <= sizeof(zx_info_vmo_t));
  user_out_ptr<T> out_;
  size_t base_offset_ = 0;
};

template <typename T>
class SubsetVmarMapsInfoWriter : public VmarMapsInfoWriter {
 public:
  SubsetVmarMapsInfoWriter(user_out_ptr<T> out) : out_(out) {}
  ~SubsetVmarMapsInfoWriter() = default;
  zx_status_t Write(const zx_info_maps_t& maps, size_t offset) override {
    T versioned_maps = MapsInfoToVersion<T>(maps);
    return out_.element_offset(offset + base_offset_).copy_to_user(versioned_maps);
  }
  UserCopyCaptureFaultsResult WriteCaptureFaults(const zx_info_maps_t& maps,
                                                 size_t offset) override {
    T versioned_maps = MapsInfoToVersion<T>(maps);
    return out_.element_offset(offset + base_offset_).copy_to_user_capture_faults(versioned_maps);
  }
  void AddOffset(size_t offset) override { base_offset_ += offset; }

 private:
  static_assert(sizeof(T) <= sizeof(zx_info_maps_t));
  user_out_ptr<T> out_;
  size_t base_offset_ = 0;
};

// TODO: figure out a better handle to hang this off to and push this copy code into
// that dispatcher.
zx_info_cpu_stats_t GetCPUStats(uint32_t cpu_num) {
  const auto* cpu = &percpu::Get(cpu_num);

  // copy the per cpu stats from the kernel percpu structure
  // NOTE: it's technically racy to read this without grabbing a lock
  // but since each field is wordwise any sane architecture will not
  // return a corrupted value.
  zx_info_cpu_stats_t stats = {};
  stats.cpu_number = cpu_num;
  stats.flags = mp_is_cpu_online(cpu_num) ? ZX_INFO_CPU_STATS_FLAG_ONLINE : 0;

  // account for idle time if a cpu is currently idle
  {
    const Thread& idle_power_thread = cpu->idle_power_thread.thread();
    SingleChainLockGuard guard{IrqSaveOption, idle_power_thread.get_lock(),
                               CLT_TAG("ZX_INFO_CPU_STATS idle time rollup")};
    zx_time_t idle_time = cpu->stats.idle_time;
    const bool is_idle = Scheduler::PeekIsIdle(cpu_num);
    if (is_idle) {
      zx_duration_mono_t recent_idle = zx_time_sub_time(
          current_mono_time(), idle_power_thread.scheduler_state().last_started_running());
      idle_time = zx_duration_add_duration(idle_time, recent_idle);
    }
    stats.idle_time = idle_time;
  }

  stats.reschedules = cpu->stats.reschedules;
  stats.context_switches = cpu->stats.context_switches;
  stats.irq_preempts = cpu->stats.irq_preempts;
  stats.preempts = cpu->stats.preempts;
  stats.yields = cpu->stats.yields;
  stats.ints = cpu->stats.interrupts;
  stats.timer_ints = cpu->stats.timer_ints;
  stats.timers = cpu->stats.timers;
  stats.page_faults = cpu->stats.page_faults;
  stats.exceptions = 0;  // deprecated, use "kcounter" command for now.
  stats.syscalls = cpu->stats.syscalls;
  stats.reschedule_ipis = cpu->stats.reschedule_ipis;
  stats.generic_ipis = cpu->stats.generic_ipis;

  return stats;
}

zx_info_guest_stats_t GetGuestCPUStats(uint32_t cpu_num) {
  const auto* cpu = &percpu::Get(cpu_num);
  zx_info_guest_stats_t stats = {};
  stats.cpu_number = cpu_num;
  stats.flags = mp_is_cpu_online(cpu_num) ? ZX_INFO_CPU_STATS_FLAG_ONLINE : 0;

  stats.vm_entries = cpu->gstats.vm_entries;
  stats.vm_exits = cpu->gstats.vm_exits;
#ifdef __aarch64__
  stats.wfi_wfe_instructions = cpu->gstats.wfi_wfe_instructions;
  stats.system_instructions = cpu->gstats.system_instructions;
  stats.instruction_aborts = cpu->gstats.instruction_aborts;
  stats.data_aborts = cpu->gstats.data_aborts;
  stats.smc_instructions = cpu->gstats.smc_instructions;
  stats.interrupts = cpu->gstats.interrupts;
#elif defined(__x86_64__)
  stats.vmcall_instructions = cpu->gstats.vmcall_instructions;
  stats.pause_instructions = cpu->gstats.pause_instructions;
  stats.xsetbv_instructions = cpu->gstats.xsetbv_instructions;
  stats.ept_violations = cpu->gstats.ept_violations;
  stats.wrmsr_instructions = cpu->gstats.wrmsr_instructions;
  stats.rdmsr_instructions = cpu->gstats.rdmsr_instructions;
  stats.io_instructions = cpu->gstats.io_instructions;
  stats.control_register_accesses = cpu->gstats.control_register_accesses;
  stats.hlt_instructions = cpu->gstats.hlt_instructions;
  stats.cpuid_instructions = cpu->gstats.cpuid_instructions;
  stats.interrupt_windows = cpu->gstats.interrupt_windows;
  stats.interrupts = cpu->gstats.interrupts;
#endif
  return stats;
}

zx::result<zx_info_process_handle_stats_t> GetHandleStats(ProcessDispatcher* process) {
  zx_info_process_handle_stats_t info = {};
  static_assert(ktl::size(info.handle_count) >= ZX_OBJ_TYPE_UPPER_BOUND,
                "Need room for each handle type.");

  process->handle_table().ForEachHandle(
      [&](zx_handle_t handle, zx_rights_t rights, const Dispatcher* dispatcher) {
        ++info.handle_count[dispatcher->get_type()];
        return ZX_OK;
      });
  return zx::ok(info);
}

zx::result<fbl::Array<zx_power_domain_info_t>> GetPowerDomainsInfo(size_t max_copy) {
  // Alternatively clamp `max_copy` to `arch_max_num_cpus()`.
  size_t power_domain_count = 0;
  power_management::KernelPowerDomainRegistry::Visit(
      [&power_domain_count](const auto& power_domain) { ++power_domain_count; });

  // Avoid arbitrary large buffers.
  max_copy = ktl::min(power_domain_count, max_copy);

  ktl::unique_ptr<zx_power_domain_info_t[]> entries = nullptr;
  if (max_copy > 0) {
    fbl::AllocChecker ac;
    entries = ktl::make_unique<zx_power_domain_info_t[]>(&ac, max_copy);
    if (!ac.check()) {
      return zx::error(ZX_ERR_NO_MEMORY);
    }
  }
  // Reset the count, in case we are racing against an update, so we can return somewhat
  // consistent `avail`.
  power_domain_count = 0;
  power_management::KernelPowerDomainRegistry::Visit(
      [&power_domain_count, &entries, max_copy](const power_management::PowerDomain& domain) {
        if (power_domain_count < max_copy) {
          zx_power_domain_info_t& entry = entries[power_domain_count];
          entry = {
              .cpus = domain.cpus(),
              .domain_id = domain.id(),
              .idle_power_levels = static_cast<uint8_t>(domain.model().idle_levels().size()),
              .active_power_levels = static_cast<uint8_t>(domain.model().active_levels().size()),
          };
        }
        power_domain_count++;
      });

  return zx::ok(fbl::Array{entries.release(), power_domain_count});
}

// Copies to usermode the actual (number of records written) and the avail (number of records
// available).
zx_status_t actual_avail_result(size_t actual, size_t avail, user_out_ptr<size_t> user_actual,
                                user_out_ptr<size_t> user_avail) {
  if (user_actual) {
    zx_status_t status = user_actual.copy_to_user(actual);
    if (status != ZX_OK)
      return status;
  }
  if (user_avail) {
    zx_status_t status = user_avail.copy_to_user(avail);
    if (status != ZX_OK)
      return status;
  }
  return ZX_OK;
}

// Copies a single record, |src_record|, into the user buffer |dst_buffer| of size
// |dst_buffer_size|. If the copy succeeds, the value 1 is copied into |user_avail|.
template <typename T>
zx_status_t single_record_result(user_out_ptr<void> dst_buffer, size_t dst_buffer_size,
                                 user_out_ptr<size_t> user_actual, user_out_ptr<size_t> user_avail,
                                 const T& src_record) {
  size_t actual = 1;
  if (dst_buffer_size >= sizeof(T)) {
    if (dst_buffer.reinterpret<T>().copy_to_user(src_record) != ZX_OK) {
      return ZX_ERR_INVALID_ARGS;
    }
  } else {
    actual = 0;
  }
  zx_status_t st = actual_avail_result(actual, 1, user_actual, user_avail);
  if (st != ZX_OK) {
    return st;
  }
  if (actual == 0) {
    return ZX_ERR_BUFFER_TOO_SMALL;
  }
  return ZX_OK;
}

// Copies to usermode an (fbl) array of results to |dst_buffer| up to |dst_buffer_size|. It uses
// actual_avail_result() to copy to usermode the available and actual copied records.
template <typename T>
zx_status_t multi_record_result(user_out_ptr<void> dst_buffer, size_t dst_buffer_size,
                                user_out_ptr<size_t> user_actual, user_out_ptr<size_t> user_avail,
                                const fbl::Array<T>& src_array) {
  size_t avail = src_array.size();
  size_t num_space_for = dst_buffer_size / sizeof(T);
  size_t actual = ktl::min(avail, num_space_for);
  // Don't try to copy if there are no bytes to copy, as the "is
  // user space" check may not handle (_buffer == NULL and len == 0).
  if (actual && dst_buffer.reinterpret<T>().copy_array_to_user(src_array.data(), actual) != ZX_OK) {
    return ZX_ERR_INVALID_ARGS;
  }
  return actual_avail_result(actual, avail, user_actual, user_avail);
}

// Base case for converting a struct from current version to older version based on the |topic|.
// in this base case there is no older version so the conversion is no-op.
template <int topic>
inline auto ConvertInfoVersion(const auto& info) {
  return info;
}

// The following are specializations of ConvertInfoVersion.
template <>
inline auto ConvertInfoVersion<ZX_INFO_TASK_RUNTIME_V1>(const zx_info_task_runtime_t& info) {
  zx_info_task_runtime_v1_t info_v1 = {
      .cpu_time = info.cpu_time,
      .queue_time = info.queue_time,
  };
  return info_v1;
}

template <>
auto ConvertInfoVersion<ZX_INFO_VMO_V1>(const zx_info_vmo_t& info) {
  return VmoInfoToVersion<zx_info_vmo_v1_t>(info);
}

template <>
auto ConvertInfoVersion<ZX_INFO_VMO_V2>(const zx_info_vmo_t& info) {
  return VmoInfoToVersion<zx_info_vmo_v2_t>(info);
}

template <>
auto ConvertInfoVersion<ZX_INFO_VMO_V3>(const zx_info_vmo_t& info) {
  return VmoInfoToVersion<zx_info_vmo_v3_t>(info);
}

template <>
auto ConvertInfoVersion<ZX_INFO_KMEM_STATS_V1>(const zx_info_kmem_stats_t& info) {
  return KernelStatsInfoToVersion<zx_info_kmem_stats_v1_t>(info);
}

template <>
auto ConvertInfoVersion<ZX_INFO_KMEM_STATS_EXTENDED>(const zx_info_kmem_stats_t& info) {
  return KernelStatsInfoToVersion<zx_info_kmem_stats_extended_t>(info);
}

template <>
auto ConvertInfoVersion<ZX_INFO_THREAD_EXCEPTION_REPORT_V1>(const zx_exception_report_t& info) {
  // Current second version is an extension of v1; simply copy over the
  // earlier header and context.arch fields.
  zx_exception_report_v1_t v1_report = {};
  v1_report.header = info.header;
  memcpy(&v1_report.context, &info.context, sizeof(v1_report.context));
  return v1_report;
}

template <>
auto ConvertInfoVersion<ZX_INFO_TASK_STATS_V1>(const zx_info_task_stats_t& info) {
  zx_info_task_stats_v1 info_v1 = {
      .mem_mapped_bytes = info.mem_mapped_bytes,
      .mem_private_bytes = info.mem_private_bytes,
      .mem_shared_bytes = info.mem_shared_bytes,
      .mem_scaled_shared_bytes = info.mem_scaled_shared_bytes,
  };
  return info_v1;
}

zx::result<zx_exception_report_t> GetExceptionReport(ThreadDispatcher* thread) {
  zx_exception_report_t report = {};
  auto err = thread->GetExceptionReport(&report);
  if (err != ZX_OK) {
    return zx::error(err);
  }
  return zx::ok(report);
}

zx::result<zx_info_thread_stats_t> GetThreadStats(ThreadDispatcher* thread) {
  zx_info_thread_stats_t info = {};
  auto err = thread->GetStatsForUserspace(&info);
  if (err != ZX_OK)
    return zx::error(err);
  return zx::ok(info);
}

zx::result<zx_info_handle_count_t> GetHandleCount(Dispatcher* dispatcher) {
  zx_info_handle_count_t info{
      .handle_count = Handle::Count(*dispatcher),
  };
  return zx::ok(info);
}

zx::result<uint64_t> GetClockMappedSize(ClockDispatcher* clock) {
  // Only mappable clocks have a defined mapped size.
  if (!clock->is_mappable()) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  return zx::ok(ClockDispatcher::kMappedSize);
}

zx::result<zx_info_task_stats_t> GetProcessStats(ProcessDispatcher* process) {
  zx_info_task_stats_t info = {};
  auto err = process->GetStats(&info);
  if (err != ZX_OK) {
    return zx::error(err);
  }
  return zx::ok(info);
}

// The following 6 macros are used to generate implementations of zx_object_get_info for
// all single (non-array) topics. The macros take the following arguments:
//
//  id : the topic you want to generate the code for.
//  Td : The type of required dispatcher.
//  bs : The resource-base, the kind is assumed to be ZX_RSRC_KIND_SYSTEM.
//  mf : The "infailble" member function of Td that returns the information.
//  Fn : A function-like adapter that must return zx::result<info> for the cases
//       not covered by |mf|
//
//  For all the topics (except the ones using OB_GET_INFO_WR and OB_GET_INFO_SR) the required rights
//  are ZX_RIGHT_INSPECT.
//
// These macros rely on the some helper functions above:
//
// 1- template<> auto ConvertInfoVersion<topic>(const& current_info_version)
//    Used for converting from flavors of info, for example from "current" to V1, or V2
//    this function takes the current version as argument and returns the older version
//    selected by |topic|.
//
// 2- template<info_version> zx_status_t single_record_result(...)
//    this function writes to the usermode buffers.

// Vanilla version, takes a member function |mf| with no arguments which unconditionally
// returns the information.
#define OB_GET_INFO(id, Td, mf)                                                             \
  if constexpr (topic == (id)) {                                                            \
    fbl::RefPtr<Td> disp;                                                                   \
    zx_status_t status =                                                                    \
        up->handle_table().GetDispatcherWithRights(*up, handle, ZX_RIGHT_INSPECT, &disp);   \
    if (status != ZX_OK) {                                                                  \
      return status;                                                                        \
    }                                                                                       \
    auto res = ConvertInfoVersion<id>(disp->mf());                                          \
    return single_record_result(dst_buffer, dst_buffer_size, user_actual, user_avail, res); \
  }

// Version that takes a member function |mf| which receives the handle rights and unconditionally
// returns the information. No rights check performed.
#define OB_GET_INFO_WR(id, Td, mf)                                                               \
  if constexpr (topic == (id)) {                                                                 \
    fbl::RefPtr<Td> disp;                                                                        \
    zx_rights_t rights;                                                                          \
    zx_status_t status = up->handle_table().GetDispatcherAndRights(*up, handle, &disp, &rights); \
    if (status != ZX_OK) {                                                                       \
      return status;                                                                             \
    }                                                                                            \
    auto res = ConvertInfoVersion<id>(disp->mf(rights));                                         \
    return single_record_result(dst_buffer, dst_buffer_size, user_actual, user_avail, res);      \
  }

// Version that takes a function-like |Fn| which takes a T* and returns zx::result<I>.
#define OB_GET_INFO_ZR(id, Td, Fn)                                                          \
  if constexpr (topic == (id)) {                                                            \
    fbl::RefPtr<Td> disp;                                                                   \
    zx_status_t status =                                                                    \
        up->handle_table().GetDispatcherWithRights(*up, handle, ZX_RIGHT_INSPECT, &disp);   \
    if (status != ZX_OK) {                                                                  \
      return status;                                                                        \
    }                                                                                       \
    auto zr = Fn((disp.get()));                                                             \
    if (zr.is_error()) {                                                                    \
      return zr.error_value();                                                              \
    }                                                                                       \
    auto res = ConvertInfoVersion<id>(zr.value());                                          \
    return single_record_result(dst_buffer, dst_buffer_size, user_actual, user_avail, res); \
  }

// Version that takes a function |fn| and validates the handle using the base |bs| resource
// of kind ZX_RSRC_KIND_SYSTEM.
#define OB_GET_INFO_SR(id, Fn, bs)                                                          \
  if constexpr (topic == (id)) {                                                            \
    zx_status_t status = validate_ranged_resource(handle, ZX_RSRC_KIND_SYSTEM, bs, 1);      \
    if (status != ZX_OK) {                                                                  \
      return status;                                                                        \
    }                                                                                       \
    auto res = ConvertInfoVersion<id>(Fn());                                                \
    return single_record_result(dst_buffer, dst_buffer_size, user_actual, user_avail, res); \
  }

// Starts a sequence of object-get-info with the same topic |id|.
#define OB_GET_INFO_BEGIN(id)                                                               \
  if constexpr (topic == (id)) {                                                            \
    {                                                                                       \
      constexpr int ID = id;                                                                \
      fbl::RefPtr<Dispatcher> disp;                                                         \
      zx_status_t status =                                                                  \
          up->handle_table().GetDispatcherWithRights(*up, handle, ZX_RIGHT_INSPECT, &disp); \
      if (status != ZX_OK) {                                                                \
        return status;                                                                      \
      }

// Defines an entry within the OB_GET_INFO_BEGIN block, with |Td| being the required Dispatcher
// type.
#define OB_GET_INFO_EL(Td, mf)                                                                \
  {                                                                                           \
    auto actual = DownCastDispatcher<Td>(&disp);                                              \
    if (actual) {                                                                             \
      auto res = ConvertInfoVersion<ID>(actual->mf());                                        \
      return single_record_result(dst_buffer, dst_buffer_size, user_actual, user_avail, res); \
    }                                                                                         \
  }

// Ends the sequence started by OB_GET_INFO_BEGIN.
#define OB_GET_INFO_END(id)  \
  static_assert((id) == ID); \
  }                          \
  return ZX_ERR_WRONG_TYPE;  \
  }

template <int topic>
zx_status_t object_get_info(ProcessDispatcher* up, zx_handle_t handle,
                            user_out_ptr<void> dst_buffer, size_t dst_buffer_size,
                            user_out_ptr<size_t> user_actual, user_out_ptr<size_t> user_avail) {
  // clang-format off
  OB_GET_INFO(ZX_INFO_PROCESS, ProcessDispatcher, GetInfo)
  OB_GET_INFO(ZX_INFO_THREAD, ThreadDispatcher, GetInfoForUserspace)
  OB_GET_INFO(ZX_INFO_JOB, JobDispatcher, GetInfo)
  OB_GET_INFO(ZX_INFO_TIMER, TimerDispatcher, GetInfo)
  OB_GET_INFO(ZX_INFO_SOCKET, SocketDispatcher, GetInfo)
  OB_GET_INFO(ZX_INFO_STREAM, StreamDispatcher, GetInfo)
  OB_GET_INFO(ZX_INFO_MSI, MsiDispatcher, GetInfo)
  OB_GET_INFO(ZX_INFO_VCPU, VcpuDispatcher, GetInfo)
  OB_GET_INFO(ZX_INFO_IOB, IoBufferDispatcher, GetInfo);
  OB_GET_INFO(ZX_INFO_INTERRUPT, InterruptDispatcher, GetInfo)
  OB_GET_INFO(ZX_INFO_VMAR, VmAddressRegionDispatcher, GetVmarInfo)
  OB_GET_INFO(ZX_INFO_RESOURCE, ResourceDispatcher, GetInfo)
  OB_GET_INFO(ZX_INFO_BTI, BusTransactionInitiatorDispatcher, GetInfo)

  OB_GET_INFO_ZR(ZX_INFO_HANDLE_COUNT, Dispatcher, GetHandleCount)
  OB_GET_INFO_ZR(ZX_INFO_THREAD_EXCEPTION_REPORT, ThreadDispatcher, GetExceptionReport)
  OB_GET_INFO_ZR(ZX_INFO_THREAD_EXCEPTION_REPORT_V1, ThreadDispatcher, GetExceptionReport)
  OB_GET_INFO_ZR(ZX_INFO_THREAD_STATS, ThreadDispatcher, GetThreadStats)
  OB_GET_INFO_ZR(ZX_INFO_CLOCK_MAPPED_SIZE, ClockDispatcher, GetClockMappedSize)
  OB_GET_INFO_ZR(ZX_INFO_TASK_STATS, ProcessDispatcher, GetProcessStats)
  OB_GET_INFO_ZR(ZX_INFO_TASK_STATS_V1, ProcessDispatcher, GetProcessStats)
  OB_GET_INFO_ZR(ZX_INFO_PROCESS_HANDLE_STATS, ProcessDispatcher, GetHandleStats)

  OB_GET_INFO_BEGIN(ZX_INFO_TASK_RUNTIME);
  OB_GET_INFO_EL(JobDispatcher, GetRuntimeStats)
  OB_GET_INFO_EL(ProcessDispatcher, GetRuntimeStats);
  OB_GET_INFO_EL(ThreadDispatcher, GetRuntimeStats)
  OB_GET_INFO_END(ZX_INFO_TASK_RUNTIME)

  OB_GET_INFO_BEGIN(ZX_INFO_TASK_RUNTIME_V1);
  OB_GET_INFO_EL(JobDispatcher, GetRuntimeStats)
  OB_GET_INFO_EL(ProcessDispatcher, GetRuntimeStats)
  OB_GET_INFO_EL(ThreadDispatcher, GetRuntimeStats)
  OB_GET_INFO_END(ZX_INFO_TASK_RUNTIME_V1)

  OB_GET_INFO_WR(ZX_INFO_HANDLE_BASIC, Dispatcher, GetHandleInfo)
  OB_GET_INFO_WR(ZX_INFO_VMO, VmObjectDispatcher, GetVmoInfo)
  OB_GET_INFO_WR(ZX_INFO_VMO_V1, VmObjectDispatcher, GetVmoInfo)
  OB_GET_INFO_WR(ZX_INFO_VMO_V2, VmObjectDispatcher, GetVmoInfo)
  OB_GET_INFO_WR(ZX_INFO_VMO_V3, VmObjectDispatcher, GetVmoInfo)

  OB_GET_INFO_SR(ZX_INFO_KMEM_STATS, GetMemoryStats, ZX_RSRC_SYSTEM_INFO_BASE)
  OB_GET_INFO_SR(ZX_INFO_KMEM_STATS_V1, GetMemoryStats, ZX_RSRC_SYSTEM_INFO_BASE)
  OB_GET_INFO_SR(ZX_INFO_KMEM_STATS_EXTENDED, GetMemoryStats, ZX_RSRC_SYSTEM_INFO_BASE)
  OB_GET_INFO_SR(ZX_INFO_KMEM_STATS_COMPRESSION, GetCompressionStats, ZX_RSRC_SYSTEM_INFO_BASE)
  OB_GET_INFO_SR(ZX_INFO_MEMORY_STALL, GetStallStats, ZX_RSRC_SYSTEM_STALL_BASE)
  // clang-format on
  return ZX_ERR_NOT_SUPPORTED;
}

}  // namespace

// actual is an optional return parameter for the number of records returned
// avail is an optional return parameter for the number of records available
//
// Topics which return a fixed number of records will return ZX_ERR_BUFFER_TOO_SMALL
// if there is not enough buffer space provided.
// This allows for zx_object_get_info(handle, topic, &info, sizeof(info), NULL, NULL)
//
// zx_status_t zx_object_get_info
zx_status_t sys_object_get_info(zx_handle_t handle, uint32_t topic, user_out_ptr<void> _buffer,
                                size_t buffer_size, user_out_ptr<size_t> _actual,
                                user_out_ptr<size_t> _avail) {
  LTRACEF("handle %x topic %u\n", handle, topic);

  ProcessDispatcher* up = ProcessDispatcher::GetCurrent();

  switch (topic) {
    case ZX_INFO_HANDLE_VALID: {
      // This syscall + topic is excepted from the ZX_POL_BAD_HANDLE policy.
      fbl::RefPtr<Dispatcher> generic_dispatcher;
      return up->handle_table().GetDispatcherWithRightsNoPolicyCheck(handle, 0, &generic_dispatcher,
                                                                     nullptr);
    }
    case ZX_INFO_HANDLE_BASIC:
      return object_get_info<ZX_INFO_HANDLE_BASIC>(up, handle, _buffer, buffer_size, _actual,
                                                   _avail);
    case ZX_INFO_PROCESS:
      return object_get_info<ZX_INFO_PROCESS>(up, handle, _buffer, buffer_size, _actual, _avail);

    case ZX_INFO_PROCESS_THREADS: {
      // grab a reference to the dispatcher
      fbl::RefPtr<ProcessDispatcher> process;
      auto error =
          up->handle_table().GetDispatcherWithRights(*up, handle, ZX_RIGHT_ENUMERATE, &process);
      if (error != ZX_OK)
        return error;
      // Getting the list of threads is inherently racy (unless the caller has already stopped all
      // threads.
      fbl::Array<zx_koid_t> threads;
      zx_status_t status = process->GetThreads(&threads);
      if (status != ZX_OK)
        return status;

      return multi_record_result(_buffer, buffer_size, _actual, _avail, threads);
    }
    case ZX_INFO_JOB_CHILDREN:
    case ZX_INFO_JOB_PROCESSES: {
      fbl::RefPtr<JobDispatcher> job;
      auto error =
          up->handle_table().GetDispatcherWithRights(*up, handle, ZX_RIGHT_ENUMERATE, &job);
      if (error != ZX_OK)
        return error;

      size_t max = buffer_size / sizeof(zx_koid_t);
      auto koids = _buffer.reinterpret<zx_koid_t>();
      SimpleJobEnumerator sje(koids, max, topic == ZX_INFO_JOB_CHILDREN);

      // Don't recurse; we only want the job's direct children.
      if (!job->EnumerateChildren(&sje)) {
        // SimpleJobEnumerator only returns false when it can't
        // write to the user pointer.
        return ZX_ERR_INVALID_ARGS;
      }
      return actual_avail_result(sje.get_count(), sje.get_avail(), _actual, _avail);
    }
    case ZX_INFO_THREAD:
      return object_get_info<ZX_INFO_THREAD>(up, handle, _buffer, buffer_size, _actual, _avail);
    case ZX_INFO_THREAD_EXCEPTION_REPORT_V1:
      return object_get_info<ZX_INFO_THREAD_EXCEPTION_REPORT_V1>(up, handle, _buffer, buffer_size,
                                                                 _actual, _avail);
    case ZX_INFO_THREAD_EXCEPTION_REPORT:
      return object_get_info<ZX_INFO_THREAD_EXCEPTION_REPORT>(up, handle, _buffer, buffer_size,
                                                              _actual, _avail);
    case ZX_INFO_THREAD_STATS:
      return object_get_info<ZX_INFO_THREAD_STATS>(up, handle, _buffer, buffer_size, _actual,
                                                   _avail);
    case ZX_INFO_TASK_STATS_V1:
      return object_get_info<ZX_INFO_TASK_STATS_V1>(up, handle, _buffer, buffer_size, _actual,
                                                    _avail);
    case ZX_INFO_TASK_STATS:
      return object_get_info<ZX_INFO_TASK_STATS>(up, handle, _buffer, buffer_size, _actual, _avail);
    case ZX_INFO_TASK_RUNTIME:
      return object_get_info<ZX_INFO_TASK_RUNTIME>(up, handle, _buffer, buffer_size, _actual,
                                                   _avail);
    case ZX_INFO_TASK_RUNTIME_V1:
      return object_get_info<ZX_INFO_TASK_RUNTIME_V1>(up, handle, _buffer, buffer_size, _actual,
                                                      _avail);
    case ZX_INFO_VMAR_MAPS: {
      fbl::RefPtr<VmAddressRegionDispatcher> vmar;
      zx_status_t status =
          up->handle_table().GetDispatcherWithRights(*up, handle, ZX_RIGHT_INSPECT, &vmar);
      if (status != ZX_OK) {
        return status;
      }

      SubsetVmarMapsInfoWriter<zx_info_maps_t> writer{_buffer.reinterpret<zx_info_maps_t>()};
      const size_t max_records = buffer_size / sizeof(zx_info_maps_t);
      size_t actual_records = 0;
      size_t avail_records = 0;
      status =
          GetVmarMaps(vmar->vmar().get(), writer, max_records, &actual_records, &avail_records);
      if (status != ZX_OK)
        return status;

      return actual_avail_result(actual_records, avail_records, _actual, _avail);
    }
    case ZX_INFO_PROCESS_MAPS_V1:
    case ZX_INFO_PROCESS_MAPS_V2:
    case ZX_INFO_PROCESS_MAPS: {
      fbl::RefPtr<ProcessDispatcher> process;
      zx_status_t status =
          up->handle_table().GetDispatcherWithRights(*up, handle, ZX_RIGHT_INSPECT, &process);
      if (status != ZX_OK) {
        return status;
      }

      size_t count = 0;
      size_t avail = 0;

      if (topic == ZX_INFO_PROCESS_MAPS_V1) {
        SubsetVmarMapsInfoWriter<zx_info_maps_v1_t> writer{
            _buffer.reinterpret<zx_info_maps_v1_t>()};
        count = buffer_size / sizeof(zx_info_maps_v1_t);
        status = process->GetAspaceMaps(writer, count, &count, &avail);
      } else if (topic == ZX_INFO_PROCESS_MAPS_V2) {
        SubsetVmarMapsInfoWriter<zx_info_maps_v2_t> writer{
            _buffer.reinterpret<zx_info_maps_v2_t>()};
        count = buffer_size / sizeof(zx_info_maps_v2_t);
        status = process->GetAspaceMaps(writer, count, &count, &avail);
      } else {
        SubsetVmarMapsInfoWriter<zx_info_maps_t> writer{_buffer.reinterpret<zx_info_maps_t>()};
        count = buffer_size / sizeof(zx_info_maps_t);
        status = process->GetAspaceMaps(writer, count, &count, &avail);
      }
      zx_status_t copy_status = actual_avail_result(count, avail, _actual, _avail);
      return (copy_status != ZX_OK) ? copy_status : status;
    }
    case ZX_INFO_PROCESS_VMOS_V1:
    case ZX_INFO_PROCESS_VMOS_V2:
    case ZX_INFO_PROCESS_VMOS_V3:
    case ZX_INFO_PROCESS_VMOS: {
      fbl::RefPtr<ProcessDispatcher> process;
      zx_status_t status =
          up->handle_table().GetDispatcherWithRights(*up, handle, ZX_RIGHT_INSPECT, &process);
      if (status != ZX_OK) {
        return status;
      }

      size_t count = 0;
      size_t avail = 0;

      if (topic == ZX_INFO_PROCESS_VMOS_V1) {
        SubsetVmoInfoWriter<zx_info_vmo_v1_t> writer{_buffer.reinterpret<zx_info_vmo_v1_t>()};
        count = buffer_size / sizeof(zx_info_vmo_v1_t);
        status = process->GetVmos(writer, count, &count, &avail);
      } else if (topic == ZX_INFO_PROCESS_VMOS_V2) {
        SubsetVmoInfoWriter<zx_info_vmo_v2_t> writer{_buffer.reinterpret<zx_info_vmo_v2_t>()};
        count = buffer_size / sizeof(zx_info_vmo_v2_t);
        status = process->GetVmos(writer, count, &count, &avail);
      } else if (topic == ZX_INFO_PROCESS_VMOS_V3) {
        SubsetVmoInfoWriter<zx_info_vmo_v3_t> writer{_buffer.reinterpret<zx_info_vmo_v3_t>()};
        count = buffer_size / sizeof(zx_info_vmo_v3_t);
        status = process->GetVmos(writer, count, &count, &avail);
      } else {
        SubsetVmoInfoWriter<zx_info_vmo_t> writer{_buffer.reinterpret<zx_info_vmo_t>()};
        count = buffer_size / sizeof(zx_info_vmo_t);
        status = process->GetVmos(writer, count, &count, &avail);
      }
      zx_status_t copy_status = actual_avail_result(count, avail, _actual, _avail);
      return (copy_status != ZX_OK) ? copy_status : status;
    }
    case ZX_INFO_VMO:
      return object_get_info<ZX_INFO_VMO>(up, handle, _buffer, buffer_size, _actual, _avail);
    case ZX_INFO_VMO_V1:
      return object_get_info<ZX_INFO_VMO_V1>(up, handle, _buffer, buffer_size, _actual, _avail);
    case ZX_INFO_VMO_V2:
      return object_get_info<ZX_INFO_VMO_V2>(up, handle, _buffer, buffer_size, _actual, _avail);
    case ZX_INFO_VMO_V3:
      return object_get_info<ZX_INFO_VMO_V3>(up, handle, _buffer, buffer_size, _actual, _avail);
    case ZX_INFO_VMAR:
      return object_get_info<ZX_INFO_VMAR>(up, handle, _buffer, buffer_size, _actual, _avail);
    case ZX_INFO_GUEST_STATS: {
      zx_status_t status =
          validate_ranged_resource(handle, ZX_RSRC_KIND_SYSTEM, ZX_RSRC_SYSTEM_INFO_BASE, 1);
      if (status != ZX_OK)
        return status;

      size_t num_cpus = arch_max_num_cpus();
      size_t num_space_for = buffer_size / sizeof(zx_info_guest_stats_t);
      size_t num_to_copy = ktl::min(num_cpus, num_space_for);
      user_out_ptr<zx_info_guest_stats_t> guest_buf = _buffer.reinterpret<zx_info_guest_stats_t>();

      for (unsigned int i = 0; i < static_cast<unsigned int>(num_to_copy); i++) {
        zx_info_guest_stats_t stats = GetGuestCPUStats(i);
        if (guest_buf.copy_array_to_user(&stats, 1, i) != ZX_OK)
          return ZX_ERR_INVALID_ARGS;
      }
      return actual_avail_result(num_to_copy, num_cpus, _actual, _avail);
    }

    case ZX_INFO_CPU_STATS: {
      zx_status_t status =
          validate_ranged_resource(handle, ZX_RSRC_KIND_SYSTEM, ZX_RSRC_SYSTEM_INFO_BASE, 1);
      if (status != ZX_OK)
        return status;

      size_t num_cpus = arch_max_num_cpus();
      size_t num_space_for = buffer_size / sizeof(zx_info_cpu_stats_t);
      size_t num_to_copy = ktl::min(num_cpus, num_space_for);
      // build an alias to the output buffer that is in units of the cpu stat structure
      user_out_ptr<zx_info_cpu_stats_t> cpu_buf = _buffer.reinterpret<zx_info_cpu_stats_t>();

      for (unsigned int i = 0; i < static_cast<unsigned int>(num_to_copy); i++) {
        zx_info_cpu_stats_t stats = GetCPUStats(i);
        if (cpu_buf.copy_array_to_user(&stats, 1, i) != ZX_OK)
          return ZX_ERR_INVALID_ARGS;
      }
      return actual_avail_result(num_to_copy, num_cpus, _actual, _avail);
    }

    case ZX_INFO_KMEM_STATS:
      return object_get_info<ZX_INFO_KMEM_STATS>(up, handle, _buffer, buffer_size, _actual, _avail);
    case ZX_INFO_KMEM_STATS_EXTENDED:
      return object_get_info<ZX_INFO_KMEM_STATS_EXTENDED>(up, handle, _buffer, buffer_size, _actual,
                                                          _avail);
    case ZX_INFO_KMEM_STATS_V1:
      return object_get_info<ZX_INFO_KMEM_STATS_V1>(up, handle, _buffer, buffer_size, _actual,
                                                    _avail);
    case ZX_INFO_KMEM_STATS_COMPRESSION:
      return object_get_info<ZX_INFO_KMEM_STATS_COMPRESSION>(up, handle, _buffer, buffer_size,
                                                             _actual, _avail);
    case ZX_INFO_RESOURCE:
      return object_get_info<ZX_INFO_RESOURCE>(up, handle, _buffer, buffer_size, _actual, _avail);
    case ZX_INFO_HANDLE_COUNT:
      return object_get_info<ZX_INFO_HANDLE_COUNT>(up, handle, _buffer, buffer_size, _actual,
                                                   _avail);
    case ZX_INFO_BTI:
      return object_get_info<ZX_INFO_BTI>(up, handle, _buffer, buffer_size, _actual, _avail);
    case ZX_INFO_PROCESS_HANDLE_STATS:
      return object_get_info<ZX_INFO_PROCESS_HANDLE_STATS>(up, handle, _buffer, buffer_size,
                                                           _actual, _avail);
    case ZX_INFO_SOCKET:
      return object_get_info<ZX_INFO_SOCKET>(up, handle, _buffer, buffer_size, _actual, _avail);
    case ZX_INFO_JOB:
      return object_get_info<ZX_INFO_JOB>(up, handle, _buffer, buffer_size, _actual, _avail);
    case ZX_INFO_TIMER:
      return object_get_info<ZX_INFO_TIMER>(up, handle, _buffer, buffer_size, _actual, _avail);
    case ZX_INFO_STREAM:
      return object_get_info<ZX_INFO_STREAM>(up, handle, _buffer, buffer_size, _actual, _avail);

    case ZX_INFO_HANDLE_TABLE: {
      fbl::RefPtr<ProcessDispatcher> process;
      auto error = up->handle_table().GetDispatcherWithRights(
          *up, handle, ZX_RIGHT_INSPECT | ZX_RIGHT_MANAGE_PROCESS | ZX_RIGHT_MANAGE_THREAD,
          &process);
      if (error != ZX_OK)
        return error;

      if (!_buffer && !_avail && _actual) {
        // Optimization for callers which call twice, the first time just to know the size.
        return _actual.copy_to_user(static_cast<size_t>(up->handle_table().HandleCount()));
      }
      fbl::Array<zx_info_handle_extended_t> handle_info;
      zx_status_t status = process->handle_table().GetHandleInfo(&handle_info);
      if (status != ZX_OK)
        return status;

      return multi_record_result(_buffer, buffer_size, _actual, _avail, handle_info);
    }
    case ZX_INFO_MSI:
      return object_get_info<ZX_INFO_MSI>(up, handle, _buffer, buffer_size, _actual, _avail);
    case ZX_INFO_VCPU:
      return object_get_info<ZX_INFO_VCPU>(up, handle, _buffer, buffer_size, _actual, _avail);
    case ZX_INFO_IOB:
      return object_get_info<ZX_INFO_IOB>(up, handle, _buffer, buffer_size, _actual, _avail);
    case ZX_INFO_IOB_REGIONS: {
      fbl::RefPtr<IoBufferDispatcher> iob;
      zx_status_t status =
          up->handle_table().GetDispatcherWithRights(*up, handle, ZX_RIGHT_INSPECT, &iob);
      if (status != ZX_OK) {
        return status;
      }

      const size_t num_regions = iob->RegionCount();
      const size_t num_space_for = buffer_size / sizeof(zx_iob_region_info_t);
      const size_t num_to_copy = ktl::min(num_regions, num_space_for);

      for (size_t i = 0; i < num_to_copy; i++) {
        zx_iob_region_info_t region = iob->GetRegionInfo(i);
        status = _buffer.reinterpret<zx_iob_region_info_t>().element_offset(i).copy_to_user(region);
        if (status != ZX_OK) {
          return status;
        }
      }
      return actual_avail_result(num_to_copy, num_regions, _actual, _avail);
    }

    case ZX_INFO_POWER_DOMAINS: {
      if (zx_status_t res =
              validate_ranged_resource(handle, ZX_RSRC_KIND_SYSTEM, ZX_RSRC_SYSTEM_INFO_BASE, 1);
          res != ZX_OK) {
        return res;
      }
      size_t max_copy = buffer_size / sizeof(zx_power_domain_info_t);
      auto result = GetPowerDomainsInfo(max_copy);
      if (result.is_error()) {
        return result.error_value();
      }
      fbl::Array<zx_power_domain_info_t> entries{ktl::move(result.value())};
      return multi_record_result(_buffer, buffer_size, _actual, _avail, entries);
    }

    case ZX_INFO_MEMORY_STALL:
      return object_get_info<ZX_INFO_MEMORY_STALL>(up, handle, _buffer, buffer_size, _actual,
                                                   _avail);
    case ZX_INFO_CLOCK_MAPPED_SIZE:
      return object_get_info<ZX_INFO_CLOCK_MAPPED_SIZE>(up, handle, _buffer, buffer_size, _actual,
                                                        _avail);
    case ZX_INFO_INTERRUPT:
      return object_get_info<ZX_INFO_INTERRUPT>(up, handle, _buffer, buffer_size, _actual, _avail);

    default:
      return ZX_ERR_NOT_SUPPORTED;
  }
}

#if ARCH_X86
static zx_status_t RequireCurrentThread(fbl::RefPtr<Dispatcher> dispatcher) {
  auto thread_dispatcher = DownCastDispatcher<ThreadDispatcher>(&dispatcher);
  if (!thread_dispatcher) {
    return ZX_ERR_WRONG_TYPE;
  }
  if (thread_dispatcher.get() != ThreadDispatcher::GetCurrent()) {
    return ZX_ERR_ACCESS_DENIED;
  }
  return ZX_OK;
}
#endif

// zx_status_t zx_object_get_property
zx_status_t sys_object_get_property(zx_handle_t handle_value, uint32_t property,
                                    user_out_ptr<void> _value, size_t size) {
  if (!_value)
    return ZX_ERR_INVALID_ARGS;

  auto up = ProcessDispatcher::GetCurrent();
  fbl::RefPtr<Dispatcher> dispatcher;
  zx_status_t status = up->handle_table().GetDispatcherWithRights(
      *up, handle_value, ZX_RIGHT_GET_PROPERTY, &dispatcher);
  if (status != ZX_OK)
    return status;
  switch (property) {
    case ZX_PROP_NAME: {
      if (size < ZX_MAX_NAME_LEN)
        return ZX_ERR_BUFFER_TOO_SMALL;
      char name[ZX_MAX_NAME_LEN] = {};
      status = dispatcher->get_name(name);
      if (status != ZX_OK) {
        return status;
      }
      if (_value.reinterpret<char>().copy_array_to_user(name, ZX_MAX_NAME_LEN) != ZX_OK)
        return ZX_ERR_INVALID_ARGS;
      return ZX_OK;
    }
    case ZX_PROP_PROCESS_DEBUG_ADDR: {
      if (size < sizeof(uintptr_t))
        return ZX_ERR_BUFFER_TOO_SMALL;
      auto process = DownCastDispatcher<ProcessDispatcher>(&dispatcher);
      if (!process)
        return ZX_ERR_WRONG_TYPE;
      uintptr_t value = process->get_debug_addr();
      return _value.reinterpret<uintptr_t>().copy_to_user(value);
    }
    case ZX_PROP_PROCESS_BREAK_ON_LOAD: {
      if (size < sizeof(uintptr_t))
        return ZX_ERR_BUFFER_TOO_SMALL;
      auto process = DownCastDispatcher<ProcessDispatcher>(&dispatcher);
      if (!process)
        return ZX_ERR_WRONG_TYPE;
      uintptr_t value = process->get_dyn_break_on_load();
      return _value.reinterpret<uintptr_t>().copy_to_user(value);
    }
    case ZX_PROP_PROCESS_VDSO_BASE_ADDRESS: {
      if (size < sizeof(uintptr_t))
        return ZX_ERR_BUFFER_TOO_SMALL;
      auto process = DownCastDispatcher<ProcessDispatcher>(&dispatcher);
      if (!process)
        return ZX_ERR_WRONG_TYPE;
      uintptr_t value = process->vdso_base_address();
      return _value.reinterpret<uintptr_t>().copy_to_user(value);
    }
    case ZX_PROP_PROCESS_HW_TRACE_CONTEXT_ID: {
      if (!gBootOptions->enable_debugging_syscalls) {
        return ZX_ERR_NOT_SUPPORTED;
      }
#if ARCH_X86
      if (size < sizeof(uintptr_t)) {
        return ZX_ERR_BUFFER_TOO_SMALL;
      }
      auto process = DownCastDispatcher<ProcessDispatcher>(&dispatcher);
      if (!process) {
        return ZX_ERR_WRONG_TYPE;
      }
      uintptr_t value = process->hw_trace_context_id();
      return _value.reinterpret<uintptr_t>().copy_to_user(value);
#else
      return ZX_ERR_NOT_SUPPORTED;
#endif
    }
    case ZX_PROP_SOCKET_RX_THRESHOLD: {
      if (size < sizeof(size_t))
        return ZX_ERR_BUFFER_TOO_SMALL;
      auto socket = DownCastDispatcher<SocketDispatcher>(&dispatcher);
      if (!socket)
        return ZX_ERR_WRONG_TYPE;
      size_t value = socket->GetReadThreshold();
      return _value.reinterpret<size_t>().copy_to_user(value);
    }
    case ZX_PROP_SOCKET_TX_THRESHOLD: {
      if (size < sizeof(size_t))
        return ZX_ERR_BUFFER_TOO_SMALL;
      auto socket = DownCastDispatcher<SocketDispatcher>(&dispatcher);
      if (!socket)
        return ZX_ERR_WRONG_TYPE;
      size_t value = socket->GetWriteThreshold();
      return _value.reinterpret<size_t>().copy_to_user(value);
    }
    case ZX_PROP_EXCEPTION_STATE: {
      if (size < sizeof(uint32_t)) {
        return ZX_ERR_BUFFER_TOO_SMALL;
      }
      auto exception = DownCastDispatcher<ExceptionDispatcher>(&dispatcher);
      if (!exception) {
        return ZX_ERR_WRONG_TYPE;
      }

      return _value.reinterpret<uint32_t>().copy_to_user(exception->GetDisposition());
    }
    case ZX_PROP_EXCEPTION_STRATEGY: {
      if (size < sizeof(uint32_t)) {
        return ZX_ERR_BUFFER_TOO_SMALL;
      }
      auto exception = DownCastDispatcher<ExceptionDispatcher>(&dispatcher);
      if (!exception) {
        return ZX_ERR_WRONG_TYPE;
      }

      bool second_chance = exception->IsSecondChance();
      return _value.reinterpret<uint32_t>().copy_to_user(
          second_chance ? ZX_EXCEPTION_STRATEGY_SECOND_CHANCE : ZX_EXCEPTION_STRATEGY_FIRST_CHANCE);
    }
    case ZX_PROP_VMO_CONTENT_SIZE: {
      if (size < sizeof(uint64_t)) {
        return ZX_ERR_BUFFER_TOO_SMALL;
      }
      auto vmo = DownCastDispatcher<VmObjectDispatcher>(&dispatcher);
      if (!vmo) {
        return ZX_ERR_WRONG_TYPE;
      }

      uint64_t value = vmo->GetContentSize();
      return _value.reinterpret<uint64_t>().copy_to_user(value);
    }
    case ZX_PROP_STREAM_MODE_APPEND: {
      if (size < sizeof(uint8_t)) {
        return ZX_ERR_BUFFER_TOO_SMALL;
      }
      auto stream = DownCastDispatcher<StreamDispatcher>(&dispatcher);
      if (!stream) {
        return ZX_ERR_WRONG_TYPE;
      }

      uint8_t value = stream->IsInAppendMode();
      return _value.reinterpret<uint8_t>().copy_to_user(value);
    }
#if ARCH_X86
    case ZX_PROP_REGISTER_FS: {
      if (size < sizeof(uintptr_t)) {
        return ZX_ERR_BUFFER_TOO_SMALL;
      }
      status = RequireCurrentThread(ktl::move(dispatcher));
      if (status != ZX_OK) {
        return status;
      }
      uintptr_t value = read_msr(X86_MSR_IA32_FS_BASE);
      return _value.reinterpret<uintptr_t>().copy_to_user(value);
    }
    case ZX_PROP_REGISTER_GS: {
      if (size < sizeof(uintptr_t)) {
        return ZX_ERR_BUFFER_TOO_SMALL;
      }
      status = RequireCurrentThread(ktl::move(dispatcher));
      if (status != ZX_OK) {
        return status;
      }
      uintptr_t value = read_msr(X86_MSR_IA32_KERNEL_GS_BASE);
      return _value.reinterpret<uintptr_t>().copy_to_user(value);
    }
#endif

    default:
      return ZX_ERR_NOT_SUPPORTED;
  }

  __UNREACHABLE;
}

// zx_status_t zx_object_set_property
zx_status_t sys_object_set_property(zx_handle_t handle_value, uint32_t property,
                                    user_in_ptr<const void> _value, size_t size) {
  if (!_value)
    return ZX_ERR_INVALID_ARGS;

  auto up = ProcessDispatcher::GetCurrent();
  fbl::RefPtr<Dispatcher> dispatcher;

  zx_rights_t rights;
  const zx_status_t get_dispatcher_status = up->handle_table().GetDispatcherWithRights(
      *up, handle_value, ZX_RIGHT_SET_PROPERTY, &dispatcher, &rights);
  if (get_dispatcher_status != ZX_OK)
    return get_dispatcher_status;

  switch (property) {
    case ZX_PROP_NAME: {
      if (size >= ZX_MAX_NAME_LEN)
        size = ZX_MAX_NAME_LEN - 1;
      char name[ZX_MAX_NAME_LEN - 1];
      if (_value.reinterpret<const char>().copy_array_from_user(name, size) != ZX_OK)
        return ZX_ERR_INVALID_ARGS;
      return dispatcher->set_name(name, size);
    }
#if ARCH_X86
    case ZX_PROP_REGISTER_FS: {
      if (size < sizeof(uintptr_t))
        return ZX_ERR_BUFFER_TOO_SMALL;
      zx_status_t status = RequireCurrentThread(ktl::move(dispatcher));
      if (status != ZX_OK)
        return status;
      uintptr_t addr;
      status = _value.reinterpret<const uintptr_t>().copy_from_user(&addr);
      if (status != ZX_OK)
        return status;
      if (!x86_is_vaddr_canonical(addr))
        return ZX_ERR_INVALID_ARGS;
      write_msr(X86_MSR_IA32_FS_BASE, addr);
      return ZX_OK;
    }
    case ZX_PROP_REGISTER_GS: {
      if (size < sizeof(uintptr_t))
        return ZX_ERR_BUFFER_TOO_SMALL;
      zx_status_t status = RequireCurrentThread(ktl::move(dispatcher));
      if (status != ZX_OK)
        return status;
      uintptr_t addr;
      status = _value.reinterpret<const uintptr_t>().copy_from_user(&addr);
      if (status != ZX_OK)
        return status;
      if (!x86_is_vaddr_canonical(addr))
        return ZX_ERR_INVALID_ARGS;
      write_msr(X86_MSR_IA32_KERNEL_GS_BASE, addr);
      return ZX_OK;
    }
#endif
    case ZX_PROP_PROCESS_DEBUG_ADDR: {
      if (size < sizeof(uintptr_t))
        return ZX_ERR_BUFFER_TOO_SMALL;
      auto process = DownCastDispatcher<ProcessDispatcher>(&dispatcher);
      if (!process)
        return ZX_ERR_WRONG_TYPE;
      uintptr_t value = 0;
      zx_status_t status = _value.reinterpret<const uintptr_t>().copy_from_user(&value);
      if (status != ZX_OK)
        return status;
      return process->set_debug_addr(value);
    }
    case ZX_PROP_PROCESS_BREAK_ON_LOAD: {
      if (size < sizeof(uintptr_t))
        return ZX_ERR_BUFFER_TOO_SMALL;
      auto process = DownCastDispatcher<ProcessDispatcher>(&dispatcher);
      if (!process)
        return ZX_ERR_WRONG_TYPE;
      uintptr_t value = 0;
      zx_status_t status = _value.reinterpret<const uintptr_t>().copy_from_user(&value);
      if (status != ZX_OK)
        return status;
      return process->set_dyn_break_on_load(value);
    }
    case ZX_PROP_SOCKET_RX_THRESHOLD: {
      if (size < sizeof(size_t))
        return ZX_ERR_BUFFER_TOO_SMALL;
      auto socket = DownCastDispatcher<SocketDispatcher>(&dispatcher);
      if (!socket)
        return ZX_ERR_WRONG_TYPE;
      size_t value = 0;
      zx_status_t status = _value.reinterpret<const size_t>().copy_from_user(&value);
      if (status != ZX_OK)
        return status;
      return socket->SetReadThreshold(value);
    }
    case ZX_PROP_SOCKET_TX_THRESHOLD: {
      if (size < sizeof(size_t))
        return ZX_ERR_BUFFER_TOO_SMALL;
      auto socket = DownCastDispatcher<SocketDispatcher>(&dispatcher);
      if (!socket)
        return ZX_ERR_WRONG_TYPE;
      size_t value = 0;
      zx_status_t status = _value.reinterpret<const size_t>().copy_from_user(&value);
      if (status != ZX_OK)
        return status;
      return socket->SetWriteThreshold(value);
    }
    case ZX_PROP_JOB_KILL_ON_OOM: {
      auto job = DownCastDispatcher<JobDispatcher>(&dispatcher);
      if (!job)
        return ZX_ERR_WRONG_TYPE;
      size_t value = 0;
      zx_status_t status = _value.reinterpret<const size_t>().copy_from_user(&value);
      if (status != ZX_OK)
        return status;
      if (value == 0u) {
        job->set_kill_on_oom(false);
      } else if (value == 1u) {
        job->set_kill_on_oom(true);
      } else {
        return ZX_ERR_INVALID_ARGS;
      }
      return ZX_OK;
    }
    case ZX_PROP_EXCEPTION_STATE: {
      if (size < sizeof(uint32_t)) {
        return ZX_ERR_BUFFER_TOO_SMALL;
      }
      auto exception = DownCastDispatcher<ExceptionDispatcher>(&dispatcher);
      if (!exception) {
        return ZX_ERR_WRONG_TYPE;
      }
      uint32_t value = 0;
      zx_status_t status = _value.reinterpret<const uint32_t>().copy_from_user(&value);
      if (status != ZX_OK) {
        return status;
      }
      if (value == ZX_EXCEPTION_STATE_HANDLED) {
        exception->SetDisposition(ZX_EXCEPTION_STATE_HANDLED);
      } else if (value == ZX_EXCEPTION_STATE_TRY_NEXT) {
        exception->SetDisposition(ZX_EXCEPTION_STATE_TRY_NEXT);
      } else if (value == ZX_EXCEPTION_STATE_THREAD_EXIT) {
        exception->SetDisposition(ZX_EXCEPTION_STATE_THREAD_EXIT);
      } else {
        return ZX_ERR_INVALID_ARGS;
      }
      return ZX_OK;
    }
    case ZX_PROP_EXCEPTION_STRATEGY: {
      if (size < sizeof(uint32_t)) {
        return ZX_ERR_BUFFER_TOO_SMALL;
      }
      auto exception = DownCastDispatcher<ExceptionDispatcher>(&dispatcher);
      if (!exception) {
        return ZX_ERR_WRONG_TYPE;
      }

      // Invalid if the exception handle is not held by a debugger.
      const zx_info_thread_t info = exception->thread()->GetInfoForUserspace();
      if (info.wait_exception_channel_type != ZX_EXCEPTION_CHANNEL_TYPE_DEBUGGER) {
        return ZX_ERR_BAD_STATE;
      }

      uint32_t value = 0;
      const zx_status_t status = _value.reinterpret<const uint32_t>().copy_from_user(&value);
      if (status != ZX_OK) {
        return status;
      }
      if (value == ZX_EXCEPTION_STRATEGY_FIRST_CHANCE) {
        exception->SetWhetherSecondChance(false);
      } else if (value == ZX_EXCEPTION_STRATEGY_SECOND_CHANCE) {
        exception->SetWhetherSecondChance(true);
      } else {
        return ZX_ERR_INVALID_ARGS;
      }
      return ZX_OK;
    }
    case ZX_PROP_VMO_CONTENT_SIZE: {
      if ((rights & ZX_RIGHT_WRITE) == 0) {
        return ZX_ERR_ACCESS_DENIED;
      }
      if (size < sizeof(uint64_t)) {
        return ZX_ERR_BUFFER_TOO_SMALL;
      }
      auto vmo = DownCastDispatcher<VmObjectDispatcher>(&dispatcher);
      if (!vmo) {
        return ZX_ERR_WRONG_TYPE;
      }
      uint64_t value = 0;
      zx_status_t status = _value.reinterpret<const uint64_t>().copy_from_user(&value);
      if (status != ZX_OK) {
        return status;
      }
      return vmo->SetContentSize(value);
    }
    case ZX_PROP_STREAM_MODE_APPEND: {
      if (size < sizeof(uint8_t)) {
        return ZX_ERR_BUFFER_TOO_SMALL;
      }
      auto stream = DownCastDispatcher<StreamDispatcher>(&dispatcher);
      if (!stream) {
        return ZX_ERR_WRONG_TYPE;
      }
      uint8_t value = 0;
      zx_status_t status = _value.reinterpret<const uint8_t>().copy_from_user(&value);
      if (status != ZX_OK) {
        return status;
      }
      return stream->SetAppendMode(value);
    }
    default:
      return ZX_ERR_NOT_SUPPORTED;
  }

  __UNREACHABLE;
}

// zx_status_t zx_object_signal
zx_status_t sys_object_signal(zx_handle_t handle_value, uint32_t clear_mask, uint32_t set_mask) {
  LTRACEF("handle %x\n", handle_value);

  auto up = ProcessDispatcher::GetCurrent();
  fbl::RefPtr<Dispatcher> dispatcher;

  auto status =
      up->handle_table().GetDispatcherWithRights(*up, handle_value, ZX_RIGHT_SIGNAL, &dispatcher);
  if (status != ZX_OK)
    return status;

  return dispatcher->user_signal_self(clear_mask, set_mask);
}

// zx_status_t zx_object_signal_peer
zx_status_t sys_object_signal_peer(zx_handle_t handle_value, uint32_t clear_mask,
                                   uint32_t set_mask) {
  LTRACEF("handle %x\n", handle_value);

  auto up = ProcessDispatcher::GetCurrent();
  fbl::RefPtr<Dispatcher> dispatcher;

  auto status = up->handle_table().GetDispatcherWithRights(*up, handle_value, ZX_RIGHT_SIGNAL_PEER,
                                                           &dispatcher);
  if (status != ZX_OK)
    return status;

  return dispatcher->user_signal_peer(clear_mask, set_mask);
}

// Given a kernel object with children objects, obtain a handle to the
// child specified by the provided kernel object id.
// zx_status_t zx_object_get_child
zx_status_t sys_object_get_child(zx_handle_t handle, uint64_t koid, zx_rights_t rights,
                                 zx_handle_t* out) {
  auto up = ProcessDispatcher::GetCurrent();

  fbl::RefPtr<Dispatcher> dispatcher;
  uint32_t parent_rights;
  auto status = up->handle_table().GetDispatcherAndRights(*up, handle, &dispatcher, &parent_rights);
  if (status != ZX_OK)
    return status;

  if (!(parent_rights & ZX_RIGHT_ENUMERATE))
    return ZX_ERR_ACCESS_DENIED;

  if (rights == ZX_RIGHT_SAME_RIGHTS) {
    rights = parent_rights;
  } else if ((parent_rights & rights) != rights) {
    return ZX_ERR_ACCESS_DENIED;
  }

  // TODO(https://fxbug.dev/42175105): Constructing the handles below may cause the handle count to
  // go from 0->1, resulting in multiple on_zero_handles invocations. Presently this is benign,
  // except for one scenario with processes in the initial state. Such processes are filtered out by
  // the SimpleJobEnumerator and should not be able to be learned about. Further protection against
  // guessing is not performed here since the worst case scenario is a misbehaving privileged
  // process guessing a koid and destroying a process that was in construction.
  auto process = DownCastDispatcher<ProcessDispatcher>(&dispatcher);
  if (process) {
    auto thread = process->LookupThreadById(koid);
    if (!thread)
      return ZX_ERR_NOT_FOUND;
    return up->MakeAndAddHandle(ktl::move(thread), rights, out);
  }

  auto job = DownCastDispatcher<JobDispatcher>(&dispatcher);
  if (job) {
    auto child = job->LookupJobById(koid);
    if (child)
      return up->MakeAndAddHandle(ktl::move(child), rights, out);
    auto proc = job->LookupProcessById(koid);
    if (proc) {
      return up->MakeAndAddHandle(ktl::move(proc), rights, out);
    }
    return ZX_ERR_NOT_FOUND;
  }

  return ZX_ERR_WRONG_TYPE;
}
