// Eclipse OS: a userspace-graphics-stack probe.
//
// `drm-probe` answers "does the kernel's DRM/KMS give a compositor what it
// needs?" -- it drives the ioctls with a static binary and never touches Mesa.
// But a black screen (or a compositor that starts and renders nothing) usually
// dies one layer *up*, in userspace: GBM cannot allocate, EGL will not
// initialise on the DRM node, the GLES driver falls back to something that
// does not actually render, Vulkan enumerates zero devices, or the Wayland
// socket is there but no compositor answers. Every one of those fails with the
// same symptom drm-probe cannot see, because drm-probe links against libc only.
//
// So this probe is drm-probe's userspace companion: it dlopen()s the *installed*
// graphics libraries at run time (like eclipse-sdl-probe), drives each layer of
// the stack a GL/Vulkan/Wayland client rests on, bottom-up, exactly the way
// Mesa and a real client drive them, and reports the first that breaks. Where
// drm-probe's proof is a test pattern you SEE, this probe's proof is a pixel it
// draws and reads BACK: it clears an off-screen framebuffer to a known colour
// with the real GL driver and verifies glReadPixels returns that colour -- the
// programmatic analogue of audio-probe's tone. It never opens a window.
//
// The layers, in the order Mesa and a client walk them:
//
//   libs     which of the stack's shared objects are actually installed
//            (libgbm, libEGL, libGLESv2, libvulkan, libwayland-client) -- the
//            inventory apk produced; a missing one turns its section to SKIP.
//   gbm      gbm_create_device on /dev/dri/renderD128 -> gbm_bo_create: the
//            buffer allocator EGL and every GBM compositor back end rest on,
//            and the backend name (should be a real driver, not "swrast").
//   egl      eglGetPlatformDisplay(GBM) -> eglInitialize -> eglChooseConfig ->
//            eglCreateContext -> eglMakeCurrent: the EGL bring-up Mesa performs
//            before any GL call, plus EGL_VENDOR/VERSION/CLIENT_APIS.
//   gl       glGetString(RENDERER/VENDOR/VERSION) then an off-screen FBO ->
//            glClear to blue -> glReadPixels: does the GL context actually
//            render, and on what (a hardware driver, llvmpipe, or nothing).
//   vulkan   vkCreateInstance -> vkEnumeratePhysicalDevices -> device names and
//            types: the ICDs the loader found (lavapipe swrast, NVK, nvidia).
//   wayland  wl_display_connect($WAYLAND_DISPLAY) -> get_registry -> roundtrip:
//            is a compositor listening, and which globals does it advertise
//            (wl_compositor, xdg_wm_base, wl_shm, linux-dmabuf, layer-shell)?
//   shm      bind wl_shm -> create_pool(memfd) -> create_buffer -> roundtrip:
//            the shared-memory buffer path a wl_shm client (foot, lunarbg)
//            presents through. No surface is created, so nothing is shown.
//   verdict  would a GL client, a Vulkan client, and a Wayland client each
//            come up on what was measured?
//
// Everything is resolved with dlopen(RTLD_NOW|RTLD_LOCAL)+dlsym, so this binary
// links against libc + libdl only and runs even when half the stack is absent.
// Enum values and struct layouts are the platform ABIs (EGL/GL/Vulkan/Wayland);
// the kernel side a compositor ultimately reaches is drm_scheme.rs.
//
// Build:  musl-gcc -O2 -o gfx-probe gfx-probe.c -ldl
// Run:    gfx-probe               (-v per-check detail, --card N for renderDN,
//                                  --skip SECTION to leave one out)
//
// Exit status is the number of failed checks, capped at 125.

#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

// ── EGL / GL / Vulkan / Wayland ABI constants (no headers required) ──────────

// EGL
#define EGL_NO_DISPLAY ((void *)0)
#define EGL_NO_CONTEXT ((void *)0)
#define EGL_NO_SURFACE ((void *)0)
#define EGL_DEFAULT_DISPLAY ((void *)0)
#define EGL_FALSE 0
#define EGL_TRUE 1
#define EGL_NONE 0x3038
#define EGL_VENDOR 0x3053
#define EGL_VERSION 0x3054
#define EGL_EXTENSIONS 0x3055
#define EGL_CLIENT_APIS 0x308D
#define EGL_SURFACE_TYPE 0x3033
#define EGL_PBUFFER_BIT 0x0001
#define EGL_RENDERABLE_TYPE 0x3040
#define EGL_OPENGL_ES2_BIT 0x0004
#define EGL_RED_SIZE 0x3024
#define EGL_GREEN_SIZE 0x3023
#define EGL_BLUE_SIZE 0x3022
#define EGL_ALPHA_SIZE 0x3021
#define EGL_WIDTH 0x3057
#define EGL_HEIGHT 0x3056
#define EGL_OPENGL_ES_API 0x30A0
#define EGL_CONTEXT_CLIENT_VERSION 0x3098
#define EGL_PLATFORM_GBM_KHR 0x31D7
#define EGL_PLATFORM_SURFACELESS_MESA 0x31DD

// GL / GLES2
#define GL_NO_ERROR 0
#define GL_VENDOR 0x1F00
#define GL_RENDERER 0x1F01
#define GL_VERSION 0x1F02
#define GL_SHADING_LANGUAGE_VERSION 0x8B8C
#define GL_COLOR_BUFFER_BIT 0x00004000
#define GL_RGBA 0x1908
#define GL_RGBA8 0x8058
#define GL_UNSIGNED_BYTE 0x1401
#define GL_FRAMEBUFFER 0x8D40
#define GL_RENDERBUFFER 0x8D41
#define GL_COLOR_ATTACHMENT0 0x8CE0
#define GL_FRAMEBUFFER_COMPLETE 0x8CD5
// GL enums the perf micro-bench needs
#define GL_FLOAT 0x1406
#define GL_FALSE 0
#define GL_TRIANGLES 0x0004
#define GL_ARRAY_BUFFER 0x8892
#define GL_STATIC_DRAW 0x88E4
#define GL_VERTEX_SHADER 0x8B31
#define GL_FRAGMENT_SHADER 0x8B30
#define GL_COMPILE_STATUS 0x8B81
#define GL_LINK_STATUS 0x8B82

// GBM
#define GBM_FORMAT_XRGB8888 0x34325258u  // 'XR24'
#define GBM_BO_USE_RENDERING 0x00000004u
#define GBM_BO_USE_SCANOUT 0x00000001u

// Vulkan
#define VK_SUCCESS 0
#define VK_STRUCTURE_TYPE_APPLICATION_INFO 0
#define VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO 1
#define VK_API_VERSION_1_0 0x00400000u  // VK_MAKE_VERSION(1,0,0)

// wl_shm pixel format (ARGB8888=0, XRGB8888=1) and registry/shm opcodes.
#define WL_SHM_FORMAT_XRGB8888 1
#define WL_DISPLAY_GET_REGISTRY 1
#define WL_REGISTRY_BIND 0
#define WL_SHM_CREATE_POOL 0
#define WL_SHM_POOL_CREATE_BUFFER 0

// ── report framework (mirrors drm-probe / audio-probe) ───────────────────────

static int g_verbose, g_card, g_bench;
static int g_pass, g_fail, g_skip;
static const char *g_skipped[16];
static int g_nskipped;

static int skipped(const char *name) {
  for (int i = 0; i < g_nskipped; i++)
    if (!strcmp(g_skipped[i], name)) return 1;
  return 0;
}
static void section(const char *name) { printf("\n[%s]\n", name); }
static void ok(const char *name, const char *why) {
  g_pass++;
  if (g_verbose) printf("  [ok]   %s  (%s)\n", name, why);
}
static void fail(const char *name, const char *why, int err) {
  g_fail++;
  printf("  [FAIL] %s: %s  (%s)\n", name, err ? strerror(err) : "unexpected result", why);
}
static void skip(const char *name, const char *why) {
  g_skip++;
  if (g_verbose) printf("  [skip] %s  (%s)\n", name, why);
}
static void check(int good, const char *name, const char *why, int err) {
  if (good) ok(name, why);
  else fail(name, why, err);
}
static void info(const char *fmt, ...) {
  va_list ap;
  va_start(ap, fmt);
  printf("         ");
  vprintf(fmt, ap);
  printf("\n");
  va_end(ap);
}

// Open a shared object read-to-run; NULL (and a note) if it is not installed.
static void *load_lib(const char *soname) {
  void *h = dlopen(soname, RTLD_NOW | RTLD_LOCAL);
  return h;
}
// dlsym with the required-symbol convention: NULL back for a missing symbol.
static void *load_sym(void *lib, const char *name) { return lib ? dlsym(lib, name) : NULL; }

// ── shared state discovered as sections run ──────────────────────────────────

static void *g_libgbm, *g_libegl, *g_libgles, *g_libvk, *g_libwl;
static int g_drm_fd = -1;
static void *g_gbm_dev;
static char g_gbm_backend[64] = "";
static void *g_egl_dpy, *g_egl_ctx, *g_egl_cfg, *g_egl_surf;
static char g_gl_renderer[128] = "";
static int g_gl_rendered = 0;   // FBO clear+readback matched
static int g_gl_swrast = 0;     // renderer looks like a software rasteriser
static int g_llvmpipe_bits = 0; // vector width parsed from a llvmpipe RENDERER
static double g_bench_fps = 0;  // per-frame-synced frame rate from the perf bench
static int g_vk_devs = 0;       // Vulkan physical devices enumerated
static void *g_wl_display;      // live compositor connection (or NULL)
static int g_wl_have_compositor, g_wl_have_xdg, g_wl_have_dmabuf, g_wl_have_layer;
static uint32_t g_wl_shm_name, g_wl_shm_ver;  // wl_shm global (name 0 == absent)

// EGL entry points (resolved once EGL is loaded).
static void *(*p_eglGetProcAddress)(const char *);
static void *(*p_eglGetPlatformDisplay)(unsigned, void *, const intptr_t *);
static void *(*p_eglGetPlatformDisplayEXT)(unsigned, void *, const int *);
static void *(*p_eglGetDisplay)(void *);
static unsigned (*p_eglInitialize)(void *, int *, int *);
static const char *(*p_eglQueryString)(void *, int);
static unsigned (*p_eglChooseConfig)(void *, const int *, void **, int, int *);
static unsigned (*p_eglBindAPI)(unsigned);
static void *(*p_eglCreateContext)(void *, void *, void *, const int *);
static void *(*p_eglCreatePbufferSurface)(void *, void *, const int *);
static unsigned (*p_eglMakeCurrent)(void *, void *, void *, void *);
static int (*p_eglGetError)(void);

// ── libs: what the apk pulled in ─────────────────────────────────────────────

static void test_libs(void) {
  section("libs");
  struct {
    const char *so;
    const char *what;
    void **slot;
  } libs[] = {
      {"libgbm.so.1", "GBM buffer allocator", &g_libgbm},
      {"libEGL.so.1", "EGL", &g_libegl},
      {"libGLESv2.so.2", "OpenGL ES 2", &g_libgles},
      {"libvulkan.so.1", "Vulkan loader", &g_libvk},
      {"libwayland-client.so.0", "Wayland client", &g_libwl},
  };
  for (size_t i = 0; i < sizeof libs / sizeof libs[0]; i++) {
    *libs[i].slot = load_lib(libs[i].so);
    if (*libs[i].slot)
      info("%s: %s (loaded)", libs[i].so, libs[i].what);
    else
      info("%s: %s NOT installed (%s)", libs[i].so, libs[i].what, dlerror());
  }
  // The base of the stack is EGL+GLES; call their absence a fail so the summary
  // is non-zero when the desktop GL path is simply not there.
  check(g_libegl && g_libgles, "core GL libraries present",
        "libEGL.so.1 + libGLESv2.so.2 back every compositor and GL app", 0);
}

// ── gbm: buffer allocation on the render node ────────────────────────────────

static void test_gbm(void) {
  section("gbm");
  if (!g_libgbm) {
    skip("gbm", "libgbm.so.1 not installed");
    return;
  }
  void *(*gbm_create_device)(int) = load_sym(g_libgbm, "gbm_create_device");
  const char *(*gbm_backend_name)(void *) = load_sym(g_libgbm, "gbm_device_get_backend_name");
  void *(*gbm_bo_create)(void *, uint32_t, uint32_t, uint32_t, uint32_t) =
      load_sym(g_libgbm, "gbm_bo_create");
  uint32_t (*gbm_bo_get_stride)(void *) = load_sym(g_libgbm, "gbm_bo_get_stride");
  void (*gbm_bo_destroy)(void *) = load_sym(g_libgbm, "gbm_bo_destroy");
  if (!gbm_create_device || !gbm_bo_create) {
    fail("gbm symbols", "libgbm.so.1 missing gbm_create_device/gbm_bo_create", 0);
    return;
  }

  char node[64];
  snprintf(node, sizeof node, "/dev/dri/renderD%d", 128 + g_card);
  g_drm_fd = open(node, O_RDWR | O_CLOEXEC);
  if (g_drm_fd < 0) {
    // Fall back to the primary node -- gbm works on card0 too.
    char card[64];
    snprintf(card, sizeof card, "/dev/dri/card%d", g_card);
    g_drm_fd = open(card, O_RDWR | O_CLOEXEC);
    if (g_drm_fd >= 0) info("render node absent; using %s", card);
  }
  if (g_drm_fd < 0) {
    fail("open DRM node", "gbm needs /dev/dri/renderD128 or card0", errno);
    return;
  }
  ok("open DRM node", "the fd every gbm_create_device is built on");

  g_gbm_dev = gbm_create_device(g_drm_fd);
  check(g_gbm_dev != NULL, "gbm_create_device", "Mesa's GBM back end starts here", errno);
  if (!g_gbm_dev) return;

  if (gbm_backend_name) {
    const char *b = gbm_backend_name(g_gbm_dev);
    if (b) {
      snprintf(g_gbm_backend, sizeof g_gbm_backend, "%s", b);
      info("gbm backend: %s", b);
    }
  }

  void *bo = gbm_bo_create(g_gbm_dev, 256, 256, GBM_FORMAT_XRGB8888,
                           GBM_BO_USE_RENDERING);
  check(bo != NULL, "gbm_bo_create XRGB8888 256x256",
        "the render-target allocation EGL wraps into a surface", errno);
  if (bo) {
    if (gbm_bo_get_stride) info("bo stride: %u bytes", gbm_bo_get_stride(bo));
    if (gbm_bo_destroy) gbm_bo_destroy(bo);
  }
}

// ── egl: bring EGL up on the GBM device (or surfaceless) ──────────────────────

static void resolve_egl(void) {
  p_eglGetProcAddress = load_sym(g_libegl, "eglGetProcAddress");
  p_eglGetPlatformDisplay = load_sym(g_libegl, "eglGetPlatformDisplay");
  p_eglGetPlatformDisplayEXT = load_sym(g_libegl, "eglGetPlatformDisplayEXT");
  p_eglGetDisplay = load_sym(g_libegl, "eglGetDisplay");
  p_eglInitialize = load_sym(g_libegl, "eglInitialize");
  p_eglQueryString = load_sym(g_libegl, "eglQueryString");
  p_eglChooseConfig = load_sym(g_libegl, "eglChooseConfig");
  p_eglBindAPI = load_sym(g_libegl, "eglBindAPI");
  p_eglCreateContext = load_sym(g_libegl, "eglCreateContext");
  p_eglCreatePbufferSurface = load_sym(g_libegl, "eglCreatePbufferSurface");
  p_eglMakeCurrent = load_sym(g_libegl, "eglMakeCurrent");
  p_eglGetError = load_sym(g_libegl, "eglGetError");
}

static void *egl_get_display(void) {
  // Mesa's preferred path: a GBM platform display on the device we opened.
  if (g_gbm_dev) {
    if (p_eglGetPlatformDisplay) {
      void *d = p_eglGetPlatformDisplay(EGL_PLATFORM_GBM_KHR, g_gbm_dev, NULL);
      if (d) return d;
    }
    if (p_eglGetPlatformDisplayEXT) {
      void *d = p_eglGetPlatformDisplayEXT(EGL_PLATFORM_GBM_KHR, g_gbm_dev, NULL);
      if (d) return d;
    }
    if (p_eglGetDisplay) {
      void *d = p_eglGetDisplay(g_gbm_dev);
      if (d) return d;
    }
  }
  // No GBM device: the surfaceless platform (llvmpipe/headless).
  if (p_eglGetPlatformDisplay) {
    void *d = p_eglGetPlatformDisplay(EGL_PLATFORM_SURFACELESS_MESA, NULL, NULL);
    if (d) return d;
  }
  if (p_eglGetDisplay) return p_eglGetDisplay(EGL_DEFAULT_DISPLAY);
  return EGL_NO_DISPLAY;
}

static void test_egl(void) {
  section("egl");
  if (!g_libegl) {
    skip("egl", "libEGL.so.1 not installed");
    return;
  }
  resolve_egl();
  if (!p_eglInitialize || !p_eglChooseConfig || !p_eglCreateContext || !p_eglMakeCurrent) {
    fail("egl symbols", "libEGL.so.1 missing core entry points", 0);
    return;
  }

  g_egl_dpy = egl_get_display();
  check(g_egl_dpy != EGL_NO_DISPLAY, "eglGetPlatformDisplay",
        "the display handle every GL client initialises", 0);
  if (g_egl_dpy == EGL_NO_DISPLAY) return;

  int major = 0, minor = 0;
  unsigned inited = p_eglInitialize(g_egl_dpy, &major, &minor);
  check(inited == EGL_TRUE, "eglInitialize",
        "Mesa loads the DRI driver here; failure is the usual black screen",
        0);
  if (inited != EGL_TRUE) {
    if (p_eglGetError) info("eglGetError: 0x%x", p_eglGetError());
    return;
  }
  info("EGL %d.%d", major, minor);
  if (p_eglQueryString) {
    const char *v = p_eglQueryString(g_egl_dpy, EGL_VENDOR);
    const char *ver = p_eglQueryString(g_egl_dpy, EGL_VERSION);
    const char *apis = p_eglQueryString(g_egl_dpy, EGL_CLIENT_APIS);
    if (v) info("EGL_VENDOR: %s", v);
    if (ver) info("EGL_VERSION: %s", ver);
    if (apis) info("EGL_CLIENT_APIS: %s", apis);
  }

  if (p_eglBindAPI)
    check(p_eglBindAPI(EGL_OPENGL_ES_API) == EGL_TRUE, "eglBindAPI(OpenGL ES)",
          "a GLES client binds the ES API before choosing a config", 0);

  int attribs[] = {EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                   EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
                   EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8,
                   EGL_ALPHA_SIZE, 8, EGL_NONE};
  void *cfg = NULL;
  int ncfg = 0;
  unsigned chose = p_eglChooseConfig(g_egl_dpy, attribs, &cfg, 1, &ncfg);
  if (chose != EGL_TRUE || ncfg == 0) {
    // Some surfaceless configs advertise no pbuffer bit -- drop the constraint.
    int any[] = {EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT, EGL_NONE};
    chose = p_eglChooseConfig(g_egl_dpy, any, &cfg, 1, &ncfg);
  }
  check(chose == EGL_TRUE && ncfg > 0, "eglChooseConfig",
        "an ES2-renderable framebuffer config", 0);
  if (chose != EGL_TRUE || ncfg == 0) return;
  g_egl_cfg = cfg;

  int ctx_attribs[] = {EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE};
  g_egl_ctx = p_eglCreateContext(g_egl_dpy, cfg, EGL_NO_CONTEXT, ctx_attribs);
  check(g_egl_ctx != EGL_NO_CONTEXT, "eglCreateContext(ES 2.0)",
        "the GL context the compositor renders in", 0);
  if (g_egl_ctx == EGL_NO_CONTEXT) {
    if (p_eglGetError) info("eglGetError: 0x%x", p_eglGetError());
    return;
  }

  // Prefer a surfaceless make-current (EGL_KHR_surfaceless_context); if the
  // driver rejects it, fall back to a 1x1 pbuffer.
  unsigned cur = p_eglMakeCurrent(g_egl_dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, g_egl_ctx);
  if (cur != EGL_TRUE && p_eglCreatePbufferSurface) {
    int pb[] = {EGL_WIDTH, 1, EGL_HEIGHT, 1, EGL_NONE};
    g_egl_surf = p_eglCreatePbufferSurface(g_egl_dpy, cfg, pb);
    if (g_egl_surf != EGL_NO_SURFACE)
      cur = p_eglMakeCurrent(g_egl_dpy, g_egl_surf, g_egl_surf, g_egl_ctx);
  }
  check(cur == EGL_TRUE, "eglMakeCurrent",
        "the context is now live; GL calls have an effect", 0);
  if (cur != EGL_TRUE && p_eglGetError) info("eglGetError: 0x%x", p_eglGetError());
}

// ── gl: does the current context actually render? ────────────────────────────

// Resolve a GL entry point via eglGetProcAddress first (works for core GLES on
// Mesa) then the GLESv2 library.
static void *gl_sym(const char *name) {
  void *p = NULL;
  if (p_eglGetProcAddress) p = p_eglGetProcAddress(name);
  if (!p && g_libgles) p = dlsym(g_libgles, name);
  return p;
}

static void test_gl(void) {
  section("gl");
  if (!g_egl_ctx) {
    skip("gl", "no current EGL context (egl section did not complete)");
    return;
  }

  const unsigned char *(*glGetString)(unsigned) = gl_sym("glGetString");
  unsigned (*glGetError)(void) = gl_sym("glGetError");
  if (!glGetString) {
    fail("glGetString", "cannot resolve the most basic GL entry point", 0);
    return;
  }
  const unsigned char *renderer = glGetString(GL_RENDERER);
  const unsigned char *vendor = glGetString(GL_VENDOR);
  const unsigned char *version = glGetString(GL_VERSION);
  const unsigned char *glsl = glGetString(GL_SHADING_LANGUAGE_VERSION);
  check(renderer != NULL, "glGetString(GL_RENDERER)",
        "which driver the context ended up on", 0);
  if (renderer) {
    snprintf(g_gl_renderer, sizeof g_gl_renderer, "%s", (const char *)renderer);
    info("GL_RENDERER: %s", renderer);
    // "llvmpipe" / "softpipe" / "swrast" mean the GPU path was not taken.
    if (strstr(g_gl_renderer, "llvmpipe") || strstr(g_gl_renderer, "softpipe") ||
        strstr(g_gl_renderer, "swrast") || strstr(g_gl_renderer, "SWR"))
      g_gl_swrast = 1;
    // llvmpipe reports its SIMD width, e.g. "llvmpipe (LLVM 22.1.3, 256 bits)":
    // 256 = AVX2, 128 = SSE only. On the same CPU, 128-bit is ~2x slower.
    const char *bits = strstr(g_gl_renderer, " bits");
    if (bits) {
      const char *p = bits;
      while (p > g_gl_renderer && (p[-1] == ' ' || (p[-1] >= '0' && p[-1] <= '9'))) p--;
      g_llvmpipe_bits = atoi(p);
    }
  }
  if (vendor) info("GL_VENDOR: %s", vendor);
  if (version) info("GL_VERSION: %s", version);
  if (glsl) info("GLSL: %s", glsl);

  // The render proof: an off-screen FBO cleared to blue, read back.
  void (*glGenFramebuffers)(int, unsigned *) = gl_sym("glGenFramebuffers");
  void (*glBindFramebuffer)(unsigned, unsigned) = gl_sym("glBindFramebuffer");
  void (*glGenRenderbuffers)(int, unsigned *) = gl_sym("glGenRenderbuffers");
  void (*glBindRenderbuffer)(unsigned, unsigned) = gl_sym("glBindRenderbuffer");
  void (*glRenderbufferStorage)(unsigned, unsigned, int, int) =
      gl_sym("glRenderbufferStorage");
  void (*glFramebufferRenderbuffer)(unsigned, unsigned, unsigned, unsigned) =
      gl_sym("glFramebufferRenderbuffer");
  unsigned (*glCheckFramebufferStatus)(unsigned) = gl_sym("glCheckFramebufferStatus");
  void (*glViewport)(int, int, int, int) = gl_sym("glViewport");
  void (*glClearColor)(float, float, float, float) = gl_sym("glClearColor");
  void (*glClear)(unsigned) = gl_sym("glClear");
  void (*glFinish)(void) = gl_sym("glFinish");
  void (*glReadPixels)(int, int, int, int, unsigned, unsigned, void *) =
      gl_sym("glReadPixels");
  if (!glGenFramebuffers || !glBindFramebuffer || !glGenRenderbuffers ||
      !glRenderbufferStorage || !glFramebufferRenderbuffer ||
      !glCheckFramebufferStatus || !glClearColor || !glClear || !glReadPixels) {
    skip("gl render", "FBO entry points unavailable; RENDERER-only check");
    return;
  }

  unsigned fbo = 0, rbo = 0;
  glGenFramebuffers(1, &fbo);
  glBindFramebuffer(GL_FRAMEBUFFER, fbo);
  glGenRenderbuffers(1, &rbo);
  glBindRenderbuffer(GL_RENDERBUFFER, rbo);
  glRenderbufferStorage(GL_RENDERBUFFER, GL_RGBA8, 16, 16);
  glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_RENDERBUFFER, rbo);
  unsigned st = glCheckFramebufferStatus(GL_FRAMEBUFFER);
  check(st == GL_FRAMEBUFFER_COMPLETE, "framebuffer complete",
        "an off-screen render target the driver accepts", 0);
  if (st != GL_FRAMEBUFFER_COMPLETE) return;

  if (glViewport) glViewport(0, 0, 16, 16);
  glClearColor(0.0f, 0.0f, 1.0f, 1.0f);  // blue
  glClear(GL_COLOR_BUFFER_BIT);
  if (glFinish) glFinish();

  unsigned char px[4] = {0, 0, 0, 0};
  glReadPixels(0, 0, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
  unsigned err = glGetError ? glGetError() : 0;
  // Expect blue: R low, G low, B high.
  int blue = px[2] > 200 && px[0] < 64 && px[1] < 64;
  g_gl_rendered = blue && err == GL_NO_ERROR;
  if (g_verbose) info("readback RGBA = %u,%u,%u,%u (glGetError 0x%x)",
                      px[0], px[1], px[2], px[3], err);
  check(g_gl_rendered, "glClear + glReadPixels == blue",
        "the GL driver truly rasterised (drm-probe's tone, drawn and read back)",
        0);
}

// ── perf: WHY is glxgears slow? the factors that govern a software GL ────────
//
// glxgears at 1,400 fps here vs 12,000 on a Linux host, with GL_RENDERER =
// llvmpipe on both, is not a "missing GPU" problem -- it is the *same software
// rasteriser* running slower. Three things set llvmpipe's speed, and this
// section measures each in the guest so the ratio can be attributed:
//
//   1. SIMD width   llvmpipe JITs to the widest vector the CPU advertises.
//                   256-bit (AVX2) does ~2x the pixels per instruction of
//                   128-bit (SSE). The RENDERER string already told us which.
//   2. worker cores llvmpipe splits the framebuffer across threads, ~linearly.
//                   Half the online CPUs is ~half the fill rate.
//   3. present cost glxgears also calls glXSwapBuffers every frame, which under
//                   Xwayland copies the result through wl_shm to the compositor
//                   to virtio-gpu -- a per-frame cost this section isolates by
//                   measuring pure render (no window) two ways: batched, and
//                   with a glFinish per "frame" (the swap-like sync).
//
// If the batched render rate here is high but glxgears is low, the bottleneck
// is the present path, not the rasteriser; if this section is itself slow, it
// is the CPU/SIMD/thread factors above. Opt-in (--bench): it spins the CPU.

static double now_ms(void) {
  struct timespec t;
  clock_gettime(CLOCK_MONOTONIC, &t);
  return (double)t.tv_sec * 1000.0 + (double)t.tv_nsec / 1.0e6;
}

// Whole-token search in a space-padded /proc/cpuinfo flags line ("avx" must not
// match inside "avx2"/"avx512f").
static int has_flag(const char *padded, const char *tok) {
  char needle[32];
  snprintf(needle, sizeof needle, " %s ", tok);
  return strstr(padded, needle) != NULL;
}

static void report_cpu(void) {
  long onln = sysconf(_SC_NPROCESSORS_ONLN);
  long conf = sysconf(_SC_NPROCESSORS_CONF);
  info("CPUs: %ld online / %ld configured  (llvmpipe fill scales ~linearly)",
       onln, conf);
  const char *lp = getenv("LP_NUM_THREADS");
  if (lp) info("LP_NUM_THREADS=%s  (caps llvmpipe worker threads)", lp);
  const char *gd = getenv("GALLIUM_DRIVER");
  if (gd) info("GALLIUM_DRIVER=%s", gd);

  FILE *f = fopen("/proc/cpuinfo", "r");
  if (f) {
    char line[8192], flags[8192] = "";
    while (fgets(line, sizeof line, f)) {
      if (!strncmp(line, "flags", 5) || !strncmp(line, "Features", 8)) {
        char *c = strchr(line, ':');
        if (c) snprintf(flags, sizeof flags, " %s ", c + 1);
        // squash the trailing newline that landed inside the padding
        for (char *p = flags; *p; p++)
          if (*p == '\n') *p = ' ';
        break;
      }
    }
    fclose(f);
    if (flags[0]) {
      info("CPU SIMD: sse4_2=%d avx=%d avx2=%d fma=%d f16c=%d avx512f=%d",
           has_flag(flags, "sse4_2"), has_flag(flags, "avx"),
           has_flag(flags, "avx2"), has_flag(flags, "fma"),
           has_flag(flags, "f16c"), has_flag(flags, "avx512f"));
      if (!has_flag(flags, "avx2"))
        info("  -> no AVX2: llvmpipe is capped at 128-bit SSE. An AVX2 host runs "
             "256-bit, ~2x faster. This is likely half of the glxgears gap.");
    }
  }

  if (g_llvmpipe_bits)
    check(g_llvmpipe_bits >= 256, "llvmpipe SIMD width >= 256-bit (AVX2)",
          g_llvmpipe_bits == 128
              ? "128-bit (SSE) -- ~2x slower than a 256-bit AVX2 host"
              : "vector width from GL_RENDERER",
          0);
}

// Compile a GLES2 shader; returns 0 on failure.
static unsigned make_shader(unsigned type, const char *src,
                            unsigned (*glCreateShader)(unsigned),
                            void (*glShaderSource)(unsigned, int, const char *const *, const int *),
                            void (*glCompileShader)(unsigned),
                            void (*glGetShaderiv)(unsigned, unsigned, int *)) {
  unsigned s = glCreateShader(type);
  if (!s) return 0;
  glShaderSource(s, 1, &src, NULL);
  glCompileShader(s);
  int ok = 0;
  if (glGetShaderiv) glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
  return ok ? s : 0;
}

static void test_perf(void) {
  section("perf");
  report_cpu();
  if (!g_bench) {
    skip("perf bench", "pass --bench to run the on-CPU render throughput test");
    return;
  }
  if (!g_egl_ctx || !g_gl_rendered) {
    skip("perf bench", "no working GL context (gl section did not render)");
    return;
  }

  // Resolve the GLES2 draw path.
  unsigned (*glCreateShader)(unsigned) = gl_sym("glCreateShader");
  void (*glShaderSource)(unsigned, int, const char *const *, const int *) =
      gl_sym("glShaderSource");
  void (*glCompileShader)(unsigned) = gl_sym("glCompileShader");
  void (*glGetShaderiv)(unsigned, unsigned, int *) = gl_sym("glGetShaderiv");
  unsigned (*glCreateProgram)(void) = gl_sym("glCreateProgram");
  void (*glAttachShader)(unsigned, unsigned) = gl_sym("glAttachShader");
  void (*glLinkProgram)(unsigned) = gl_sym("glLinkProgram");
  void (*glGetProgramiv)(unsigned, unsigned, int *) = gl_sym("glGetProgramiv");
  void (*glUseProgram)(unsigned) = gl_sym("glUseProgram");
  int (*glGetAttribLocation)(unsigned, const char *) = gl_sym("glGetAttribLocation");
  void (*glGenBuffers)(int, unsigned *) = gl_sym("glGenBuffers");
  void (*glBindBuffer)(unsigned, unsigned) = gl_sym("glBindBuffer");
  void (*glBufferData)(unsigned, long, const void *, unsigned) = gl_sym("glBufferData");
  void (*glVertexAttribPointer)(unsigned, int, unsigned, unsigned char, int, const void *) =
      gl_sym("glVertexAttribPointer");
  void (*glEnableVertexAttribArray)(unsigned) = gl_sym("glEnableVertexAttribArray");
  void (*glDrawArrays)(unsigned, int, int) = gl_sym("glDrawArrays");
  void (*glViewport)(int, int, int, int) = gl_sym("glViewport");
  void (*glClear)(unsigned) = gl_sym("glClear");
  void (*glClearColor)(float, float, float, float) = gl_sym("glClearColor");
  void (*glFinish)(void) = gl_sym("glFinish");
  void (*glGenFramebuffers)(int, unsigned *) = gl_sym("glGenFramebuffers");
  void (*glBindFramebuffer)(unsigned, unsigned) = gl_sym("glBindFramebuffer");
  void (*glGenRenderbuffers)(int, unsigned *) = gl_sym("glGenRenderbuffers");
  void (*glBindRenderbuffer)(unsigned, unsigned) = gl_sym("glBindRenderbuffer");
  void (*glRenderbufferStorage)(unsigned, unsigned, int, int) = gl_sym("glRenderbufferStorage");
  void (*glFramebufferRenderbuffer)(unsigned, unsigned, unsigned, unsigned) =
      gl_sym("glFramebufferRenderbuffer");
  if (!glCreateShader || !glCreateProgram || !glGenBuffers || !glDrawArrays ||
      !glViewport || !glClear || !glFinish || !glGenFramebuffers) {
    skip("perf bench", "GLES2 draw entry points unavailable");
    return;
  }

  // A 512x512 off-screen target -- a small window, like glxgears' default.
  const int W = 512, H = 512;
  unsigned fbo = 0, rbo = 0;
  glGenFramebuffers(1, &fbo);
  glBindFramebuffer(GL_FRAMEBUFFER, fbo);
  glGenRenderbuffers(1, &rbo);
  glBindRenderbuffer(GL_RENDERBUFFER, rbo);
  glRenderbufferStorage(GL_RENDERBUFFER, GL_RGBA8, W, H);
  glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_RENDERBUFFER, rbo);
  glViewport(0, 0, W, H);

  static const char *VS =
      "attribute vec2 pos;\n"
      "void main(){ gl_Position = vec4(pos, 0.0, 1.0); }\n";
  static const char *FS =
      "precision mediump float;\n"
      "void main(){ gl_FragColor = vec4(0.30, 0.60, 0.90, 1.0); }\n";
  unsigned vs = make_shader(GL_VERTEX_SHADER, VS, glCreateShader, glShaderSource,
                            glCompileShader, glGetShaderiv);
  unsigned fs = make_shader(GL_FRAGMENT_SHADER, FS, glCreateShader, glShaderSource,
                            glCompileShader, glGetShaderiv);
  if (!vs || !fs) {
    fail("compile shaders", "the GLES2 compiler rejected a trivial shader", 0);
    return;
  }
  unsigned prog = glCreateProgram();
  glAttachShader(prog, vs);
  glAttachShader(prog, fs);
  glLinkProgram(prog);
  int linked = 0;
  if (glGetProgramiv) glGetProgramiv(prog, GL_LINK_STATUS, &linked);
  check(linked, "link program", "a minimal GLES2 pipeline", 0);
  if (!linked) return;
  glUseProgram(prog);

  // A batch of small triangles spread over the viewport: geometry setup + fill,
  // the same mix a spinning gear presents.
  const int NTRI = 4000;
  float *verts = malloc((size_t)NTRI * 3 * 2 * sizeof(float));
  if (!verts) {
    skip("perf bench", "out of memory building the vertex batch");
    return;
  }
  for (int i = 0; i < NTRI; i++) {
    // deterministic pseudo-random placement in clip space [-1,1]
    unsigned r = (unsigned)(i * 2654435761u);
    float cx = ((float)((r >> 3) & 1023) / 1023.0f) * 2.0f - 1.0f;
    float cy = ((float)((r >> 13) & 1023) / 1023.0f) * 2.0f - 1.0f;
    float s = 0.06f;
    float *v = &verts[i * 6];
    v[0] = cx;     v[1] = cy + s;
    v[2] = cx - s; v[3] = cy - s;
    v[4] = cx + s; v[5] = cy - s;
  }
  unsigned vbo = 0;
  glGenBuffers(1, &vbo);
  glBindBuffer(GL_ARRAY_BUFFER, vbo);
  glBufferData(GL_ARRAY_BUFFER, (long)NTRI * 3 * 2 * sizeof(float), verts, GL_STATIC_DRAW);
  int loc = glGetAttribLocation ? glGetAttribLocation(prog, "pos") : 0;
  if (loc < 0) loc = 0;
  glVertexAttribPointer((unsigned)loc, 2, GL_FLOAT, GL_FALSE, 0, NULL);
  glEnableVertexAttribArray((unsigned)loc);
  free(verts);

  if (glClearColor) glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
  const int verts_total = NTRI * 3;

  // (a) batched: draw many frames, one glFinish at the end -> raw raster rate,
  //     what llvmpipe can push when not stalled on per-frame sync.
  int frames = 0;
  double t0 = now_ms(), budget = 400.0;
  while (frames < 100000) {
    glClear(GL_COLOR_BUFFER_BIT);
    glDrawArrays(GL_TRIANGLES, 0, verts_total);
    frames++;
    if ((frames & 15) == 0 && now_ms() - t0 >= budget) break;
  }
  glFinish();
  double batched = (double)frames * 1000.0 / (now_ms() - t0);

  // (b) synced: a glFinish after each frame -> the per-frame sync a swap forces.
  int f2 = 0;
  double t1 = now_ms();
  while (f2 < 100000) {
    glClear(GL_COLOR_BUFFER_BIT);
    glDrawArrays(GL_TRIANGLES, 0, verts_total);
    glFinish();
    f2++;
    if (now_ms() - t1 >= budget) break;
  }
  double synced = (double)f2 * 1000.0 / (now_ms() - t1);
  g_bench_fps = synced;

  double mtris = batched * (double)NTRI / 1.0e6;
  info("render %dx%d, %d tris/frame:", W, H, NTRI);
  info("  batched (finish/run):  %8.0f fps   %6.1f Mtri/s", batched, mtris);
  info("  synced  (finish/frame):%8.0f fps   (the glXSwapBuffers-style stall)", synced);
  if (synced > 0 && batched / synced > 1.5)
    info("  -> per-frame sync costs %.0f%%: glxgears' swap adds this on top.",
         (batched / synced - 1.0) * 100.0);
  ok("render throughput measured", "an in-guest number to compare with the host");
}

// ── vulkan: which ICDs did the loader find? ──────────────────────────────────

typedef struct {
  uint32_t sType;
  const void *pNext;
  const char *pApplicationName;
  uint32_t applicationVersion;
  const char *pEngineName;
  uint32_t engineVersion;
  uint32_t apiVersion;
} VkApplicationInfo;

typedef struct {
  uint32_t sType;
  const void *pNext;
  uint32_t flags;
  const VkApplicationInfo *pApplicationInfo;
  uint32_t enabledLayerCount;
  const char *const *ppEnabledLayerNames;
  uint32_t enabledExtensionCount;
  const char *const *ppEnabledExtensionNames;
} VkInstanceCreateInfo;

typedef struct {
  uint32_t apiVersion;
  uint32_t driverVersion;
  uint32_t vendorID;
  uint32_t deviceID;
  uint32_t deviceType;
  char deviceName[256];
  uint8_t pipelineCacheUUID[16];
  // limits + sparseProperties follow; over-size the tail so the driver's
  // write of the whole VkPhysicalDeviceProperties never runs past this struct.
  uint8_t tail[1024];
} VkPhysicalDeviceProperties;

static const char *vk_device_type(uint32_t t) {
  switch (t) {
    case 1: return "integrated GPU";
    case 2: return "discrete GPU";
    case 3: return "virtual GPU";
    case 4: return "CPU (software)";
    default: return "other";
  }
}

static void test_vulkan(void) {
  section("vulkan");
  if (!g_libvk) {
    skip("vulkan", "libvulkan.so.1 not installed");
    return;
  }
  void *(*vkGetInstanceProcAddr)(void *, const char *) =
      load_sym(g_libvk, "vkGetInstanceProcAddr");
  if (!vkGetInstanceProcAddr) {
    fail("vkGetInstanceProcAddr", "libvulkan.so.1 missing the loader entry point", 0);
    return;
  }
  int (*vkCreateInstance)(const VkInstanceCreateInfo *, const void *, void **) =
      vkGetInstanceProcAddr(NULL, "vkCreateInstance");
  if (!vkCreateInstance) {
    fail("vkCreateInstance lookup", "loader did not resolve vkCreateInstance", 0);
    return;
  }

  VkApplicationInfo app = {0};
  app.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO;
  app.pApplicationName = "gfx-probe";
  app.apiVersion = VK_API_VERSION_1_0;
  VkInstanceCreateInfo ci = {0};
  ci.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO;
  ci.pApplicationInfo = &app;

  void *inst = NULL;
  int r = vkCreateInstance(&ci, NULL, &inst);
  check(r == VK_SUCCESS && inst, "vkCreateInstance",
        "the Vulkan loader initialises and finds at least one ICD", 0);
  if (r != VK_SUCCESS || !inst) {
    info("VkResult = %d (no usable ICD -> no Vulkan)", r);
    return;
  }

  int (*vkEnumeratePhysicalDevices)(void *, uint32_t *, void **) =
      vkGetInstanceProcAddr(inst, "vkEnumeratePhysicalDevices");
  void (*vkGetPhysicalDeviceProperties)(void *, VkPhysicalDeviceProperties *) =
      vkGetInstanceProcAddr(inst, "vkGetPhysicalDeviceProperties");
  void (*vkDestroyInstance)(void *, const void *) =
      vkGetInstanceProcAddr(inst, "vkDestroyInstance");

  if (vkEnumeratePhysicalDevices && vkGetPhysicalDeviceProperties) {
    uint32_t n = 0;
    vkEnumeratePhysicalDevices(inst, &n, NULL);
    check(n > 0, "vkEnumeratePhysicalDevices",
          "at least one Vulkan device (lavapipe swrast counts)", 0);
    g_vk_devs = (int)n;
    if (n > 0) {
      if (n > 8) n = 8;
      void *devs[8];
      vkEnumeratePhysicalDevices(inst, &n, devs);
      for (uint32_t i = 0; i < n; i++) {
        VkPhysicalDeviceProperties pr;
        memset(&pr, 0, sizeof pr);
        vkGetPhysicalDeviceProperties(devs[i], &pr);
        info("device %u: %s [%s, API %u.%u.%u]", i, pr.deviceName,
             vk_device_type(pr.deviceType), pr.apiVersion >> 22,
             (pr.apiVersion >> 12) & 0x3ff, pr.apiVersion & 0xfff);
      }
    }
  }
  if (vkDestroyInstance) vkDestroyInstance(inst, NULL);
}

// ── wayland: is a compositor listening, and what does it advertise? ──────────

// Registry listener: record the globals a compositor a client would use.
static void reg_global(void *data, void *reg, uint32_t name, const char *iface,
                       uint32_t version) {
  (void)data;
  (void)reg;
  if (g_verbose) info("global: %s v%u (name %u)", iface, version, name);
  if (!strcmp(iface, "wl_compositor")) g_wl_have_compositor = 1;
  else if (!strcmp(iface, "xdg_wm_base")) g_wl_have_xdg = 1;
  else if (!strcmp(iface, "zwp_linux_dmabuf_v1")) g_wl_have_dmabuf = 1;
  else if (!strcmp(iface, "zwlr_layer_shell_v1")) g_wl_have_layer = 1;
  else if (!strcmp(iface, "wl_shm")) {
    g_wl_shm_name = name;
    g_wl_shm_ver = version;
  }
}
static void reg_global_remove(void *data, void *reg, uint32_t name) {
  (void)data;
  (void)reg;
  (void)name;
}

// Wayland entry points needed by both the wayland and shm sections.
static void *(*p_wl_connect)(const char *);
static void (*p_wl_disconnect)(void *);
static int (*p_wl_roundtrip)(void *);
static uint32_t (*p_wl_get_version)(void *);
static int (*p_wl_add_listener)(void *, void (**)(void), void *);
static void (*p_wl_proxy_destroy)(void *);
// wl_proxy_marshal_flags(proxy, opcode, interface, version, flags, ...)
static void *(*p_wl_marshal_flags)(void *, uint32_t, const void *, uint32_t,
                                   uint32_t, ...);
static void *g_wl_registry;
static void *g_iface_registry, *g_iface_shm, *g_iface_shm_pool, *g_iface_buffer;

static void test_wayland(void) {
  section("wayland");
  if (!g_libwl) {
    skip("wayland", "libwayland-client.so.0 not installed");
    return;
  }
  const char *disp = getenv("WAYLAND_DISPLAY");
  if (!disp || !*disp) {
    skip("wayland", "WAYLAND_DISPLAY unset (no session; run from a compositor)");
    return;
  }

  p_wl_connect = load_sym(g_libwl, "wl_display_connect");
  p_wl_disconnect = load_sym(g_libwl, "wl_display_disconnect");
  p_wl_roundtrip = load_sym(g_libwl, "wl_display_roundtrip");
  p_wl_get_version = load_sym(g_libwl, "wl_proxy_get_version");
  p_wl_add_listener = load_sym(g_libwl, "wl_proxy_add_listener");
  p_wl_proxy_destroy = load_sym(g_libwl, "wl_proxy_destroy");
  p_wl_marshal_flags = load_sym(g_libwl, "wl_proxy_marshal_flags");
  g_iface_registry = load_sym(g_libwl, "wl_registry_interface");
  g_iface_shm = load_sym(g_libwl, "wl_shm_interface");
  g_iface_shm_pool = load_sym(g_libwl, "wl_shm_pool_interface");
  g_iface_buffer = load_sym(g_libwl, "wl_buffer_interface");
  if (!p_wl_connect || !p_wl_roundtrip || !p_wl_add_listener ||
      !p_wl_marshal_flags || !g_iface_registry) {
    fail("wayland symbols", "libwayland-client.so.0 missing marshal/registry ABI", 0);
    return;
  }

  g_wl_display = p_wl_connect(disp);
  check(g_wl_display != NULL, "wl_display_connect",
        "the compositor socket a Wayland client opens", errno);
  if (!g_wl_display) return;
  info("connected to WAYLAND_DISPLAY=%s", disp);

  // wl_display.get_registry (opcode 1) -> a new wl_registry proxy.
  uint32_t dver = p_wl_get_version ? p_wl_get_version(g_wl_display) : 1;
  g_wl_registry = p_wl_marshal_flags(g_wl_display, WL_DISPLAY_GET_REGISTRY,
                                     g_iface_registry, dver, 0, NULL);
  check(g_wl_registry != NULL, "wl_display.get_registry",
        "the object every global is advertised through", 0);
  if (!g_wl_registry) return;

  void (*listener[2])(void) = {(void (*)(void))reg_global,
                               (void (*)(void))reg_global_remove};
  p_wl_add_listener(g_wl_registry, listener, NULL);
  int rt = p_wl_roundtrip(g_wl_display);
  check(rt >= 0, "wl_display_roundtrip",
        "the compositor answered and sent its globals", 0);

  check(g_wl_have_compositor, "wl_compositor advertised",
        "without it no surface can be created", 0);
  info("xdg_wm_base: %s   wl_shm: %s   linux-dmabuf: %s   layer-shell: %s",
       g_wl_have_xdg ? "yes" : "no", g_wl_shm_name ? "yes" : "no",
       g_wl_have_dmabuf ? "yes" : "no", g_wl_have_layer ? "yes" : "no");
}

// ── shm: the shared-memory buffer path a wl_shm client presents through ──────

static void test_shm(void) {
  section("shm");
  if (!g_wl_display || !g_wl_registry) {
    skip("shm", "no Wayland connection (wayland section did not complete)");
    return;
  }
  if (!g_wl_shm_name || !g_iface_shm || !g_iface_shm_pool || !g_iface_buffer) {
    skip("shm", "compositor advertised no wl_shm, or ABI symbols missing");
    return;
  }

  // Bind wl_shm: wl_registry.bind (opcode 0), signature "usun".
  uint32_t ver = g_wl_shm_ver;
  void *shm = p_wl_marshal_flags(g_wl_registry, WL_REGISTRY_BIND, g_iface_shm,
                                 ver, 0, g_wl_shm_name, "wl_shm", ver, NULL);
  check(shm != NULL, "bind wl_shm", "the pixel-buffer factory a wl_shm client uses", 0);
  if (!shm) return;

  const int w = 64, h = 64, stride = w * 4;
  const int size = stride * h;
  // A memfd-backed shared buffer, the way foot/lunarbg back a wl_shm pool.
  int fd = -1;
#ifdef __NR_memfd_create
  fd = (int)syscall(__NR_memfd_create, "gfx-probe-shm", 0u);
#endif
  if (fd < 0) {
    char name[64];
    snprintf(name, sizeof name, "/gfx-probe-%d", (int)getpid());
    fd = shm_open(name, O_RDWR | O_CREAT | O_EXCL, 0600);
    if (fd >= 0) shm_unlink(name);
  }
  if (fd < 0) {
    fail("shm fd", "no memfd_create or shm_open for the pool backing store", errno);
    if (p_wl_proxy_destroy) p_wl_proxy_destroy(shm);
    return;
  }
  if (ftruncate(fd, size) != 0) {
    fail("ftruncate pool", "cannot size the shared buffer", errno);
    close(fd);
    if (p_wl_proxy_destroy) p_wl_proxy_destroy(shm);
    return;
  }
  void *map = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  if (map != MAP_FAILED) {
    memset(map, 0x20, size);  // a flat grey; never attached to a surface
    munmap(map, size);
  }

  // wl_shm.create_pool (opcode 0), signature "nhi": new_id, fd, size.
  uint32_t sver = p_wl_get_version ? p_wl_get_version(shm) : ver;
  void *pool = p_wl_marshal_flags(shm, WL_SHM_CREATE_POOL, g_iface_shm_pool, sver,
                                  0, NULL, fd, size);
  check(pool != NULL, "wl_shm.create_pool",
        "the compositor maps the client's shared memory", 0);

  void *buffer = NULL;
  if (pool) {
    // wl_shm_pool.create_buffer (opcode 0), signature "niiiiu".
    buffer = p_wl_marshal_flags(pool, WL_SHM_POOL_CREATE_BUFFER, g_iface_buffer,
                                sver, 0, NULL, 0, w, h, stride,
                                WL_SHM_FORMAT_XRGB8888);
    check(buffer != NULL, "wl_shm_pool.create_buffer",
          "a presentable XRGB8888 buffer (never shown -- no surface)", 0);
  }

  // A roundtrip forces any protocol error (bad stride/format) to surface as a
  // disconnect; if the compositor accepted the buffer, this returns >= 0.
  int rt = p_wl_roundtrip(g_wl_display);
  check(rt >= 0, "roundtrip after buffer creation",
        "the compositor did not reject the shared buffer", 0);

  close(fd);
  if (p_wl_proxy_destroy) {
    if (buffer) p_wl_proxy_destroy(buffer);
    if (pool) p_wl_proxy_destroy(pool);
    p_wl_proxy_destroy(shm);
  }
}

// ── verdict ───────────────────────────────────────────────────────────────

static void verdict(void) {
  section("verdict");
  int gl_ok = g_egl_ctx && g_gl_rendered;
  printf("  GBM: %s%s%s\n",
         g_gbm_dev ? "device created" : "NOT created (no buffer allocator)",
         g_gbm_backend[0] ? " backend=" : "", g_gbm_backend);
  printf("  EGL/GL: %s\n",
         gl_ok ? (g_gl_swrast ? "renders, but on a SOFTWARE rasteriser (llvmpipe)"
                              : "renders on a hardware driver")
               : "does NOT render -- GL clients see a black/empty window");
  if (g_gl_renderer[0]) printf("        renderer: %s\n", g_gl_renderer);
  if (g_gl_swrast && g_llvmpipe_bits)
    printf("        llvmpipe SIMD: %d-bit%s\n", g_llvmpipe_bits,
           g_llvmpipe_bits < 256 ? "  (no AVX2 -> ~2x slower than a 256-bit host)" : "");
  if (g_bench_fps > 0)
    printf("        render bench: %.0f fps synced (see [perf] for the glxgears gap)\n",
           g_bench_fps);
  printf("  Vulkan: %s\n",
         g_vk_devs > 0 ? "at least one device (ICD present)"
                       : "no device -- Vulkan clients (Zink, vkcube) fail");
  printf("  Wayland: %s\n",
         g_wl_display ? (g_wl_have_compositor ? "compositor answered with a usable surface path"
                                              : "connected but no wl_compositor global")
                      : "no session (WAYLAND_DISPLAY unset or connect failed)");
  if (gl_ok && !g_gl_swrast)
    printf("  => the full GL path is hardware-accelerated; the desktop should be fluid.\n");
  else if (gl_ok)
    printf("  => GL works but in software; the desktop will draw, slowly.\n");
  else
    printf("  => the userspace GL path is broken above the kernel -- this is the black window.\n");
}

int main(int argc, char **argv) {
  for (int i = 1; i < argc; i++) {
    if (!strcmp(argv[i], "-v")) g_verbose = 1;
    else if (!strcmp(argv[i], "--bench")) g_bench = 1;
    else if (!strcmp(argv[i], "--card") && i + 1 < argc) g_card = atoi(argv[++i]);
    else if (!strcmp(argv[i], "--skip") && i + 1 < argc && g_nskipped < 16)
      g_skipped[g_nskipped++] = argv[++i];
    else if (!strcmp(argv[i], "-h") || !strcmp(argv[i], "--help")) {
      printf("usage: gfx-probe [-v] [--bench] [--card N] [--skip SECTION]...\n"
             "Drives every userspace graphics layer a GL/Vulkan/Wayland client rests\n"
             "on -- GBM buffer allocation, EGL bring-up, an off-screen GL render it\n"
             "reads back, Vulkan device enumeration, and a live Wayland/wl_shm round-\n"
             "trip -- and says whether the desktop's graphics path actually works.\n"
             "The companion to drm-probe: drm-probe drives the kernel, this drives Mesa.\n"
             "Opens no window; all libraries are dlopen'd, so a missing one just SKIPs.\n"
             "--bench   run the perf section's on-CPU render throughput bench and the\n"
             "          llvmpipe SIMD/thread report -- WHY software GL (glxgears) is slow.\n"
             "--card N  use /dev/dri/renderD(128+N) for the GBM device.\n"
             "--skip SECTION leaves one out (libs, gbm, egl, gl, perf, vulkan, wayland, shm).\n");
      return 0;
    }
  }

  printf("Userspace graphics-stack probe (render node card %d)\n", g_card);
  printf("Each check mirrors what Mesa or a Wayland client asks of the stack.\n");

  struct {
    const char *name;
    void (*run)(void);
  } sections[] = {
      {"libs", test_libs},     {"gbm", test_gbm},         {"egl", test_egl},
      {"gl", test_gl},         {"perf", test_perf},       {"vulkan", test_vulkan},
      {"wayland", test_wayland}, {"shm", test_shm},
  };
  for (size_t i = 0; i < sizeof sections / sizeof sections[0]; i++) {
    if (skipped(sections[i].name)) {
      printf("\n[%s] skipped (--skip)\n", sections[i].name);
      continue;
    }
    sections[i].run();
  }

  verdict();

  // Tear down what is still live (best-effort; the process is about to exit).
  if (g_egl_dpy && p_eglMakeCurrent)
    p_eglMakeCurrent(g_egl_dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
  if (g_wl_display && p_wl_disconnect) p_wl_disconnect(g_wl_display);

  printf("\n%d passed, %d failed, %d skipped\n", g_pass, g_fail, g_skip);
  if (g_fail == 0) printf("Nothing here stands in the way of the desktop.\n");
  return g_fail > 125 ? 125 : g_fail;
}
