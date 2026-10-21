// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/arch/asm.h>

#ifdef __arm__
.thumb
.macro ret
  bx lr
.endm
#endif

.function NoCallGraph, global
  ret
.end_function

.function DirectNoCalls, global, callgraph=direct
  ret
.end_function

.function DirectCalls, global, callgraph=direct
  callgraph.call Callee1
  nop
  callgraph.call Callee2
  nop
  callgraph.tail Callee3
.end_function

.function DirectAndIndirectCalls, global, callgraph=direct
  callgraph.call Callee1
  nop
  .llvm.callgraph.indirect_callee_type 0x1234
  nop
  .llvm.callgraph.indirect_callee_type 0x5678, 0xdeadbeeffeedface
  ret
.end_function

.function Indirectable, global, callgraph=0xfeedface
  callgraph.call Callee1
  ret
.end_function
