// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/arch/x86/system.h>

#include <hwreg/asm.h>

// Generate the <lib/arch/x86/system-asm.h> header that assembly code uses.
// It provides bit masks for the main x86 system registers.

int main(int argc, char** argv) {
  return hwreg::AsmHeader()  //
      .Register<arch::X86Cr0>("X86_CR0_")
      .Main(argc, argv);
}
