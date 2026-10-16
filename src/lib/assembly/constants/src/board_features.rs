// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![deny(missing_docs)]

//! This module defines the `BoardFeature` enum listing all valid features a board can provide.

/// The list of known features that a board can provide to the product.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum BoardFeature {
    /// Always-on counter used for timekeeping instead of a persistent RTC.
    AlwaysOnCounter,
    /// High-Resolution Timer support.
    Hrtimer,
    /// Bluetooth transport support over UART.
    BtTransportUart,
    /// User-space PCI bus support.
    BusPci,
    /// CPU power boost/frequency scaling support.
    CpuPowerBoost,
    /// Use a custom audio core component instead of the default.
    CustomAudioCore,
    /// The driver for the kernel's debug UART powers it down across system suspend, which needs
    /// the `fuchsia.kernel.DebugResource` capability. This includes
    /// `component_manager_policy_debug_uart_suspend.json5`, which grants it only to the driver
    /// monikers it lists (today Iris's `uart-db61000`). Other boards must add theirs there.
    DebugUartSuspend,
    /// Request the deep system suspend state (`ZX_SYSTEM_SUSPEND_OPTION_DEEP`) when suspending.
    /// Routed as `fuchsia.power.DeepSuspendEnabled` to suspend drivers (e.g. generic-suspend);
    /// only has an effect when suspend is enabled. Products can disable it with
    /// `platform.power.deep_suspend_enabled`.
    DeepSuspend,
    /// Eager startup for power-manager component.
    EagerPowerManager,
    /// Fake battery driver for testing or emulators.
    FakeBattery,
    /// Fake display stack host for testing.
    FakeDisplay,
    /// Fake power sensor driver for testing.
    FakePowerSensor,
    /// Cooling fan device support.
    Fan,
    /// Global Navigation Satellite System (GPS/GNSS) support.
    Gnss,
    /// Input devices support (keyboard, touchscreen, mouse).
    Input,
    /// Intel High Definition Audio support.
    IntelHda,
    /// Network interconnect/fabric management support.
    Interconnect,
    /// Android KeyMint (software/hardware) backed keystore support.
    Keymint,
    /// KeySafe Trusted Application support.
    KeysafeTa,
    /// ARM Mali GPU driver support.
    MaliGpu,
    /// Support for running as a guest in a virtual machine.
    Paravirtualization,
    /// Device flashing and paving utilities support.
    Paver,
    /// Physical Memory Manager checker for memory corruption issues.
    PmmChecker,
    /// Automatic Physical Memory Manager checker configuration.
    PmmCheckerAuto,
    /// Power management and frequency scaling support.
    Power,
    /// Power framework support for WLAN driver.
    PowerFrameworkEnabledForWlanDriver,
    /// Pulse Width Modulation controller support.
    Pwm,
    /// Radar sensor support.
    Radar,
    /// Real-time clock hardware support.
    RealTimeClock,
    /// Realtek RTL8211F PHY driver support.
    Realtek8211f,
    /// Runtime processor power management scaling.
    RuntimeProcessorPowerManagement,
    /// Use the Rust-based driver manager.
    RustDriverManager,
    /// Secure Digital Host Controller Interface support.
    Sdhci,
    /// SD/MMC Command Queueing Engine support.
    SdmmcCqe,
    /// Support for sharing registers across multiple drivers/components.
    SharedRegisters,
    /// Software-based cryptography support for boards lacking hardware acceleration.
    SoftCrypto,
    /// A/B slot management support (switching slots, marking slots successful, etc).
    SlotManagement,
    /// Hardware-backed inline storage encryption support.
    StorageInlineCrypto,
    /// Power management features for storage devices.
    StoragePowerManagement,
    /// System suspend/resume support.
    Suspender,
    /// Support for waiting for suspending token during suspend.
    SuspendingToken,
    /// Universal Flash Storage over PCI support.
    UfsPci,
    /// Universal Flash Storage over Platform Device support.
    UfsPdev,
    /// USB Bus driver support.
    UsbBus,
    /// USB Host mode support.
    UsbHost,
    /// USB Peripheral/Device mode support.
    UsbPeripheralSupport,
    /// Automatically set or synchronize UTC time at system startup.
    UtcStartAtStartup,
    /// Hardware video encoder driver support.
    VideoEncoders,
    /// Vulkan-capable GPU driver support.
    VulkanGpu,
    /// WLAN FullMAC driver support.
    WlanFullmac,
    /// WLAN SoftMAC driver support.
    WlanSoftmac,
    /// eXtensible Host Controller Interface (USB 3.0) support.
    Xhci,
    /// Unknown feature that is not yet added to the enum.
    Unknown(String),
}

impl AsRef<str> for BoardFeature {
    fn as_ref(&self) -> &str {
        match self {
            Self::AlwaysOnCounter => "fuchsia::always_on_counter",
            Self::Hrtimer => "fuchsia::hrtimer",
            Self::BtTransportUart => "fuchsia::bt_transport_uart",
            Self::BusPci => "fuchsia::bus_pci",
            Self::CpuPowerBoost => "fuchsia::cpu_power_boost",
            Self::CustomAudioCore => "fuchsia::custom_audio_core",
            Self::DebugUartSuspend => "fuchsia::debug_uart_suspend",
            Self::DeepSuspend => "fuchsia::deep_suspend",
            Self::EagerPowerManager => "fuchsia::eager_power_manager",
            Self::FakeBattery => "fuchsia::fake_battery",
            Self::FakeDisplay => "fuchsia::fake_display",
            Self::FakePowerSensor => "fuchsia::fake_power_sensor",
            Self::Fan => "fuchsia::fan",
            Self::Gnss => "fuchsia::gnss",
            Self::Input => "fuchsia::input",
            Self::IntelHda => "fuchsia::intel_hda",
            Self::Interconnect => "fuchsia::interconnect",
            Self::Keymint => "fuchsia::keymint",
            Self::KeysafeTa => "fuchsia::keysafe_ta",
            Self::MaliGpu => "fuchsia::mali_gpu",
            Self::Paravirtualization => "fuchsia::paravirtualization",
            Self::Paver => "fuchsia::paver",
            Self::PmmChecker => "fuchsia::pmm_checker",
            Self::PmmCheckerAuto => "fuchsia::pmm_checker_auto",
            Self::Power => "fuchsia::power",
            Self::PowerFrameworkEnabledForWlanDriver => {
                "fuchsia::power_framework_enabled_for_wlan_driver"
            }
            Self::Pwm => "fuchsia::pwm",
            Self::Radar => "fuchsia::radar",
            Self::RealTimeClock => "fuchsia::real_time_clock",
            Self::Realtek8211f => "fuchsia::realtek_8211f",
            Self::RuntimeProcessorPowerManagement => "fuchsia::runtime_processor_power_management",
            Self::RustDriverManager => "fuchsia::rust_driver_manager",
            Self::Sdhci => "fuchsia::sdhci",
            Self::SdmmcCqe => "fuchsia::sdmmc_cqe",
            Self::SharedRegisters => "fuchsia::shared_registers",
            Self::SoftCrypto => "fuchsia::soft_crypto",
            Self::SlotManagement => "fuchsia::slot_management",
            Self::StorageInlineCrypto => "fuchsia::storage_inline_crypto",
            Self::StoragePowerManagement => "fuchsia::storage_power_management",
            Self::Suspender => "fuchsia::suspender",
            Self::SuspendingToken => "fuchsia::suspending_token",
            Self::UfsPci => "fuchsia::ufs_pci",
            Self::UfsPdev => "fuchsia::ufs_pdev",
            Self::UsbBus => "fuchsia::usb_bus",
            Self::UsbHost => "fuchsia::usb_host",
            Self::UsbPeripheralSupport => "fuchsia::usb_peripheral_support",
            Self::UtcStartAtStartup => "fuchsia::utc_start_at_startup",
            Self::VideoEncoders => "fuchsia::video_encoders",
            Self::VulkanGpu => "fuchsia::vulkan_gpu",
            Self::WlanFullmac => "fuchsia::wlan_fullmac",
            Self::WlanSoftmac => "fuchsia::wlan_softmac",
            Self::Xhci => "fuchsia::xhci",
            Self::Unknown(s) => s.as_str(),
        }
    }
}
