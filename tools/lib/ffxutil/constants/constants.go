// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package constants

const (
	// General ffx error.
	CommandFailedMsg       = "FFX command failed"
	ClientChannelClosedMsg = "There was an internal error running tests: Fidl(ClientChannelClosed { status: Status(PEER_CLOSED)"
	// LINT.IfChange(timeout_reaching_target)
	TimeoutReachingTargetMsg = "Timeout attempting to reach target"
	// LINT.ThenChange(
	//     //src/developer/ffx/lib/target/src/lib.rs:timeout_reaching_target,
	//     //src/testing/end_to_end/honeydew/honeydew/transports/ffx/ffx.py:timeout_reaching_target,
	//     //tools/testing/tefmocheck/check-configs.yaml:timeout_reaching_target
	// )
	UnableToResolveAddressMsg = "unable to resolve address for target"
)
