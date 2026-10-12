// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/uart/dw8250.h>
#include <lib/uart/mock.h>
#include <lib/uart/uart.h>

#include <zxtest/zxtest.h>

namespace {

using SimpleTestDriver = uart::KernelDriver<uart::dw8250::Driver, uart::mock::IoProvider,
                                            uart::UnsynchronizedPolicy, uart::mock::IrqProvider>;
constexpr zbi_dcfg_simple_t kTestConfig = {};

template <typename Mock>
void AppendInitPrologue(Mock& mock) {
  mock
      // Init()
      .ExpectRead(uint32_t{0x103d32}, 0x3d)  // ComponentParameterRegister read
                                             // From real hardware:
                                             // 0x103d32 =
                                             //   256 byte fifo
                                             //   DMA_EXTRA
                                             //   UART_ADD_ENCODED_PARAMS
                                             //   SHADOW
                                             //   FIFO_STAT
                                             //   ADDITIONAL_FAT
                                             //   THRE_MODE
                                             //   AFCE_MODE
                                             //   APB_DATA_WIDTH = 2
      .ExpectRead(uint32_t{0x60}, 5)         // LineStatus (THRE and TEMT set: TX drained)
      .ExpectWrite(uint32_t{0x80}, 1)        // InterruptEnableRegister write
      .ExpectWrite(uint32_t{0x17}, 2)        // FifoControlRegister write
      .ExpectWrite(uint32_t{0x03}, 4)        // ModemControlRegister write
      .ExpectRead(uint32_t{0x00}, 31)        // UartStatus (not busy)
      .ExpectRead(uint32_t{0x03}, 3);        // LineControl (8N1)
}

template <typename Mock>
void AppendInitSequence(Mock& mock) {
  AppendInitPrologue(mock);
  mock
      // Record the divisor.
      .ExpectRead(uint32_t{0x00}, 31)  // UartStatus (not busy)
      .ExpectWrite(uint32_t{0x83}, 3)  // LineControl (8N1 | DLAB)
      .ExpectRead(uint32_t{0x83}, 3)   // LineControl readback (took effect)
      .ExpectRead(uint32_t{0x0d}, 0)   // DivisorLatchLower
      .ExpectRead(uint32_t{0x01}, 1)   // DivisorLatchUpper
      .ExpectRead(uint32_t{0x00}, 31)  // UartStatus (not busy)
      .ExpectWrite(uint32_t{0x03}, 3)  // LineControl (8N1)
      .ExpectRead(uint32_t{0x03}, 3)   // LineControl readback (took effect)
      // End of Init()
      ;
}

// The sequence of register accesses made by PrepareForSuspend.
template <typename Mock>
void AppendPrepareForSuspendSequence(Mock& mock) {
  mock.ExpectRead(uint32_t{0x85}, 1)   // InterruptEnable (PTIME | ELSI | ERBFI)
      .ExpectWrite(uint32_t{0x00}, 1)  // InterruptEnable (all disabled)
      .ExpectRead(uint32_t{0x03}, 4)   // ModemControl (RTS | DTR)
      .ExpectRead(uint32_t{0x03}, 3);  // LineControl (8N1)
}

// The sequence of register accesses made by WakeupFromSuspend after the state
// above has been saved (with a divisor of 0x010d), when the UART is never busy
// and every LCR write takes effect.
template <typename Mock>
void AppendWakeupFromSuspendSequence(Mock& mock) {
  mock.ExpectRead(uint32_t{0x00}, 31)   // UartStatus (not busy)
      .ExpectWrite(uint32_t{0x83}, 3)   // LineControl (8N1 | DLAB)
      .ExpectRead(uint32_t{0x83}, 3)    // LineControl readback (took effect)
      .ExpectWrite(uint32_t{0x0d}, 0)   // DivisorLatchLower
      .ExpectWrite(uint32_t{0x01}, 1)   // DivisorLatchUpper
      .ExpectRead(uint32_t{0x00}, 31)   // UartStatus (not busy)
      .ExpectWrite(uint32_t{0x03}, 3)   // LineControl (8N1)
      .ExpectRead(uint32_t{0x03}, 3)    // LineControl readback (took effect)
      .ExpectWrite(uint32_t{0x17}, 2)   // FifoControl
      .ExpectWrite(uint32_t{0x03}, 4)   // ModemControl
      .ExpectWrite(uint32_t{0x85}, 1);  // InterruptEnable
}

TEST(Dw8250Tests, HelloWorld) {
  SimpleTestDriver driver(kTestConfig);

  AppendInitSequence(driver.io().mock());
  driver.io()
      .mock()
      // Write()
      .ExpectRead(uint32_t{0b000'0010}, 31)  // UserStatus.TFNF (tx fifo not full)
      .ExpectRead(uint32_t{0x100}, 32)       // TransmitFifoLevel (reads 256)
      .ExpectWrite(uint32_t{'h'}, 0)         // Write
      .ExpectWrite(uint32_t{'i'}, 0)
      .ExpectWrite(uint32_t{'\r'}, 0)
      .ExpectWrite(uint32_t{'\n'}, 0);

  driver.Init();
  EXPECT_EQ(3, driver.Write("hi\n"));
}

TEST(Dw8250Tests, SetLineControl8N1) {
  SimpleTestDriver driver(kTestConfig);

  AppendInitSequence(driver.io().mock());
  driver.io()
      .mock()
      // SetLineControl()
      .ExpectRead(uint32_t{0b0000'0000}, 31)  // UserStatus
      .ExpectWrite(uint32_t{0b1000'0000}, 3)  // LineControl setting divisor latch access
      .ExpectRead(uint32_t{0b1000'0000}, 3)   // LineControl readback (took effect)
      .ExpectWrite(uint32_t{0b0000'0001}, 0)  // Divisor to 1
      .ExpectWrite(uint32_t{0b0000'0000}, 1)
      .ExpectRead(uint32_t{0b0000'0000}, 31)   // UserStatus
      .ExpectWrite(uint32_t{0b0000'0011}, 3);  // LineControl setting 8N1

  driver.Init();
  driver.SetLineControl(uart::DataBits::k8, uart::Parity::kNone, uart::StopBits::k1);
}

TEST(Dw8250Tests, SetLineControl7E1) {
  SimpleTestDriver driver(kTestConfig);

  AppendInitSequence(driver.io().mock());
  driver.io()
      .mock()
      // SetLineControl()
      .ExpectRead(uint32_t{0b0000'0000}, 31)  // UserStatus
      .ExpectWrite(uint32_t{0b1000'0000}, 3)  // LineControl setting divisor latch access
      .ExpectRead(uint32_t{0b1000'0000}, 3)   // LineControl readback (took effect)
      .ExpectWrite(uint32_t{0b0000'0001}, 0)  // Divisor to 1
      .ExpectWrite(uint32_t{0b0000'0000}, 1)
      .ExpectRead(uint32_t{0b0000'0000}, 31)   // UserStatus
      .ExpectWrite(uint32_t{0b0001'1010}, 3);  // LineControl setting 7E1

  driver.Init();
  driver.SetLineControl(uart::DataBits::k7, uart::Parity::kEven, uart::StopBits::k1);
}

// If the DLAB write in SetLineControl is ignored (because the UART became busy
// after the USR.BUSY check), SetLineControl must not write the divisor (which
// would land in THR and IER) or record it.
TEST(Dw8250Tests, SetLineControlIgnoredDlabWrite) {
  SimpleTestDriver driver(kTestConfig);

  auto& mock = driver.io().mock();
  AppendInitPrologue(mock);
  // Init is unable to record the divisor.
  for (size_t i = 0; i < uart::dw8250::kLcrWriteAttempts; ++i) {
    mock.ExpectRead(uint32_t{0x00}, 31)  // UartStatus (not busy)
        .ExpectWrite(uint32_t{0x83}, 3)  // LineControl (8N1 | DLAB)
        .ExpectRead(uint32_t{0x03}, 3);  // LineControl readback (ignored)
  }
  mock
      // SetLineControl()
      .ExpectRead(uint32_t{0x00}, 31)  // UartStatus (not busy)
      .ExpectWrite(uint32_t{0x80}, 3)  // LineControl setting divisor latch access
      .ExpectRead(uint32_t{0x03}, 3);  // LineControl readback (ignored, UART became busy)
  AppendPrepareForSuspendSequence(mock);
  mock
      // WakeupFromSuspend() leaves the divisor alone, since none was recorded.
      .ExpectRead(uint32_t{0x00}, 31)   // UartStatus (not busy)
      .ExpectWrite(uint32_t{0x03}, 3)   // LineControl (8N1)
      .ExpectRead(uint32_t{0x03}, 3)    // LineControl readback (took effect)
      .ExpectWrite(uint32_t{0x17}, 2)   // FifoControl
      .ExpectWrite(uint32_t{0x03}, 4)   // ModemControl
      .ExpectWrite(uint32_t{0x85}, 1);  // InterruptEnable

  driver.Init();
  driver.SetLineControl(uart::DataBits::k8, uart::Parity::kNone, uart::StopBits::k1);
  driver.PrepareForSuspend();
  driver.WakeupFromSuspend();
}

TEST(Dw8250Tests, Write) {
  SimpleTestDriver driver(kTestConfig);

  // Write using the expected FIFO_STAT mode
  AppendInitSequence(driver.io().mock());
  driver.io()
      .mock()
      // Write()
      .ExpectRead(uint32_t{0b000'0000}, 31)  // UserStatus.TFNF (tx fifo full)
      // TX fifo is full, poll until it isn't.
      .ExpectRead(uint32_t{0b000'0010}, 31)  // UserStatus.TFNF (tx fifo not full)
      .ExpectRead(uint32_t{0x100 - 2}, 32)   // TransmitFifoLevel (reads 256 - 2)
      // Only 2 bytes available in the tx fifo.
      .ExpectWrite(uint32_t{'a'}, 0)  // Write
      .ExpectWrite(uint32_t{'b'}, 0)
      // Go back to polling if the fifo has space in it.
      .ExpectRead(uint32_t{0b000'0000}, 31)  // UserStatus.TFNF (tx fifo full)
      // TX fifo is full, poll until it isn't.
      .ExpectRead(uint32_t{0b000'0010}, 31)  // UserStatus.TFNF (tx fifo not full)
      .ExpectRead(uint32_t{0x100}, 32)       // TransmitFifoLevel (reads 256)
      // Write the rest of the message.
      .ExpectWrite(uint32_t{'c'}, 0)
      .ExpectWrite(uint32_t{'d'}, 0)
      .ExpectWrite(uint32_t{'e'}, 0)
      .ExpectWrite(uint32_t{'f'}, 0);

  driver.Init();
  EXPECT_EQ(6, driver.Write("abcdef"));
}

TEST(Dw8250Tests, Read) {
  SimpleTestDriver driver(kTestConfig);

  AppendInitSequence(driver.io().mock());
  driver.io()
      .mock()
      // Write()
      .ExpectRead(uint32_t{0b000'0010}, 31)  // UserStatus.TFNF (tx fifo not full)
      .ExpectRead(uint32_t{0x100}, 32)       // TransmitFifoLevel (reads 256)
      .ExpectWrite(uint32_t{'?'}, 0)         // Write
      .ExpectWrite(uint32_t{'\r'}, 0)
      .ExpectWrite(uint32_t{'\n'}, 0)
      // Read()
      .ExpectRead(uint32_t{0b0110'0001}, 5)  // Read (data_ready)
      .ExpectRead(uint32_t{'q'}, 0)          // Read (data)
      // Read()
      .ExpectRead(uint32_t{0b0110'0001}, 5)  // Read (data_ready)
      .ExpectRead(uint32_t{'\n'}, 0);        // Read (data)

  driver.Init();
  EXPECT_EQ(2, driver.Write("?\n"));
  EXPECT_EQ(uint32_t{'q'}, driver.Read());
  EXPECT_EQ(uint32_t{'\n'}, driver.Read());
}

TEST(Dw8250Tests, TxDrained) {
  SimpleTestDriver driver(kTestConfig);

  AppendInitSequence(driver.io().mock());
  driver.io()
      .mock()
      .ExpectRead(uint32_t{0b0010'0000}, 5)   // LineStatus (THRE set, TEMT clear)
      .ExpectRead(uint32_t{0b0110'0000}, 5);  // LineStatus (THRE and TEMT set)

  driver.Init();
  EXPECT_FALSE(driver.TxDrained());
  EXPECT_TRUE(driver.TxDrained());
}

TEST(Dw8250Tests, SuspendResume) {
  SimpleTestDriver driver(kTestConfig);

  auto& mock = driver.io().mock();
  AppendInitSequence(mock);
  // The divisor was recorded by Init, so PrepareForSuspend does not touch it.
  AppendPrepareForSuspendSequence(mock);
  AppendWakeupFromSuspendSequence(mock);

  driver.Init();
  driver.PrepareForSuspend();
  // A second suspend is a no-op.
  driver.PrepareForSuspend();
  driver.WakeupFromSuspend();
  // A second wakeup is a no-op.
  driver.WakeupFromSuspend();
}

// Init must let output queued by a previous user of the UART drain before
// resetting the FIFOs or touching LCR, and then wait for any reception in
// progress to finish before touching LCR.
TEST(Dw8250Tests, InitWaitsForTxDrained) {
  SimpleTestDriver driver(kTestConfig);

  auto& mock = driver.io().mock();
  mock.ExpectRead(uint32_t{0x103d32}, 0x3d)  // ComponentParameterRegister read
      .ExpectRead(uint32_t{0x00}, 5)         // LineStatus (TX FIFO not empty)
      .ExpectRead(uint32_t{0x20}, 5)         // LineStatus (THRE set, TEMT clear)
      .ExpectRead(uint32_t{0x60}, 5)         // LineStatus (THRE and TEMT set: TX drained)
      .ExpectWrite(uint32_t{0x80}, 1)        // InterruptEnableRegister write
      .ExpectWrite(uint32_t{0x17}, 2)        // FifoControlRegister write
      .ExpectWrite(uint32_t{0x03}, 4)        // ModemControlRegister write
      .ExpectRead(uint32_t{0x01}, 31)        // UartStatus (busy, e.g. RX in progress)
      .ExpectRead(uint32_t{0x00}, 31)        // UartStatus (not busy)
      .ExpectRead(uint32_t{0x03}, 3)         // LineControl (8N1)
      .ExpectRead(uint32_t{0x00}, 31)        // UartStatus (not busy)
      .ExpectWrite(uint32_t{0x83}, 3)        // LineControl (8N1 | DLAB)
      .ExpectRead(uint32_t{0x83}, 3)         // LineControl readback (took effect)
      .ExpectRead(uint32_t{0x0d}, 0)         // DivisorLatchLower
      .ExpectRead(uint32_t{0x01}, 1)         // DivisorLatchUpper
      .ExpectRead(uint32_t{0x00}, 31)        // UartStatus (not busy)
      .ExpectWrite(uint32_t{0x03}, 3)        // LineControl (8N1)
      .ExpectRead(uint32_t{0x03}, 3);        // LineControl readback (took effect)

  driver.Init();
}

// LCR must never be written while USR.BUSY is set.  Instead the driver forces
// the UART idle and checks again.
TEST(Dw8250Tests, InitDoesNotWriteLcrWhileBusy) {
  SimpleTestDriver driver(kTestConfig);

  auto& mock = driver.io().mock();
  AppendInitPrologue(mock);
  mock
      // Record the divisor.
      .ExpectRead(uint32_t{0x01}, 31)  // UartStatus (busy, e.g. RX in progress)
      .ExpectWrite(uint32_t{0x17}, 2)  // FifoControl (reset FIFOs to force idle)
      .ExpectRead(uint32_t{0x00}, 0)   // RxBuffer (drain)
      .ExpectRead(uint32_t{0x00}, 31)  // UartStatus (not busy)
      .ExpectWrite(uint32_t{0x83}, 3)  // LineControl (8N1 | DLAB)
      .ExpectRead(uint32_t{0x83}, 3)   // LineControl readback (took effect)
      .ExpectRead(uint32_t{0x0d}, 0)   // DivisorLatchLower
      .ExpectRead(uint32_t{0x01}, 1)   // DivisorLatchUpper
      .ExpectRead(uint32_t{0x01}, 31)  // UartStatus (busy)
      .ExpectWrite(uint32_t{0x17}, 2)  // FifoControl (reset FIFOs to force idle)
      .ExpectRead(uint32_t{0x00}, 0)   // DivisorLatchLower (drain, DLAB still set)
      .ExpectRead(uint32_t{0x00}, 31)  // UartStatus (not busy)
      .ExpectWrite(uint32_t{0x03}, 3)  // LineControl (8N1)
      .ExpectRead(uint32_t{0x03}, 3);  // LineControl readback (took effect)
  // The divisor was recorded, so it is restored on wakeup.
  AppendPrepareForSuspendSequence(mock);
  AppendWakeupFromSuspendSequence(mock);

  driver.Init();
  driver.PrepareForSuspend();
  driver.WakeupFromSuspend();
}

// If an LCR write is ignored anyway (because the UART became busy between the
// USR.BUSY check and the write), the driver must notice (by reading LCR back)
// and try again.  In particular, it must not touch the divisor latch unless
// DLAB was actually set, since otherwise the "divisor" writes would land in THR
// and IER.
TEST(Dw8250Tests, WakeupRetriesIgnoredLcrWrite) {
  SimpleTestDriver driver(kTestConfig);

  auto& mock = driver.io().mock();
  AppendInitSequence(mock);
  AppendPrepareForSuspendSequence(mock);
  mock
      // WakeupFromSuspend()
      .ExpectRead(uint32_t{0x00}, 31)   // UartStatus (not busy)
      .ExpectWrite(uint32_t{0x83}, 3)   // LineControl (8N1 | DLAB)
      .ExpectRead(uint32_t{0x00}, 3)    // LineControl readback (ignored, UART became busy)
      .ExpectRead(uint32_t{0x01}, 31)   // UartStatus (busy)
      .ExpectWrite(uint32_t{0x17}, 2)   // FifoControl (reset FIFOs to force idle)
      .ExpectRead(uint32_t{0x00}, 0)    // RxBuffer (drain)
      .ExpectRead(uint32_t{0x00}, 31)   // UartStatus (not busy)
      .ExpectWrite(uint32_t{0x83}, 3)   // LineControl (8N1 | DLAB)
      .ExpectRead(uint32_t{0x83}, 3)    // LineControl readback (took effect)
      .ExpectWrite(uint32_t{0x0d}, 0)   // DivisorLatchLower
      .ExpectWrite(uint32_t{0x01}, 1)   // DivisorLatchUpper
      .ExpectRead(uint32_t{0x00}, 31)   // UartStatus (not busy)
      .ExpectWrite(uint32_t{0x03}, 3)   // LineControl (8N1)
      .ExpectRead(uint32_t{0x83}, 3)    // LineControl readback (ignored, UART became busy)
      .ExpectRead(uint32_t{0x00}, 31)   // UartStatus (not busy)
      .ExpectWrite(uint32_t{0x03}, 3)   // LineControl (8N1)
      .ExpectRead(uint32_t{0x03}, 3)    // LineControl readback (took effect)
      .ExpectWrite(uint32_t{0x17}, 2)   // FifoControl
      .ExpectWrite(uint32_t{0x03}, 4)   // ModemControl
      .ExpectWrite(uint32_t{0x85}, 1);  // InterruptEnable

  driver.Init();
  driver.PrepareForSuspend();
  driver.WakeupFromSuspend();
}

// If Init is unable to set LCR.DLAB, no divisor is recorded.  PrepareForSuspend
// does not try again (it must not wait for the UART to go idle), and
// WakeupFromSuspend leaves the divisor latch alone rather than programming
// garbage.
TEST(Dw8250Tests, WakeupWithoutDivisor) {
  SimpleTestDriver driver(kTestConfig);

  auto& mock = driver.io().mock();
  AppendInitPrologue(mock);
  // Every attempt to set DLAB is ignored.
  for (size_t i = 0; i < uart::dw8250::kLcrWriteAttempts; ++i) {
    mock.ExpectRead(uint32_t{0x00}, 31)  // UartStatus (not busy)
        .ExpectWrite(uint32_t{0x83}, 3)  // LineControl (8N1 | DLAB)
        .ExpectRead(uint32_t{0x03}, 3);  // LineControl readback (ignored)
  }
  AppendPrepareForSuspendSequence(mock);
  mock
      // WakeupFromSuspend()
      .ExpectRead(uint32_t{0x00}, 31)   // UartStatus (not busy)
      .ExpectWrite(uint32_t{0x03}, 3)   // LineControl (8N1)
      .ExpectRead(uint32_t{0x03}, 3)    // LineControl readback (took effect)
      .ExpectWrite(uint32_t{0x17}, 2)   // FifoControl
      .ExpectWrite(uint32_t{0x03}, 4)   // ModemControl
      .ExpectWrite(uint32_t{0x85}, 1);  // InterruptEnable

  driver.Init();
  driver.PrepareForSuspend();
  driver.WakeupFromSuspend();
}

// If DLAB is set but can't be cleared again, the divisor is not recorded (the
// capture did not succeed), so WakeupFromSuspend leaves the divisor latch
// alone.
TEST(Dw8250Tests, DivisorNotRecordedIfDlabCannotBeCleared) {
  SimpleTestDriver driver(kTestConfig);

  auto& mock = driver.io().mock();
  AppendInitPrologue(mock);
  mock.ExpectRead(uint32_t{0x00}, 31)  // UartStatus (not busy)
      .ExpectWrite(uint32_t{0x83}, 3)  // LineControl (8N1 | DLAB)
      .ExpectRead(uint32_t{0x83}, 3)   // LineControl readback (took effect)
      .ExpectRead(uint32_t{0x0d}, 0)   // DivisorLatchLower
      .ExpectRead(uint32_t{0x01}, 1);  // DivisorLatchUpper
  // Every attempt to clear DLAB is ignored.
  for (size_t i = 0; i < uart::dw8250::kLcrWriteAttempts; ++i) {
    mock.ExpectRead(uint32_t{0x00}, 31)  // UartStatus (not busy)
        .ExpectWrite(uint32_t{0x03}, 3)  // LineControl (8N1)
        .ExpectRead(uint32_t{0x83}, 3);  // LineControl readback (ignored)
  }
  mock
      // PrepareForSuspend()
      .ExpectRead(uint32_t{0x85}, 1)   // InterruptEnable (PTIME | ELSI | ERBFI)
      .ExpectWrite(uint32_t{0x00}, 1)  // InterruptEnable (all disabled)
      .ExpectRead(uint32_t{0x03}, 4)   // ModemControl (RTS | DTR)
      .ExpectRead(uint32_t{0x83}, 3)   // LineControl (8N1 | DLAB, still stuck)
      // WakeupFromSuspend() restores LCR (with DLAB clear) but not the divisor.
      .ExpectRead(uint32_t{0x00}, 31)   // UartStatus (not busy)
      .ExpectWrite(uint32_t{0x03}, 3)   // LineControl (8N1)
      .ExpectRead(uint32_t{0x03}, 3)    // LineControl readback (took effect)
      .ExpectWrite(uint32_t{0x17}, 2)   // FifoControl
      .ExpectWrite(uint32_t{0x03}, 4)   // ModemControl
      .ExpectWrite(uint32_t{0x85}, 1);  // InterruptEnable

  driver.Init();
  driver.PrepareForSuspend();
  driver.WakeupFromSuspend();
}

// If WakeupFromSuspend sets DLAB but can't clear it again, it must not restore
// IER, since that write would land in DLH and corrupt the divisor.
TEST(Dw8250Tests, WakeupDoesNotRestoreIerIfDlabCannotBeCleared) {
  SimpleTestDriver driver(kTestConfig);

  auto& mock = driver.io().mock();
  AppendInitSequence(mock);
  AppendPrepareForSuspendSequence(mock);
  mock
      // WakeupFromSuspend()
      .ExpectRead(uint32_t{0x00}, 31)   // UartStatus (not busy)
      .ExpectWrite(uint32_t{0x83}, 3)   // LineControl (8N1 | DLAB)
      .ExpectRead(uint32_t{0x83}, 3)    // LineControl readback (took effect)
      .ExpectWrite(uint32_t{0x0d}, 0)   // DivisorLatchLower
      .ExpectWrite(uint32_t{0x01}, 1);  // DivisorLatchUpper
  // Every attempt to clear DLAB is ignored.
  for (size_t i = 0; i < uart::dw8250::kLcrWriteAttempts; ++i) {
    mock.ExpectRead(uint32_t{0x00}, 31)  // UartStatus (not busy)
        .ExpectWrite(uint32_t{0x03}, 3)  // LineControl (8N1)
        .ExpectRead(uint32_t{0x83}, 3);  // LineControl readback (ignored)
  }
  mock.ExpectWrite(uint32_t{0x17}, 2)   // FifoControl
      .ExpectWrite(uint32_t{0x03}, 4);  // ModemControl
  // No InterruptEnable write.

  driver.Init();
  driver.PrepareForSuspend();
  driver.WakeupFromSuspend();
}

TEST(Dw8250Tests, WakeupWithoutSuspendIsNoop) {
  SimpleTestDriver driver(kTestConfig);

  AppendInitSequence(driver.io().mock());

  driver.Init();
  driver.WakeupFromSuspend();
}

}  // namespace
