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

Usage: gen_ksyms.py <kernel.elf> [--nm TOOL] [--objcopy TOOL]

Exits 0 with a warning, leaving the kernel untouched, if the tools are missing
or the section is absent: a kernel without the table still boots and still
reports, it just prints bare addresses like it always did.
"""

import argparse
from pathlib import Path
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


def rustlib_bins():
    """Directories where `rustup component add llvm-tools-preview` puts llvm-nm.

    rustup installs them under the sysroot and deliberately does NOT put that
    directory on PATH, so every CI job that asked for the component still
    failed `tool_ok("llvm-nm")` and shipped its kernel without a symbol table.
    That is why kernel crash dumps in CI print bare addresses -- "[kfault-bt]
    (no in-kernel symbol table in this build ...)" -- exactly when a backtrace
    would have been most useful.
    """
    try:
        result = subprocess.run(
            ["rustc", "--print", "sysroot"], capture_output=True, check=True, text=True
        )
    except (OSError, subprocess.CalledProcessError):
        return []
    return sorted(Path(result.stdout.strip(), "lib", "rustlib").glob("*/bin"))


def resolve_tool(name):
    """`name` if it runs as given, else the same tool inside the rustup sysroot.

    An explicit path or an already-working PATH lookup always wins, so this
    only ever adds a fallback.
    """
    if tool_ok(name):
        return name
    for directory in rustlib_bins():
        candidate = str(directory / name)
        if tool_ok(candidate):
            return candidate
    return None


SHT_PROGBITS = 1


def section_size(elf):
    """Size of `.ksyms`, or None when the kernel has no reservation.

    Read straight out of the ELF rather than shelling out to readelf. The
    rustup `llvm-tools-preview` component -- the only copy of these tools many
    CI machines have -- ships llvm-nm and llvm-objcopy but no llvm-readelf, so
    a readelf fallback would still come up empty exactly where it is needed.
    """
    try:
        data = Path(elf).read_bytes()
    except OSError as e:
        warn(f"cannot read {elf} ({e})")
        return None
    if data[:4] != b"\x7fELF":
        warn(f"{elf} is not an ELF file")
        return None
    elf64 = data[4] == 2
    endian = "<" if data[5] == 1 else ">"
    # Section header: name, type, flags, addr, offset, size, ... -- the fields
    # we want sit at the same indices in both widths, only the widths differ.
    shdr = endian + ("IIQQQQ" if elf64 else "IIIIII")
    NAME, TYPE, OFFSET, SIZE = 0, 1, 4, 5
    if elf64:
        (shoff,) = struct.unpack_from(endian + "Q", data, 0x28)
        shentsize, shnum, shstrndx = struct.unpack_from(endian + "HHH", data, 0x3A)
    else:
        (shoff,) = struct.unpack_from(endian + "I", data, 0x20)
        shentsize, shnum, shstrndx = struct.unpack_from(endian + "HHH", data, 0x32)

    def header(i):
        return struct.unpack_from(shdr, data, shoff + i * shentsize)

    if shnum == 0 or shstrndx >= shnum:
        warn(f"{elf} has no section headers -- nothing to patch")
        return None
    names = header(shstrndx)[OFFSET]
    for i in range(shnum):
        h = header(i)
        start = names + h[NAME]
        if data[start : data.index(b"\0", start)] != b".ksyms":
            continue
        if h[TYPE] != SHT_PROGBITS:
            warn(f".ksyms is section type {h[TYPE]}, not PROGBITS -- cannot patch it")
            return None
        return h[SIZE]
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


def is_noise(name):
    """True for assembler bookkeeping that is not a function.

    `.L*` are compiler-local labels -- `.L0`, `.Lpcrel_hi7` -- and `$x`/`$d`
    are the ARM/RISC-V mapping symbols that mark code/data transitions. They
    sit at real addresses **inside** functions, so a lookup for an address in
    `trap_handler` resolved to the nearest one and printed `<.Lpcrel_hi7+0x4>`
    instead of the function name. They also dominated the table by count:
    17183 of the 19066 local text symbols in a riscv64 release kernel, and
    122922 of 124805 with the LLVM 18 nm on a Debian host, so most of the
    2 MiB reservation went to them instead of to functions. Dropping them also
    makes the table reproducible: both nm versions now emit the same 1894
    entries, byte for byte.
    """
    return name.startswith(".L") or (len(name) == 2 and name[0] == "$")


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
        if addr == 0 or not name or name in MARKERS or is_noise(name):
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
    args = ap.parse_args()

    nm, objcopy = (resolve_tool(tool) for tool in (args.nm, args.objcopy))
    for requested, resolved in ((args.nm, nm), (args.objcopy, objcopy)):
        if resolved is None:
            warn(
                f"{requested} not found on PATH or in the rustup sysroot"
                " -- kernel left without a symbol table"
            )
            return 0

    cap = section_size(args.elf)
    if cap is None:
        warn("no .ksyms section in this kernel -- nothing to patch")
        return 0

    try:
        syms = collect(nm, args.elf)
    except subprocess.CalledProcessError as e:
        warn(f"{nm} failed ({e}) -- kernel left without a symbol table")
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
            [objcopy, f"--update-section=.ksyms={path}", args.elf], check=True
        )
    except (OSError, subprocess.CalledProcessError) as e:
        warn(f"{objcopy} --update-section failed ({e}) -- kernel left unpatched")
        return 0
    print(f"[ksyms] {count} symbols patched into {args.elf} ({cap} bytes reserved)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
