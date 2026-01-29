# VFS, Paging, Syscalls, and Procfs Implementation Guide

## Overview

This document provides a comprehensive implementation plan for the four critical kernel components requested:

1. Virtual Filesystem (VFS) implementation
2. Complete page table setup
3. Critical syscalls (fork, exec, wait, signal)
4. /proc filesystem (optional)

## Current State Analysis

### ✅ What Already Exists

**VFS Infrastructure (40% complete)**:
- `/eclipse_kernel/src/virtual_fs.rs` - In-memory RAM FS (688 lines)
- `/eclipse_kernel/src/filesystem/vfs.rs` - Abstract VFS trait (158 lines)
- Basic file operations: create, read, write, delete
- Permission system and metadata tracking
- Directory tree structure

**Paging Infrastructure (80% complete)**:
- `/eclipse_kernel/src/paging.rs` - Full 4-level page table implementation
- CR3 switching via `switch_to_pml4()`
- TLB invalidation
- Page mapping with flags (USER, WRITABLE, NX)
- Userland memory region support

**Syscalls Infrastructure (20% complete)**:
- `/eclipse_kernel/src/syscalls/` - 67 syscalls defined
- Basic handler framework
- Fork/exec/exit stubs in `process.rs`
- File operation stubs in `file.rs`

**Process Management**:
- `/eclipse_kernel/src/process/` - Process structures
- Process table (256 processes)
- File descriptor table
- Process states (Running, Sleeping, Zombie, etc.)

### ❌ What's Missing

**VFS**:
- Integration with real filesystems (EclipseFS, FAT32)
- Mount point hierarchy
- File locking
- Symbolic/hard links

**Paging**:
- Per-process page tables
- Page fault handler
- Copy-on-Write (COW)
- Demand paging

**Syscalls**:
- Functional fork() - memory copying
- Functional exec() - ELF loading integration
- wait/waitpid() - process synchronization
- Signal handling infrastructure

**Procfs**:
- Entire /proc filesystem
- /proc/[pid]/ directories
- Process status files

## Implementation Plan

### Phase 1: VFS Integration (HIGH PRIORITY)

#### 1.1 Create Global VFS Instance

```rust
// In eclipse_kernel/src/lib.rs or virtual_fs.rs
use spin::Mutex;
use lazy_static::lazy_static;

lazy_static! {
    pub static ref GLOBAL_VFS: Mutex<VirtualFileSystem> = {
        let mut vfs = VirtualFileSystem::new(10 * 1024 * 1024); // 10MB RAM FS
        vfs.create_directory("/proc").ok();
        vfs.create_directory("/dev").ok();
        vfs.create_directory("/sys").ok();
        vfs.create_directory("/sbin").ok();
        vfs.create_directory("/bin").ok();
        vfs.create_directory("/etc").ok();
        vfs.create_directory("/etc/eclipse").ok();
        vfs.create_directory("/etc/eclipse/systemd").ok();
        vfs.create_directory("/etc/eclipse/systemd/system").ok();
        vfs.create_directory("/var").ok();
        vfs.create_directory("/var/log").ok();
        Mutex::new(vfs)
    };
}

pub fn get_vfs() -> &'static Mutex<VirtualFileSystem> {
    &GLOBAL_VFS
}
```

#### 1.2 Integrate VFS with Syscalls

Update `syscalls/file.rs` to use real VFS:

```rust
pub fn sys_open(path: &str, flags: OpenFlags, mode: FileMode) -> SyscallResult {
    let vfs = get_vfs().lock();
    
    // Check if file exists
    if vfs.exists(path) {
        if flags.contains(OpenFlags::O_CREAT) && flags.contains(OpenFlags::O_EXCL) {
            return SyscallResult::Error(SyscallError::FileExists);
        }
    } else {
        if flags.contains(OpenFlags::O_CREAT) {
            vfs.create_file(path, &[])?;
        } else {
            return SyscallResult::Error(SyscallError::NotFound);
        }
    }
    
    // Allocate file descriptor
    let fd = allocate_fd(path, flags)?;
    SyscallResult::Success(fd as u64)
}

pub fn sys_read(fd: i32, buf: &mut [u8]) -> SyscallResult {
    let fd_entry = get_fd_entry(fd)?;
    let vfs = get_vfs().lock();
    
    let data = vfs.read_file(&fd_entry.path)?;
    let offset = fd_entry.offset as usize;
    let to_read = core::cmp::min(buf.len(), data.len() - offset);
    
    buf[..to_read].copy_from_slice(&data[offset..offset + to_read]);
    
    // Update offset
    update_fd_offset(fd, offset + to_read);
    
    SyscallResult::Success(to_read as u64)
}

pub fn sys_write(fd: i32, buf: &[u8]) -> SyscallResult {
    let fd_entry = get_fd_entry(fd)?;
    let mut vfs = get_vfs().lock();
    
    // For simplicity, append or overwrite
    if fd_entry.offset == 0 {
        vfs.write_file(&fd_entry.path, buf)?;
    } else {
        // Read existing, append, write back
        let mut data = vfs.read_file(&fd_entry.path)?;
        data.extend_from_slice(buf);
        vfs.write_file(&fd_entry.path, &data)?;
    }
    
    update_fd_offset(fd, fd_entry.offset + buf.len() as u64);
    
    SyscallResult::Success(buf.len() as u64)
}
```

### Phase 2: Per-Process Page Tables (HIGH PRIORITY)

#### 2.1 Add Page Table to Process Structure

```rust
// In process/process.rs
pub struct Process {
    pub pid: ProcessId,
    pub ppid: ProcessId,
    // ... existing fields ...
    pub page_table: Option<u64>, // Physical address of PML4
    pub memory_regions: Vec<MemoryRegion>,
}

pub struct MemoryRegion {
    pub start: VirtAddr,
    pub end: VirtAddr,
    pub permissions: PageFlags,
    pub physical_frames: Vec<PhysAddr>,
}
```

#### 2.2 Implement Page Table Copying for Fork

```rust
pub fn copy_page_tables(parent_pid: ProcessId) -> Result<u64, &'static str> {
    let parent = get_process(parent_pid)?;
    let parent_pml4 = parent.page_table.ok_or("No page table")?;
    
    // Allocate new PML4 for child
    let child_pml4 = allocate_page_table()?;
    
    // Copy all user-space mappings (0x400000 - 0x7FFFFFFFFFFF)
    unsafe {
        let parent_pml4_ptr = parent_pml4 as *const PageTable;
        let child_pml4_ptr = child_pml4 as *mut PageTable;
        
        for i in 0..256 { // Only user space (first half of address space)
            if (*parent_pml4_ptr).entries[i].flags().contains(PageTableFlags::PRESENT) {
                // Copy PDPT, PD, PT recursively
                copy_page_table_level(
                    &(*parent_pml4_ptr).entries[i],
                    &mut (*child_pml4_ptr).entries[i],
                    3 // level (4 = PML4, 3 = PDPT, 2 = PD, 1 = PT)
                )?;
            }
        }
    }
    
    Ok(child_pml4)
}

fn copy_page_table_level(
    parent_entry: &PageTableEntry,
    child_entry: &mut PageTableEntry,
    level: usize
) -> Result<(), &'static str> {
    if level == 0 {
        // Leaf level - copy page with COW
        let parent_frame = parent_entry.addr();
        let child_frame = allocate_physical_frame()?;
        
        // Copy page content
        unsafe {
            core::ptr::copy_nonoverlapping(
                parent_frame.as_u64() as *const u8,
                child_frame.as_u64() as *mut u8,
                4096
            );
        }
        
        child_entry.set_addr(child_frame, parent_entry.flags());
        
        // Mark both as read-only for COW
        parent_entry.set_flags(parent_entry.flags() - PageTableFlags::WRITABLE);
        
        Ok(())
    } else {
        // Intermediate level - recurse
        let child_table = allocate_page_table()?;
        
        unsafe {
            let parent_table = parent_entry.addr().as_u64() as *const PageTable;
            let child_table_ptr = child_table as *mut PageTable;
            
            for i in 0..512 {
                if (*parent_table).entries[i].flags().contains(PageTableFlags::PRESENT) {
                    copy_page_table_level(
                        &(*parent_table).entries[i],
                        &mut (*child_table_ptr).entries[i],
                        level - 1
                    )?;
                }
            }
        }
        
        child_entry.set_addr(PhysAddr::new(child_table), parent_entry.flags());
        Ok(())
    }
}
```

### Phase 3: Fork Syscall Implementation (CRITICAL)

```rust
// In syscalls/process.rs
pub fn sys_fork() -> SyscallResult {
    let current_pid = get_current_pid();
    let parent = get_process(current_pid)?;
    
    // 1. Allocate new PID
    let child_pid = allocate_pid()?;
    
    // 2. Copy page tables
    let child_pml4 = copy_page_tables(current_pid)?;
    
    // 3. Create child process
    let mut child = parent.clone();
    child.pid = child_pid;
    child.ppid = current_pid;
    child.page_table = Some(child_pml4);
    child.state = ProcessState::Ready;
    
    // 4. Copy file descriptors
    child.fd_table = parent.fd_table.clone();
    
    // 5. Copy register state (would need to capture from interrupt frame)
    child.registers = capture_current_registers();
    
    // 6. Add to process table
    add_process(child)?;
    
    // 7. Add to scheduler
    schedule_process(child_pid)?;
    
    // Return: 0 to child, child_pid to parent
    // This would be set in the interrupt return path
    SyscallResult::Success(child_pid as u64)
}
```

### Phase 4: Exec Syscall Implementation (CRITICAL)

```rust
pub fn sys_execve(path: &str, argv: &[&str], envp: &[&str]) -> SyscallResult {
    let current_pid = get_current_pid();
    
    // 1. Read ELF file from VFS
    let vfs = get_vfs().lock();
    let elf_data = vfs.read_file(path)?;
    
    // 2. Parse ELF
    let mut elf_loader = ElfLoader::new();
    let loaded = elf_loader.load_elf(&elf_data)?;
    
    // 3. Create new page tables
    let new_pml4 = create_userland_page_tables()?;
    
    // 4. Map ELF segments
    map_elf_segments(&loaded, new_pml4)?;
    
    // 5. Setup stack with argv/envp
    let stack_top = setup_initial_stack(argv, envp, new_pml4)?;
    
    // 6. Replace process page tables
    let process = get_process_mut(current_pid)?;
    let old_pml4 = process.page_table;
    process.page_table = Some(new_pml4);
    
    // 7. Free old page tables
    if let Some(old) = old_pml4 {
        free_page_tables(old)?;
    }
    
    // 8. Setup initial registers
    process.registers.rip = loaded.entry_point;
    process.registers.rsp = stack_top;
    process.registers.rdi = argv.len() as u64; // argc
    
    // 9. Switch to new page tables
    switch_to_page_table(new_pml4);
    
    // 10. Jump to entry point (via iretq)
    // This would be done in the syscall return path
    SyscallResult::NoReturn
}
```

### Phase 5: Wait Syscall Implementation (CRITICAL)

```rust
pub fn sys_wait(pid: i32, status: *mut i32) -> SyscallResult {
    let current_pid = get_current_pid();
    let process = get_process(current_pid)?;
    
    loop {
        // Check for zombie children
        for child_pid in find_children(current_pid) {
            let child = get_process(child_pid)?;
            
            if child.state == ProcessState::Zombie {
                if pid == -1 || pid == child_pid as i32 {
                    // Found a zombie child
                    let exit_code = child.exit_code;
                    
                    // Write status
                    if !status.is_null() {
                        unsafe { *status = exit_code << 8; }
                    }
                    
                    // Remove child from process table
                    remove_process(child_pid)?;
                    
                    return SyscallResult::Success(child_pid as u64);
                }
            }
        }
        
        // No zombie children yet - block
        process.state = ProcessState::Waiting;
        yield_cpu(); // Schedule another process
    }
}
```

### Phase 6: Signal Handling (CRITICAL)

```rust
// In process/process.rs
pub struct SignalHandler {
    pub handlers: [u64; 32], // Function pointers for each signal
    pub pending: u32,         // Bitmask of pending signals
    pub blocked: u32,         // Bitmask of blocked signals
}

impl Process {
    pub fn send_signal(&mut self, signum: i32) {
        if signum < 0 || signum >= 32 {
            return;
        }
        
        self.signal_handler.pending |= 1 << signum;
    }
    
    pub fn deliver_signals(&mut self) {
        let deliverable = self.signal_handler.pending & !self.signal_handler.blocked;
        
        for sig in 0..32 {
            if (deliverable & (1 << sig)) != 0 {
                self.signal_handler.pending &= !(1 << sig);
                
                let handler = self.signal_handler.handlers[sig];
                if handler == 0 {
                    // Default action
                    self.handle_default_signal(sig);
                } else if handler == 1 {
                    // Ignore
                    continue;
                } else {
                    // Custom handler - setup stack and jump
                    self.setup_signal_stack(sig, handler);
                }
            }
        }
    }
    
    fn handle_default_signal(&mut self, sig: i32) {
        match sig {
            9 => { // SIGKILL
                self.state = ProcessState::Dead;
                self.exit_code = 128 + sig;
            }
            15 => { // SIGTERM
                self.state = ProcessState::Zombie;
                self.exit_code = 128 + sig;
            }
            _ => {}
        }
    }
}
```

### Phase 7: Procfs Implementation (OPTIONAL)

```rust
// Create procfs.rs
pub struct ProcFs {
    vfs: &'static Mutex<VirtualFileSystem>,
}

impl ProcFs {
    pub fn init() -> Result<(), &'static str> {
        let vfs = get_vfs();
        let mut vfs_lock = vfs.lock();
        
        vfs_lock.create_directory("/proc")?;
        vfs_lock.create_directory("/proc/self")?;
        
        // Create static files
        Self::create_cpuinfo(&mut vfs_lock)?;
        Self::create_meminfo(&mut vfs_lock)?;
        
        Ok(())
    }
    
    pub fn update_process(&self, pid: ProcessId) -> Result<(), &'static str> {
        let process = get_process(pid)?;
        let pid_dir = alloc::format!("/proc/{}", pid);
        
        let mut vfs = self.vfs.lock();
        
        // Create PID directory
        vfs.create_directory(&pid_dir).ok();
        
        // Create status file
        let status = alloc::format!(
            "Name: {}\nPid: {}\nPPid: {}\nState: {:?}\n",
            process.name, process.pid, process.ppid, process.state
        );
        vfs.write_file(&alloc::format!("{}/status", pid_dir), status.as_bytes())?;
        
        // Create maps file
        let maps = self.generate_maps_file(process)?;
        vfs.write_file(&alloc::format!("{}/maps", pid_dir), maps.as_bytes())?;
        
        Ok(())
    }
    
    fn create_cpuinfo(vfs: &mut VirtualFileSystem) -> Result<(), &'static str> {
        let cpuinfo = "processor\t: 0\nvendor_id\t: GenuineIntel\nmodel name\t: Eclipse CPU\n";
        vfs.write_file("/proc/cpuinfo", cpuinfo.as_bytes())
    }
    
    fn create_meminfo(vfs: &mut VirtualFileSystem) -> Result<(), &'static str> {
        let total_mem = get_total_memory();
        let free_mem = get_free_memory();
        let meminfo = alloc::format!(
            "MemTotal: {} kB\nMemFree: {} kB\nMemAvailable: {} kB\n",
            total_mem / 1024, free_mem / 1024, free_mem / 1024
        );
        vfs.write_file("/proc/meminfo", meminfo.as_bytes())
    }
}
```

## Integration Steps

### Step 1: Enable VFS in Kernel Main

```rust
// In main_simple.rs, after system initialization
pub fn kernel_main(fb: &mut FramebufferDriver) -> ! {
    // ... existing initialization ...
    
    // Initialize VFS
    fb.write_text_kernel("Inicializando VFS...", Color::WHITE);
    match init_vfs() {
        Ok(_) => fb.write_text_kernel("✓ VFS inicializado", Color::GREEN),
        Err(e) => fb.write_text_kernel(&alloc::format!("⚠ VFS: {}", e), Color::YELLOW),
    }
    
    // Initialize Procfs
    fb.write_text_kernel("Inicializando /proc...", Color::WHITE);
    match ProcFs::init() {
        Ok(_) => fb.write_text_kernel("✓ /proc inicializado", Color::GREEN),
        Err(e) => fb.write_text_kernel(&alloc::format!("⚠ /proc: {}", e), Color::YELLOW),
    }
    
    // ... rest of initialization ...
}
```

### Step 2: Create Eclipse-Systemd Binary in VFS

```rust
pub fn prepare_systemd_in_vfs() -> Result<(), &'static str> {
    let vfs = get_vfs();
    let mut vfs_lock = vfs.lock();
    
    // For now, create a minimal stub executable
    // In a real system, this would be loaded from disk
    let systemd_stub = create_minimal_systemd_stub();
    vfs_lock.write_file("/sbin/init", &systemd_stub)?;
    vfs_lock.write_file("/sbin/eclipse-systemd", &systemd_stub)?;
    
    // Create service files
    create_default_service_files(&mut vfs_lock)?;
    
    Ok(())
}
```

### Step 3: Update Init System to Use Real VFS

```rust
// In init_system.rs
fn load_executable_elf(&self, path: &str) -> Result<LoadedProcess, &'static str> {
    // Read from VFS instead of fake data
    let vfs = get_vfs();
    let vfs_lock = vfs.lock();
    
    let elf_data = vfs_lock.read_file(path)?;
    
    let mut elf_loader = ElfLoader::new();
    elf_loader.load_elf(&elf_data)
}
```

## Testing Plan

### Test 1: VFS Operations
```rust
#[test]
fn test_vfs_file_operations() {
    let vfs = get_vfs();
    let mut vfs_lock = vfs.lock();
    
    // Create file
    vfs_lock.create_file("/test.txt", b"Hello").unwrap();
    
    // Read file
    let data = vfs_lock.read_file("/test.txt").unwrap();
    assert_eq!(data, b"Hello");
    
    // Write file
    vfs_lock.write_file("/test.txt", b"World").unwrap();
    let data = vfs_lock.read_file("/test.txt").unwrap();
    assert_eq!(data, b"World");
}
```

### Test 2: Fork
```rust
#[test]
fn test_fork() {
    let parent_pid = get_current_pid();
    let result = sys_fork();
    
    match result {
        SyscallResult::Success(0) => {
            // Child process
            assert_ne!(get_current_pid(), parent_pid);
        }
        SyscallResult::Success(child_pid) => {
            // Parent process
            assert!(child_pid > 0);
            assert_eq!(get_current_pid(), parent_pid);
        }
        _ => panic!("Fork failed"),
    }
}
```

## Minimal Implementation Checklist

For systemd to work minimally, implement in this order:

1. ✅ **VFS with file operations** - Needed to read service files
2. ✅ **Per-process page tables** - Needed for process isolation
3. ✅ **Fork syscall** - Needed to spawn service processes
4. ✅ **Exec syscall** - Needed to run service binaries
5. ✅ **Wait syscall** - Needed to monitor child processes
6. ✅ **Signal handling** - Needed for SIGTERM/SIGKILL
7. ⚠️ **Procfs /proc/[pid]/stat** - Needed for process monitoring
8. ⚠️ **Basic scheduler changes** - Switch between processes

## Estimated Effort

- **VFS Integration**: 200-300 lines of new code
- **Per-Process Paging**: 400-500 lines
- **Fork Implementation**: 300-400 lines
- **Exec Implementation**: 200-300 lines
- **Wait Implementation**: 100-150 lines
- **Signal Handling**: 200-300 lines
- **Procfs**: 300-400 lines

**Total**: ~1,800-2,350 lines of new/modified code

## References

- Linux VFS: https://www.kernel.org/doc/html/latest/filesystems/vfs.html
- x86-64 Paging: https://wiki.osdev.org/Page_Tables
- ELF Format: https://wiki.osdev.org/ELF
- Fork/Exec: https://man7.org/linux/man-pages/man2/fork.2.html

## Next Steps

1. Start with VFS integration (highest priority, easiest)
2. Implement per-process page tables
3. Implement fork with page table copying
4. Implement exec with ELF loading
5. Add wait and signal support
6. Add procfs for monitoring
7. Test with eclipse-systemd

This implementation will enable eclipse-systemd to actually function as PID 1 and manage services.
