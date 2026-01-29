# eclipsefs-tools

Comprehensive utilities for managing EclipseFS filesystems.

## Tools Included

### mkfs.eclipsefs
Format a device or file with the EclipseFS filesystem.

**Usage:**
```bash
mkfs.eclipsefs [OPTIONS] <device>
```

**Options:**
- `-L, --label <LABEL>` - Set filesystem label (max 100 chars)
- `-b, --block-size <SIZE>` - Block size (512, 1024, 2048, 4096, 8192) [default: 4096]
- `-N, --inodes <COUNT>` - Number of inodes [default: 10000]
- `-f, --force` - Force format without confirmation
- `-v, --verbose` - Verbose output

**Examples:**
```bash
# Create a filesystem on a device
sudo mkfs.eclipsefs /dev/sda2

# Create with custom label and block size
sudo mkfs.eclipsefs -L "My Data" -b 8192 /dev/sda2

# Create test image
mkfs.eclipsefs -N 1000 test.img
```

### eclipsefs (CLI Tool)
Interact with EclipseFS filesystems without mounting.

**Usage:**
```bash
eclipsefs <command> [OPTIONS]
```

**Commands:**
- `info <device>` - Show filesystem information
- `ls <device> <path>` - List directory contents
- `cat <device> <path>` - Display file contents
- `tree <device>` - Show filesystem tree
- `check <device>` - Check filesystem integrity
- `stats <device>` - Show detailed statistics

**Examples:**
```bash
# Show filesystem info
eclipsefs info /dev/sda2

# List root directory
eclipsefs ls /dev/sda2 /

# Show filesystem tree
eclipsefs tree test.img

# Check integrity
eclipsefs check /dev/sda2
```

## Building

```bash
# Build all tools
cargo build --release

# Build specific tool
cd mkfs-eclipsefs && cargo build --release
```

## Installation

```bash
# Install to system
sudo cp target/release/mkfs.eclipsefs /usr/local/bin/
sudo cp target/release/eclipsefs /usr/local/bin/

# Or use cargo install
cargo install --path mkfs-eclipsefs
```

## Advanced Features

EclipseFS supports several advanced features:

### Journaling (Crash Recovery)
All filesystem operations are logged for crash recovery.

### Copy-on-Write (Versioning)
File modifications create new versions while preserving old ones.

### Intelligent Caching
LRU cache with prefetching for improved performance.

### Auto-Defragmentation
Automatic defragmentation in the background.

### Load Balancing
Intelligent load distribution across storage.

### Snapshots
Create point-in-time snapshots of the entire filesystem.

See `examples/advanced_features.rs` for usage examples.
