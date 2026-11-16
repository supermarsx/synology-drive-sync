from __future__ import annotations

import importlib.util
import os
import struct
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path


TEST_ROOT = Path(__file__).resolve().parent
SCRIPT = TEST_ROOT.parent / "check-armv7-fpu.py"
SPEC = importlib.util.spec_from_file_location("check_armv7_fpu", SCRIPT)
if SPEC is None or SPEC.loader is None:  # pragma: no cover - import machinery invariant
    raise RuntimeError(f"could not load {SCRIPT}")
check_armv7_fpu = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(check_armv7_fpu)


def elf32_with_functions(functions: list[tuple[str, int, int]]) -> bytes:
    """A little-endian ELF32 file holding only a symbol table of FUNC symbols (name, value, size)."""
    strings = b"\0"
    symbols = struct.pack("<IIIBBH", 0, 0, 0, 0, 0, 0)
    for name, value, size in functions:
        symbols += struct.pack("<IIIBBH", len(strings), value, size, 0x12, 0, 1)
        strings += name.encode() + b"\0"
    # A NOTYPE local label inside the first function, as hand-written assembly leaves them.
    symbols += struct.pack("<IIIBBH", len(strings), functions[0][1] + 4, 0, 0x00, 0, 1)
    strings += b"._mainloop\0"
    header_size = 52
    symtab_offset = header_size
    strtab_offset = symtab_offset + len(symbols)
    section_offset = strtab_offset + len(strings)
    header = bytearray(header_size)
    header[:7] = b"\x7fELF\x01\x01\x01"
    struct.pack_into("<I", header, 0x20, section_offset)
    struct.pack_into("<HH", header, 0x2E, 40, 3)
    sections = struct.pack("<IIIIIIIIII", *([0] * 10))
    sections += struct.pack("<IIIIIIIIII", 0, 2, 0, 0, symtab_offset, len(symbols), 2, 1, 4, 16)
    sections += struct.pack("<IIIIIIIIII", 0, 3, 0, 0, strtab_offset, len(strings), 0, 0, 1, 0)
    return bytes(header) + symbols + strings + sections


class OffendingInstructionTests(unittest.TestCase):
    def test_d16_safe_vfp_is_accepted(self):
        for instruction in (
            "vldr d8, [r0]",
            "vldr\td15, [pc, #876]",
            "vcvt.f64.u32 d0, s0",
            "vcvt.s32.f64 s0, d1",
            "vmov r0, r1, d7",
            "vmov.32 r0, d0[1]",
            "vmov.f64 d0, #1.000000e+00",
            "vpush {d8, d9, d10, d11, d12, d13, d14, d15}",
            "vpush {d8-d15}",
            "vldmia r0, {d0-d15}",
            "vmul.f64 d0, d1, d2",
            "vcmp.f64 d0, d1",
            "vmrs APSR_nzcv, fpscr",
            "vabs.f64 d1, d2",
            "vsqrt.f64 d3, d4",
            "ldr r0, [pc, #8]",
            "mov r0, #16",
            "bl 0x1000 <d16>",
        ):
            with self.subTest(instruction=instruction):
                self.assertFalse(check_armv7_fpu.offending(instruction))

    def test_high_d_registers_q_registers_and_neon_are_refused(self):
        for instruction in (
            "vcvt.f64.u32 d16, s0",
            "vldr d17, [r0, #8]",
            "vmov d16, r0, r1",
            "vldmia r0, {d16-d31}",
            "vldmia r0, {d16, d17, d18, d19}",
            "vstmia r0, {d16, d17, d18, d19, d20, d21, d22, d23}",
            "veor q0, q1, q2",
            "vld1.8 {d0, d1}, [r1]!",
            "vst1.64 {d4}, [r0]",
            "vmov.i32 d0, #0x1",
            "vadd.i64 d0, d1, d2",
            "vshr.u64 d24, d20, #0xe",
            "vshr.u64 d4, d2, #0xe",
            "vmull.p8 q0, d1, d2",
            "vdup.32 d0, r0",
            "vbsl d0, d1, d2",
            "vmax.f32 d0, d1, d2",
        ):
            with self.subTest(instruction=instruction):
                self.assertTrue(check_armv7_fpu.offending(instruction))

    def test_trailing_comments_do_not_count(self):
        self.assertFalse(check_armv7_fpu.offending("vldr d1, [pc, #8] @ 0x1000 <d16>"))
        self.assertEqual(
            check_armv7_fpu.normalize("vldr\td16,  [pc, #8]\t@ 0x10 <x>"),
            "vldr d16, [pc, #8]",
        )


class AllowListTests(unittest.TestCase):
    def test_runtime_dispatched_code_is_allowed(self):
        for function in (
            "_ZN9libunwind13Registers_arm12restoreVFPv3EPv",
            "_ZN9libunwind13Registers_arm9saveVFPv3EPv",
            "ring_core_0_17_14__sha512_block_data_order_neon",
            "ring_core_0_17_14__x25519_NEON",
            "ring_core_0_17_14__openssl_poly1305_neon2_blocks",
            "ring_core_0_17_14__bsaes_ctr32_encrypt_blocks",
            "ring_core_0_17_14__vpaes_encrypt_key_to_bsaes",
            "ring_core_0_17_15__ChaCha20_ctr32_neon",
            "_bsaes_encrypt8",
            "_vpaes_schedule_core",
        ):
            with self.subTest(function=function):
                self.assertIsNotNone(check_armv7_fpu.allowed_reason(function))

    def test_compiled_code_is_never_allowed(self):
        for function in (
            "_RNvNtCs7RvQI1Kdg5x_19synology_drive_sync8progress14human_progress",
            "_RNvXs2_NtNtCsdn5KWBssBII_4core3num11float_parsedNtNtNtB9_3str6traits7FromStr8from_str",
            "ring_core_0_17_14__x25519_ge_scalarmult_base",
            "ring_core_0_17_14__sha512_block_data_order_nohw",
            "printf_core",
            "_ZN9libunwind13Registers_arm16restoreCoreAndJumpToEv",
            "<outside every function symbol>",
        ):
            with self.subTest(function=function):
                self.assertIsNone(check_armv7_fpu.allowed_reason(function))


class FunctionSymbolTests(unittest.TestCase):
    def test_reads_functions_and_extends_unsized_assembly_to_the_next(self):
        with tempfile.TemporaryDirectory() as scratch:
            path = Path(scratch) / "twin"
            path.write_bytes(
                elf32_with_functions(
                    [
                        ("ring_core_0_17_14__x25519_NEON", 0x1000, 0),
                        ("thumb_function", 0x2001, 0x20),
                        ("last", 0x3000, 0x10),
                    ]
                )
            )
            self.assertEqual(
                check_armv7_fpu.function_symbols(path),
                [
                    (0x1000, 0x2000, "ring_core_0_17_14__x25519_NEON"),
                    (0x2000, 0x2020, "thumb_function"),
                    (0x3000, 0x3010, "last"),
                ],
            )

    def test_a_file_without_function_symbols_is_refused(self):
        with tempfile.TemporaryDirectory() as scratch:
            path = Path(scratch) / "stripped"
            path.write_bytes(b"\x7fELF\x01\x01\x01" + bytes(45))
            with self.assertRaises(SystemExit):
                check_armv7_fpu.function_symbols(path)


@unittest.skipIf(os.name == "nt", "the fake disassembler is a POSIX script")
class CheckTests(unittest.TestCase):
    """check() end to end, with a fake llvm-objdump and llvm-objcopy."""

    FILLER = 100_000

    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory()
        self.root = Path(self.scratch.name)
        self.objcopy = self.root / "objcopy"
        # --strip-all keeps the bytes; the fake disassembler reads the listing named after the
        # file's first line, so a stripped copy disassembles like its source.
        self.objcopy.write_text(
            f"#!{sys.executable}\nimport shutil, sys\nshutil.copyfile(sys.argv[-2], sys.argv[-1])\n"
        )
        self.objdump = self.root / "objdump"
        self.objdump.write_text(
            textwrap.dedent(
                f"""\
                #!{sys.executable}
                import sys
                from pathlib import Path
                name = Path(sys.argv[-1]).read_bytes().split(b"\\n", 1)[0].decode()
                sys.stdout.write((Path({str(self.root)!r}) / (name + ".dis")).read_text())
                """
            )
        )
        for tool in (self.objcopy, self.objdump):
            tool.chmod(0o755)

    def tearDown(self):
        self.scratch.cleanup()

    def binary(self, name: str, functions: list[tuple[str, int, int]], lines: list[str]) -> Path:
        path = self.root / name
        path.write_bytes(name.encode() + b"\n" + elf32_with_functions(functions))
        filler = "".join(f"  {0x100000 + 4 * i:x}:\tmov r0, r0\n" for i in range(self.FILLER))
        (self.root / f"{name}.dis").write_text("\n".join(lines) + "\n" + filler)
        return path

    def run_check(self, shipped: Path, twin: Path) -> list[str]:
        # The ELF symbol table sits after the marker line, so read it from the real offset.
        original = check_armv7_fpu.function_symbols

        def symbols(path: Path):
            data = path.read_bytes()
            stripped = path.with_suffix(".elf")
            stripped.write_bytes(data.split(b"\n", 1)[1])
            return original(stripped)

        check_armv7_fpu.function_symbols = symbols
        try:
            return check_armv7_fpu.check(
                str(self.objdump), str(self.objcopy), shipped, twin, self.root
            )
        finally:
            check_armv7_fpu.function_symbols = original

    def test_runtime_dispatched_neon_passes(self):
        functions = [("ring_core_0_17_14__gcm_ghash_neon", 0x1000, 0x10), ("main", 0x2000, 0x10)]
        neon = ["  1000:\tvld1.64\t{d1}, [r0]!", "  1004:\tveor\tq0, q1, q2", "  2000:\tvldr d8, [r0]"]
        shipped = self.binary("shipped", functions, neon)
        twin = self.binary("twin", functions, neon)
        self.assertEqual(self.run_check(shipped, twin), [])

    def test_high_register_in_compiled_code_is_named(self):
        functions = [("ring_core_0_17_14__gcm_ghash_neon", 0x1000, 0x10), ("main", 0x2000, 0x10)]
        lines = ["  1000:\tvld1.64\t{d1}, [r0]!", "  2004:\tvcvt.f64.u32\td16, s0"]
        problems = self.run_check(
            self.binary("shipped", functions, lines), self.binary("twin", functions, lines)
        )
        self.assertEqual(len(problems), 1)
        self.assertIn("1 in main", problems[0])
        self.assertIn("2004: vcvt.f64.u32 d16, s0", problems[0])

    def test_shipped_instructions_the_twin_does_not_account_for_fail(self):
        functions = [("ring_core_0_17_14__gcm_ghash_neon", 0x1000, 0x10), ("main", 0x2000, 0x10)]
        twin_lines = ["  1000:\tvld1.64\t{d1}, [r0]!"]
        shipped_lines = twin_lines + ["  2004:\tvldr\td17, [r0]"]
        problems = self.run_check(
            self.binary("shipped", functions, shipped_lines),
            self.binary("twin", functions, twin_lines),
        )
        self.assertEqual(len(problems), 1)
        self.assertIn("1 only in the shipped binary", problems[0])
        self.assertIn("2004: vldr d17, [r0]", problems[0])

    def test_a_disassembly_that_is_not_armv7_code_is_refused(self):
        functions = [("main", 0x2000, 0x10)]
        path = self.root / "tiny"
        path.write_bytes(b"tiny\n" + elf32_with_functions(functions))
        (self.root / "tiny.dis").write_text("  2000:\tvldr d8, [r0]\n")
        with self.assertRaises(SystemExit):
            check_armv7_fpu.offending_instructions(str(self.objdump), path)


if __name__ == "__main__":
    unittest.main()
