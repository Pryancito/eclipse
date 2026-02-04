# NVIDIA GPU Advanced Features - Usage Examples

This document provides practical examples of using the advanced NVIDIA GPU features in Eclipse OS.

## CUDA Runtime

### Basic CUDA Context and Memory Operations

```rust
use eclipse_kernel::nvidia::cuda::{CudaContext, KernelConfig, CudaStream};

// Create CUDA context for first GPU
let context = CudaContext::new(0)?;

// Allocate device memory (1 MB)
let device_buffer = context.allocate_device_memory(1024 * 1024)?;

// Transfer data from host to device
let host_data: Vec<u8> = vec![0; 1024];
context.copy_host_to_device(
    host_data.as_ptr() as usize,
    device_buffer,
    1024
)?;

// Launch a CUDA kernel
let kernel_config = KernelConfig {
    blocks: (256, 1, 1),       // 256 blocks
    threads: (256, 1, 1),      // 256 threads per block
    shared_memory: 4096,       // 4KB shared memory
};

context.launch_kernel(kernel_ptr, kernel_config)?;

// Copy results back
let mut result_data = vec![0u8; 1024];
context.copy_device_to_host(
    device_buffer,
    result_data.as_mut_ptr() as usize,
    1024
)?;
```

### Asynchronous CUDA Operations with Streams

```rust
// Create high-priority stream for time-sensitive operations
let stream = CudaStream::new(-1)?;  // Negative = higher priority

// All subsequent operations on this stream execute asynchronously
// This allows overlapping computation and data transfer
```

## Ray Tracing (RT Cores)

### Building Acceleration Structures

```rust
use eclipse_kernel::nvidia::raytracing::{
    AccelerationStructure, RtPipeline, RtCoreCapabilities
};
use eclipse_kernel::nvidia::get_nvidia_gpus;

// Get RT core capabilities
let gpus = get_nvidia_gpus();
let rt_caps = RtCoreCapabilities::detect(&gpus[0]);

println!("RT Cores: {}", rt_caps.rt_cores);
println!("Max Recursion: {}", rt_caps.max_recursion_depth);
println!("Inline RT: {}", rt_caps.supports_inline_rt);

// Build acceleration structure for scene geometry
let vertices: Vec<f32> = vec![
    // Triangle vertices in 3D space
    0.0, 0.0, 0.0,
    1.0, 0.0, 0.0,
    0.5, 1.0, 0.0,
];

let indices: Vec<u32> = vec![0, 1, 2];

let accel_struct = AccelerationStructure::build(&vertices, &indices)?;
```

### Creating Ray Tracing Pipeline

```rust
// Create RT pipeline with maximum recursion depth
let rt_pipeline = RtPipeline::new(8)?;  // 8 levels of recursion

// Pipeline is now ready for ray tracing operations
// In a full implementation, you would:
// 1. Bind shaders (ray generation, closest hit, miss, etc.)
// 2. Create shader binding table
// 3. Trace rays through the acceleration structure
```

## Display Output

### Detecting and Configuring Displays

```rust
use eclipse_kernel::nvidia::display::{DisplayConnector, DisplayMode, ConnectorType};

// Detect all connected displays
let connectors = DisplayConnector::detect_all();

for connector in &connectors {
    println!("Connector: {:?}", connector.connector_type);
    println!("Connected: {}", connector.connected);
    println!("Max Resolution: {}x{}", connector.max_width, connector.max_height);
    
    if connector.edid_available {
        // Read EDID to get detailed display capabilities
        let edid = connector.read_edid()?;
        // Parse EDID to extract supported modes, manufacturer, etc.
    }
}

// Set display mode for primary output
if let Some(primary) = connectors.first() {
    let mode = DisplayMode {
        width: 1920,
        height: 1080,
        refresh_rate: 60,
        pixel_clock: 148500,  // kHz
    };
    
    primary.set_mode(mode)?;
}
```

### Multi-Display Setup

```rust
// Configure dual monitor setup
let displays = DisplayConnector::detect_all();

if displays.len() >= 2 {
    // Primary display: 4K@60Hz
    displays[0].set_mode(DisplayMode {
        width: 3840,
        height: 2160,
        refresh_rate: 60,
        pixel_clock: 533250,
    })?;
    
    // Secondary display: 1080p@144Hz
    displays[1].set_mode(DisplayMode {
        width: 1920,
        height: 1080,
        refresh_rate: 144,
        pixel_clock: 325080,
    })?;
}
```

## Power Management

### Dynamic Power State Control

```rust
use eclipse_kernel::nvidia::power::{PowerManager, PowerState, ClockDomain};

let mut power_mgr = PowerManager::new();

// Read current temperature
let temp = power_mgr.read_temperature()?;
println!("GPU Temperature: {}°C", temp);

// Adjust power state based on workload
if is_idle() {
    // Switch to power saving mode
    power_mgr.set_power_state(PowerState::P2)?;
} else if is_gaming() {
    // Maximum performance
    power_mgr.set_power_state(PowerState::P0)?;
} else {
    // Balanced mode
    power_mgr.set_power_state(PowerState::P1)?;
}
```

### Clock Frequency Management

```rust
// Boost graphics clock for gaming
power_mgr.set_clock_frequency(ClockDomain::Graphics, 1900)?;  // 1.9 GHz

// Standard memory clock
power_mgr.set_clock_frequency(ClockDomain::Memory, 7000)?;    // 7 GHz effective

// Set power limit (in milliwatts)
power_mgr.set_power_limit(250_000)?;  // 250W TDP
```

### Temperature-Based Throttling

```rust
loop {
    let temp = power_mgr.read_temperature()?;
    
    if temp > 80 {
        // Temperature too high, reduce clocks
        power_mgr.set_clock_frequency(ClockDomain::Graphics, 1500)?;
        power_mgr.set_power_state(PowerState::P2)?;
        println!("Throttling due to high temperature");
    } else if temp < 70 {
        // Temperature acceptable, full performance
        power_mgr.set_clock_frequency(ClockDomain::Graphics, 1900)?;
        power_mgr.set_power_state(PowerState::P0)?;
    }
    
    sleep(1000);  // Check every second
}
```

## Video Encoding (NVENC)

### H.264 Video Encoding

```rust
use eclipse_kernel::nvidia::video::{NvencEncoder, VideoCodec, EncoderCapabilities};
use eclipse_kernel::nvidia::get_nvidia_gpus;

// Check encoder capabilities
let gpus = get_nvidia_gpus();
let enc_caps = EncoderCapabilities::detect(&gpus[0]);

println!("Supported codecs: {:?}", enc_caps.supported_codecs);
println!("Max resolution: {}x{}", enc_caps.max_width, enc_caps.max_height);
println!("Max framerate: {} fps", enc_caps.max_framerate);

// Create H.264 encoder for 1080p
let encoder = NvencEncoder::new(VideoCodec::H264, 1920, 1080)?;

// Encode frames
let input_frame_ptr = get_video_frame();
let output_buffer_ptr = allocate_bitstream_buffer();

let encoded_size = encoder.encode_frame(input_frame_ptr, output_buffer_ptr)?;
println!("Encoded {} bytes", encoded_size);
```

### AV1 Encoding (Ada Lovelace+)

```rust
// AV1 encoding requires Ada Lovelace or Hopper architecture
if enc_caps.supported_codecs.contains(&VideoCodec::AV1) {
    let av1_encoder = NvencEncoder::new(VideoCodec::AV1, 3840, 2160)?;
    
    // Encode 4K frame with AV1
    let size = av1_encoder.encode_frame(input_ptr, output_ptr)?;
    println!("AV1 encoded {} bytes (better compression than H.265)", size);
}
```

## Video Decoding (NVDEC)

### H.265 Video Decoding

```rust
use eclipse_kernel::nvidia::video::{NvdecDecoder, VideoCodec, DecoderCapabilities};

// Check decoder capabilities
let dec_caps = DecoderCapabilities::detect(&gpus[0]);

println!("Supported codecs: {:?}", dec_caps.supported_codecs);
println!("Film grain support: {}", dec_caps.supports_film_grain);

// Create H.265 decoder
let decoder = NvdecDecoder::new(VideoCodec::H265, 1920, 1080)?;

// Decode compressed frame
let bitstream_ptr = get_compressed_data();
let output_frame_ptr = allocate_frame_buffer();

decoder.decode_frame(bitstream_ptr, output_frame_ptr)?;
```

### Multi-Stream Decoding

```rust
// NVDEC can decode multiple streams simultaneously
let decoder1 = NvdecDecoder::new(VideoCodec::H264, 1920, 1080)?;
let decoder2 = NvdecDecoder::new(VideoCodec::H265, 3840, 2160)?;

// Decode both streams in parallel
decoder1.decode_frame(stream1_data, output1)?;
decoder2.decode_frame(stream2_data, output2)?;
```

## Combined Example: Real-Time Ray Traced Video Encoding

```rust
// This example combines multiple features:
// 1. Ray tracing for rendering
// 2. Power management for efficiency
// 3. NVENC for capturing the result

use eclipse_kernel::nvidia::{
    cuda::CudaContext,
    raytracing::{AccelerationStructure, RtPipeline},
    power::PowerManager,
    video::NvencEncoder,
};

// Initialize components
let cuda_ctx = CudaContext::new(0)?;
let rt_pipeline = RtPipeline::new(4)?;
let mut power_mgr = PowerManager::new();
let encoder = NvencEncoder::new(VideoCodec::H265, 1920, 1080)?;

// Set to maximum performance for real-time rendering
power_mgr.set_power_state(PowerState::P0)?;

// Main render loop
loop {
    // 1. Render frame using ray tracing
    let render_buffer = render_frame_with_raytracing(&rt_pipeline)?;
    
    // 2. Encode the rendered frame
    let encoded_size = encoder.encode_frame(render_buffer, output_buffer)?;
    
    // 3. Monitor temperature
    let temp = power_mgr.read_temperature()?;
    if temp > 75 {
        // Throttle if getting too hot
        power_mgr.set_power_state(PowerState::P1)?;
    }
    
    // Save or stream the encoded data
    save_or_stream_video(output_buffer, encoded_size);
}
```

## Error Handling

All NVIDIA GPU operations return `Result` types for proper error handling:

```rust
match context.launch_kernel(kernel_ptr, config) {
    Ok(_) => println!("Kernel launched successfully"),
    Err(e) => {
        eprintln!("Kernel launch failed: {}", e);
        // Handle error appropriately
    }
}
```

## Best Practices

1. **Always check capabilities** before using features (e.g., AV1 encode on Ada Lovelace only)
2. **Monitor temperature** when running intensive workloads
3. **Use streams** for asynchronous operations to maximize throughput
4. **Set appropriate power states** based on workload requirements
5. **Clean up resources** properly (contexts, streams, encoders, decoders)
6. **Handle errors** - GPU operations can fail for various reasons

## See Also

- [NVIDIA_SUPPORT.md](NVIDIA_SUPPORT.md) - Complete NVIDIA GPU support documentation
- [README.md](../README.md) - Eclipse OS overview
- [NVIDIA open-gpu-kernel-modules](https://github.com/NVIDIA/open-gpu-kernel-modules) - Official NVIDIA driver source
