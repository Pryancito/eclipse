/*
 * OUR code, not NVIDIA's -- a tiny shim for the handful of real NVIDIA
 * symbols that are variadic C functions. Stable Rust cannot export a
 * variadic `extern "C" fn` (only import one), so the variadic side has to
 * live in C; this just forwards to a fixed-arity Rust-exported logger,
 * dropping the format arguments for now (TODO: a real freestanding
 * vsnprintf once formatted NVIDIA diagnostics actually matter -- for now
 * this only needs to link and not crash).
 *
 * nvDbg_Printf is NVIDIA's real printf backend (see
 * src/nvidia/inc/kernel/core/printf.h and inc/libraries/utils/nvprintf.h,
 * NVRM_PRINTF_FUNCTION) -- confirmed as a real link-time dependency of
 * nvassert.c's NV_ASSERT_PRINTF path, not something we invented.
 */

extern void nvrm_shim_log_raw(const char *msg);

void nvDbg_Printf(const char *file, int line, const char *function, int debuglevel, const char *s, ...)
{
    (void)file;
    (void)line;
    (void)function;
    (void)debuglevel;
    nvrm_shim_log_raw(s);
}
