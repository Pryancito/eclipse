# NVIDIA GPU Support in Eclipse OS

Eclipse OS includes support for NVIDIA GPUs through integration with NVIDIA's open-source GPU kernel modules.

## Overview

The NVIDIA GPU support in Eclipse OS is built on top of [NVIDIA's open-gpu-kernel-modules](https://github.com/NVIDIA/open-gpu-kernel-modules), which provides open-source kernel drivers for modern NVIDIA GPUs.

## Supported GPUs

Based on the open-gpu-kernel-modules, Eclipse OS supports:

### Turing Architecture (2018)
- GeForce RTX 2080 Ti
- GeForce RTX 2080 Super / 2080
- GeForce RTX 2070 Super / 2070
- GeForce RTX 2060 Super / 2060

### Ampere Architecture (2020)
- GeForce RTX 3090
- GeForce RTX 3080 Ti / 3080
- GeForce RTX 3070
- GeForce RTX 3060 Ti / 3060

### Ada Lovelace Architecture (2022)
- GeForce RTX 4090
- GeForce RTX 4080
- GeForce RTX 4070 Ti / 4070
- GeForce RTX 4060 Ti / 4060

### Hopper Architecture (2022)
- H100 and newer datacenter GPUs

**Note:** NVIDIA's open-gpu-kernel-modules require Turing architecture or newer. Older GPUs (Pascal, Maxwell, etc.) are not supported by the open-source kernel modules.

## Architecture

The NVIDIA support in Eclipse OS is divided into three layers:

### 1. Kernel Driver (`eclipse_kernel/src/nvidia.rs`)

The kernel driver provides:
- PCI device detection and enumeration
- GPU identification (device ID, architecture, specifications)
- BAR (Base Address Register) mapping for memory access
- Basic GPU initialization
- Multi-GPU support

### 2. Display Service (`eclipse_kernel/userspace/display_service`)

The display service handles:
- Graphics output and framebuffer operations
- Display mode configuration
- Driver selection (NVIDIA vs VESA fallback)
- Frame rendering and V-Sync

### 3. Driver Loader (`userland/driver_loader`)

The driver loader system provides:
- Dynamic driver loading
- IPC-based driver communication
- CUDA and ray tracing capability management

## Features

### Current Features

- ✅ **PCI Detection**: Automatic detection of NVIDIA GPUs via PCI bus scanning
- ✅ **GPU Identification**: Recognizes specific GPU models and architectures
- ✅ **Architecture Detection**: Identifies Turing, Ampere, Ada Lovelace, and Hopper
- ✅ **Specifications**: Reports CUDA cores, SM count, and VRAM size
- ✅ **Multi-GPU Support**: Detects and initializes multiple NVIDIA GPUs
- ✅ **Device Enablement**: Configures I/O, Memory, and Bus Master modes

### Planned Features

- 🔄 **Full Driver Integration**: Complete integration with open-gpu-kernel-modules
- 🔄 **CUDA Support**: User-space CUDA runtime for compute workloads
- 🔄 **Ray Tracing**: RT core support for real-time ray tracing
- 🔄 **Display Output**: Direct display output via DisplayPort/HDMI
- 🔄 **Power Management**: GPU power states and frequency management
- 🔄 **Video Decode/Encode**: NVDEC/NVENC hardware acceleration

## Integration with open-gpu-kernel-modules

Eclipse OS is designed to work with NVIDIA's open-source kernel modules:

### What open-gpu-kernel-modules Provides

- **kernel-open**: Open-source kernel driver code
- **Device Initialization**: Sequences for initializing different GPU architectures
- **Register Documentation**: Information about GPU register access
- **Memory Management**: GPU memory allocation and mapping

### How Eclipse OS Uses It

1. **Device IDs**: Uses the same device ID mappings as open-gpu-kernel-modules
2. **Initialization**: Follows similar initialization sequences
3. **BAR Mapping**: Implements compatible memory-mapped I/O access
4. **Architecture Support**: Supports the same GPU architectures

## Usage

### Kernel Boot Messages

When an NVIDIA GPU is detected, the kernel will print:

```
[NVIDIA] Initializing NVIDIA GPU subsystem...
[NVIDIA] Compatible with open-gpu-kernel-modules
[NVIDIA] Repository: https://github.com/NVIDIA/open-gpu-kernel-modules
[NVIDIA] Found 1 NVIDIA GPU(s)
[NVIDIA] GPU 0: GeForce RTX 3080
[NVIDIA]   Device ID: 0x2206
[NVIDIA]   Architecture: Ampere
[NVIDIA]   Memory: 10240 MB
[NVIDIA]   CUDA Cores: 8704
[NVIDIA]   SM Count: 68
[NVIDIA]   BAR0: 0xE0000000
[NVIDIA]   ✓ Supported by open-gpu-kernel-modules
[NVIDIA]   Device enabled (I/O, Memory, Bus Master)
[NVIDIA] Initialization complete
```

### Display Service Messages

The display service will attempt to use the NVIDIA driver:

```
[DISPLAY-SERVICE] Scanning for graphics hardware...
[DISPLAY-SERVICE] NVIDIA GPU detected!
[DISPLAY-SERVICE] Initializing NVIDIA driver...
[DISPLAY-SERVICE]   - Interfacing with kernel nvidia module
[DISPLAY-SERVICE]   - Using NVIDIA open-gpu-kernel-modules
[DISPLAY-SERVICE]   - Detecting NVIDIA GPU model
[DISPLAY-SERVICE]   - Configuring GPU memory
[DISPLAY-SERVICE]   - Setting up display modes
[DISPLAY-SERVICE]   - Initializing CUDA cores (optional)
[DISPLAY-SERVICE]   - NVIDIA driver initialized successfully
```

## Development

### Adding Support for New GPUs

To add support for a new NVIDIA GPU:

1. Add the device ID to `eclipse_kernel/src/nvidia.rs` in the `identify_gpu()` function:

```rust
fn identify_gpu(device_id: u16) -> (NvidiaArchitecture, &'static str, u32, u32, u32) {
    match device_id {
        // Your new GPU
        0xXXXX => (NvidiaArchitecture::AdaLovelace, "GPU Name", memory_mb, cuda_cores, sm_count),
        // ... existing GPUs
    }
}
```

2. The GPU will be automatically detected on next boot

### Extending Driver Capabilities

The NVIDIA kernel module can be extended in several ways:

- **Register Access**: Add functions to read/write GPU registers
- **Memory Management**: Implement GPU memory allocation
- **Command Submission**: Add support for submitting work to the GPU
- **Interrupt Handling**: Implement GPU interrupt processing

## References

- [NVIDIA open-gpu-kernel-modules](https://github.com/NVIDIA/open-gpu-kernel-modules)
- [NVIDIA GPU Architecture Documentation](https://docs.nvidia.com/cuda/cuda-c-programming-guide/)
- [PCI Express Base Specification](https://pcisig.com/)
- [Eclipse OS Documentation](../docs/)

## License

The NVIDIA integration in Eclipse OS follows the same license as the rest of Eclipse OS. The open-gpu-kernel-modules from NVIDIA are under dual MIT/GPLv2 license.

## Contributing

Contributions to improve NVIDIA GPU support are welcome! Please see [CONTRIBUTING.md](../docs/CONTRIBUTING.md) for guidelines.

### Areas for Contribution

- Complete driver integration with open-gpu-kernel-modules
- CUDA runtime implementation
- Display output support
- Power management
- Video acceleration
- Additional GPU model support
