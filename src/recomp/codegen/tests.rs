//! Execution tests: compile generated C with gcc and check ARM semantics.
//! Skipped (with a message) when gcc is not installed.

use super::*;
use crate::recomp::analysis::analyze_function;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::process::Command;

const BASE: u32 = 0x0200_0000;

/// Harness: 64 KiB of RAM at 0x02000000; anything else is an error.
const HARNESS: &str = r#"
#include <stdio.h>
#include <stdlib.h>
static uint8_t ram[0x10000];
static void fail(const char *what, uint32_t a) { printf("error %s %08X\n", what, a); exit(1); }
uint32_t rt_read8_slow(Cpu *c, uint32_t a) { (void)c; fail("read8", a); return 0; }
uint32_t rt_read16_slow(Cpu *c, uint32_t a) { (void)c; fail("read16", a); return 0; }
uint32_t rt_read32_slow(Cpu *c, uint32_t a) { (void)c; fail("read32", a); return 0; }
void rt_write8_slow(Cpu *c, uint32_t a, uint32_t v) { (void)c; (void)v; fail("write8", a); }
void rt_write16_slow(Cpu *c, uint32_t a, uint32_t v) { (void)c; (void)v; fail("write16", a); }
void rt_write32_slow(Cpu *c, uint32_t a, uint32_t v) { (void)c; (void)v; fail("write32", a); }
void rt_call(Cpu *c, uint32_t t) { (void)c; printf("call %u\n", t); }
void rt_jump(Cpu *c, uint32_t t, uint32_t f) { (void)c; (void)f; printf("jump %u\n", t); }
void rt_swi(Cpu *c, uint32_t n, uint32_t t) { (void)c; (void)t; printf("swi %u\n", n); }
void rt_mcr(Cpu *c, uint32_t a, uint32_t b, uint32_t d, uint32_t e, uint32_t f, uint32_t v) {
    (void)c; (void)a; (void)b; (void)d; (void)e; (void)f; (void)v; }
uint32_t rt_mrc(Cpu *c, uint32_t a, uint32_t b, uint32_t d, uint32_t e, uint32_t f) {
    (void)c; (void)a; (void)b; (void)d; (void)e; (void)f; return 0; }
uint32_t rt_read_psr(Cpu *c, uint32_t s) { (void)c; (void)s; return 0; }
void rt_write_psr(Cpu *c, uint32_t s, uint32_t f, uint32_t v) { (void)c; (void)s; (void)f; (void)v; }
void rt_exception_return(Cpu *c, uint32_t t) { (void)c; printf("eret %u\n", t); }
void rt_undefined(Cpu *c, uint32_t a, uint32_t r) { (void)c; (void)r; printf("undefined %u\n", a); }
void rt_tick(Cpu *c) { c->budget = 1000; }
void rt_stack_mismatch(Cpu *c, uint32_t f, uint32_t sp) { (void)c; printf("stack mismatch %u %u\n", f, sp); }
"#;

/// Compile the function at BASE with gcc, run it after the `setup` C
/// statements, and return the final state by name.
fn run(name: &str, code: &[u8], thumb: bool, setup: &str) -> Option<HashMap<String, u32>> {
    if Command::new("gcc").arg("--version").output().is_err() {
        eprintln!("gcc not found; skipping codegen execution test");
        return None;
    }
    let program = Program {
        entry: BASE,
        modules: vec![Module {
            id: ModuleId::Main,
            base: BASE,
            bytes: code.to_vec(),
            bss_size: 0,
            sinit: None,
        }],
    };
    let key = FunctionKey {
        module: ModuleId::Main,
        address: BASE,
    };
    let function = analyze_function(&program, key, thumb, &HashSet::new());
    let analysis = Analysis {
        functions: BTreeMap::from([(key, function)]),
        ..Analysis::default()
    };
    let source = emit_function(&analysis, &program.modules[0], &analysis.functions[&key]);
    let bytes: Vec<String> = code.iter().map(u8::to_string).collect();
    let main = format!(
        "#include \"gen5rt.h\"\n{HARNESS}\n{source}\n\
         int main(void) {{\n\
         static uint8_t *pages[1 << 20];\n\
         for (int i = 0; i < 16; i++) pages[(0x02000000u >> 12) + i] = ram + i * 4096;\n\
         static const uint8_t code[] = {{{bytes}}};\n\
         memcpy(ram, code, sizeof code);\n\
         Cpu cpu = {{0}};\n\
         cpu.budget = 1000;\n\
         cpu.read_pages = pages;\n\
         cpu.write_pages = pages;\n\
         uint32_t *r = cpu.r;\n\
         (void)r;\n\
         {setup}\n\
         {name_fn}(&cpu);\n\
         for (int i = 0; i < 16; i++) printf(\"r%d %u\\n\", i, cpu.r[i]);\n\
         printf(\"n %u\\nz %u\\nc %u\\nv %u\\nq %u\\n\", cpu.n, cpu.z, cpu.c, cpu.v, cpu.q);\n\
         for (int i = 0; i < 4; i++) {{\n\
         uint32_t w;\n\
         memcpy(&w, ram + 0x700 + 4 * i, 4);\n\
         printf(\"m%d %u\\n\", i, w);\n\
         }}\n\
         return 0;\n\
         }}\n",
        bytes = bytes.join(","),
        name_fn = function_name(key)
    );
    let dir = std::env::temp_dir().join("gen5-codegen-tests");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("gen5rt.h"), RUNTIME_HEADER).unwrap();
    let c_path = dir.join(format!("{name}.c"));
    let exe_path = dir.join(format!("{name}.exe"));
    fs::write(&c_path, main).unwrap();
    let compile = Command::new("gcc")
        .args(["-O1", "-Wall", "-Werror", "-o"])
        .arg(&exe_path)
        .arg(&c_path)
        .output()
        .unwrap();
    assert!(
        compile.status.success(),
        "gcc failed for {name}:\n{}\n{source}",
        String::from_utf8_lossy(&compile.stderr)
    );
    let output = Command::new(&exe_path).output().unwrap();
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(!text.contains("error"), "{name}: {text}");
    let state = text
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(' ')?;
            Some((key.to_owned(), value.parse().ok()?))
        })
        .collect();
    Some(state)
}

fn arm(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|word| word.to_le_bytes()).collect()
}

fn thumb(halves: &[u16]) -> Vec<u8> {
    halves.iter().flat_map(|half| half.to_le_bytes()).collect()
}

const BX_LR: u32 = 0xE12F_FF1E;

#[test]
fn arithmetic_flags() {
    // adds r2, r0, r1 with signed overflow
    let setup = "r[0] = 0x7FFFFFFF; r[1] = 1;";
    let Some(s) = run("adds", &arm(&[0xE090_2001, BX_LR]), false, setup) else {
        return;
    };
    assert_eq!(
        (s["r2"], s["n"], s["z"], s["c"], s["v"]),
        (0x8000_0000, 1, 0, 0, 1)
    );

    // subs r2, r0, r1 with a borrow
    let s = run(
        "subs",
        &arm(&[0xE050_2001, BX_LR]),
        false,
        "r[0] = 1; r[1] = 2;",
    )
    .unwrap();
    assert_eq!((s["r2"], s["n"], s["c"], s["v"]), (0xFFFF_FFFF, 1, 0, 0));

    // sbcs r2, r0, r1 with C clear: 5 - 3 - 1
    let setup = "r[0] = 5; r[1] = 3; cpu.c = 0;";
    let s = run("sbcs", &arm(&[0xE0D0_2001, BX_LR]), false, setup).unwrap();
    assert_eq!((s["r2"], s["c"]), (1, 1));

    // cmp r0, r1 (equal)
    let s = run(
        "cmp",
        &arm(&[0xE150_0001, BX_LR]),
        false,
        "r[0] = 9; r[1] = 9;",
    )
    .unwrap();
    assert_eq!((s["z"], s["c"], s["n"]), (1, 1, 0));
}

#[test]
fn shifter_carry() {
    // movs r0, r1, lsr #32
    let setup = "r[1] = 0x80000000;";
    let Some(s) = run("lsr32", &arm(&[0xE1B0_0021, BX_LR]), false, setup) else {
        return;
    };
    assert_eq!((s["r0"], s["c"], s["z"]), (0, 1, 1));

    // Thumb lsls r0, r1 (by register, amount 32)
    let s = run(
        "lslreg",
        &thumb(&[0x4088, 0x4770]),
        true,
        "r[0] = 1; r[1] = 32;",
    )
    .unwrap();
    assert_eq!((s["r0"], s["c"], s["z"]), (0, 1, 1));

    // ands r0, r0, #0xFF000000: a rotated immediate sets C from bit 31
    let setup = "r[0] = 0xF0000000; cpu.c = 0;";
    let s = run("andimm", &arm(&[0xE210_04FF, BX_LR]), false, setup).unwrap();
    assert_eq!((s["r0"], s["c"], s["n"]), (0xF000_0000, 1, 1));
}

#[test]
fn memory_semantics() {
    // ldr r0, [r1] from an unaligned address rotates the word.
    let setup = "uint32_t w = 0x44332211; memcpy(ram + 0x700, &w, 4); r[1] = 0x02000701;";
    let Some(s) = run("ldrrot", &arm(&[0xE591_0000, BX_LR]), false, setup) else {
        return;
    };
    assert_eq!(s["r0"], 0x1144_3322);

    // ldrsb r0, [r1]
    let setup = "ram[0x700] = 0x80; r[1] = 0x02000700;";
    let s = run("ldrsb", &arm(&[0xE1D1_00D0, BX_LR]), false, setup).unwrap();
    assert_eq!(s["r0"], 0xFFFF_FF80);

    // str r0, [r1, #4]! then strh r2, [r1, #2]
    let setup = "r[0] = 0xAABBCCDD; r[1] = 0x02000700; r[2] = 0x1234;";
    let s = run(
        "strwb",
        &arm(&[0xE5A1_0004, 0xE1C1_20B2, BX_LR]),
        false,
        setup,
    )
    .unwrap();
    assert_eq!((s["r1"], s["m1"]), (0x0200_0704, 0x1234_CCDD));
}

#[test]
fn stack_and_multiply() {
    // push {r4, lr} ; movs r4, #7 ; pop {r4, pc}
    let setup = "r[4] = 3; r[13] = 0x02000780; r[14] = 0x1235;";
    let Some(s) = run("pushpop", &thumb(&[0xB510, 0x2407, 0xBD10]), true, setup) else {
        return;
    };
    assert_eq!((s["r4"], s["r13"]), (3, 0x0200_0780));

    // smull r0, r1, r2, r3 with -2 * 3
    let setup = "r[2] = (uint32_t)-2; r[3] = 3;";
    let s = run("smull", &arm(&[0xE0C1_0392, BX_LR]), false, setup).unwrap();
    assert_eq!((s["r0"], s["r1"]), (0xFFFF_FFFA, 0xFFFF_FFFF));
}

#[test]
fn thumb_switch_table() {
    let code = thumb(&[
        0x2802, // 0x00: cmp r0, #2
        0xD80E, // 0x02: bhi 0x22
        0x0040, // 0x04: lsls r0, r0, #1
        0x4478, // 0x06: add r0, pc
        0x88C0, // 0x08: ldrh r0, [r0, #6]
        0x0400, // 0x0A: lsls r0, r0, #16
        0x1400, // 0x0C: asrs r0, r0, #16
        0x4487, // 0x0E: add pc, r0
        0x0004, 0x0008, 0x000C, // 0x10: offsets from 0x12
        0x210A, 0x4770, // 0x16: movs r1, #10 ; bx lr
        0x210B, 0x4770, // 0x1A: movs r1, #11 ; bx lr
        0x210C, 0x4770, // 0x1E: movs r1, #12 ; bx lr
        0x2163, 0x4770, // 0x22: movs r1, #99 ; bx lr
    ]);
    let Some(s) = run("switch1", &code, true, "r[0] = 1;") else {
        return;
    };
    assert_eq!(s["r1"], 11);
    let s = run("switch5", &code, true, "r[0] = 5;").unwrap();
    assert_eq!(s["r1"], 99);
}

#[test]
fn unknown_callee_goes_through_runtime() {
    // bl 0x02000010 ; bx lr ; ... ; 0x10: bx lr
    let code = arm(&[0xEB00_0002, BX_LR, 0, 0, BX_LR]);
    let Some(s) = run("call", &code, false, "r[14] = 0x100;") else {
        return;
    };
    assert_eq!(s["call"], 0x0200_0010);
    assert_eq!(s["r14"], 0x0200_0004);
}

#[test]
fn callee_with_decode_problems_goes_through_runtime() {
    // bl 0x02000010 ; bx lr ; ... ; 0x10: bx lr
    let code = arm(&[0xEB00_0002, BX_LR, 0, 0, BX_LR]);
    let program = Program {
        entry: BASE,
        modules: vec![Module {
            id: ModuleId::Main,
            base: BASE,
            bytes: code,
            bss_size: 0,
            sinit: None,
        }],
    };
    let caller = FunctionKey {
        module: ModuleId::Main,
        address: BASE,
    };
    let callee = FunctionKey {
        module: ModuleId::Main,
        address: BASE + 0x10,
    };
    let emit = |problem: bool| {
        let mut target = analyze_function(&program, callee, false, &HashSet::new());
        if problem {
            target.problems.push("undefined instruction".to_owned());
        }
        let analysis = Analysis {
            functions: BTreeMap::from([
                (caller, analyze_function(&program, caller, false, &HashSet::new())),
                (callee, target),
            ]),
            ..Analysis::default()
        };
        emit_function(&analysis, &program.modules[0], &analysis.functions[&caller])
    };
    let direct = format!("{}(cpu);", function_name(callee));
    assert!(emit(false).contains(&direct));
    let source = emit(true);
    assert!(!source.contains(&direct), "{source}");
    assert!(source.contains("rt_call(cpu, 0x02000010u)"), "{source}");
}
