// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! ARM CoreSight component identification and ROM table traversal, mirroring
//! `<dev/coresight/component.h>` and `<dev/coresight/rom_table.h>`.

#![cfg_attr(not(feature = "std"), no_std)]

pub mod component;
pub mod rom_table;

pub use component::{
    ClassId, ComponentIdRegister, DeviceAffinityRegister, DeviceArchRegister, DeviceType,
    DeviceTypeRegister, MINIMUM_COMPONENT_SIZE, MajorType, PeripheralId0Register,
    PeripheralId1Register, PeripheralId2Register, PeripheralId4Register, RegisterIo, arm,
    class_id_to_string, device_type_to_string, get_designer, get_part_id,
};
pub use rom_table::{
    Class0x1RomEntry, Class0x9Rom32BitEntry, Class0x9Rom64BitEntry, Class0x9RomDeviceIdRegister,
    Class0x9RomEntryFormat, RomTable, WalkError,
};
