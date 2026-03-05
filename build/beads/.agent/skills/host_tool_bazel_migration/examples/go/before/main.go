// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import "github.com/kr/pretty"

func helloMigration() string {
	return "hello, migration!"
}

func main() {
	pretty.Println(helloMigration())
}
