# COSMIC Desktop Environment - Userland Module

This is the COSMIC desktop environment for Eclipse OS, now running in userland (migrated from eclipse_kernel).

## Overview

COSMIC (Computer Operating System Main Interface Components) is a modern, Rust-based desktop environment that provides:

- **Wayland compositor** with advanced window management
- **AI-driven features** including smart notifications, adaptive behavior, and performance optimization
- **Advanced visual effects** with GPU acceleration support
- **Modular architecture** with plugin and applet systems
- **Theme system** with dynamic and AI-powered theming

## Architecture

This module is now a **userland component**, decoupled from kernel internals:

- ✅ **No direct kernel dependencies** - Uses syscalls for system interaction
- ✅ **Modern Rust ecosystem** - Leverages standard crates from the Rust community
- ✅ **COSMIC-inspired** - Integrates ideas and crates from System76's COSMIC DE
- ✅ **Modular design** - Can be built with different feature sets

## Key Dependencies

### COSMIC Ecosystem (from System76/Pop!_OS)

- **cosmic-text** (v0.16) - Pure Rust multi-line text handling with advanced features:
  - Text shaping with HarfBuzz/rustybuzz
  - Font discovery and fallback
  - Bidirectional text support
  - Color emoji support
  - Multi-line layout and editing

### Wayland Compositor Framework (Optional)

When built with the `compositor` feature:

- **smithay** - Modular building blocks for Wayland compositors
- **wayland-server** - Wayland server protocol implementation
- **wayland-protocols** - Standard Wayland protocol extensions
- **calloop** - Event loop for async compositor operations

### Wayland Client Support (Optional)

When built with the `client` feature:

- **wayland-client** - Connect to Wayland display servers

## Features

### Default Features

The default build includes core COSMIC functionality with minimal dependencies.

### Optional Features

- **`compositor`** - Enable full Wayland compositor capabilities using smithay
- **`client`** - Enable Wayland client functionality
- **`serialization`** - Enable serde support for configuration and state persistence

## Modules

### Core Modules

- `mod.rs` - Main COSMIC manager and orchestration
- `compositor.rs`, `advanced_compositor.rs` - Window compositing
- `window_manager.rs`, `window_operations.rs`, `window_system.rs` - Window management
- `theme.rs`, `dynamic_themes.rs`, `ai_themes.rs` - Theming system
- `integration.rs`, `wayland_integration.rs` - Desktop integration

### UI Components

- `taskbar.rs`, `start_menu.rs` - Desktop UI elements
- `notification_system_advanced.rs`, `smart_notifications.rs` - Notification system
- `smart_widgets.rs`, `modern_widgets.rs`, `floating_widgets.rs` - Widget systems
- `applet_system.rs`, `plugin_system.rs` - Extensibility

### AI Features

- `ai_engine.rs`, `ai_renderer.rs`, `ai_features.rs` - AI rendering and features
- `ai_autodiagnostic.rs`, `ai_error_detection.rs` - Diagnostics and error detection
- `ai_performance.rs`, `intelligent_performance.rs` - Performance optimization
- `ai_learning_system.rs`, `ai_learning_persistence.rs` - Adaptive learning
- `user_behavior_predictor.rs`, `user_preference_tracker.rs` - User behavior analysis
- `intelligent_assistant.rs`, `intelligent_recommendations.rs` - Smart assistance

### Visual Effects

- `visual_effects.rs`, `advanced_visual_effects.rs`, `beautiful_effects.rs` - Effects system
- `animations.rs`, `widget_animations.rs` - Animation system
- `advanced_particles.rs` - Particle effects
- `visual_shaders.rs` - Shader system
- `cuda_acceleration.rs` - GPU acceleration (CUDA support)
- `opengl_renderer.rs`, `optimized_renderer.rs` - Rendering engines

### Advanced Features

- `global_search.rs` - System-wide search
- `input_system.rs`, `touch_gestures.rs` - Input handling
- `desktop_portal.rs` - XDG desktop portal integration
- `icon_system.rs` - Icon management
- `audio_visual.rs` - Audio visualization

### COSMIC Inspired

The `cosmic_inspired/` subdirectory contains reference implementations inspired by System76's COSMIC Epoch desktop environment.

## Building

### Basic build (no_std compatible)
```bash
cargo build --release
```

### With compositor support
```bash
cargo build --release --features compositor
```

### With all features
```bash
cargo build --release --features compositor,client,serialization
```

## Migration from Kernel

This module was migrated from `eclipse_kernel/src/cosmic` to userland. Key changes:

1. **Removed kernel dependencies**: All `crate::drivers::`, `crate::ai_inference::`, and `crate::desktop_ai::` imports have been commented out
2. **Added modern crates**: Integrated cosmic-text, smithay, and other ecosystem crates
3. **Userland-compatible**: Can now run as a userland process using syscalls

## References

- [COSMIC Desktop (System76)](https://system76.com/cosmic)
- [libcosmic Documentation](https://pop-os.github.io/libcosmic-book/)
- [cosmic-text on GitHub](https://github.com/pop-os/cosmic-text)
- [Smithay Compositor Framework](https://github.com/Smithay/smithay)
- [COSMIC Epoch Repository](https://github.com/pop-os/cosmic-epoch)

## License

Part of Eclipse OS project.
