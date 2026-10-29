// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Common data types for emulator engines. These are implementation-agnostic
//! data types, not the engine-specific command types that each engine will define for itself. These
//! types will be directly deserializable from the PBM, and converted into engine-specific types at
//! runtime.

use crate::enumerations::{EngineConsoleType, ShowDetail, SnapshotInfo};
use crate::input_types::InputStep;
use emulator_instance::{EmulatorConfiguration, EmulatorInstanceData, EngineState, EngineType};
use ffx_config::EnvironmentContext;
pub use ffx_futures::LocalBoxFuture as BoxFuture;
use fho::{Result, user_error};
use std::backtrace::Backtrace;
use std::path::Path;
use std::process::Command;

pub trait EmulatorEngine: Send + Sync {
    /// Expose the EmulatorInstanceData object. This stores all the instance data for the
    /// emulator instance. There is no "mut" getter to avoid changing the contents without
    /// coordinating via the concrete type.
    fn get_instance_data(&self) -> &EmulatorInstanceData {
        let bt = Backtrace::force_capture();
        unimplemented!(
            "These default trait implementations are to allow for easier testing and \
            should not be used directly\n{:?}",
            bt
        )
    }

    /// Prepare an emulator to run. This function shouldn't require any additional configuration as
    /// input, since the object should be fully configured by the EngineBuilder. At its most basic,
    /// this should assemble the command-line to invoke the emulator binary. If support processes
    /// are required, or temporary files need to be written to disk, that would be handled here.
    fn stage(&mut self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async {
            let bt = Backtrace::force_capture();
            unimplemented!(
                "These default trait implementations are to allow for easier testing and \
                should not be used directly\n{:?}",
                bt
            )
        })
    }

    /// Given a staged emulator instance, start it running. When the function returns, either the
    /// emulator will be running independently, or an error will be sent back explaining the failure.
    fn start<'a>(
        &'a mut self,
        _context: &'a EnvironmentContext,
        mut _emulator_cmd: Command,
    ) -> BoxFuture<'a, Result<i32>> {
        Box::pin(async {
            let bt = Backtrace::force_capture();
            unimplemented!(
                "These default trait implementations are to allow for easier testing and \
                should not be used directly\n{:?}",
                bt
            )
        })
    }

    /// Shut down a running emulator instance. The engine should have been instantiated from a saved
    /// and serialized instance, so no additional initialization should be needed. This function
    /// will terminate a running emulator instance, which will be specified on the command line.
    /// The QEMU-based engines (femu and qemu) also wait for the emulator's process to exit, so
    /// that the instance can be started again as soon as this returns. It may return an error if
    /// the instance doesn't exist or the shut down fails, but should succeed if it's no longer
    /// running or gets successfully shut down.
    fn stop(&mut self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async {
            let bt = Backtrace::force_capture();
            unimplemented!(
                "These default trait implementations are to allow for easier testing and \
                should not be used directly\n{:?}",
                bt
            )
        })
    }

    /// Takes a screenshot of the emulator's primary display and saves it to the given path.
    ///
    /// On success, returns [`Ok(())`] and creates a valid PNG file at `path`.
    /// If a file already exists at `path`, it will be overwritten.
    ///
    /// The output is a PNG file in RGB color space; it has the same resolution
    /// as the emulator's primary display device.
    ///
    /// # Panics
    ///
    /// This method will panic if:
    /// * The emulator is not in the [`EngineState::Running`] state.
    /// * A virtual display is not enabled in the emulator configuration.
    /// * The output path is a directory.
    /// * The parent directory of `path` is not writable.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying capture, conversion, or file I/O fails.
    fn screenshot<'a>(&'a mut self, _path: &'a Path) -> BoxFuture<'a, Result<()>> {
        Box::pin(async {
            let bt = Backtrace::force_capture();
            unimplemented!(
                "These default trait implementations are to allow for easier testing and \
                should not be used directly\n{:?}",
                bt
            )
        })
    }

    /// Saves a snapshot of the running emulator under the given name. A snapshot captures the
    /// state of the guest's memory, CPUs, devices and disk. If a snapshot with the same name
    /// already exists, it is replaced.
    ///
    /// Snapshots are an optional capability: the default implementation, which is used by the
    /// engines that don't support them, returns an error.
    fn save_snapshot<'a>(&'a mut self, _name: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Err(user_error!("Snapshots are not supported by this emulator engine.")) })
    }

    /// Restores the running emulator to the state which was saved in the snapshot with the given
    /// name. The emulator's current state is discarded.
    ///
    /// The default implementation, which is used by the engines that don't support snapshots,
    /// returns an error.
    fn load_snapshot<'a>(&'a mut self, _name: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Err(user_error!("Snapshots are not supported by this emulator engine.")) })
    }

    /// Returns the snapshots which have been saved for the emulator instance: the ones which the
    /// running emulator can be restored to, or which the stopped emulator can be resumed from.
    ///
    /// The default implementation, which is used by the engines that don't support snapshots,
    /// returns an error.
    fn list_snapshots(&mut self) -> BoxFuture<'_, Result<Vec<SnapshotInfo>>> {
        Box::pin(async { Err(user_error!("Snapshots are not supported by this emulator engine.")) })
    }

    /// Deletes the snapshot with the given name, whether the emulator is running or stopped.
    ///
    /// The default implementation, which is used by the engines that don't support snapshots,
    /// returns an error.
    fn delete_snapshot<'a>(&'a mut self, _name: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Err(user_error!("Snapshots are not supported by this emulator engine.")) })
    }

    /// Injects input events into the emulator's virtual input devices.
    ///
    /// Each [`InputStep`] is sent as one QMP `input-send-event` command. The engine waits for
    /// [`InputStep::delay_after`] after each step except the last. Only the QEMU engine supports
    /// this method.
    ///
    /// # Errors
    ///
    /// Errors if the engine doesn't support input, the emulator is not running, or the emulator
    /// rejects an event. The error names the step that failed. The steps before it were delivered.
    fn send_input<'a>(&'a mut self, _steps: &'a [InputStep]) -> BoxFuture<'a, Result<()>> {
        // Input injection is an optional capability, like snapshots. Engines that support it
        // override this method; the others return this error.
        Box::pin(async {
            Err(user_error!("Input injection is not supported by this emulator engine."))
        })
    }

    /// Output the details of an existing emulation instance. The engine should have been
    /// instantiated from a saved and serialized instance, so no additional initialization should be
    /// needed. This function will output text to the terminal describing the instance, its status,
    /// and its configuration. This is an engine-specific output with more detail than `ffx list`.
    fn show(&self, _details: Vec<ShowDetail>) -> Vec<ShowDetail> {
        let bt = Backtrace::force_capture();
        unimplemented!(
            "These default trait implementations are to allow for easier testing and \
            should not be used directly\n{:?}",
            bt
        )
    }

    /// Complete and validate the configuration parameters that have been provided to this engine,
    /// according to the requirements for this engine type. If there are fields which are required,
    /// mutually exclusive, only work when applied in certain combinations, or don't apply to this
    /// engine type, this function will return an error indicating which field(s) and why. It also
    /// returns an error if the engine has already been started. Otherwise, this returns Ok(()).
    fn configure(&mut self) -> Result<()> {
        let bt = Backtrace::force_capture();
        unimplemented!(
            "These default trait implementations are to allow for easier testing and \
            should not be used directly\n{:?}",
            bt
        )
    }

    /// Returns the EngineType used when building this engine. Each engine implementation should
    /// always return the same EngineType.
    fn engine_type(&self) -> EngineType {
        let bt = Backtrace::force_capture();
        unimplemented!(
            "These default trait implementations are to allow for easier testing and \
            should not be used directly\n{:?}",
            bt
        )
    }

    /// Returns true if this instance of the emulator is currently running.
    /// This is checked by using signal to the process id, no consideration is
    /// made for multi threaded access.
    fn is_running(&mut self) -> BoxFuture<'_, bool> {
        Box::pin(async {
            let bt = Backtrace::force_capture();
            unimplemented!(
                "These default trait implementations are to allow for easier testing and \
                should not be used directly\n{:?}",
                bt
            )
        })
    }

    /// Once the engine has been staged, this generates the command line required to start
    /// emulation. There are no side-effects, and the operation can be repeated as necessary.
    fn build_emulator_cmd(&self) -> Command {
        let bt = Backtrace::force_capture();
        unimplemented!(
            "These default trait implementations are to allow for easier testing and \
            should not be used directly\n{:?}",
            bt
        )
    }

    /// Determine the appropriate binary for the target engine, and load its path into the
    /// engine's configuration for future use.
    fn load_emulator_binary(&mut self) -> Result<()> {
        let bt = Backtrace::force_capture();
        unimplemented!(
            "These default trait implementations are to allow for easier testing and \
            should not be used directly\n{:?}",
            bt
        )
    }

    /// Access to the engine's emulator_configuration field.
    fn emu_config(&self) -> &EmulatorConfiguration {
        let bt = Backtrace::force_capture();
        unimplemented!(
            "These default trait implementations are to allow for easier testing and \
            should not be used directly\n{:?}",
            bt
        )
    }

    /// Mutable access to the engine's emulator_configuration field.
    fn emu_config_mut(&mut self) -> &mut EmulatorConfiguration {
        let bt = Backtrace::force_capture();
        unimplemented!(
            "These default trait implementations are to allow for easier testing and \
            should not be used directly\n{:?}",
            bt
        )
    }

    /// Attach the current process to one of the emulator's consoles.
    fn attach(&self, _console: EngineConsoleType) -> Result<()> {
        let bt = Backtrace::force_capture();
        unimplemented!(
            "These default trait implementations are to allow for easier testing and \
            should not be used directly\n{:?}",
            bt
        )
    }

    /// Access to the engine's engine_state field.
    fn engine_state(&self) -> EngineState {
        let bt = Backtrace::force_capture();
        unimplemented!(
            "These default trait implementations are to allow for easier testing and \
            should not be used directly\n{:?}",
            bt
        )
    }
    /// Serialize the contents of the EmulatorEngine to its instance directory on the
    /// local file system.
    fn save_to_disk(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async {
            let bt = Backtrace::force_capture();
            unimplemented!(
                "These default trait implementations are to allow for easier testing and \
                should not be used directly\n{:?}",
                bt
            )
        })
    }
}
