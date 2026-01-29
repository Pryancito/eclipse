//! /proc Filesystem Implementation
//!
//! This module implements a basic /proc filesystem for process information.
//! It provides /proc/[pid]/ directories with status, maps, and other process info.

use crate::vfs_global::get_vfs;
use crate::virtual_fs::FsResult;
use alloc::string::String;
use alloc::format;

/// Initialize /proc filesystem
pub fn init_procfs() -> FsResult<()> {
    let vfs = get_vfs();
    let mut vfs_lock = vfs.lock();
    
    // Create /proc directory structure
    vfs_lock.create_directory("/proc")?;
    vfs_lock.create_directory("/proc/self")?;
    
    // Create static files
    create_cpuinfo(&mut vfs_lock)?;
    create_meminfo(&mut vfs_lock)?;
    create_version(&mut vfs_lock)?;
    create_uptime(&mut vfs_lock)?;
    
    Ok(())
}

/// Create /proc/cpuinfo
fn create_cpuinfo(vfs: &mut crate::virtual_fs::VirtualFileSystem) -> FsResult<()> {
    let cpuinfo = "\
processor\t: 0
vendor_id\t: GenuineIntel
cpu family\t: 6
model\t\t: 142
model name\t: Eclipse OS Virtual CPU
stepping\t: 12
microcode\t: 0xb4
cpu MHz\t\t: 2400.000
cache size\t: 6144 KB
physical id\t: 0
siblings\t: 1
core id\t\t: 0
cpu cores\t: 1
apicid\t\t: 0
initial apicid\t: 0
fpu\t\t: yes
fpu_exception\t: yes
cpuid level\t: 22
wp\t\t: yes
flags\t\t: fpu vme de pse tsc msr pae mce cx8 apic sep mtrr pge mca cmov pat pse36 clflush mmx fxsr sse sse2 syscall nx pdpe1gb rdtscp lm
bogomips\t: 4800.00
clflush size\t: 64
cache_alignment\t: 64
address sizes\t: 39 bits physical, 48 bits virtual
";
    
    vfs.create_file("/proc/cpuinfo", cpuinfo.as_bytes())
}

/// Create /proc/meminfo
fn create_meminfo(vfs: &mut crate::virtual_fs::VirtualFileSystem) -> FsResult<()> {
    // Get memory info from kernel
    let (total, free) = get_memory_info();
    
    let meminfo = format!("\
MemTotal:        {} kB
MemFree:         {} kB
MemAvailable:    {} kB
Buffers:         {} kB
Cached:          {} kB
SwapCached:      0 kB
Active:          {} kB
Inactive:        {} kB
SwapTotal:       0 kB
SwapFree:        0 kB
Dirty:           0 kB
Writeback:       0 kB
",
        total / 1024,
        free / 1024,
        free / 1024,
        1024,  // Buffers
        2048,  // Cached
        (total - free) / 2 / 1024,  // Active
        (total - free) / 2 / 1024,  // Inactive
    );
    
    vfs.create_file("/proc/meminfo", meminfo.as_bytes())
}

/// Create /proc/version
fn create_version(vfs: &mut crate::virtual_fs::VirtualFileSystem) -> FsResult<()> {
    let version = "Eclipse OS version 0.6.0 (eclipse-kernel) (gcc version 11.4.0) #1 SMP PREEMPT Wed Jan 29 00:00:00 UTC 2026\n";
    vfs.create_file("/proc/version", version.as_bytes())
}

/// Create /proc/uptime
fn create_uptime(vfs: &mut crate::virtual_fs::VirtualFileSystem) -> FsResult<()> {
    let uptime = "60.00 60.00\n"; // Simulated: 60 seconds up, 60 seconds idle
    vfs.create_file("/proc/uptime", uptime.as_bytes())
}

/// Update /proc/[pid]/ directory for a process
pub fn update_process_info(pid: u32) -> FsResult<()> {
    let vfs = get_vfs();
    let mut vfs_lock = vfs.lock();
    
    let pid_dir = format!("/proc/{}", pid);
    
    // Create PID directory
    vfs_lock.create_directory(&pid_dir).ok();
    
    // Create status file
    let status = create_process_status(pid);
    vfs_lock.create_file(&format!("{}/status", pid_dir), status.as_bytes())?;
    
    // Create cmdline file
    let cmdline = create_process_cmdline(pid);
    vfs_lock.create_file(&format!("{}/cmdline", pid_dir), cmdline.as_bytes())?;
    
    // Create stat file
    let stat = create_process_stat(pid);
    vfs_lock.create_file(&format!("{}/stat", pid_dir), stat.as_bytes())?;
    
    // Create maps file
    let maps = create_process_maps(pid);
    vfs_lock.create_file(&format!("{}/maps", pid_dir), maps.as_bytes())?;
    
    Ok(())
}

/// Create /proc/[pid]/status content
fn create_process_status(pid: u32) -> String {
    // Get process info (in a real system, from process table)
    let (name, state, ppid, uid, gid) = get_process_info(pid);
    
    format!("\
Name:\t{}
Umask:\t0022
State:\t{} (running)
Tgid:\t{}
Ngid:\t0
Pid:\t{}
PPid:\t{}
TracerPid:\t0
Uid:\t{}\t{}\t{}\t{}
Gid:\t{}\t{}\t{}\t{}
FDSize:\t256
Groups:\t{}
VmPeak:\t   10240 kB
VmSize:\t   10240 kB
VmLck:\t       0 kB
VmPin:\t       0 kB
VmHWM:\t    1024 kB
VmRSS:\t    1024 kB
VmData:\t    512 kB
VmStk:\t    512 kB
VmExe:\t    256 kB
VmLib:\t      0 kB
Threads:\t1
SigQ:\t0/7919
SigPnd:\t0000000000000000
ShdPnd:\t0000000000000000
SigBlk:\t0000000000000000
SigIgn:\t0000000000000000
SigCgt:\t0000000000000000
",
        name,
        state,
        pid,
        pid,
        ppid,
        uid, uid, uid, uid,
        gid, gid, gid, gid,
        gid,
    )
}

/// Create /proc/[pid]/cmdline content
fn create_process_cmdline(pid: u32) -> String {
    let (name, ..) = get_process_info(pid);
    format!("{}\0", name)
}

/// Create /proc/[pid]/stat content
fn create_process_stat(pid: u32) -> String {
    let (name, state, ppid, ..) = get_process_info(pid);
    
    // Format: pid (comm) state ppid pgrp session tty_nr tpgid flags minflt cminflt majflt cmajflt utime stime cutime cstime priority nice num_threads itrealvalue starttime vsize rss rsslim
    format!("{} ({}) {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {}\n",
        pid,               // 1: pid
        name,              // 2: comm
        state,             // 3: state
        ppid,              // 4: ppid
        pid,               // 5: pgrp
        pid,               // 6: session
        0,                 // 7: tty_nr
        -1,                // 8: tpgid
        0,                 // 9: flags
        0, 0, 0, 0,        // 10-13: minflt, cminflt, majflt, cmajflt
        100, 50,           // 14-15: utime, stime (in clock ticks)
        0, 0,              // 16-17: cutime, cstime
        20,                // 18: priority
        0,                 // 19: nice
        1,                 // 20: num_threads
        0,                 // 21: itrealvalue
        1000,              // 22: starttime
        10485760,          // 23: vsize (10MB)
        256,               // 24: rss (256 pages = 1MB)
        18446744073709551615u64, // 25: rsslim
        0x400000,          // 26: startcode
        0x500000,          // 27: endcode
        0x7FFF0000,        // 28: startstack
        0x7FFFFFFF,        // 29: kstkesp
        0x7FFFFFFF,        // 30: kstkeip
        0, 0, 0, 0,        // 31-34: signal, blocked, sigignore, sigcatch
        0,                 // 35: wchan
        0, 0,              // 36-37: nswap, cnswap
        0,                 // 38: exit_signal
        0,                 // 39: processor
        0, 0,              // 40-41: rt_priority, policy
        0,                 // 42: delayacct_blkio_ticks
        0, 0,              // 43-44: guest_time, cguest_time
        0, 0, 0, 0,        // 45-48: start_data, end_data, start_brk, arg_start
        0, 0, 0,           // 49-51: arg_end, env_start, env_end
        0,                 // 52: exit_code
    )
}

/// Create /proc/[pid]/maps content
fn create_process_maps(pid: u32) -> String {
    // Simulated memory map
    format!("\
00400000-00401000 r-xp 00000000 00:00 0                                  [text]
00600000-00601000 rw-p 00000000 00:00 0                                  [data]
00601000-00701000 rw-p 00000000 00:00 0                                  [heap]
7fff00000000-7fff00001000 rw-p 00000000 00:00 0                          [stack]
")
}

/// Get process information (stub - would query process table)
fn get_process_info(pid: u32) -> (String, &'static str, u32, u32, u32) {
    // In a real system, this would query the process table
    let name = if pid == 1 {
        "systemd".to_string()
    } else {
        format!("process-{}", pid)
    };
    
    let state = "R"; // Running
    let ppid = if pid == 1 { 0 } else { 1 };
    let uid = 0; // root
    let gid = 0; // root
    
    (name, state, ppid, uid, gid)
}

/// Get memory information (stub - would query kernel memory manager)
fn get_memory_info() -> (u64, u64) {
    // Total: 64MB, Free: 32MB (simulated)
    (64 * 1024 * 1024, 32 * 1024 * 1024)
}

/// Update uptime in /proc
pub fn update_uptime(uptime_secs: u64) -> FsResult<()> {
    let vfs = get_vfs();
    let mut vfs_lock = vfs.lock();
    
    let uptime = format!("{}.00 {}.00\n", uptime_secs, uptime_secs);
    vfs_lock.write_file("/proc/uptime", uptime.as_bytes())
}

/// Update meminfo in /proc
pub fn update_meminfo() -> FsResult<()> {
    let vfs = get_vfs();
    let mut vfs_lock = vfs.lock();
    
    let (total, free) = get_memory_info();
    
    let meminfo = format!("\
MemTotal:        {} kB
MemFree:         {} kB
MemAvailable:    {} kB
",
        total / 1024,
        free / 1024,
        free / 1024,
    );
    
    vfs_lock.write_file("/proc/meminfo", meminfo.as_bytes())
}
