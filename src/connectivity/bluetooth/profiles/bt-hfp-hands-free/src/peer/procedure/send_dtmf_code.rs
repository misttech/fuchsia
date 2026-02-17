// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Result, format_err};
use at_commands as at;

use super::{CommandFromHf, Procedure, ProcedureInput, ProcedureOutput, at_cmd, at_ok};

use crate::peer::procedure_manipulated_state::ProcedureManipulatedState;

/// HFP v1.8 §4.28
#[derive(Debug, PartialEq)]
pub enum SendDtmfCodeProcedure {
    Started,
    WaitingForOk,
    Terminated,
}

impl SendDtmfCodeProcedure {
    pub fn new() -> Self {
        Self::Started
    }
}

impl Procedure<ProcedureInput, ProcedureOutput> for SendDtmfCodeProcedure {
    fn name(&self) -> &str {
        "Send DTMF Code Procedure"
    }

    fn transition(
        &mut self,
        _state: &mut ProcedureManipulatedState,
        input: ProcedureInput,
    ) -> Result<Vec<ProcedureOutput>> {
        match (&self, input) {
            (
                Self::Started,
                ProcedureInput::CommandFromHf(CommandFromHf::SendDtmfCode { code }),
            ) => {
                *self = Self::WaitingForOk;
                Ok(vec![at_cmd!(Vts { code: code.into() })])
            }
            (Self::WaitingForOk, at_ok!()) => {
                *self = Self::Terminated;
                Ok(vec![])
            }
            (_, input) => {
                return Err(format_err!(
                    "Received invalid response {:?} during a send DTMF code procedure in state {:?}.",
                    input,
                    self
                ));
            }
        }
    }

    fn is_terminated(&self) -> bool {
        *self == Self::Terminated
    }
}

// TODO(b/484091228): Add tests.
