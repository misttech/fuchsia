// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub use pw_bluetooth_att_emb as att;
pub use pw_bluetooth_hci_commands_emb as hci_commands;
pub use pw_bluetooth_hci_common_emb as hci_common;
pub use pw_bluetooth_hci_h4_emb as hci_h4;
pub use pw_bluetooth_l2cap_frames_emb as l2cap_frames;

#[cfg(test)]
mod tests {
    use super::*;
    use att::{AttErrorRsp, AttErrorRspMut, AttOpcode, ErrorCode};
    use hci_commands::ResetCommand;
    use hci_common::OpCode;

    #[test]
    fn test_reset_command() {
        let buffer = [0x03u8, 0x0cu8, 0x00u8];
        let view = ResetCommand::new(&buffer[..]);
        assert_eq!(view.header().opcode().try_read().unwrap(), OpCode::RESET);
        assert_eq!(view.header().parameter_total_size().try_read().unwrap(), 0);
    }

    #[test]
    fn test_error_rsp() {
        let mut buffer = [0u8; 5];
        let mut view = AttErrorRspMut::new(&mut buffer[..]);
        view.attribute_opcode().try_write(AttOpcode::ATT_ERROR_RSP).unwrap();
        view.request_opcode_in_error_uint().try_write(u8::from(AttOpcode::ATT_READ_REQ)).unwrap();
        view.attribute_handle().try_write(0x1234).unwrap();
        view.error_code().try_write(ErrorCode::READ_NOT_PERMITTED).unwrap();

        let read_view = AttErrorRsp::new(&buffer[..]);
        assert_eq!(read_view.attribute_opcode().try_read().unwrap(), AttOpcode::ATT_ERROR_RSP);
        assert_eq!(
            read_view.request_opcode_in_error_uint().try_read().unwrap(),
            u8::from(AttOpcode::ATT_READ_REQ)
        );
        assert_eq!(read_view.attribute_handle().try_read().unwrap(), 0x1234);
        assert_eq!(read_view.error_code().try_read().unwrap(), ErrorCode::READ_NOT_PERMITTED);
    }
}
