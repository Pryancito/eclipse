# DRM / KMS on Eclipse OS — conformance with the Linux UAPI

[Español](README-drm.md) · **English**

This document maps Eclipse OS's DRM (Direct Rendering Manager) implementation
against the Linux kernel documentation
([`Documentation/gpu`](https://github.com/torvalds/linux/tree/master/Documentation/gpu)),
and records what is implemented, what is partial and what is missing.

## Scope: what "being compatible" means

`Documentation/gpu` has two very different kinds of content:

- **The contract with userspace (UAPI)** — `drm-uapi.rst`,
  `drm-usage-stats.rst`, `driver-uapi.rst`. This is what really decides whether
  Linux graphics software (libdrm, Mesa, wlroots, Xorg…) works. **This is what
  Eclipse OS implements.**
- **Linux kernel internals** — `drm-internals.rst`, `drm-mm.rst`,
  `drm-kms-helpers.rst`, `drm-ras.rst`, etc. They describe internal structures
  and *helpers* of Linux's DRM (TTM, `drm_device`, KMS midlayers…). They are not
  a contract observable from userspace: a from-scratch reimplementation **does
  not need to reproduce them**, only to offer the same UAPI on top.

Eclipse OS is not Linux: there is no DRM *midlayer*. The base path implements
the UAPI directly over a **"software KMS"** (NVIDIA GPUs add the nouveau uAPI on
top, see [README-nouveau-uapi.en.md](README-nouveau-uapi.en.md)): when there is
a framebuffer (UEFI GOP, virtio-gpu, …) the minimal KMS objects are synthesised
(1 CRTC + 1 connector + 1 encoder + 1 primary plane) and scanout is done by
copying the client's *dumb buffer* to the framebuffer (`blit_from`). That is
enough for software Wayland compositors (wlroots/labwc with pixman) and for
Xorg with the `fbdev` driver.

## Device nodes

| Node | major:minor | Status |
|---|---|---|
| `/dev/dri/card0` | 226:0 | ✅ primary node (KMS + dumb buffers) |
| `/dev/dri/renderD128` | 226:128 | ✅ render node; only `DRM_RENDER_ALLOW` (modeset/dumb/master → EACCES) |
| `/dev/fb0` | 29:0 | ✅ legacy framebuffer (`fbdev`) |
| `/sys/class/drm/card0` | — | ✅ minimal sysfs entries |

### Render nodes (`drm-uapi.rst` → *Render nodes*)

Linux makes `renderD128` accept **only** the ioctls marked `DRM_RENDER_ALLOW` in
`drm_ioctl.c` (`VERSION`, `GET_CAP`, `GEM_CLOSE`, `PRIME_*`, `SYNCOBJ_*`,
`SET_CLIENT_NAME` and the driver's command range) and return **EACCES** to any
modeset, *dumb buffer* or master/auth ioctl. Eclipse applies the same filter
(`render_allowed` in `drm_scheme.rs`): the desktop drives KMS on `card0` with
`WLR_DRM_DEVICES=/dev/dri/card0`, so the render node is no longer a second open
KMS.

### User pointers

Every ioctl argument (and every nested pointer: `fb_id_ptr`, `modes_ptr`,
`clips_ptr`, `handles`, blob `data`, `GETPROPERTY`'s
`values_ptr`/`enum_blob_ptr`, nouveau's `op_ptr`/`push_ptr`…) is checked
against the user half of the address space before it is touched; a null or
kernel address returns **EFAULT**, like `access_ok()` in Linux.

> This sentence was false until 20 Sep 2026: `GETPROPERTY`'s two output
> pointers were the only nested ones in the file left unchecked, and the ioctl
> requires no privileges, so any client could pass a kernel address and make
> the kernel write there.

And none of them requires alignment: Linux reads and writes nested arrays with
`get_user`/`put_user`/`copy_to_user`, which take any address, and here
`uget`/`uput`/`ucopy_out` do it (`read_unaligned`/`write_unaligned`, next to
`ucheck_n`). Until 20 Oct 2026 the 25 nested arrays (`SETCRTC`'s connectors,
`DIRTYFB`'s clips, the lease ids and lessees, `GETCONNECTOR`'s encoder and
properties, `GETPLANE`'s formats, `GETPROPERTY`'s values and enums, the id and
property lists, the syncobj handles and points and the four `ATOMIC` arrays)
were read with a direct deref (`*(ptr as *const T).add(i)`): with an odd pointer
that is UB, and with debug assertions an abort of the whole kernel.

### Dispatch by ioctl number

Linux never dispatches on the encoded command: `drm_ioctl()` takes
`nr = _IOC_NR(cmd)`, looks up the handler by that number and then
**reconciles** the size (`drm_ioctl_kernel`: reserve `max(in, out, drv)`, copy
what the client sent, zero-fill the rest, run, return `out_size`). That is what
keeps a 2019 libdrm working with a 2026 kernel. Eclipse does the same since
20 Sep 2026: comparing the full command turned every structure that grew a field
into an unknown ioctl, and it cost three hand patches (the deadline sizes of
`SYNCOBJ_WAIT`/`TIMELINE_WAIT`, `SYNCOBJ_HANDLE_TO_FD` at 24 bytes and PRIME's).
An unknown core ioctl number answers **EINVAL**, like Linux, not ENOSYS.

### Ownership of handles and framebuffers

Linux keeps the GEM handle table in an idr **per open file** and the
framebuffer list in `file_priv->fbs`, and resolves every uAPI id against the
calling file (`drm_gem_object_lookup(file, handle)`). Here the tables are global
and the substitute is the owner's pid, checked at the same entry points: PRIME
export, `mmap` of a driver object, `ADDFB`/`ADDFB2`, `RMFB`/`CLOSEFB`; and the
handle returned by `GETFB`/`GETFB2` is resolved to one of the caller's pid. The
kernel's internal paths (pid 0) and the presentation path still check nothing,
which is the same separation Linux makes.

It is **weaker** than Linux in one respect: two fds of the same process are not
isolated from each other. It closes the cross-process paths, which were the
serious ones.

## DRM UAPI coverage (`drm-uapi.rst`)

Legend: ✅ implemented · 🟡 partial / deliberate no-op · ❌ not implemented.

### Generic and authentication

| ioctl | Status | Notes |
|---|---|---|
| `DRM_IOCTL_VERSION` | ✅ | name `zcore`, version 1.0.0 (`nouveau` 1.4.0 with the nouveau uAPI, `eclipse-compute` 0.1.0 on the compute node); `strlen` lengths without NUL, like `drm_copy_field` |
| `DRM_IOCTL_GET_UNIQUE` | ✅ | the master's *busid* as in `drm_getunique`: empty (length 0) until `SET_VERSION` asks for interface 1.1 or later (`drm_set_busid`), then `pci:DDDD:BB:SS.F` of the PCI device named by the node's `device` link in sysfs (`drm_pci_set_busid`; `zcore-gpu` for a node with no PCI behind it); copied only if it fits whole, without NUL, and a `unique_len` with a null pointer is EFAULT. It goes away with the master (`DROP_MASTER` or close). Xorg parses it to find the node's `pci_device` and pick the primary GPU |
| `DRM_IOCTL_SET_VERSION` | ✅ | interface 1.4 and the driver version VERSION reports; validates the requested pair (major and minor) like `drm_setversion` (used by Xorg/modesetting); an accepted 1.1+ interface sets the *busid* GET_UNIQUE returns; it is `DRM_MASTER` (with `drm.master_check`, EACCES to a non-master) |
| `DRM_IOCTL_GET_MAGIC` / `AUTH_MAGIC` | ✅ | per-file magic, minted once; `AUTH_MAGIC` only from the master (EACCES), with a magic of that node (EINVAL) and only once; it authenticates the magic's owner (`drm_file.authenticated`, which whoever takes master and every root `open` also have, as in `drm_file_alloc`) |
| `DRM_IOCTL_SET_MASTER` / `DROP_MASTER` | ✅ | one master per node: the first `open` takes it (`drm_master_open`), `SET_MASTER` with another file as master is EBUSY, `DROP_MASTER` without being master is EINVAL; with `drm.master_check`, before that, `drm_master_check_perm`: EACCES to an fd that was never master, or that was but is called by a process other than the one that opened it, except root; released when the fd closes; switches the kernel text console (KD_GRAPHICS/KD_TEXT) |
| `DRM_IOCTL_GET_CLIENT` / `GET_STATS` / `FINISH` / `SET_UNIQUE` / `BLOCK` / `UNBLOCK` / `ADD_DRAW` / `RM_DRAW` / `UPDATE_DRAW` / `MODE_ATTACHMODE` / `MODE_DETACHMODE` | ✅ | the hollow ones in `drm_ioctl.c`: `GET_CLIENT` only answers `idx == 0` (with `auth` = `authenticated`, the caller's pid, `uid` = *overflowuid* and magic/iocs at 0; any other idx is EINVAL, which stops libva's enumeration), `GET_STATS` zeroes the struct, `FINISH` is a `DRM_AUTH` no-op (EACCES when unauthenticated), `SET_UNIQUE` is `drm_invalid_op` and `BLOCK`…`UPDATE_DRAW` no-ops, all `DRM_AUTH \| DRM_MASTER \| DRM_ROOT_ONLY` (EACCES without `CAP_SYS_ADMIN` or unauthenticated, and with `drm.master_check` also to a non-master, see below), `ATTACHMODE`/`DETACHMODE` no-ops |
| `DRM_IOCTL_GET_CAP` | ✅ | see the caps table |
| `DRM_IOCTL_SET_CLIENT_CAP` | ✅ | `ATOMIC` depending on `drm.atomic` (see the atomic section), and as in `drm_setclientcap` its value is stored at once in `atomic`, `universal_planes` and `aspect_ratio_allowed` ("No atomic user-space blows up on aspect ratio mode data"): an atomic client receives and returns aspect codes without asking for `ASPECT_RATIO`, and `ATOMIC` 0 removes them; `WRITEBACK` only for atomic clients (EINVAL like Linux); `UNIVERSAL_PLANES` and `ASPECT_RATIO` are booleans remembered per open; `STEREO_3D` accepted; `CURSOR_PLANE_HOTSPOT` EOPNOTSUPP |
| `DRM_IOCTL_WAIT_VBLANK` | 🟡 | synthetic ~60 Hz vblank. Event mode: honours `RELATIVE`/`ABSOLUTE`/`NEXTONMISS` and delivers `DRM_EVENT_VBLANK` when the counter reaches the requested sequence (never before the next vblank). Blocking mode: really waits, up to Linux's 3 s cap (`sys_ioctl` sleeps before the synchronous arm; see `README-async-ioctl-vblank.md`); if the cap stops the wait before the requested vblank, the reply carries the current counter and the code is EBUSY (`drm_wait_vblank_ioctl`), not a 0 the client would take as its vblank. With the CRTC off (DPMS off, SETCRTC without a mode, RMFB of the scanout: what `drm_crtc_vblank_off` turns off) both forms are EINVAL (`drm_vblank_get`), after the type and pipe checks; a blocking wait that was parked when it turns off ends at once with 0 and the current counter (`wait_event_interruptible_timeout` ends with `!vblank->enabled`), and event-mode events waiting for their vblank are delivered at once with the current counter (`drm_crtc_vblank_off`: "Sending premature vblank event on disable"; flip events stay, since only `vblank_event_list` is walked) |
| `DRM_IOCTL_CRTC_GET_SEQUENCE` / `CRTC_QUEUE_SEQUENCE` | ✅ | `GET_SEQUENCE`: unknown CRTC ENOENT; `active` is GETCRTC's `mode_valid` (`crtc_state->enable`), `sequence` the same counter as `WAIT_VBLANK` and `sequence_ns` the time of the vblank it counts (`drm_vblank_count_and_time`). `QUEUE_SEQUENCE`: CRTC (ENOENT) before flags (outside `RELATIVE \| NEXT_ON_MISS`, EINVAL); the target is resolved like `WAIT_VBLANK`'s event mode and the reply is the sequence it was queued at (the current one if it already passed); the `DRM_EVENT_CRTC_SEQUENCE` (`user_data`, `time_ns`, 64-bit `sequence`) arrives with that vblank and never before the next one (Linux sends it at once if it already passed). Xorg (`ms_get_crtc_ust_msc`, `ms_queue_vblank`) probes them at start-up; without them it fell back to `drmWaitVBlank`. With the CRTC off both are EINVAL (`drm_crtc_vblank_get`), `GET_SEQUENCE` after its ENOENT and `QUEUE_SEQUENCE` after ENOENT and flags: `active = 0` with the counter is not an answer Linux gives (`disable_outputs` requires the driver to have called `drm_crtc_vblank_off`); a `QUEUE_SEQUENCE` waiting for its vblank is delivered at once when the CRTC turns off, with the current counter, like event-mode vblank |

### GEM / *dumb buffers* / PRIME

| ioctl | Status | Notes |
|---|---|---|
| `DRM_IOCTL_MODE_CREATE_DUMB` | ✅ | physically contiguous memory via a VMO; *pitch* = `ceil(bpp/8) * width` aligned to 64 B; `size` = `pitch * height` rounded up to a page, like every `dumb_create` (a GEM object is whole pages); width, height or bpp of 0 are EINVAL as in `drm_mode_create_dumb`. The `mmap` shares the buffer's VMO (cached, WB): the mapping and every framebuffer built on the handle keep the memory alive after `DESTROY_DUMB`, like `drm_gem_object`'s refcount |
| `DRM_IOCTL_MODE_MAP_DUMB` | ✅ | *offset* = `handle << 12`; `mmap` maps the physical VMO |
| `DRM_IOCTL_MODE_DESTROY_DUMB` | ✅ | |
| `DRM_IOCTL_GEM_CLOSE` | ✅ | |
| `DRM_IOCTL_PRIME_HANDLE_TO_FD` / `FD_TO_HANDLE` | ✅ | real dma-buf (a process fd); dispatched in the syscall layer because it needs the fd table. Export looks the handle up in the file first (`ENOENT` if nobody created it or it belongs to another process, like `drm_gem_object_lookup`) and rejects with `EPERM` a nouveau object created `NO_SHARE` (`nouveau_gem_prime_export`), whether or not it has a host address. **A VRAM-only object exists but cannot be exported from here** (the dma-buf is built on a host physical address and it has none; Linux would export it): `EINVAL`, which is what every export gave before, and not `ENOENT`; the driver (`gem_aperture`) is the one that tells "exists without an address" from "does not exist", and it is asked **on the card of the node the file was opened on and on behalf of the caller** (`drm_gem_object_lookup(file_priv, handle)`: a handle names an object of ONE file on ONE card): another process's VRAM object, or one only the other card has, is `ENOENT` (before, the primary card was asked and by bare handle: `EINVAL` for someone else's, `ENOENT` for `card1`'s through its own node). Import returns the handle the process already has on that same buffer (its own, when reimporting what it exported, or that of an earlier import), like `drm_prime_lookup_buf_handle`, whose table export and import fill; before, every import of a generic dma-buf created a new handle (a nouveau object's already went back to the original). Linux's table is per file and here handles are per process. The dma-buf fd has `DMA_BUF_IOCTL_SYNC`, `SET_NAME`, and `EXPORT_SYNC_FILE` / `IMPORT_SYNC_FILE` (also in the syscall layer): flags only READ/WRITE and at least one (`EINVAL`), export is a close-on-exec `sync_file` with the buffer's fences (the driver's ring probes and whatever was imported, per `dma_resv_usage_rw`), import requires a `sync_file` (`EINVAL` otherwise) and its fence is waited on by `CPU_PREP`, the legacy present and the next export. Both used to be `ENOTTY` |
| `DRM_IOCTL_GEM_FLINK` / `GEM_OPEN` | 🟡 | `drm_gem_flink_ioctl` / `drm_gem_open_ioctl`, both `DRM_AUTH` (EACCES when unauthenticated): `FLINK` names (from 1, always the same) a handle of the caller, dumb/PRIME or nouveau (ENOENT for anyone else's); `OPEN` returns the handle that process already has on the object or a new one (like PRIME; for nouveau the global handle with one more reference, like `GETFB`) and its `size`; the name dies with the last handle (`GEM_CLOSE`, process exit); ENOENT for a name nobody minted or whose object no longer exists. An insecure legacy interface by design (any authenticated open opens any name): new clients should use PRIME, as `drm-uapi.rst` recommends |

### Framebuffers

| ioctl | Status | Notes |
|---|---|---|
| `DRM_IOCTL_MODE_ADDFB` | ✅ | the fb id comes from the same counter as property blobs (from 30000, above every KMS id and the EDID range), like Linux's single idr: no fb shares a number with a CRTC, a plane, a property or a blob |
| `DRM_IOCTL_MODE_ADDFB2` | ✅ | `framebuffer_check` in its order for the only plane here (linear XR24/AR24): flags outside `INTERLACED \| MODIFIERS` EINVAL, `MODIFIERS` EINVAL (unsupported), unknown or non-scanout format EINVAL, width or height 0 EINVAL, no handle EINVAL, a pixel row that does not fit in u32 or a plane whose last byte goes past 4 GiB **ERANGE**, pitch smaller than the row EINVAL, modifier without the flag EINVAL; for planes the format does not have only the modifier is checked (handle, pitch and offset only count under `MODIFIERS`). Local: `offsets[0]` must be 0 and there is no `max_width` (an fb bigger than the panel is cropped) |
| `DRM_IOCTL_MODE_RMFB` | ✅ | turns off the CRTC that was showing the fb, like `drm_framebuffer_remove` (process exit keeps the last image for console restore) |
| `DRM_IOCTL_MODE_CLOSEFB` | ✅ | Linux 6.6+: drops the reference without turning off the plane; a non-zero `pad` is EINVAL before looking up the fb, and an id that is not the file's ENOENT (`drm_mode_closefb_ioctl`) |
| `DRM_IOCTL_MODE_GETFB` | ✅ | geometry for everyone; the handle only to the node's master or to root (`drm_mode_getfb`: "GET_FB() is an unprivileged ioctl so we must not return a buffer-handle to non-master processes!"), and as a handle of the caller (`create_handle`: its own if it already has one, otherwise a new one on the same object: a dumb shares the `VmObject` like a PRIME self-import, a nouveau object adds the caller as a holder) |
| `DRM_IOCTL_MODE_GETFB2` | ✅ | `XR24` format, plane 0; the handle with the same rule as `GETFB` |
| `DRM_IOCTL_MODE_DIRTYFB` | ✅ | blits the union of the `drm_clip_rect`s (without clips, or if over the limit, rescans everything); horizontal edges are expanded to 64 B WC lines so neighbouring pixels are not smudged. `drm_mode_dirtyfb_ioctl`'s checks in their order: unknown fb ENOENT; count without pointer or pointer without count EINVAL; flags are **masked** to `ANNOTATE_COPY \| ANNOTATE_FILL` (a bit Linux does not define is ignored, not EINVAL); `ANNOTATE_COPY` with an odd count EINVAL; more than 256 clips EINVAL; and the list is read **whole** before looking at a clip (`copy_from_user`): one the kernel cannot read is EFAULT whatever the count |

### KMS (legacy modeset)

| ioctl | Status | Notes |
|---|---|---|
| `DRM_IOCTL_MODE_GETRESOURCES` | ✅ | 1 synthetic CRTC + 1 connector + 1 encoder. The lists follow Linux's two-call pattern: copied as far as `count` allows (`GETCONNECTOR`'s encoders and modes and `GETPLANE`'s formats, only if the whole list fits) and the total comes back; a `count` with a null pointer is EFAULT (`put_user`), not a skip |
| `DRM_IOCTL_MODE_GETCRTC` / `SETCRTC` | ✅ | `SETCRTC` with a mode scans out the fb, and with a mode and an empty connector list it is EINVAL ("Count connectors is 0 but mode set", after the viewport and before reading the list; each connector in the list is looked up by id, `drm_connector_lookup`); without a mode it turns off the CRTC and clears the mode (the fb it names is ignored, as in `drm_mode_setcrtc`, and the empty list is its own); `GETCRTC` returns the current mode (`mode_valid=1`) only while the CRTC has a mode (`crtc_state->enable`): after a `SETCRTC` without a mode or an `RMFB` of the on-screen fb it answers `mode_valid=0` and `fb_id=0` until the next modeset; DPMS off does not clear it. A `SETCRTC` with a mode is also a commit of the state the atomic uAPI reads (`__drm_atomic_helper_set_config`): `ACTIVE=1`, `MODE_ID` with a kernel blob holding that mode (the same id on every modeset), the connector routed to the CRTC and the primary plane covering the CRTC from the fb's `(x, y)` (`CRTC_X/Y=0`, `CRTC_W/H` = active area, `SRC_X/Y = x<<16, y<<16`, `SRC_W/H` = active area `<<16`); without a mode, and on `RMFB` of the on-screen fb (`atomic_remove_fb`), the mode and `ACTIVE` go away and the connector is unhooked, but the plane's rectangles stay as they were. `GETCRTC` returns `x`/`y` = the primary plane's `SRC_X>>16`/`SRC_Y>>16`, like `drm_mode_getcrtc` on an atomic driver, and the mode is the one the last `SETCRTC` set (`crtc_state->mode`, the `MODE_ID` blob) as `drm_mode_convert_to_umode` returns it: the client's timings, `vrefresh` recomputed with `drm_mode_vrefresh` (×2 with `INTERLACE`, ÷2 with `DBLSCAN`, ÷`vscan`, rounded to nearest), `type & DRM_MODE_TYPE_ALL`, the name at 31 bytes and NUL-padded (`strscpy_pad`) and the aspect bits only if that open set `DRM_CLIENT_CAP_ASPECT_RATIO` (`drm_file.aspect_ratio_allowed`, per open); only the boot console, which no modeset set, returns the native timings. The mode goes through `drm_mode_validate_basic`: a flag outside `DRM_MODE_FLAG_ALL` (`BCAST`, `PIXMUX`, bits above the aspect field) or a 3D layout beyond `SIDE_BY_SIDE_HALF` is EINVAL. An `x` or `y` with bits above bit 16 is ERANGE before anything else ("Universal plane src offsets are only 16.16": before the CRTC and the fb, with or without a mode), and a `clock` or `vrefresh` greater than `INT_MAX` is ERANGE (`drm_mode_convert_umode`), after the fb and before the aspect code. An aspect code other than NONE without `DRM_CLIENT_CAP_ASPECT_RATIO` is EINVAL ("Unexpected aspect-ratio flag bits"), after the fb and before `drm_mode_convert_umode` |
| `DRM_IOCTL_MODE_GETENCODER` | ✅ | `VIRTUAL` encoder, `possible_crtcs=1`; `crtc_id=0` while the CRTC has no mode |
| `DRM_IOCTL_MODE_GETCONNECTOR` | ✅ | 1 mode = native resolution (preferred); standard properties (see below); `encoder_id=0` while the CRTC has no mode |
| `DRM_IOCTL_MODE_GETPLANERESOURCES` | ✅ | 1 primary plane |
| `DRM_IOCTL_MODE_GETPLANE` | ✅ | `XR24`/`AR24` formats; `crtc_id` and `fb_id` are the CRTC and framebuffer the primary plane shows (like `plane->state->crtc`/`->fb`): 0 and 0 with the CRTC turned off by `SETCRTC` without a mode or `RMFB` of the scanout, unchanged with DPMS off, and the fb follows each flip; `gamma_size` goes back to 0 after the lookup (`drm_mode_getplane`: "FIXME: Plane gamma"), not whatever value the client brought, and with ENOENT it is left untouched; the driver's own fb id never comes out |
| `DRM_IOCTL_MODE_SETPLANE` | ✅ | equivalent to scanning out the fb (SW primary path); writes the plane's eight rectangles as they arrive (`drm_atomic_helper_update_plane`), which `GETCRTC` (`x`/`y`) and the plane properties read afterwards; a CRTC rectangle that overflows `int` (`crtc_w`/`crtc_h` > INT_MAX, or `crtc_x + crtc_w` / `crtc_y + crtc_h` > INT_MAX) → **ERANGE**, after the CRTC mask and before the source rectangle (`__setplane_check`); with `fb_id=0` it does nothing (Linux disables the plane, and if it is the primary each driver decides whether it allows that with the CRTC on) |
| `DRM_IOCTL_MODE_PAGE_FLIP` | ✅ | scanout; `DRM_EVENT_FLIP_COMPLETE` (with `crtc_id` and the vblank sequence) only with `PAGE_FLIP_EVENT`; `ASYNC`/`TARGET_*`/unknown flags → EINVAL, unknown fb → ENOENT; the new fb must contain the primary plane's source rectangle (`drm_framebuffer_check_src_coords` with `SRC_X/Y/W/H`, ENOSPC: a viewport shifted by `SETCRTC`/`SETPLANE`/ATOMIC counts), and the mode only measures when the plane has never been positioned (here you can start with a flip; Linux gives EBUSY) |
| `DRM_IOCTL_MODE_OBJ_GETPROPERTIES` | ✅ | per-object property table; `DRM_MODE_PROP_ATOMIC` properties are only shown to atomic clients (same filtering as Linux). An fb, a blob or a property as `obj_id` is EINVAL (`drm_mode_object_find` finds them in the same idr, with `ANY` or their own type, and they have no table: `!obj->properties`); with another type, ENOENT, like any lookup; an id that is nothing is still ENOENT. The synthetic encoder, which `GETRESOURCES` names, answers an empty list where Linux would say EINVAL |
| `DRM_IOCTL_MODE_GETPROPERTY` | ✅ | full metadata: flags, name, ranges/enums/object type; like `drm_mode_getproperty_ioctl`, writes the first `count_values` values and the first `count_enum_blobs` enums (a null array with room is EFAULT), values before enums, and returns the totals; `count_enum_blobs` is only written for enum/bitmask (the total) and blob (0): range or object leave the client's. Before, it filled each array all or nothing, skipped a null one and set `count_enum_blobs` to 0 for every property |
| `DRM_IOCTL_MODE_OBJ_SETPROPERTY` / `SETPROPERTY` | ✅ | **DPMS does act** (`drm_atomic_connector_commit_dpms`): any value other than "On" turns the CRTC off (black screen, and the cursor and damage stop being repainted), "On" turns it on and shows the primary plane's fb again without waiting for the next frame, and the property reads back with the value set; with the connector routed to a CRTC (mode set) the CRTC's `ACTIVE` follows DPMS (off = `ACTIVE 0`, on = `ACTIVE 1`, and an ATOMIC that changes `ACTIVE` afterwards needs `ALLOW_MODESET`), with no CRTC routed only the connector changes (and here the panel too: the boot console is not in the atomic state). An ATOMIC with `ACTIVE 0 → 1` without `FB_ID` also shows the plane's fb again. The rest is `set_property_atomic` as in `drm_mode_obj_set_property_ioctl`: with `drm.atomic` an atomic commit of that single property, without event and without `ALLOW_MODESET` (an `FB_ID` on the primary plane is a flip; `ACTIVE`/`MODE_ID` that change the state are EINVAL; an `OUT_FENCE_PTR` writes -1 and there is no fence, which only the ATOMIC ioctl creates); without `drm.atomic` it is `set_property_legacy`, and this KMS has `set_property` only for DPMS: EINVAL. An fb, a blob or a property as `obj_id` is EINVAL (`!obj->properties`), not ENOENT, with `ANY` or its own type; with another type, ENOENT |
| `DRM_IOCTL_MODE_GETPROPBLOB` | ✅ | EDID + blobs from the property store. Like `drm_mode_getblob_ioctl`: the bytes are copied only if `length` is **exactly** the blob's (the two-call pattern of `drmModeGetPropertyBlob`), with EFAULT if the pointer is invalid; `length` always comes back with the blob's |
| `DRM_IOCTL_MODE_CREATEPROPBLOB` / `DESTROYPROPBLOB` | ✅ | blob store; `length` 0 (or more than 64 KiB, a local cap) EINVAL before looking at the pointer, and a null or invalid pointer with a length EFAULT (`copy_from_user`), not EINVAL; destroying a blob of another file or of the kernel is EPERM (`drm_mode_destroyblob_ioctl`: "ensure the property was actually created by this user"), not EACCES |
| `DRM_IOCTL_MODE_CURSOR` / `CURSOR2` | ✅ | cursor composited by the kernel on every frame (`set_cursor_bo`/`move_cursor`); no flag or an unknown one → EINVAL before looking up the CRTC (ENOENT), like `drm_mode_cursor_common`; a handle with width or height 0, or larger than 64×64 (the size advertised in the caps) → EINVAL; only handle 0 hides the pointer |
| `DRM_IOCTL_MODE_ATOMIC` | ✅ | **opt-in** with `drm.atomic` (see the next section) |
| `DRM_IOCTL_MODE_GETGAMMA` / `SETGAMMA` | 🟡 | accepted as no-ops (`gamma_size=0`, no `GAMMA_LUT`); returning ENOTTY caused a SETGAMMA storm from Xorg |
| `DRM_IOCTL_MODE_CREATE_LEASE` / `LIST_LESSEES` / `REVOKE_LEASE` | 🟡 | all three are `DRM_MASTER` (EACCES to a non-master). `CREATE_LEASE` validates like `drm_mode_create_lease_ioctl`: `flags` outside `O_CLOEXEC \| O_NONBLOCK` → EINVAL; with `object_count` the list is read (EFAULT) and each id must be a CRTC, connector or plane (ENOENT if it does not exist, EINVAL if it is an encoder), the set carries at least one CRTC and one connector, and a plane if the client asked for `UNIVERSAL_PLANES` (EINVAL); without universal planes each CRTC drags its primary and cursor planes along, and a repeated id in the set is ENOSPC (`idr_alloc`); an object another live lessee of this node still holds is EBUSY (`drm_lease_create` → `_drm_has_leased`), and once revoked it is free again. The `lessee_id` is the lowest free one from 1 per node. **The fd restricts nothing**: it is a `dup` of the lessor's fd, so a "leased" client keeps full control of the device. `LIST_LESSEES` (`pad` → EINVAL) lists the leases with objects, as many as fit and the total in `count_lessees`; `REVOKE_LEASE` empties one (ENOENT if it does not exist; repeating it is a no-op). `GET_LEASE` does not exist |
| `DRM_IOCTL_SYNCOBJ_*` | 🟡 | implemented (`CREATE`/`DESTROY`/`WAIT`/`RESET`/`SIGNAL`/`TIMELINE_*`/`QUERY`/`TRANSFER`, `drivers/src/scheme/syncobj.rs`) but only active with `nvidia.nouveau_uapi`; without it EOPNOTSUPP and `DRM_CAP_SYNCOBJ=0`. `TRANSFER` reads its request in the order of `drm_syncobj_transfer_ioctl`: `pad` → EINVAL, unknown destination → ENOENT, a flag outside `WAIT_FOR_SUBMIT` → EINVAL, unknown source → ENOENT; without `WAIT_FOR_SUBMIT` the source must already carry the requested point (EINVAL, `dma_fence_chain_find_seqno`) |

### Standard KMS properties (`drm-kms.rst` → *KMS Properties*)

| Object | Property | Type | Notes |
|---|---|---|---|
| plane | `type` | immutable enum | {Overlay, Primary, Cursor} |
| plane | `FB_ID`, `CRTC_ID` | object, atomic | |
| plane | `CRTC_X/Y/W/H`, `SRC_X/Y/W/H` | range, atomic | `SRC_*` in 16.16 |
| CRTC | `ACTIVE` | range 0–1, atomic | |
| CRTC | `MODE_ID` | blob, atomic | `drm_mode_modeinfo` blob (68 B) |
| connector | `DPMS` | enum | "On" or "Off" depending on whether the CRTC is on; writing it turns the CRTC off or on and moves its `ACTIVE` if routed (`OBJ_SETPROPERTY` row) |
| connector | `link-status` | enum | always "Good" |
| connector | `non-desktop` | immutable range | 0 |
| connector | `EDID` | immutable blob | real EDID (UEFI/DDC) if there is one |
| connector | `CRTC_ID` | object, atomic | |

### Capabilities (`DRM_IOCTL_GET_CAP`)

| Capability | Value | Notes |
|---|---|---|
| `DRM_CAP_DUMB_BUFFER` | 1 | |
| `DRM_CAP_DUMB_PREFERRED_DEPTH` | 24 | XRGB8888 scanout |
| `DRM_CAP_DUMB_PREFER_SHADOW` | 1 | the present is a CPU blit over PCIe: rendering to a shadow and copying is exactly what is advisable |
| `DRM_CAP_PRIME` | 3 | IMPORT \| EXPORT (real dma-buf) |
| `DRM_CAP_TIMESTAMP_MONOTONIC` | 1 | |
| `DRM_CAP_ASYNC_PAGE_FLIP` | 0 | |
| `DRM_CAP_CURSOR_WIDTH` / `HEIGHT` | 64 | cursor composited by the kernel |
| `DRM_CAP_ADDFB2_MODIFIERS` | 0 | on purpose: with 1, wlroots tried tiled modifiers the software scanout does not understand |
| `DRM_CAP_PAGE_FLIP_TARGET` | 0 | |
| `DRM_CAP_CRTC_IN_VBLANK_EVENT` | 1 | the flip event carries `crtc_id` |
| `DRM_CAP_SYNCOBJ` / `SYNCOBJ_TIMELINE` | 0 or 1 | 1 with `nvidia.nouveau_uapi`, 0 without it — like the ioctls, which answer EOPNOTSUPP without the flag |
| `DRM_CAP_ATOMIC_ASYNC_PAGE_FLIP` | 0 | |

## Atomic KMS (`drm-uapi.rst` → *Atomic Mode Setting*) — opt-in

The full atomic path is implemented for the synthetic pipeline (software KMS):
negotiation with `DRM_CLIENT_CAP_ATOMIC`, atomic properties on the three
objects, mode blobs (`CREATEPROPBLOB`/`MODE_ID`) and `DRM_IOCTL_MODE_ATOMIC`
with Linux's semantics:

- `TEST_ONLY` validates without touching state (and `TEST_ONLY`+`PAGE_FLIP_EVENT`
  is EINVAL, as in Linux).
- `ALLOW_MODESET` is mandatory for `MODE_ID`/`ACTIVE` changes ("requires full
  modeset").
- The CRTC and its connector go together, on the state the commit **leaves** the
  CRTC in (`drm_atomic_helper_check_modeset`, "enabled/connectors mismatch"): a
  mode is set with the connector's `CRTC_ID` in the same commit and cleared with
  it set to 0; a mode without a connector, a connector without a mode or a
  connector `CRTC_ID = 0` with the mode set are EINVAL. `ACTIVE` without a mode
  in the resulting state is EINVAL whether the commit names `ACTIVE` or
  `MODE_ID = 0` (`drm_atomic_crtc_check`, "active without enabled"). The
  connector's `CRTC_ID` reads back while there is a mode, active or not.
- `PAGE_FLIP_EVENT` queues one `DRM_EVENT_FLIP_COMPLETE` per CRTC in the commit.
- `PAGE_FLIP_ASYNC` is rejected (the async caps are 0).
- Unknown object/property → ENOENT; a value the property does not accept →
  EINVAL **on the property**, before the commit, like
  `drm_property_change_valid_get`: an `FB_ID`/`CRTC_ID` that names nothing or
  exceeds 32 bits, a `MODE_ID`/`FB_DAMAGE_CLIPS` without a blob, a range or enum
  outside what is advertised (`IN_FENCE_FD` is `[-1, INT_MAX]`).
- The plane check (`drm_atomic_plane_check`) looks at the state the commit
  **leaves** the plane in, not only at what the commit names: CRTC and FB both
  set or both cleared (an `FB_ID = 0` or a `CRTC_ID = 0` on its own is EINVAL and
  applies nothing; the plane is turned off and on with both in the same commit),
  the 16.16 source rectangle inside the fb it ends up with
  (`src_w > width << 16 || src_x > (width << 16) - src_w`, a fractional edge
  counts) → **ENOSPC**, and each `FB_DAMAGE_CLIPS` clip a real rectangle from the
  origin (EINVAL; the far edge is compared with the fb size **shifted by 16
  bits**, as Linux does, so a clip that overshoots in pixels is accepted and
  clipped on present); a clip blob that is not a whole number of rectangles is
  EINVAL when staged. On a plane that ends up off nothing else is checked. A
  CRTC rectangle that overflows `int` (`crtc_x + crtc_w` or `crtc_y + crtc_h` >
  INT_MAX; `crtc_w` and `crtc_h` alone are already stopped by the property) →
  **ERANGE** (`FsError::OutOfRange`), before the source rectangle, like
  `drm_atomic_plane_check`.
- Committing an `FB_ID` performs the scanout (the same blit as the legacy path).

**How to enable it**: it is strictly **opt-in** while the legacy path is the one
tested on real hardware — the same rollout nouveau did with
`nouveau.atomic=1`. Boot with the `drm.atomic` flag on the kernel `cmdline`
(`zCore/rboot.conf`):

```ini
cmdline=LOG=warn:drm.atomic:SHELL=/bin/busybox?sh
```

and remove `WLR_DRM_NO_ATOMIC=1` from the environment so wlroots uses the
atomic path. Without the flag, `SET_CLIENT_CAP(ATOMIC)` returns **EOPNOTSUPP**
(like a Linux driver without `DRIVER_ATOMIC`) and compositors fall back to the
legacy path exactly as before.

`IN_FENCE_FD` and `OUT_FENCE_PTR` **do exist**: the first really waits for the
client's fence before presenting (with a 100 ms cap, after which it presents
anyway), and the second is a real fence, as in Linux: the client's slot receives
`-1` as soon as the property is staged (`put_user(-1, fence_ptr)`), the
`sync_file` is created and installed in its fd table **before** the commit
(`prepare_signaling`; a failing commit removes it and leaves the `-1`), and its
fence **is signalled when the flip completion is delivered**, at the synthetic
vblank, with the event if `PAGE_FLIP_EVENT` was requested and on its own if not.
A commit with only `OUT_FENCE_PTR` owes that completion just like one with an
event (and is rejected the same way on a CRTC that is off and stays off). It
used to be an **already signalled** `sync_file`: whoever released the previous
buffer on seeing it signed did so with that buffer still on screen.

A failing commit leaves nothing half done: the KMS state, the CRTC's
framebuffer and the kernel's mode blob are restored, as
`drm_atomic_helper_swap_state` does, which only swaps the duplicated state when
the check and the commit have gone well. `ACTIVE=0` turns the CRTC off.

Deliberate limitations: a single mode (the panel's native one), no atomic
cursor plane (the legacy cursor is still composited by the kernel) and only on
the software-KMS path (with a hardware KMS driver atomic negotiation is
rejected).

## Waiting for the fence before presenting (implicit sync)

The atomic path waits for `IN_FENCE_FD` since it was written, but the **legacy**
path — `SETCRTC` and `PAGE_FLIP`, the one this machine really runs because
`drm.atomic` is opt-in — carried no fence and waited for nothing:
`present_now_checked` read the buffer the instant the ioctl arrived. A
compositor that submits its drawing and flips without a round trip through the
GPU put on screen whatever the GPU had finished up to that point, that is,
garbage in the bottom part of the frame.

Linux solves this case from the buffer's *reservation object*:
`drm_atomic_helper_prepare_planes` collects the fences hanging off the BO and
the commit waits for them before programming the flip. Here there is no
reservation object, and nouveau's `EXEC` **carries no buffer list**, so nothing
says *which* submissions touched this buffer. What can be known is which channel
its owner submits on and whether that channel has drained — the same question
`GEM_CPU_PREP` answers —, and it errs in the safe direction: it may wait for work
that did not touch the buffer, never skip work that did.

The owner is whoever **allocated** the GEM object, not whoever created the
framebuffer: with *direct scanout* the client draws into its own object and the
compositor imports it and does `ADDFB`, so the fb carries the compositor's pid
while the pixels still come from the client's channel.

Details:

- A **100 ms** cap, the same as the atomic wait and for the same reason:
  presenting a half-drawn frame is a visible defect, never presenting is a hang.
  Past the cap it presents anyway and warns on the console with a budget (8
  lines, then it goes quiet).
- It waits on the **asynchronous** path of the `ioctl` (`sys_ioctl`, before the
  synchronous arm), sleeping ~1 ms between polls, not spinning: spinning here
  eats a whole core, which is the starvation `WAIT_VBLANK` already caused.
- A `SETCRTC` with `fb_id=0` turns off the pipe and presents nothing, so it does
  not wait.
- **On by default.** The escape hatch is `drm.flip_fence=off` on the `cmdline`,
  which restores the previous behaviour (present at once, with tearing if the GPU
  is still drawing) without recompiling.
- **Every** registered driver is asked, not just the primary: on a two-GPU box
  the one drawing need not be the one scanning out (that is precisely the split
  this kernel runs, with the console GPU owning the panel and the compute one
  drawing).
- Without a registered NVIDIA driver the wait does not exist: the trait hook
  (`DrmScheme::render_fence_for_scanout`) returns `None` by default, so QEMU's
  software desktop behaves exactly as before.

## Known gaps and their justification

- **Gamma/CTM/HDR**: no LUTs (`gamma_size=0`, no `GAMMA_LUT`/`CTM`); the
  software scanout does not apply them. Visible consequence: gammastep, night
  light and RandR gamma do nothing.
- **Leases**: `CREATE_LEASE` validates the request like Linux but returns a
  `dup` of the fd without restricting anything, `GET_LEASE` does not exist, and
  leases are per node and last until shutdown (a `DROP_MASTER` does not clean
  them up). **Syncobj**: only with the nouveau UAPI (above).
- **Per-open state**: events and `DRM_CLIENT_CAP_ATOMIC` **are** per open fd
  (`DrmFileState`, which `sys_open` creates for every `open`). GEM handles,
  framebuffers and the cursor are still in global tables; for handles and
  framebuffers the owner's pid covers cross-process access (see above), but does
  not isolate two fds of the same process. The master **is** per fd and per node
  (`drm::set_master`/`drop_master`, taken by the first `open` as in
  `drm_master_open`), and **the ioctls Linux marks `DRM_MASTER` require it only
  with `drm.master_check`** (opt-in: EACCES to a non-master, before reading the
  argument, like `drm_ioctl_permit`; without the flag anyone can issue them), and
  with the same flag `SET_MASTER` and `DROP_MASTER` go through
  `drm_master_check_perm` (EACCES to whoever was not master from their own
  process and is not root, before the EBUSY or the EINVAL). `AUTH_MAGIC` and
  leases always require it. It is opt-in because turning it on blindly could
  leave without a screen a compositor that is not the first `open` of `card0`.
- **`drm-usage-stats.rst` (fdinfo)**. No usage/memory/engine statistics are
  exposed through `fdinfo`.
- **Render / 3D**. Without an NVIDIA GPU, Mesa's software rendering is used
  (`llvmpipe`/`softpipe`). With NVIDIA and `nvidia.nouveau_uapi`, Mesa uses NVK
  and zink on the GPU ([README-nouveau-uapi.en.md](README-nouveau-uapi.en.md)).

## What still diverges (audit of 20 Sep 2026)

In order of what breaks real clients. The items above are already fixed; these
are not:

| Divergence | What it breaks |
|---|---|
| ~~`SYNCOBJ_TRANSFER` loses the `dst_point` on the software path~~ | **Fixed:** `syncobj::transfer` carries the `dst_point` into the destination's dependency link (`drivers/src/scheme/syncobj.rs`), so a software timeline reaches the promised point and wlroots' `linux-drm-syncobj-v1` releases the client's frame |
| ~~A `sync_file` fd does not support `SYNC_IOC_MERGE`~~ | **Fixed:** `SyncobjHandle::merge` creates a syncobj that reaches 1 when ALL its sources do (`syncobj::merge_fences`, Linux's `dma_fence_array`). Mesa asks for it on every acquire of an already presented swapchain image (the compositor's release fence + the previous present's fence); without the ioctl the acquire failed after the first `image_count` frames and zink killed the GLX swapchain (`zink: swapchain killed`, then `GLXBadCurrentWindow`), with nothing in dmesg because the client's error is compiled out. Errors as in `sync_file_ioctl_merge`: `flags` or `pad` EINVAL, and an `fd2` that names no file (EBADF before) or is not a `sync_file` ENOENT (`sync_file_fdget`); the name is cut at 31 bytes and returned NUL-terminated |
| ~~Destroying a syncobj released its dependants at the promised point~~ | **Fixed:** a syncobj that links still depend on and that can make progress (HW fence in flight or its own link) stays as an orphan until nobody names it; before, Mesa's pattern (surrogate + transfer + export + immediate destroy) signalled the acquire semaphore before the compositor released the image |
| ~~`sync_file` fds cannot be polled~~ | **Fixed:** `SyncobjHandle::poll` reports `POLLIN` when the fence has reached the point; `subscribe_readiness` + the HW fence poller wake `sys_poll` when it lands (without that, Mesa's `sync_wait()` in GLX/DRI3 killed the swapchain: `zink: swapchain killed`) |
| ~~The `sync_file` fd had neither `SYNC_IOC_FILE_INFO` nor `SYNC_IOC_SET_DEADLINE`, and both fds (`sync_file` and syncobj) answered `ENOSYS` to `read`/`write` and claimed to be `O_RDWR`~~ | **Fixed:** both are anonymous inodes opened with flags 0, as in Linux: `O_RDONLY` (`read` EINVAL, `write` EBADF, `pread`/`pwrite`/`lseek` ESPIPE). The syncobj fd has neither `poll` nor `ioctl` (`poll(2)` always reports it ready, `epoll_ctl` EPERM, any ioctl ENOTTY, `SYNC_IOC_MERGE` included). The `sync_file` one has `SYNC_IOC_FILE_INFO` (status, name and the single fence, with driver `eclipse`, timeline `syncobj`, `-ENODEV` if the fence timed out, and the name `SYNC_IOC_MERGE` gave it) and `SYNC_IOC_SET_DEADLINE` (checks `pad` and does nothing). It is what libsync's `sync_file_info()` and Mesa's `sync_wait` query without polling |
| ~~`IN_FENCE_FD` with an fd that is not a `sync_file` (or does not exist) presented without waiting, a syncobj fd was taken as its current fence, and a second `IN_FENCE_FD` on the plane overwrote the first~~ | **Fixed:** as in `drm_atomic_plane_set_property`: the fd is resolved with `sync_file_get_fence`'s semantics (only a `sync_file`; anything else, EINVAL and the commit does not run) and a plane whose state already carries a fence does not accept another `IN_FENCE_FD`, not even -1 (EINVAL) |
| ~~`OUT_FENCE_PTR` returns an already signalled fence, and the client's slot is not touched if the commit never runs~~ | **Fixed:** the fence is signalled when the flip completion is delivered (`drm_send_event_helper`), with or without an event and even if the DRM fd has been closed; the slot receives `-1` when the property is staged and the fd only if the commit runs. Whoever paces frames with it, or hands it to a client as the release of its buffer, now waits until the previous buffer has left the screen |
| The synchronous arm of `SYNCOBJ_WAIT` can spin without yielding the CPU | A stuck kernel coroutine; the machine looks frozen |
| No per-object dma-buf cache nor per-file PRIME handle cache | GEM cap raised to 256; still no per-file PRIME dedup |
| ~~`ADDFB2` does not validate format, flags or modifiers~~ | **Fixed:** the whole `framebuffer_check` (row in the ioctl table); a tiled modifier is EINVAL |
| ~~`DIRTYFB`'s clip limit is 64 where Linux uses 256, and `ANNOTATE_COPY` is missing~~ | **Fixed:** 256 clips like Linux, `ANNOTATE_COPY` requires pairs, and the list is read whole or is EFAULT (row in the ioctl table) |
| ~~`DRM_MASTER` ioctls do not check for master (which does exist)~~ | **Fixed, opt-in:** with `drm.master_check` the nineteen ioctls `drm_ioctls[]` marks `DRM_MASTER` (the twelve modeset ones `SETCRTC`, `SETPLANE`, `CURSOR`/`CURSOR2`, `SETGAMMA`, `ATTACHMODE`/`DETACHMODE`, `SETPROPERTY`, `PAGE_FLIP`, `DIRTYFB`, `OBJ_SETPROPERTY`, `ATOMIC`; `SET_VERSION`; and the six hollow legacy ones `SET_UNIQUE`, `BLOCK`, `UNBLOCK`, `ADD_DRAW`, `RM_DRAW`, `UPDATE_DRAW`) are EACCES for a non-master, before reading the argument (`AUTH_MAGIC` and leases check it in their own arm, with or without the flag), and `SET_MASTER`/`DROP_MASTER` are EACCES (`drm_master_check_perm`) for an fd that was never master or that is called by a process other than the one that opened it, except root; without the flag they stay open to anyone |
| ~~`GET_CLIENT` does not exist~~ | **Fixed:** `GET_CLIENT` answers `idx == 0` (row in the ioctl table) |
| No `GAMMA_LUT`/`CTM` | gammastep, night light and RandR gamma do nothing |
| No `IN_FORMATS` on the plane | Harmless while `DRM_CAP_ADDFB2_MODIFIERS` is 0; mandatory the day it is set to 1 |
| ~~`CRTC_GET_SEQUENCE`/`QUEUE_SEQUENCE` do not exist~~ | **Fixed:** both exist on top of the synthetic counter (row in the ioctl table); Xorg no longer probes in vain or falls back to `drmWaitVBlank` |
| ~~The render node filter only observes~~ | Fixed: `renderD128` rejects modeset/dumb/master with EACCES |
| No `fdinfo` (`drm-usage-stats.rst`) | `nvtop` and the like see nothing |

## How to launch labwc

The kernel implements the **legacy KMS + dumb buffers + software scanout** path
(and, opt-in, the atomic one). For wlroots/labwc to use the default path (and NOT
try GBM/EGL/GL, which has no acceleration here) the **pixman** renderer and
legacy KMS have to be forced through environment variables (in the image the
`/usr/local/bin/labwc` wrapper already does this; this is for launching it by
hand):

```sh
# Seat manager (or start it in /etc/init.d/rcS). It gives access to
# /dev/dri/card0 and /dev/input/event* without logind.
seatd -g video &

# Runtime directory for the Wayland socket (mode 0700, owned by the user).
export XDG_RUNTIME_DIR=/run/user/0
mkdir -p "$XDG_RUNTIME_DIR" && chmod 0700 "$XDG_RUNTIME_DIR"

# Software rendering (no GBM/EGL) and legacy KMS (non-atomic, no modifiers).
export WLR_RENDERER=pixman
export WLR_DRM_NO_ATOMIC=1     # unnecessary without drm.atomic; remove it to try the atomic path
export WLR_DRM_NO_MODIFIERS=1

labwc
```

> **Important**: without `WLR_RENDERER=pixman`, wlroots first tries the GLES2
> renderer on GBM/EGL (Mesa). Without an accelerated GPU, that path may **hang,
> fail or run extremely slowly**: Mesa falls back to `llvmpipe` (CPU rendering)
> and every frame is then copied again to the DRM framebuffer in software. The
> typical symptom of the initialisation failure is that the kernel log stops at
> `[drm] VERSION …` and never reaches the scanout (frozen screen). If it does
> start with `WLR_RENDERER=gles2`, the compositor will still be very slow because
> of that double CPU work. Forcing pixman avoids that path entirely.

## Diagnosis

With `LOG=error`, the kernel logs the progress of the DRM negotiation. Careful:
almost all the lines below are `log::debug!`, so they are **not** seen with
`LOG=error`; `LOG=debug` is needed for the full trace. The ones that come out at
any level are the `klog_info!` ones (VERSION under nouveau, DROP_MASTER, the
syncobj cap, the render node, the first present and turning off the CRTC). A
healthy session (legacy path) prints, in order:

```
[drm] VERSION — /dev/dri/card0 opened by userspace (minor=0)
[drm] GET_CAP cap=0x1 -> 1
[drm] SET_CLIENT_CAP cap=2 -> accepted        # UNIVERSAL_PLANES
[drm] SET_CLIENT_CAP ATOMIC -> EOPNOTSUPP ... # without drm.atomic (we force legacy)
[drm] SET_MASTER (minor=0)
[drm] GETRESOURCES: software KMS -> 1 crtc, 1 connector ...
[drm] GETCONNECTOR id=2 connected=true modes=1 ...
[drm] CREATE_DUMB 1920x1080 bpp=32 -> handle=1 ...
[drm] SETCRTC crtc=1 fb=1 ...
[drm] scanout: fb=1 ... -> display ...
```

With `drm.atomic` and the atomic path active you also see:

```
[drm] SET_CLIENT_CAP ATOMIC=1 -> accepted
[drm] CREATEPROPBLOB len=68 -> blob=30000     # the compositor's MODE_ID
[drm] ATOMIC objs=3 test_only=true allow_modeset=true ...   # test commit
[drm] ATOMIC objs=3 test_only=false ... fb=Some(1) ...      # real modeset
```

- If it stops at `VERSION` (or `minor=128`, the render node) and `GETRESOURCES`
  never appears, it is the GL renderer's initialisation: use
  `WLR_RENDERER=pixman` and make sure of `WLR_DRM_DEVICES=/dev/dri/card0` (the
  render node now rejects modeset/dumb with EACCES).
- If it reaches `SETCRTC`/`scanout` but nothing is seen, the problem is in the
  blit to the framebuffer (see [`drm.rs`](../linux-object/src/fs/devfs/drm.rs)).
- `[drm] UNHANDLED ioctl …` indicates an ioctl labwc asks for that we do not
  handle yet (the `drm nr` identifies the `DRM_IOCTL_*`).

The kernel text console yields to graphics (`KD_GRAPHICS`) only on the first
real scanout, not on `SET_MASTER`: that way, if labwc gets stuck before painting,
the terminal stays usable and its logs visible. The mode is applied to the VT
the compositor claimed on its first present; if the user switches VT mid-frame,
the text VT keeps its mode and is repainted.

## How to test (Xorg)

- **Xorg**: the `fbdev` driver on `/dev/fb0`. See [`README-xorg.md`](README-xorg.md).

## `fbdev` (`/dev/fb0`, legacy framebuffer API)

| ioctl | Status |
|---|---|
| `FBIOGET_VSCREENINFO` / `FBIOGET_FSCREENINFO` | ✅ |
| `FBIOPUT_VSCREENINFO` | 🟡 (fixed resolution; returns the real one) |
| `FBIOPAN_DISPLAY` / `FBIOBLANK` | 🟡 (no-op) |
| `FBIOGETCMAP` / `FBIOPUTCMAP` | 🟡 (no-op; TrueColor) |

## File map

| File | Role |
|---|---|
| `linux-object/src/fs/devfs/drm_scheme.rs` | ioctl dispatch for `/dev/dri/card*` (incl. atomic, blobs, render node filter) |
| `linux-object/src/fs/devfs/drm.rs` | DRM core: GEM, framebuffers, scanout, events, synthetic KMS, atomic commit |
| `linux-object/src/fs/devfs/fbdev.rs` | `/dev/fb0` (legacy `fbdev` API) |
| `linux-object/src/fs/dmabuf.rs` | dma-buf (PRIME) |
| `drivers/src/scheme/drm.rs` | `DrmScheme` trait for drivers (virtio-gpu, nvidia) |
| `drivers/src/virtio/gpu.rs` | virtio-gpu driver |
| `linux-object/src/fs/sysfs.rs` | `/sys/class/drm/card0` |
