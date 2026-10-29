# abiprobe — the Linux ABI an Electron app leans on before it prints anything

A single static binary that makes, one at a time, the calls a Chromium/Electron
main process makes between `main()` and its first log line, and says PASS or
FAIL with the errno for each. It is green on a real Linux kernel, so a FAIL
here is a divergence of ours and nothing else.

It exists because `claude-desktop --no-sandbox` dies on Eclipse with a bare
`Segmentation fault` and no other output: an Electron is a very good stress
test of this ABI, but a terrible one to debug, because it reports nothing. The
probe is the same surface with the reporting put back.

What it covers: the address-space reservations V8 opens with (1 TiB, 32 GiB and
4 GiB, each aligned to its own size by over-reserving and trimming, then
`mprotect`ed and touched), a `PROT_NONE` region grown in the middle, `MAP_FIXED`
over a live mapping, a file mapped at its exact byte length and read past it,
a multi-megabyte `MAP_SHARED` resource bundle read end to end, the sandbox's
probes (`PR_SET_NO_NEW_PRIVS`, `seccomp`, `/proc/self/task`), `membarrier`,
affinity and threads, `fork` after threads, `getrandom`, Chromium's shared
memory (`memfd_create` + seals + a read-only remap), `MAP_SHARED|MAP_ANONYMOUS`
across `fork`, the zygote's `socketpair` + `SCM_RIGHTS` + `SO_PASSCRED`, an
edge-triggered `eventfd`/`epoll` wakeup, the five clocks `base::TimeTicks` is
built on, `/proc/self/exe`, and a `SIGSEGV` handler on an alternate stack.

## Running it

Build it against glibc, statically, on the host:

    gcc -O1 -static -o tools/abiprobe/abiprobe tools/abiprobe/abiprobe.c -lpthread
    ./tools/abiprobe/abiprobe          # must be all PASS on Linux first

Then put it in the guest's rootfs and repack. It is deliberately NOT wired into
the image build: it is a diagnostic to reach for, not something every image
should carry.

    cp tools/abiprobe/abiprobe rootfs/x86_64-minimal/bin/
    cargo xtask mk-sfs --image zCore/x86_64-minimal.img \
        --dir rootfs/x86_64-minimal --size 419430400
    make -C zCore build MODE=release LINUX=1 GRAPHIC= LOG=warn VARIANT=minimal
    make -C zCore justrun MODE=release LINUX=1 GRAPHIC= LOG=warn VARIANT=minimal

`make build` after `mk-sfs` is not optional: rboot reads the initramfs from the
ESP, so an image that is not copied there is an image that is not booted.

Time it (`time abiprobe`): a check that passes slowly is its own finding. The
1 TiB reservation passed in 94 s before `GenericPageTable::unmapped_span`
existed, and in 0.92 s after.
