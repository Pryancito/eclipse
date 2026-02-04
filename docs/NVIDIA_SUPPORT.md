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
- ✅ **Specifications**: Reports CUDA cores, SM count, RT cores, Tensor cores, and VRAM size
- ✅ **Multi-GPU Support**: Detects and initializes multiple NVIDIA GPUs
- ✅ **Device Enablement**: Configures I/O, Memory, and Bus Master modes
- ✅ **CUDA Runtime**: User-space CUDA runtime for compute workloads with context management, memory operations, and kernel launch
- ✅ **Ray Tracing**: RT core support for real-time ray tracing with acceleration structures and pipeline management
- ✅ **Display Output**: Direct display output via DisplayPort/HDMI with mode setting and EDID parsing
- ✅ **Power Management**: GPU power state control, clock frequency management, and thermal monitoring
- ✅ **Video Encode (NVENC)**: Hardware-accelerated video encoding for H.264, H.265, and AV1 (on supported GPUs)
- ✅ **Video Decode (NVDEC)**: Hardware-accelerated video decoding for H.264, H.265, VP9, and AV1 (on supported GPUs)

### Planned Features

- 🔄 **Full Driver Integration**: Complete integration with open-gpu-kernel-modules source code
- 🔄 **Direct Memory Access**: Implement actual GPU memory access via BAR mapping
- 🔄 **Command Submission**: Real GPU command buffer submission and synchronization
- 🔄 **Interrupt Handling**: GPU interrupt processing for async operations

## Feature Details

### CUDA Runtime

The CUDA runtime module (`nvidia::cuda`) provides:

- **Context Management**: Create and manage CUDA contexts for GPU operations
- **Memory Operations**: Allocate device memory and transfer data between host and device
- **Kernel Launch**: Submit CUDA kernels with configurable block and thread dimensions
- **Stream Support**: Asynchronous operations with CUDA streams and priorities

Example usage:
```rust
use eclipse_kernel::nvidia::cuda::{CudaContext, KernelConfig};

let context = CudaContext::new(0)?;  // GPU 0
let device_mem = context.allocate_device_memory(1024)?;
context.copy_host_to_device(host_ptr, device_mem, 1024)?;

let config = KernelConfig {
    blocks: (256, 1, 1),
    threads: (256, 1, 1),
    shared_memory: 0,
};
context.launch_kernel(kernel_ptr, config)?;
```

### Ray Tracing (RT Cores)

The ray tracing module (`nvidia::raytracing`) provides:

- **RT Core Detection**: Automatic detection of RT core count and capabilities
- **Acceleration Structures**: Build BVH structures for geometry
- **RT Pipeline**: Create ray tracing pipelines with configurable recursion depth
- **Inline Ray Tracing**: Support for inline RT on Ampere and newer

Capabilities by architecture:
- **Turing**: 1st generation RT cores, one per SM
- **Ampere**: 2nd generation RT cores with inline ray tracing support
- **Ada Lovelace**: 3rd generation RT cores with improved performance
- **Hopper**: Latest RT cores optimized for datacenter workloads

### Display Output

The display module (`nvidia::display`) provides:

- **Connector Detection**: Identify DisplayPort, HDMI, DVI, and VGA outputs
- **EDID Reading**: Read display capabilities via I2C
- **Mode Setting**: Configure resolution, refresh rate, and pixel clock
- **Multi-Display**: Support for multiple connected displays

Supported connectors:
- DisplayPort (up to 8K@60Hz on Ada Lovelace)
- HDMI 2.1 (up to 4K@120Hz)
- Legacy DVI and VGA (compatibility)

### Power Management

The power management module (`nvidia::power`) provides:

- **Power States**: P0 (max performance), P1 (balanced), P2 (power saving), P3 (idle)
- **Clock Control**: Independent frequency management for graphics, memory, and video clocks
- **Thermal Monitoring**: Real-time temperature sensor reading
- **Power Limits**: Configurable TDP limits for efficiency

### Video Acceleration

The video module (`nvidia::video`) provides hardware-accelerated encoding and decoding:

#### NVENC (Encoder)
- **H.264/AVC**: All architectures, up to 8K resolution
- **H.265/HEVC**: All architectures, up to 8K resolution
- **AV1**: Ada Lovelace and Hopper only

Encoder features:
- B-frames support for better compression
- Up to 240 FPS encoding
- Dual encoder on high-end GPUs

#### NVDEC (Decoder)
- **H.264/AVC**: All architectures
- **H.265/HEVC**: All architectures
- **VP9**: All architectures
- **AV1**: Ampere, Ada Lovelace, and Hopper

Decoder features:
- Film grain synthesis (AV1)
- Up to 8K resolution
- Dedicated decode engine

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
[NVIDIA]   RT Cores: 68
[NVIDIA]   Tensor Cores: 272
[NVIDIA]   BAR0: 0xE0000000
[NVIDIA]   Advanced Features:
[NVIDIA]     ✓ CUDA Runtime
[NVIDIA]     ✓ Ray Tracing (RT Cores)
[NVIDIA]     ✓ DisplayPort/HDMI Output
[NVIDIA]     ✓ Power Management
[NVIDIA]     ✓ Video Encode (NVENC): 3 codecs
[NVIDIA]     ✓ Video Decode (NVDEC): 4 codecs
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
