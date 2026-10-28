# Universal Flash Storage (UFS) Specification & Protocol Reference

This document provides a technical reference for the JEDEC Universal Flash
Storage (UFS 3.1/4.0, JESD220/JESD223) specifications implemented by the
Fuchsia UFS driver.

---

## Table of Contents

1. [High-Level UFS Overview & Architecture](#high-level-ufs-overview--architecture)
2. [UFS & SCSI Protocol Reference](#ufs--scsi-protocol-reference)
   - [SCSI Architecture Model (SAM) & SCSI Commands](#scsi-architecture-model-sam--scsi-commands)
   - [UFS Protocol Information Units (UPIU)](#ufs-protocol-information-units-upiu)
   - [Admin / Native Commands & Query Requests](#admin--native-commands--query-requests)
   - [Task Management Requests](#task-management-requests)
   - [LUNs vs Well-Known LUNs (W-LUNs)](#luns-vs-well-known-luns-w-luns)
   - [UniPro Interconnect & UIC DME Commands](#unipro-interconnect--uic-dme-commands)
   - [UFSHCI DMA & Register Interface](#ufshci-dma--register-interface)
3. [UFS 4.0 vs UFS 3.1 & MCQ Architecture](#ufs-40-vs-ufs-31--mcq-architecture)
   - [Architectural & Hardware Comparison Table](#architectural--hardware-comparison-table)
   - [Multi-Circular Queue (MCQ) Overview](#multi-circular-queue-mcq-overview)

---

## High-Level UFS Overview & Architecture

Universal Flash Storage (UFS, JEDEC UFS 3.1/4.0 section 2) is a
high-performance, low-power non-volatile storage standard for mobile and
embedded systems. Unlike eMMC (which uses a shared, half-duplex parallel bus),
UFS employs:

- **Full-Duplex Serial Interface**: Uses MIPI M-PHY differential signaling with
  dedicated differential transmit (TX) and receive (RX) lanes.
- **Layered Protocol Stack**: Separates application commands, transport
  packetization, network routing, and physical signaling.
- **Queued Execution**: Supports multiple simultaneous command queues via UTP
  Transfer Request Descriptors (UTRD) or Multi-Circular Queues (MCQ), plus Task
  Management Request Descriptors (UTMRD).

```
+-----------------------------------------------------------------------+
|                       UFS Application Layer (UAP)                     |
|  +---------------------------+  +----------------+  +---------------+ |
|  | UFS Command Set (SCSI/SBC)|  | Device Manager |  | Task Manager  | |
|  +---------------------------+  +----------------+  +---------------+ |
+-----------------------------------------------------------------------+
|                     UFS Transport Protocol Layer (UTP)                |
|           UPIU Packetization / UTRD Descriptors / PRDT DMA Tables     |
+-----------------------------------------------------------------------+
|                     UFS InterConnect Layer (UIC)                      |
|  +-----------------------------------------------------------------+  |
|  | MIPI UniPro: Transport, Network, Data Link, PHY Adapter (DME)   |  |
|  +-----------------------------------------------------------------+  |
|  | MIPI M-PHY: Physical Layer (Differential Transceivers / Gears)  |  |
|  +-----------------------------------------------------------------+  |
+-----------------------------------------------------------------------+
```

### Key Protocol Layers

1. **UFS Application Layer (UAP, UFS 3.1/4.0 section 11 - 14)**:
   - **UFS Command Set (UCS)**: Implements standard SCSI Primary Commands (SPC)
     and SCSI Block Commands (SBC) for block storage operations.
   - **Device Manager**: Manages device-level operations, power modes, flags,
     attributes, and descriptors via Query UPIU transactions.
   - **Task Manager**: Manages aborts, task sets, and logical unit resets via
     Task Management UPIUs.
2. **UFS Transport Protocol Layer (UTP, UFS 3.1/4.0 section 10)**:
   - Encapsulates commands, data, and responses into **UFS Protocol Information
     Units (UPIUs)**.
   - Manages host-side DMA transfer descriptor lists (UTRD / MCQ) and task
     management lists (UTMRD).
3. **UFS InterConnect Layer (UIC, UFS 3.1/4.0 section 5 - 9)**:
   - **MIPI UniPro**: Provides point-to-point packet delivery, flow control,
     CRC checking, retransmission, and Device Management Entity (DME) access.
   - **MIPI M-PHY**: Physical differential signaling layer operating in
     low-speed (PWM-G1 through PWM-G7) or high-speed (HS-G1 through HS-G5,
     Rate A & B) gears.

---

## UFS & SCSI Protocol Reference

### SCSI Architecture Model (SAM) & SCSI Commands

UFS adopts a subset of the SCSI command set for block storage operations (UFS
3.1/4.0 section 11.3). The host constructs a **Command Descriptor Block (CDB)**
containing an opcode, target Logical Unit Number (LUN), logical block address
(LBA), transfer length, and control flags, wrapped inside a **Command UPIU**.

#### Primary SCSI Commands Used in UFS

| Command | Opcode | Size | Description & Usage |
| :--- | :---: | :---: | :--- |
| **`TEST_UNIT_READY`** | `0x00` | 6B | Checks if target LUN is ready to accept commands. |
| **`REQUEST_SENSE`** | `0x03` | 6B | Retrieves sense key, ASC, and ASCQ after `CHECK_CONDITION`. |
| **`INQUIRY`** | `0x12` | 6B | Queries vendor ID, product ID, revision, and VPD pages. |
| **`MODE_SENSE_10`** | `0x5A` | 10B | Reads device mode pages (e.g. Caching Page). |
| **`START_STOP_UNIT`** | `0x1B` | 6B | Changes LUN power condition (Active, Idle, PowerDown). |
| **`READ_CAPACITY_10 / 16`** | `0x25 / 0x9E` | 10/16B | Queries block count and block size. |
| **`READ_10 / READ_16`** | `0x28 / 0x88` | 10/16B | Reads logical blocks via PRDT DMA. |
| **`WRITE_10 / WRITE_16`** | `0x2A / 0x8A` | 10/16B | Writes logical blocks via PRDT DMA. |
| **`SYNCHRONIZE_CACHE_10 / 16`** | `0x35 / 0x91` | 10/16B | Flushes volatile cache to flash. |
| **`UNMAP`** | `0x42` | 10B | TRIM / Discard command to deallocate logical block ranges. |
| **`REPORT_LUNS`** | `0xA0` | 12B | Queries available LUNs (sent to Report LUNs W-LUN `0x81`). |
| **`SECURITY_PROTOCOL_IN / OUT`** | `0xA2 / 0xB5` | 12B | Transfers RPMB frames (`0xC4`). |
| **`READ_BUFFER / WRITE_BUFFER`** | `0x3C / 0x3B` | 10B | Firmware updates (FFU) & diagnostics. |

---

### UFS Protocol Information Units (UPIU)

Communication between the host and device across the UTP layer uses UPIUs
(UFS 3.1/4.0 section 10.6): a 12-byte basic header followed by 20 bytes of
transaction-specific fields (32 bytes fixed), optionally followed by Extended
Header Segments (EHS) and data payloads:

```
+---------------------------------------------------------------+
| Byte 0: Transaction Type (Opcode)                             |
| Byte 1: Flags (Read/Write/Attr: Simple, Ordered, Head of Q)   |
| Byte 2: Logical Unit Number (LUN / W-LUN)                     |
| Byte 3: Task Tag (Unique ID matching Request to Response)     |
| Byte 4: Initiator ID [7:4] / Command Set Type [3:0]           |
| Byte 5: Query / Task Management Function                      |
| Byte 6: Response Code                                         |
| Byte 7: SCSI / Transaction Status                             |
| Byte 8: Total EHS Length (in 32-bit words)                    |
| Byte 9: Device Information                                    |
| Bytes 10-11: Data Segment Length (Big Endian)                 |
| Bytes 12-15: Expected Data Transfer Length / TSFs (Big Endian)|
| Bytes 16-31: Transaction Specific Fields / SCSI CDB (16B)     |
+---------------------------------------------------------------+
```

#### Common UPIU Transaction Types (UFS 3.1/4.0 section 10.5.1)

| Type | Opcode | Direction | Description |
| :--- | :---: | :---: | :--- |
| **`NOP_OUT`** | `0x00` | Host -> Dev | Ping / link check to confirm device responsiveness. |
| **`NOP_IN`** | `0x20` | Dev -> Host | Response to `NOP_OUT`. |
| **`COMMAND`** | `0x01` | Host -> Dev | Encapsulates a SCSI CDB, target LUN, and transfer length. |
| **`RESPONSE`** | `0x21` | Dev -> Host | SCSI status (`GOOD`, `CHECK_CONDITION`) + sense data. |
| **`DATA_OUT`** | `0x02` | Host -> Dev | Host data payload sent after `READY_TO_TRANSFER`. |
| **`DATA_IN`** | `0x22` | Dev -> Host | Read data payload returned to the host. |
| **`READY_TO_TRANSFER`** | `0x31` | Dev -> Host | Signals device is ready for write DMA. |
| **`TASK_MANAGE_REQ`** | `0x04` | Host -> Dev | Abort task, query task, or reset LUN. |
| **`TASK_MANAGE_RESP`** | `0x24` | Dev -> Host | Task management status (`COMPLETE`, `REJECTED`). |
| **`QUERY_REQ`** | `0x16` | Host -> Dev | Read/Write Descriptors, Flags, and Attributes. |
| **`QUERY_RESP`** | `0x36` | Dev -> Host | Query response with descriptor, flag, or attribute. |
| **`REJECT`** | `0x3F` | Dev -> Host | Device rejected a UPIU with an invalid header. |

---

### Admin / Native Commands & Query Requests

Query Requests (UFS 3.1/4.0 section 10.7.8) manage UFS device configuration,
health, geometry, and power states:

#### 1. Descriptors (UFS 3.1/4.0 section 14.1, Read Opcode `0x01` / Write `0x02`)

- **Device Descriptor (`0x00`)**: Device class, number of LUNs, boot enable,
  extended UFS features support, WriteBooster parameters.
- **Configuration Descriptor (`0x01`)**: Provisions the physical device into
  LUNs.
- **Unit Descriptor (`0x02`)**: Per-LUN configuration: logical block count,
  block size (`1 << bLogicalBlockSize`), boot LUN ID, WriteBooster buffer size.
- **RPMB Unit Descriptor (`0x02`, index `0xC4`)**: RPMB LU size
  (`qLogicalBlockCount` x 256 B) and provisioning parameters.
- **Interconnect Descriptor (`0x04`)**: UniPro version and M-PHY version.
- **String Descriptor (`0x05`)**: UTF-16 string descriptors (manufacturer,
  product name, serial number, OEM ID).
- **Geometry Descriptor (`0x07`)**: Total raw device capacity, segment size,
  allocation unit size, max number of LUNs (`bMaxNumberLU`), RPMB read/write
  size (`bRPMB_ReadWriteSize`).
- **Power Parameters Descriptor (`0x08`)**: Active/Idle/Sleep current consumption
  per voltage rail (`VCC`, `VCCQ`, `VCCQ2`).
- **Device Health Descriptor (`0x09`)**: Pre-EOL status and device lifetime
  estimation (Type A & B).

#### 2. Flags (UFS 3.1/4.0 section 14.2, Read `0x05` / Set `0x06` / Clear `0x07` / Toggle `0x08`)

- **`fDeviceInit` (`0x01`)**: Set to `1` by the host during initialization to
  trigger internal device initialization; polled until cleared to `0` by the
  device.
- **`fPermanentWPEn` (`0x02`)**: Permanently enables write protection.
- **`fPowerOnWPEn` (`0x03`)**: Enables write protection until the next power
  cycle.
- **`fBackgroundOpsEn` (`0x04`)**: Permits device background maintenance when
  idle.
- **`fPurgeEnable` (`0x06`)**: Triggers background purge of unmapped blocks.
- **`fWriteBoosterEn` (`0x0E`)**: Enables SLC-mode WriteBooster caching.
- **`fWBBufferFlushEn` (`0x0F`)**: Triggers WriteBooster buffer flush to TLC
  flash during idle/hibernate or active states.
- **`fWBBufferFlushDuringHibernate` (`0x10`)**: Enables automatic WriteBooster
  flushing while the link is in Hibernate (`HIBERN8`).

#### 3. Attributes (UFS 3.1/4.0 section 14.3, Read `0x03` / Write `0x04`)

- **`bBootLunEn` (`0x00`)**: Selects which Boot LUN is mapped to the Boot W-LUN.
- **`bCurrentPowerMode` (`0x02`)**: Current device power mode (`Active`,
  `UFS-Sleep`, `UFS-PowerDown`).
- **`bActiveICCLevel` (`0x03`)**: Maximum active current consumption level.
- **`bRefClkFreq` (`0x0A`)**: Reference clock frequency (`19.2 MHz`, `26 MHz`,
  `38.4 MHz`, `52 MHz`).
- **`bMaxNumOfRTT` (`0x0C`)**: Maximum number of outstanding `READY_TO_TRANSFER`
  requests supported by the device.
- **`dSecondsPassed` (`0x0F`)**: Host time synchronization counter.
- **`bWBBufferFlushStatus` (`0x1C`)**: Status of an ongoing WriteBooster flush
  operation.
- **`bAvailableWBBufferSize` (`0x1D`)**: Remaining available WriteBooster SLC
  buffer capacity (in 10% increments).
- **`bWBBufferLifeTimeEst` (`0x1E`)**: SLC buffer lifetime wear estimate.
- **`dCurrentWBBufferSize` (`0x1F`)**: Configured WriteBooster buffer size in
  allocation units.

---

### Task Management Requests

Task management UPIUs (UFS 3.1/4.0 section 10.7.6) are submitted via the UTP
Task Management Request List (`UTMRD`, UFSHCI 3.0/4.0 section 5.5):

- **`ABORT_TASK` (`0x01`)**: Aborts an individual outstanding UTRD transfer
  matching a specific `Task Tag`.
- **`ABORT_TASK_SET` (`0x02`)**: Aborts all outstanding tasks for a target LUN.
- **`CLEAR_TASK_SET` (`0x04`)**: Clears the task set queue on the target LUN.
- **`LOGICAL_UNIT_RESET` (`0x08`)**: Resets a single LUN without resetting the
  entire UFS host or other LUNs.
- **`QUERY_TASK` (`0x80`)**: Checks whether a specific task tag is executing.
- **`QUERY_TASK_SET` (`0x81`)**: Checks if any tasks are present in the LUN
  task set.

---

### LUNs vs Well-Known LUNs (W-LUNs)

A UFS device is partitioned into independent logical storage units (UFS 3.1/4.0
section 10.8.5):

```
+-------------------------------------------------------------------------------+
|                             Physical UFS Target Device                        |
|                                                                               |
|  Standard Logical Units (LUN 0..31):                                          |
|  +---------------+  +---------------+  +---------------+  +---------------+   |
|  | LUN 0         |  | LUN 1         |  | LUN 2         |  | LUN 3..31     |   |
|  +---------------+  +---------------+  +---------------+  +---------------+   |
|                                                                               |
|  Well-Known Logical Units (W-LUNs, UPIU bit 7 = 1):                           |
|  +--------------------+  +--------------------+  +--------------------+       |
|  | Report LUNs (0x81) |  | RPMB W-LUN (0xC4)  |  | UFS Device (0xD0)  |       |
|  | LUN Enumeration    |  | Secure Key & Auth  |  | Power, Sleep, Reset|       |
|  +--------------------+  +--------------------+  +--------------------+       |
+-------------------------------------------------------------------------------+
```

1. **Standard Logical Units (`0x00` through `0x1F`, up to 32 LUNs)**:
   - Block devices exposed to the Fuchsia storage stack.
2. **Well-Known LUNs (W-LUNs, UPIU LUN field has bit 7 set = `0x80 | id`)**:
   - **`REPORT_LUNS` (`0x81`)**: Target for `REPORT_LUNS` SCSI command.
   - **`BOOT` (`0xB0`)**: Alias for the active boot LUN selected by
     `bBootLunEn`.
   - **`RPMB` (`0xC4`)**: Replay Protected Memory Block W-LUN accessed via
     `SECURITY_PROTOCOL_IN` / `SECURITY_PROTOCOL_OUT` using HMAC-SHA256 frames.
   - **`UFS_DEVICE` (`0xD0`)**: Device-level management W-LUN (used for
     `START_STOP_UNIT` device power mode transitions).

---

### UniPro Interconnect & UIC DME Commands

The UFS InterConnect (UIC) layer uses the MIPI UniPro stack. Management
commands are submitted through UFSHCI UIC Command registers (`UICCMD` at `0x90`,
`UICCMDARG1` at `0x94`, `UICCMDARG2` at `0x98`, `UICCMDARG3` at `0x9C`;
UFSHCI 3.0/4.0 section 5.6):

```
+---------------------+---------+--------------------------------------------------+
| Command Name        | Opcode  | Description & Arguments                          |
+---------------------+---------+--------------------------------------------------+
| DME_GET             | 0x01    | Read local UniPro MIB attribute                  |
|                     |         | Arg1: [31:16] MIB Attribute ID, [15:0] GenIndex  |
|                     |         | Arg2: [7:0] Result Code (0x00 = SUCCESS)         |
|                     |         | Arg3: Returns Read Value                         |
| DME_SET             | 0x02    | Write local UniPro MIB attribute                 |
|                     |         | Arg1: [31:16] MIB Attribute ID, [15:0] GenIndex  |
|                     |         | Arg2: [23:16] AttrSetType (0=Normal, 1=Static),  |
|                     |         |       [7:0] Result Code                          |
|                     |         | Arg3: Write Value                                |
| DME_PEER_GET        | 0x03    | Read remote (device-side) UniPro MIB attribute   |
| DME_PEER_SET        | 0x04    | Write remote (device-side) UniPro MIB attribute  |
| DME_POWERON         | 0x10    | Power on UniPro stack                            |
| DME_POWEROFF        | 0x11    | Power off UniPro stack                           |
| DME_ENABLE          | 0x12    | Enable UniPro link state machine                 |
| DME_RESET           | 0x14    | Reset local UniPro stack                         |
| DME_ENDPOINTRESET   | 0x15    | Send UniPro EndPointReset to remote device       |
| DME_LINKSTARTUP     | 0x16    | Initiate UniPro link training & synchronization  |
| DME_HIBERNATE_ENTER | 0x17    | Enter UniPro Hibernate (HIBERN8) link state      |
| DME_HIBERNATE_EXIT  | 0x18    | Exit UniPro Hibernate (HIBERN8) link state       |
| DME_TEST_MODE       | 0x1A    | Enter UniPro test mode                           |
+---------------------+---------+--------------------------------------------------+
```

#### Link Startup & Gear Negotiation Sequence (UFS 3.1/4.0 section 7.4)

1. Issue `DME_LINKSTARTUP` (`0x16`) and verify `HCS.DP = 1` (Device Present).
2. Read `PA_ConnectedTxDataLanes` (`0x1561`), `PA_ConnectedRxDataLanes`
   (`0x1581`), `PA_MaxRxHSGear` (`0x1587`), and peer `PA_MaxRxHSGear` via
   `DME_PEER_GET`.
3. Set `PA_ActiveTxDataLanes` (`0x1560`), `PA_ActiveRxDataLanes` (`0x1580`),
   `PA_TxGear` (`0x1568`), `PA_RxGear` (`0x1583`), `PA_TxTermination`
   (`0x1569`), `PA_RxTermination` (`0x1584`), and `PA_HSSeries` (`0x156A`,
   Rate A or Rate B for HS-G1..G4; Rate B required for HS-G5).
4. Write `PA_PWRMode` (`0x1571`) = `(Fast_Mode << 4) | Fast_Mode` (`0x11`) via
   `DME_SET` and wait for `IS.UPMS` (UIC Power Mode Status) with
   `HCS.UPMCRS = PWR_LOCAL`.

---

### UFSHCI DMA & Register Interface

UFSHCI (UFSHCI 3.0/4.0 sections 5 & 6, JESD223D/JESD223E) uses memory-mapped
descriptor rings for command submission and scatter-gather tables for
zero-copy DMA:

```
Host Controller Registers (MMIO)
+-------------------------------------------------------+
| 0x00: CAP    - Capabilities (Max Slots, 64-bit, MCQ)  |
| 0x20: IS     - Interrupt Status (UTP, UIC, Fatal Err) |
| 0x34: HCE    - Host Controller Enable                 |
| 0x50: UTRLBA - UTP Transfer Request List Base Address |
| 0x58: UTRLDBR- UTRD Door Bell (1 bit per slot)        |
+-------------------------------------------------------+
       |
       | Points to physical memory
       v
UTP Transfer Request List (UTRD Table - 32 slots x 32 bytes)
+---------------------------------------------------------------+
| Slot 0: [ Command Type | Data Dir | OCS | UCD Base | PRDT ]   |
| Slot 1: [ ...                                               ] |
+---------------------------------------------------------------+
       |
       | UCD (UTP Command Descriptor)
       v
+---------------------------------------------------------------+
| Command UPIU (32 B: 12 B header + EDTL + 16 B CDB)            |
| Response UPIU (at request length; header + sense data)        |
| Physical Region Description Table (after the response UPIU)   |
|   - Entry 0: [ Base Addr 64-bit | Data Byte Count ]           |
|   - Entry 1: [ Base Addr 64-bit | Data Byte Count ]           |
+---------------------------------------------------------------+
       |
       | Zero-Copy Direct DMA
       v
Host Physical RAM (Pinned VMO Pages)
```

---

## UFS 4.0 vs UFS 3.1 & MCQ Architecture

### Architectural & Hardware Comparison Table

UFS 4.0 is a significant evolution over 3.1, doubling interface bandwidth and
drastically improving lock contention on multi-core systems:

| Feature / Metric | UFS 3.1 (UFSHCI 3.0) | UFS 4.0 (UFSHCI 4.0) |
| :--- | :--- | :--- |
| **Physical Layer (M-PHY)** | M-PHY v4.1 (HS-G4) | **M-PHY v5.0 (HS-G5)** |
| **Max Rate (per lane)** | 11.6 Gbps (~1.16 GB/s 8b/10b) | **23.2 Gbps (~2.32 GB/s 8b/10b)** |
| **Max Bandwidth (2 lanes)** | ~2.32 GB/s/dir (4.64 GB/s duplex) | **~4.64 GB/s/dir** |
| **Interconnect (UniPro)** | MIPI UniPro v1.8 | **MIPI UniPro v2.0** |
| **Host Queue Model** | Single 32-slot UTRD list | **MCQ (up to 256 SQs/CQs, N:1 SQ->CQ)** |
| **Doorbell Model** | Single 32-bit `UTRLDBR` | **Per-queue tail (`SQxTP`)** |
| **Interrupt Model** | Global `IS` register | **Per-CQ `CQxIS` / MSI-X** |

---

### Multi-Circular Queue (MCQ) Overview

In UFSHCI 3.0 (legacy single doorbell mode), all CPUs share a single 32-slot
request list (`UTRLBA`) and a single 32-bit doorbell register (`UTRLDBR` at
offset `0x58`).

UFSHCI 4.0 introduces **Multi-Circular Queue (MCQ, UFSHCI 4.0 section 5.8 &
6.4)** with independent Submission Queues (SQ) and Completion Queues (CQ):

```
+-------------------------------------------------------------------------------+
|                         UFSHCI 4.0 MCQ Architecture                           |
|                                                                               |
|  Submission Q 0 (SQ0)       Submission Q 1 (SQ1)       Submission Q N (SQN)   |
|  - 32-byte UTRD SQEs        - 32-byte UTRD SQEs        - 32-byte UTRD SQEs    |
|  - Tail Pointer (SQ0TP)     - Tail Pointer (SQ1TP)     - Tail Pointer (SQNTP) |
|            |                         |                         |              |
|            +-------------------------+-------------------------+              |
|                                      |                                        |
|                                      v                                        |
|                      +-------------------------------+                        |
|                      |   UFS 4.0 Controller DMA      |                        |
|                      +---------------+---------------+                        |
|                                      |                                        |
|            +-------------------------+-------------------------+              |
|            v                         v                         v              |
|  Completion Q 0 (CQ0)       Completion Q 1 (CQ1)       Completion Q N (CQN)   |
|  - 32-byte CQEs             - 32-byte CQEs             - 32-byte CQEs         |
|  - Head Pointer (CQ0HP)     - Head Pointer (CQ1HP)     - Head Pointer (CQNHP) |
+-------------------------------------------------------------------------------+
```

1. **Submission Queues (SQ)**: Circular rings of 32-byte Submission Queue
   Entries (each SQE is a UTRD pointing to a UCD). Each SQ has an independent
   Tail Pointer register (`SQxTP`) written by the host to submit work.
2. **Completion Queues (CQ)**: Circular rings of 32-byte Completion Queue
   Entries (CQEs) written by the controller upon transfer completion. Each CQ
   has a Head Pointer register (`CQxHP`) and Interrupt Status register
   (`CQxIS`) updated by the host after consuming completions.
3. **Queue count**: `MCQCAP` (offset `0x04`) `MAXQ` is an 8-bit, 0-based field,
   so a controller supports up to 256 queues. Several SQs may map onto one CQ.
4. **Registers**: `CAP.MCQS` (30), `CAP.LSDBS` (29, no legacy doorbell),
   `CAP[7:0]` (0-based MCQ slots); `MCQCAP.QCFGPTR` (23:16, units of 0x200);
   `CONFIG` 0x300 (`QT`, bit 0: 1 selects MCQ); `MCQCONFIG` 0x380 (`MAC` 16:8,
   0-based).
   `IS`/`IE` bit 20 is `CQES`/`CQEE`, bit 21 `IAGES`/`IAGEE`.
5. **Queue configuration** (`QCFGPTR * 0x200 + 0x40 * i`): `SQATTR` 0x00
   (`SQEN` 31, `CQID` 23:16, `SIZE` 15:0 in dwords minus 1 = `entries * 8 - 1`), `SQLBA`/`SQUBA`
   0x04/0x08 (1 KiB aligned), `SQDAO` 0x0C, `SQISAO` 0x10, `CQATTR` 0x20
   (`CQEN` 31, `SIZE` 15:0 in dwords minus 1 = `entries * 8 - 1`),
   `CQLBA`/`CQUBA` 0x24/0x28, `CQDAO` 0x2C, `CQISAO` 0x30, `CQCFG` 0x34
   (`IAGVLD` 8, `IAG` 4:0). Enable the CQ before its SQ.
6. **Runtime blocks** at the `xQDAO`/`xQISAO` offsets: SQ `SQHP` 0x00, `SQTP`
   0x04, `SQRTC` 0x08 (`STOP` 0, `ICU` 1), `SQCTI` 0x0C
   (`IID << 16 | LUN << 8 | tag`), `SQRTS` 0x10 (`SQS` 0, `CUS` 1, `RTC` 7:4);
   CQ `CQHP` 0x00, `CQTP` 0x04; CQ interrupt `CQIS` 0x00 (`TEPS` 0, W1C), `CQIE`
   0x04, `CQIACR` 0x08. Pointers are byte offsets into the ring. Some
   controllers preset the offsets (QEMU); other controllers read 0 at reset and
   the host programs them (default layout: SQD 0x4000 + 0x14 * i, SQIS 0x4200 +
   0x8 * i, CQD 0x4400 + 0x8 * i, CQIS 0x4600 + 0xC * i).
7. **CQE** (32 bytes): DW0 `UCDBA` 31:7 and SQ ID 4:0, DW1 `UCDBAU`, DW2-3
   copies of the UTRD response/PRDT fields, DW4 `OCS` 7:0, DW5 task tag 7:0 and
   LUN 15:8 (UFSHCI 4.1; reserved in 4.0).
8. **Abort** (UFSHCI 4.0 SQ cleanup): stop the SQ (`SQRTC.STOP`, wait
   `SQRTS.SQS`), write `SQCTI`, set `SQRTC.ICU`, wait `SQRTS.CUS`, restart.
   There is no `UTRLCLR` in MCQ mode.
