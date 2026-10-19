// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <zircon/features.h>
#include <zircon/syscalls.h>

#include <array>
#include <atomic>
#include <format>
#include <thread>
#include <vector>

#include <zxtest/zxtest.h>

namespace {

[[gnu::noinline]] double fpu_test_loop(int ex_loops, int factor) {
  double ev[4] = {1.0, -1.0, -1.0, -1.0};
  double t = 0.49999975;

  const int inner = 120 * factor;

  for (int ix = 0; ix < ex_loops; ix++) {
    for (int iz = 0; iz < inner; iz++) {
      ev[0] = (ev[0] + ev[1] + ev[2] - ev[3]) * t;
      ev[1] = (ev[0] + ev[1] - ev[2] + ev[3]) * t;
      ev[2] = (ev[0] - ev[1] + ev[2] + ev[3]) * t;
      ev[3] = (-ev[0] + ev[1] + ev[2] + ev[3]) * t;
    }
    t = 1.0 - t;
  }
  return ev[3];
}

// This is a floating point computation that takes longer than one
// quantum. It is meant to test the code that handles saving and
// restoring the floating point registers, in particular for ARM.
// For reference, with the parameters below it takes about 500ms
// to complete in the arm-qemu-kvm bots.
TEST(RegisterContextSwitchTests, FpuLongComputeLoop) {
  double result = fpu_test_loop(5, 100);
  std::string result_str = std::format("{:3.18f}", result);
  ASSERT_STREQ(result_str, "-1.123982548697285422");
}

#ifdef __aarch64__
TEST(RegisterContextSwitchTests, SveContextSwitch) {
  uint32_t features = 0;
  zx_status_t status = zx_system_get_features(ZX_FEATURE_KIND_CPU, &features);
  ASSERT_OK(status);

  if ((features & ZX_ARM64_FEATURE_ISA_SVE) == 0) {
    ZXTEST_SKIP("FEAT_SVE not detected");
    return;
  }

  // Create 32 threads to test SVE context switching that load a unique value in each Z-register,
  // P-register, and FFR, and then sleep for 1ms to trigger a context switch. This tests the
  // kernel's ability to save and restore the SVE registers during context switching.
  std::atomic<bool> failed[32]{};
  std::vector<std::thread> threads;
  threads.reserve(32);
  for (uint32_t t = 0; t < 32; t++) {
    threads.emplace_back([t, &failed] {
      for (uint32_t iter = 0; iter < 1000; iter++) {
        alignas(16) std::array<std::array<uint8_t, 16>, 32> z_out{};
        std::array<uint16_t, 16> p_in{};
        std::array<uint16_t, 16> p_out{};
        uint32_t seed = iter + (t * 100);

        for (size_t r = 0; r < p_in.size(); ++r) {
          p_in[r] =
              static_cast<uint16_t>((seed * 257u) ^ (static_cast<uint32_t>(r) * 0x1021u) ^ 0x5a5au);
        }
        // FFR must be a monotonically decreasing sequence of 1s from bit 0.
        uint16_t ffr_in = static_cast<uint16_t>((1u << ((seed + t) % 17u)) - 1u);
        uint16_t ffr_out = 0;

        // Generate unique values for each byte in each Z-register using the SVE 'index'
        // instruction, load P0-P15 and FFR with unique values per thread and iteration,
        // force a context switch by sleeping for 1ms via zx_nanosleep, and then immediately
        // save the P-registers, FFR, and Z-registers to memory.
        uint32_t z_seed = seed;
        uint8_t* ptr = z_out[0].data();
        register zx_instant_mono_t x0 __asm__("x0") = zx_deadline_after(ZX_MSEC(1));
        __asm__ volatile(
            ".arch_extension sve\n"
            "index   z0.b, %w[seed], #1\n"
            "add     %w[seed], %w[seed], #1\n"
            "index   z1.b, %w[seed], #1\n"
            "add     %w[seed], %w[seed], #1\n"
            "index   z2.b, %w[seed], #1\n"
            "add     %w[seed], %w[seed], #1\n"
            "index   z3.b, %w[seed], #1\n"
            "add     %w[seed], %w[seed], #1\n"
            "index   z4.b, %w[seed], #1\n"
            "add     %w[seed], %w[seed], #1\n"
            "index   z5.b, %w[seed], #1\n"
            "add     %w[seed], %w[seed], #1\n"
            "index   z6.b, %w[seed], #1\n"
            "add     %w[seed], %w[seed], #1\n"
            "index   z7.b, %w[seed], #1\n"
            "add     %w[seed], %w[seed], #1\n"
            "index   z8.b, %w[seed], #1\n"
            "add     %w[seed], %w[seed], #1\n"
            "index   z9.b, %w[seed], #1\n"
            "add     %w[seed], %w[seed], #1\n"
            "index   z10.b, %w[seed], #1\n"
            "add     %w[seed], %w[seed], #1\n"
            "index   z11.b, %w[seed], #1\n"
            "add     %w[seed], %w[seed], #1\n"
            "index   z12.b, %w[seed], #1\n"
            "add     %w[seed], %w[seed], #1\n"
            "index   z13.b, %w[seed], #1\n"
            "add     %w[seed], %w[seed], #1\n"
            "index   z14.b, %w[seed], #1\n"
            "add     %w[seed], %w[seed], #1\n"
            "index   z15.b, %w[seed], #1\n"
            "add     %w[seed], %w[seed], #1\n"
            "index   z16.b, %w[seed], #1\n"
            "add     %w[seed], %w[seed], #1\n"
            "index   z17.b, %w[seed], #1\n"
            "add     %w[seed], %w[seed], #1\n"
            "index   z18.b, %w[seed], #1\n"
            "add     %w[seed], %w[seed], #1\n"
            "index   z19.b, %w[seed], #1\n"
            "add     %w[seed], %w[seed], #1\n"
            "index   z20.b, %w[seed], #1\n"
            "add     %w[seed], %w[seed], #1\n"
            "index   z21.b, %w[seed], #1\n"
            "add     %w[seed], %w[seed], #1\n"
            "index   z22.b, %w[seed], #1\n"
            "add     %w[seed], %w[seed], #1\n"
            "index   z23.b, %w[seed], #1\n"
            "add     %w[seed], %w[seed], #1\n"
            "index   z24.b, %w[seed], #1\n"
            "add     %w[seed], %w[seed], #1\n"
            "index   z25.b, %w[seed], #1\n"
            "add     %w[seed], %w[seed], #1\n"
            "index   z26.b, %w[seed], #1\n"
            "add     %w[seed], %w[seed], #1\n"
            "index   z27.b, %w[seed], #1\n"
            "add     %w[seed], %w[seed], #1\n"
            "index   z28.b, %w[seed], #1\n"
            "add     %w[seed], %w[seed], #1\n"
            "index   z29.b, %w[seed], #1\n"
            "add     %w[seed], %w[seed], #1\n"
            "index   z30.b, %w[seed], #1\n"
            "add     %w[seed], %w[seed], #1\n"
            "index   z31.b, %w[seed], #1\n"
            "ldr     p0, [%[ffr_in]]\n"
            "wrffr   p0.b\n"
            "ldr     p0, [%[p_in], #0, mul vl]\n"
            "ldr     p1, [%[p_in], #1, mul vl]\n"
            "ldr     p2, [%[p_in], #2, mul vl]\n"
            "ldr     p3, [%[p_in], #3, mul vl]\n"
            "ldr     p4, [%[p_in], #4, mul vl]\n"
            "ldr     p5, [%[p_in], #5, mul vl]\n"
            "ldr     p6, [%[p_in], #6, mul vl]\n"
            "ldr     p7, [%[p_in], #7, mul vl]\n"
            "ldr     p8, [%[p_in], #8, mul vl]\n"
            "ldr     p9, [%[p_in], #9, mul vl]\n"
            "ldr     p10, [%[p_in], #10, mul vl]\n"
            "ldr     p11, [%[p_in], #11, mul vl]\n"
            "ldr     p12, [%[p_in], #12, mul vl]\n"
            "ldr     p13, [%[p_in], #13, mul vl]\n"
            "ldr     p14, [%[p_in], #14, mul vl]\n"
            "ldr     p15, [%[p_in], #15, mul vl]\n"
            "bl      zx_nanosleep\n"
            "str     p0, [%[p_out], #0, mul vl]\n"
            "str     p1, [%[p_out], #1, mul vl]\n"
            "str     p2, [%[p_out], #2, mul vl]\n"
            "str     p3, [%[p_out], #3, mul vl]\n"
            "str     p4, [%[p_out], #4, mul vl]\n"
            "str     p5, [%[p_out], #5, mul vl]\n"
            "str     p6, [%[p_out], #6, mul vl]\n"
            "str     p7, [%[p_out], #7, mul vl]\n"
            "str     p8, [%[p_out], #8, mul vl]\n"
            "str     p9, [%[p_out], #9, mul vl]\n"
            "str     p10, [%[p_out], #10, mul vl]\n"
            "str     p11, [%[p_out], #11, mul vl]\n"
            "str     p12, [%[p_out], #12, mul vl]\n"
            "str     p13, [%[p_out], #13, mul vl]\n"
            "str     p14, [%[p_out], #14, mul vl]\n"
            "str     p15, [%[p_out], #15, mul vl]\n"
            "rdffr   p0.b\n"
            "str     p0, [%[ffr_out]]\n"
            "ptrue   p0.b\n"
            "st1b    z0.b, p0, [%[ptr]]\n"
            "add     %[ptr], %[ptr], #16\n"
            "st1b    z1.b, p0, [%[ptr]]\n"
            "add     %[ptr], %[ptr], #16\n"
            "st1b    z2.b, p0, [%[ptr]]\n"
            "add     %[ptr], %[ptr], #16\n"
            "st1b    z3.b, p0, [%[ptr]]\n"
            "add     %[ptr], %[ptr], #16\n"
            "st1b    z4.b, p0, [%[ptr]]\n"
            "add     %[ptr], %[ptr], #16\n"
            "st1b    z5.b, p0, [%[ptr]]\n"
            "add     %[ptr], %[ptr], #16\n"
            "st1b    z6.b, p0, [%[ptr]]\n"
            "add     %[ptr], %[ptr], #16\n"
            "st1b    z7.b, p0, [%[ptr]]\n"
            "add     %[ptr], %[ptr], #16\n"
            "st1b    z8.b, p0, [%[ptr]]\n"
            "add     %[ptr], %[ptr], #16\n"
            "st1b    z9.b, p0, [%[ptr]]\n"
            "add     %[ptr], %[ptr], #16\n"
            "st1b    z10.b, p0, [%[ptr]]\n"
            "add     %[ptr], %[ptr], #16\n"
            "st1b    z11.b, p0, [%[ptr]]\n"
            "add     %[ptr], %[ptr], #16\n"
            "st1b    z12.b, p0, [%[ptr]]\n"
            "add     %[ptr], %[ptr], #16\n"
            "st1b    z13.b, p0, [%[ptr]]\n"
            "add     %[ptr], %[ptr], #16\n"
            "st1b    z14.b, p0, [%[ptr]]\n"
            "add     %[ptr], %[ptr], #16\n"
            "st1b    z15.b, p0, [%[ptr]]\n"
            "add     %[ptr], %[ptr], #16\n"
            "st1b    z16.b, p0, [%[ptr]]\n"
            "add     %[ptr], %[ptr], #16\n"
            "st1b    z17.b, p0, [%[ptr]]\n"
            "add     %[ptr], %[ptr], #16\n"
            "st1b    z18.b, p0, [%[ptr]]\n"
            "add     %[ptr], %[ptr], #16\n"
            "st1b    z19.b, p0, [%[ptr]]\n"
            "add     %[ptr], %[ptr], #16\n"
            "st1b    z20.b, p0, [%[ptr]]\n"
            "add     %[ptr], %[ptr], #16\n"
            "st1b    z21.b, p0, [%[ptr]]\n"
            "add     %[ptr], %[ptr], #16\n"
            "st1b    z22.b, p0, [%[ptr]]\n"
            "add     %[ptr], %[ptr], #16\n"
            "st1b    z23.b, p0, [%[ptr]]\n"
            "add     %[ptr], %[ptr], #16\n"
            "st1b    z24.b, p0, [%[ptr]]\n"
            "add     %[ptr], %[ptr], #16\n"
            "st1b    z25.b, p0, [%[ptr]]\n"
            "add     %[ptr], %[ptr], #16\n"
            "st1b    z26.b, p0, [%[ptr]]\n"
            "add     %[ptr], %[ptr], #16\n"
            "st1b    z27.b, p0, [%[ptr]]\n"
            "add     %[ptr], %[ptr], #16\n"
            "st1b    z28.b, p0, [%[ptr]]\n"
            "add     %[ptr], %[ptr], #16\n"
            "st1b    z29.b, p0, [%[ptr]]\n"
            "add     %[ptr], %[ptr], #16\n"
            "st1b    z30.b, p0, [%[ptr]]\n"
            "add     %[ptr], %[ptr], #16\n"
            "st1b    z31.b, p0, [%[ptr]]\n"
            ".arch_extension nosve\n"
            : [seed] "+r"(z_seed), [ptr] "+r"(ptr), [x0] "+r"(x0)
            : [p_in] "r"(p_in.data()), [ffr_in] "r"(&ffr_in), [p_out] "r"(p_out.data()),
              [ffr_out] "r"(&ffr_out)
            : "z0", "z1", "z2", "z3", "z4", "z5", "z6", "z7", "z8", "z9", "z10", "z11", "z12",
              "z13", "z14", "z15", "z16", "z17", "z18", "z19", "z20", "z21", "z22", "z23", "z24",
              "z25", "z26", "z27", "z28", "z29", "z30", "z31", "p0", "p1", "p2", "p3", "p4", "p5",
              "p6", "p7", "p8", "p9", "p10", "p11", "p12", "p13", "p14", "p15", "x1", "x2", "x3",
              "x4", "x5", "x6", "x7", "x8", "x9", "x10", "x11", "x12", "x13", "x14", "x15", "x16",
              "x17", "lr", "cc", "memory");

        for (size_t r = 0; r < z_out.size(); ++r) {
          for (size_t i = 0; i < z_out[r].size(); ++i) {
            uint8_t expected = static_cast<uint8_t>(seed + r + i);
            if (z_out[r][i] != expected) {
              failed[t] = true;
            }
          }
        }
        for (size_t r = 0; r < p_out.size(); ++r) {
          if (p_out[r] != p_in[r]) {
            failed[t] = true;
          }
        }
        if (ffr_out != ffr_in) {
          failed[t] = true;
        }
      }
    });
  }

  for (auto& t : threads) {
    t.join();
  }

  for (int i = 0; i < 32; i++) {
    EXPECT_FALSE(failed[i], "Thread %d failed SVE verification", i);
  }
}
#endif

}  // namespace
