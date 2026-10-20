// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! ARM CoreSight component identification registers and constants, mirroring
//! `<dev/coresight/component.h>`.

use core::ffi::c_char;
use core::fmt;
use zr::static_assert;

/// Typically components are 4KiB in size; the spec permits them to be larger.
pub const MINIMUM_COMPONENT_SIZE: usize = 4096;

/// Device architecture constants for ARM-designed components.
pub mod arm {
    /// `DEVARCH.ARCHITECT`.
    pub const ARCHITECT: u16 = 0x23b;

    /// `DEVARCH.ARCHID` values.
    pub mod archid {
        /// Cross-Trigger Interface (CTI).
        pub const CTI: u16 = 0x1a14;
        /// Embedded Trace Macrocell (ETM) v3.
        pub const ETM3: u16 = 0x3a13;
        /// Embedded Trace Macrocell (ETM) v4.
        pub const ETM4: u16 = 0x4a13;
        /// Performance Monitors Extension (PMU) v2.
        pub const PMU2: u16 = 0x1a16;
        /// Performance Monitors Extension (PMU) v3.
        pub const PMU3: u16 = 0x2a16;
        /// Class 0x9 ROM Table.
        pub const ROM_TABLE: u16 = 0x0af7;
        /// ARM v8.0-A Core Debug Interface.
        pub const CORE_DEBUG_INTERFACE_8_0A: u16 = 0x6a15;
        /// ARM v8.1-A Core Debug Interface.
        pub const CORE_DEBUG_INTERFACE_8_1A: u16 = 0x7a15;
        /// ARM v8.2-A Core Debug Interface.
        pub const CORE_DEBUG_INTERFACE_8_2A: u16 = 0x8a15;
    }

    /// Peripheral part ID (`PARTID`) values.
    pub mod partid {
        /// Cross-Trigger Interface (SoC400 generation).
        pub const CTI_400: u16 = 0x0906;
        /// Cross-Trigger Interface (SoC600 generation).
        pub const CTI_600: u16 = 0x09ed;
        /// Embedded Trace Buffer (ETB).
        pub const ETB: u16 = 0x0907;
        /// Timestamp Generator.
        pub const TIMESTAMP_GENERATOR: u16 = 0x0101;
        /// Trace Memory Controller (TMC).
        pub const TMC: u16 = 0x0961;
        /// Trace Port Interface Unit (TPIU).
        pub const TPIU: u16 = 0x0912;
        /// Trace Funnel.
        pub const TRACE_FUNNEL: u16 = 0x0908;
        /// Trace Replicator.
        pub const TRACE_REPLICATOR: u16 = 0x0909;
    }
}

/// Abstracts 32-bit and 64-bit register reads and writes at byte offsets from a component base.
pub trait RegisterIo {
    /// Reads a 32-bit register at `offset` bytes.
    fn read32(&mut self, offset: u32) -> u32;

    /// Reads a 64-bit register at `offset` bytes.
    fn read64(&mut self, offset: u32) -> u64;

    /// Writes a 32-bit register at `offset` bytes.
    fn write32(&mut self, val: u32, offset: u32);

    /// Writes a 64-bit register at `offset` bytes.
    fn write64(&mut self, val: u64, offset: u32);
}

impl<T: RegisterIo + ?Sized> RegisterIo for &mut T {
    #[inline]
    fn read32(&mut self, offset: u32) -> u32 {
        (**self).read32(offset)
    }

    #[inline]
    fn read64(&mut self, offset: u32) -> u64 {
        (**self).read64(offset)
    }

    #[inline]
    fn write32(&mut self, val: u32, offset: u32) {
        (**self).write32(val, offset);
    }

    #[inline]
    fn write64(&mut self, val: u64, offset: u32) {
        (**self).write64(val, offset);
    }
}

/// Component class identifier encoded in `CIDR1.CLASS` (`ComponentIdRegister::Class`).
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ClassId {
    /// Generic verification component (`0x0`).
    GenericVerification = 0x0,
    /// Class `0x1` ROM table (`0x1`).
    RomTable0x1 = 0x1,
    /// CoreSight component (`0x9`).
    CoreSight = 0x9,
    /// Peripheral test block (`0xb`).
    PeripheralTestBlock = 0xb,
    /// Generic IP component (`0xe`).
    GenericIp = 0xe,
    /// Non-standard component (`0xf`). For older components without standardized registers.
    NonStandard = 0xf,
}

static_assert!(size_of::<ClassId>() == size_of::<u8>());
static_assert!(align_of::<ClassId>() == align_of::<u8>());

impl TryFrom<u8> for ClassId {
    type Error = u8;

    #[inline]
    fn try_from(val: u8) -> Result<Self, Self::Error> {
        match val {
            0x0 => Ok(Self::GenericVerification),
            0x1 => Ok(Self::RomTable0x1),
            0x9 => Ok(Self::CoreSight),
            0xb => Ok(Self::PeripheralTestBlock),
            0xe => Ok(Self::GenericIp),
            0xf => Ok(Self::NonStandard),
            other => Err(other),
        }
    }
}

impl ClassId {
    /// Returns the human-readable description of this component class.
    #[inline]
    pub const fn as_str(self) -> &'static str {
        class_id_to_string(self as u8)
    }
}

impl fmt::Display for ClassId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Returns the human-readable description of a raw `CIDR1.CLASS` value, or `"unknown"` if the raw
/// value does not match a known [`ClassId`].
#[inline]
pub const fn class_id_to_string(classid: u8) -> &'static str {
    match classid {
        x if x == ClassId::GenericVerification as u8 => "generic verification",
        x if x == ClassId::RomTable0x1 as u8 => "0x1 ROM table",
        x if x == ClassId::CoreSight as u8 => "CoreSight",
        x if x == ClassId::PeripheralTestBlock as u8 => "peripheral test block",
        x if x == ClassId::GenericIp as u8 => "generic IP",
        x if x == ClassId::NonStandard as u8 => "non-standard",
        _ => "unknown",
    }
}

/// [CS] B2.2.1
/// The first component identification register (`CIDR1`).
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct ComponentIdRegister(pub u32);

impl ComponentIdRegister {
    /// Byte offset of `CIDR1` within a component page.
    pub const OFFSET: u32 = 0xff4;

    /// Creates a `ComponentIdRegister` from its raw 32-bit value.
    #[inline]
    pub const fn from_raw(val: u32) -> Self {
        Self(val)
    }

    /// Reads `CIDR1` from `io`.
    #[inline]
    pub fn read_from<Io: RegisterIo>(mut io: Io) -> Self {
        Self(io.read32(Self::OFFSET))
    }

    /// Returns the raw 32-bit register value.
    #[inline]
    pub const fn reg_value(self) -> u32 {
        self.0
    }

    /// Returns the raw 4-bit `classid` field (`[7:4]`).
    ///
    /// Should conventionally be called `class` to match the spec, but that is a reserved
    /// keyword.
    #[inline]
    pub const fn classid_raw(self) -> u8 {
        ((self.0 >> 4) & 0xf) as u8
    }

    /// Returns the decoded [`ClassId`] from `[7:4]`, or `Err(raw)` if unrecognized.
    ///
    /// Should conventionally be called `class` to match the spec, but that is a reserved
    /// keyword.
    #[inline]
    pub fn classid(self) -> Result<ClassId, u8> {
        ClassId::try_from(self.classid_raw())
    }

    /// Sets the `classid` field (`[7:4]`).
    #[inline]
    pub const fn set_classid(&mut self, classid: ClassId) -> &mut Self {
        self.0 = (self.0 & !(0xf << 4)) | (((classid as u32) & 0xf) << 4);
        self
    }

    /// Returns the `prmbl_1` field (`[3:0]`).
    #[inline]
    pub const fn prmbl_1(self) -> u8 {
        (self.0 & 0xf) as u8
    }

    /// Sets the `prmbl_1` field (`[3:0]`).
    #[inline]
    pub const fn set_prmbl_1(&mut self, prmbl_1: u8) -> &mut Self {
        self.0 = (self.0 & !0xf) | ((prmbl_1 as u32) & 0xf);
        self
    }
}

/// [CS] B2.2.2
/// Peripheral ID0 register (`PIDR0`).
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct PeripheralId0Register(pub u32);

impl PeripheralId0Register {
    /// Byte offset of `PIDR0` within a component page.
    pub const OFFSET: u32 = 0xfe0;

    /// Creates a `PeripheralId0Register` from its raw 32-bit value.
    #[inline]
    pub const fn from_raw(val: u32) -> Self {
        Self(val)
    }

    /// Reads `PIDR0` from `io`.
    #[inline]
    pub fn read_from<Io: RegisterIo>(mut io: Io) -> Self {
        Self(io.read32(Self::OFFSET))
    }

    /// Returns the raw 32-bit register value.
    #[inline]
    pub const fn reg_value(self) -> u32 {
        self.0
    }

    /// Returns the `part0` field (`[7:0]`).
    #[inline]
    pub const fn part0(self) -> u8 {
        (self.0 & 0xff) as u8
    }

    /// Sets the `part0` field (`[7:0]`).
    #[inline]
    pub const fn set_part0(&mut self, part0: u8) -> &mut Self {
        self.0 = (self.0 & !0xff) | (part0 as u32);
        self
    }
}

/// [CS] B2.2.2
/// Peripheral ID1 register (`PIDR1`).
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct PeripheralId1Register(pub u32);

impl PeripheralId1Register {
    /// Byte offset of `PIDR1` within a component page.
    pub const OFFSET: u32 = 0xfe4;

    /// Creates a `PeripheralId1Register` from its raw 32-bit value.
    #[inline]
    pub const fn from_raw(val: u32) -> Self {
        Self(val)
    }

    /// Reads `PIDR1` from `io`.
    #[inline]
    pub fn read_from<Io: RegisterIo>(mut io: Io) -> Self {
        Self(io.read32(Self::OFFSET))
    }

    /// Returns the raw 32-bit register value.
    #[inline]
    pub const fn reg_value(self) -> u32 {
        self.0
    }

    /// Returns the `des0` field (`[7:4]`).
    #[inline]
    pub const fn des0(self) -> u8 {
        ((self.0 >> 4) & 0xf) as u8
    }

    /// Sets the `des0` field (`[7:4]`).
    #[inline]
    pub const fn set_des0(&mut self, des0: u8) -> &mut Self {
        self.0 = (self.0 & !(0xf << 4)) | (((des0 as u32) & 0xf) << 4);
        self
    }

    /// Returns the `part1` field (`[3:0]`).
    #[inline]
    pub const fn part1(self) -> u8 {
        (self.0 & 0xf) as u8
    }

    /// Sets the `part1` field (`[3:0]`).
    #[inline]
    pub const fn set_part1(&mut self, part1: u8) -> &mut Self {
        self.0 = (self.0 & !0xf) | ((part1 as u32) & 0xf);
        self
    }
}

/// [CS] B2.2.2
/// Peripheral ID2 register (`PIDR2`).
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct PeripheralId2Register(pub u32);

impl PeripheralId2Register {
    /// Byte offset of `PIDR2` within a component page.
    pub const OFFSET: u32 = 0xfe8;

    /// Creates a `PeripheralId2Register` from its raw 32-bit value.
    #[inline]
    pub const fn from_raw(val: u32) -> Self {
        Self(val)
    }

    /// Reads `PIDR2` from `io`.
    #[inline]
    pub fn read_from<Io: RegisterIo>(mut io: Io) -> Self {
        Self(io.read32(Self::OFFSET))
    }

    /// Returns the raw 32-bit register value.
    #[inline]
    pub const fn reg_value(self) -> u32 {
        self.0
    }

    /// Returns the `revision` field (`[7:4]`).
    #[inline]
    pub const fn revision(self) -> u8 {
        ((self.0 >> 4) & 0xf) as u8
    }

    /// Sets the `revision` field (`[7:4]`).
    #[inline]
    pub const fn set_revision(&mut self, revision: u8) -> &mut Self {
        self.0 = (self.0 & !(0xf << 4)) | (((revision as u32) & 0xf) << 4);
        self
    }

    /// Returns the `jedec` bit (`[3]`).
    #[inline]
    pub const fn jedec(self) -> bool {
        ((self.0 >> 3) & 0x1) != 0
    }

    /// Sets the `jedec` bit (`[3]`).
    #[inline]
    pub const fn set_jedec(&mut self, jedec: bool) -> &mut Self {
        if jedec {
            self.0 |= 1 << 3;
        } else {
            self.0 &= !(1 << 3);
        }
        self
    }

    /// Returns the `des1` field (`[2:0]`).
    #[inline]
    pub const fn des1(self) -> u8 {
        (self.0 & 0x7) as u8
    }

    /// Sets the `des1` field (`[2:0]`).
    #[inline]
    pub const fn set_des1(&mut self, des1: u8) -> &mut Self {
        self.0 = (self.0 & !0x7) | ((des1 as u32) & 0x7);
        self
    }
}

/// [CS] B2.2.2
/// Peripheral ID4 register (`PIDR4`).
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct PeripheralId4Register(pub u32);

impl PeripheralId4Register {
    /// Byte offset of `PIDR4` within a component page.
    pub const OFFSET: u32 = 0xfd0;

    /// Creates a `PeripheralId4Register` from its raw 32-bit value.
    #[inline]
    pub const fn from_raw(val: u32) -> Self {
        Self(val)
    }

    /// Reads `PIDR4` from `io`.
    #[inline]
    pub fn read_from<Io: RegisterIo>(mut io: Io) -> Self {
        Self(io.read32(Self::OFFSET))
    }

    /// Returns the raw 32-bit register value.
    #[inline]
    pub const fn reg_value(self) -> u32 {
        self.0
    }

    /// Returns the `size` field (`[7:4]`).
    #[inline]
    pub const fn size(self) -> u8 {
        ((self.0 >> 4) & 0xf) as u8
    }

    /// Sets the `size` field (`[7:4]`).
    #[inline]
    pub const fn set_size(&mut self, size: u8) -> &mut Self {
        self.0 = (self.0 & !(0xf << 4)) | (((size as u32) & 0xf) << 4);
        self
    }

    /// Returns the `des2` field (`[3:0]`).
    #[inline]
    pub const fn des2(self) -> u8 {
        (self.0 & 0xf) as u8
    }

    /// Sets the `des2` field (`[3:0]`).
    #[inline]
    pub const fn set_des2(&mut self, des2: u8) -> &mut Self {
        self.0 = (self.0 & !0xf) | ((des2 as u32) & 0xf);
        self
    }
}

/// [CS] B2.3.3
/// Used to determine whether two components have an affinity with one another
/// (e.g., if both correspond to the same CPU).
///
/// This 64-bit register is actually an amalgamation of the two device affinity
/// registers, `DEVAFF0` and `DEVAFF1`. We combine them as, in practice, the
/// resulting value is typically that of the 64-bit `MPIDR` register of the
/// associated CPU.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct DeviceAffinityRegister(pub u64);

impl DeviceAffinityRegister {
    /// Byte offset of `DEVAFF0`/`DEVAFF1` within a component page.
    pub const OFFSET: u32 = 0xfa8;

    /// Creates a `DeviceAffinityRegister` from its raw 64-bit value.
    #[inline]
    pub const fn from_raw(val: u64) -> Self {
        Self(val)
    }

    /// Reads `DEVAFF0`/`DEVAFF1` from `io`.
    #[inline]
    pub fn read_from<Io: RegisterIo>(mut io: Io) -> Self {
        Self(io.read64(Self::OFFSET))
    }

    /// Returns the raw 64-bit register value.
    #[inline]
    pub const fn reg_value(self) -> u64 {
        self.0
    }
}

/// [CS] B2.3.4
/// Identifies the architect and architecture of a CoreSight component (`DEVARCH`).
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct DeviceArchRegister(pub u32);

impl DeviceArchRegister {
    /// Byte offset of `DEVARCH` within a component page.
    pub const OFFSET: u32 = 0xfbc;

    /// Creates a `DeviceArchRegister` from its raw 32-bit value.
    #[inline]
    pub const fn from_raw(val: u32) -> Self {
        Self(val)
    }

    /// Reads `DEVARCH` from `io`.
    #[inline]
    pub fn read_from<Io: RegisterIo>(mut io: Io) -> Self {
        Self(io.read32(Self::OFFSET))
    }

    /// Returns the raw 32-bit register value.
    #[inline]
    pub const fn reg_value(self) -> u32 {
        self.0
    }

    /// Returns the `architect` field (`[31:21]`).
    #[inline]
    pub const fn architect(self) -> u16 {
        ((self.0 >> 21) & 0x7ff) as u16
    }

    /// Sets the `architect` field (`[31:21]`).
    #[inline]
    pub const fn set_architect(&mut self, architect: u16) -> &mut Self {
        self.0 = (self.0 & !(0x7ff << 21)) | (((architect as u32) & 0x7ff) << 21);
        self
    }

    /// Returns the `present` bit (`[20]`).
    #[inline]
    pub const fn present(self) -> bool {
        ((self.0 >> 20) & 0x1) != 0
    }

    /// Sets the `present` bit (`[20]`).
    #[inline]
    pub const fn set_present(&mut self, present: bool) -> &mut Self {
        if present {
            self.0 |= 1 << 20;
        } else {
            self.0 &= !(1 << 20);
        }
        self
    }

    /// Returns the `revision` field (`[19:16]`).
    #[inline]
    pub const fn revision(self) -> u8 {
        ((self.0 >> 16) & 0xf) as u8
    }

    /// Sets the `revision` field (`[19:16]`).
    #[inline]
    pub const fn set_revision(&mut self, revision: u8) -> &mut Self {
        self.0 = (self.0 & !(0xf << 16)) | (((revision as u32) & 0xf) << 16);
        self
    }

    /// Returns the `archid` field (`[15:0]`).
    #[inline]
    pub const fn archid(self) -> u16 {
        (self.0 & 0xffff) as u16
    }

    /// Sets the `archid` field (`[15:0]`).
    #[inline]
    pub const fn set_archid(&mut self, archid: u16) -> &mut Self {
        self.0 = (self.0 & !0xffff) | (archid as u32);
        self
    }
}

/// [CS] B2.2.2
/// JEDEC ID of the designer.
#[inline]
pub fn get_designer<Io: RegisterIo>(mut io: Io) -> u16 {
    let des0 = u16::from(PeripheralId1Register::read_from(&mut io).des0());
    let des1 = u16::from(PeripheralId2Register::read_from(&mut io).des1());
    let des2 = u16::from(PeripheralId4Register::read_from(&mut io).des2());
    (des2 << 7) | (des1 << 4) | des0
}

/// [CS] B2.2.2
/// This number is an ID chosen by the designer.
#[inline]
pub fn get_part_id<Io: RegisterIo>(mut io: Io) -> u16 {
    let part0 = u16::from(PeripheralId0Register::read_from(&mut io).part0());
    let part1 = u16::from(PeripheralId1Register::read_from(&mut io).part1());
    (part1 << 8) | part0
}

/// Major component type encoded in `DEVTYPE.MAJOR` (`DeviceTypeRegister::MajorType`).
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MajorType {
    /// Miscellaneous component (`0x0`).
    Miscellaneous = 0x0,
    /// Trace sink (`0x1`).
    TraceSink = 0x1,
    /// Trace link (`0x2`).
    TraceLink = 0x2,
    /// Trace source (`0x3`).
    TraceSource = 0x3,
    /// Debug control (`0x4`).
    DebugControl = 0x4,
    /// Debug logic (`0x5`).
    DebugLogic = 0x5,
    /// Performance monitor (`0x6`).
    PerformanceMonitor = 0x6,
}

static_assert!(size_of::<MajorType>() == size_of::<u8>());
static_assert!(align_of::<MajorType>() == align_of::<u8>());

/// Combined CoreSight device type (`DeviceTypeRegister::Type`) decoded from `(major, sub)`.
#[repr(u32)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DeviceType {
    /// Unknown or reserved `(major, sub)` combination.
    Unknown = 0,
    /// Validation component (`Miscellaneous`, `0x4`).
    ValidationComponent,
    /// Trace port (`TraceSink`, `0x1`).
    TracePort,
    /// Trace buffer (`TraceSink`, `0x2`).
    TraceBuffer,
    /// Trace router (`TraceSink`, `0x3`).
    TraceRouter,
    /// Trace funnel (`TraceLink`, `0x1`).
    TraceFunnel,
    /// Trace filter (`TraceLink`, `0x2`).
    TraceFilter,
    /// Trace FIFO (`TraceLink`, `0x3`).
    TraceFifo,
    /// CPU trace source (`TraceSource`, `0x1`).
    CpuTraceSource,
    /// DSP trace source (`TraceSource`, `0x2`).
    DspTraceSource,
    /// Data engine or coprocessor trace source (`TraceSource`, `0x3`).
    DataEngineTraceSource,
    /// Bus trace source (`TraceSource`, `0x4`).
    BusTraceSource,
    /// Software trace source (`TraceSource`, `0x6`).
    SoftwareTraceSource,
    /// Trigger matrix (`DebugControl`, `0x1`).
    TriggerMatrix,
    /// Debug authentication module (`DebugControl`, `0x2`).
    DebugAuthenticationModule,
    /// Power requestor (`DebugControl`, `0x3`).
    PowerRequestor,
    /// CPU debug logic (`DebugLogic`, `0x1`).
    CpuDebugLogic,
    /// DSP debug logic (`DebugLogic`, `0x2`).
    DspDebugLogic,
    /// Data engine or coprocessor debug logic (`DebugLogic`, `0x3`).
    DataEngineDebugLogic,
    /// Bus debug logic (`DebugLogic`, `0x4`).
    BusDebugLogic,
    /// Memory debug logic (`DebugLogic`, `0x5`).
    MemoryDebugLogic,
    /// CPU performance monitor (`PerformanceMonitor`, `0x1`).
    CpuPerformanceMonitor,
    /// DSP performance monitor (`PerformanceMonitor`, `0x2`).
    DspPerformanceMonitor,
    /// Data engine or coprocessor performance monitor (`PerformanceMonitor`, `0x3`).
    DataEnginePerformanceMonitor,
    /// Bus performance monitor (`PerformanceMonitor`, `0x4`).
    BusPerformanceMonitor,
    /// MMU performance monitor (`PerformanceMonitor`, `0x5`).
    MmuPerformanceMonitor,
}

static_assert!(size_of::<DeviceType>() == size_of::<u32>());
static_assert!(align_of::<DeviceType>() == align_of::<u32>());

impl TryFrom<u8> for MajorType {
    type Error = u8;

    #[inline]
    fn try_from(val: u8) -> Result<Self, Self::Error> {
        match val {
            0x0 => Ok(Self::Miscellaneous),
            0x1 => Ok(Self::TraceSink),
            0x2 => Ok(Self::TraceLink),
            0x3 => Ok(Self::TraceSource),
            0x4 => Ok(Self::DebugControl),
            0x5 => Ok(Self::DebugLogic),
            0x6 => Ok(Self::PerformanceMonitor),
            other => Err(other),
        }
    }
}

impl TryFrom<u32> for DeviceType {
    type Error = u32;

    #[inline]
    fn try_from(val: u32) -> Result<Self, Self::Error> {
        match val {
            x if x == Self::Unknown as u32 => Ok(Self::Unknown),
            x if x == Self::ValidationComponent as u32 => Ok(Self::ValidationComponent),
            x if x == Self::TracePort as u32 => Ok(Self::TracePort),
            x if x == Self::TraceBuffer as u32 => Ok(Self::TraceBuffer),
            x if x == Self::TraceRouter as u32 => Ok(Self::TraceRouter),
            x if x == Self::TraceFunnel as u32 => Ok(Self::TraceFunnel),
            x if x == Self::TraceFilter as u32 => Ok(Self::TraceFilter),
            x if x == Self::TraceFifo as u32 => Ok(Self::TraceFifo),
            x if x == Self::CpuTraceSource as u32 => Ok(Self::CpuTraceSource),
            x if x == Self::DspTraceSource as u32 => Ok(Self::DspTraceSource),
            x if x == Self::DataEngineTraceSource as u32 => Ok(Self::DataEngineTraceSource),
            x if x == Self::BusTraceSource as u32 => Ok(Self::BusTraceSource),
            x if x == Self::SoftwareTraceSource as u32 => Ok(Self::SoftwareTraceSource),
            x if x == Self::TriggerMatrix as u32 => Ok(Self::TriggerMatrix),
            x if x == Self::DebugAuthenticationModule as u32 => Ok(Self::DebugAuthenticationModule),
            x if x == Self::PowerRequestor as u32 => Ok(Self::PowerRequestor),
            x if x == Self::CpuDebugLogic as u32 => Ok(Self::CpuDebugLogic),
            x if x == Self::DspDebugLogic as u32 => Ok(Self::DspDebugLogic),
            x if x == Self::DataEngineDebugLogic as u32 => Ok(Self::DataEngineDebugLogic),
            x if x == Self::BusDebugLogic as u32 => Ok(Self::BusDebugLogic),
            x if x == Self::MemoryDebugLogic as u32 => Ok(Self::MemoryDebugLogic),
            x if x == Self::CpuPerformanceMonitor as u32 => Ok(Self::CpuPerformanceMonitor),
            x if x == Self::DspPerformanceMonitor as u32 => Ok(Self::DspPerformanceMonitor),
            x if x == Self::DataEnginePerformanceMonitor as u32 => {
                Ok(Self::DataEnginePerformanceMonitor)
            }
            x if x == Self::BusPerformanceMonitor as u32 => Ok(Self::BusPerformanceMonitor),
            x if x == Self::MmuPerformanceMonitor as u32 => Ok(Self::MmuPerformanceMonitor),
            other => Err(other),
        }
    }
}

impl DeviceType {
    /// Returns the human-readable description of this device type.
    #[inline]
    pub const fn as_str(self) -> &'static str {
        device_type_to_string(self)
    }
}

impl fmt::Display for DeviceType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Returns the human-readable description of a [`DeviceType`].
#[inline]
pub const fn device_type_to_string(dev_type: DeviceType) -> &'static str {
    match dev_type {
        DeviceType::Unknown => "unknown",
        DeviceType::ValidationComponent => "validation component",
        DeviceType::TracePort => "trace port",
        DeviceType::TraceBuffer => "trace buffer",
        DeviceType::TraceRouter => "trace router",
        DeviceType::TraceFunnel => "trace funnel",
        DeviceType::TraceFilter => "trace filter",
        DeviceType::TraceFifo => "trace FIFO",
        DeviceType::CpuTraceSource => "CPU trace source",
        DeviceType::DspTraceSource => "DSP trace source",
        DeviceType::DataEngineTraceSource => "data engine or coprocessor trace source",
        DeviceType::BusTraceSource => "bus trace source",
        DeviceType::SoftwareTraceSource => "software trace source",
        DeviceType::TriggerMatrix => "trigger matrix",
        DeviceType::DebugAuthenticationModule => "debug authentication module",
        DeviceType::PowerRequestor => "power requestor",
        DeviceType::CpuDebugLogic => "CPU debug logic",
        DeviceType::DspDebugLogic => "DSP debug logic",
        DeviceType::DataEngineDebugLogic => "data engine or coprocessor debug logic",
        DeviceType::BusDebugLogic => "bus debug logic",
        DeviceType::MemoryDebugLogic => "memory debug logic",
        DeviceType::CpuPerformanceMonitor => "CPU performance monitor",
        DeviceType::DspPerformanceMonitor => "DSP performance monitor",
        DeviceType::DataEnginePerformanceMonitor => {
            "Data engine or coprocessor performance monitor"
        }
        DeviceType::BusPerformanceMonitor => "bus performance monitor",
        DeviceType::MmuPerformanceMonitor => "MMU performance monitor",
    }
}

/// [CS] B2.3.8
/// Gives high-level information about the type of a CoreSight component (`DEVTYPE`).
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct DeviceTypeRegister(pub u32);

impl DeviceTypeRegister {
    /// Byte offset of `DEVTYPE` within a component page.
    pub const OFFSET: u32 = 0xfcc;

    /// Creates a `DeviceTypeRegister` from its raw 32-bit value.
    #[inline]
    pub const fn from_raw(val: u32) -> Self {
        Self(val)
    }

    /// Reads `DEVTYPE` from `io`.
    #[inline]
    pub fn read_from<Io: RegisterIo>(mut io: Io) -> Self {
        Self(io.read32(Self::OFFSET))
    }

    /// Returns the raw 32-bit register value.
    #[inline]
    pub const fn reg_value(self) -> u32 {
        self.0
    }

    /// Returns the `sub` (subtype) field (`[7:4]`).
    #[inline]
    pub const fn sub(self) -> u8 {
        ((self.0 >> 4) & 0xf) as u8
    }

    /// Sets the `sub` (subtype) field (`[7:4]`).
    #[inline]
    pub const fn set_sub(&mut self, sub: u8) -> &mut Self {
        self.0 = (self.0 & !(0xf << 4)) | (((sub as u32) & 0xf) << 4);
        self
    }

    /// Returns the raw 4-bit `major` field (`[3:0]`).
    #[inline]
    pub const fn major_raw(self) -> u8 {
        (self.0 & 0xf) as u8
    }

    /// Returns the decoded [`MajorType`] from `[3:0]`, or `Err(raw)` if unrecognized.
    #[inline]
    pub fn major(self) -> Result<MajorType, u8> {
        MajorType::try_from(self.major_raw())
    }

    /// Sets the `major` field (`[3:0]`).
    #[inline]
    pub const fn set_major(&mut self, major: MajorType) -> &mut Self {
        self.0 = (self.0 & !0xf) | ((major as u32) & 0xf);
        self
    }

    /// The type encoded by `sub` and `major` fields.
    #[inline]
    pub fn device_type(self) -> DeviceType {
        // Allows us to switch across (major, sub-) type pairs.

        // [CS] Table B2-9.
        match (self.major_raw(), self.sub()) {
            (x, 0x4) if x == MajorType::Miscellaneous as u8 => DeviceType::ValidationComponent,
            (x, 0x1) if x == MajorType::TraceSink as u8 => DeviceType::TracePort,
            (x, 0x2) if x == MajorType::TraceSink as u8 => DeviceType::TraceBuffer,
            (x, 0x3) if x == MajorType::TraceSink as u8 => DeviceType::TraceRouter,
            (x, 0x1) if x == MajorType::TraceLink as u8 => DeviceType::TraceFunnel,
            (x, 0x2) if x == MajorType::TraceLink as u8 => DeviceType::TraceFilter,
            (x, 0x3) if x == MajorType::TraceLink as u8 => DeviceType::TraceFifo,
            (x, 0x1) if x == MajorType::TraceSource as u8 => DeviceType::CpuTraceSource,
            (x, 0x2) if x == MajorType::TraceSource as u8 => DeviceType::DspTraceSource,
            (x, 0x3) if x == MajorType::TraceSource as u8 => DeviceType::DataEngineTraceSource,
            (x, 0x4) if x == MajorType::TraceSource as u8 => DeviceType::BusTraceSource,
            (x, 0x6) if x == MajorType::TraceSource as u8 => DeviceType::SoftwareTraceSource,
            (x, 0x1) if x == MajorType::DebugControl as u8 => DeviceType::TriggerMatrix,
            (x, 0x2) if x == MajorType::DebugControl as u8 => DeviceType::DebugAuthenticationModule,
            (x, 0x3) if x == MajorType::DebugControl as u8 => DeviceType::PowerRequestor,
            (x, 0x1) if x == MajorType::DebugLogic as u8 => DeviceType::CpuDebugLogic,
            (x, 0x2) if x == MajorType::DebugLogic as u8 => DeviceType::DspDebugLogic,
            (x, 0x3) if x == MajorType::DebugLogic as u8 => DeviceType::DataEngineDebugLogic,
            (x, 0x4) if x == MajorType::DebugLogic as u8 => DeviceType::BusDebugLogic,
            (x, 0x5) if x == MajorType::DebugLogic as u8 => DeviceType::MemoryDebugLogic,
            (x, 0x1) if x == MajorType::PerformanceMonitor as u8 => {
                DeviceType::CpuPerformanceMonitor
            }
            (x, 0x2) if x == MajorType::PerformanceMonitor as u8 => {
                DeviceType::DspPerformanceMonitor
            }
            (x, 0x3) if x == MajorType::PerformanceMonitor as u8 => {
                DeviceType::DataEnginePerformanceMonitor
            }
            (x, 0x4) if x == MajorType::PerformanceMonitor as u8 => {
                DeviceType::BusPerformanceMonitor
            }
            (x, 0x5) if x == MajorType::PerformanceMonitor as u8 => {
                DeviceType::MmuPerformanceMonitor
            }
            _ => DeviceType::Unknown,
        }
    }
}

/// FFI trampoline for `coresight::DeviceTypeRegister::type()`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_coresight_device_type_from_raw(reg_value: u32) -> u32 {
    DeviceTypeRegister::from_raw(reg_value).device_type() as u32
}

/// FFI trampoline for `coresight::ToString(ComponentIdRegister::Class)`.
///
/// # Safety
///
/// `out_len` must be a valid, aligned pointer writable for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_coresight_class_id_to_string(
    raw_classid: u8,
    out_len: *mut usize,
) -> *const c_char {
    let s = class_id_to_string(raw_classid);
    // SAFETY: Caller guarantees `out_len` is a valid, aligned, writable pointer.
    unsafe {
        *out_len = s.len();
    }
    s.as_ptr().cast::<c_char>()
}

/// FFI trampoline for `coresight::ToString(DeviceTypeRegister::Type)`.
///
/// # Safety
///
/// `out_len` must be a valid, aligned pointer writable for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_coresight_device_type_to_string(
    raw_dev_type: u32,
    out_len: *mut usize,
) -> *const c_char {
    let dev_type = DeviceType::try_from(raw_dev_type).unwrap_or(DeviceType::Unknown);
    let s = device_type_to_string(dev_type);
    // SAFETY: Caller guarantees `out_len` is a valid, aligned, writable pointer.
    unsafe {
        *out_len = s.len();
    }
    s.as_ptr().cast::<c_char>()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeComponentIo {
        pidr0: u32,
        pidr1: u32,
        pidr2: u32,
        pidr4: u32,
        devaff: u64,
        devarch: u32,
        devtype: u32,
        cidr1: u32,
    }

    impl RegisterIo for FakeComponentIo {
        fn read32(&mut self, offset: u32) -> u32 {
            match offset {
                PeripheralId0Register::OFFSET => self.pidr0,
                PeripheralId1Register::OFFSET => self.pidr1,
                PeripheralId2Register::OFFSET => self.pidr2,
                PeripheralId4Register::OFFSET => self.pidr4,
                DeviceArchRegister::OFFSET => self.devarch,
                DeviceTypeRegister::OFFSET => self.devtype,
                ComponentIdRegister::OFFSET => self.cidr1,
                other => panic!("unexpected 32-bit read at {other:#x}"),
            }
        }

        fn read64(&mut self, offset: u32) -> u64 {
            match offset {
                DeviceAffinityRegister::OFFSET => self.devaff,
                other => panic!("unexpected 64-bit read at {other:#x}"),
            }
        }

        fn write32(&mut self, _val: u32, _offset: u32) {}
        fn write64(&mut self, _val: u64, _offset: u32) {}
    }

    #[test]
    fn device_type_decoding_and_strings() {
        let cases = [
            (
                (MajorType::Miscellaneous, 0x4),
                DeviceType::ValidationComponent,
                "validation component",
            ),
            ((MajorType::TraceSink, 0x1), DeviceType::TracePort, "trace port"),
            ((MajorType::TraceSink, 0x2), DeviceType::TraceBuffer, "trace buffer"),
            ((MajorType::TraceSink, 0x3), DeviceType::TraceRouter, "trace router"),
            ((MajorType::TraceLink, 0x1), DeviceType::TraceFunnel, "trace funnel"),
            ((MajorType::TraceLink, 0x2), DeviceType::TraceFilter, "trace filter"),
            ((MajorType::TraceLink, 0x3), DeviceType::TraceFifo, "trace FIFO"),
            ((MajorType::TraceSource, 0x1), DeviceType::CpuTraceSource, "CPU trace source"),
            ((MajorType::TraceSource, 0x2), DeviceType::DspTraceSource, "DSP trace source"),
            (
                (MajorType::TraceSource, 0x3),
                DeviceType::DataEngineTraceSource,
                "data engine or coprocessor trace source",
            ),
            ((MajorType::TraceSource, 0x4), DeviceType::BusTraceSource, "bus trace source"),
            (
                (MajorType::TraceSource, 0x6),
                DeviceType::SoftwareTraceSource,
                "software trace source",
            ),
            ((MajorType::DebugControl, 0x1), DeviceType::TriggerMatrix, "trigger matrix"),
            (
                (MajorType::DebugControl, 0x2),
                DeviceType::DebugAuthenticationModule,
                "debug authentication module",
            ),
            ((MajorType::DebugControl, 0x3), DeviceType::PowerRequestor, "power requestor"),
            ((MajorType::DebugLogic, 0x1), DeviceType::CpuDebugLogic, "CPU debug logic"),
            ((MajorType::DebugLogic, 0x2), DeviceType::DspDebugLogic, "DSP debug logic"),
            (
                (MajorType::DebugLogic, 0x3),
                DeviceType::DataEngineDebugLogic,
                "data engine or coprocessor debug logic",
            ),
            ((MajorType::DebugLogic, 0x4), DeviceType::BusDebugLogic, "bus debug logic"),
            ((MajorType::DebugLogic, 0x5), DeviceType::MemoryDebugLogic, "memory debug logic"),
            (
                (MajorType::PerformanceMonitor, 0x1),
                DeviceType::CpuPerformanceMonitor,
                "CPU performance monitor",
            ),
            (
                (MajorType::PerformanceMonitor, 0x2),
                DeviceType::DspPerformanceMonitor,
                "DSP performance monitor",
            ),
            (
                (MajorType::PerformanceMonitor, 0x3),
                DeviceType::DataEnginePerformanceMonitor,
                "Data engine or coprocessor performance monitor",
            ),
            (
                (MajorType::PerformanceMonitor, 0x4),
                DeviceType::BusPerformanceMonitor,
                "bus performance monitor",
            ),
            (
                (MajorType::PerformanceMonitor, 0x5),
                DeviceType::MmuPerformanceMonitor,
                "MMU performance monitor",
            ),
        ];

        for ((major, sub), expected_type, expected_str) in cases {
            let mut reg = DeviceTypeRegister::default();
            reg.set_major(major).set_sub(sub);
            assert_eq!(reg.device_type(), expected_type);
            assert_eq!(reg.device_type().as_str(), expected_str);
            assert_eq!(rust_coresight_device_type_from_raw(reg.reg_value()), expected_type as u32);
        }

        let unknown = DeviceTypeRegister::from_raw(0xff);
        assert_eq!(unknown.device_type(), DeviceType::Unknown);
        assert_eq!(unknown.device_type().as_str(), "unknown");
    }

    #[test]
    fn class_id_decoding_and_strings() {
        assert_eq!(class_id_to_string(0x0), "generic verification");
        assert_eq!(class_id_to_string(0x1), "0x1 ROM table");
        assert_eq!(class_id_to_string(0x9), "CoreSight");
        assert_eq!(class_id_to_string(0xb), "peripheral test block");
        assert_eq!(class_id_to_string(0xe), "generic IP");
        assert_eq!(class_id_to_string(0xf), "non-standard");
        assert_eq!(class_id_to_string(0x5), "unknown");
    }

    #[test]
    fn designer_and_part_id() {
        let mut pidr0 = PeripheralId0Register::default();
        pidr0.set_part0(0x06);
        let mut pidr1 = PeripheralId1Register::default();
        pidr1.set_part1(0x09).set_des0(0xb);
        let mut pidr2 = PeripheralId2Register::default();
        pidr2.set_des1(0x3).set_jedec(true).set_revision(0x2);
        let mut pidr4 = PeripheralId4Register::default();
        pidr4.set_des2(0x4).set_size(0x0);

        let mut io = FakeComponentIo {
            pidr0: pidr0.reg_value(),
            pidr1: pidr1.reg_value(),
            pidr2: pidr2.reg_value(),
            pidr4: pidr4.reg_value(),
            devaff: 0x8000_0001,
            devarch: 0,
            devtype: 0,
            cidr1: 0x90,
        };

        assert_eq!(get_designer(&mut io), arm::ARCHITECT);
        assert_eq!(get_part_id(&mut io), arm::partid::CTI_400);
        assert_eq!(DeviceAffinityRegister::read_from(&mut io).reg_value(), 0x8000_0001);
        assert_eq!(ComponentIdRegister::read_from(&mut io).classid(), Ok(ClassId::CoreSight));
    }

    #[test]
    fn ffi_trampolines() {
        let mut len = 0usize;
        // SAFETY: `&mut len` is a valid, aligned pointer.
        let ptr = unsafe { rust_coresight_class_id_to_string(ClassId::CoreSight as u8, &mut len) };
        // SAFETY: `rust_coresight_class_id_to_string` returns a static UTF-8 slice of length `len`.
        let s = unsafe {
            core::str::from_utf8_unchecked(core::slice::from_raw_parts(ptr.cast::<u8>(), len))
        };
        assert_eq!(s, "CoreSight");

        // SAFETY: `&mut len` is a valid, aligned pointer.
        let ptr = unsafe {
            rust_coresight_device_type_to_string(DeviceType::CpuDebugLogic as u32, &mut len)
        };
        // SAFETY: `rust_coresight_device_type_to_string` returns a static UTF-8 slice of length
        // `len`.
        let s = unsafe {
            core::str::from_utf8_unchecked(core::slice::from_raw_parts(ptr.cast::<u8>(), len))
        };
        assert_eq!(s, "CPU debug logic");

        // SAFETY: `&mut len` is a valid, aligned pointer.
        let ptr = unsafe { rust_coresight_device_type_to_string(0xffff, &mut len) };
        // SAFETY: `rust_coresight_device_type_to_string` returns a static UTF-8 slice of length
        // `len`.
        let s = unsafe {
            core::str::from_utf8_unchecked(core::slice::from_raw_parts(ptr.cast::<u8>(), len))
        };
        assert_eq!(s, "unknown");
    }
}
