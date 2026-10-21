// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
#include <arch.h>
#include <inttypes.h>
#include <stdio.h>

#include <arch/regs.h>

void PrintFrame(const iframe_t& frame, FILE* f) {
  fprintf(f, "iframe %p:\n", &frame);
  fprintf(f, "x0  %#18" PRIx64 " x1  %#18" PRIx64 " x2  %#18" PRIx64 " x3  %#18" PRIx64 "\n",
          frame.r[0], frame.r[1], frame.r[2], frame.r[3]);
  fprintf(f, "x4  %#18" PRIx64 " x5  %#18" PRIx64 " x6  %#18" PRIx64 " x7  %#18" PRIx64 "\n",
          frame.r[4], frame.r[5], frame.r[6], frame.r[7]);
  fprintf(f, "x8  %#18" PRIx64 " x9  %#18" PRIx64 " x10 %#18" PRIx64 " x11 %#18" PRIx64 "\n",
          frame.r[8], frame.r[9], frame.r[10], frame.r[11]);
  fprintf(f, "x12 %#18" PRIx64 " x13 %#18" PRIx64 " x14 %#18" PRIx64 " x15 %#18" PRIx64 "\n",
          frame.r[12], frame.r[13], frame.r[14], frame.r[15]);
  fprintf(f, "x16 %#18" PRIx64 " x17 %#18" PRIx64 " x18 %#18" PRIx64 " x19 %#18" PRIx64 "\n",
          frame.r[16], frame.r[17], frame.r[18], frame.r[19]);
  fprintf(f, "x20 %#18" PRIx64 " x21 %#18" PRIx64 " x22 %#18" PRIx64 " x23 %#18" PRIx64 "\n",
          frame.r[20], frame.r[21], frame.r[22], frame.r[23]);
  fprintf(f, "x24 %#18" PRIx64 " x25 %#18" PRIx64 " x26 %#18" PRIx64 " x27 %#18" PRIx64 "\n",
          frame.r[24], frame.r[25], frame.r[26], frame.r[27]);
  fprintf(f, "x28 %#18" PRIx64 " x29 %#18" PRIx64 " lr  %#18" PRIx64 " usp %#18" PRIx64 "\n",
          frame.r[28], frame.r[29], frame.lr, frame.usp);
  fprintf(f, "elr  %#18" PRIx64 "\n", frame.elr);
  fprintf(f, "spsr %#18" PRIx64 "\n", frame.spsr);
}
