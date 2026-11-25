# sysfuzz: random Linux syscalls against the kernel, in QEMU

`sysfuzz.c` is a static test program. It forks one child per *program*: 48
calls (`-c`) drawn from a table of 132 syscalls, with arguments built from a
seeded generator. The arguments are fds of every kind the kernel serves
(files, pipes, unix/inet/netlink sockets, eventfd, epoll, memfd, timerfd,
inotify), pointers that are NULL, kernel-half, non-canonical, read-only,
misaligned or one byte before a `PROT_NONE` page, and sizes and flags at
their edges. Each child runs with rlimits, `alarm(4)` and its own process
group, so a program can hang only itself.

It prints one line per program, `FZ-PROG n SEED`. `-r SEED -v` replays that
one program and prints every call (`FZ-CALL`) and its answer (`FZ-RET`). A
child still alive 8 s after it started is `FZ-HANG SEED`: `alarm(4)` should
have killed it, so a hang is a call that a signal could not interrupt.

`scripts/qemu-sysfuzz.py` boots the minimal x86_64 image, runs sysfuzz in
rounds, and watches the console for what a broken kernel prints. When it
sees a crash, it reboots, replays the program that was running, and writes
the replay's console to `--out/SEED.txt`:

    gcc -static -O2 -o /tmp/srv/sysfuzz tools/sysfuzz/sysfuzz.c
    make -C zCore build ARCH=x86_64 LINUX=1 MODE=release VARIANT=minimal
    make image ARCH=x86_64 VARIANT=minimal
    scripts/qemu-sysfuzz.py --srv /tmp/srv -m 2G -s 4 --minutes 60

What the first runs found: #2832 (a `shmat` address that wrapped `test_map`),
#2833 (a robust futex list with a garbage offset) and #2834 (an eventfd
`write` that spun in the kernel, deaf to signals).

## Kani proofs of the on-disk and wire decoders

`vendor/nfs3-rs`, `vendor/btrfs-rs` and `vendor/eclipsefs-rs` have
`src/kani_proofs.rs`. They prove, for every input up to a size, that the
decoders never panic and never read past their buffer:

    cargo +stable install --locked kani-verifier && cargo +stable kani setup
    cd vendor/nfs3-rs && cargo +stable kani --harness xdr_decoder_never_reads_past_the_buffer

Run one harness at a time: CBMC takes several GB, and "CBMC failed" on a big
harness is usually the OOM killer, not a counterexample. Kani cannot build
the kernel crates: the pinned `x86_64` crate does not build on its
toolchain. These three crates are the parsers that take untrusted bytes
(network replies, disk images).
