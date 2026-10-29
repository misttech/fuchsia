// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_IDLE_STATES_H_
#define ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_IDLE_STATES_H_

#include <zircon/compiler.h>
#include <zircon/time.h>
#include <zircon/types.h>

#include <arch/x86/idle_states_ffi.h>
#include <kernel/ffi.h>

#define X86_MAX_CSTATES 12

typedef struct x86_idle_state {  // NOLINT(modernize-use-using)
  // A human-readable name for the state.
  const char* name;
  // The value to set the mwait hint register to in order to enter the state.
  uint32_t mwait_hint;
  // The expected latency (in us) of exiting the C-state.
  uint32_t exit_latency;
  // Whether entering the state can result in a TLB flush.
  bool flushes_tlb;
} x86_idle_state_t;

typedef struct x86_idle_states {  // NOLINT(modernize-use-using)
  // A list of states supported by the system, sorted by descending
  // latency to exit the state.
  // The list must be non-empty and end with X86_CSTATE_C1; all states after
  // this entry must be ignored.
  x86_idle_state_t states[X86_MAX_CSTATES];
  // A bitmask of MWAIT hints which the system can request to enter.
  //
  // Bit x being set means that MWAIT(Cx+1) can be requested. For example:
  //   0b0000 0000 0001 1111
  // Allows states MWAIT(C1-C5) (and all of their sub-states) to be requested.
  //
  // NB:
  //  - The base state C1 is always allowed, so bit 0 is ignored.
  //  - MWAIT(Cx) does not necessarily map to C-state x; for example on Kaby
  //    Lake processors, C-state C6 has mwait hint 0x20 (MWAIT(C3)).
  //
  // Can be overridden via `k idlestates setmask`.
  uint32_t default_state_mask;
} x86_idle_states_t;

// Every processor must support at least C1.
#define X86_CSTATE_C1_NAME "C1"
#define X86_CSTATE_C1_MWAIT_HINT 0x00

#define X86_CSTATE_C1(exit_latency_us)        \
  {                                           \
      .name = X86_CSTATE_C1_NAME,             \
      .mwait_hint = X86_CSTATE_C1_MWAIT_HINT, \
      .exit_latency = (exit_latency_us),      \
      .flushes_tlb = false,                   \
  }

static inline bool x86_is_base_idle_state(const x86_idle_state_t* state) {
  return state->mwait_hint == X86_CSTATE_C1_MWAIT_HINT;
}

// Returns a read-only pointer to the list of idle states supported by the
// system.
inline const x86_idle_states_t* x86_get_idle_states() { return rust_x86_get_idle_states(); }

constexpr uint32_t kX86IdleStateMaskC1Only = 0x1;

class X86IdleState {
 public:
  X86IdleState() = delete;
  X86IdleState(const X86IdleState&) = delete;
  X86IdleState& operator=(const X86IdleState&) = delete;
  X86IdleState(X86IdleState&&) = delete;
  X86IdleState& operator=(X86IdleState&&) = delete;

  const char* Name() const { return rust_x86_idle_state_name(this); }

  // Returns the hint to provide to MWAIT to enter this state.
  uint32_t MwaitHint() const { return rust_x86_idle_state_mwait_hint(this); }

  // Marks the idle state as entered.
  void CountEntry() const { rust_x86_idle_state_count_entry(this); }

  // Records that the system spent |duration| in this state before exiting.
  void RecordDuration(zx_duration_t duration) const {
    rust_x86_idle_state_record_duration(this, duration);
  }

 private:
  [[maybe_unused]] OpaqueStorage<24, 8> storage_{};
};

static_assert(sizeof(X86IdleState) == 24);
static_assert(alignof(X86IdleState) == 8);

class X86IdleStates {
 public:
  // |states| and any non-null |name| strings within it must have static
  // storage duration ('static).
  explicit X86IdleStates(const x86_idle_states_t* states) {
    rust_x86_idle_states_init(this, states);
  }
  X86IdleStates(const X86IdleStates&) = delete;
  X86IdleStates& operator=(const X86IdleStates&) = delete;
  X86IdleStates(X86IdleStates&&) = delete;
  X86IdleStates& operator=(X86IdleStates&&) = delete;

  // Returns the list of states supported by the CPU, with the same
  // ordering constraints as documented for x86_idle_states_t.
  const X86IdleState* States() const { return rust_x86_idle_states_states(this); }

  size_t NumStates() const { return rust_x86_idle_states_num_states(this); }

  // Picks an idle state to enter.
  X86IdleState* PickIdleState() {
    return const_cast<X86IdleState*>(rust_x86_idle_states_pick_idle_state(this));
  }
  const X86IdleState* PickIdleState() const { return rust_x86_idle_states_pick_idle_state(this); }

  // Callback to call when the system becomes idle.
  void RecordDuration(zx_duration_t duration) const {
    rust_x86_idle_states_record_duration(this, duration);
  }

  // Updates the mask of valid C-states.
  void SetStateMask(uint32_t mask) const { rust_x86_idle_states_set_state_mask(this, mask); }

 private:
  [[maybe_unused]] OpaqueStorage<312, 8> storage_{};
};

static_assert(sizeof(X86IdleStates) == 312);
static_assert(alignof(X86IdleStates) == 8);

#endif  // ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_IDLE_STATES_H_
