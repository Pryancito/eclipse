/*
 * eclipse-sdl-probe: SDL smoke test / micro-bench for the Eclipse OS desktop.
 *
 * Opens the installed SDL library at RUN time (dlopen), so this binary links
 * against nothing but libc and can be built by xtask with the musl cross
 * toolchain without SDL headers or import libraries on the host. It reports
 * which SDL, which video backend and which render driver the session's
 * SDL_* policy actually produced (see docs/README-desktop.md, "SDL"), then
 * draws a moving colour bar for a while and prints the frame rate, so the
 * pixman (software / wl_shm) and gles2 (opengles2 on zink+NVK or llvmpipe)
 * paths can be told apart and timed from a terminal in the session.
 *
 * Two libraries, one binary:
 *   default  libSDL2-2.0.so.0  (also what sdl12-compat sits on)
 *   --sdl3   libSDL3.so.0      (the one with a native wl_shm framebuffer)
 * Two draw paths:
 *   default    SDL_CreateRenderer + RenderFillRect + RenderPresent
 *   --surface  SDL_GetWindowSurface + FillRect + UpdateWindowSurface
 *              (the "framebuffer" path that SDL_FRAMEBUFFER_ACCELERATION
 *              steers; on SDL3/Wayland this is pure wl_shm, no GL at all)
 *
 * Output is one `SDLPROBE:` line per fact, so it greps like the other
 * Eclipse benches. Exit status 0 on success, 1 on any SDL failure.
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

/* ---- the slice of the SDL ABI we use (same numbers in SDL2 and SDL3) ---- */
#define SDL_INIT_VIDEO 0x00000020u
#define SDL_WINDOW_RESIZABLE 0x00000020u
#define SDL2_WINDOWPOS_UNDEFINED 0x1FFF0000
#define EV_QUIT 0x100u
#define EV_WINDOW2 0x200u          /* SDL2 SDL_WINDOWEVENT */
#define EV_WINDOW2_CLOSE 14        /* SDL2 SDL_WINDOWEVENT_CLOSE (event.window.event) */
#define EV_WINDOW3_CLOSE 0x202u    /* SDL3 SDL_EVENT_WINDOW_CLOSE_REQUESTED */
#define EV_KEYDOWN 0x300u
#define SDL_EVENT_SIZE 128         /* sizeof(SDL_Event), both majors */

typedef struct { const char *name; uint32_t flags; uint32_t nfmt; uint32_t fmt[16]; int maxw, maxh; } sdl2_renderer_info;
typedef struct { uint8_t major, minor, patch; } sdl2_version;
typedef struct { int x, y, w, h; } sdl_rect;
typedef struct { float x, y, w, h; } sdl_frect;

struct api {
    int is3;
    void *lib;
    /* common shape */
    const char *(*GetError)(void);
    void (*Quit)(void);
    const char *(*GetCurrentVideoDriver)(void);
    int (*GetNumVideoDrivers)(void);
    const char *(*GetVideoDriver)(int);
    int (*GetNumRenderDrivers)(void);
    void (*DestroyWindow)(void *);
    void (*DestroyRenderer)(void *);
    int (*PollEvent)(void *);          /* SDL2: int (1 = event); SDL3: bool */
    int (*SetRenderDrawColor)(void *, uint8_t, uint8_t, uint8_t, uint8_t);
    int (*RenderClear)(void *);
    int (*RenderPresent)(void *);
    void *(*GetWindowSurface)(void *);
    int (*UpdateWindowSurface)(void *);
    int (*GetWindowSizeInPixels)(void *, int *, int *); /* optional */
    /* SDL2 shape */
    int (*Init2)(uint32_t);
    void (*GetVersion2)(sdl2_version *);
    void *(*CreateWindow2)(const char *, int, int, int, int, uint32_t);
    void *(*CreateRenderer2)(void *, int, uint32_t);
    int (*GetRendererInfo2)(void *, sdl2_renderer_info *);
    int (*GetRenderDriverInfo2)(int, sdl2_renderer_info *);
    int (*RenderFillRect2)(void *, const sdl_rect *);
    int (*FillRect2)(void *, const sdl_rect *, uint32_t);
    /* SDL3 shape */
    int (*Init3)(uint32_t);            /* bool */
    int (*GetVersion3)(void);
    void *(*CreateWindow3)(const char *, int, int, uint64_t);
    void *(*CreateRenderer3)(void *, const char *);
    const char *(*GetRendererName3)(void *);
    const char *(*GetRenderDriver3)(int);
    int (*RenderFillRect3)(void *, const sdl_frect *);
    int (*FillSurfaceRect3)(void *, const sdl_rect *, uint32_t);
};

static void *sym(struct api *a, const char *name, int required) {
    void *p = dlsym(a->lib, name);
    if (!p && required) {
        fprintf(stderr, "SDLPROBE: FAIL missing symbol %s: %s\n", name, dlerror());
        exit(1);
    }
    return p;
}

#define S(field, name, req) a->field = (__typeof__(a->field))sym(a, name, req)

static void load(struct api *a, int is3) {
    const char *soname = is3 ? "libSDL3.so.0" : "libSDL2-2.0.so.0";
    const char *pkg = is3 ? "sdl3" : "sdl2";
    memset(a, 0, sizeof *a);
    a->is3 = is3;
    a->lib = dlopen(soname, RTLD_NOW | RTLD_LOCAL);
    if (!a->lib) {
        fprintf(stderr, "SDLPROBE: FAIL dlopen %s: %s (apk add %s)\n", soname, dlerror(), pkg);
        exit(1);
    }
    printf("SDLPROBE: library %s\n", soname);
    S(GetError, "SDL_GetError", 1);
    S(Quit, "SDL_Quit", 1);
    S(GetCurrentVideoDriver, "SDL_GetCurrentVideoDriver", 1);
    S(GetNumVideoDrivers, "SDL_GetNumVideoDrivers", 1);
    S(GetVideoDriver, "SDL_GetVideoDriver", 1);
    S(GetNumRenderDrivers, "SDL_GetNumRenderDrivers", 1);
    S(DestroyWindow, "SDL_DestroyWindow", 1);
    S(DestroyRenderer, "SDL_DestroyRenderer", 1);
    S(PollEvent, "SDL_PollEvent", 1);
    S(SetRenderDrawColor, "SDL_SetRenderDrawColor", 1);
    S(RenderClear, "SDL_RenderClear", 1);
    S(RenderPresent, "SDL_RenderPresent", 1);
    S(GetWindowSurface, "SDL_GetWindowSurface", 1);
    S(UpdateWindowSurface, "SDL_UpdateWindowSurface", 1);
    S(GetWindowSizeInPixels, "SDL_GetWindowSizeInPixels", 0);
    if (is3) {
        S(Init3, "SDL_Init", 1);
        S(GetVersion3, "SDL_GetVersion", 1);
        S(CreateWindow3, "SDL_CreateWindow", 1);
        S(CreateRenderer3, "SDL_CreateRenderer", 1);
        S(GetRendererName3, "SDL_GetRendererName", 1);
        S(GetRenderDriver3, "SDL_GetRenderDriver", 1);
        S(RenderFillRect3, "SDL_RenderFillRect", 1);
        S(FillSurfaceRect3, "SDL_FillSurfaceRect", 1);
    } else {
        S(Init2, "SDL_Init", 1);
        S(GetVersion2, "SDL_GetVersion", 1);
        S(CreateWindow2, "SDL_CreateWindow", 1);
        S(CreateRenderer2, "SDL_CreateRenderer", 1);
        S(GetRendererInfo2, "SDL_GetRendererInfo", 1);
        S(GetRenderDriverInfo2, "SDL_GetRenderDriverInfo", 1);
        S(RenderFillRect2, "SDL_RenderFillRect", 1);
        S(FillRect2, "SDL_FillRect", 1);
    }
}

/* SDL2 returns 0 on success; SDL3 returns true (nonzero) on success. */
static int ok(const struct api *a, int rc) { return a->is3 ? rc != 0 : rc == 0; }

static double now_s(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (double)ts.tv_sec + (double)ts.tv_nsec / 1e9;
}

static void print_env(void) {
    static const char *const keys[] = {
        "SDL_VIDEODRIVER", "SDL_VIDEO_DRIVER", "SDL_RENDER_DRIVER",
        "SDL_FRAMEBUFFER_ACCELERATION", "SDL_AUDIODRIVER", "SDL_AUDIO_DRIVER",
        "WLR_RENDERER", "LIBGL_ALWAYS_SOFTWARE", "GALLIUM_DRIVER",
        "WAYLAND_DISPLAY", "DISPLAY", NULL,
    };
    for (int i = 0; keys[i]; i++) {
        const char *v = getenv(keys[i]);
        printf("SDLPROBE: env %s=%s\n", keys[i], v ? v : "(unset)");
    }
}

/* Returns 1 when the user asked to close (quit, key, window close). */
static int drain_events(const struct api *a) {
    unsigned char ev[SDL_EVENT_SIZE] __attribute__((aligned(8)));
    int quit = 0;
    while (a->PollEvent(ev)) {
        uint32_t type;
        memcpy(&type, ev, sizeof type);
        if (type == EV_QUIT || type == EV_KEYDOWN)
            quit = 1;
        else if (a->is3 && type == EV_WINDOW3_CLOSE)
            quit = 1;
        else if (!a->is3 && type == EV_WINDOW2 && ev[12] == EV_WINDOW2_CLOSE)
            quit = 1;
    }
    return quit;
}

static void usage(void) {
    puts("usage: eclipse-sdl-probe [--sdl3] [--surface] [--frames N | --hold] [--size WxH]\n"
         "  --sdl3     probe libSDL3.so.0 instead of libSDL2-2.0.so.0\n"
         "  --surface  draw through SDL_GetWindowSurface/UpdateWindowSurface\n"
         "             (the path SDL_FRAMEBUFFER_ACCELERATION controls) instead\n"
         "             of an SDL_Renderer (the path SDL_RENDER_DRIVER controls)\n"
         "  --frames N stop after N frames (default 300; 0 = until closed)\n"
         "  --hold     same as --frames 0: keep running, stats every 300 frames,\n"
         "             close the window or press a key to quit\n"
         "  --size WxH window size (default 640x400)");
}

int main(int argc, char **argv) {
    int is3 = 0, surface = 0, w = 640, h = 400;
    long frames = 300;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--sdl3")) is3 = 1;
        else if (!strcmp(argv[i], "--surface")) surface = 1;
        else if (!strcmp(argv[i], "--hold")) frames = 0;
        else if (!strcmp(argv[i], "--frames") && i + 1 < argc) frames = atol(argv[++i]);
        else if (!strcmp(argv[i], "--size") && i + 1 < argc) {
            if (sscanf(argv[++i], "%dx%d", &w, &h) != 2 || w < 16 || h < 16) { usage(); return 2; }
        } else { usage(); return !strcmp(argv[i], "--help") ? 0 : 2; }
    }

    struct api A, *a = &A;
    load(a, is3);
    print_env();

    if (is3) {
        int v = a->GetVersion3();
        printf("SDLPROBE: version %d.%d.%d\n", v / 1000000, (v / 1000) % 1000, v % 1000);
    } else {
        sdl2_version v = {0, 0, 0};
        a->GetVersion2(&v);
        printf("SDLPROBE: version %d.%d.%d\n", v.major, v.minor, v.patch);
    }

    int nvid = a->GetNumVideoDrivers();
    printf("SDLPROBE: video drivers compiled in:");
    for (int i = 0; i < nvid; i++) printf(" %s", a->GetVideoDriver(i));
    printf("\n");
    int nrend = a->GetNumRenderDrivers();
    printf("SDLPROBE: render drivers compiled in:");
    for (int i = 0; i < nrend; i++) {
        if (is3) printf(" %s", a->GetRenderDriver3(i));
        else { sdl2_renderer_info info; memset(&info, 0, sizeof info);
               if (a->GetRenderDriverInfo2(i, &info) == 0 && info.name) printf(" %s", info.name); }
    }
    printf("\n");

    int rc = is3 ? a->Init3(SDL_INIT_VIDEO) : a->Init2(SDL_INIT_VIDEO);
    if (!ok(a, rc)) {
        fprintf(stderr, "SDLPROBE: FAIL SDL_Init(VIDEO): %s\n", a->GetError());
        return 1;
    }
    const char *vd = a->GetCurrentVideoDriver();
    printf("SDLPROBE: video driver in use: %s\n", vd ? vd : "(none)");

    void *win = is3 ? a->CreateWindow3("Eclipse SDL probe", w, h, SDL_WINDOW_RESIZABLE)
                    : a->CreateWindow2("Eclipse SDL probe", SDL2_WINDOWPOS_UNDEFINED,
                                       SDL2_WINDOWPOS_UNDEFINED, w, h, SDL_WINDOW_RESIZABLE);
    if (!win) {
        fprintf(stderr, "SDLPROBE: FAIL SDL_CreateWindow: %s\n", a->GetError());
        a->Quit();
        return 1;
    }
    if (a->GetWindowSizeInPixels) {
        int pw = 0, ph = 0;
        if (ok(a, a->GetWindowSizeInPixels(win, &pw, &ph)))
            printf("SDLPROBE: window %dx%d logical, %dx%d pixels\n", w, h, pw, ph);
    }

    void *ren = NULL;
    if (surface) {
        void *surf = a->GetWindowSurface(win);
        if (!surf) {
            fprintf(stderr, "SDLPROBE: FAIL SDL_GetWindowSurface: %s\n", a->GetError());
            a->DestroyWindow(win); a->Quit();
            return 1;
        }
        printf("SDLPROBE: draw path: window surface (SDL_FRAMEBUFFER_ACCELERATION=%s)\n",
               getenv("SDL_FRAMEBUFFER_ACCELERATION") ? getenv("SDL_FRAMEBUFFER_ACCELERATION") : "(unset)");
    } else {
        ren = is3 ? a->CreateRenderer3(win, NULL) : a->CreateRenderer2(win, -1, 0);
        if (!ren) {
            fprintf(stderr, "SDLPROBE: FAIL SDL_CreateRenderer: %s\n", a->GetError());
            a->DestroyWindow(win); a->Quit();
            return 1;
        }
        const char *rn = NULL;
        if (is3) rn = a->GetRendererName3(ren);
        else { static sdl2_renderer_info info; memset(&info, 0, sizeof info);
               if (a->GetRendererInfo2(ren, &info) == 0) rn = info.name; }
        printf("SDLPROBE: draw path: renderer '%s' (SDL_RENDER_DRIVER=%s)\n", rn ? rn : "?",
               getenv("SDL_RENDER_DRIVER") ? getenv("SDL_RENDER_DRIVER") : "(unset)");
    }

    /* Draw loop: dark background + a colour bar sweeping left to right. */
    double t0 = now_s(), tlast = t0;
    long n = 0, nlast = 0;
    int failed = 0;
    for (;;) {
        if (drain_events(a)) break;
        if (frames > 0 && n >= frames) break;
        int barw = w / 8;
        int x = (int)((n * 4) % (long)(w + barw)) - barw;
        uint8_t r = (uint8_t)(128 + 127 * __builtin_sin((double)n * 0.05));
        uint8_t g = (uint8_t)(96 + 64 * __builtin_sin((double)n * 0.031 + 2.0));
        uint8_t b = 0xe0;
        if (surface) {
            void *surf = a->GetWindowSurface(win); /* may change on resize */
            sdl_rect bar = { x, 0, barw, h };
            uint32_t bg = 0xFF141020u, fg = 0xFF000000u | ((uint32_t)r << 16) | ((uint32_t)g << 8) | b;
            int r1 = is3 ? a->FillSurfaceRect3(surf, NULL, bg) : a->FillRect2(surf, NULL, bg);
            int r2 = is3 ? a->FillSurfaceRect3(surf, &bar, fg) : a->FillRect2(surf, &bar, fg);
            int r3 = a->UpdateWindowSurface(win);
            if (!ok(a, r1) || !ok(a, r2) || !ok(a, r3)) { failed = 1; break; }
        } else {
            a->SetRenderDrawColor(ren, 0x14, 0x10, 0x20, 0xff);
            if (!ok(a, a->RenderClear(ren))) { failed = 1; break; }
            a->SetRenderDrawColor(ren, r, g, b, 0xff);
            int r2;
            if (is3) { sdl_frect bar = { (float)x, 0.f, (float)barw, (float)h }; r2 = a->RenderFillRect3(ren, &bar); }
            else     { sdl_rect  bar = { x, 0, barw, h };                        r2 = a->RenderFillRect2(ren, &bar); }
            if (!ok(a, r2)) { failed = 1; break; }
            if (!ok(a, a->RenderPresent(ren))) { failed = 1; break; }
        }
        n++;
        if (frames == 0 && n % 300 == 0) {
            double t = now_s();
            printf("SDLPROBE: %ld frames, last 300 at %.1f fps (%.2f ms/frame)\n", n,
                   300.0 / (t - tlast), (t - tlast) * 1000.0 / 300.0);
            fflush(stdout);
            tlast = t; nlast = n;
        }
    }
    double t1 = now_s();
    if (failed) {
        fprintf(stderr, "SDLPROBE: FAIL drawing frame %ld: %s\n", n, a->GetError());
    } else if (n > 0) {
        printf("SDLPROBE: %ld frames in %.2f s: %.1f fps, %.2f ms/frame\n", n, t1 - t0,
               (double)n / (t1 - t0), (t1 - t0) * 1000.0 / (double)n);
    }
    (void)nlast;

    if (ren) a->DestroyRenderer(ren);
    a->DestroyWindow(win);
    a->Quit();
    printf("SDLPROBE: %s\n", failed ? "FAIL" : "OK");
    return failed ? 1 : 0;
}
