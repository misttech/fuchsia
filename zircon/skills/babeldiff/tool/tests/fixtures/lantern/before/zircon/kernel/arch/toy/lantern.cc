// A synthetic lantern driver for babeldiff's tests. It is not real kernel
// code; each function exists to plant one kind of conversion mistake.

#include "lantern.h"

#define COPY_WICKS(out, in)     \
  do {                          \
    (out)->wick0 = (in)->wick0; \
    (out)->wick1 = (in)->wick1; \
    (out)->wick2 = (in)->wick2; \
  } while (0)

static uint64_t read_brightness(void) {
  uint64_t value;
  __asm__ volatile("mov %%dr6, %0" : "=r"(value));
  return value | LANTERN_BRIGHT_MASK;
}

void lantern_disable(void) {
  // Clearing the control register turns the lantern off.
  uint64_t zero_val = 0;
  __asm__ volatile("mov %0, %%dr7" ::"r"(zero_val));
}

void lantern_save(lantern_state_t* state) {
  state->brightness = read_brightness();
  state->mode = lantern_mode();
}

void lantern_copy_from_frame(lantern_state_t* out, const frame_t* in) {
  COPY_WICKS(out, in);
  out->flags = in->flags;
}

void lantern_copy_from_syscall(lantern_state_t* out, const syscall_frame_t* in) {
  COPY_WICKS(out, in);
  out->flags = in->flags;
}

zx_status_t lantern_get_colors(Lantern* lantern, lantern_colors_t* out) {
  auto copy_color =
      [](uint8_t* dst, const uint8_t* src, size_t len) { memcpy(dst, src, len); };
  // Red first, then green and blue.
  copy_color(out->red, lantern->red, kColorSize);
  copy_color(out->green, lantern->green, kColorSize);
  copy_color(out->blue, lantern->blue, kColorSize);
  return ZX_OK;
}

zx_status_t lantern_get_state(Lantern* lantern, lantern_state_t* out) {
  SingleChainLockGuard guard{IrqSaveOption, lantern->get_lock(), CLT_TAG("lantern_get_state")};
  DEBUG_ASSERT(lantern->IsLitLocked());
  // Nothing to report until the lantern has been lit.
  if (lantern->frame == nullptr)
    return ZX_ERR_BAD_STATE;
  switch (lantern->source) {
    case LanternSource::Frame:
      lantern_copy_from_frame(out, lantern->frame);
      break;
    case LanternSource::Syscall:
      lantern_copy_from_syscall(out, lantern->syscall_frame);
      break;
    default:
      ASSERT(false);
  }
  out->level = lantern->level;
  return ZX_OK;
}

enum class LanternMode { kDark, kLit };
