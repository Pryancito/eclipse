# nouveau-compatible uAPI on the NVIDIA GPU — status

[Español](README-nouveau-uapi.md) · **English**

This document maps, ioctl by ioctl, the surface of `nouveau.ko`
(`include/uapi/drm/nouveau_drm.h` in the Linux kernel) against what
`NvidiaGpu::ioctl` implements in Eclipse OS. It is the same exercise that
[`README-drm.en.md`](README-drm.en.md) already does for the generic DRM/KMS
UAPI, applied to nouveau's *driver-specific* surface.

## Why this exists

Mesa's accelerated OpenGL/Vulkan for NVIDIA (`nouveau_dri.so`, NVK) is
ordinary Linux userspace, already built by Ubuntu (the apt packages), and it
already runs unmodified under Eclipse's syscall layer — just like Xorg, labwc
or busybox. What it is missing is a **kernel side** that speaks the same ioctl
protocol as `nouveau.ko`: a public, stable contract, unlike the private and
undocumented protocol between `nvidia.ko` and NVIDIA's closed driver. See the
full discussion in the thread that started this work.

## Scope of this milestone — read before extending

This was written in a sandbox **with no NVIDIA GPU, no `/dev/kvm` and no QEMU
installed**, and the sentence that used to be here — "nothing below has been
run against real hardware" — no longer holds: since then Moebius has exercised
this uAPI on his RTX 2060 SUPERs (NVK rendering, glxgears). What is still true
is that **every table row states its own status**, and that what is here is
written without hardware in front of it, so every operation falls into one of
these three categories:

- **(a) It reuses an entry point already exercised on real hardware, as
  is** — `nvidia_rm_sys::rm_init::step16`/`step17`, the same calls that
  `/proc/gpustep16`/`gpustep17` make.
- **(b) It is pure bookkeeping, with no hardware/register access** — the GEM
  handle table, the VRAM bitmap allocator (`NvidiaVramAllocator`, which
  already existed as dead code — it was never called).
- **(c) It is explicitly rejected with `EOPNOTSUPP`/`ENOSYS` and a log line —
  a false success is never faked.**

**How to enable it**: strictly opt-in, like `drm.atomic`. Boot with
`nvidia.nouveau_uapi` in the `cmdline` (`zCore/rboot.conf`):

```ini
cmdline=LOG=warn:nvidia.nouveau_uapi:SHELL=/bin/busybox?sh
```

Without the flag, `NvidiaGpu::ioctl` behaves exactly as before (`ENOSYS` for
any unrecognised ioctl) — zero change in default behaviour.

## ioctl coverage

Legend: ✅ implemented (real, no new untested hardware) · 🟡 partial ·
❌ explicitly rejected (`EOPNOTSUPP`/`ENOSYS`, never faked).

| ioctl | Status | Notes |
|---|---|---|
| `DRM_IOCTL_NOUVEAU_GETPARAM` | ✅ | `PCI_VENDOR`/`PCI_DEVICE`/`FB_SIZE` are real; `VRAM_BAR_SIZE` is the BAR1 aperture (not the VRAM: NVK compares both to decide its host-visible heap); `CHIPSET_ID` is the **real chip id** from `NV_PMC_BOOT_0` (not the architecture minimum: 0x170 decoded to GA100/SM80 instead of GA10x/SM86); `PTIMER_TIME` (=14, not 10 — see the note in `nouveau_uapi.rs`) returns a monotonic ns counter derived from the CPU (an honest stand-in for `GL_TIMESTAMP`, without reading BAR0); `EXEC_PUSH_MAX` = 64 (this driver's real `EXEC` cap); `GRAPH_UNITS` is **real** (`gpc \| tpc<<8`, with the GSP-RM topology if the RM is bound; it is fatal for Mesa's enumeration, it cannot be EINVAL); `HAS_BO_USAGE` and `HAS_PAGEFLIP` are 1, as in Linux since they have existed (previously 0: libdrm filled `PUSHBUF`'s buffer list as on a pre-2011 kernel), and `VM_VRAM_BASE` (deprecated) answers 0, not `EINVAL`; `VRAM_USED` is the sum of the live VRAM-domain objects (like Linux's VRAM manager, across all clients); `AGP_SIZE` is `drm->gem.gart_available`, which on a PCIe card (no AGP bridge) is the client VMM's limit: 49 bits on GP100+ (`nvkm_vmm_ctor`: the 47-bit page plus the 2 bits of its directory), 512 TiB, the same number `GEM_PUSHBUF` returns in `gart_available` (previously 0, which libdrm read as a card without GART); neither `FB_SIZE` nor `AGP_SIZE` subtracts what the kernel keeps pinned (its channels' pushbuffers), which this driver does not count |
| **DRM `VERSION` name = `"nouveau"` (v1.4.0)** | ✅ | With `nvidia.nouveau_uapi` active, `DRM_IOCTL_VERSION` reports the driver name **`nouveau`** and version **1.4.0** (`linux-object/src/fs/devfs/drm_scheme.rs`), instead of `"zcore"`. **Critical**: Mesa picks the DRI driver (`nouveau_dri.so`) by that name — with `"zcore"` nothing loaded. Version 1.4.0 is the one that, in real nouveau, enables the NEW submission uAPI (`VM_INIT`/`VM_BIND`/`EXEC`), which is exactly what this driver implements (not the legacy `GEM_PUSHBUF`). Without the flag it is still `"zcore"` (QEMU's virtio-gpu path is untouched) |
| `DRM_IOCTL_NOUVEAU_SETPARAM` | ✅ | deprecated even in Linux, where it is `drm_invalid_op` under `DRM_AUTH \| DRM_MASTER \| DRM_ROOT_ONLY`: EACCES for whoever is not root, not authenticated or (with `drm.master_check`) not the master, and EINVAL if the permission check passes; previously it went to the driver and returned ENOSYS |
| `DRM_IOCTL_NOUVEAU_GROBJ_ALLOC` | 🟡 | The `nouveau_object_new` of a libdrm without NVIF (deprecated in the header, still in nouveau's table). Like `nouveau_abi16_ioctl_grobj_alloc`: handle `~0` → EINVAL before anything else; a channel that is not the caller's → ENOENT; the old `0x*6e` (software) and `0x*b1/b2/b3` (video) identifiers are mapped to the channel class with that low byte, and on Turing there is none → EINVAL; class 0 → EINVAL; the rest creates the same engine object as an NVIF NEW on the channel route (a real RM object on an RM-backed channel, bookkeeping on a discovery one). Previously ENOSYS |
| `DRM_IOCTL_NOUVEAU_NOTIFIEROBJ_ALLOC` | ❌ | EINVAL without looking at the channel, like `nouveau_abi16_ioctl_notifierobj_alloc` on Fermi and later ("completely unnecessary for these chipsets"). Previously ENOSYS |
| `DRM_IOCTL_NOUVEAU_GPUOBJ_FREE` | 🟡 | Like `nouveau_abi16_ioctl_gpuobj_free`: a channel that is not the caller's → EINVAL; a handle that is not an abi16 object of that channel → ENOENT (NVIF NEW objects go in another list, and an NVIF DEL does not reach a GROBJ_ALLOC one); frees the RM object if there is one. It does not wait for the channel to go idle (`nouveau_channel_idle`): without GEM_PUSHBUF, a client without NVIF cannot have queued anything. Previously ENOSYS |
| `DRM_IOCTL_NOUVEAU_CHANNEL_ALLOC` | 🟡 | The first RM-backed process (the compositor) reuses the `step16`+`step17` ladder (context 0); every **later client process** gets its OWN context (`ensure_ctx_for_pid` → `ctx_alloc`, see "Per-process GPU context"), and only if that fails does it fall back to a **discovery** channel (enumeration OK, `EXEC` ENODEV → software). RM backing is **per process**, not per channel: two channels of the same pid are two names for the same hardware (NVK creates a throwaway context just to read classes). The channel table is released during the `ensure`/prime (~500 ms of RM) so as not to block the compositor's `EXEC`. As in `nouveau_abi16_ioctl_channel_alloc` (Kepler+): with `fb_ctxdma_handle == ~0`, `tt_ctxdma_handle` names the engine (`NOUVEAU_FIFO_ENGINE_GR` or `_CE`, which is what NVK and libdrm's nve0 channels send; `_VP`/`_PPP`/`_BSP` have no runlist on Turing → `EINVAL`; any other value → `ENOSYS`) and both fields come back as 0. **The name is read and validated, it does not pick a ladder**: here every channel, whether requested as GR or as CE, is the same RM channel on the graphics runlist (`step17`/`ctx_alloc`, `TURING_COMPUTE_A`); a channel requested as CE copies with that runlist's GRCEs, not with a copy runlist of its own, which Linux would give it (`NV_DEVICE_HOST_RUNLIST_ENGINES_CE0`); otherwise they are legacy handles and only `tt == ~0` is rejected (`EINVAL`). `subchan[]` is ignored: it does not apply to Turing+. `pushbuf_domains` comes back as `VRAM \| GART`, as on Tesla and later (previously VRAM only) |
| `DRM_IOCTL_NOUVEAU_CHANNEL_FREE` | 🟡 | Bookkeeping: deletes the channel entry (an id that is not the process's, was never allocated or is already freed: `ENOENT`, like `nouveau_abi16_chan`). **It does NOT touch the VM** — in the nouveau uAPI, `VM_BIND` mappings belong to the DRM FILE, not to any channel (a drain here, inherited from the single-channel model, deleted the live binds of the first `vkCreateDevice` when the second destroyed its temporary context → MMU fault → `RING FULL`). Mappings are reclaimed where they should be: `GEM_CLOSE` (per handle) and process exit. Client contexts (`ctx>=1`) are freed with `ctx_free` when their process exits. The reset of the **ctx 0** singleton (`ctx0_reset`) fires on process exit of the compositor's RM-backed owner, not on `CHANNEL_FREE` (this avoids tearing down ctx 0 while the same process still uses it through another live channel) |
| `DRM_IOCTL_NOUVEAU_NVIF` | 🟡 | What NVK uses is implemented: `NEW` (NV_DEVICE and engine-class objects — on an RM-backed channel it allocates the REAL class object via `class_alloc`, which is what builds the engine context; on discovery channels, bookkeeping), `MTHD(NV_DEVICE_V0_INFO)` (real chipset/VRAM), `SCLASS` (classes per architecture) and `DEL` (frees the RM object, scoped per pid — the token is a userspace pointer and can collide between processes). It is read like `nouveau_abi16_ioctl`: a header with `version` 0 and `route` 0x00 (client objects) or 0xff (a channel's, by `token`), unknown type `EINVAL`; `NEW` with a version-0 body, on route 0 only the device under the client (`object` 0, class NV_DEVICE) and on route 0xff an engine object on the channel; `MTHD` on route 0 on a device object of the caller's, only `NV_DEVICE_V0_INFO`, with a body of exactly the reply's size and a version-0 template; `DEL` on route 0 and without a body (an object nobody created is not an error). `SCLASS` with a body of exactly `count` entries (otherwise `EINVAL`), and `count` comes back as the TOTAL number of classes on the channel whether they fit or not: that is the loop of `nvif_object_sclass_get` and of `nouveau_object_sclass_get` (libdrm), which first ask with 0 to learn how many to offer. It multiplexes five sizes over nr 0x47: it only works with dispatch by NR |
| `DRM_IOCTL_NOUVEAU_SVM_INIT` / `SVM_BIND` | ❌ | unified CPU/GPU memory — out of scope for this milestone |
| `DRM_IOCTL_NOUVEAU_VM_INIT` | 🟡 | **Standalone** (NVK issues it BEFORE `CHANNEL_ALLOC`, while creating the physical device; requiring a channel here cost the whole GPU). It checks that the `kernel_managed` range fits in the VA space (overflowing or going past `1 << 47` → `EINVAL`, like `nouveau_uvmm_ioctl_vm_init`) and records that this pid now has uvmm: **without it, `VM_BIND` and `EXEC` answer `ENOSYS`** before looking at anything else (as in Linux). Repeating it is idempotent. Linux's rule that an earlier legacy ioctl (`GEM_NEW`, `CHANNEL_ALLOC`) disables uvmm forever is NOT applied: here the state is per pid, not per file, and a process with a legacy GL fd and an NVK one would lose the second. The real VA reservation happens in `VM_BIND` against the RM. It is `_IOW` in practice (Mesa issues it with `drmCommandWrite`) even though the header says `_IOWR` — the original reason for dispatch by NR |
| `DRM_IOCTL_NOUVEAU_VM_BIND` | 🟡 | **Real**: `eclipse_rm_vm_bind_map`/`unmap` (new in `eclipse_rm_init.c`) generalise the `step17` pattern (reserve VA in `hVas` + `Map`) for a GEM handle and an address chosen by the caller. **Real `op_count`, from 1 to 64** per call (`op_ptr` as an array of `DrmNouveauVmBindOp`; see "Known gaps" on non-atomicity across ops) — more than 4096: `EOPNOTSUPP`; 0 ops without syncs is an empty job that answers 0 (`nouveau_uvmm_bind_job_init` accepts the empty list), and a count without an array (`op_ptr`, `wait_ptr` or `sig_ptr` null) is `EFAULT`, like `u_memcpya`. A synchronous bind (without `RUN_ASYNC`) carries no syncs (`EINVAL`, the `nouveau_job_init` rule when building the job); the asynchronous one NVK sends from `vkQueueBindSparse` waits for its waits on the CPU, applies the ops and signals its sigs. That rule comes AFTER copying the three lists, just as `nouveau_uvmm_vm_bind_ucopy` runs before `nouveau_uvmm_vm_bind`: a count without an array is `EFAULT` whether the bind is synchronous or not (previously the rule came first, and a synchronous bind naming a list without an array was `EINVAL`). **PTE kind**: on Turing+ the kind only drives L2 compression (the block-linear swizzle is done by the ENGINE via block-height methods, not the page table), and this driver downgrades the compressed kind unless `nvidia.gem_comptags` is on (**never for lack of an allocator: on Turing there is none to be missing**, the line comes from the physical address, `memmgrDetermineComptag_TU102` = `1 + physAddr[35:16]`). Without that hatch — the default case — a compressed kind that arrives **here** (VM_BIND) is **mapped uncompressed** (`0x06`): correct bytes for an uncompressed surface, and — crucially — **it is never rejected here**, because doing so left the surface UNMAPPED and hung the channel on NVK's first draw using it (`exec_submit_signaled TIMEOUT`, `GPGet` frozen, no MMU fault — seen on RTX with kind `0x01`). Early rejection of compressed kinds is done in `GEM_NEW` (see its row), where NVK can still react at allocation time; VM_BIND's map-uncompressed is the anti-hang fallback for those that only arrive here. If a surface really depended on compression it will look corrupt (worst case), but the channel keeps going and the frame completes. **With `nvidia.gem_comptags`** the kind passes as is and the RM derives the line itself (`kgmmuFieldSetKindCompTags` via `bPhysBasedComptags`): the glue's downgrade stopped being unconditional and is now a parameter, `bHonourCompressedKind`, decided by `drivers/src/display/nvidia/comptags.rs` — in Rust and under test, because a wrong kind returns no error, it gives the engine the compression state of another surface. It passes only if ALL of this holds: the object is in VRAM, its page reaches 64 KiB (the `>> 16` of the derivation is exactly why), scrub-on-free is active, the compbits backing store is not off, the GPU's `comprPageShift` is 16, and the mode is raw or one-to-one with `memmgrUseVasForCeMemoryOps`. **There is a THIRD mode**, `bUseOneToFourComptagLineAllocation`, in which the derivation does NOT hold: it is rejected by name, because "it is not one-to-one, so it is raw" is exactly the inference that would put someone else's line in a PTE. The caps are read by `eclipse_rm_comptag_caps` once per boot, and if the read fails (which happens without a GPU) compression stays off. Off by default because none of this has been seen running against hardware. **Each op's window** (`drm_gpuvm_range_valid`): MAP and UNMAP must fall in `[0, 1<<47)` and outside the `kernel_managed` range the client reserved in `VM_INIT` (an empty reservation blocks nothing); otherwise `EINVAL`, and the whole batch is rejected without touching the RM. The sync lists are checked before anything else, in `nouveau_job_submit`'s order: the waits (unknown type → `EOPNOTSUPP`, foreign or non-existent handle → `ENOENT`) and then the sigs (unknown type → `EINVAL`, handle → `ENOENT`); only the type nibble of `flags` counts. A wait on a point nobody submitted (binary syncobj without a fence, timeline point past its chain) is `EINVAL` when building the job, before the sigs, the ops and waiting for anything (`nouveau_job_add_deps` → `drm_syncobj_find_fence` with flags 0); an in-flight fence that never lands is still the bounded wait and `EIO`. A MAP with handle 0 and without SPARSE names no object: `ENOENT` (`drm_gem_object_lookup`) and the batch does nothing; here it used to be taken as an unmap of the range. The object is looked up **before** measuring the window and the range, as in `nouveau_uvmm_bind_job_submit` (the `lookup` precedes `bind_validate_op`): a MAP without an object that is off-page, outside the VA space or inside the `VM_INIT` reservation is `ENOENT`, not `EINVAL` (previously the range was measured first). An unknown op is `EINVAL` on a second pass, once all ops have been looked up and measured (Linux's second loop): behind it a MAP without an object is still `ENOENT`, and ahead of it a good MAP is not applied |
| `DRM_IOCTL_NOUVEAU_EXEC` | 🟡 | **Real**: `eclipse_rm_exec_submit` generalises the `step18` mechanics (GP entry + `GPPut` + doorbell) for a *pushbuffer* `(va, len)` the caller already wrote. **Real `push_count`, from 0 to 64** (more: `EINVAL`, like `nouveau_exec_ioctl_exec` over its cap; a count without an array in pushes, waits or sigs: `EFAULT`, like `u_memcpya`; more than 256 syncs: `EOPNOTSUPP`, this driver's cap). In `nouveau_exec_ioctl_exec`'s order: first the channel (`ENOENT` if it is not in the caller's list, also for a client with no channel at all; `ENODEV` if it is a discovery one, without GPFIFO: Linux's "killed" channel), then the push cap (`EINVAL`) and only after that the arrays and the sync caps (previously the arrays came first, and a foreign channel or 65 pushes with a null array were `EFAULT`); with the arrays copied, every push is measured (empty, not a multiple of 4 or longer than a GPFIFO entry can name: `EINVAL`) BEFORE looking up the syncs, just as `nouveau_exec_job_init` measures them when building the job and only `nouveau_job_submit` looks up the syncs when submitting it: a bad push beats a foreign sig (`ENOENT`) and a wait of unknown type (`EOPNOTSUPP`), and a count without an array (`EFAULT`) beats both (previously pushes were measured after the syncs, and a bad push behind a foreign sig was `ENOENT`) — each *pushbuffer* is submitted in order; if `sig_count > 0` the kernel fence is attached only to the last one (GPFIFO is strictly ordered, so a signal there proves ALL the previous ones were fetched too). `sig_count == 0` (fire-and-forget) or **real `sig_count`, from 0 to 64** DRM syncobjs (see the syncobjs section) — `eclipse_rm_exec_submit_signaled` adds a second GP entry with a kernel-owned semaphore after the last *push* and only marks the syncobjs (all of them, not atomically — see "Known gaps") after confirming it landed. **Real `wait_count`, from 0 to 64**, but by a **CPU wait before submitting** (`crate::scheme::syncobj::wait` with `wait_all=true`, a fixed 1 s timeout for the whole array), NOT by a semaphore `ACQUIRE` executed by the hardware channel itself — see the note in "Known gaps". The sync lists are checked as in `VM_BIND` (waits: unknown type `EOPNOTSUPP`, then `ENOENT`; sigs: `EINVAL`, then `ENOENT`), before touching the ring. A wait on a point nobody submitted (binary syncobj without a fence, timeline point past its chain) is `EINVAL` when building the job, before the sigs, the ops and waiting for anything (`nouveau_job_add_deps` → `drm_syncobj_find_fence` with flags 0); an in-flight fence that never lands is still the bounded wait and `EIO` |
| `DRM_IOCTL_NOUVEAU_GET_ZCULL_INFO` | ❌ | An honest `ENOSYS` — Mesa 26.x probes it and TOLERATES the failure (`has_zcull_info` stays false) |
| `DRM_IOCTL_NOUVEAU_GEM_NEW` | 🟡 | Accepts VRAM and GART, but **GART (alone or with VRAM) is backed with CPU-visible sysmem** (`NV01_MEMORY_SYSTEM`, `_LOCATION_PCI`, contiguous — the `step17` pushbuffer pattern): a **VRAM-only** request does get real `NV01_MEMORY_LOCAL_USER`, but it is not CPU-mappable: BAR1 is virtual and `bar1_phys + offset` does not point at that VRAM. Only a `VRAM | GART` buffer with `nvidia.gem_vram` gets a CPU mapping, the one the RM makes in BAR1 (see `drivers/src/display/nvidia/gem_vram.rs`). `req.info.domain` returns the domain actually used (GART), which is what Mesa reads back; NVIF INFO still advertises the real VRAM only so that NVK exposes a `DEVICE_LOCAL` type (wlroots requires it). **Tile kind**: the kind byte of `tile_flags` must be in Turing's MMU kind table (`tu102_mmu_kind`: `0x00..=0x06` and the compressible `0x08..=0x0e`; `0x07` and `0x0f` are its invalid entries and from `0x10` up there is no table), or `GEM_NEW` answers `EINVAL` before touching the RM, like `nouveau_bo_alloc` via `nvif_mmu_kind_valid` (previously it accepted any byte). A compressible kind is accepted, and for a client without uvmm it makes the object a compressed one (`nvbo->comp`): in VRAM it goes to a 64 KiB page at least (the smallest with compression, even if the request is 100 bytes) and keeps the kind; in sysmem (4 KiB pages, no compression) it is stored and reported as its uncompressed counterpart, like `nouveau_bo_alloc` when the page has no compression. A client with uvmm (`VM_INIT`) does not go through that: its object goes to the page the request fills and the kind comes back as is, because the kind of its mappings is set by each `VM_BIND` op (previously everything under 64 KiB went to 4 KiB pages with the kind downgraded, with or without uvmm). `VM_BIND` downgrades the kind unless `nvidia.gem_comptags` is on (see its row). The object's size (allocated, counted and reported) is the request rounded up to its GPU page: see the `GEM_INFO` row. **Real `map_handle`**: `gem_map_cpu` resolves the host PA (`ADDR_SYSMEM`; rejects non-contiguous sysmem) and `(phys_addr, size)` is registered in `drivers/src/scheme/gem_mmap.rs` under the nouveau handle (high range `0x8000_0001+`, no collision with the generic table). If it fails, `map_handle`=0 and the object is still valid for `VM_BIND`/`EXEC`. `NOUVEAU_GEM_DOMAIN_NO_SHARE` requires uvmm (a prior `VM_INIT`; otherwise `EINVAL` before looking at the domain) and leaves the object unexportable via PRIME (`EPERM`, like `nouveau_gem_prime_export`), whether or not it has a host address (the registration goes by handle and owner in `gem_mmap`, apart from the mapping table: a VRAM-only object has no entry there and is just as private); `GEM_INFO` does not return that bit. |
| `DRM_IOCTL_NOUVEAU_GEM_PUSHBUF` | 🟡 | **Course correction (important)**: an investigation of the image showed that the Mesa being installed is **only the classic Gallium stack** (`mesa-dri-gallium` + `mesa-gl`, in the Alpine image of the time); for Turing that is the **nvc0** driver, which submits OpenGL through **this** ioctl (`GEM_PUSHBUF`), NOT through the new `VM_BIND`/`EXEC` uAPI (that one is used by NVK/Vulkan). **That is no longer the case**: the image installs `libvulkan1` and `mesa-vulkan-drivers` (NVK and lavapipe) as well as `libgl1-mesa-dri` — see `DESKTOP_UBUNTU` in `xtask/src/linux/apt_rootfs.rs` —, and since Mesa 25.1 the default OpenGL path on NVIDIA is **zink on NVK**, not nvc0. So the image's hardware GL arrives through `VM_BIND`/`EXEC`, which are implemented, and this ioctl is the classic stack's, which remains follow-up work. Before anything else it does `nouveau_gem_ioctl_pushbuf`'s checks in their order: a client with uvmm (`VM_INIT`) is `ENOSYS` (before looking at the channel); the channel must be the caller's (`ENOENT`, like `CHANNEL_FREE`); `vram_available`/`gart_available` come back written with what `GETPARAM` `FB_SIZE` and `AGP_SIZE` give (`drm->gem.vram_available`/`gart_available`); a request with `nr_push == 0` is a success with `suffix0`/`suffix1` at 0 (an IB channel, all of Kepler+); more than 512 pushes, 1024 buffers or 1024 relocs is `EINVAL` before reading the arrays; a null array with a count is `EFAULT`; and a push whose `bo_index` is not in the list is `EINVAL` (with the suffixes already written); then `nouveau_gem_pushbuf_validate` walks the buffer list, with or without the hatch and before mapping anything: `validate_init` in order and each entry in full before the next (a handle that is not a GEM object of the caller's, counting a PRIME import it holds, `ENOENT`; repeated `EINVAL`, "multiple instances of buffer"; `valid_domains` with neither VRAM nor GART `EINVAL`, "invalid valid domains") and then `validate_list`/`nouveau_gem_set_domain` (the entry's `valid_domains`, the object's — what `GEM_NEW` asked for, VRAM\|GART at most — and its `write_domains`, or the `read_domains` if it does not write, must have something in common, or `EINVAL`, "fail set_domain"; the driver does not move objects, so it does nothing more with it), all with the suffixes written. Previously it answered `EOPNOTSUPP` to all of that, including a request with nothing to send, and the buffer list was not read. Whatever passes those checks is **parsed and dumped** (channel, `nr_buffers`/`nr_relocs`/`nr_push`, each BO's domains, each push's `(bo_index, offset, length)` — prefix capped at 8 per array), and **with `nvidia.gem_pushbuf` on it really submits**; without the hatch — the default case — it returns the usual honest **`EOPNOTSUPP`**. The real submission does, in `nouveau_gem_ioctl_pushbuf`'s order: map every BO in the list into the channel's VAS (the `eclipse_rm_vm_bind_map` mechanism, driven from here instead of from `VM_BIND`, with `requested_va`=0 and a generic kind; the VA is recorded in `nouveau_vm_mappings`, so teardown on process exit and context retirement already free it, and an earlier whole allocation of the same handle is reused instead of asking for a new virtual range per buffer and per frame); return `presumed` written with that VA and its domain, which is half the uAPI this path exists for (Mesa caches the VA it is told and only sends relocs for the buffers whose `presumed` came back wrong); apply the **relocs**, patching the word in the buffer they name (`drivers/src/display/nvidia/pushbuf.rs` computes the word, the bound and the range, under test); and pass each push to the ring, which already existed (`eclipse_rm_exec_submit`, the one that drives `EXEC`). Three details that go wrong silently and are pinned by tests: **`LOW` beats `HIGH`** (the kernel is an `if LOW / else if HIGH`, so a reloc with both bits gets the low half); **`OR` is ADDED** to the address half, picking `vor` or `tor` by domain, it does not replace it; and **a misaligned `reloc_bo_offset` is rejected here**, where Linux lets it through and patches the previous word because it stores with `nouveau_bo_wr32(nvbo, off >> 2, ...)` — a deliberate divergence, patching the wrong word is not something to be compatible with. Off by default because no method stream on this path has reached a real engine yet. **The 3D class is NOT missing**: `eclipse_rm_init.c` allocates `TURING_A` (0xc597) on every channel it builds — stage 7b of `step17` for the fallback ctx-0 channel and 10b of `ctx_alloc` for each per-process context — because a channel without a graphics object hangs FECS halfway through RESTORE on the first 3D `SET_OBJECT` — all follow-up work validated on hardware, never simulated |
| `DRM_IOCTL_NOUVEAU_GEM_CPU_PREP` / `CPU_FINI` | 🟡 | `CPU_PREP` validates the handle (and that the process owns it) and **really waits**: it queues a fence-only GP entry behind everything the process has in its direct-submit ring and waits for it to land (with `RELEASE_WFI_EN` that proves the engines finished everything before it). It is a superset of Linux's per-BO wait (there is no per-buffer fence). `NOWAIT` → EBUSY if still running; 10 s without landing → EBUSY. Then it waits for the fences userspace put in the buffer's dma-buf with `DMA_BUF_IOCTL_IMPORT_SYNC_FILE` (the `dma_resv`), with `dma_resv_usage_rw`'s usage rule: a read waits for the writes, a write for everything; same `NOWAIT`/EBUSY. The per-submission RM path is synchronous and does not wait. `CPU_FINI` only validates |
| `DRM_IOCTL_NOUVEAU_GEM_INFO` | 🟡 | `size` is the object's, like `nouveau_gem_info`: the request rounded up to its GPU page (`nouveau_bo_fixup_align`: 4 KiB in sysmem; in VRAM 2 MiB, 64 KiB or 4 KiB depending on which one the request already fills, and 64 KiB at least for a compressed object), which is also what the RM allocates, what the quotas count and what the mmap registration covers. `tile_flags` comes back as `NONCONTIG | kind << 8` and nothing else, with the kind downgraded to its uncompressed counterpart when a compressed object (compressible kind from a client without uvmm) goes in 4 KiB pages (sysmem, no compression), as `nouveau_bo_alloc` does; in VRAM that object takes 64 KiB at least and keeps the kind, and a uvmm client's always comes back as is (see the `GEM_NEW` row); previously it returned the request as is. **Real `map_handle`** (same as `GEM_NEW`, same mechanism). **Real `offset`** when `VM_BIND` has already mapped the object (it cross-references `nouveau_vm_mappings` by `gem_handle`); still 0 if there is no `VM_BIND` yet |

## DRM syncobjs (`drm_syncobj`)

DRM core, not nouveau-specific — its ioctl range (`0xBF`-`0xCF`) sits ABOVE
`DRM_COMMAND_END` (0xA0), unlike private driver ioctls, so unlike the nouveau
uAPI numbers these never carry the `DRM_COMMAND_BASE` offset. The state lives
in `drivers::scheme::syncobj` (not in `linux-object`, where the ioctls are
dispatched) precisely so that a driver's own submission path — the nouveau
uAPI's `EXEC`, here — can signal a syncobj directly after a real hardware
completion, without calling up into a higher layer (this part of the crate
tree does not depend on `linux-object`, and the layer order only allows
calling down).

| ioctl | Status | Notes |
|---|---|---|
| `DRM_IOCTL_SYNCOBJ_CREATE` / `DESTROY` | ✅ | Pure bookkeeping (`Vec` + handle counter), no hardware access. `DESTROY` of a handle that is not in the caller's table (never created, already destroyed, another process's): `EINVAL`, like `drm_syncobj_destroy`'s `idr_remove` (not `drm_syncobj_find`'s `ENOENT`) |
| `DRM_IOCTL_SYNCOBJ_RESET` / `SIGNAL` | ✅ | Binary signal = timeline point 1. Like `drm_syncobj_array_find`: `pad` and a count of 0 are `EINVAL`, a handle array without an array (null pointer with a count) is `EFAULT`, a foreign handle `ENOENT` |
| `DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL` / `QUERY` | ✅ | Monotonic: a signal never moves the point backwards (real `drm_syncobj` semantics). Handles without an array: `EFAULT`; the points come after looking up the handles: in `TIMELINE_SIGNAL` a null points array is all zeros (Linux's `memset`; point 0 is `drm_syncobj_add_point`'s unordered signal, which here moves no counter), and in `QUERY`, which writes them, `EFAULT` |
| `DRM_IOCTL_SYNCOBJ_WAIT` / `TIMELINE_WAIT` | 🟡 | Real, and the wait **sleeps**: the syscall does it before `io_control`, on the async path (`DrmDev::syncobj_wait_sleep`, `linux-object/src/fs/devfs/drm_scheme.rs`), polling `syncobj::wait_ready` (which itself resolves landed hardware fences) with `fence_poll_wait`'s backoff: the first tries yield the CPU and the following ones sleep, up to ~1 ms between tries. The synchronous arm only finishes the ioctl (almost always on the first try). It is still polling with backoff and not a wait queue (hence the 🟡), but it no longer occupies the core. `timeout_nsec` is treated as an **absolute** `CLOCK_MONOTONIC` deadline (real Linux semantics, confirmed against this kernel's own `now_monotonic()`). Without `WAIT_FOR_SUBMIT` or `WAIT_AVAILABLE`, a handle with no fence for its point (binary just created or after `RESET`, a timeline point nobody submitted) is `EINVAL` immediately, for all the handles in the array (`drm_syncobj_array_wait_timeout`); an in-flight fence waits until the deadline (`ETIME`). A null handle array with a count is `EFAULT` (`drm_syncobj_array_find`); in `TIMELINE_WAIT` a null points array is all zeros. |
| `DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD` / `FD_TO_HANDLE` | 🟡 | **Real**, but dispatched in `linux-syscall` (`sys_drm_syncobj_fd`), NOT in `io_control` — they are the only syncobj ioctls that need the process's fd table, just like `PRIME_HANDLE_TO_FD`/`FD_TO_HANDLE`. Since the syncobj table is a GLOBAL handle space (not per process), "exporting" moves or copies nothing — the handle number is already globally valid, the fd only carries it. The `_SYNC_FILE` variants (bit 0 in both ioctls) move a loose FENCE, not the object: `EXPORT_SYNC_FILE` wraps the syncobj's current fence in an fd, and `IMPORT_SYNC_FILE` hands it to ANOTHER already existing syncobj. Implemented: here a fence is "syncobj S reached point N", so a `sync_file` is that pair, and the import is resolved on the fly (immediately if already reached — the normal case, because `EXEC` only signals its `sig` syncobjs once the GPU fence has landed — or as a registered dependency if not). It is the path by which Mesa passes finished work between a GLX/DRI3 client and the X server. Both fds are anonymous `O_RDONLY` inodes without `read` or `write` (EINVAL/EBADF, `pread`/`lseek` ESPIPE); the syncobj one has no `poll` (always ready; `epoll_ctl` EPERM) nor ioctl (ENOTTY), and the `sync_file` one has `SYNC_IOC_MERGE` (in `linux-syscall`), `SYNC_IOC_FILE_INFO` and `SYNC_IOC_SET_DEADLINE` (in `syncobj_file.rs`). Errors as in `drm_syncobj.c`: `HANDLE_TO_FD` of a foreign handle is EINVAL (the object) or ENOENT (`EXPORT_SYNC_FILE`, via `drm_syncobj_find_fence`); `FD_TO_HANDLE` looks at the fd first (one that names no file is EINVAL, not EBADF; `IMPORT_SYNC_FILE` only accepts a `sync_file`, a syncobj fd is EINVAL as for `sync_file_get_fence`) and then the destination handle (ENOENT) |
| `DRM_IOCTL_SYNCOBJ_TRANSFER` | ✅ | `drm_syncobj_transfer_ioctl`, in Linux's order: a non-zero `pad` is EINVAL, the destination is looked up before anything else (ENOENT), a flag other than `WAIT_FOR_SUBMIT` is EINVAL and then the source (ENOENT). Without `WAIT_FOR_SUBMIT`, a `src_point` nobody submitted (or a syncobj without a fence) is EINVAL immediately, like `dma_fence_chain_find_seqno`; with the flag the transfer is deferred and the destination signals when the source does. `syncobj::transfer` carries the `dst_point` to the destination's dependency link (see README-drm), so a software *timeline* reaches the promised point |
| `DRM_IOCTL_SYNCOBJ_EVENTFD` | 🟡 | **Real**, but dispatched in `linux-syscall` (`sys_drm_syncobj_eventfd`, `file.rs`), not in `io_control`, because it needs the eventfd from the process's fd table, like the fd ioctls; the *waiter* table and delivery (a `write` of 1 to the eventfd, immediate if the point is already reached, otherwise when the syncobj advances, with the 250 µs timer below) live in `linux-object/src/fs/syncobj_eventfd.rs`. Errors like `drm_syncobj_eventfd_ioctl`: a flag other than `WAIT_AVAILABLE` or a non-zero `pad` are EINVAL before looking at the handle; a foreign handle ENOENT; an fd that is not there EBADF; an fd that is not an eventfd EINVAL. `WAIT_AVAILABLE` is accepted (NVK and wlroots set it) but **does not bring delivery forward, on purpose**: delivering on submit let labwc sample the client's dma-buf with GR still writing (snow all over the Firefox window), so the eventfd always waits for the fence to land. That is the 🟡 |

Like the rest of this work, gated behind `nvidia.nouveau_uapi`
(`DRM_CAP_SYNCOBJ`/`SYNCOBJ_TIMELINE` in `GET_CAP` report 0 without the flag,
1 with it — so that no other driver/client that never touched this work sees a
capability bit change by default).

**What an `EXEC` signal really proves**: `eclipse_rm_exec_submit_signaled`
adds a SECOND GP entry right after the caller's — a single host semaphore
`RELEASE` written by the kernel at a fixed offset of the channel's OWN buffer
(never the caller's), with a single doorbell for both entries. The `RELEASE`
carries **`RELEASE_WFI_EN`**: the host waits for the channel's engines (GR/CE)
to go *idle* before writing the payload, so a signal that lands proves the
engine **finished executing** the caller's entry — the real equivalent of a
`dma_fence`, without having to know which engine class bound the caller's
content. It used to carry `WFI_DIS` and only proved the PBDMA had *fetched* the
entry; since all of NVK's syncobjs resolve against this payload, Mesa and
wlroots reused the staging and sampled textures the GPU was still writing
(partial uploads, "stale tiles", garbage on freshly created surfaces: the
`lunarbar` menu popup). Linux nouveau and NVK release with `WFI_EN` in exactly
the same way.

## Direct submit: `EXEC` without entering the RM, with pending fences

Measured on the RTX, `glxgears` with `vblank_mode=0` gave ~720 FPS under
Eclipse versus ~12,000 on Linux with the same GPU. The cost was not in the GPU
but in what the kernel did **on every** `EXEC`:

1. `eclipse_rm_exec_submit[_signaled]` entered the RM on every submission:
   API+GPU locks, a lookup of the channel and the buffer in the *resource
   server*, a **new BAR1 mapping of the USERD** (which lives in VRAM) via
   `memmgrMemDescBeginTransfer(USE_BAR1)` — that is, a BAR1 VA reservation,
   writing GMMU PTEs and a kernel MMIO mapping, undone again in
   `EndTransfer` — and several captured and formatted `nv_printf` lines.
2. Then the ioctl **blocked the CPU** *polling* the fence semaphore until the
   GPU consumed the push: CPU and GPU were serialised on every frame, for
   glxgears, for Xwayland's blit and for the compositor.

nouveau on Linux does none of that: per submission it writes two GP entries in
the ring, bumps `GPPut` and rings the *doorbell* (even from userspace), and the
syncobj is signalled when the fence lands, with nobody waiting.

### How it works now (by default)

- **Preparation, once per context** (`eclipse_rm_exec_fast_prepare`,
  `nvidia-rm-sys/vendor/eclipse_rm_init.c`): under the same locks every
  submission used to take, the RM computes what is **constant** for the
  channel — the *work-submit token* (a pure function of `runlist|chId` on
  Turing), a **persistent BAR1 mapping** of the USERD (`memdescMapOld`,
  released in `eclipse_rm_exec_fast_release` right before `ctx_free`), the
  physical addresses of the three sysmem pages a submission touches (GPFIFO
  ring at `0xC000`, 128 32-byte `SEM RELEASE` method *streams* at
  `0xA000..0xB000`, a `u32` landing zone at `0xB000`) and the BAR0 offset of
  the *doorbell* (`sriovState.virtualRegPhysOffset +
  NV_VIRTUAL_FUNCTION_DOORBELL`, the same as
  `kfifoUpdateUsermodeDoorbell_TU102`). It also returns the values encoded
  with the SDK's DRF macros for a test push, and the kernel **checks that its
  own encoder matches** before the first real submission (if it does not
  match, that context stays on the RM path and says so).
- **Submission, from Rust and without the RM** (`fast_submit`,
  `drivers/src/display/nvidia.rs`): with a per-driver spinlock of a few
  microseconds, it reads `GPGet`/`GPPut` through the BAR1 window, writes one GP
  entry per push, a `SEM RELEASE` *stream* in the ring slot the fence occupies
  (so a *stream* the PBDMA has not consumed is never rewritten: reusing the
  slot implies `GPGet` has passed it), `sfence`, `GPPut`, `sfence`,
  *doorbell*. If the ring is full it waits up to 1 s for `GPGet` to advance
  and, if not, latches the context as *wedged* and captures the GR hang probe
  (the same one as before).
- **Pending fence** (`syncobj::attach_hw_fence`): instead of waiting, `EXEC`
  records "syncobj S reaches point N when `*zone >= payload`" and returns. The
  payload is a monotonic sequence per context (it starts at 1; the zone starts
  at 0) and the comparison is a *wrapping* `>=`, so several in-flight fences
  share a single zone without ambiguity. Resolution is **lazy**: every access
  to the syncobj table (`WAIT`, `QUERY`, `EXPORT`/`IMPORT_SYNC_FILE`,
  `TRANSFER`, `EXEC` itself) first resolves the fences that have already
  landed. For the only client that does not ask again — an armed
  `SYNCOBJ_EVENTFD` (wlroots/Xwayland with *explicit sync*) —
  `linux-object/src/fs/syncobj_eventfd.rs` arms a 250 µs timer while there
  are *waiters* and pending fences, and nothing more.
- **`EXEC` waits (`wait_count > 0`)**: a pending fence **on the same channel**
  that is about to be submitted to counts as satisfied without waiting
  (`syncobj::wait_ordered`), because the GPFIFO executes in order — the new
  submission cannot run before it lands. Only fences from OTHER channels
  (another process, an imported `sync_file`) are waited for on the CPU.
- **Timeout**: a fence that does not land within 1 s fires the timeout *hook*
  (`nouveau_fence_timeout_hook`), which latches the context *wedged* (never
  0, the compositor's), captures the GR probe and releases the *waiter* by
  signalling the point — exactly what a killed channel does on Linux: the wait
  returns, and the client's next `EXEC` fails with `EIO` (NVK: *device
  lost*). When a context is released (`fast_release`), fences that can never
  land are abandoned by signalling them, so that a compositor waiting for a
  dead client's buffer carries on.

`EXEC` with `sig_count == 0` still has no fence (GP entries and that's it). A
context whose preparation fails (or whose encoder does not pass the self-check)
stays on the RM path **forever**, without retrying on every submission.

### How to compare and what to look at

- `nvidia.exec_rm` on the cmdline disables the direct path (everything
  through the RM, with the old synchronous *poll*). Useful for A/B with the
  same binary.
- `cat /proc/gpudbg` now has a `nouveau ioctl profile` section: count, total
  /mean/max time **per uAPI ioctl**, direct versus RM submissions,
  microseconds spent waiting for a ring slot or for syncobjs on the CPU, and
  the submit→land latency of fences. It is cumulative: two reads N seconds of
  `glxgears` apart tell how many µs of kernel time each frame costs and in
  which ioctl.
- In `dmesg`, `direct submit ACTIVE (token=… doorbell=BAR0+…)` per context
  confirms the path; `direct submit encoder self-check FAILED` or
  `exec_fast_prepare … failed` explain a fallback to the RM path.

### What does NOT change

The fence is still a host `SEM RELEASE` after the last push, now with
`RELEASE_WFI_EN` on both paths (direct and RM): it proves the engine finished
the caller's entry (see "What an `EXEC` signal really proves"). The Rust
encoder and the C `chkSemExecute` must match, or the boot self-test disables
the direct path. The wait in `SYNCOBJ_WAIT` sleeps on the syscall's async path
(`syncobj_wait_sleep`), polling with backoff.

## Reclaim on process exit

`linux-object` already had a process-exit hook
(`zircon_object::task::set_process_exit_hook`, in
`linux-object/src/fs/mod.rs`) that frees the `CREATE_DUMB`/PRIME objects of a
process that dies without `DESTROY_DUMB`/`GEM_CLOSE` (`drm::release_process`).
Nothing equivalent existed for nouveau's private state — a client that crashes
(or is killed by the compositor) without `CHANNEL_FREE`/`GEM_CLOSE` leaked the
channel and all its real VRAM until the next reboot.

**The underlying problem**: `drivers` (where `NvidiaGpu` lives) cannot know
which process is making a call — it does not depend on
`kernel-hal`/`zircon-object` (`kernel-hal`'s `get_current_thread()` returns an
opaque `Arc<dyn Any>` on purpose; only layers above, which do know the concrete
type `zircon_object::task::Thread`, can turn it into a pid). `linux-object`
does know it (it already uses it for `release_process`), so the pid is pushed
down instead of trying to make `drivers` work it out:

- `DrmScheme` gains `ioctl_owned(request, arg, owner_pid)` (default: ignores
  `owner_pid` and calls `ioctl` — ZERO impact on any other driver, e.g.
  `virtio-gpu`). `drm_scheme.rs`'s dispatch of unknown ioctls now calls
  `ioctl_owned(cmd, data, drm::current_pid())` instead of `ioctl(cmd, data)`
  directly.
- `NvidiaGpu::ioctl` (the trait method, still needed because some paths call
  it without knowing a pid) becomes a thin wrapper over
  `ioctl_owned(request, arg, 0)`; all the real logic now lives in
  `ioctl_owned`, which passes `owner_pid` to `nouveau_ioctl`.
- `CHANNEL_ALLOC` stores that `owner_pid` in `NouveauChannelState`.
- `DrmScheme` gains `nouveau_release_process(pid)` (default: no-op).
  `NvidiaGpu` implements it with **per-process** scope (no longer "a single
  channel"): it drains only the `VM_BIND`s of the exiting pid (`owner_pid`),
  frees its `nouveau_gem` objects **respecting the PRIME refcount**
  (`dec_ref`, not an unconditional `unregister` — a buffer another holder
  still imports is kept alive with the owner detached, `owner_pid=0`, and is
  freed by the last holder's `GEM_CLOSE`), frees its RM class objects
  (`class_objects_drain_pid`, CHILDREN of the channel, BEFORE the channel so as
  not to double-free) and its own context (`ctx_free`, ctx ≥ 1).
- The exit hook (`drm_release_on_exit`, `linux-object/src/fs/mod.rs`) now also
  calls `driver.nouveau_release_process(pid)` alongside the generic
  `release_process(pid)` it already had.

**Also covered** by `gem_mmap`'s per-pid bookkeeping (`release_pid`): a client
that **self-imports its OWN** buffer (GBM export → EGL/NVK re-import) and dies
**without `GEM_CLOSE`** (^C/crash) drops all its references, the creator's and
the import's, so it no longer leaks the buffer until reboot. Before that, the
following detail was a known limitation; it is kept as history:
work with hardware in the loop.

## Per-process GPU context (multi-context)

The "single global channel/VAS" model described above was the starting point;
for client GL apps (NVK/zink) to coexist with the compositor, each process
needs its OWN address space and channel. On top of the shared `step16` ladder
(client/device/subdevice), up to `ECLIPSE_MAX_CTX-1` independent contexts are
built:

- **`eclipse_rm_ctx_alloc(gpuInstance, ctxIdx)`** builds its own VAS + TSG +
  ctxshare + USERD + buffer + GPFIFO + `TURING_COMPUTE_A` for each `ctxIdx` in
  `1..8`, reusing the compositor's client/device/subdevice. The compositor
  keeps `ctxIdx == 0` (the original `step16`/`step17` ladder, intact).
- **`ensure_ctx_for_pid`** (in `nvidia.rs`) is the pid→context routing
  authority: on a process's FIRST touch (its first `VM_BIND` or
  `CHANNEL_ALLOC`, whichever comes first) it assigns a free `ctxIdx` and
  records it in `nouveau_pid_ctx` (pid → ctxIdx/hVas/hNotifier). That way a
  client's `VM_BIND`s and its channel ALWAYS share a VAS — the root cause of
  the MMU fault NVK saw was that its binds landed in the compositor's VAS (ctx
  0) while its channel ran in ctx 1.
- **`eclipse_rm_ctx_prime(gpuInstance, ctxIdx)`** loads the compute engine's
  *golden context* for each client context when it is assigned, running the
  minimal `step18` stream (host-sem + `SET_OBJECT(TURING_COMPUTE_A)` + engine
  report-sem) on the context's own channel and CPU-polling the engine
  semaphore (which only lands after the context switch that loads the golden
  context), capped at 500 ms. Without this, NVK's FIRST push triggers the cold
  load of the golden context, which hangs the PBDMA before reaching NVK's
  fence (seen on RTX as `GPGet=1 GPPut=2`, with no MMU fault or GR exception)
  → fence timeout → `vkCreateDevice` EIO. The compositor (ctx 0) never suffers
  it because `step18` already primes its golden context at bring-up.
  `ensure_ctx_for_pid` calls the prime after RELEASING the `nouveau_pid_ctx`
  lock (the prime can take 500 ms and `ctx_idx_for_pid` — `EXEC`'s hot path,
  including the compositor's own submissions — shares that lock); the context
  is kept whatever happens with the prime (a timeout is diagnostic, not
  fatal). On RTX the prime lands (`golden context primed OK`), but on its own
  it does NOT unblock NVK: it proves the client channel's **compute** engine
  works, while NVK's real push uses the **3D** class — see the next point.
- **`eclipse_rm_class_alloc(gpuInstance, ctxIdx, classId)`** allocates an
  engine CLASS object (3D `0xc597`, compute `0xc5c0`, copy `0xc5b5`, …) on the
  channel of context `ctxIdx`. The class object is a CHILD of the channel, and
  allocating the class is what makes the RM/GSP build that engine's context on
  THAT channel (for GR: the golden image, the patch buffer and the global
  buffers, mapped into its VAS). NVK's init push opens with
  `SET_OBJECT(subch 0, 0xC597)` + a 3D method; without the 3D object on the
  client's channel, the GR engine loads a context never built there and the
  PBDMA hangs one GP entry before the fence (`GPGet=N GPPut=N+1`), with no MMU
  fault or exception. `class_alloc` used to fire on `g_grChanCache.hChannel`
  (ctx 0) unconditionally — a legacy of the single-channel era — so the
  client's 3D class was built on the COMPOSITOR's channel and the client's
  channel was left without a GR context. Now `NVIF NEW` resolves
  `ctx_idx_for_pid(owner_pid)` and allocates it on the right channel
  (compositor → ctx 0, each client → its ctx). Class objects are tracked by
  `owner_pid` (`CLASS_OBJECTS`), so cleanup on process exit frees only its
  own, not those of another live channel.
- **`eclipse_rm_ctx_free(gpuInstance, ctxIdx)`** (called from
  `nouveau_release_process`) frees the client's context and clears its slot
  when the process exits, so that a relaunch gets a FRESH context (not a
  cached and possibly RC-wedged one) and slots do not leak one per launch.
  `NouveauVmMapping`/`NouveauGemObject` carry `owner_pid`, so draining
  binds/objects and teardown are ALWAYS scoped to the owning pid — one
  client's state never touches the compositor's or another client's.

### One context per DRM file (`nvidia.ctx_per_file`)

Linux has a VM not per process but per file: every `open()` of the node has
its `nouveau_cli`, and `VM_INIT`, `VM_BIND`, `EXEC` and the channels live in
it. Here all of that goes by pid, and it is the same as long as the process
has a single NVK device (a GL client, labwc with GLES2). labwc with Vulkan has
two alive at once: wlroots' `VkDevice` and the one zink creates under GBM for
the allocator. NVK hands out VAs from its own heap (the same addresses on both
devices) and keeps its channel state per device, so when they shared a context
the second trampled the first's mappings and GR state, and wlroots' first
frame was `VK_ERROR_DEVICE_LOST`.

With `nvidia.ctx_per_file` (also turned on by `nvidia.wlr_vulkan`;
`nvidia.ctx_per_file=0` turns it off) the VM ioctls (`VM_INIT`, `VM_BIND`,
`EXEC`, `CHANNEL_ALLOC`/`FREE`, `NVIF`) run under a per-file **key**
(`drivers/src/display/nvidia/file_ctx.rs`). The process's first active file
takes the pid as its key, so a process with a single device works exactly as
before; another file active at the same time gets a key of its own (the pid
with bit 61 and a serial) and with it its own context, VAS and ring. GEM
objects, syncobjs and PRIME holds still belong to the process
(`file_ctx::pid_of`), and a GEM's memory is allocated in the pid's context, so
closing a second device's file never takes away memory someone else is still
using. The file NVK enumerates with (a throwaway channel, and no binds) gives
its key back when that channel is freed; a closed file's context is freed on
the next ioctl, and those of an exiting process before the pid's.

## Known gaps and what is needed to close them

- **`EXEC` with `wait_count > 0`**: same-ctx uses a **HW ACQUIRE** in the
  pushbuffer (`sem_acquire_stream` + a GP entry before the client's pushes).
  Cross-ctx is still a CPU wait (10 s). A real ACQUIRE between different
  channels would need the GPU VA of the other ctx's semaphore.
- **`SYNCOBJ_WAIT`/`TIMELINE_WAIT` by polling with backoff, not a real wait
  queue**: see the table above — the wait sleeps on the syscall's async path
  and no longer occupies the core, but it is still polling.
- **Syncobj refcount**: `CREATE` starts at refs=1; `HANDLE_TO_FD` does
  `add_ref`; `DESTROY`/closing the fd decrements; the object only dies on
  reaching 0 (previously a DESTROY killed exported fds).
- **GSP GPU console**: on-demand auto-bringup is **opt-in** with
  `nvidia.console_gsp` (in addition to `nvidia.nouveau_uapi`). Without it, a
  manual `cat /proc/gpustep14` is needed.
- **`nvidia.console_gpu`**: schedules the deferred bring-up of the console GPU
  (wait for KD_GRAPHICS, pause the scanout, `gpustep14`, resume) even if no GPU
  client ever shows up. It opens the same gate as `nvidia.console_gsp`, but
  **from inside the deferred task and with the scanout already paused**, not
  at boot: that gate is also the one that lets the first GPU client's
  `CHANNEL_ALLOC` launch `bringup_step14`, and that client can appear as soon
  as the desktop starts, with the scanout live — exactly the window this path
  exists to avoid. If you also want on-demand bring-up at the first client,
  pass both flags. The scanout pause has a watchdog (90 s): if `gpustep14`
  hangs or aborts, the scanout resumes by itself instead of leaving the
  desktop frozen forever. It does not touch the boot path: if SEC2 STARTCPU
  hangs, it hangs with the desktop already on screen, not halfway through
  boot. If the GPU comes up, present moves to its own copy engine — a copy
  within the same card, no P2P over PCIe — and `CE_PRESENT_ENABLED` is
  re-evaluated at that moment (unless `nvidia.nocepresent`). It is the only
  route to some acceleration on a **single-GPU** box: without a second card
  there is nowhere to do P2P from, so a cold console GPU means CPU blit and
  nothing more.
- **`osInitMapping`**: the portable half of Linux's implementation
  (`osinit.c:1260`) now lives in `vendor/eclipse_rm_init.c`: it disables the
  expansion ROM mapping and sets the PCI latency timer to the maximum, through
  the card's own `NV_PCFG` window. It used to be an `os_boundary.rs` stub that
  returned `NV_ERR_NOT_SUPPORTED`, and since `kbifInit` (`kernel_bif.c:130`)
  calls it under `NV_CHECK_OK_OR_RETURN`, that aborted `gpuStatePreInit` at
  stage 4: the console GPU stayed attached but with no RM state at all
  (`PFB_CSTATUS=0xbadf5040`, runlists at zero). It runs on **every** card the
  RM attaches, the compute one too.
- **GRAPHICS prime**: `ctx_prime` requires compute+3D; a failure **discards**
  the context (software fallback) instead of publishing READY and hanging FECS
  on the first draw. `step18` also primes GRAPHICS on ctx0. Prime timeout =
  3 s.
- **`VM_BIND` with `op_count` > 1 is not atomic**: each op is applied in order
  with its own real RM call; if `op[i]` fails, `op[0..i]` have already been
  applied and stay that way, and `op[i+1..]` never run. This matches how real
  nouveau `VM_BIND` behaves (each op is validated and applied as it is
  processed, not as an all-or-nothing transaction), but it is worth keeping in
  mind when debugging a failure halfway through an array.
- **`EXEC` with `sig_count` > 1 is not atomic when signalling either**: if
  syncobj `i` has an invalid handle, the syncobjs before `i` have already been
  signalled and those after `i` are never attempted — the same behaviour a
  single bad handle already had before this milestone, only now there is more
  than one that can fail.
- **An explicit `CHANNEL_FREE` does not free `hMemory`**: `CHANNEL_FREE` and
  `GEM_CLOSE` share `drain_vm_mappings` (`nvidia.rs`) to drop the VA
  reservations (`h_virt`) of any `VM_BIND` left alive, but `CHANNEL_FREE`
  itself does NOT touch `nouveau_gem` — GEM objects (and their `hMemory` in the
  RM heap) stay allocated even though they are no longer mapped in any VAS.
  Correct in the sense that real nouveau's `CHANNEL_FREE` does not free the
  client's GEM objects either (they are independent resources), so this is not
  "fixed" — but it means a client that calls `CHANNEL_FREE` and stays alive
  without ever calling `GEM_CLOSE` keeps that VRAM allocated until the process
  ends (see "Reclaim on process exit" below, which does cover that final
  case).
- **`GEM_CLOSE` counts PRIME references (self-import)**: a swapchain buffer is
  owned by several at once — the original owner of the `GEM_NEW` (the
  compositor's GBM/NVK allocation) PLUS every PRIME self-import that
  `FD_TO_HANDLE` resolved to the SAME nouveau handle (self-import; see
  `gem_mmap::lookup_by_phys`). NVK imports a buffer, uses it and does
  `GEM_CLOSE` on that handle while wlroots still owns the same handle, so
  freeing on the FIRST close ripped the object out from under wlroots.
  `gem_mmap` keeps a list of **holders per pid**: `register` (in `GEM_NEW`)
  records the creator, `add_ref` adds the importer on each self-import
  (`drm::nouveau_gem_add_ref` from `sys_drm_prime`), `dec_ref` removes ONE
  reference of the pid doing `GEM_CLOSE` (`nouveau_gem_close`; a pid that is
  not a holder gets EINVAL) and `release_pid` removes all those of the dying
  process. The GEM object, its `VM_BIND`s and its `hMemory` in the RM are only
  freed when the list is empty — the LAST holder. That same list is what
  authorises `GEM_INFO`, `VM_BIND MAP`, `CPU_PREP` and `CPU_FINI`
  (`gem_usable_by`): a process cannot touch, or map into its VAS, a buffer it
  neither created nor imported; `CHANNEL_FREE` only accepts the process's own
  channels. Closing the dma-buf fd does not touch the list (it only drops its
  `Arc<VmObject>`). See the RTX log in `README-nvk-hardware-status.md` ("El
  export funcionó...") for the `dmesg` that uncovered it (a second
  self-import falling back to a generic handle → `GEM_INFO` ENOENT → zink
  "heap=0").
- **`CPU_PREP` waits per channel, not per buffer**: there is no implicit
  fencing per `hMemory` (which fence was the last to touch each buffer), so
  `CPU_PREP` waits for EVERYTHING the process has queued in its ring (a
  fence-only entry at the end, see the table). Correct but more conservative
  than Linux. What IS per buffer is what userspace imports by name: the
  dma-buf fd has `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` and `IMPORT_SYNC_FILE`
  (`drivers/src/scheme/resv.rs`, the buffer's `dma_resv`, by physical
  address), and `CPU_PREP`, the legacy present and the next export wait for
  those fences with Linux's usage rule. The export returns a `sync_file` that
  joins the ring probes of the owner and of every process mapping the buffer
  (`DrmScheme::queued_fences`, the same superset) with what was imported.
  Previously both ioctls were `ENOTTY`.
- **Real `CHIPSET_ID`**: today it is the minimum of the `PMC_BOOT0` range of
  the architecture already identified by PCI ID, not a live read of
  `PMC_BOOT0`. Deliberate: any new register read on the path of an ioctl Mesa
  can call at any point during boot needs the same caution already documented
  in `DrmScheme::debug_dump` ("an early BAR0 access can hang some GPUs").
- **More than one channel**: `CHANNEL_ALLOC` still depends on `step16`/
  `step17`, which always build the same fixed ladder — they do not accept
  "another client, another TSG". Unchanged since the previous milestone.
- **GPFIFO ring shared with `step18`/`step19`**: `eclipse_rm_exec_submit`
  reads `GPPut`/`GPGet` live from the USERD (not a Rust-side counter of its
  own), so it should not trample entries `step18`/`step19` already wrote in
  the same boot — but that interaction has never been tested on real
  hardware. If you are going to experiment with `EXEC`, bear in mind that
  `/proc/gpustep18`/`19` also ran in the same boot.

## What to test first on real hardware

### Step 0 — discovery/identity (BEFORE any ioctl)

NVK (Mesa's Vulkan, which Zink→GL goes through) **does not touch a single
nouveau ioctl** until it has *discovered* the DRM node via libdrm and accepted
it. Its candidate filter is, in order: a `renderD*` node present,
`bustype == DRM_BUS_PCI`, and **`vendor_id == 0x10de`** — all read from sysfs
(`/sys/dev/char/226:128/device/{vendor,subsystem,…}`) *before* opening the
device. If any of them fails, NVK skips the node and
`vkEnumeratePhysicalDevices` returns 0 GPUs **without a single nouveau trace**
(because `nouveau_ws_device_new`, which issues `VERSION`/`GETPARAM`, is never
called). A `vulkaninfo` with "Failed to detect any valid GPUs" + an empty
`dmesg | grep nouveau` = a failure here, not in the uAPI.

A concrete trap already seen: the DRM node's PCI backing picked "the first
class 0x03 device". On any board with **another video device that enumerates
before the RTX** — an integrated iGPU, or (typical on workstation/server
boards with 2 NVIDIA GPUs) an **onboard BMC management VGA** (ASPEED
`0x1a03`, Matrox `0x102b`) — that "first 0x03" is NOT the NVIDIA → sysfs
advertised the wrong vendor → NVK discarded the node. Note: the sysfs PCI scan
(`scan_pci_devices`) uses the SAME `scan_bus(PortOpsImpl, PCI_ACCESS)` as the
probe that already bound the driver, so the GPUs ARE in the list — the failure
was the *choice*, not their absence. Fixed: with the uAPI active, the class
0x03 device **with vendor `0x10de`** is preferred (`display_pci_index` in
`linux-object/src/fs/sysfs.rs`).

Pending/possible refinement: with 2 NVIDIA GPUs the node is backed by the
*first* NVIDIA by bus, while `get_primary_driver()` (where the ioctls go) is
the *last* one probed; both are NVIDIA with a real `nouveau_ioctl`, so NVK's
enumeration works the same, but the right thing would be to back the node with
the exact BDF of the primary driver (it requires reconciling the decimal BDF
in the driver's name with sysfs's hex one).

Boot diagnostics (level `warn`, only with the uAPI active):
- `[drm-probe] PCI[i] BDF vendor=… class=…` — the full PCI inventory, and
  `[drm-probe] render node backed by PCI[i] … vendor=…` — which GPU the node
  points at (it must be `0x10de`).
- `[drm] VERSION on /dev/dri/renderD128 … primary_driver="Nvidia GPU"` — if
  it shows up, NVK **did** pass discovery and reached `VERSION`; if in
  addition `primary_driver` is not `"Nvidia GPU"`, the driver-private ioctls
  are being routed to the wrong driver.

With `nvidia.nouveau_uapi` active, the GPU already attached to the RM
(`/proc/gpustep5` `;6;8;9`, or `gpustep14` on the console GPU), **and Step 0
passed**:

1. `GETPARAM` with `PCI_VENDOR`/`PCI_DEVICE`/`FB_SIZE` — no hardware access,
   it should always work; if it fails, the bug is in the ioctl dispatch, not
   in the RM.
2. `CHANNEL_ALLOC` — reuses `step16`+`step17`; if those two already work via
   `/proc/gpustepN`, this should only wrap the same result in nouveau's shape.
   Confirm that `hVas`/`hNotifier` in the log match what those endpoints
   already report.
3. `GEM_NEW` with `DOMAIN_VRAM` and a small size (e.g. 4096) — now a real
   allocation from the RM heap; confirm in the log the returned `hMemory` and
   that the allocation does not interfere with anything else already using
   the RM.
4. `VM_INIT` **standalone, FIRST** (before `CHANNEL_ALLOC`) — it must accept
   and return 0. NVK calls it in `nouveau_ws_device_new` while creating the
   physical device, long before any channel; requiring a channel here aborts
   NVK's enumeration (0 GPUs). It must NOT fail with "no channel".
5. `VM_BIND` (`MAP`) of the handle from (3) at a chosen VA (e.g.
   `0x7000_0000_0000`, page-aligned) — confirm `virtStatus`/`mapStatus` in the
   log and that `actualVA` matches what was asked for.
6. `VM_BIND` (`UNMAP`) of the same region — confirm it does not leave the VAS
   in an odd state for a later `MAP`.
7. **The riskiest experiment**: write by hand (from another program, via
   `mmap` of a generic `CREATE_DUMB` + `PRIME` to the real GEM, or some other
   CPU-visible path) a simple method (e.g. a single host semaphore `RELEASE`,
   just as `step18` does) into the buffer mapped by (5), and then `EXEC` with
   `sig_count=0` pointing at that VA — confirm the semaphore shows up. This is
   what really proves the generic path (not `step18`'s hard-coded one) works.
8. `SYNCOBJ_CREATE` (without `DRM_SYNCOBJ_CREATE_SIGNALED`) — confirm that
   `SYNCOBJ_QUERY` returns point 0.
9. `EXEC` of the same pushbuffer as (7), this time with `sig_count=1` pointing
   at the syncobj from (8) — confirm in the log that
   `fenceSubmitStatus`/`fenceWaitStatus` are `NV_OK` and that `SYNCOBJ_QUERY`
   now returns point 1 (or the requested `timeline_value`, if it is a
   timeline syncobj).
10. `SYNCOBJ_WAIT` on an ALREADY signalled syncobj — it must return
    immediately. On an unsignalled one with a short `timeout_nsec` (an
    absolute `CLOCK_MONOTONIC` deadline, not relative) — it must return with
    an error after the timeout, not hang.
11. `EXEC` with `wait_count=1` pointing at an UNSIGNALLED syncobj — confirm
    the ioctl blocks (rather than failing immediately) and that if another
    thread/process signals it (`SYNCOBJ_SIGNAL`) within 1 s, only then does
    `EXEC` submit the pushbuffer and return `NV_OK`. Repeat without ever
    signalling it — it must return `EIO` after ~1 s, and the pushbuffer must
    NOT have been submitted (check that the USERD's `GPPut` did not advance).
12. `GEM_NEW` with `DOMAIN_VRAM` — confirm in the log that `gem_map_cpu`
    returns `lookupStatus=0 addressSpace=0` and a non-zero `map_handle`. Then
    `mmap(card_fd, size, PROT_READ|PROT_WRITE, MAP_SHARED, fd, map_handle)`
    from the same process: it must return a valid pointer, not fail with
    `EINVAL`.
13. Write a known pattern through the pointer from (12), and read it back —
    it confirms the mapping points at real, stable memory (not some reused
    page). If there is a way to read VRAM by another path (e.g. `VM_BIND` +
    `EXEC` with a kernel that copies to a verification buffer), compare
    against that to confirm it is the SAME VRAM the GPU sees, not a copy.
14. `GEM_CLOSE` (generic ioctl, not `DRM_IOCTL_NOUVEAU_*`) on the handle from
    (12) — confirm in the log `nouveau_gem_close` with
    `gem_free status=Some(0x0)`, and that a later `mmap()` with the SAME
    `map_handle` now fails (the `gem_mmap` entry was removed). Repeat
    `GEM_CLOSE` on the same handle a second time — it must fail (`EINVAL`),
    not repeat the `gem_free`.
15. `GEM_NEW` + `VM_BIND` `MAP` (without `UNMAP`) + a direct `GEM_CLOSE` on
    that handle — confirm in the log a line "GEM_CLOSE handle=...: dropped
    stale VM_BIND VA=... -> vm_bind_unmap status=0x0" before the `gem_free`,
    and that a later `VM_BIND` `UNMAP` on that same VA now fails with `ENOENT`
    (the entry was drained, not orphaned).
16. `GEM_NEW` + `VM_BIND` `MAP` + `CHANNEL_FREE` (without `GEM_CLOSE` or a
    `VM_BIND` `UNMAP` before it) — confirm in the log "CHANNEL_FREE: dropped
    stale VM_BIND VA=..." for that VA, and that a later `CHANNEL_ALLOC` +
    `VM_INIT` followed by `GEM_INFO` on the same GEM handle reports
    `offset=0` again (the mapping really was dropped, not just hidden) while
    `map_handle` stays the same (the CPU-mmap registration in `gem_mmap` is
    independent of `VM_BIND` and `CHANNEL_FREE` does not touch it). The GEM
    handle itself must stay alive — `GEM_INFO` must not return `ENOENT`.
17. `CHANNEL_ALLOC` + `GEM_NEW` + `VM_BIND` `MAP` from a process, and kill it
    (`kill -9` or let it crash by itself) WITHOUT calling `CHANNEL_FREE` or
    `GEM_CLOSE` — confirm in the log a line "process exit pid=...: released
    nouveau channel + N GEM object(s), K KiB", and that a later
    `CHANNEL_ALLOC` from ANOTHER process works immediately (not `EBUSY`,
    which is what it would return if the channel had been left occupied).
    Repeat by killing ANY process that never called `CHANNEL_ALLOC` — nothing
    must happen (no "released nouveau channel" logs, and another client's
    channel is not touched).
18. `VM_BIND` with `op_count=3` (two `MAP`s of different handles + an `UNMAP`
    of a VA that does NOT exist) — confirm the first two `MAP`s really were
    applied (`GEM_INFO` on both handles must report their `offset`) even
    though the third returns `ENOENT` and the whole ioctl fails; the log must
    show "op[2] of 3 failed, stopping (2 earlier op(s) already applied)".
19. `EXEC` with `push_count=3` and `sig_count=1` — each *push* must show up in
    the log ("EXEC pushVA=... -> submitted") in order, and only the kernel
    fence runs after the LAST one; confirm the syncobj is signalled only after
    the three log lines, never before.
20. `EXEC` with `wait_count=3` pointing at three syncobjs — confirm the ioctl
    blocks until ALL THREE are signalled (`wait_all=true`), not just the
    first; signalling two and leaving one unsignalled must keep blocking until
    the 1 s timeout.
21. `VM_BIND` with `op_count=65` or `EXEC` with `push_count=65` — they must
    return `EOPNOTSUPP` immediately (above this milestone's limit of 64), not
    try to read 65 elements.
22. `SYNCOBJ_CREATE` + `SYNCOBJ_HANDLE_TO_FD` on that handle — the returned
    `fd` must be a valid descriptor (`fstat` must not fail). Pass it to
    ANOTHER process (`fork` + inheritance, or `SCM_RIGHTS` over a Unix socket)
    and from there call `SYNCOBJ_FD_TO_HANDLE` — the resulting handle must
    behave like the original: `SYNCOBJ_QUERY` reports the same point, and
    `SIGNAL`/`WAIT` on either handle (the exporter's or the importer's)
    affects the same underlying syncobj.
23. `sync_file`: create two syncobjs A and B; signal A; export A's fence with
    `SYNCOBJ_HANDLE_TO_FD` + `flags=..._EXPORT_SYNC_FILE`; import it into B
    with `SYNCOBJ_FD_TO_HANDLE` + `flags=..._IMPORT_SYNC_FILE` (`handle`=B,
    `fd`=the exported one) — `SYNCOBJ_QUERY(B)` must go to 1 and a `WAIT` on
    B return immediately. Repeat by exporting BEFORE signalling A: B must stay
    unsignalled, and signalling A afterwards must be enough for a `WAIT` on B
    to return. Passing that same `sync_file` fd to `FD_TO_HANDLE` WITHOUT the
    flag must give `EINVAL` (aliasing the two objects instead of copying the
    fence would produce fences that seem to signal early).

## File map

| File | Role |
|---|---|
| `drivers/src/display/nouveau_uapi.rs` | ioctl numbers, structs (exact C layout of `nouveau_drm.h`), opt-in flag |
| `drivers/src/display/nvidia.rs` (`NvidiaGpu::ioctl_owned` → `nouveau_ioctl`) | the real dispatch; also `drain_vm_mappings` and `nouveau_release_process` |
| `drivers/src/scheme/syncobj.rs` | DRM syncobj state and polling — generic, no hardware access |
| `drivers/src/scheme/gem_mmap.rs` | `handle -> (phys_addr, size)` registry for a driver's private GEM objects (today: nouveau `GEM_NEW`) that need to be mmap-able through the same fake-offset mechanism `CREATE_DUMB` already uses |
| `drivers/src/scheme/drm.rs` (`DrmScheme::ioctl_owned`/`nouveau_gem_close`/`nouveau_release_process`) | trait extension points for the caller's pid and cleanup of a driver's private resources — default no-op for any driver that does not need them |
| `nvidia-rm-sys/vendor/eclipse_rm_init.c` (`eclipse_rm_gem_alloc_vram`/`gem_free`/`vm_bind_map`/`vm_bind_unmap`/`exec_submit`/`exec_submit_signaled`) | the generic RM primitives, modelled line by line on `step16`-`step19` (which did run on real hardware) |
| `nvidia-rm-sys/src/rm_init.rs` (`gem_alloc_vram`/`gem_free`/`vm_bind_map`/`vm_bind_unmap`/`exec_submit`/`exec_submit_signaled`) | safe Rust wrappers over the above |
| `linux-object/src/fs/devfs/drm_scheme.rs` | dispatch of the `SYNCOBJ_*` ioctls (DRM core, not nouveau-specific), of `GET_CAP` for `DRM_CAP_SYNCOBJ*`, and the fallback to `ioctl_owned` for unrecognised ioctls |
| `linux-object/src/fs/devfs/drm.rs` (`current_pid`) | resolves the current process's pid — it already existed for `release_process`, now also used for `ioctl_owned` |
| `linux-object/src/fs/mod.rs` (`drm_release_on_exit`) | process-exit hook — reclaims `CREATE_DUMB`/PRIME (already existed) and now also nouveau's private state (`nouveau_release_process`) |
| `linux-object/src/fs/syncobj_file.rs` (`SyncobjHandle`) | a `FileLike` object wrapping a syncobj handle for `HANDLE_TO_FD`/`FD_TO_HANDLE` — the same pattern as `dmabuf.rs` for PRIME |
| `linux-syscall/src/file/file.rs` (`sys_drm_syncobj_fd`) | dispatches `SYNCOBJ_HANDLE_TO_FD`/`FD_TO_HANDLE` with access to the process's fd table — like `sys_drm_prime` for PRIME, and for the same reason (`io_control`, at inode level, does not have that table) |
| `kernel-hal/src/drivers.rs` (`set_nouveau_uapi_enabled`) | a bridge so `zCore` can enable the flag without depending directly on `zcore-drivers` |
| `zCore/src/main.rs` | reads `nvidia.nouveau_uapi` from the cmdline |
