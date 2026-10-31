/*
 * ABI between statically recompiled ARM9 code and the gen5 runtime.
 *
 * Every recompiled function has the signature `void f(Cpu *cpu)`. A call
 * sets lr and calls the callee's C function; a return is a C return.
 * Anything not known statically (indirect jumps and calls, calls whose
 * overlay is only known at run time, BIOS calls, coprocessor and PSR
 * access) goes through the rt_* functions that the runtime implements.
 */
#ifndef GEN5RT_H
#define GEN5RT_H

#include <stdint.h>
#include <string.h>

typedef struct Cpu {
    uint32_t r[16];
    /* Condition flags, each 0 or 1. */
    uint32_t n, z, c, v, q;
    /* Pages of host memory for 4 KiB guest pages, or NULL when the access
     * must go through the runtime (I/O registers, unmapped memory). */
    uint8_t **read_pages;
    uint8_t **write_pages;
    void *runtime;
} Cpu;

#define GEN5_PAGE_SHIFT 12
#define GEN5_PAGE_MASK 0xFFFu

/* Implemented by the runtime. */
uint32_t rt_read8_slow(Cpu *cpu, uint32_t address);
uint32_t rt_read16_slow(Cpu *cpu, uint32_t address);
uint32_t rt_read32_slow(Cpu *cpu, uint32_t address);
void rt_write8_slow(Cpu *cpu, uint32_t address, uint32_t value);
void rt_write16_slow(Cpu *cpu, uint32_t address, uint32_t value);
void rt_write32_slow(Cpu *cpu, uint32_t address, uint32_t value);
/* Call the function at `target` (bit 0 selects Thumb), resolving it in the
 * currently loaded overlays. lr is already set. */
void rt_call(Cpu *cpu, uint32_t target);
/* Continue at `target` as a tail call; the caller returns afterwards. */
void rt_jump(Cpu *cpu, uint32_t target, uint32_t from);
void rt_swi(Cpu *cpu, uint32_t comment, uint32_t thumb);
void rt_mcr(Cpu *cpu, uint32_t cp, uint32_t opc1, uint32_t crn, uint32_t crm, uint32_t opc2,
            uint32_t value);
uint32_t rt_mrc(Cpu *cpu, uint32_t cp, uint32_t opc1, uint32_t crn, uint32_t crm, uint32_t opc2);
uint32_t rt_read_psr(Cpu *cpu, uint32_t spsr);
void rt_write_psr(Cpu *cpu, uint32_t spsr, uint32_t fields, uint32_t value);
/* `movs pc, lr` / `subs pc, lr, #4` / `ldm ... {..., pc}^`. */
void rt_exception_return(Cpu *cpu, uint32_t target);
void rt_undefined(Cpu *cpu, uint32_t address, uint32_t raw);

static inline uint32_t read8(Cpu *cpu, uint32_t a) {
    uint8_t *page = cpu->read_pages[a >> GEN5_PAGE_SHIFT];
    return page ? page[a & GEN5_PAGE_MASK] : rt_read8_slow(cpu, a);
}

static inline uint32_t read16(Cpu *cpu, uint32_t a) {
    a &= ~1u;
    uint8_t *page = cpu->read_pages[a >> GEN5_PAGE_SHIFT];
    if (!page) return rt_read16_slow(cpu, a);
    uint16_t value;
    memcpy(&value, page + (a & GEN5_PAGE_MASK), 2);
    return value;
}

static inline uint32_t read32(Cpu *cpu, uint32_t a) {
    a &= ~3u;
    uint8_t *page = cpu->read_pages[a >> GEN5_PAGE_SHIFT];
    if (!page) return rt_read32_slow(cpu, a);
    uint32_t value;
    memcpy(&value, page + (a & GEN5_PAGE_MASK), 4);
    return value;
}

static inline void write8(Cpu *cpu, uint32_t a, uint32_t v) {
    uint8_t *page = cpu->write_pages[a >> GEN5_PAGE_SHIFT];
    if (page) page[a & GEN5_PAGE_MASK] = (uint8_t)v;
    else rt_write8_slow(cpu, a, v);
}

static inline void write16(Cpu *cpu, uint32_t a, uint32_t v) {
    a &= ~1u;
    uint8_t *page = cpu->write_pages[a >> GEN5_PAGE_SHIFT];
    if (!page) { rt_write16_slow(cpu, a, v); return; }
    uint16_t value = (uint16_t)v;
    memcpy(page + (a & GEN5_PAGE_MASK), &value, 2);
}

static inline void write32(Cpu *cpu, uint32_t a, uint32_t v) {
    a &= ~3u;
    uint8_t *page = cpu->write_pages[a >> GEN5_PAGE_SHIFT];
    if (!page) { rt_write32_slow(cpu, a, v); return; }
    memcpy(page + (a & GEN5_PAGE_MASK), &v, 4);
}

/* LDR from an unaligned address rotates the aligned word (ARMv5). */
static inline uint32_t read32_rotated(Cpu *cpu, uint32_t a) {
    uint32_t value = read32(cpu, a);
    uint32_t rotate = (a & 3u) * 8u;
    return rotate ? (value >> rotate) | (value << (32u - rotate)) : value;
}

static inline uint32_t ror32(uint32_t v, uint32_t n) {
    n &= 31u;
    return n ? (v >> n) | (v << (32u - n)) : v;
}

/* Register-specified shifts: amount is the bottom byte of a register.
 * `*c` is the shifter carry out, left unchanged for an amount of zero. */
static inline uint32_t shift_lsl(uint32_t v, uint32_t n, uint32_t *c) {
    if (n == 0) return v;
    if (n < 32) { *c = (v >> (32u - n)) & 1u; return v << n; }
    *c = n == 32 ? v & 1u : 0;
    return 0;
}

static inline uint32_t shift_lsr(uint32_t v, uint32_t n, uint32_t *c) {
    if (n == 0) return v;
    if (n < 32) { *c = (v >> (n - 1u)) & 1u; return v >> n; }
    *c = n == 32 ? v >> 31 : 0;
    return 0;
}

static inline uint32_t shift_asr(uint32_t v, uint32_t n, uint32_t *c) {
    if (n == 0) return v;
    if (n < 32) { *c = (v >> (n - 1u)) & 1u; return (uint32_t)((int32_t)v >> n); }
    *c = v >> 31;
    return (uint32_t)((int32_t)v >> 31);
}

static inline uint32_t shift_ror(uint32_t v, uint32_t n, uint32_t *c) {
    if (n == 0) return v;
    n &= 31u;
    if (n == 0) { *c = v >> 31; return v; }
    *c = (v >> (n - 1u)) & 1u;
    return ror32(v, n);
}

/* a + b + carry_in, setting C and V; the caller sets N and Z. */
static inline uint32_t add_flags(Cpu *cpu, uint32_t a, uint32_t b, uint32_t carry_in) {
    uint64_t wide = (uint64_t)a + b + carry_in;
    uint32_t result = (uint32_t)wide;
    cpu->c = (uint32_t)(wide >> 32);
    cpu->v = ((~(a ^ b)) & (a ^ result)) >> 31;
    return result;
}

static inline void set_nz(Cpu *cpu, uint32_t result) {
    cpu->n = result >> 31;
    cpu->z = result == 0;
}

static inline uint32_t clz32(uint32_t v) {
    return v ? (uint32_t)__builtin_clz(v) : 32u;
}

static inline int32_t saturate64(Cpu *cpu, int64_t v) {
    if (v > INT32_MAX) { cpu->q = 1; return INT32_MAX; }
    if (v < INT32_MIN) { cpu->q = 1; return INT32_MIN; }
    return (int32_t)v;
}

static inline int32_t half_of(uint32_t v, uint32_t top) {
    return top ? (int32_t)(int16_t)(v >> 16) : (int32_t)(int16_t)v;
}

/* Function table for indirect calls: module is 0xFFFF for main, 0xFF00 + n
 * for autoload block n, and the overlay ID otherwise. */
typedef void (*Gen5Function)(Cpu *cpu);
typedef struct Gen5FunctionEntry {
    uint16_t module;
    uint16_t thumb;
    uint32_t address;
    Gen5Function function;
} Gen5FunctionEntry;

#endif
