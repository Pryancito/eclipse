/*
 * OUR code, not NVIDIA's -- a tiny shim for the handful of real NVIDIA
 * symbols that are variadic C functions. Stable Rust cannot export a
 * variadic `extern "C" fn` (only import one), so the variadic side has to
 * live in C; this just forwards to a fixed-arity Rust-exported logger,
 * dropping the format arguments for now (TODO: a real freestanding
 * vsnprintf once formatted NVIDIA diagnostics actually matter -- for now
 * this only needs to link and not crash).
 *
 * nv_printf is os-interface.h's real printf entry point (see
 * arch/nvalloc/unix/include/os-interface.h) -- confirmed as a real
 * link-time dependency of diagnostics/nvlog_printf.c, which is itself
 * NVIDIA's real implementation of nvDbg_Printf (nvlog_printf.c calls
 * nv_printf internally), so nvDbg_Printf no longer needs a hand-written
 * stand-in here once that file is vendored -- only its own OS-specific
 * backend does.
 */

extern void nvrm_shim_log_raw(const char *msg);

int nv_printf(unsigned int debuglevel, const char *printf_format, ...)
{
    (void)debuglevel;
    nvrm_shim_log_raw(printf_format);
    return 0;
}
