// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_LIB_AMLOGIC_INCLUDE_SOC_AML_A311D_A311D_HW_H_
#define SRC_DEVICES_LIB_AMLOGIC_INCLUDE_SOC_AML_A311D_A311D_HW_H_

// HIU - includes clock control registers
#define A311D_HIU_BASE 0xff63c000
#define A311D_HIU_LENGTH 0x2000

// gpio
#define A311D_GPIO_BASE 0xff634400
#define A311D_GPIO_LENGTH 0x400
#define A311D_GPIO_AO_BASE 0xff800000
#define A311D_GPIO_AO_LENGTH 0x1000
#define A311D_GPIO_INTERRUPT_BASE 0xffd00000
#define A311D_GPIO_INTERRUPT_LENGTH 0x10000

// audio
#define A311D_EE_PDM_BASE (0xff640000)
#define A311D_EE_PDM_LENGTH (0x2000)

#define A311D_EE_AUDIO_BASE (0xff642000)
#define A311D_EE_AUDIO_LENGTH (0x2000)

#define A311D_AUDIO_TODDR_A 180
#define A311D_AUDIO_TODDR_B 181
#define A311D_AUDIO_TODDR_C 182
#define A311D_AUDIO_SPDIFIN 183
#define A311D_AUDIO_FRDDR_A 184
#define A311D_AUDIO_FRDDR_B 185
#define A311D_AUDIO_FRDDR_C 186
#define A311D_AUDIO_VAD_FLAG 187
#define A311D_AUDIO_TORAM 188

// i2c
#define A311D_I2C_LENGTH 0x1000

#define A311D_I2C_AOBUS_BASE 0xff805000
#define A311D_EE_I2C_M0_BASE 0xffd1f000
#define A311D_EE_I2C_M1_BASE 0xffd1e000
#define A311D_EE_I2C_M2_BASE 0xffd1d000
#define A311D_EE_I2C_M3_BASE 0xffd1c000

// Video decoder/encoder bus.
#define A311D_DOS_BASE 0xff620000
#define A311D_DOS_LENGTH 0x10000

// spicc
#define A311D_SPICC0_BASE 0xffd13000
#define A311D_SPICC1_BASE 0xffd15000

#define A311D_PWM_LENGTH 0x1000  // applies to each PWM bank
#define A311D_PWM_AB_BASE 0xffd1b000
#define A311D_PWM_PWM_A 0x0
#define A311D_PWM_PWM_B 0x4
#define A311D_PWM_MISC_REG_AB 0x8
#define A311D_DS_A_B 0xc
#define A311D_PWM_TIME_AB 0x10
#define A311D_PWM_A2 0x14
#define A311D_PWM_B2 0x18
#define A311D_PWM_BLINK_AB 0x1c

#define A311D_PWM_CD_BASE 0xffd1a000
#define A311D_PWM_PWM_C 0x0
#define A311D_PWM_PWM_D 0x4
#define A311D_PWM_MISC_REG_CD 0x8
#define A311D_DS_C_D 0xc
#define A311D_PWM_TIME_CD 0x10
#define A311D_PWM_C2 0x14
#define A311D_PWM_D2 0x18
#define A311D_PWM_BLINK_CD 0x1c

#define A311D_PWM_EF_BASE 0xffd19000
#define A311D_PWM_PWM_E 0x0
#define A311D_PWM_PWM_F 0x4
#define A311D_PWM_MISC_REG_EF 0x8
#define A311D_DS_E_F 0xc
#define A311D_PWM_TIME_EF 0x10
#define A311D_PWM_E2 0x14
#define A311D_PWM_F2 0x18
#define A311D_PWM_BLINK_EF 0x1c

#define A311D_AO_PWM_AB_BASE 0xff807000
#define A311D_AO_PWM_PWM_A 0x0
#define A311D_AO_PWM_PWM_B 0x4
#define A311D_AO_PWM_MISC_REG_AB 0x8
#define A311D_AO_DS_A_B 0xc
#define A311D_AO_PWM_TIME_AB 0x10
#define A311D_AO_PWM_A2 0x14
#define A311D_AO_PWM_B2 0x18
#define A311D_AO_PWM_BLINK_AB 0x1c

#define A311D_AO_PWM_CD_BASE 0xff802000
#define A311D_AO_PWM_PWM_C 0x0
#define A311D_AO_PWM_PWM_D 0x4
#define A311D_AO_PWM_MISC_REG_CD 0x8
#define A311D_AO_DS_C_D 0xc
#define A311D_AO_PWM_TIME_CD 0x10
#define A311D_AO_PWM_C2 0x14
#define A311D_AO_PWM_D2 0x18
#define A311D_AO_PWM_BLINK_CD 0x1c
#define A311D_AO_PWM_LENGTH 0x1000

// Peripherals - datasheet is nondescript about this section, but it contains
//  top level ethernet control and temp sensor registers
#define A311D_PERIPHERALS_BASE 0xff634000
#define A311D_PERIPHERALS_LENGTH 0x1000

// Ethernet
#define A311D_ETH_PHY_BASE 0xff64c000
#define A311D_ETH_PHY_LENGTH 0x2000
#define A311D_ETH_MAC_BASE 0xff3f0000
#define A311D_ETH_MAC_LENGTH 0x10000

// eMMC
#define A311D_EMMC_A_BASE 0xffe03000
#define A311D_EMMC_A_LENGTH 0x2000
#define A311D_EMMC_B_BASE 0xffe05000
#define A311D_EMMC_B_LENGTH 0x2000
#define A311D_EMMC_C_BASE 0xffe07000
#define A311D_EMMC_C_LENGTH 0x2000

// NNA
#define A311D_NNA_SRAM_BASE 0xFF000000
#define A311D_NNA_SRAM_LENGTH 0x100000
#define A311D_NNA_BASE 0xFF100000
#define A311D_NNA_LENGTH 0x30000

// Timer
#define A311D_TIMER_BASE 0xffd00000
#define A311D_TIMER_LENGTH 0x10000

// Power domain
#define A311D_POWER_DOMAIN_BASE 0xff800000
#define A311D_POWER_DOMAIN_LENGTH 0x1000

// Memory Power Domain
#define A311D_MEMORY_PD_BASE 0xff63c000
#define A311D_MEMORY_PD_LENGTH 0x1000

// Reset
#define A311D_RESET_BASE 0xffd01000
#define A311D_RESET_LENGTH 0x100
#define A311D_RESET0_REGISTER 0x04
#define A311D_RESET1_REGISTER 0x08
#define A311D_RESET2_REGISTER 0x0c
#define A311D_RESET3_REGISTER 0x10
#define A311D_RESET4_REGISTER 0x14
#define A311D_RESET6_REGISTER 0x1c
#define A311D_RESET7_REGISTER 0x20
#define A311D_RESET0_MASK 0x40
#define A311D_RESET1_MASK 0x44
#define A311D_RESET2_MASK 0x48
#define A311D_RESET3_MASK 0x4c
#define A311D_RESET4_MASK 0x50
#define A311D_RESET6_MASK 0x58
#define A311D_RESET7_MASK 0x5c
#define A311D_RESET0_LEVEL 0x80
#define A311D_RESET1_LEVEL 0x84
#define A311D_RESET2_LEVEL 0x88
#define A311D_RESET3_LEVEL 0x8c
#define A311D_RESET4_LEVEL 0x90
#define A311D_RESET6_LEVEL 0x98
#define A311D_RESET7_LEVEL 0x9c

// IRQs
#define A311D_VIU1_VSYNC_IRQ 35
#define A311D_ETH_GMAC_IRQ 40
#define A311D_USB_IDDIG_IRQ 48
#define A311D_I2C_M0_IRQ 53
#define A311D_DEMUX_IRQ 55
#define A311D_UART_EE_A_IRQ 58
#define A311D_USB0_IRQ 62
#define A311D_USB1_IRQ 63
#define A311D_PARSER_IRQ 64
#define A311D_TS_PLL_IRQ 67
#define A311D_TS_DDR_IRQ 68
#define A311D_I2C_M3_IRQ 71
#define A311D_DOS_MBOX_0_IRQ 75
#define A311D_DOS_MBOX_1_IRQ 76
#define A311D_DOS_MBOX_2_IRQ 77
#define A311D_GPIO_IRQ_0 96
#define A311D_GPIO_IRQ_1 97
#define A311D_GPIO_IRQ_2 98
#define A311D_GPIO_IRQ_3 99
#define A311D_GPIO_IRQ_4 100
#define A311D_GPIO_IRQ_5 101
#define A311D_GPIO_IRQ_6 102
#define A311D_GPIO_IRQ_7 103
#define A311D_UART1_IRQ 107
#define A311D_SPICC0_IRQ 113
#define A311D_VID1_WR_IRQ 118
#define A311D_RDMA_DONE_IRQ 121
#define A311D_SPICC1_IRQ 122
#define A311D_UART2_IRQ 125
#define A311D_NNA_IRQ 179
#define A311D_MALI_IRQ_GP 192
#define A311D_MALI_IRQ_GPMMU 193
#define A311D_MALI_IRQ_PP 194
#define A311D_SD_EMMC_A_IRQ 221
#define A311D_SD_EMMC_B_IRQ 222
#define A311D_SD_EMMC_C_IRQ 223
#define A311D_I2C_AO_IRQ 227
#define A311D_SARADC_IRQ 232
#define A311D_I2C_M1_IRQ 246
#define A311D_I2C_M2_IRQ 247
#define A311D_AO_TIMER_A_IRQ 236
#define A311D_AO_TIMER_B_IRQ 237
#define A311D_TIMER_A_IRQ 42
#define A311D_TIMER_B_IRQ 43
#define A311D_TIMER_C_IRQ 38
#define A311D_TIMER_D_IRQ 61
// Timer E does not trigger an IRQ.
#define A311D_TIMER_F_IRQ 92
#define A311D_TIMER_G_IRQ 93
#define A311D_TIMER_H_IRQ 94
#define A311D_TIMER_I_IRQ 95

// USB
#define A311D_USB0_BASE 0xff500000
#define A311D_USB0_LENGTH 0x100000

#define A311D_USB1_BASE 0xff400000
#define A311D_USB1_LENGTH 0x100000

#define A311D_USBCTRL_BASE 0xffe09000
#define A311D_USBCTRL_LENGTH 0x2000

#define A311D_USBPHY20_BASE 0xff636000
#define A311D_USBPHY20_LENGTH 0x2000

#define A311D_USBPHY21_BASE 0xff63a000
#define A311D_USBPHY21_LENGTH 0x2000

#define A311D_USB3_PCIE_PHY_BASE 0xff646000
#define A311D_USB3_PCIE_PHY_LENGTH 0x2000

// UART
#define A311D_UART_EE_A_BASE 0xffd24000
#define A311D_UART_EE_A_LENGTH 0x18

// Alternate functions for UARTs
#define A311D_UART_EE_A_TX A311D_GPIOX(12)
#define A311D_UART_EE_A_TX_FN 1
#define A311D_UART_EE_A_RX A311D_GPIOX(13)
#define A311D_UART_EE_A_RX_FN 1
#define A311D_UART_EE_A_CTS A311D_GPIOX(14)
#define A311D_UART_EE_A_CTS_FN 1
#define A311D_UART_EE_A_RTS A311D_GPIOX(15)
#define A311D_UART_EE_A_RTS_FN 1

// Mali G52 MP4 r0p0
#define A311D_MALI_BASE 0xffe40000
#define A311D_MALI_LENGTH 0x40000

// Temperature
#define A311D_TEMP_SENSOR_PLL_BASE 0xff634800
#define A311D_TEMP_SENSOR_PLL_LENGTH 0x80

#define A311D_TEMP_SENSOR_DDR_BASE 0xff634c00
#define A311D_TEMP_SENSOR_DDR_LENGTH 0x80

// These registers are used to derive calibration data for the temperature sensors. The registers
// are not documented in the datasheet - they were copied over from u-boot/Cast code.
#define A311D_TEMP_SENSOR_PLL_TRIM 0xff800268
#define A311D_TEMP_SENSOR_DDR_TRIM 0xff800230
#define A311D_TEMP_SENSOR_TRIM_LENGTH 0x4

// Display Related
#define A311D_DMC_BASE 0xff638000
#define A311D_DMC_LENGTH 0x2000

// The Memory Map in A311D Datasheet Section 8.1 (page 78-82) lists "HDMITX"
// as the whole region from 0xff60'0000 to 0xff60'ffff.
//
// However, A311D Datasheet Section 10.2.3.43 "HDMITX Top-Level and HDMI TX
// Controller IP Register Access" (page 1135) states that their data widths and
// the ways to access top-level and controller IP registers are different, so
// it makes more sense for the driver to separate the region into two
// sub-regions (HDMITX_CONTROLLER_IP and HDMITX_TOP_LEVEL) so that the driver
// can apply different memory access policies on different MMIO regions.
#define A311D_HDMITX_CONTROLLER_IP_BASE 0xff600000
#define A311D_HDMITX_CONTROLLER_IP_LENGTH 0x8000
#define A311D_HDMITX_TOP_LEVEL_BASE 0xff608000
#define A311D_HDMITX_TOP_LEVEL_LENGTH 0x8000

#define A311D_VPU_BASE 0xff900000
#define A311D_VPU_LENGTH 0x40000
#define A311D_CBUS_BASE 0xff634400
#define A311D_CBUS_LENGTH 0x100000
#define A311D_AOBUS_BASE 0xff800000
#define A311D_AOBUS_LENGTH 0x100000
#define A311D_FULL_CBUS_BASE 0xffd00000
#define A311D_FULL_CBUS_LENGTH 0x100000

#define A311D_MIPI_DSI_BASE 0xffd07000
#define A311D_MIPI_DSI_LENGTH 0x1000

#define A311D_DSI_PHY_BASE 0xff644000
#define A311D_DSI_PHY_LENGTH 0x2000

#define A311D_TOP_MIPI_DSI_BASE 0xffd073C0
#define A311D_TOP_MIPI_DSI_LENGTH 0x40

#define A311D_MSR_CLK_BASE 0xffd18000
#define A311D_MSR_CLK_LENGTH 0x1000

#define A311D_SARADC_BASE 0xff809000
#define A311D_SARADC_LENGTH 0x1000

#endif  // SRC_DEVICES_LIB_AMLOGIC_INCLUDE_SOC_AML_A311D_A311D_HW_H_
