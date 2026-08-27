#!/bin/sh
# Exercise the kernel's ALSA hw_params negotiation without hardware.
#
# The refine/install logic lives in linux-object/src/fs/devfs/snd.rs and is
# EXTRACTED from that file at run time (never copied), so this always tests the
# code that actually ships. It is then driven by a faithful replay of what
# alsa-lib does to a driver: snd_pcm_hw_params_any, the *_near helpers, and
# snd_pcm_hw_params_choose's fix-one-parameter-at-a-time walk — the sequence
# that produced "Unable to set hw params: Invalid argument" on real hardware.
#
# Usage:  tools/alsa-hwparams-sim/run.sh        (exit 0 = every scenario passes)
set -e
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
out=${TMPDIR:-/tmp}/alsa-hwparams-sim
mkdir -p "$out"

python3 - "$root" "$here" "$out" <<'PYEOF'
import sys
root, here, out = sys.argv[1], sys.argv[2], sys.argv[3]
src = open(f'{root}/linux-object/src/fs/devfs/snd.rs').read()
block = src[src.index('    // ── hw_params refine'):src.index('    /// Blocking interleaved write')]
# Bridge the kernel block into a hosted harness: the ring size is a constant,
# the device call and the kernel logging macros are stubbed.
block = block.replace('let ring = self.ring_frames();', 'let ring = RING;')
i = block.index('        let (actual_rate, _ch) = self')
j = block.index('        // Hand back exact singletons for everything.')
block = block[:i] + '        let actual_rate = rate;\n' + block[j:]
k = block.index('        info!(\n            "[snd] pcmC{}D0p configured')
l = block.index('        Ok(())\n    }', k)
block = block[:k] + block[l:]
block = (block.replace('warn!(', 'eprintln!(').replace('info!(', 'eprintln!(')
              .replace('use core::sync::atomic::{AtomicU32, Ordering};',
                       'use std::sync::atomic::{AtomicU32, Ordering};')
              .replace('|b| {', '|b: u32| {'))
harness = open(f'{here}/harness.rs').read()
open(f'{out}/main.rs', 'w').write(harness.replace('// @@KERNEL_BLOCK@@', block))
PYEOF

rustc -O -o "$out/sim" "$out/main.rs" 2>&1 | grep -E '^error' -A8 || true
exec "$out/sim"
