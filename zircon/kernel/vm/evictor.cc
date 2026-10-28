// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <vm/evictor.h>

extern "C" {

void rust_evictor_init(Evictor* evictor);
void rust_evictor_destroy(Evictor* evictor);

}  // extern "C"

Evictor::Evictor() { rust_evictor_init(this); }

Evictor::~Evictor() { rust_evictor_destroy(this); }
