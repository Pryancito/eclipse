/// Eclipse OS minimal POSIX shell (`/bin/sh`).
///
/// This shell is started by the terminal emulator as the interactive back-end.
/// It reads lines from stdin (the PTY slave file descriptor), parses them into
/// a command name and arguments, executes them via the Eclipse OS spawn syscall,
/// waits for the child to finish, and then prints the next prompt.
///
/// Only simple one-word commands (no pipes, redirects, or builtins beyond `exit`)
/// are supported at this stage.  The shell is intentionally minimal so that it
/// compiles in `no_std` mode without an allocator beyond what eclipse_std provides.
#[cfg_attr(target_vendor = "eclipse", no_std)]

#[cfg(target_vendor = "eclipse")]
extern crate alloc;
#[cfg(target_vendor = "eclipse")]
extern crate eclipse_std as std;

#[cfg(target_vendor = "eclipse")]
fn main() {
    use std::prelude::v1::*;

    let prompt = b"\x1b[32meclipse\x1b[0m:\x1b[34m~\x1b[0m$ ";

    loop {
        // Print prompt
        let _ = eclipse_syscall::call::write(1, prompt);

        // Read one line from stdin
        let mut line_buf = [0u8; 256];
        let n = match eclipse_syscall::call::read(0, &mut line_buf) {
            Ok(n) => n,
            Err(_) => break,
        };
        if n == 0 {
            // EOF — terminal closed
            break;
        }

        // Trim trailing newline / carriage-return
        let line = {
            let mut end = n;
            while end > 0 && (line_buf[end - 1] == b'\n' || line_buf[end - 1] == b'\r') {
                end -= 1;
            }
            &line_buf[..end]
        };

        if line.is_empty() {
            continue;
        }

        // Parse: split on spaces into cmd + args (max 8 args)
        let mut parts: heapless::Vec<&[u8], 9> = heapless::Vec::new();
        let mut start = 0usize;
        for i in 0..=line.len() {
            if i == line.len() || line[i] == b' ' {
                if i > start {
                    let _ = parts.push(&line[start..i]);
                }
                start = i + 1;
            }
        }

        if parts.is_empty() {
            continue;
        }

        // Built-in: exit
        if parts[0] == b"exit" {
            break;
        }

        // Build a null-terminated path for the command.
        // Try /bin/<cmd> first, then the raw path.
        let cmd_name = parts[0];
        let mut path_buf = [0u8; 128];
        let path = if cmd_name.starts_with(b"/") {
            // Absolute path — use directly
            let len = cmd_name.len().min(127);
            path_buf[..len].copy_from_slice(&cmd_name[..len]);
            path_buf[len] = 0;
            &path_buf[..=len]
        } else {
            // Relative — prepend /bin/
            let prefix = b"/bin/";
            let len = (prefix.len() + cmd_name.len()).min(127);
            path_buf[..prefix.len()].copy_from_slice(prefix);
            let cmd_len = len - prefix.len();
            path_buf[prefix.len()..len].copy_from_slice(&cmd_name[..cmd_len]);
            path_buf[len] = 0;
            &path_buf[..=len]
        };

        // Read the ELF binary
        let path_str = match core::str::from_utf8(path) {
            Ok(s) => s,
            Err(_) => {
                let _ = eclipse_syscall::call::write(1, b"sh: bad command name\r\n");
                continue;
            }
        };

        // Open the ELF binary, stat it, and read it into a fixed heap buffer.
        let fd = match eclipse_syscall::call::open(path_str, 0) {
            Ok(f) => f,
            Err(_) => {
                let _ = eclipse_syscall::call::write(1, b"sh: command not found\r\n");
                continue;
            }
        };

        let mut stat = eclipse_syscall::call::Stat {
            dev: 0, ino: 0, mode: 0, nlink: 0, uid: 0, gid: 0,
            size: 0, blksize: 0, blocks: 0, atime: 0, mtime: 0, ctime: 0,
        };
        let file_size = match eclipse_syscall::call::fstat(fd, &mut stat) {
            Ok(()) => stat.size as usize,
            Err(_) => {
                let _ = eclipse_syscall::call::close(fd);
                let _ = eclipse_syscall::call::write(1, b"sh: stat failed\r\n");
                continue;
            }
        };

        let mut elf: alloc::vec::Vec<u8> = alloc::vec::Vec::with_capacity(file_size);
        // SAFETY: Vec<u8> elements are u8 (always initialised below by read).
        unsafe { elf.set_len(file_size) };
        let mut read_total = 0usize;
        let mut ok = true;
        while read_total < file_size {
            match eclipse_syscall::call::read(fd, &mut elf[read_total..]) {
                Ok(0) => break,
                Ok(n) => read_total += n,
                Err(_) => { ok = false; break; }
            }
        }
        let _ = eclipse_syscall::call::close(fd);
        if !ok || read_total == 0 {
            let _ = eclipse_syscall::call::write(1, b"sh: read error\r\n");
            continue;
        }
        elf.truncate(read_total);

        // Spawn the command — inherit stdin/stdout/stderr (fds 0/1/2)
        match eclipse_syscall::call::spawn_with_stdio(&elf, Some(path_str), 0, 1, 2) {
            Ok(_child_pid) => {
                // Wait for any child to finish
                let mut status: u32 = 0;
                let _ = eclipse_syscall::call::waitpid(&mut status as *mut u32);
            }
            Err(_) => {
                let _ = eclipse_syscall::call::write(1, b"sh: exec failed\r\n");
            }
        }
    }
}

#[cfg(not(target_vendor = "eclipse"))]
fn main() {
    println!("Eclipse OS shell — not runnable on host.");
}
