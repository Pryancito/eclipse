# Audio: HD Audio driver, NVIDIA HDMI, PulseAudio, and `/dev/dsp`

Eclipse's audio subsystem is a single Intel HD Audio (HDA) driver that covers
every PCI class-0403 controller:

- the PCH's onboard controller (e.g. `00:1f.3` on X299 boards) with its
  analog codec,
- the HDA function of the NVIDIA GPU that drives the monitor (`xx:00.1`).
  Extra GPUs on a dual-board (e.g. a second RTX 2060 SUPER with no display)
  are left unbound, like Linux leaving those HDMI pins without ELD —
  `aplay -l` shows Intel PCH + one HDMI, not two silent NVIDIA cards.
- QEMU's `-device intel-hda -device hda-output`, used to exercise the
  driver in emulation.

## Architecture

```
userspace:  pactl / paplay / mpg123 / aplay / SDL / OpenAL
                 │ libpulse  or  ALSA `type pulse`
PulseAudio ── /run/pulse/native  (system instance, mmap=0 sink on hw:0,0)
                 │
userspace:  wavplay / ffmpeg -f oss
                 │ write(2) + OSS ioctls
/dev/dsp[N] ─ linux-object/src/fs/devfs/dsp.rs   (OSS node, one per controller)
/dev/snd/*  ─ linux-object/src/fs/devfs/snd.rs   (ALSA, Pulse's backend)
                 │ AudioScheme
drivers/src/audio/hda.rs                          (controller + codec + PCM ring)
                 │ CORB/RIRB verbs + stream DMA (all polled, no interrupts)
HDA controller (PCI 04:03) ── codec ── pin ── HDMI/DP or analog jack
```

- **Controller** (`drivers/src/audio/hda.rs`): CRST reset, codec discovery
  via STATESTS, CORB/RIRB rings with polled responses, one output stream
  over a 128 KiB physically contiguous cyclic ring described by a BDL.
  Progress is read from LPIB; consumed ring space is re-zeroed behind the
  DMA position so an underrun plays silence, never stale audio.
- **Codec graph**: the widget walk collects every output-capable pin with a
  reachable converter as a *candidate path*. Path choice is scored (digital
  HDMI/DP pin > presence > ELD valid) and — crucially — **re-evaluated at
  every stream start** (`repick_path`), because on NVIDIA GPUs presence/ELD
  only appear on the pins after the display driver pushes the monitor's ELD,
  long after this driver's PCI probe.
- **HDMI specifics**: digital converter enable, `SET_CVT_CHAN_COUNT`
  (`0x72d`) + HDMI channel slots, CEA audio infoframe through the pin's
  DIP buffer (reindexed every 8 bytes), and the NVIDIA coherent-DMA
  (snoop) PCI config bits. Path scoring prefers a pin with live
  presence/ELD so an unconnected GPU connector does not steal card 0
  from analog. At stream start the HDA driver re-enables HDMI transmission
  (`kick_hdmi_audio`) and re-runs pin sense (`SET_PIN_SENSE` then
  `GET_PIN_SENSE`) because GOP never enables audio packets.

On boot, `eclipse-boot-sound` plays `/usr/share/eclipse/Eclipse_Awakening.mp3`
once the compositor is up (`mpg123` via ALSA card 0).

## The display side (NVIDIA GPUs)

The HDA codec alone is not enough on a GPU: the **display engine** must
transmit audio packets for the head, and the codec pin only reports
presence/ELD once someone writes the ELD. That is the same split Linux
uses (`snd_hda_intel` + `nvidia`/`nouveau`).

Eclipse scans out on the UEFI GOP's boot modeset, and firmware never
enables audio. The console GPU — the one the monitor is on — is not
GSP-booted (SEC2 wedges the bus), so HDMI audio is enabled the **nouveau
way**: BAR0 MMIO on the live GOP modeset (`gf119_sor_hda_eld` /
`gf119_sor_hda_hpd` + GV100 SF_USER GCP unmute), using the UEFI EDID to
build the ELD. No GSP, no `cat /proc/gpustep14`. A GPU that already has
RM attached still uses the nvkms-equivalent RM controls
(`SET_HDMI_ENABLE`, `SET_ELD_AUDIO_CAPS`, `SET_AUDIO_ENABLE`, unmute).

Cards match a typical Linux desktop:

- **card 0** is HDMI when the monitor pin has presence/ELD (PipeWire's
  rule); otherwise the PCH analog codec (ALSA's PCI order).
- **card 1** is the other one. A second NVIDIA HDA with no display is
  not registered.

Watch for `[hdmi-audio]` lines in dmesg (`GOP ... ELD+PD+GCP unmute`
on the console GPU); `[hda]` lines show each codec's candidate paths
with their live presence/ELD state.

## Diagnosing silence: `/proc/gpusnd`

A GPU HDA function that accepts a stream proves nothing — the codec drains the
ring whether or not the display engine puts audio packets on the cable, so the
failure mode is *silence with no error anywhere*. `cat /proc/gpusnd` dumps, per
card, what the hardware is actually doing right now:

```sh
cat /proc/gpusnd
```

- **Controller/stream**: GCAP, STATESTS, the codec address, and the output
  stream descriptor — `SD_CTL` (RUN bit and stream tag), `SD_FMT`, `SD_CBL`,
  and `SD_LPIB` sampled twice 2 ms apart, reported as `ADVANCING` or
  `STALLED`. `STALLED` with RUN set means the DMA engine is not fetching:
  a controller/BDL problem, not a display one.
- **Active path**, read back *from the codec* rather than from what the driver
  believes it wrote: the converter's stream id and format, digital-converter
  enable, power state, and on the pin the OUT_EN bit, presence/ELD-valid from
  `GET_PIN_SENSE`, and EAPD. A converter whose stream id reads back 0 was
  never armed; a pin with `present=0 eld=0` has no live display.
- **Every candidate path** with its live presence/ELD, `<== ACTIVE` marking
  the one in use — this is where a wrong pin choice shows up.
- **`[hdmi-audio]`**: one line per NVIDIA GPU from the last stream-start
  kick. The monitor GPU is enabled via the GOP/BAR0 path (or RM, if GSP
  is up). Extra GPUs with no display are skipped. If the stream is
  `ADVANCING` on a pin that reports present+ELD and the monitor is still
  silent, this block is the remaining suspect.

Play something first — most of the state above only exists while a stream is
running:

```sh
speaker-test -c 2 -t sine &
cat /proc/gpusnd
```

## Userspace API: PulseAudio

The session runs a **system-instance** PulseAudio daemon (`pulseaudio --system`)
on top of ALSA card 0. That is what gives Eclipse several simultaneous playback
clients: the kernel PCM is single-client (no dmix — it needs SysV IPC), and
Pulse mixes in userspace.

- Socket: `unix:/run/pulse/native` (`PULSE_SERVER` is set in eclipse-init, the
  labwc wrapper, `/etc/profile` and labwc `environment`).
- ALSA `default` is the pulse plugin (`/etc/asound.conf`), so `aplay` / `mpg123`
  / `SDL_AUDIODRIVER=alsa` go through the daemon too. Direct kernel access is
  `aplay -D eclipse_hw` (fails while Pulse holds the device).
- The ALSA sink is `mmap=0 tsched=0`: this kernel's PCM is RW-interleaved +
`SYNC_PTR` only. Period wakeups come from the poll timer (~4 ms). After a
track ends, a kernel watchdog keeps reading LPIB so the cyclic DMA ring
cannot loop the last fragment. `module-suspend-on-idle` is deliberately NOT
loaded: a suspended sink was not being resumed when a new stream attached
(the resume runs in the sink IO thread), so every later play went silent.
The sink stays IDLE with the PCM open instead.
- A bare `mpg123 file.mp3` reaches the daemon because `/dev/dsp` refuses it.
  mpg123 1.3x has no config file at all, so with no `-o` libout123 walks its
  built-in driver list and takes the first module that both loads AND opens --
  and the OSS one is in that list. `/dev/dsp` shares the native PCM's
  single-client claim, so while PulseAudio holds `hw:0,0` the OSS open returns
  `EBUSY` and libout123 moves on to its ALSA module, `/etc/asound.conf`, the
  pulse plugin and the daemon. (Before that claim existed, OSS won the list,
  put a second writer into Pulse's ring and played silent.) `audio-probe`
  checks it in `[daemon]`, and `[oss]` skips while the daemon has the card.
- OpenAL (`ALSOFT_DRIVERS=pulse,alsa`) talks native libpulse. PI-futexes are
  implemented, so `pa_mutex_new()` no longer aborts.

```sh
pactl info
pactl list sinks short
pactl set-sink-volume @DEFAULT_SINK@ 50%
paplay music.wav          # PCM; MP3 still goes through mpg123 → ALSA → Pulse
speaker-test -c 2 -t sine # ALSA default → Pulse
```

`module-native-protocol-unix` **must** carry `auth-cookie-enabled=0`. Without
it the module loads-or-creates a cookie under a path the `pulse` account
cannot write, fails to initialise, and the daemon runs on with **no socket**:
alive, owning the cards, refusing every client (`Connection refused` from the
pulse plugin, `EBUSY` from `/dev/dsp`). xtask only rewrites configs carrying
its `# eclipse-generated` marker, so an older `system.pa` used to survive
every rebuild in that state; it now moves such a file aside (`system.pa.bak`),
always ships the canonical script as `/etc/pulse/system.pa.eclipse`, and
`eclipse-pulseaudio` starts from that copy (`pulseaudio -n --file=`) whenever
the config in place lacks the key. `audio-probe` checks the line directly.

`system.pa` loads `module-native-protocol-unix` **before** the ALSA sinks.
`module-alsa-sink` touches hardware, and a card whose load wedges leaves the
daemon alive, owning that PCM, with the socket module never reached: every
client gets `Connection refused` (ALSA `default` is the pulse plugin) while
`/dev/dsp` answers `EBUSY` because the daemon really does hold the card. With
the socket first, that costs one sink instead of the whole daemon, and
`pactl list sinks` still names the card that did not come up. `audio-probe`
reports a live daemon with no socket as its own failure.

The daemon is an eclipse-init `respawn` service (`pulseaudio.service` →
`/usr/local/bin/eclipse-pulseaudio`). Logs: `/tmp/pulseaudio.log`. A ~150 ms
exit is a startup abort (`--system` could not chown `/var/run/pulse` /
`/var/lib/pulse`, or `XDG_RUNTIME_DIR` was still `/run/user/0` after the
drop to user `pulse`); those directories are ramfs and the wrapper unsets
`XDG_RUNTIME_DIR` before exec.

## Userspace API: ALSA (`/dev/snd/*`)

System sound is mixed by PulseAudio; the kernel side is still the native ALSA
ABI: one `controlC<card>` + `pcmC<card>D0p` pair per HDA controller
(`linux-object/src/fs/devfs/snd.rs`), in the same card order as `/dev/dsp<N>`.
It implements what alsa-lib's `hw` plugin (and Pulse's `module-alsa-sink`)
needs in RW-interleaved mode — `HW_REFINE`/`HW_PARAMS` (constrained to
S16LE stereo at the HDA rate set), `SW_PARAMS`, `PREPARE`, `WRITEI_FRAMES`,
`DRAIN`/`DROP`, `PAUSE` (stops DMA, keeps the ring), `REWIND`/`FORWARD`
(silence the dropped range), `STATUS`, `DELAY` and `SYNC_PTR` (the
status/control pages are not mmap-able; alsa-lib falls back to `SYNC_PTR`
automatically).

The stream state machine follows `sound/core/pcm_native.c`:

- `PREPARE` arms the driver's start hold (`AudioScheme::set_start_hold`):
  writes queue into the ring and the engine starts once `start_threshold`
  frames are queued (alsa-lib's default is 1, aplay uses a period) or on an
  explicit `START` (PulseAudio sets the threshold to the boundary). `START`
  needs PREPARED and data (`EBADFD` / `EPIPE` otherwise).
- The ring running dry on a RUNNING stream is an underrun once `avail`
  reaches `stop_threshold` (the buffer size by default): `writei`, `DELAY`
  and `HWSYNC` answer `EPIPE`, `poll()` raises `POLLERR|POLLOUT`, and
  `PREPARE` recovers. A `stop_threshold` at the boundary keeps the stream
  free-running (the HDA engine stops itself and the next write restarts it
  seamlessly), which is also what a stall with the ring still full reports
  after a whole buffer's worth of time.
- `HW_PARAMS` (OPEN/SETUP/PREPARED only), `HW_FREE` (SETUP/PREPARED),
  `PREPARE` (not OPEN, RUNNING or DRAINING), `DROP` (not OPEN) and `PAUSE`
  (pause RUNNING, resume PAUSED) refuse other states with `EBADFD`, as Linux
  does. `SW_PARAMS` validates like `snd_pcm_sw_params` (`avail_min` 0,
  a bad `tstamp_mode`, an oversized `silence_threshold` are `EINVAL`), keeps
  every field for readback and reports the kernel's boundary rather than
  taking the client's.
- The control node also answers `TLV_READ` (`ENXIO`, no dB scale),
  `HWDEP_NEXT_DEVICE`/`RAWMIDI_NEXT_DEVICE` (none) and `POWER_STATE` (D0).

`/etc/asound.conf` (written by xtask) sets `default` to the **PulseAudio
plugin** when `pulseaudio` and `alsa-plugins-pulse` are in the image, so
mpg123/`aplay` multiplex through the daemon. The kernel PCM remains
`eclipse_hw` (`hw:0,0`) for Pulse's own ALSA sink. Format conversion is still
available as `aplay -D plug`. Without Pulse in the image, `default` is `hw:0,0`
and playback is single-client. Card 0 is the preferred playback device (HDMI/DP
with a live display outranks analog jacks); the remaining controllers follow
in PCI probe order:

```sh
aplay -l                      # list cards
aplay music.wav               # default = Pulse → hw:0,0 (usually HDMI, S16LE stereo)
aplay -D plug music.wav       # convert format/rate in userspace (via Pulse)
aplay -D eclipse_hw music.wav # kernel PCM directly (busy if Pulse is up)
speaker-test -c 2 -t sine
pactl set-sink-volume @DEFAULT_SINK@ 50%
amixer set Master 50%         # Pulse ctl, or kernel Master if Pulse is down
amixer set Master mute
```

`alsa-lib`, `alsa-utils`, `pulseaudio`, `pulseaudio-alsa`, `pulseaudio-utils`,
`libpulse` and `alsa-plugins-pulse` are baked into the rootfs package set
(`xtask/src/linux/xorg.rs`); anything missing can be added at runtime with
`apk add`. Each card exposes a simple **Master** mixer (`amixer set Master 50%`,
`amixer set Master mute`) on the kernel node; with Pulse running, lunarbar and
`pactl` adjust the sink volume instead.

Card 0 is the preferred playback device — HDMI/DP with a live display outranks
analog jacks — so `aplay` and `amixer` without `-c` hit the monitor speakers.

### hw_params negotiation (and how to test it without hardware)

alsa-lib does not hand the kernel a finished configuration. It narrows one
parameter at a time (`snd_pcm_hw_params_choose`), calling `HW_REFINE` after
every step and expecting the kernel to derive the dependent parameters — most
clients, `speaker-test` and `aplay` included, only ever set `period_time` and
`buffer_time` and let the frame counts fall out. So the refine implements
Linux-style constraint propagation, iterated to a fixed point:

```
frame_bits   = sample_bits × channels          (= 32, S16LE stereo)
period_bytes = period_size × 4
buffer_bytes = buffer_size × 4
buffer_size ≈ period_size × periods            (see below)
period_time  = period_size × 1e6 / rate
buffer_time  = buffer_size × 1e6 / rate
```

Two properties of that arithmetic are load-bearing, and both were bugs first:

* **The time↔size directions must be exact inverses.** A size of N frames owns
  the half-open time cell `[N/rate, (N+1)/rate)`. If one direction rounds a
  time up to a size and the other rounds that size back to a *different* time,
  the interval empties and hw_params fails with EINVAL — which is how 0.5 s at
  11.025 kHz (5512.5 frames) got rejected.
* **`buffer_size = period_size × periods` is approximate here.** The DMA ring
  is continuous, not carved into period segments, so a buffer that is not a
  whole number of periods plays fine. Demanding the exact multiple rejects
  ordinary requests: 0.5 s at 44.1 kHz is 22050 frames while four 125 ms
  periods are 22048. The refine carries a period of slack and `install` picks
  the exactly-coherent triple at the end.

`tools/alsa-hwparams-sim/run.sh` exercises all of this with no hardware and no
QEMU. It **extracts** the refine/install code from `snd.rs` at run time (so it
always tests what ships) and drives it with a faithful replay of alsa-lib's
negotiation across 23 scenarios — every supported rate, low latency, oversized
buffers, explicit period/periods. Exit code 0 means every scenario negotiated a
coherent configuration. Run it after touching the refine.

A rejected configuration logs the offending parameter and the full interval
state (`[snd] hw_params rejected: …`, budgeted to 8 lines a boot), so a bare
EINVAL in userspace can still be traced to the constraint that caused it.

## Userspace API: `/dev/dsp` (OSS)

One node per controller in the same order as `/dev/snd`: `/dev/dsp` is card 0
(preferred HDMI/DP when a display is live), `/dev/dsp1`, `/dev/dsp2`, … for
the rest, each with a `/dev/audio<N>` twin (Sun defaults: µ-law, 8 kHz,
mono) and a `/dev/mixer<N>`. The node follows Linux's `snd-pcm-oss`
(`sound/core/oss/pcm_oss.c`), so a program written for a Linux `/dev/dsp`
behaves the same here:

- **Formats** (`SNDCTL_DSP_SETFMT`/`GETFMTS`): µ-law, A-law, U8, S8,
  S16 LE/BE, U16 LE/BE, S24 packed / in 32 bits (LE/BE), S32 LE/BE, float.
  The ring carries S16LE stereo; the node converts on the way in and
  duplicates mono onto both channels. An unknown format is answered with
  `AFMT_U8`, as on Linux.
- **Rate** (`SPEED`, `SOUND_PCM_READ_RATE`): clamped to 1000..192000 and
  snapped to the nearest rate the HDA stream format encodes (8000, 11025,
  16000, 22050, 32000, 44100, 48000, 88200, 96000, 176400, 192000); the ioctl
  hands back the rate in effect. Linux would resample to the exact request;
  every real client reads the granted value back.
- **Fragments** (`SETFRAGMENT`, `SUBDIVIDE`, `GETBLKSIZE`, `GETOSPACE`,
  `GETOPTR`, `GETODELAY`): Linux's `snd_pcm_oss_period_size` algorithm with
  the ring as the slave constraint. By default a 48 kHz S16LE stereo client
  gets 8 fragments of 4096 bytes; `SETFRAGMENT` (once per open, as on Linux)
  picks the size and count, and a write never queues past
  `fragments × fragsize`, so the latency a client asks for is the latency it
  gets. `GETOSPACE.bytes`, `GETODELAY` and `GETOPTR` are in the client's
  bytes.
- **Triggers** (`SETTRIGGER`/`GETTRIGGER`, `DSP_CAP_TRIGGER`): clearing
  `PCM_ENABLE_OUTPUT` drops what is queued and holds the stream; writes then
  fill the buffer and nothing plays until the bit is set again (the "fill,
  then start" idiom). The hold is a driver primitive
  (`AudioScheme::set_start_hold`) and dies with the fd.
- **Writes**: whole frames go to the device, a trailing partial frame is
  kept for the next write. Blocking writes retry against the ring (bounded);
  with `O_NONBLOCK` or after `SNDCTL_DSP_NONBLOCK` a write returns what fit
  or `EAGAIN`. Underruns never surface: the driver restarts the stream on
  the next write, exactly as Linux's emulation re-prepares on `EPIPE`.
  `poll(2)` reports writable while the stream is stopped or once a whole
  fragment fits.
- **The rest**: `SYNC` (drain, then the next write re-prepares), `POST`,
  `RESET`, `GETCAPS` (revision 1, `REALTIME`, `TRIGGER`; no mmap, no
  duplex), `OSS_GETVERSION` (3.8.1a), `SOUND_PCM_READ_BITS/CHANNELS`,
  `SETSYNCRO`/`PROFILE` (accepted), `SETDUPLEX` and the `FILTER` pair
  (`EIO`), `GETISPACE`/`GETIPTR`/`MAPINBUF`/`MAPOUTBUF` (`EINVAL`).
  Anything else, including a stray `TCGETS`, is `EINVAL` as on Linux.
  `read(2)` is `ENXIO`; an `O_RDONLY` open is `EINVAL` (no capture).

The one deliberate difference: a fresh `/dev/dsp` is 48 kHz S16LE stereo
rather than Linux's 8 kHz U8 mono, so `cat music.raw > /dev/dsp` plays a
modern raw file. Only `cat` can tell; `/dev/audio` keeps the µ-law defaults.

`/dev/mixer<N>` follows `snd-mixer-oss`: `SOUND_MIXER_READ/WRITE_VOLUME` and
`_PCM` both drive the card's one gain (`Master`; HDMI/DP has no analog
volume, the driver scales S16LE), a channel written as 0 is muted,
`DEVMASK`/`STEREODEVS` list those two, `RECSRC`/`RECMASK`/`CAPS` are 0 and
`SOUND_MIXER_INFO` names the card. `aumix`, `ossmix` and mpv's OSS volume
control land here.

`/dev/dsp<N>` and `/dev/snd/pcmC<N>D0p` are two front ends onto the SAME
hardware ring, and the ring has no mixer, so they share one single-client
claim: whichever opens second gets `EBUSY`, as on Linux (one substream behind
both nodes). With PulseAudio running — it keeps `hw:0,0` open, since
`module-suspend-on-idle` is not loaded — every OSS client therefore gets
`EBUSY` and should play through the daemon instead (`mpg123 file.mp3`,
`paplay`). Stop `pulseaudio` to use `/dev/dsp` or `wavplay` directly. Two
writers on one ring is the failure this refusal prevents: the second one
interleaves into the first one's frames and both come out wrong or silent.

## Testing

```sh
wavplay --tone                 # 440 Hz sine, 3 s, /dev/dsp (card 0)
wavplay --tone 880             # another frequency
wavplay -d /dev/dsp1 --tone    # next codec (often analog)
wavplay file.wav               # 16-bit PCM WAV (mono is upmixed)
aplay -l                       # ALSA cards (needs /usr/share/alsa/alsa.conf)
aplay music.wav                # default = Pulse → hw:0,0
pactl info                     # PulseAudio daemon
pactl set-sink-volume @DEFAULT_SINK@ 80%
amixer set Master 80%          # Pulse ctl, or kernel Master if Pulse is down
```

`tools/wavplay` is a static musl binary installed into the rootfs by xtask.
`aplay`/`amixer`/`mpg123` talk to `/dev/snd` through alsa-lib (and, with Pulse
up, through the pulse plugin); QEMU boots the live initramfs, which must
include `usr/share/alsa` (the `hw` plugin lives in `alsa.conf`) and
`usr/bin/pulseaudio`. OSS (`wavplay` → `/dev/dsp`) does not need that file
and bypasses Pulse.

`HW_REFINE` returning `EINVAL` during `aplay`/`mpg123` is normal: alsa-lib
probes unsupported format/period combinations that way. A real failure prints
`Unable to set hw params` / `cannot set hw params` in the client. Rebuild the
kernel after changing `snd.rs`, and the rootfs after changing `/etc/asound.conf`.

`make qemu` attaches Intel HD Audio automatically (`-device intel-hda` +
`hda-output`). The host backend is picked from whatever QEMU supports —
pipewire, then PulseAudio, then ALSA. The `/usr/local` QEMU build in PATH
often has none of those; in that case the run uses `/usr/bin/qemu-system-x86_64`
so you can hear the guest. Override with `AUDIODEV=wav` (PCM to
`/tmp/eclipse-qemu.wav`) or `AUDIO=off` (codec present, host silent).

## Known limits

- Playback only (no capture), stereo only, S16LE only at the kernel PCM.
  PulseAudio resamples other formats in userspace.
- Several clients can play at once through PulseAudio. Direct `hw:0,0` and
  `/dev/dsp` are single-client (no dmix) and share ONE claim between them, so
  while the daemon holds the card both answer `EBUSY`.
- Volume is software PCM scaling (no analog AMP programming); already-queued
  ring contents are not retroactively gained — the new level applies to the
  next `write`. Pulse sink volume applies to mixed output.
- The DP audio path uses the same ELD/enable controls but has not been
  exercised; DP-MST audio (device entries > 0) is not implemented.
- The HDMI/DP unmute is re-sent at every digital stream start (GOP never
  enables audio packets). A monitor hot-plugged after boot gets ELD/PD when
  playback starts.
- The audio path never boots the console GPU's GSP (a `write(2)` to
  `/dev/snd` must not hang the machine). HDMI on that GPU uses the GOP/BAR0
  path instead; `/proc/gpusnd` shows `GOP ... ELD+PD+GCP unmute`.
- ALSA card 0 prefers a *live* HDMI/DP pin (presence/ELD). A dead NVIDIA pin
  does not outrank the PCH analog codec. A second GPU with no monitor is not
  registered as a sound card.
