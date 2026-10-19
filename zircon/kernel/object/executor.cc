// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <object/executor.h>

extern "C" {
void rust_executor_init(Executor* executor);
}

void Executor::Init() { rust_executor_init(this); }
