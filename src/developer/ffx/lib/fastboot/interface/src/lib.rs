// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod fastboot_interface;
pub mod fastboot_proxy;
pub mod interface_factory;
pub mod stream;
pub mod util;

pub mod test {
    use super::fastboot_interface::{
        Fastboot, FastbootError, FastbootInterface, RebootEvent, UploadProgress, Variable,
    };
    use super::stream::StreamCommand;
    use chrono::Duration;
    use ffx_futures::BoxFuture;
    use std::collections::HashMap;
    use std::default::Default;
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc::Sender;

    #[derive(Default, Debug)]
    pub struct FakeServiceCommands {
        pub staged_files: Vec<String>,
        /// All transmitted OEM commands; each will have the "oem " prefix automatically inserted
        /// to match real fastboot behavior.
        pub oem_commands: Vec<String>,
        pub bootloader_reboots: usize,
        pub boots: usize,
        pub continue_boots: usize,
        pub set_actives: Vec<String>,
        /// Variable => (Option(Value), Call Count)
        variables: HashMap<String, (Option<String>, u32)>,
    }

    impl FakeServiceCommands {
        /// Sets the provided variable to the given value preserving the past
        /// call count.
        pub fn set_var(&mut self, var: String, value: String) {
            match self.variables.get_mut(&var) {
                Some((old_val, _)) => *old_val = Some(value),
                None => {
                    self.variables.insert(var, (Some(value), 0));
                }
            }
        }

        /// Returns (variable_set, call_count)
        pub fn get_var_call_count(&self, var: impl Into<String>) -> (bool, u32) {
            self.variables
                .get(&var.into())
                .map(|(val, count)| (val.is_some(), *count))
                .unwrap_or((false, 0))
        }

        /// Helper for setting multiple variables at once
        pub fn set_multiple_vars(
            &mut self,
            vars: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>,
        ) {
            vars.into_iter().for_each(|(var, val)| self.set_var(var.into(), val.into()))
        }
    }

    #[derive(Debug)]
    pub struct TestFastbootInterface {
        state: Arc<Mutex<FakeServiceCommands>>,
    }

    impl TestFastbootInterface {
        pub fn new(state: Arc<Mutex<FakeServiceCommands>>) -> Self {
            Self { state }
        }
    }

    impl FastbootInterface for TestFastbootInterface {}

    impl Fastboot for TestFastbootInterface {
        fn get_var<'a>(
            &'a mut self,
            name: &'a str,
        ) -> BoxFuture<'a, Result<String, FastbootError>> {
            Box::pin(async move {
                let mut state = self.state.lock().unwrap();
                match state.variables.get_mut(name) {
                    Some((Some(val), count)) => {
                        *count += 1;
                        Ok(val.clone())
                    }
                    Some((None, count)) => {
                        *count += 1;
                        Err(FastbootError::GetVariableError {
                            variable: name.to_string(),
                            message: "Variable not found".to_string(),
                        })
                    }
                    None => {
                        state.variables.insert(name.to_string(), (None, 1));
                        Err(FastbootError::GetVariableError {
                            variable: name.to_string(),
                            message: "Variable not found".to_string(),
                        })
                    }
                }
            })
        }

        fn get_all_vars(
            &mut self,
            listener: Sender<Variable>,
        ) -> BoxFuture<'_, Result<(), FastbootError>> {
            Box::pin(async move {
                let vars: Vec<Variable> = {
                    let state = self.state.lock().unwrap();
                    if state.variables.is_empty() {
                        vec![Variable { name: "test".to_string(), value: "test".to_string() }]
                    } else {
                        state
                            .variables
                            .iter()
                            .filter_map(|(name, (val, _))| {
                                val.as_ref()
                                    .map(|v| Variable { name: name.clone(), value: v.clone() })
                            })
                            .collect()
                    }
                };
                for var in vars {
                    listener.send(var).await.unwrap();
                }
                Ok(())
            })
        }

        fn flash<'a>(
            &'a mut self,
            _partition_name: &'a str,
            _path: &'a str,
            listener: Sender<UploadProgress>,
            _timeout: Duration,
        ) -> BoxFuture<'a, Result<(), FastbootError>> {
            Box::pin(async move {
                listener
                    .send(UploadProgress::OnStarted { size: 1 })
                    .await
                    .map_err(|_| FastbootError::ProgressSendError)?;
                listener
                    .send(UploadProgress::OnProgress { bytes_written: 1 })
                    .await
                    .map_err(|_| FastbootError::ProgressSendError)?;
                listener
                    .send(UploadProgress::OnFinished)
                    .await
                    .map_err(|_| FastbootError::ProgressSendError)?;
                Ok(())
            })
        }

        fn flash_from_reader<'a>(
            &'a mut self,
            _partition_name: &'a str,
            size: u32,
            _reader: &'a mut (dyn std::io::Read + Send),
            listener: Sender<UploadProgress>,
            _timeout: Duration,
        ) -> BoxFuture<'a, Result<(), FastbootError>> {
            Box::pin(async move {
                listener
                    .send(UploadProgress::OnStarted { size: size as u64 })
                    .await
                    .map_err(|_| FastbootError::ProgressSendError)?;
                listener
                    .send(UploadProgress::OnProgress { bytes_written: size as u64 })
                    .await
                    .map_err(|_| FastbootError::ProgressSendError)?;
                listener
                    .send(UploadProgress::OnFinished)
                    .await
                    .map_err(|_| FastbootError::ProgressSendError)?;
                Ok(())
            })
        }

        fn erase<'a>(
            &'a mut self,
            _partition_name: &'a str,
        ) -> BoxFuture<'a, Result<(), FastbootError>> {
            Box::pin(async move { Ok(()) })
        }

        fn boot(&mut self) -> BoxFuture<'_, Result<(), FastbootError>> {
            Box::pin(async move {
                let mut state = self.state.lock().unwrap();
                state.boots += 1;
                Ok(())
            })
        }

        fn reboot(&mut self) -> BoxFuture<'_, Result<(), FastbootError>> {
            Box::pin(async move { Ok(()) })
        }

        fn reboot_bootloader(
            &mut self,
            listener: Sender<RebootEvent>,
        ) -> BoxFuture<'_, Result<(), FastbootError>> {
            Box::pin(async move {
                listener.send(RebootEvent::OnReboot).await.unwrap();
                let mut state = self.state.lock().unwrap();
                state.bootloader_reboots += 1;
                Ok(())
            })
        }

        fn continue_boot(&mut self) -> BoxFuture<'_, Result<(), FastbootError>> {
            Box::pin(async move {
                let mut state = self.state.lock().unwrap();
                state.continue_boots += 1;
                Ok(())
            })
        }

        fn get_staged<'a>(
            &'a mut self,
            _path: &'a str,
        ) -> BoxFuture<'a, Result<(), FastbootError>> {
            Box::pin(async move { Ok(()) })
        }

        fn stage<'a>(
            &'a mut self,
            path: &'a str,
            _listener: Sender<UploadProgress>,
        ) -> BoxFuture<'a, Result<(), FastbootError>> {
            Box::pin(async move {
                let mut state = self.state.lock().unwrap();
                state.staged_files.push(path.to_string());
                Ok(())
            })
        }

        fn set_active<'a>(&'a mut self, slot: &'a str) -> BoxFuture<'a, Result<(), FastbootError>> {
            Box::pin(async move {
                let mut state = self.state.lock().unwrap();
                state.set_actives.push(slot.to_string());
                Ok(())
            })
        }

        fn oem<'a>(&'a mut self, command: &'a str) -> BoxFuture<'a, Result<(), FastbootError>> {
            Box::pin(async move {
                let mut state = self.state.lock().unwrap();
                state.oem_commands.push(format!("oem {}", command));
                Ok(())
            })
        }

        fn stream<'a>(
            &'a mut self,
            _partition_name: &'a str,
            _stream_command: StreamCommand,
            listener: &'a Sender<UploadProgress>,
            _timeout: Duration,
        ) -> BoxFuture<'a, Result<(), FastbootError>> {
            Box::pin(async move {
                listener
                    .send(UploadProgress::OnProgress { bytes_written: 1 })
                    .await
                    .map_err(|_| FastbootError::ProgressSendError)
            })
        }
    }

    pub fn setup() -> (Arc<Mutex<FakeServiceCommands>>, TestFastbootInterface) {
        let state = Arc::new(Mutex::new(FakeServiceCommands::default()));
        let interface = TestFastbootInterface { state: state.clone() };
        (state, interface)
    }
}
