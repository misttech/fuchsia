// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// The trace category used for binder related traces.
pub const CATEGORY_STARNIX_BINDER: &'static str = "starnix:binder";

// The name used to track the duration of a local binder ioctl.
pub const NAME_BINDER_IOCTL: &'static str = "BinderIoctl";

// The name used to track handling a thread write in the binder driver.
pub const NAME_HANDLE_THREAD_WRITE: &'static str = "HandleThreadWrite";

// The name used to track binder flow events.
pub const NAME_BINDER_FLOW: &'static str = "BinderFlow";

// The names used to track remote binder operations.
pub const NAME_REMOTE_BINDER_IOCTL: &'static str = "RemoteBinderIoctl";
pub const NAME_REMOTE_BINDER_IOCTL_SEND_WORK: &'static str = "RemoteBinderIoctlSendWork";
pub const NAME_REMOTE_BINDER_IOCTL_FIDL_REPLY: &'static str = "RemoteBinderIoctlFidlReply";
pub const NAME_REMOTE_BINDER_IOCTL_WORKER_PROCESS: &'static str = "RemoteBinderIoctlWorkerProcess";
