//! The boot chime: the one service that says the boot is over, and the two
//! wrappers behind it.
//!
//! Its own file because it is the one row of the service table with a rule of
//! its own -- it belongs to no session and runs last (`after = *`) -- and
//! because `write_init_services`'s file has a line ceiling that the reasons
//! for that rule do not fit in. Called from there and from
//! `write_init_wrappers`, so the table and the scripts still go down in one
//! pass each; nothing else may write these three files.

use std::fs;
use std::path::Path;

/// The `boot-sound.service` row, into `/etc/eclipse/services`.
pub(super) fn service(svc_dir: &Path) {
    // Boot chime: the last thing in the boot, in every session. One row,
    // no `desktop =`, no `requires =`, `after = *`.
    //
    // It shipped as TWO rows, `desktop = labwc` and `desktop = xorg`,
    // while the desktop image's own default session is `abyss` whenever
    // `ec-abyss` landed in it (`abyss::session`). Init drops every service
    // tagged for another session, so BOTH were dropped before the boot
    // began and the image played nothing; a third row would have fixed
    // that boot and broken the next session added.
    //
    // No `requires =`, because it is best-effort: it named `pulseaudio`,
    // so a sound server given up on (no HDA card) cancelled a chime that
    // would have played anyway -- the player waits for Pulse's socket OR a
    // raw PCM and exits 0 when there is neither.
    //
    // `after = *` (see `AFTER_EVERYTHING` in tools/eclipse-init) is what
    // makes it the END of the boot and not the middle of it (`b` sorts
    // early). It also replaces the `wait_socket = /run/user/0/wayland-0`
    // this row carried: after the panels is after their own gates on the
    // compositor socket, and a console session has no such socket to burn
    // ten seconds on.
    fs::write(
        svc_dir.join("boot-sound.service"),
        b"# Startup MP3 (Eclipse_Awakening). See /usr/local/bin/eclipse-boot-sound.\n\
          # Session-agnostic and ordered last: the chime marks the end of\n\
          # the boot, whichever session (or none) this image boots.\n\
          exec = /usr/local/bin/eclipse-boot-sound\n\
          type = oneshot\n\
          after = *\n\
          log = /tmp/boot-sound.log\n\
          timeout = 15\n",
    )
    .unwrap();
    // The incremental path never removes a row it stopped writing, so the
    // second chime has to go by name: an image rebuilt over an older
    // rootfs would keep `boot-sound-xorg` and play the track twice.
    let _ = fs::remove_file(svc_dir.join("boot-sound-xorg.service"));
}

/// `eclipse-boot-sound` (the oneshot init runs) and `eclipse-boot-sound-play`
/// (the detached player it forks), into `/usr/local/bin`. The x bit is added
/// by `write_init_wrappers`'s own pass, like every other wrapper's.
pub(super) fn wrappers(localbin: &Path) {
    fs::write(
        localbin.join("eclipse-boot-sound"),
        b"#!/bin/sh\n\
          # Detach so the oneshot does not hold up lunarbar/xkbmap.\n\
          setsid /usr/local/bin/eclipse-boot-sound-play </dev/null >>/tmp/boot-sound.log 2>&1 &\n\
          exit 0\n",
    )
    .unwrap();
    fs::write(
        localbin.join("eclipse-boot-sound-play"),
        b"#!/bin/sh\n\
          # Wait for PulseAudio (or a raw PCM), unmute, play the startup MP3 once.\n\
          MP3=/usr/share/eclipse/Eclipse_Awakening.mp3\n\
          [ -f \"$MP3\" ] || { echo \"eclipse-boot-sound: missing $MP3\" >&2; exit 0; }\n\
          # Every mpg123 here runs under a watchdog. A player wedged in the\n\
          # audio path never comes back on its own, and it holds the PCM for\n\
          # the whole session (the device is exclusive: every later open\n\
          # answers EBUSY), so the chime would cost all later sound. The\n\
          # track is a few seconds long; anything past MAXPLAY is stuck.\n\
          MAXPLAY=20\n\
          # Prefer the real binary. Wrappers like mpg123-pulse already pass\n\
          # `-o pulse`; if we then add `-o alsa` the later flag wins, mpg123\n\
          # grabs the exclusive PCM, hangs, and the chime (and session audio)\n\
          # loops until the watchdog -- the \"final loop\" seen on Ubuntu.\n\
          MPG123=\n\
          for c in mpg123.bin mpg123; do\n\
          \x20 command -v \"$c\" >/dev/null 2>&1 && { MPG123=$c; break; }\n\
          done\n\
          # Decided BEFORE the wait below: an image without a player (the\n\
          # ISO installer root carries no usr/bin) would otherwise spend\n\
          # twenty seconds polling for a PCM it has no way to use.\n\
          [ -n \"$MPG123\" ] || { echo 'eclipse-boot-sound: mpg123 not installed' >&2; exit 0; }\n\
          play() {\n\
          \x20 \"$MPG123\" \"$@\" &\n\
          \x20 p=$!\n\
          \x20 ( i=0\n\
          \x20\x20 while [ \"$i\" -lt \"$MAXPLAY\" ]; do\n\
          \x20\x20\x20 kill -0 \"$p\" 2>/dev/null || exit 0\n\
          \x20\x20\x20 i=$((i + 1))\n\
          \x20\x20\x20 sleep 1\n\
          \x20\x20 done\n\
          \x20\x20 echo \"eclipse-boot-sound: $MPG123 stuck after ${MAXPLAY}s; killing it\" >&2\n\
          \x20\x20 kill -TERM \"$p\" 2>/dev/null\n\
          \x20\x20 sleep 1\n\
          \x20\x20 kill -KILL \"$p\" 2>/dev/null ) &\n\
          \x20 w=$!\n\
          \x20 wait \"$p\"\n\
          \x20 r=$?\n\
          \x20 kill \"$w\" 2>/dev/null\n\
          \x20 wait \"$w\" 2>/dev/null\n\
          \x20 return \"$r\"\n\
          }\n\
          i=0\n\
          while [ \"$i\" -lt 20 ]; do\n\
          \x20 if [ -S /run/pulse/native ] || [ -e /dev/snd/pcmC0D0p ] || [ -e /dev/dsp ]; then\n\
          \x20\x20 break\n\
          \x20 fi\n\
          \x20 i=$((i + 1))\n\
          \x20 sleep 1\n\
          done\n\
          if [ ! -S /run/pulse/native ] && [ ! -e /dev/snd/pcmC0D0p ] && [ ! -e /dev/dsp ]; then\n\
          \x20 echo 'eclipse-boot-sound: no PCM device' >&2\n\
          \x20 exit 0\n\
          fi\n\
          # Compositor DRM query enables NVIDIA HDMI packets; give it a beat.\n\
          sleep 2\n\
          if command -v pactl >/dev/null 2>&1 && [ -S /run/pulse/native ]; then\n\
          \x20 pactl set-sink-volume @DEFAULT_SINK@ 70% 2>/dev/null\n\
          \x20 pactl set-sink-mute @DEFAULT_SINK@ 0 2>/dev/null\n\
          \x20 # Force every sink OUT of SUSPENDED via the EXPLICIT resume path.\n\
          \x20 # The kernel PCM's implicit cold resume (run from the sink IO thread\n\
          \x20 # when a stream attaches) is unreliable under SMP+KVM: it plays silent\n\
          \x20 # or hangs. The explicit `suspend-sink 0` resume always works, so do it\n\
          \x20 # once now; with no module-suspend-on-idle loaded the sink then stays\n\
          \x20 # active for the whole session and every later play (mpg123/paplay/\n\
          \x20 # Firefox) hits an already-running PCM instead of the broken resume.\n\
          \x20 for s in $(pactl list short sinks 2>/dev/null | awk '{print $1}'); do\n\
          \x20\x20 pactl suspend-sink \"$s\" 0 2>/dev/null || true\n\
          \x20 done\n\
          elif command -v amixer >/dev/null 2>&1; then\n\
          \x20 amixer -q set Master 70% unmute 2>/dev/null\n\
          fi\n\
          if [ -n \"$MPG123\" ]; then\n\
          \x20 if [ -S /run/pulse/native ]; then\n\
          \x20\x20 # Pulse is up: ONLY use the pulse output. Opening ALSA here\n\
          \x20\x20 # races Pulse for the exclusive PCM and wedges the player.\n\
          \x20\x20 play -q -o pulse --encoding s16 --no-gapless \"$MP3\"\n\
          \x20\x20 # drain can block forever if the sink is wedged; bound it.\n\
          \x20\x20 if command -v pactl >/dev/null 2>&1; then\n\
          \x20\x20\x20 pactl drain 2>/dev/null &\n\
          \x20\x20\x20 d=$!\n\
          \x20\x20\x20 ( sleep 5; kill \"$d\" 2>/dev/null ) &\n\
          \x20\x20\x20 wait \"$d\" 2>/dev/null || true\n\
          \x20\x20 fi\n\
          \x20 else\n\
          \x20\x20 play -q -o alsa,oss --encoding s16 --no-gapless \"$MP3\"\n\
          \x20 fi\n\
          \x20 exit 0\n\
          fi\n\
          echo 'eclipse-boot-sound: mpg123 not installed' >&2\n\
          exit 0\n",
    )
    .unwrap();
}
