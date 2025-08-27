// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <stdint.h>

#include "third_party/github.com/ridiculousfish/libdivide/include/libdivide.h"

// Divides a 64-bit number `n` by a constant `d`, and returns the quotient
// and remainder.
//
// This function uses libdivide to ensure an efficient operation.
template <uint64_t d>
static inline uint64_t div_rem(uint64_t n, uint64_t* rem) {
  constexpr libdivide::branchfree_divider<uint64_t> divider(d);
  uint64_t q = n / divider;
  *rem = n - q * d;
  return q;
}

static __attribute__((used)) uint64_t udiv64(uint64_t dividend, uint64_t divisor,
                                             uint64_t* remainder) asm("udiv64");

// Specific constant to improve the division
// Used for the conversion between nanoseconds and seconds.
constexpr uint64_t k1e9 = 1'000'000'000;
// Used to convert cpu ticks to nanoseconds.
constexpr uint64_t k_19200000 = 19'200'000;

// Performs 64-bit integer division.
//
// This function is a standard division algorithm that is used when a magic
// multiplier is not available for the given divisor.
static uint64_t udiv64(uint64_t dividend, uint64_t divisor, uint64_t* remainder) {
  switch (divisor) {
    case 1:
      *remainder = 0;
      return dividend;
    case k1e9:
      return div_rem<k1e9>(dividend, remainder);
    case k_19200000:
      return div_rem<k_19200000>(dividend, remainder);
  }

  uint64_t quotient = 0;
  uint32_t count = 1;

  // Shortcut special cases
  if (divisor == 0) {
    // div-by-0.
    return UINT64_MAX;
  }
  if (divisor > dividend) {
    *remainder = dividend;
    return 0;
  }
  if (divisor == dividend) {
    *remainder = 0;
    return 1;
  }

  // If not, we want to move the divisor as far left as we can,
  // and then compare against an accumulator of the dividend's left
  // bits. If the accumulator is larger, then we subtract it out, set
  // the quotient bit and keep going.  The quotient bit can be set
  // and shifted because it can't be larger than the divisor was.
  *remainder = 0;

  // Find the first bit in the divisor and shift it left,
  // so we can test at each ste.
  while ((divisor >> 63) == 0) {
    count++;
    divisor <<= 1;
  }
  *remainder = dividend;
  while (count) {
    quotient <<= 1;  // shift here so our last bit is available.
    if (*remainder >= divisor) {
      quotient |= 1;
      *remainder -= divisor;
    }
    count -= 1;
    divisor >>= 1;
  }
  return quotient;
}

extern "C" __attribute__((naked)) void __aeabi_uldivmod() {
  asm("push {r11, lr}");
  asm("sub  sp, sp, #16");
  asm("add  r12, sp, #8");
  asm("str  r12, [sp]");
  asm("bl   udiv64");
  asm("ldr  r2, [sp, #8]");
  asm("ldr  r3, [sp, #12]");
  asm("add  sp, sp, #16");
  asm("pop  {r11, lr}");
  asm("bx   lr");
}
