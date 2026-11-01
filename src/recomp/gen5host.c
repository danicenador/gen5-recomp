/*
 * Bridge between the recompiled game DLL and the host runtime.
 *
 * The host passes a table of callbacks with gen5_set_host(); every rt_*
 * function forwards to it. The host reads the function table with
 * gen5_function_table() to dispatch indirect calls.
 */
#include "gen5rt.h"

typedef struct Gen5Host {
    uint32_t (*read8)(Cpu *cpu, uint32_t address);
    uint32_t (*read16)(Cpu *cpu, uint32_t address);
    uint32_t (*read32)(Cpu *cpu, uint32_t address);
    void (*write8)(Cpu *cpu, uint32_t address, uint32_t value);
    void (*write16)(Cpu *cpu, uint32_t address, uint32_t value);
    void (*write32)(Cpu *cpu, uint32_t address, uint32_t value);
    void (*call)(Cpu *cpu, uint32_t target);
    void (*jump)(Cpu *cpu, uint32_t target, uint32_t from);
    void (*swi)(Cpu *cpu, uint32_t comment, uint32_t thumb);
    void (*mcr)(Cpu *cpu, uint32_t cp, uint32_t opc1, uint32_t crn, uint32_t crm, uint32_t opc2,
                uint32_t value);
    uint32_t (*mrc)(Cpu *cpu, uint32_t cp, uint32_t opc1, uint32_t crn, uint32_t crm,
                    uint32_t opc2);
    uint32_t (*read_psr)(Cpu *cpu, uint32_t spsr);
    void (*write_psr)(Cpu *cpu, uint32_t spsr, uint32_t fields, uint32_t value);
    void (*exception_return)(Cpu *cpu, uint32_t target);
    void (*undefined)(Cpu *cpu, uint32_t address, uint32_t raw);
    void (*hle)(Cpu *cpu, uint32_t id);
    void (*tick)(Cpu *cpu);
} Gen5Host;

static Gen5Host host;

extern const Gen5FunctionEntry gen5_functions[];
extern const uint32_t gen5_function_count;

__declspec(dllexport) void gen5_set_host(const Gen5Host *callbacks) { host = *callbacks; }

__declspec(dllexport) const Gen5FunctionEntry *gen5_function_table(uint32_t *count) {
    *count = gen5_function_count;
    return gen5_functions;
}

uint32_t rt_read8_slow(Cpu *cpu, uint32_t a) { return host.read8(cpu, a); }
uint32_t rt_read16_slow(Cpu *cpu, uint32_t a) { return host.read16(cpu, a); }
uint32_t rt_read32_slow(Cpu *cpu, uint32_t a) { return host.read32(cpu, a); }
void rt_write8_slow(Cpu *cpu, uint32_t a, uint32_t v) { host.write8(cpu, a, v); }
void rt_write16_slow(Cpu *cpu, uint32_t a, uint32_t v) { host.write16(cpu, a, v); }
void rt_write32_slow(Cpu *cpu, uint32_t a, uint32_t v) { host.write32(cpu, a, v); }
void rt_call(Cpu *cpu, uint32_t target) { host.call(cpu, target); }
void rt_jump(Cpu *cpu, uint32_t target, uint32_t from) { host.jump(cpu, target, from); }
void rt_swi(Cpu *cpu, uint32_t comment, uint32_t thumb) { host.swi(cpu, comment, thumb); }
void rt_mcr(Cpu *cpu, uint32_t cp, uint32_t opc1, uint32_t crn, uint32_t crm, uint32_t opc2,
            uint32_t value) {
    host.mcr(cpu, cp, opc1, crn, crm, opc2, value);
}
uint32_t rt_mrc(Cpu *cpu, uint32_t cp, uint32_t opc1, uint32_t crn, uint32_t crm, uint32_t opc2) {
    return host.mrc(cpu, cp, opc1, crn, crm, opc2);
}
uint32_t rt_read_psr(Cpu *cpu, uint32_t spsr) { return host.read_psr(cpu, spsr); }
void rt_write_psr(Cpu *cpu, uint32_t spsr, uint32_t fields, uint32_t value) {
    host.write_psr(cpu, spsr, fields, value);
}
void rt_exception_return(Cpu *cpu, uint32_t target) { host.exception_return(cpu, target); }
void rt_undefined(Cpu *cpu, uint32_t address, uint32_t raw) { host.undefined(cpu, address, raw); }
void rt_hle(Cpu *cpu, uint32_t id) { host.hle(cpu, id); }
void rt_tick(Cpu *cpu) { host.tick(cpu); }
