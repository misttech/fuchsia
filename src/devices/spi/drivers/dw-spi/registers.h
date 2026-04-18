// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_SPI_DRIVERS_DW_SPI_REGISTERS_H_
#define SRC_DEVICES_SPI_DRIVERS_DW_SPI_REGISTERS_H_

#include <zircon/types.h>

#include <hwreg/bitfields.h>

namespace dw_spi {

constexpr uint32_t DW_SPI_CTRLR0 = 0x00;
constexpr uint32_t DW_SPI_CTRLR1 = 0x04;
constexpr uint32_t DW_SPI_SSIENR = 0x08;
constexpr uint32_t DW_SPI_MWCR = 0x0c;
constexpr uint32_t DW_SPI_SER = 0x10;
constexpr uint32_t DW_SPI_BAUDR = 0x14;
constexpr uint32_t DW_SPI_TXFTLR = 0x18;
constexpr uint32_t DW_SPI_RXFTLR = 0x1c;
constexpr uint32_t DW_SPI_TXFLR = 0x20;
constexpr uint32_t DW_SPI_RXFLR = 0x24;
constexpr uint32_t DW_SPI_SR = 0x28;
constexpr uint32_t DW_SPI_IMR = 0x2c;
constexpr uint32_t DW_SPI_ISR = 0x30;
constexpr uint32_t DW_SPI_RISR = 0x34;
constexpr uint32_t DW_SPI_TXOICR = 0x38;
constexpr uint32_t DW_SPI_RXOICR = 0x3c;
constexpr uint32_t DW_SPI_RXUICR = 0x40;
constexpr uint32_t DW_SPI_MSTICR = 0x44;
constexpr uint32_t DW_SPI_ICR = 0x48;
constexpr uint32_t DW_SPI_DMACR = 0x4c;
constexpr uint32_t DW_SPI_DMATDLR = 0x50;
constexpr uint32_t DW_SPI_DMARDLR = 0x54;
constexpr uint32_t DW_SPI_IDR = 0x58;
constexpr uint32_t DW_SPI_VERSION = 0x5c;
constexpr uint32_t DW_SPI_DR0 = 0x60;

class CtrlR0 : public hwreg::RegisterBase<CtrlR0, uint32_t, hwreg::EnablePrinter> {
 public:
  DEF_FIELD(22, 21, spi_frf);
  DEF_FIELD(20, 16, dfs_32);
  DEF_FIELD(15, 12, cfs);
  DEF_BIT(11, srl);
  DEF_BIT(10, slv_oe);
  DEF_FIELD(9, 8, tmod);
  DEF_BIT(7, scpol);
  DEF_BIT(6, scph);
  DEF_FIELD(5, 4, frf);
  DEF_FIELD(3, 0, dfs);

  static auto Get() { return hwreg::RegisterAddr<CtrlR0>(DW_SPI_CTRLR0); }
};

class CtrlR1 : public hwreg::RegisterBase<CtrlR1, uint32_t, hwreg::EnablePrinter> {
 public:
  DEF_FIELD(15, 0, ndf);

  static auto Get() { return hwreg::RegisterAddr<CtrlR1>(DW_SPI_CTRLR1); }
};

class SsiEnr : public hwreg::RegisterBase<SsiEnr, uint32_t, hwreg::EnablePrinter> {
 public:
  DEF_BIT(0, ssi_en);

  static auto Get() { return hwreg::RegisterAddr<SsiEnr>(DW_SPI_SSIENR); }
};

class Ser : public hwreg::RegisterBase<Ser, uint32_t, hwreg::EnablePrinter> {
 public:
  // Bits are slave select lines, up to 16.
  DEF_FIELD(15, 0, ser);

  static auto Get() { return hwreg::RegisterAddr<Ser>(DW_SPI_SER); }
};

class Baudr : public hwreg::RegisterBase<Baudr, uint32_t, hwreg::EnablePrinter> {
 public:
  DEF_FIELD(15, 0, sckdv);

  static auto Get() { return hwreg::RegisterAddr<Baudr>(DW_SPI_BAUDR); }
};

class Sr : public hwreg::RegisterBase<Sr, uint32_t, hwreg::EnablePrinter> {
 public:
  DEF_BIT(6, dcol);
  DEF_BIT(5, txe);
  DEF_BIT(4, rff);
  DEF_BIT(3, rfne);
  DEF_BIT(2, tfe);
  DEF_BIT(1, tfnf);
  DEF_BIT(0, busy);

  static auto Get() { return hwreg::RegisterAddr<Sr>(DW_SPI_SR); }
};

class Imr : public hwreg::RegisterBase<Imr, uint32_t, hwreg::EnablePrinter> {
 public:
  DEF_BIT(5, mstim);
  DEF_BIT(4, rxfim);
  DEF_BIT(3, rxoim);
  DEF_BIT(2, rxuim);
  DEF_BIT(1, txoim);
  DEF_BIT(0, txeim);

  static auto Get() { return hwreg::RegisterAddr<Imr>(DW_SPI_IMR); }
};

}  // namespace dw_spi

#endif  // SRC_DEVICES_SPI_DRIVERS_DW_SPI_REGISTERS_H_
