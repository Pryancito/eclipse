// Eclipse OS: a probe of the GPU's video engines through NVIDIA's own
// libraries: NVENC through libnvidia-encode, NVDEC through libnvcuvid, the
// two that ffmpeg's h264_nvenc / hevc_nvenc and -hwaccel cuda (and OBS,
// GStreamer's nvcodec, Jellyfin) go through.
//
// On each GPU (or the one -g names) it encodes a few moving frames with
// NVENC, decodes that stream back with NVDEC and checks the pictures that
// come out against the ones that went in:
//
//   cuda      cuInit, the GPU, a context: what both libraries run on.
//   encode    NvEncOpenEncodeSessionEx on the context, H.264 (and HEVC
//             when the GPU has it), constant QP, no B frames, from system
//             memory input buffers; frames per second and bytes out.
//   decode    cuvidGetDecoderCaps, then cuvidCreateVideoParser on that
//             stream, cuvidCreateDecoder from its sequence, every picture
//             decoded and mapped, its luma copied back and compared with
//             the frame encoded (PSNR, which must be 30 dB or more).
//
// A step that fails prints NVIDIA's error for it. When the library is not
// there at all, ECLIPSE_NVIDIA_USERSPACE=0 built the image, or it is older
// than this probe. `dmesg | grep -E 'nvidia|NVRM'` then says what the
// kernel refused, if it refused anything.
//
// Needs the kernel booted with `nvidia.cuda`. A glibc program like the
// libraries; their headers are the MIT ones of ffmpeg's nv-codec-headers
// (ffnvcodec/), and it reaches every library through dlopen.
#include <dlfcn.h>
#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include "ffnvcodec/dynlink_cuda.h"
#include "ffnvcodec/dynlink_nvcuvid.h"
#include "ffnvcodec/nvEncodeAPI.h"

enum { WIDTH = 640, HEIGHT = 368, FRAMES = 16, QP = 20 };

static tcuInit *cu_init;
static tcuDeviceGetCount *cu_count;
static tcuDeviceGet *cu_get;
static tcuDeviceGetName *cu_name;
static tcuCtxCreate_v2 *cu_ctx_create;
static tcuCtxDestroy_v2 *cu_ctx_destroy;
static tcuCtxPushCurrent_v2 *cu_push;
static tcuCtxPopCurrent_v2 *cu_pop;
static tcuMemcpy2D_v2 *cu_copy_2d;
static tcuGetErrorName *cu_error_name;

static tcuvidGetDecoderCaps *cuvid_caps;
static tcuvidCreateDecoder *cuvid_create;
static tcuvidDestroyDecoder *cuvid_destroy;
static tcuvidDecodePicture *cuvid_decode;
static tcuvidMapVideoFrame64 *cuvid_map;
static tcuvidUnmapVideoFrame64 *cuvid_unmap;
static tcuvidCreateVideoParser *cuvid_parser;
static tcuvidParseVideoData *cuvid_parse;
static tcuvidDestroyVideoParser *cuvid_parser_destroy;

static NVENCSTATUS(NVENCAPI *nvenc_max_version)(uint32_t *);
static NVENCSTATUS(NVENCAPI *nvenc_create_instance)(NV_ENCODE_API_FUNCTION_LIST *);
static NV_ENCODE_API_FUNCTION_LIST nvenc;

static double now_s(void)
{
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

static int cu_ok(const char *step, const char *what, CUresult r)
{
    const char *name = "?";
    if (r == CUDA_SUCCESS)
        return 1;
    if (cu_error_name)
        cu_error_name(r, &name);
    printf("  %-8s FAILED: %s: %d %s\n", step, what, (int)r, name ? name : "?");
    return 0;
}

static int nv_ok(const char *step, const char *what, NVENCSTATUS s)
{
    if (s == NV_ENC_SUCCESS)
        return 1;
    printf("  %-8s FAILED: %s: NVENCSTATUS %d\n", step, what, (int)s);
    return 0;
}

static void *open_lib(const char *name, const char *package)
{
    void *lib = dlopen(name, RTLD_NOW | RTLD_GLOBAL);
    if (!lib)
        printf("FAILED: dlopen %s: %s (NVIDIA's %s is not in the image)\n", name, dlerror(), package);
    return lib;
}

#define SYM(lib, var, name)                                                                      \
    ((var = (__typeof__(var))dlsym(lib, name)) != NULL ||                                        \
     (printf("FAILED: %s has no %s\n", #lib, name), 0))

static int load(void)
{
    void *cuda = open_lib("libcuda.so.1", "libnvidia-compute");
    void *nvcuvid = cuda ? open_lib("libnvcuvid.so.1", "libnvidia-decode") : NULL;
    void *encode = nvcuvid ? open_lib("libnvidia-encode.so.1", "libnvidia-encode") : NULL;
    if (!encode)
        return 0;
    return SYM(cuda, cu_init, "cuInit") && SYM(cuda, cu_count, "cuDeviceGetCount") &&
           SYM(cuda, cu_get, "cuDeviceGet") && SYM(cuda, cu_name, "cuDeviceGetName") &&
           SYM(cuda, cu_ctx_create, "cuCtxCreate_v2") && SYM(cuda, cu_ctx_destroy, "cuCtxDestroy_v2") &&
           SYM(cuda, cu_push, "cuCtxPushCurrent_v2") && SYM(cuda, cu_pop, "cuCtxPopCurrent_v2") &&
           SYM(cuda, cu_copy_2d, "cuMemcpy2D_v2") && SYM(cuda, cu_error_name, "cuGetErrorName") &&
           SYM(nvcuvid, cuvid_caps, "cuvidGetDecoderCaps") &&
           SYM(nvcuvid, cuvid_create, "cuvidCreateDecoder") &&
           SYM(nvcuvid, cuvid_destroy, "cuvidDestroyDecoder") &&
           SYM(nvcuvid, cuvid_decode, "cuvidDecodePicture") &&
           SYM(nvcuvid, cuvid_map, "cuvidMapVideoFrame64") &&
           SYM(nvcuvid, cuvid_unmap, "cuvidUnmapVideoFrame64") &&
           SYM(nvcuvid, cuvid_parser, "cuvidCreateVideoParser") &&
           SYM(nvcuvid, cuvid_parse, "cuvidParseVideoData") &&
           SYM(nvcuvid, cuvid_parser_destroy, "cuvidDestroyVideoParser") &&
           SYM(encode, nvenc_max_version, "NvEncodeAPIGetMaxSupportedVersion") &&
           SYM(encode, nvenc_create_instance, "NvEncodeAPICreateInstance");
}

// Frame `n`: a diagonal gradient that moves 4 pixels a frame, with a box
// that moves the other way, so P frames have motion to find.
static uint8_t luma_of(int n, int x, int y)
{
    int v = ((x + y + 4 * n) * 255) / (WIDTH + HEIGHT);
    int bx = WIDTH - 160 - 6 * n, by = 96;
    if (x >= bx && x < bx + 96 && y >= by && y < by + 96)
        v = 255 - v;
    return (uint8_t)v;
}

static void fill_nv12(uint8_t *p, uint32_t pitch, int n)
{
    for (int y = 0; y < HEIGHT; y++)
        for (int x = 0; x < WIDTH; x++)
            p[(size_t)y * pitch + x] = luma_of(n, x, y);
    uint8_t *uv = p + (size_t)HEIGHT * pitch;
    for (int y = 0; y < HEIGHT / 2; y++)
        for (int x = 0; x < WIDTH; x += 2) {
            uv[(size_t)y * pitch + x] = (uint8_t)(64 + y % 128);
            uv[(size_t)y * pitch + x + 1] = (uint8_t)(192 - x % 128);
        }
}

struct stream {
    uint8_t *bytes;
    size_t len, cap;
};

static int append(struct stream *s, const void *p, size_t n)
{
    if (s->len + n > s->cap) {
        size_t cap = s->cap ? s->cap * 2 : 1 << 20;
        while (cap < s->len + n)
            cap *= 2;
        uint8_t *b = realloc(s->bytes, cap);
        if (!b)
            return 0;
        s->bytes = b;
        s->cap = cap;
    }
    memcpy(s->bytes + s->len, p, n);
    s->len += n;
    return 1;
}

static int has_guid(const GUID *list, uint32_t n, const GUID *want)
{
    for (uint32_t i = 0; i < n; i++)
        if (memcmp(&list[i], want, sizeof *want) == 0)
            return 1;
    return 0;
}

// NVENC: FRAMES frames of `codec` into `out`. 1 done, 0 failed, -1 the GPU
// does not encode that codec.
static int encode(CUcontext ctx, const GUID *codec, const char *name, struct stream *out)
{
    const char *step = "encode";
    void *enc = NULL;
    NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS open;
    memset(&open, 0, sizeof open);
    open.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
    open.deviceType = NV_ENC_DEVICE_TYPE_CUDA;
    open.device = ctx;
    open.apiVersion = NVENCAPI_VERSION;
    if (!nv_ok(step, "nvEncOpenEncodeSessionEx", nvenc.nvEncOpenEncodeSessionEx(&open, &enc)))
        return 0;

    int result = 0;
    NV_ENC_INPUT_PTR input = NULL;
    NV_ENC_OUTPUT_PTR bitstream = NULL;
    uint32_t count = 0, got = 0;
    GUID guids[16];
    if (!nv_ok(step, "nvEncGetEncodeGUIDCount", nvenc.nvEncGetEncodeGUIDCount(enc, &count)) ||
        !nv_ok(step, "nvEncGetEncodeGUIDs",
               nvenc.nvEncGetEncodeGUIDs(enc, guids, count < 16 ? count : 16, &got)))
        goto out;
    if (!has_guid(guids, got, codec)) {
        result = -1;
        goto out;
    }

    NV_ENC_PRESET_CONFIG preset;
    memset(&preset, 0, sizeof preset);
    preset.version = NV_ENC_PRESET_CONFIG_VER;
    preset.presetCfg.version = NV_ENC_CONFIG_VER;
    if (!nv_ok(step, "nvEncGetEncodePresetConfigEx",
               nvenc.nvEncGetEncodePresetConfigEx(enc, *codec, NV_ENC_PRESET_P4_GUID,
                                                  NV_ENC_TUNING_INFO_LOW_LATENCY, &preset)))
        goto out;
    NV_ENC_CONFIG config = preset.presetCfg;
    config.version = NV_ENC_CONFIG_VER;
    config.frameIntervalP = 1;
    config.gopLength = NVENC_INFINITE_GOPLENGTH;
    config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_CONSTQP;
    config.rcParams.constQP.qpIntra = QP;
    config.rcParams.constQP.qpInterP = QP;
    config.rcParams.constQP.qpInterB = QP;

    NV_ENC_INITIALIZE_PARAMS init;
    memset(&init, 0, sizeof init);
    init.version = NV_ENC_INITIALIZE_PARAMS_VER;
    init.encodeGUID = *codec;
    init.presetGUID = NV_ENC_PRESET_P4_GUID;
    init.tuningInfo = NV_ENC_TUNING_INFO_LOW_LATENCY;
    init.encodeWidth = init.darWidth = init.maxEncodeWidth = WIDTH;
    init.encodeHeight = init.darHeight = init.maxEncodeHeight = HEIGHT;
    init.frameRateNum = 30;
    init.frameRateDen = 1;
    init.enablePTD = 1;
    init.encodeConfig = &config;
    if (!nv_ok(step, "nvEncInitializeEncoder", nvenc.nvEncInitializeEncoder(enc, &init)))
        goto out;

    NV_ENC_CREATE_INPUT_BUFFER in;
    memset(&in, 0, sizeof in);
    in.version = NV_ENC_CREATE_INPUT_BUFFER_VER;
    in.width = WIDTH;
    in.height = HEIGHT;
    in.bufferFmt = NV_ENC_BUFFER_FORMAT_NV12;
    NV_ENC_CREATE_BITSTREAM_BUFFER bs;
    memset(&bs, 0, sizeof bs);
    bs.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
    if (!nv_ok(step, "nvEncCreateInputBuffer", nvenc.nvEncCreateInputBuffer(enc, &in)) ||
        !nv_ok(step, "nvEncCreateBitstreamBuffer", nvenc.nvEncCreateBitstreamBuffer(enc, &bs)))
        goto out;
    input = in.inputBuffer;
    bitstream = bs.bitstreamBuffer;

    double t0 = now_s();
    for (int n = 0; n <= FRAMES; n++) {
        NV_ENC_PIC_PARAMS pic;
        memset(&pic, 0, sizeof pic);
        pic.version = NV_ENC_PIC_PARAMS_VER;
        if (n == FRAMES) {
            // End of stream: what the encoder still holds comes out.
            pic.encodePicFlags = NV_ENC_PIC_FLAG_EOS;
            if (!nv_ok(step, "nvEncEncodePicture (EOS)", nvenc.nvEncEncodePicture(enc, &pic)))
                goto out;
            break;
        }
        NV_ENC_LOCK_INPUT_BUFFER lock;
        memset(&lock, 0, sizeof lock);
        lock.version = NV_ENC_LOCK_INPUT_BUFFER_VER;
        lock.inputBuffer = input;
        if (!nv_ok(step, "nvEncLockInputBuffer", nvenc.nvEncLockInputBuffer(enc, &lock)))
            goto out;
        fill_nv12(lock.bufferDataPtr, lock.pitch, n);
        uint32_t pitch = lock.pitch;
        if (!nv_ok(step, "nvEncUnlockInputBuffer", nvenc.nvEncUnlockInputBuffer(enc, input)))
            goto out;
        pic.inputWidth = WIDTH;
        pic.inputHeight = HEIGHT;
        pic.inputPitch = pitch;
        pic.inputBuffer = input;
        pic.outputBitstream = bitstream;
        pic.bufferFmt = NV_ENC_BUFFER_FORMAT_NV12;
        pic.pictureStruct = NV_ENC_PIC_STRUCT_FRAME;
        pic.inputTimeStamp = (uint64_t)n;
        pic.frameIdx = (uint32_t)n;
        if (!nv_ok(step, "nvEncEncodePicture", nvenc.nvEncEncodePicture(enc, &pic)))
            goto out;
        NV_ENC_LOCK_BITSTREAM lb;
        memset(&lb, 0, sizeof lb);
        lb.version = NV_ENC_LOCK_BITSTREAM_VER;
        lb.outputBitstream = bitstream;
        if (!nv_ok(step, "nvEncLockBitstream", nvenc.nvEncLockBitstream(enc, &lb)))
            goto out;
        int kept = append(out, lb.bitstreamBufferPtr, lb.bitstreamSizeInBytes);
        nvenc.nvEncUnlockBitstream(enc, bitstream);
        if (!kept) {
            printf("  %-8s FAILED: no memory for the stream\n", step);
            goto out;
        }
    }
    double t1 = now_s();
    printf("  encode   %s %dx%d, %d frames, %zu bytes, %.0f frames/s  ok\n", name, WIDTH, HEIGHT,
           FRAMES, out->len, FRAMES / (t1 - t0));
    result = out->len > 0 ? 1 : 0;
    if (!result)
        printf("  %-8s FAILED: the encoder gave no bytes\n", step);
out:
    if (bitstream)
        nvenc.nvEncDestroyBitstreamBuffer(enc, bitstream);
    if (input)
        nvenc.nvEncDestroyInputBuffer(enc, input);
    nvenc.nvEncDestroyEncoder(enc);
    return result;
}

struct decoding {
    cudaVideoCodec codec;
    CUvideodecoder decoder;
    int pictures, failed;
    double worst_psnr;
    uint8_t *luma;
};

static int CUDAAPI on_sequence(void *user, CUVIDEOFORMAT *format)
{
    struct decoding *d = user;
    if (d->decoder)
        return 8;
    CUVIDDECODECREATEINFO info;
    memset(&info, 0, sizeof info);
    info.ulWidth = format->coded_width;
    info.ulHeight = format->coded_height;
    info.ulNumDecodeSurfaces = 8;
    info.CodecType = format->codec;
    info.ChromaFormat = format->chroma_format;
    info.ulCreationFlags = cudaVideoCreate_PreferCUVID;
    info.bitDepthMinus8 = format->bit_depth_luma_minus8;
    info.display_area.left = (short)format->display_area.left;
    info.display_area.top = (short)format->display_area.top;
    info.display_area.right = (short)format->display_area.right;
    info.display_area.bottom = (short)format->display_area.bottom;
    info.OutputFormat = cudaVideoSurfaceFormat_NV12;
    info.DeinterlaceMode = cudaVideoDeinterlaceMode_Weave;
    info.ulTargetWidth = format->display_area.right - format->display_area.left;
    info.ulTargetHeight = format->display_area.bottom - format->display_area.top;
    info.ulNumOutputSurfaces = 2;
    if (!cu_ok("decode", "cuvidCreateDecoder", cuvid_create(&d->decoder, &info))) {
        d->failed = 1;
        return 0;
    }
    return 8;
}

static int CUDAAPI on_decode(void *user, CUVIDPICPARAMS *pic)
{
    struct decoding *d = user;
    if (!d->decoder || !cu_ok("decode", "cuvidDecodePicture", cuvid_decode(d->decoder, pic))) {
        d->failed = 1;
        return 0;
    }
    return 1;
}

static int CUDAAPI on_display(void *user, CUVIDPARSERDISPINFO *info)
{
    struct decoding *d = user;
    if (!info)
        return 1;
    CUVIDPROCPARAMS proc;
    memset(&proc, 0, sizeof proc);
    proc.progressive_frame = info->progressive_frame;
    proc.top_field_first = info->top_field_first;
    unsigned long long frame = 0;
    unsigned int pitch = 0;
    if (!cu_ok("decode", "cuvidMapVideoFrame64", cuvid_map(d->decoder, info->picture_index, &frame, &pitch, &proc))) {
        d->failed = 1;
        return 0;
    }
    CUDA_MEMCPY2D copy;
    memset(&copy, 0, sizeof copy);
    copy.srcMemoryType = CU_MEMORYTYPE_DEVICE;
    copy.srcDevice = frame;
    copy.srcPitch = pitch;
    copy.dstMemoryType = CU_MEMORYTYPE_HOST;
    copy.dstHost = d->luma;
    copy.dstPitch = WIDTH;
    copy.WidthInBytes = WIDTH;
    copy.Height = HEIGHT;
    int copied = cu_ok("decode", "cuMemcpy2D", cu_copy_2d(&copy));
    cuvid_unmap(d->decoder, frame);
    if (!copied) {
        d->failed = 1;
        return 0;
    }
    // No B frames: pictures come out in the order they went in.
    int n = d->pictures;
    double se = 0;
    for (int y = 0; y < HEIGHT; y++)
        for (int x = 0; x < WIDTH; x++) {
            double e = (double)d->luma[(size_t)y * WIDTH + x] - luma_of(n, x, y);
            se += e * e;
        }
    double mse = se / ((double)WIDTH * HEIGHT);
    double psnr = mse > 0 ? 10 * log10(255.0 * 255.0 / mse) : 99;
    if (d->pictures == 0 || psnr < d->worst_psnr)
        d->worst_psnr = psnr;
    d->pictures++;
    return 1;
}

// NVDEC: `stream` of `codec` decoded and checked.
static int decode(cudaVideoCodec codec, const char *name, const struct stream *stream)
{
    const char *step = "decode";
    CUVIDDECODECAPS caps;
    memset(&caps, 0, sizeof caps);
    caps.eCodecType = codec;
    caps.eChromaFormat = cudaVideoChromaFormat_420;
    caps.nBitDepthMinus8 = 0;
    if (!cu_ok(step, "cuvidGetDecoderCaps", cuvid_caps(&caps)))
        return 0;
    if (!caps.bIsSupported) {
        printf("  %-8s FAILED: NVDEC says it does not decode %s 4:2:0 8-bit\n", step, name);
        return 0;
    }

    struct decoding d;
    memset(&d, 0, sizeof d);
    d.codec = codec;
    d.luma = malloc((size_t)WIDTH * HEIGHT);
    if (!d.luma)
        return 0;
    CUVIDPARSERPARAMS params;
    memset(&params, 0, sizeof params);
    params.CodecType = codec;
    params.ulMaxNumDecodeSurfaces = 8;
    params.ulMaxDisplayDelay = 0;
    params.pUserData = &d;
    params.pfnSequenceCallback = on_sequence;
    params.pfnDecodePicture = on_decode;
    params.pfnDisplayPicture = on_display;
    CUvideoparser parser = NULL;
    int ok = cu_ok(step, "cuvidCreateVideoParser", cuvid_parser(&parser, &params));
    double t0 = now_s();
    if (ok) {
        // The whole elementary stream as one packet, then end of stream.
        CUVIDSOURCEDATAPACKET packet;
        memset(&packet, 0, sizeof packet);
        packet.payload = stream->bytes;
        packet.payload_size = (unsigned long)stream->len;
        packet.flags = CUVID_PKT_ENDOFSTREAM;
        ok = cu_ok(step, "cuvidParseVideoData", cuvid_parse(parser, &packet));
    }
    double t1 = now_s();
    if (parser)
        cuvid_parser_destroy(parser);
    if (d.decoder)
        cuvid_destroy(d.decoder);
    free(d.luma);
    if (!ok || d.failed)
        return 0;
    if (d.pictures != FRAMES) {
        printf("  %-8s FAILED: %d pictures came out of %d encoded\n", step, d.pictures, FRAMES);
        return 0;
    }
    if (d.worst_psnr < 30) {
        printf("  %-8s FAILED: a picture differs from the frame encoded (PSNR %.1f dB, want 30)\n", step,
               d.worst_psnr);
        return 0;
    }
    printf("  decode   %s %d pictures, %.0f frames/s, worst PSNR %.1f dB  ok\n", name, d.pictures,
           d.pictures / (t1 - t0), d.worst_psnr);
    return 1;
}

static int probe_gpu(int index)
{
    CUdevice dev;
    CUcontext ctx = NULL;
    char gpu[128] = "?";
    if (!cu_ok("cuda", "cuDeviceGet", cu_get(&dev, index)))
        return 0;
    cu_name(gpu, sizeof gpu, dev);
    printf("[gpu %d] %s\n", index, gpu);
    if (!cu_ok("cuda", "cuCtxCreate", cu_ctx_create(&ctx, 0, dev)))
        return 0;
    struct {
        GUID guid;
        cudaVideoCodec codec;
        const char *name;
        int required;
    } codecs[] = {
        {NV_ENC_CODEC_H264_GUID, cudaVideoCodec_H264, "H.264", 1},
        {NV_ENC_CODEC_HEVC_GUID, cudaVideoCodec_HEVC, "HEVC", 0},
    };
    int ok = 1;
    for (size_t i = 0; ok && i < sizeof codecs / sizeof codecs[0]; i++) {
        struct stream s = {0};
        int e = encode(ctx, &codecs[i].guid, codecs[i].name, &s);
        if (e < 0) {
            printf("  encode   %s: not on this GPU%s\n", codecs[i].name, codecs[i].required ? "" : ", skipped");
            ok = !codecs[i].required;
        } else {
            ok = e && decode(codecs[i].codec, codecs[i].name, &s);
        }
        free(s.bytes);
    }
    cu_ctx_destroy(ctx);
    return ok;
}

int main(int argc, char **argv)
{
    int only = -1;
    for (int i = 1; i < argc; i++) {
        if (strcmp(argv[i], "-g") == 0 && i + 1 < argc) {
            only = atoi(argv[++i]);
        } else {
            printf("usage: nvcodec-probe [-g GPU]\n"
                   "  NVENC encode and NVDEC decode of a few frames on every GPU (or GPU)\n");
            return 2;
        }
    }
    if (!load())
        return 1;
    uint32_t max = 0;
    if (nvenc_max_version(&max) != NV_ENC_SUCCESS) {
        printf("FAILED: NvEncodeAPIGetMaxSupportedVersion\n");
        return 1;
    }
    printf("[nvenc] driver API %u.%u, probe built for %u.%u\n", max >> 4, max & 0xf, NVENCAPI_MAJOR_VERSION,
           NVENCAPI_MINOR_VERSION);
    if (((uint32_t)NVENCAPI_MAJOR_VERSION << 4 | NVENCAPI_MINOR_VERSION) > max) {
        printf("FAILED: the driver's NVENC is older than this probe's headers\n");
        return 1;
    }
    memset(&nvenc, 0, sizeof nvenc);
    nvenc.version = NV_ENCODE_API_FUNCTION_LIST_VER;
    if (nvenc_create_instance(&nvenc) != NV_ENC_SUCCESS) {
        printf("FAILED: NvEncodeAPICreateInstance\n");
        return 1;
    }
    int count = 0;
    if (!cu_ok("cuda", "cuInit", cu_init(0)) || !cu_ok("cuda", "cuDeviceGetCount", cu_count(&count)))
        return 1;
    if (count == 0) {
        printf("FAILED: no CUDA device (is the kernel booted with nvidia.cuda?)\n");
        return 1;
    }
    int failed = 0, tried = 0;
    for (int i = 0; i < count; i++) {
        if (only >= 0 && i != only)
            continue;
        tried++;
        if (!probe_gpu(i))
            failed++;
    }
    if (tried == 0) {
        printf("FAILED: no GPU %d (there are %d)\n", only, count);
        return 1;
    }
    if (failed) {
        printf("NVCODEC FAILED on %d of %d GPU(s); the FAILED line names the step\n", failed, tried);
        return 1;
    }
    printf("NVCODEC OK on %d GPU(s): NVENC and NVDEC\n", tried);
    return 0;
}
