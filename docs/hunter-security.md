# hunter — Eclipse OS in-kernel security subsystem

`hunter` is a small in-kernel **security solution** for Eclipse OS that combines
an **LSM-style enforcement layer** (mandatory policy checks at well-defined
kernel hook points) with a **behavioural intrusion-detection system** (IDS).
The kernel calls into a handful of hooks; hunter consults its policy engine,
runs anomaly heuristics, and records every decision in a forensic event log
that userspace can read at `/proc/hunter`.

The crate lives at [`hunter/`](../hunter) and is `#![no_std]`. It has no hard
dependency on `kernel-hal`: the kernel injects a monotonic clock at boot via
`hunter::set_time_source`, keeping the crate low in the build graph and unit
testable on the host.

## Hooks

| Hook                       | Kernel call site                         | Domain          |
|----------------------------|------------------------------------------|-----------------|
| `check_syscall`            | `linux-syscall` dispatch                 | seccomp + IDS   |
| `check_exec_target`        | `sys_execve`                             | exec policy     |
| `check_elf_binary`         | boot-time spawn (`loader`)               | exec policy     |
| `check_exec_path`          | `PT_INTERP` / `#!` interpreters          | exec policy     |
| `check_exec_integrity`     | `sys_execve`, interpreters, boot spawn   | exec integrity  |
| `check_exec_av`            | `sys_execve`, interpreters, boot spawn   | antivirus       |
| `check_mmap`               | `sys_mmap`                               | W^X memory      |
| `firewall::check`          | `sys_connect`, `sys_bind`, `sys_sendto`, `sys_sendmsg` | per-application firewall |
| `check_mprotect`           | `sys_mprotect`                           | W^X memory      |
| `check_file_write`         | `write`/`pwrite`/`writev`/`pwritev`, per chunk, regular files | behavior (ransomware) |
| `check_file_rename`        | `rename`/`renameat`/`renameat2`          | behavior (ransomware) |
| `check_file_unlink`        | `unlink`/`unlinkat` (not `rmdir`)        | behavior (ransomware) |
| `check_process_write`      | `process_vm_writev`                      | behavior (injection) |
| `task_fork` / `task_exec`  | `clone` / `execve`                       | lineage for `behavior` |
| `task_exit`                | `sys_exit_group`                         | state cleanup   |
| `netmon::on_listen`        | `sys_listen`                             | network monitor |
| `netmon::observe`          | each read of `/proc/hunter_net`          | network monitor |
| `init` / `set_time_source` | `zCore` boot (`primary_main`)            | bring-up        |

## Enforcement domains and modes

There are three independently-tunable enforcement domains, each with its own
`Mode` so the subsystem can be rolled out audit-first and tightened per-domain:

- **`Off`** — domain disabled (no checks, no logging).
- **`Report`** — log the violation but allow the action (audit / IDS mode).
- **`Enforce`** — log the violation and block the action.

| Domain    | What it governs                                   | Default   |
|-----------|---------------------------------------------------|-----------|
| `syscall` | Per-process syscall whitelists (lightweight seccomp) | `Enforce` |
| `wx`      | Write-xor-execute memory (`mmap` / `mprotect`)    | `Report`  |
| `exec`    | Executable path policy (untrusted/world-writable) | `Report`  |
| `anomaly` | Block (vs. only log) detected floods / fork bombs | `Report`  |
| `integrity` | SHA-256 of the image vs. its pinned digest; also the [boot](#boot-integrity-measured-boot) | `Report`  |
| `av`      | Antivirus signatures matched on the image          | `Report`  |
| `net`     | Which programs may reach the network ([firewall](#per-application-firewall)) | `Report`  |
| `behavior` | Ransomware and process injection, by behaviour    | `Report`  |

The defaults are deliberately conservative. The syscall domain is `Enforce`
because whitelists are opt-in per process (no policy registered ⇒ permissive),
so it cannot break a process that didn't ask for filtering. The W^X, exec-path
and anomaly domains default to `Report` because real dynamic linkers / JITs
transiently create `W+X` mappings and the base system legitimately execs from a
variety of paths — blocking by default would risk breaking userspace. Operators
opt into `Enforce` per domain (`policy::set_wx_mode`, `set_exec_mode`,
`set_anomaly_mode`). The control plane can be sealed **tighten-only**
(`policy::seal_tighten_only`) so that, after boot, no domain can be relaxed —
only moved towards stricter enforcement. Operators set all of this without
touching kernel code through the [control plane](#control-plane-policyconf-proc-hunter-hunterctl).

### Hardening (v0.3.0)

A multi-agent adversarial red-team (see `docs/hunter-hardening.md`) drove these
defenses, all preserving the conservative-default contract:

- **W^X is structural, not per-call.** File-backed `mmap` is capped to a
  W^X-preserving permission *ceiling* (no execute on writable file pages, no
  write on executable ones) instead of the old blanket `RXW`, and a per-process
  *ever-writable region* map catches the two-step `mmap(W)`→`mprotect(X)`
  bypass. Under enforcement, `mprotect` no longer swallows a failed narrowing.
- **Exec gate hardened.** Full ELF `e_ident`/`e_type`/`e_machine` validation
  (foreign-arch images rejected); `#!` scripts recognised as valid; the integrity
  check reads the *same VMO bytes* that get mapped (no TOCTOU); `/proc/*/fd/*`
  magic-links and path-traversal treated as untrusted; the dynamic linker and
  shebang interpreter audited through the same gate.
- **Forensics tamper-evident.** Severity-segregated rings so a flood of benign
  events cannot evict `Warning`/`Critical` evidence; per-severity drop counters;
  an optional durable sink streams high-severity events to the kernel log.
- **State lifecycle fixed.** Cleanup runs from the central `Process::terminate`
  (every exit path, not just `exit_group`); per-pid maps are LRU-capped; the
  monotonic clock is sealed and the IDS window has a count backstop so a frozen
  clock cannot silence detection.
- **Per-architecture syscall tables** (x86_64 + asm-generic for aarch64/riscv64)
  and a **system-wide fork-rate** signal for distributed fork bombs.

## Trusted-program allowlist + blacklist (application control)

The `exec` domain combines a **learning allowlist** (a whitelist that never
denies) with an operator **blacklist** (the only hard deny), plus the existing
location denylist. Order of precedence at every `execve` — and for the dynamic
linker and `#!` interpreters, which pass through the same gate:

1. **Blacklist** — a program matching `/etc/hunter/blacklist` is always blocked
   (`EACCES`) while the `exec` domain is active. This is the deny half.
2. **Learning (trust-on-first-use)** — when enabled, a *safe* program (a valid
   ELF/script, not blacklisted, not from a world-writable location such as
   `/tmp`, `/var/tmp`, `/dev/shm` or a `/proc/*/fd/*` magic-link) is
   automatically added to the allowlist and **allowed without denial**. Merely
   relative paths (e.g. a package manager exec'ing `lib/apk/.../busybox`) are
   *not* treated as unsafe, so they are learned rather than warned about.
3. **Denylist + allowlist** — anything left is checked against the untrusted
   locations and, if the allowlist is active, the trusted set. Under `Report`
   this only logs; under `Enforce` a non-trusted program is blocked.

Canonicalization runs first, so a traversal like `/bin/../opt/evil` cannot
masquerade as a trusted program.

### `/etc/hunter/` configuration

At boot the kernel **reads** (never writes) two optional newline-delimited
files from the root filesystem and enables learning:

| File                      | Effect                                         |
|---------------------------|------------------------------------------------|
| `/etc/hunter/whitelist`   | trusted programs that may always run           |
| `/etc/hunter/blacklist`   | denied programs that must never run            |

Each non-empty, non-`#` line is one entry; a trailing `/` makes it a directory
prefix, otherwise it is an exact program path. Missing files are fine (the
lists stay empty). Example `/etc/hunter/whitelist`:

```
# trusted directories
/bin/
/usr/bin/
# one exact extra binary
/opt/myapp/bin/agent
```

### Persistence of learned programs

Learned entries live in kernel memory and are surfaced at `/proc/hunter` under
a `learned-programs (N):` block. The kernel deliberately does **not** write the
filesystem. [`hunterd`](#hunterd-events-and-learning-on-disk) copies that block
to `/var/lib/hunter/learned` every few seconds, and at the next boot the kernel
reads the file back (after the white/blacklists, before `policy.conf`) into
the **learned** set, so learning picks up where it stopped and `/proc/hunter`
still tells learned programs from configured ones.

Each restored line passes the same test a program passes to be learned: an
exact absolute path (a directory prefix is skipped), not blacklisted, not in a
world-writable location, not a `/proc/*/fd/*` link, and no control characters.
That last rule also applies to learning itself now: a program whose name holds
a newline still runs, but is not learned, because `/proc/hunter` would list it
as two lines and the second would come back from disk as a program that never
ran. The file only grows; to stop trusting a learned program, `hunterctl deny`
it (a denied program is not restored).

`hunterctl save-learned` is still there to promote the learned set into
`/etc/hunter/whitelist`, where an operator can review and edit it.

### Programmatic control

```rust
use hunter::{policy, Mode};

policy::set_exec_learning(true);                              // whitelist learns, never denies
policy::add_blacklisted_exec_prefix(String::from("/tmp/"));  // hard-deny a location
policy::add_trusted_exec_path(String::from("/srv/agent"));   // pre-trust a program
policy::set_exec_mode(Mode::Enforce);                        // also deny non-trusted execs
```

## Control plane: `policy.conf`, `/proc/hunter`, `hunterctl`

Every knob above used to be a Rust function that only kernel code could call,
and the kernel called none of the ones that tighten: a booted system ran
`wx`, `exec` and `anomaly` in `Report` for good, and the tighten-only latch was
never engaged. The control plane is one small language
([`hunter::control`](../hunter/src/control.rs)) with three ways in:

| Way in                     | When               | Who                  |
|----------------------------|--------------------|----------------------|
| `/etc/hunter/policy.conf`  | once, at boot, after the white/blacklists | the kernel |
| write to `/proc/hunter`    | any time           | root (`euid 0`) only |
| `/sbin/hunterctl`          | any time           | root; a front end for the write above |

### Language

One directive per line; blank lines and `#` comments are ignored.

| Directive                              | Effect                                              |
|----------------------------------------|-----------------------------------------------------|
| `mode <domain> <off\|report\|enforce>` | `domain` is `syscall`, `wx`, `exec`, `anomaly`, `integrity`, `av` or `behavior` |
| `learning <on\|off>`                   | exec trust-on-first-use                             |
| `anomaly-detection <on\|off>`          | per-process flood / fork-bomb counters              |
| `privileged-deny <on\|off>`            | block module/ptrace/bpf/kexec under `anomaly=enforce` |
| `trust <path>` / `trust <dir/>`        | add to the exec allowlist                           |
| `deny <path>` / `deny <dir/>`          | add to the exec blacklist                           |
| `untrusted <dir/>`                     | add a world-writable location                       |
| `hash <sha256> <path>`                 | pin a program's SHA-256 (see below)                 |
| `require-hash <on\|off>`               | an unpinned program is an integrity violation       |
| `signature <hash\|bytes>:<data>:<Name>` | add an antivirus signature (never removable)       |
| `media-scan <on\|off>`                 | queue new removable media for a scan (default on)   |
| `media-lock <on\|off>`                 | nothing runs from media until it scans clean (default off) |
| `media <id> scanning\|failed`          | a scan agent took / gave up on a device             |
| `media <id> done <files> <infected>`   | a scan agent's verdict on a device                  |
| `net <allow\|deny> <path> [in\|out]`  | a [firewall](#per-application-firewall) rule; no direction is both |
| `net forget <path>` / `net dismiss <path>` | drop a program's rules / its open questions     |
| `net timeout <secs>` / `net mode <mode>` | how long `enforce` waits for an answer (1-300 s); same as `mode net` |
| `canary <path>`                        | a decoy file: touching it is ransomware             |
| `exempt <path>`                        | a program the `behavior` detectors leave alone      |
| `release <pid>`                        | lift a process's ransomware containment             |
| `seal`                                 | engage the one-way tighten-only latch               |

**All or nothing.** A script (a whole `policy.conf`, or one `write(2)` to
`/proc/hunter`) is parsed in full before anything is applied; one bad line
rejects all of it. The rejection is a `CRIT` event naming the line
(`CONTROL REJECTED: /etc/hunter/policy.conf: line 7: unknown mode 'enforcing'`)
and, for a write, `EINVAL`. A policy file that is larger than 1 MiB, not
UTF-8 or comes back short is rejected the same way: half a policy is a stance
nobody wrote.

**The latch.** Directives run in order, so `seal` binds the lines after it.
Once sealed, every relaxing directive is refused and recorded (`mode X` to a
lower mode, `learning on`, `trust`, `anomaly-detection off`,
`privileged-deny off`, `media-scan off`, `media-lock off`, `exempt`, `release`, a
clean verdict for a device already found infected); tightening ones still apply.
Only a reboot unseals.

**Who may write.** `/proc/hunter` is mode `0644`, and the write handler checks
the *effective* uid of the writing process, not just the open: a descriptor
opened by root and inherited by an unprivileged child cannot drive hunter.
Each applied script is recorded with the writer's pid.

### `hunterctl`

```
hunterctl [status]              modes, counters, lists (no event ring)
hunterctl events                the full report, event ring included
hunterctl mode <domain> <mode>
hunterctl learning|anomaly-detection|privileged-deny <on|off>
hunterctl trust|deny <path|dir/>
hunterctl untrusted <dir/>
hunterctl canary <file>...      create decoy files, arm them now and at every boot
hunterctl exempt <program>      the ransomware/injection detectors leave it alone
hunterctl release <pid>         lift a process's ransomware containment
hunterctl seal
hunterctl apply <file>          send a whole script (e.g. a new policy.conf)
hunterctl hash <file>...        pin SHA-256 digests (live, and in /etc/hunter/hashes)
hunterctl require-hash <on|off>
hunterctl verify                sha256sum -c /etc/hunter/hashes
hunterctl boot                  boot integrity: verdict, measured and pinned digests
hunterctl boot pin [item...]    pin this boot's measurements (live + /etc/hunter/boot)
hunterctl boot verify           re-hash the kernel's code now (exit 1 if tampered)
hunterctl scan [--quarantine] <path>...   antivirus scan (exit 1 if infected)
hunterctl quarantine <file>...  move into /var/lib/hunter/quarantine, deny the path
hunterctl quarantined           list the quarantine
hunterctl restore <sha256>      put a quarantined file back
hunterctl update-signatures     download the feeds in /etc/hunter/feeds (signed ones verified)
hunterctl feeds                 installed feeds, trusted keys, saved versions
hunterctl feed-rollback <feed> [<version>]   back to a saved version, live
hunterctl trust-key <name> <hex>   trust an Ed25519 feed key (live, and in /etc/hunter/keys)
hunterctl require-signed-feeds <on|off>
hunterctl scan-media [id|dir]...  scan removable media (default: every pending one)
hunterctl media-scan|media-lock <on|off>
hunterctl lockdown              exec + integrity + av enforce, media-lock on, learning off, seal
hunterctl report [--pdf|--txt] [--days N] [-o FILE]
                                security report (below), into /var/lib/hunter/reports/
hunterctl save-learned          append learned programs to /etc/hunter/whitelist
hunterctl daemon [seconds]      hunterd (see below); eclipse-init runs it
hunterctl persist               one pass of hunterd, now
hunterctl history [--alerts|--critical] [--this-boot] [N]
                                the last N events on disk (default 50, 0 = all)
```

A typical hardening pass on a running system: let it run with learning on
while the usual workload executes, `hunterctl save-learned`, then
`hunterctl lockdown`. From then on only allowlisted (configured or learned)
programs run, and nothing can loosen that until reboot. `lockdown` leaves W^X
alone on purpose: enforcing it breaks every JIT (Firefox, any JS engine).

The image ships `/etc/hunter/policy.conf` as a fully commented example
([`tools/hunterctl/policy.conf`](../tools/hunterctl/policy.conf)), so it
changes nothing until edited; a unit test keeps every line of it valid.

### From the desktop: lunarbar's shield

The bottom bar ([`tools/lunarbar`](../tools/lunarbar)) carries a shield next to
the keyboard pill, read from `/proc/hunter` every three seconds:

| Shield | Meaning |
|---|---|
| solid, accent colour | every domain at least reports |
| outline, dim | some domain is `off` |
| solid, red | a critical event arrived that nobody has looked at yet |

Clicking it opens hunter's menu, which also clears the red (opening the menu is
what "looked at" means). The menu has:

- the seal (`sellado` / `sealed`) and the counters (blocked, criticals,
  signatures);
- one row per domain with `off / report / enforce` (`off / aviso / bloqueo` in
  Spanish), plus `learning off / on`. A click sends `mode <domain> <mode>` or
  `learning <on|off>`. Once sealed, the relaxing segments are dimmed and not
  clickable; should a relaxation still be refused (hunter accepts the write
  and only says so in its ring), the menu re-reads the modes and says so;
- the newest four warnings and criticals;
- four buttons: **scan**, **signatures**, **log** and **lockdown**. With
  `lunarhunter` installed (below) the first three open it on the right tab
  (`lunarhunter --quarantine --scan $HOME`, `--update`, `--tab eventos`);
  without it they run `hunterctl scan --quarantine $HOME`,
  `hunterctl update-signatures` and `hunterctl events` in a terminal.
  **Lockdown** takes a second click to confirm and then sends exactly what
  `hunterctl lockdown` does (a unit test compares the two);
- **abrir** / **open** in the header, which opens `lunarhunter`.

The bar holds no policy of its own: it reads the same report and sends the
same directives as `hunterctl`, through the same root-only `/proc/hunter`
(the desktop session runs as root). A kernel without hunter has no
`/proc/hunter`, and then there is no shield. `LUNARBAR_DUMP=/path:WxH
LUNARBAR_DUMP_HUNTER=1 lunarbar` renders the menu to a file without a
compositor.

### lunarhunter: hunter's window

`lunarhunter` ([`tools/lunarbar/src/bin/lunarhunter.rs`](../tools/lunarbar/src/bin/lunarhunter.rs))
is the rest of hunter as a desktop application: a normal xdg-shell window
(in the taskbar, movable, resizable, framed by labwc), built and installed with
the bar as `/bin/lunarhunter`, in the app menu as **Seguridad (hunter)**
(`/usr/local/bin/eclipse-security`, which falls back to a terminal on
`hunterctl events`). Eight tabs, keys `1`-`8` or Tab:

| Tab | What it does |
|---|---|
| Resumen | the stance in one line, eight counters, the latest alerts; quick actions: scan home, update signatures, lockdown, **export report** (the week's report as HTML into `$HOME`) |
| Protección | the six domains with what each guards, the six switches (`learning`, `anomaly-detection`, `privileged-deny`, `require-hash`, `media-scan`, `media-lock`); **save as boot policy** writes the live stance to `/etc/hunter/policy.conf` (old file kept as `policy.conf.bak`), **apply policy.conf**, and **seal** |
| Eventos | the whole event ring with time, pid, severity and category, filtered by all / alerts / critical |
| Escaneo | presets (home, `/tmp`, programs, `/`, or `--scan PATH...`), a live count with a stop button, per-finding or all-at-once quarantine, or automatic quarantine as it goes |
| Cuarentena | `/var/lib/hunter/quarantine/index`, each row restorable (two clicks) |
| Firmas | what the kernel holds (hash / byte patterns / detections), the files under `/etc/hunter/signatures{,.d}`, the feeds, and **update now** |
| Programas | the allowlist counts, kernel vs. disk pins, the learned programs, **save learned** (`hunterctl save-learned`) and **verify integrity** (`hunterctl verify`) |
| Programados | the scheduled scans (see [Antivirus](#antivirus-signatures)): per job off / daily / weekly, the day, the time in 30-minute steps, alert or quarantine, its next run and **run now** (`hunterctl run-scheduled`); then the history, newest first |
| Red | every TCP/UDP socket with the process holding it (pid, program, local and remote address), unexpected listeners first and in red with an **allow** button (`hunterctl expect-port`, two clicks), the alerts switch, and the expected ports with **remove** (`hunterctl forget-port`); re-read every two seconds while open. See [Network monitor](#network-monitor) |
| Cortafuegos | the [firewall](#per-application-firewall): each open question ("¿Permitir que wget se conecte a la red?") with **Permitir** / **Bloquear** / **Ignorar**, the rules with **Olvidar**, and the `net` mode; every change is sent to `/proc/hunter` and the live rules are written to `/etc/hunter/firewall` |

Like the bar it holds no policy: settings go through `/proc/hunter`, scans
through `/proc/hunter_scan` (the same batches of 256 paths `hunterctl scan`
writes, from a thread, so the count moves), and the file work (quarantine,
restore, feed downloads, the whitelist, verify) runs `/sbin/hunterctl` and
shows its output in the tab. One-way or file-moving actions (lockdown, seal,
saving the boot policy, quarantine all, restore) take a second click. Under
the seal, relaxing controls are not offered; without root, nothing that
changes hunter is. `lunarhunter --tab NAME --dump /path:WxH` renders a tab to
a raw ARGB8888 file without a compositor (from a built-in sample when there
is no `/proc/hunter`).

### Security reports

`hunterctl report`, `lunarhunter --export PATH [--days N]` and the **Exportar
informe** button write the same report
([`tools/lunarbar/src/hunter_export.rs`](../tools/lunarbar/src/hunter_export.rs)),
in the desktop's language, for the last 7 days by default:

- the stance and the counters of the period (events by severity, criticals,
  antivirus detections, files quarantined, signatures, pinned programs);
- **recommendations**, each naming the `hunterctl` command that acts on it:
  domains off or only reporting, stale or missing signatures, learning still
  on, an overflowed event ring, no pins or `require-hash off`, no seal;
- the six modes and the switches; where the alerts come from (by category
  and by file/program); every alert with its date, pid and text; the
  quarantine of the period; the signature files and feeds; the learned
  programs; and the whole event log, folded.

Three formats, by the file's extension: **HTML** (one self-contained page,
nothing fetched, prints cleanly), **PDF** (the text version paginated in the
standard Courier font, so it needs no library and no embedded font) and
**text**. `-o -` writes to stdout. Without `-o` the file is
`/var/lib/hunter/reports/hunter-informe-YYYY-MM-DD.<ext>`, one per day, so a
weekly run keeps the history.

The events come from hunterd's history (`/var/log/hunter/events.log` and its
rotations, `HUNTERD BOOT` lines left out, `LOST` lines kept) plus whatever the
ring holds that hunterd has not written yet (by this boot's id and sequence
number, so nothing counts twice). When the history cannot be read (no hunterd,
or a reader that is not root: the log is `0640`) the ring is all there is, and
it starts at boot. Either way the events section covers the period **or from
where the events start, if that is later**, and the report says which; the
quarantine and the signatures are on disk and cover the whole period. The
report also flags when the ring overflowed (`dropped=`).

### Notifications on the desktop

lunarbar pops a card up in the corner above the bar for every new warning or
critical in hunter's event ring
([`hunter_toast.rs`](../tools/lunarbar/src/hunter_toast.rs), what each event
offers in [`hunter_notify.rs`](../tools/lunarbar/src/hunter_notify.rs)):

| Event | Buttons |
|---|---|
| malware (`AV`) with a file | **cuarentena** (`hunterctl quarantine FILE`), ver, ignorar |
| a program the allowlist refused (`EXEC`, *not on trusted-program allowlist*), not sealed | **permitir** (`hunterctl trust PATH`), ver, ignorar |
| a firewall question (`NET ASK`) | **permitir** / **bloquear** (`net allow\|deny PATH out\|in`, then the live rules saved to `/etc/hunter/firewall`), ver (the Cortafuegos tab); stays until answered |
| anything else | ver, ignorar |

**ver** opens the event log (`lunarhunter --tab eventos`), **ignorar** stops
showing that exact event until the bar restarts, and the cross just closes the
card. Quarantine and allow report back with a second card saying whether it
worked. Warnings go after 12 s, criticals stay until answered, and no card
times out while the pointer is on it. At most three are shown; a burst
beyond that waits behind a **+N más** line, and the same event repeated is
one card with a count. The backlog the ring holds when the bar starts is not
shown (the menu and the window are for that). Permitir is not offered for a
program in an untrusted location such as `/tmp`, because hunter checks those
before the allowlist and trusting one would change nothing.

**Anything else can raise one too**, which is how the other parts of hunter
are meant to tell or ask the user something: drop a file in
`/run/hunter/notify/` (root-owned, mode 700), or from a shell:

```sh
hunterctl notify --severity info "Escaneo programado" "0 amenazas en /home"
ans=$(hunterctl notify --severity crit --wait 60 \
        --action allow:permitir:"hunterctl ..." --action deny:denegar \
        "Cortafuegos" "¿Permitir que /usr/bin/curl se conecte a la red?")
# $ans is allow, deny, dismiss or timeout (exit 2)
```

Each action is `ID:LABEL[:COMMAND]`: the bar runs COMMAND with `/bin/sh -c`
when the button is pressed, and with `--wait` writes the ID back. From Rust,
`lunarbar::hunter_notify::{post, take_answer}` do the same. The bar runs those
commands as root, so it reads only a spool directory and files owned by root
(or its own user) and writable by nobody else, and skips anything else.
`LUNARBAR_DUMP=/path:WxH LUNARBAR_DUMP_TOAST=1 lunarbar` renders a sample
stack without a compositor.

### hunterd: events and learning on disk

`/proc/hunter`'s event ring holds the last few hundred events of the current
boot, and the learned set lives in kernel memory: a reboot used to lose both.
`hunterd` is `hunterctl daemon`, started by eclipse-init
(`/etc/eclipse/services/hunterd.service` → `/usr/local/bin/eclipse-hunterd`)
and restarted if it dies. Every 5 seconds it reads `/proc/hunter` and:

- appends every event it has not written yet to `/var/log/hunter/events.log`;
- merges the learned programs into `/var/lib/hunter/learned` (see above).

| File | Mode | What |
|---|---|---|
| `/var/log/hunter/events.log` (+ `.1` … `.4`) | `0640` | the history, rotated past 1 MiB, so at most 5 MiB |
| `/var/lib/hunter/learned` | `0600` | the learned set the kernel restores at boot |
| `/var/lib/hunter/events.cursor` | `0640` | `<boot_id> <next seq>`: where the next pass starts |

`events.log` is tab-separated, one event per line, oldest first:

```
time                  boot      seq  uptime   pid  severity category action  description
2026-10-29T18:46:00Z  aaaaaaaa  17   4.500    142  CRIT     EXEC     BLOCKED blacklisted program: /tmp/x
```

`time` is UTC, worked out from the uptime (so it is only as right as the
clock was), `boot` is the first 8 characters of
`/proc/sys/kernel/random/boot_id`, and `seq` is hunter's own sequence number,
which starts again at 0 each boot. hunterd adds two kinds of lines of its
own, all with category `HUNTERD` and seq `-`: `BOOT` opens each boot,
and `LOST` names the sequence numbers that left the ring before a pass could
read them (a flood between two passes), so a gap is never silent.

A restarted hunterd carries on from the cursor, so nothing is written twice.
eclipse-init does not signal its services before `reboot(2)`, so at most the
last 5 seconds of events before a reboot do not reach the disk (the durable
sink still sends every `Warning`+ event to the kernel log). A `TERM` makes
hunterd do one last pass and exit.

`hunterctl history` prints the file for people (`--alerts` for warnings and
criticals, `--critical`, `--this-boot`, and a count); it reads only the disk,
so it works after a reboot and on a kernel without hunter. Tools that want the
history (reports, scheduled scans) should read `events.log*` directly.

## Per-application firewall

`hunter::firewall` ([`hunter/src/firewall.rs`](../hunter/src/firewall.rs))
decides which **programs** may reach the network. Every other domain sees a
dropper calling home and `curl` fetching a page as the same `connect(2)`; this
one asks which executable is doing it, and keeps the answer as a rule.

**What is checked.** `connect`, and `sendto` / `sendmsg` with a destination
address, are the outgoing direction (`out`); `bind` to a fixed port is the
incoming one (`in`, the program wants to be a server). Never asked about:
loopback either way, the unspecified address as a destination, a bind to port
0 (a client choosing its source port), and anything that is not IP (unix,
netlink, packet sockets) — the desktop itself lives on those. The kernel half
is [`linux-syscall/src/net/hunter_firewall.rs`](../linux-syscall/src/net/hunter_firewall.rs).
The program is the process's executable as `execve` recorded it (so a script
is its interpreter).

**Rules and questions.** A rule is `allow` or `deny`, for one program and one
direction or both. A program with no rule becomes a **question**: it shows up
in `/proc/hunter_firewall` and in lunarhunter's **Cortafuegos** tab, which
answers it with `net allow|deny <path> <dir>` on `/proc/hunter`. What happens
meanwhile depends on the mode:

| Mode | No rule | `deny` rule |
|---|---|---|
| `off` | allowed, not asked | allowed |
| `report` (default) | allowed, and asked | **blocked** (`EACCES`) |
| `enforce` | the call **waits** for the answer (30 s, `net timeout`), then `EACCES` | **blocked** |

A `deny` blocks under `report` too: `report` is about hunter's own judgement,
and a rule the person gave is not that. Under `enforce` the waiting call wakes
on `allow`/`deny` within a quarter of a second, and on a signal with `EINTR`;
a question that timed out stays open, so the next attempt can still be allowed.
The question list is capped at 32 (the least recently asked goes first), and
a program retrying against a `deny` is logged at 1, 2, 4, 8... blocks so it
cannot flood the ring.

**`/proc/hunter_firewall`** (read-only), one record per line, path last:

```
mode report
timeout 30
now 640
rule allow both /usr/bin/curl
rule deny out /usr/bin/nc
pending out 3 142 610 636 93.184.216.34:443 /usr/bin/wget
```

`pending <dir> <count> <pid> <first> <last> <peer> <path>`, times in seconds
since boot.

**`/etc/hunter/firewall`** is loaded at boot (before `policy.conf`, so a
`seal` there seals the rules), line by line — a bad line loses one rule:

```
mode enforce
timeout 30
allow /usr/bin/curl
deny /usr/bin/nc out
```

lunarhunter rewrites it from the live state after every answer.

**The latch.** `net allow` and `net forget` of a program with a `deny` relax
the policy and are refused once sealed; `net deny` always applies, and
`net dismiss` changes no rule.

## Exec integrity (SHA-256)

The `exec` domain decides *where* a program may run from and *which names*
are trusted. It cannot tell `/usr/bin/ssh` from a replacement that someone
with write access to `/usr/bin` dropped in under the same name. The
`integrity` domain ([`hunter::integrity`](../hunter/src/integrity.rs)) checks
exactly that, IMA-style:

- A program with a **pinned** SHA-256 runs only if the image about to be
  mapped still hashes to that digest.
- With `require-hash on`, a program with **no** pin is a violation too, which
  makes integrity a deny-by-default allowlist.

`Report` (the default) records a mismatch as a `WARN` event and lets the
program run; `Enforce` blocks it with `EACCES` and a `CRIT` event. With
nothing pinned, the domain checks nothing.

**What is hashed** is the VMO the loader maps, not a second read of the file,
so a writer racing the check cannot swap the bytes between hash and load. That
covers the program, its `PT_INTERP` dynamic linker, `#!` interpreters, and
the programs the kernel spawns at boot.

**Names.** A program is looked up both by the name it was run by and by the
path it resolves to through links, after canonicalization. Pinning
`/bin/busybox` therefore covers every applet link to it. If both names are
pinned, both pins must match.

**Cost.** Only pinned images (or every image, under `require-hash`) are
hashed, and each image only once: the digest is cached by the image's kernel
object id, and the kernel's executable cache hands the same image to every
exec of an unchanged file.

**Pins** come from `/etc/hunter/hashes`, read at boot in `sha256sum` format
(`<sha256>  <path>`), so `sha256sum /bin/* > /etc/hunter/hashes` is a valid
file. A line that does not parse is skipped by name; the rest still load. Pins
can also come from `hash` directives, or from `hunterctl hash`, which pins live
and updates the file in one step.

Pinning is a relaxation: under `require-hash` it admits a program, and a new
digest admits new bytes under an old name. So the tighten-only latch refuses
pins, and `require-hash off`, after `seal`. Once a system is sealed, a
package update that changes a pinned binary stops that binary from running
until reboot. That is the intended trade-off.

### The `execve` gate is back

While adding this domain it turned out that `sys_execve` had stopped calling
hunter at all. The gate was dropped in "cache executable file images across
execve" (June 2026), which replaced the block that called it. Since then,
every runtime `execve` skipped the blacklist, the untrusted-location policy
and the allowlist; only boot-time spawns and interpreters were still checked.
The gate is restored as `check_exec_target`, after the ELF/`#!` preflight (so
a file that is not a program is still `ENOEXEC` for the shell) and before
the old image is torn down (so a refusal is an ordinary `EACCES`). Lists are
asked of the name, and the resolved path is also checked against the
blacklist and the untrusted locations. That means a symlink from a
trusted-looking name to `/tmp/evil` no longer hides where the file is.

## Boot integrity (measured boot)

Exec integrity starts at the first `execve`. Everything before it -- the
kernel, its command line, the initramfs holding `/sbin/init` and
`/etc/hunter/policy.conf` -- decides whether any later check means anything:
a cmdline without hunter's switches, a `kernel.elf` with the exec gate patched
out, or an initramfs whose `policy.conf` says `mode exec off` all boot a
system on which hunter reports green. [`hunter::boot`](../hunter/src/boot.rs)
covers that stretch, in two halves.

**Measured boot.** rboot hashes (SHA-256) each input it hands the kernel and
passes the digests in `BootInfo` ([`rboot::measure`](../rboot/src/measure.rs)).
`measure=` in `rboot.conf` chooses what:

| `measure=` | Measured | Cost |
|------------|----------|------|
| `kernel` (default) | the kernel ELF, `rboot.conf`, the cmdline | lost in the time the firmware takes to read them |
| `all` | those and the initramfs, hashed while it is read | seconds: the initramfs is the whole live root |
| `off` | nothing | -- |

rboot never refuses to boot over a digest: it does not know what they should
be, and a loader that stopped on a mismatch would turn a kernel update nobody
re-pinned into a machine that shows a splash and stops. The kernel compares.

**Kernel code.** Right after hunter comes up the kernel hashes its own
`.text` and `.rodata` as mapped (the `kernel-code` item), and re-hashes them
every ten minutes and on `hunterctl boot verify`. Nothing in this kernel
writes either range after boot, so a digest that changes is code patched in
memory -- which is how a kernel rootkit hooks a function. A change is sticky:
code patched and patched back is still a compromised boot.

**Pins** live in `/etc/hunter/boot`, `sha256sum`-shaped with an item where the
path would be (`<sha256>  kernel`). Items: `kernel`, `initramfs`, `rboot.conf`,
`cmdline`, `kernel-code`; anything else is refused as a typo. `hunterctl boot
pin` writes the running boot's own measurements there and pins them live --
trust on first use, so run it on a boot you trust: right after installing or
updating the kernel. Only pinned items are judged:

- measured and equal: `ok`;
- measured and different: `MISMATCH`;
- pinned and **not measured**: `UNMEASURED`, a violation too -- `measure=off`
  or an older rboot that never measured must not be the way around a pin.

The verdict is in `/proc/hunter_boot` (and as `boot:` in `/proc/hunter`).
The domain shares `mode integrity`: `report` records each violation as a
`CRIT` `BOOT` event; `enforce` records it and **locks hunter down** -- exec,
integrity and av to `enforce`, learning off, the latch sealed -- because a
boot cannot be un-booted, and a system that started from inputs nobody pinned
should keep every guard up and not be talked out of them (not even by the
`policy.conf` a tampered initramfs ships) until a clean reboot. The appraisal
runs after `policy.conf` is applied, so the mode is the operator's.

**Limits.** The chain starts at rboot, and nothing measures rboot: a replaced
rboot reports whatever digests it likes. That link is UEFI Secure Boot's.
The pins live on the root filesystem, which in a live boot *is* the
initramfs, so there pin from a trusted copy rather than from the running
system; on an installed system the root is its own partition. The installer
rewrites the cmdline (`ROOT=`), so pin after installing, not from the live
medium.

## Antivirus (signatures)

[`hunter::av`](../hunter/src/av.rs) is a classic signature engine. A
signature is either the SHA-256 of a whole known-bad file
(`hash:<sha256>:<Name>`) or a byte pattern found inside one
(`bytes:<hex>:<Name>`). In a byte pattern `??` matches any byte; a pattern
needs at least 4 fixed bytes and must start with one.

**On access.** Every image the kernel is about to run is scanned before it is
mapped: `execve`, the `PT_INTERP` dynamic linker, `#!` interpreters and
boot-time spawns. The scan runs on the same VMO the loader maps, next to the
integrity check. A match is a `WARN` under `mode av report` (the default) and
a blocked exec (`EACCES`, `CRIT AV BLOCKED`) under `enforce`. With an empty
database nothing is scanned. Verdicts are cached per image together with the
database generation, so an unchanged binary is scanned once and scanned again
after the database grows.

**On demand.** `/proc/hunter_scan` (root only): write paths, one per line;
each write is scanned before it returns, then reading gives `clamscan`-style
results (`path: OK`, `path: <Name> FOUND`, `path: ERROR …`). `hunterctl scan
/home /tmp` walks the trees and feeds it in batches. It prints only detections
and errors plus a summary, and exits 1 if anything was found. A detection is
also recorded as an `AV DETECT` event.

**Quarantine.** `hunterctl quarantine <file>` (or `scan --quarantine`) moves
the file to `/var/lib/hunter/quarantine/<sha256>` with mode `000`, logs its
original path and mode in `index`, and adds the path to the exec blacklist, so
a fresh copy dropped there will not run. `hunterctl restore <sha256>` puts the
file back with its mode. The path stays denied until reboot, because hunter
never forgets a deny.

**Database and updates.** `/etc/hunter/signatures` (local; ships
empty, since a sample signature such as EICAR's text pattern matched every
file that quoted it, the shipped programs and documentation included) and every feed in `/etc/hunter/signatures.d/` are
loaded at boot. `hunterctl update-signatures` downloads each feed in
`/etc/hunter/feeds` (`<format> <name> <url> [<key>]`; formats `sha256` and
`sigs`) with curl or wget into a dot-file in `signatures.d`, and writes
`install <feed> <file> [<file.sig>]` to `/proc/hunter_feed`. The kernel reads
the file itself, checks it and loads it whole; only once it said yes is the
feed renamed into place and a copy saved in `/var/lib/hunter/feeds/<feed>/`
(the last five versions). An empty, failed or refused download keeps the
previous feed. The default feed is abuse.ch MalwareBazaar's list of malware
SHA-256s from the last 48 hours. Schedule it with crond, e.g.
`0 */6 * * * /sbin/hunterctl update-signatures`.

**Signed feeds.** A signature is not harmless because it can only make
hunter refuse more: `hash:<sha256 of /bin/sh>` under `mode av enforce` stops
the system. A feed with a `<key>` in `/etc/hunter/feeds` must come with
`<url>.sig`, a six-line header signed with Ed25519 (RFC 8032):

```
hunter-feed-signature 1
feed <feed>
serial <version, a number that only grows>
key <key name>
sha256 <SHA-256 of the feed file>
ed25519 <signature over the five lines above>
```

The kernel (`hunter::feed`) installs it only if the key is trusted, the
signature verifies, the SHA-256 is the file's, the feed name is the one it is
installed under, and the serial is not older than the installed one (a
replayed old feed is refused). A feed installed signed is never replaced by
an unsigned one, and `require-signed-feeds on` refuses unsigned feeds
altogether; MalwareBazaar signs nothing, so that is off by default. Keys come
from `feed-key <name> <hex>` (`/etc/hunter/keys` at boot, read before the
feeds; `hunterctl trust-key` live). The same checks run at boot on each
`signatures.d/<feed>` with its `<feed>.sig`. The publisher signs on another
machine with `tools/hunterctl/hunter-feed-sign` (OpenSSL 3): `keygen` makes a
key and prints its `feed-key` line, `sign` writes the `.sig`, and
`from-sha256` turns a MalwareBazaar list into hunter lines, so a signed
mirror of it is three commands.

**Rollback.** hunter remembers which signatures each feed brought, so a new
version takes out what the old one had and it does not, unless another feed
or the local database still holds it. `hunterctl feed-rollback <feed>
[<version>]` (`hunterctl feeds` lists the saved versions) reinstalls a saved
version with `rollback`, the only way past the serial check, and its
signatures are gone live, without a reboot: the cure for a feed whose newest
version blocks a legitimate program. Once sealed, no signature leaves the
database: updates and rollbacks still add, and what they would remove stays
until reboot. The seal also freezes the keys and `require-signed-feeds on`.

**Scheduled scans.** `/etc/hunter/schedule` holds the scans that run on
their own, one per line: `<job> <when> <HH:MM> <report|quarantine>
<path>...`, where `when` is `daily`, a weekday (`mon` .. `sun`) or `off`, in
local time. The image ships two: `quick`, daily at 12:00, over the program
directories, `/tmp`, `/root` and `/home`; and `full`, on Sundays at 13:00,
over `/` (a scan never enters `/proc`, `/sys`, `/dev`, `/run` or the
quarantine). Both only report by default; `quarantine` also puts what they
find away. The `hunter-scheduler` service (`hunterctl scheduler`, supervised
by eclipse-init) checks once a minute. A job is due at the first slot after
the one it last started in, like anacron, so a machine that was off at noon
runs the day's scan when it comes back, once. A job seen for the first time
starts counting from then.

- `hunterctl schedule` lists the jobs with their last and next run.
- `hunterctl schedule quick mon 08:30 quarantine` changes the named fields
  and leaves the rest of the file, comments included, alone.
- `hunterctl run-scheduled full` runs a job now. It is recorded like a
  scheduled run, with trigger `manual`.
- `hunterctl scan-history [N]` prints the last runs from
  `/var/lib/hunter/scans/history`: date, job, trigger, seconds, files,
  infected, quarantined, status (`clean`, `infected` or `failed`) and the
  report file. The newest 30 reports and 1000 runs are kept.

Only one scan runs at a time (a pid file in `/var/lib/hunter/scans/lock`).
Every finding is an `AV DETECT` event like any other scan's, so it reaches
lunarbar's shield. A run the clock started also posts a card on the desktop
(`hunterctl notify`): a passing note when it is clean, a warning when
everything it found was quarantined, a critical card with a **quarantine**
button when something was left in place, and a warning with the report's
path when the run did not finish. A run started by hand posts nothing. lunarhunter's **Programados** tab shows the same jobs and
history and changes them through these commands.

**Limits.** Hashes cover the whole file. Byte patterns are searched in the
first 128 MiB of an image, and each pattern is a separate anchored pass, which
fits the hundreds of patterns an operator curates rather than a commercial
database. Shared libraries loaded later with `mmap(PROT_EXEC)` are not scanned
yet; their bytes are only scanned on demand.

## Removable media (USB sticks, cards, external disks)

[`hunter::media`](../hunter/src/media.rs) scans every filesystem mounted from
a block device after boot: a USB stick, an SD card, an external disk, a loop
image. `mount(2)` tells hunter about each one (the boot mounts from
`/etc/fstab` do not go through it), and `/proc/hunter` lists them:

```
media: auto-scan=on lock=off devices=1
media-device: id=3 state=pending files=0 infected=0 fstype=vfat source=/dev/sdb1 target=/media/usb
```

**Who scans.** The kernel does not walk the new tree itself: `mount(2)` would
not return until every file on a 64 GB stick had been read. A userland agent
does, through the same `/proc/hunter_scan` batches as `hunterctl scan`, and
reports back in the control language (`media <id> scanning`, then
`media <id> done <files> <infected>` or `media <id> failed`):

* **lunarbar** (as root) sees a `state=pending` device on its next read of
  `/proc/hunter`, within a few seconds, and runs `hunterctl scan-media <id>`
  in the background, one device at a time. Its shield menu has a line for the
  device being scanned (or an infected one, or the newest), and lunarhunter's
  *Resumen* tab lists every device with its verdict, the findings of an
  infected one and a button to scan it again.
* **`hunterctl scan-media`** scans every pending device by hand: on a console
  without the desktop, or when the bar is not running as root. With an id or
  a mount point it scans that device again.

`scan-media` walks with `find -xdev` (a disk mounted inside the stick is its
own device), leaves one report per device in `/run/hunter/media/<id>` (the
`FOUND` and `ERROR` lines and a summary), and exits 1 if anything was found.

**What a detection does.** An infected device is a `CRIT MEDIA INFECTED`
event, so the bar's shield turns to alert. Each infected file is `deny`ed
(the exec blacklist, until reboot), so it does not run even with
`mode av report`. The files are not moved into the quarantine: they are on
somebody's stick, and `hunterctl quarantine` is one step away.

**The lock.** `media-lock on` refuses to run anything from a device whose
scan has not come back clean: pending, being scanned, failed or infected
(`CRIT MEDIA BLOCKED`). It is asked of the file's real location, like the
blacklist, so a link elsewhere to a program on the stick is still that
program. It is off by default, because without an agent a device stays
pending and its programs would never run; `hunterctl lockdown` turns it on.
`media-scan off` mounts new devices as `state=skipped`, which the lock lets
through.

**Limits.** Only one scan runs at a time, and `/proc/hunter_scan` keeps one
batch of results for everybody: a manual `hunterctl scan` while a stick is
being scanned can mix their results. A file written to the stick after its
scan is not scanned again until it is run (on-access) or the device is
scanned again.

## Network monitor

`hunter::netmon` answers the two questions `/proc/net/tcp` cannot: **which
program holds which socket**, and **did something start listening that
nobody expected**. A backdoor's first move once it runs is to open a port
(`nc -l -p 4444 -e /bin/sh`) or call home, and the other domains only watch
what runs.

The kernel walks every process's descriptor table and hands hunter one entry
per (process, inet socket), as `netstat -p` pairs them. A **listener** is a TCP
socket in `LISTEN`, or an unconnected UDP socket on a port below the ephemeral
range (49152): a client's DNS query is not a service, `bind(0.0.0.0:53)` is. A
listener no rule expects is a `WARN` event in category `NET` (so the shield in
lunarbar lights up), once per process and port:

```text
[  412] +  1203.551s pid=733   WARN   NET       WARNING: unexpected listener: tcp 0.0.0.0:4444 by /tmp/.x/nc
```

TCP listeners are reported from `sys_listen` the moment they open; UDP ones,
and anything else, at the next read of the table. Listeners on loopback only
are exempt by default, since the network cannot reach them. The monitor only
watches: it never refuses a `bind`, `listen` or `connect` (that is a
firewall's decision, per program), so it cannot break a service by being
wrong.

**`/proc/hunter_net`** — read (by anyone, like `/proc/net/tcp`), a fresh
snapshot:

```text
hunter netmon
alerts=on ignore-loopback=on sockets=3 listeners=2 unexpected=1 reported=1
expect udp 68
expect udp 546
expect tcp 22 /usr/sbin/sshd
sock 412 tcp LISTEN 0.0.0.0:22 - listen /usr/sbin/sshd
sock 733 tcp LISTEN 0.0.0.0:4444 - ALERT /tmp/.x/nc
sock 612 tcp ESTAB 10.0.2.15:49822 93.184.216.34:443 ok /usr/bin/firefox
```

Written by root, a script in netmon's own small language, all or nothing:

| Directive | Effect |
|---|---|
| `expect <tcp\|udp\|any> <port\|lo-hi> [<program>]` | that listener is expected; with a program, only for that program |
| `forget <tcp\|udp\|any> <port\|lo-hi> [<program>]` | drop a rule written the same way |
| `alerts <on\|off>` | unexpected listeners raise events or not |
| `ignore-loopback <on\|off>` | loopback-only listeners are exempt or not |

Once sealed, the directives that silence alerts (`expect`, `alerts off`,
`ignore-loopback on`) are refused and recorded, like every relaxation.

**`/etc/hunter/ports`** holds the same directives, read at boot before
`policy.conf` (line by line: a bad line loses one rule, named in the log). The
DHCP clients (`udp 68`, `udp 546`) are expected without a line. `hunterctl net`
prints the table; `hunterctl expect-port tcp 22 /usr/sbin/sshd` and
`hunterctl forget-port ...` change the live rules and the file together.

## Intrusion detection (heuristics)

Two signals are produced on the syscall hot path, *after* the policy check so
they only ever observe calls that were allowed to run:

1. **Sensitive-syscall watch** — constant-time classification of each syscall
   against a small table of security-relevant operations (kernel module
   loading, `ptrace`, `bpf`, credential changes, namespace escapes, `kexec`,
   cross-process writes, …). Matches are recorded as audit events; they are
   never blocked here unless the optional privileged-deny latch is engaged. The
   table is selected per architecture (x86_64, and asm-generic for
   aarch64/riscv64).

2. **Rate anomalies** — cheap per-process sliding-window counters that flag
   syscall **floods** (possible DoS) and **fork bombs**, plus a system-wide
   fork-rate signal for *distributed* fork bombs. Each anomaly is reported at
   most once per window to avoid log storms; under the `anomaly` domain's
   `Enforce` mode the offending syscall is denied. Toggle with
   `heuristics::set_anomaly_detection`.

## Ransomware and process injection (`behavior`)

Signatures catch what somebody has already seen; the `behavior` domain
catches what an attack has to *do*. Code: `hunter/src/behavior.rs` (domain,
lineage, containment), `ransomware.rs`, `injection.rs`; the kernel side is
`linux-syscall/src/file/hunter_gate.rs` and `task/process_vm.rs`.

### Ransomware

The kernel tells hunter about every write, rename and unlink of a regular
file (pseudo-files under `/proc`, `/sys`, `/dev` and pipes are skipped). Each
process gets a 10-second window, and an alarm is raised when within it:

| Rule | Fires on |
|------|----------|
| encrypt and replace | ≥ 12 files written with random-looking contents **and** ≥ 12 of their originals deleted (`a.docx` gone, `a.docx.locked` or `a.locked` written, in either order) |
| encrypt in place    | ≥ 12 files that already had contents overwritten with random-looking bytes |
| mass renaming       | ≥ 12 renames in a row giving files one new extension, appended or replacing the old one (`a.docx` → `a.docx.locked`, `b.pdf` → `b.WNCRY`) |
| canary              | any write, rename or unlink of a decoy declared with `canary <path>` |

Each rule needs two signals because one is ordinary: unpacking an archive of
photos writes many random-looking files (compressed data looks random), and
`rm -rf` deletes thousands, but neither destroys what it read. The names
matter too: a package manager writes `x.png.dpkg-new`, renames it over `x.png`
and deletes `x.png.dpkg-tmp`, which links no copy to its original, so an
upgrade trips nothing. What does look exactly like ransomware is `gzip` over a
dozen files at once (`a.log` → `a.log.gz`, original deleted): that is the same
shape, and `exempt /usr/bin/gzip` is the answer if it is routine. Overwriting
in place is told from an image editor saving over its files by truncation: the
editor opens with `O_TRUNC`, so the file is already empty when it writes.
"Random-looking" is the number of distinct byte values in the first 512 bytes
of a write (≈221 for ciphertext, a few dozen for text, under 200 for machine code);
writes shorter than that are not judged.

Under `Enforce` the operation that crosses the line is refused with `EPERM`
and the process is **contained**: every later write, rename or unlink of a
file by it -- or by children it forks afterwards, or after an `execve` -- is
refused too, until it exits or `release <pid>`. Pseudo-files stay writable.

Canaries are the strongest signal and cost nothing: `hunterctl canary
/root/Documents/.~presupuesto.xlsx /home/u/Documentos/.~nominas.ods` creates
the files, declares them, and appends `canary` lines to
`/etc/hunter/policy.conf` so they are armed at every boot.

### Process injection

`process_vm_writev`, `ptrace(POKETEXT/POKEDATA/POKEUSER/SETREGS/SETFPREGS/
SETREGSET)` and `ptrace(ATTACH/SEIZE)` are judged by who the target is. A
process writing into itself or into one of its descendants is what a debugger
does (`gdb ./prog`, `strace -f make`) and passes; writing into anything else
(a sibling, a parent, a stranger) raises an alarm, once per target, and under
`Enforce` is refused with `EPERM`. hunter knows the lineage from `task_fork`.
`exempt /usr/bin/gdb` lets `gdb -p <pid>` attach to processes it did not
start. Eclipse does not implement `ptrace` or `/proc/<pid>/mem` yet: a
`ptrace` attempt is still judged from its syscall arguments, and
`hunter::check_process_write` is the hook a `/proc/<pid>/mem` writer would
call.

`/proc/hunter` shows the mode in the `enforcement:` line (`behavior=`) and a
`behavior:` line with canaries, exempt programs, contained processes and the
number of alarms. Events are logged under `RANSOMWARE` and `INJECTION`.

## `/proc/hunter`

Reading `/proc/hunter` renders a live report: a status header (per-domain modes,
event counters, active syscall policies) followed by the recent event ring.
Example:

```
hunter security subsystem v0.4.0
enforcement: syscall=enforce wx=report exec=report anomaly=report
events: total=3 blocked=0 warnings=1 reported=1 critical=0 dropped=0 critical_dropped=0
active syscall policies: 0
heuristics: anomaly-detection=off privileged-deny=off
trusted-program allowlist: inactive (0 entries)

recent events (oldest first):
[     0] +         0.000s pid=0     INFO   SYSTEM    INIT: hunter security subsystem v0.3.0 initialized (...)
[     1] +         4.512s pid=142   NOTICE PRIVILEGE WATCH: sensitive syscall #101 (ptrace)
[     2] +         5.001s pid=142   WARN   ANOMALY   WARNING: possible fork bomb: >200 clone/fork within 1000ms
```

## Programmatic configuration

```rust
use hunter::{policy, Mode};

// Restrict a process to a syscall whitelist (a lightweight seccomp).
policy::register_policy(pid, vec![/* allowed syscall numbers */]);

// Tighten W^X to active blocking.
policy::set_wx_mode(Mode::Enforce);

// Treat execution from world-writable paths as fatal.
policy::set_exec_mode(Mode::Enforce);
policy::add_untrusted_exec_prefix(String::from("/run/user/"));
```
