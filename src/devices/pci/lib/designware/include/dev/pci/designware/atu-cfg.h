// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_PCI_LIB_DESIGNWARE_INCLUDE_DEV_PCI_DESIGNWARE_ATU_CFG_H_
#define SRC_DEVICES_PCI_LIB_DESIGNWARE_INCLUDE_DEV_PCI_DESIGNWARE_ATU_CFG_H_

typedef struct iatu_translation_entry {
  // Address on the CPU's memory bus.
  uint64_t cpu_addr;

  // Address on the PCI bus.
  uint64_t pci_addr;

  // Size of the translation aperture.
  size_t length;
} iatu_translation_entry_t;

#endif  // SRC_DEVICES_PCI_LIB_DESIGNWARE_INCLUDE_DEV_PCI_DESIGNWARE_ATU_CFG_H_
