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
| `task_exit`                | `sys_exit_group`                         | state cleanup   |
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
| `integrity` | SHA-256 of the image vs. its pinned digest      | `Report`  |
| `av`      | Antivirus signatures matched on the image          | `Report`  |
| `net`     | Which programs may reach the network ([firewall](#per-application-firewall)) | `Report`  |

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
filesystem; `hunterctl save-learned` reads that block and appends new
programs to `/etc/hunter/whitelist`, so the learned set survives a reboot.

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
| `mode <domain> <off\|report\|enforce>` | `domain` is `syscall`, `wx`, `exec` or `anomaly`    |
| `learning <on\|off>`                   | exec trust-on-first-use                             |
| `anomaly-detection <on\|off>`          | per-process flood / fork-bomb counters              |
| `privileged-deny <on\|off>`            | block module/ptrace/bpf/kexec under `anomaly=enforce` |
| `trust <path>` / `trust <dir/>`        | add to the exec allowlist                           |
| `deny <path>` / `deny <dir/>`          | add to the exec blacklist                           |
| `untrusted <dir/>`                     | add a world-writable location                       |
| `hash <sha256> <path>`                 | pin a program's SHA-256 (see below)                 |
| `require-hash <on\|off>`               | an unpinned program is an integrity violation       |
| `signature <hash\|bytes>:<data>:<Name>` | add an antivirus signature (never removable)       |
| `net <allow\|deny> <path> [in\|out]`  | a [firewall](#per-application-firewall) rule; no direction is both |
| `net forget <path>` / `net dismiss <path>` | drop a program's rules / its open questions     |
| `net timeout <secs>` / `net mode <mode>` | how long `enforce` waits for an answer (1-300 s); same as `mode net` |
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
`privileged-deny off`); tightening ones still apply. Only a reboot unseals.

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
hunterctl seal
hunterctl apply <file>          send a whole script (e.g. a new policy.conf)
hunterctl hash <file>...        pin SHA-256 digests (live, and in /etc/hunter/hashes)
hunterctl require-hash <on|off>
hunterctl verify                sha256sum -c /etc/hunter/hashes
hunterctl scan [--quarantine] <path>...   antivirus scan (exit 1 if infected)
hunterctl quarantine <file>...  move into /var/lib/hunter/quarantine, deny the path
hunterctl quarantined           list the quarantine
hunterctl restore <sha256>      put a quarantined file back
hunterctl update-signatures     download the feeds in /etc/hunter/feeds
hunterctl lockdown              exec + integrity + av enforce, learning off, seal
hunterctl save-learned          append learned programs to /etc/hunter/whitelist
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
| Resumen | the stance in one line, eight counters, the latest alerts; quick actions: scan home, update signatures, EICAR test, lockdown |
| Protección | the six domains with what each guards, the four switches (`learning`, `anomaly-detection`, `privileged-deny`, `require-hash`); **save as boot policy** writes the live stance to `/etc/hunter/policy.conf` (old file kept as `policy.conf.bak`), **apply policy.conf**, and **seal** |
| Eventos | the whole event ring with time, pid, severity and category, filtered by all / alerts / critical |
| Escaneo | presets (home, `/tmp`, programs, `/`, or `--scan PATH...`), a live count with a stop button, per-finding or all-at-once quarantine, or automatic quarantine as it goes |
| Cuarentena | `/var/lib/hunter/quarantine/index`, each row restorable (two clicks) |
| Firmas | what the kernel holds (hash / byte patterns / detections), the files under `/etc/hunter/signatures{,.d}`, the feeds, **update now**, and the **EICAR test** (writes the test file, scans it, deletes it, says whether the engine caught it) |
| Programas | the allowlist counts, kernel vs. disk pins, the learned programs, **save learned** (`hunterctl save-learned`) and **verify integrity** (`hunterctl verify`) |
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

**Database and updates.** `/etc/hunter/signatures` (local; ships with the
EICAR test signature) and every file in `/etc/hunter/signatures.d/` are
loaded at boot. `hunterctl update-signatures` downloads each feed in
`/etc/hunter/feeds` (`<format> <name> <url>`; formats `sha256` and `sigs`)
with curl or wget. It loads the new signatures into the running kernel and,
only if hunter accepted all of them, installs the feed as
`signatures.d/<name>` (written to a dot-file, then renamed). An empty or
failed download keeps the previous feed. The default feed is abuse.ch
MalwareBazaar's list of malware SHA-256s from the last 48 hours. Schedule it
with crond, e.g. `0 */6 * * * /sbin/hunterctl update-signatures`. Adding a
signature only ever makes hunter stricter, so the tighten-only latch does not
block updates.

**Limits.** Hashes cover the whole file. Byte patterns are searched in the
first 128 MiB of an image, and each pattern is a separate anchored pass, which
fits the hundreds of patterns an operator curates rather than a commercial
database. Shared libraries loaded later with `mmap(PROT_EXEC)` are not scanned
yet; their bytes are only scanned on demand.

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
