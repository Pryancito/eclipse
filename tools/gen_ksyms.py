#!/usr/bin/env python3
"""Patch the in-kernel symbol table into a linked zCore image.

The kernel reserves a fixed-size `.ksyms` section (see `zCore/src/ksyms.rs`).
This reads the ELF's own symbol table, builds the compact lookup table the
kernel expects, and writes it into that section **in place**. The reservation
never changes size, so nothing shifts and no relink is needed.

Why the kernel carries symbols at all: crash reports print raw addresses, and
those are only meaningful against the exact ELF that produced them. Two builds
of one commit on different machines do not share a `.text` layout, so a log
symbolized against the wrong kernel names the wrong functions -- confidently.

Usage: gen_ksyms.py <kernel.elf> [--cap BYTES] [--nm TOOL] [--objcopy TOOL]

Exits 0 with a warning, leaving the kernel untouched, if the tools are missing
or the section is absent: a kernel without the table still boots and still
reports, it just prints bare addresses like it always did.
"""

import argparse
import re
import struct
import subprocess
import sys
import tempfile

KSYM_MAGIC = 0x4D59534B  # "KSYM"
KSYM_VERSION = 1
HEADER_LEN = 32
# Keep names readable without letting one monomorphized generic eat kilobytes.
MAX_NAME = 96


def warn(msg):
    print(f"[ksyms] {msg}", file=sys.stderr)


def tool_ok(name):
    try:
        subprocess.run([name, "--version"], capture_output=True, check=True)
        return True
    except (OSError, subprocess.CalledProcessError):
        return False


def section_size(objcopy_readelf, elf):
    """Size of `.ksyms`, or None when the kernel has no reservation."""
    try:
        out = subprocess.run(
            [objcopy_readelf, "-S", "--wide", elf], capture_output=True, text=True, check=True
        ).stdout
    except (OSError, subprocess.CalledProcessError):
        return None
    for line in out.splitlines():
        m = re.search(r"\.ksyms\s+(\w+)\s+([0-9a-f]+)\s+([0-9a-f]+)\s+([0-9a-f]+)", line)
        if m:
            if m.group(1) != "PROGBITS":
                warn(f".ksyms is {m.group(1)}, not PROGBITS -- cannot patch it")
                return None
            return int(m.group(4), 16)
    return None


# Linker-script markers: zero-length labels that sit at a section boundary, not
# functions. They must never enter the table. `_copy_user_end` is the reason:
# the `.text.copy_user` region is declared but empty, so the label lands on the
# image base, and `MAX_SYM_SPAN` then let it claim every low address --
# including the 0x...06 stack-bottom sentinel that ends every coroutine
# backtrace, which printed as a confident `<_copy_user_end+0x6>` frame in every
# crash report this hunt produced.
MARKERS = {
    "stext",
    "etext",
    "_copy_user_start",
    "_copy_user_end",
    "ksyms_start",
    "ksyms_end",
    "kcounters_desc_start",
    "kcounters_desc_end",
    "kcounters_desc_vmo_start",
    "kcounters_arena_start",
    "kcounters_arena_end",
}


def shorten(name):
    """Trim a long symbol from the MIDDLE, never the end.

    Rust trait-impl symbols put the informative part last: cutting at a fixed
    prefix length produced

        <zcore::handler::ZcoreKernelHandler as kernel_hal::kernel_handler::KernelHandler

    in a crash report -- 80 characters that name the type and the trait and not
    the method, which is the one thing the reader needs. Keeping both ends costs
    nothing and answers the question.
    """
    if len(name) <= MAX_NAME:
        return name
    # Bias toward the tail: the method name lives there.
    keep_tail = (MAX_NAME * 2) // 3
    keep_head = MAX_NAME - keep_tail - 2
    return name[:keep_head] + ".." + name[-keep_tail:]


def collect(nm, elf):
    """`[(addr, name)]` for every function symbol, address-sorted and deduped."""
    out = subprocess.run(
        [nm, "--defined-only", "--demangle", "--print-size", "--numeric-sort", elf],
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    syms = []
    for line in out.splitlines():
        parts = line.split(" ", 3)
        # With --print-size: "addr size type name"; sizeless symbols (hand
        # written assembly, markers) print "addr type name" instead.
        if len(parts) >= 4 and len(parts[1]) > 1 and parts[2].lower() == "t":
            addr_s, size_s, name = parts[0], parts[1], parts[3]
        elif len(parts) >= 3 and parts[1].lower() == "t":
            addr_s, size_s, name = parts[0], "0", " ".join(parts[2:])
        else:
            continue
        try:
            addr, size = int(addr_s, 16), int(size_s, 16)
        except ValueError:
            continue
        name = name.strip()
        if addr == 0 or not name or name in MARKERS:
            continue
        syms.append((addr, size, shorten(name)))
    # Address first, then sized symbols before sizeless ones: at a shared
    # address the real function wins over an alias or a stray label.
    syms.sort(key=lambda s: (s[0], 0 if s[1] else 1))
    deduped = []
    for addr, _size, name in syms:
        if deduped and deduped[-1][0] == addr:
            continue
        deduped.append((addr, name))
    return deduped


def build(syms, cap):
    """The blob, padded to `cap`. Drops trailing symbols rather than overflow.

    Entries hold 32-bit offsets from `image_base` (the first symbol's address,
    rounded down to a page) rather than absolute addresses -- half the index for
    the same information. The base travels in the header, so the table is not
    tied to any one architecture's kernel base.
    """
    image_base = syms[0][0] & ~0xFFF
    syms = [(a, n) for a, n in syms if a - image_base < (1 << 32)]
    strtab = bytearray()
    offsets = {}
    entries = []
    for addr, name in syms:
        off = offsets.get(name)
        if off is None:
            off = len(strtab)
            offsets[name] = off
            strtab += name.encode("utf-8", "replace") + b"\0"
        entries.append((addr - image_base, off))
        if HEADER_LEN + len(entries) * 8 + len(strtab) > cap:
            entries.pop()
            warn(
                f"table does not fit in {cap} bytes -- keeping the first {len(entries)} "
                f"of {len(syms)} symbols. Raise KSYMS_CAP in zCore/src/ksyms.rs."
            )
            break
    strtab_off = HEADER_LEN + len(entries) * 8
    blob = bytearray(
        struct.pack(
            "<IIIIQQ",
            KSYM_MAGIC,
            KSYM_VERSION,
            len(entries),
            strtab_off,
            image_base,
            0,
        )
    )
    assert len(blob) == HEADER_LEN
    for addr_off, name_off in entries:
        blob += struct.pack("<II", addr_off, name_off)
    blob += strtab
    if len(blob) > cap:
        return None, len(entries)
    blob += b"\0" * (cap - len(blob))
    return bytes(blob), len(entries)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("elf")
    ap.add_argument("--nm", default="llvm-nm")
    ap.add_argument("--objcopy", default="llvm-objcopy")
    ap.add_argument("--readelf", default="llvm-readelf")
    args = ap.parse_args()

    for tool in (args.nm, args.objcopy, args.readelf):
        if not tool_ok(tool):
            warn(f"{tool} not found -- kernel left without a symbol table")
            return 0

    cap = section_size(args.readelf, args.elf)
    if cap is None:
        warn("no .ksyms section in this kernel -- nothing to patch")
        return 0

    try:
        syms = collect(args.nm, args.elf)
    except subprocess.CalledProcessError as e:
        warn(f"{args.nm} failed ({e}) -- kernel left without a symbol table")
        return 0
    if not syms:
        warn("no function symbols found -- is the ELF fully stripped?")
        return 0

    blob, count = build(syms, cap)
    if blob is None:
        warn("could not fit even a truncated table -- kernel left unpatched")
        return 0

    with tempfile.NamedTemporaryFile(suffix=".ksyms", delete=False) as f:
        f.write(blob)
        path = f.name
    try:
        subprocess.run(
            [args.objcopy, f"--update-section=.ksyms={path}", args.elf], check=True
        )
    except (OSError, subprocess.CalledProcessError) as e:
        warn(f"{args.objcopy} --update-section failed ({e}) -- kernel left unpatched")
        return 0
    print(f"[ksyms] {count} symbols patched into {args.elf} ({cap} bytes reserved)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
