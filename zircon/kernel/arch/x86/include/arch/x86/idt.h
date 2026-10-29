// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_IDT_H_
#define ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_IDT_H_

#include <stdint.h>
#include <zircon/compiler.h>

struct idt;

extern struct idt _idt_startup;  // NOLINT(bugprone-reserved-identifier)

__BEGIN_CDECLS

void load_startup_idt(void);

/*
 * @brief Set the Interrupt Stack Table index to use
 *
 * @param idt Pointer to the IDT to change.
 * @param vec The vector to change.
 * @param ist_idx A value in the range [0, 8) indicating which stack to use.
 *        If ist_idx == 0, use the normal stack for the target privilege level.
 */
void idt_set_ist_index(struct idt *idt, uint8_t vec, uint8_t ist_idx);

/*
 * @brief Initialize this IDT with our default values
 *
 * @param idt Pointer to the IDT to initialize
 */
void idt_setup(struct idt *idt);

/*
 * @brief Setup the read-only remapping of the IDT.
 */
void idt_setup_readonly(void);

/*
 * @brief Switch to the given IDT
 *
 * @param idt Pointer to the IDT
 */
void idt_load(const struct idt *idt);

/*
 * @brief Get the read-only IDT.
 */
struct idt *idt_get_readonly(void);

__END_CDECLS

#endif  // ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_IDT_H_
