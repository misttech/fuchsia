// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Result;
use flex_fuchsia_driver_development as fdd;
use flex_fuchsia_driver_registrar as fdr;
use flex_fuchsia_test_manager as ftm;
use std::future::Future;

pub trait DriverConnector {
    fn get_driver_development_proxy(
        &self,
        select: bool,
    ) -> impl Future<Output = Result<fdd::ManagerProxy>> + Send;
    fn get_driver_registrar_proxy(
        &self,
        select: bool,
    ) -> impl Future<Output = Result<fdr::DriverRegistrarProxy>> + Send;
    fn get_suite_runner_proxy(&self) -> impl Future<Output = Result<ftm::SuiteRunnerProxy>> + Send;
}
