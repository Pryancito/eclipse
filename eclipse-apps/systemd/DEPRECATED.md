# DEPRECATED - Migrated to S6

This directory contains the old systemd implementation which has been **replaced by S6**.

## Migration Complete

Eclipse OS now uses the **S6 supervision suite** as its init system, located in:
- `eclipse-apps/s6/` - Main S6 implementation

## Why S6?

S6 provides:
- **Perfect Modularity**: Each component does one thing well
- **Minimal Footprint**: ~200KB vs systemd ~10MB  
- **Better Reliability**: Designed for 24/7 uptime
- **Simplicity**: Shell scripts instead of complex configuration files

## This Directory

This directory is kept for historical reference only and will be removed in a future release.

For the current init system, see:
- [eclipse-apps/s6/README.md](../s6/README.md)
- [Main README.md](../../README.md#sistema-s6)

---

**Status**: DEPRECATED  
**Replacement**: eclipse-apps/s6/  
**Last Updated**: 2026-01-30
