# ffnvcodec

Headers of NVIDIA's Video Codec SDK as ffmpeg carries them
(`nv-codec-headers`, tag `n13.0.19.0`, NVENC API 13.0), unchanged. They
are MIT licensed (the notice is at the top of each file). `nvcodec-probe.c`
is built against them and reaches every library through dlopen, so nothing
of NVIDIA's is linked.

The NVENC API version must not be newer than the one the RM's driver
version (`open-gpu-kernel-modules/version.mk`) serves: 580 serves 13.0.
