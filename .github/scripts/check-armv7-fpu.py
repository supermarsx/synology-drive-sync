#!/usr/bin/env python3
"""Refuse armv7 code that a VFPv3-D16 CPU without NEON cannot execute.

Usage:
    check-armv7-fpu.py --objdump LLVM_OBJDUMP --objcopy LLVM_OBJCOPY SHIPPED TWIN [SHIPPED TWIN ...]

The DSM armv7 package must run on D16-only cores without NEON, such as the Armada 370, which have
d0-d15 and no Advanced SIMD unit. rustc 1.99.1 resolved its armv7 target to `+d32` and emitted
d16-d31 throughout std and our own code. The QEMU smoke on such a CPU still passed, because none
of its paths reached a float. So this check reads every instruction, not only the ones a run
executes.

SHIPPED is the stripped release binary. TWIN is the same binary rebuilt with `-C strip=none`
through `cargo rustc`, for its symbol table. The two are not byte-identical: when the link does not
strip, the musl and compiler-rt code that Zig links in differs (from `_start_c` on), and the code
after it moves. So the check does not take the twin's word for the shipped code.

1. In TWIN, whose mapping symbols separate code from data, every instruction that names d16-d31
   or a Q register, or that is NEON-only, is attributed to the function containing it. Only the
   functions in ALLOWED may hold one: code that runs only after a run-time check has found the
   hardware.
2. SHIPPED has no symbols, so its data tables disassemble as instructions too. A copy of it,
   stripped once more, is compared with a copy of TWIN stripped the same way: both must contain
   exactly the same D32/NEON instruction texts, as a multiset with addresses left out. So an
   instruction that the twin does not account for fails the check as well. An address-sized
   literal that moved with the code cannot fake one, because a value below 0x0C000000 never
   encodes a coprocessor or NEON instruction.

A failure names the function, or the address and the instruction.
"""

from __future__ import annotations

import argparse
import bisect
import re
import struct
import subprocess
import sys
import tempfile
from collections import Counter
from pathlib import Path

# Functions that may hold D32/NEON instructions, each with the reason it never runs on a D16 CPU.
ALLOWED: tuple[tuple[re.Pattern[str], str], ...] = (
    (
        re.compile(r"^_ZN9libunwind13Registers_arm(?:12restoreVFPv3|9saveVFPv3)EPv$"),
        "libunwind saves and restores d16-d31 only when a frame's unwind table names them, "
        "which D16 code never does",
    ),
    (
        re.compile(
            r"^ring_core_\d+_\d+_\d+__(?:\w*_neon\w*|\w*NEON\w*|bsaes_\w+|vpaes_\w+)$"
        ),
        "ring's NEON assembly, selected at run time only when AT_HWCAP reports NEON",
    ),
    (
        re.compile(r"^_(?:bsaes|vpaes)_\w+$"),
        "internal routines of ring's bit-sliced and vector-permutation AES, which need NEON and "
        "are reached only through its NEON entry points",
    ),
)

HIGH_D = re.compile(r"\bd(?:1[6-9]|2[0-9]|3[01])\b")
Q_REG = re.compile(r"\bq(?:[0-9]|1[0-5])\b")
# NEON-only element types: VFP never uses an integer or polynomial element type, except for
# the source or destination type of a vcvt and the 32-bit lane of a vmov.
NEON_TYPE = re.compile(
    r"^v[a-z0-9]+\.(?:i|p)(?:8|16|32|64)\b"
    r"|^v(?!cvt)[a-z0-9]+\.[su](?:8|16|64)\b"
    r"|^v(?!cvt|mov)[a-z0-9]+\.[su]32\b"
)
NEON_MNEMONIC = re.compile(
    r"^v(?:ld[1-4]|st[1-4]|dup|ext|tbl|tbx|zip|uzp|trn|rev(?:16|32|64)|cnt|clz|cls|bsl|bit|bif"
    r"|swp|and|orr|orn|eor|bic|mvn|pada?l?|padd|pmax|pmin|qadd|qsub|shl|shr|sra|sri|sli|rshr"
    r"|rsra|qshl|qrshl|movl|movn|qmovn|qmovun|mull|mlal|mlsl|tst|ceq|cge|cgt|cle|clt|abd|aba"
    r"|max|min|recpe|rsqrte|recps|rsqrts|hadd|hsub|rhadd)(?:\.|$)"
)
INSTRUCTION = re.compile(r"^\s*([0-9a-f]+):\s+(\S.*)$")
# The fewest instructions a real build of either binary disassembles to; fewer means the
# disassembler did not read the file as armv7 code.
MINIMUM_INSTRUCTIONS = 100_000

STT_FUNC = 2
SHT_SYMTAB = 2


def fail(message: str) -> None:
    print(f"check-armv7-fpu: {message}", file=sys.stderr)
    raise SystemExit(1)


def normalize(instruction: str) -> str:
    """The instruction without its trailing comment and with single spaces."""
    return " ".join(instruction.split("@", 1)[0].split(";", 1)[0].split())


def offending(instruction: str) -> bool:
    """True when INSTRUCTION ("mnemonic operands") needs d16-d31, a Q register or NEON."""
    instruction = normalize(instruction)
    if not instruction.startswith("v"):
        return False
    mnemonic, _, operands = instruction.partition(" ")
    if HIGH_D.search(operands) or Q_REG.search(operands):
        return True
    return bool(NEON_TYPE.search(mnemonic) or NEON_MNEMONIC.search(mnemonic))


def allowed_reason(function: str) -> str | None:
    for pattern, reason in ALLOWED:
        if pattern.match(function):
            return reason
    return None


def function_symbols(path: Path) -> list[tuple[int, int, str]]:
    """Every STT_FUNC symbol of a little-endian ELF32 file as sorted (start, end, name).

    Hand-written assembly often declares a function without a `.size`; such a symbol extends to
    the next function symbol.
    """
    data = path.read_bytes()
    if data[:4] != b"\x7fELF" or data[4] != 1 or data[5] != 1:
        fail(f"{path} is not a little-endian ELF32 file")
    (shoff,) = struct.unpack_from("<I", data, 0x20)
    shentsize, shnum = struct.unpack_from("<HH", data, 0x2E)
    sections = [
        struct.unpack_from("<IIIIIIIIII", data, shoff + index * shentsize)
        for index in range(shnum)
    ]
    symbols: list[tuple[int, int, str]] = []
    for _name, kind, _flags, _addr, offset, size, link, _info, _align, entsize in sections:
        if kind != SHT_SYMTAB:
            continue
        strtab = sections[link]
        strings = data[strtab[4] : strtab[4] + strtab[5]]
        for index in range(size // entsize):
            name_offset, value, symbol_size, info, _other, _shndx = struct.unpack_from(
                "<IIIBBH", data, offset + index * entsize
            )
            if info & 0xF != STT_FUNC:
                continue
            end = strings.index(b"\0", name_offset)
            name = strings[name_offset:end].decode("utf-8", "replace")
            start = value & ~1  # a Thumb function's address has bit 0 set
            symbols.append((start, start + symbol_size, name))
    if not symbols:
        fail(f"{path} has no function symbols; build the twin with -C strip=none")
    symbols.sort()
    starts = [start for start, _end, _name in symbols]
    sized = []
    for start, end, name in symbols:
        if end == start:
            following = bisect.bisect_right(starts, start)
            end = starts[following] if following < len(starts) else start
        sized.append((start, end, name))
    return sized


def offending_instructions(objdump: str, path: Path) -> list[tuple[int, str]]:
    """(address, normalized text) of every D32/NEON instruction in PATH's .text."""
    if not path.is_file():
        fail(f"not a file: {path}")
    disassembly = subprocess.run(
        [objdump, "-d", "--no-show-raw-insn", "--section=.text", str(path)],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    count = 0
    found = []
    for line in disassembly.splitlines():
        match = INSTRUCTION.match(line)
        if not match:
            continue
        count += 1
        if offending(match.group(2)):
            found.append((int(match.group(1), 16), normalize(match.group(2))))
    if count < MINIMUM_INSTRUCTIONS:
        fail(f"{objdump} disassembled only {count} instructions from {path}")
    return found


def check(objdump: str, objcopy: str, shipped: Path, twin: Path, scratch: Path) -> list[str]:
    symbols = function_symbols(twin)
    starts = [start for start, _end, _name in symbols]
    allowed: Counter[str] = Counter()
    refused: dict[str, list[str]] = {}
    twin_found = offending_instructions(objdump, twin)
    for address, text in twin_found:
        position = bisect.bisect_right(starts, address) - 1
        function = "<outside every function symbol>"
        if position >= 0 and address < symbols[position][1]:
            function = symbols[position][2]
        if allowed_reason(function):
            allowed[function] += 1
        else:
            refused.setdefault(function, []).append(f"{address:x}: {text}")

    label = shipped.name
    problems = []
    for function, hits in sorted(refused.items()):
        problems.append(
            f"{label}: {len(hits)} in {function} (first: {'; '.join(hits[:3])})"
        )

    stripped = []
    for role, path in (("shipped", shipped), ("twin", twin)):
        copy = scratch / f"{path.name}.{role}.stripped"
        subprocess.run([objcopy, "--strip-all", str(path), str(copy)], check=True)
        stripped.append(copy)
    shipped_found = offending_instructions(objdump, stripped[0])
    shipped_texts = Counter(text for _address, text in shipped_found)
    twin_texts = Counter(text for _address, text in offending_instructions(objdump, stripped[1]))
    if shipped_texts != twin_texts:
        extra = shipped_texts - twin_texts
        missing = twin_texts - shipped_texts
        samples = [f"{a:x}: {t}" for a, t in shipped_found if t in extra][:5]
        problems.append(
            f"{label}: the shipped binary's D32/NEON instructions are not the twin's "
            f"({sum(extra.values())} only in the shipped binary, e.g. {'; '.join(samples) or '-'}; "
            f"{sum(missing.values())} only in the twin): the twin does not account for them"
        )

    print(
        f"{label}: {len(twin_found)} D32/NEON instructions in the symbolized twin, "
        f"{sum(allowed.values())} of them in {len(allowed)} run-time dispatched functions and "
        f"{sum(len(h) for h in refused.values())} elsewhere; the shipped binary "
        + ("decodes to the same" if shipped_texts == twin_texts else "does NOT decode to the same")
        + f" {len(shipped_found)} as the twin stripped alike"
    )
    for function, count in sorted(allowed.items()):
        print(f"  allowed {count:5d}  {function}")
    return problems


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--objdump", required=True, help="llvm-objdump with the ARM target")
    parser.add_argument("--objcopy", required=True, help="llvm-objcopy")
    parser.add_argument("binaries", nargs="+", type=Path, metavar="SHIPPED TWIN")
    args = parser.parse_args(argv)
    if len(args.binaries) % 2:
        parser.error("binaries come in SHIPPED TWIN pairs")
    problems: list[str] = []
    with tempfile.TemporaryDirectory() as scratch:
        for index in range(0, len(args.binaries), 2):
            problems += check(
                args.objdump,
                args.objcopy,
                args.binaries[index],
                args.binaries[index + 1],
                Path(scratch),
            )
    if problems:
        print(
            "check-armv7-fpu: instructions a VFPv3-D16 CPU without NEON cannot execute, outside "
            "code that checks for the hardware first:",
            file=sys.stderr,
        )
        for problem in problems:
            print(f"  {problem}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
