//! Process Module - Process management using eclipse-libc
//!
//! Provides std-like Command and Child interfaces for spawning and managing processes.

use crate::libc::*;
use ::alloc::string::String;
use ::alloc::vec::Vec;
use crate::io::{Result, Error, ErrorKind};
use crate::fs;

/// A process builder, providing fine-grained control over how a new process should be spawned.
pub struct Command {
    program: String,
    args: Vec<String>,
    envs: Vec<(String, String)>,
}

impl Command {
    /// Constructs a new Command for launching the program at path program.
    pub fn new(program: &str) -> Self {
        Command {
            program: String::from(program),
            args: Vec::new(),
            envs: Vec::new(),
        }
    }
    
    /// Adds an argument to pass to the program.
    pub fn arg(&mut self, arg: &str) -> &mut Self {
        self.args.push(String::from(arg));
        self
    }
    
    /// Adds multiple arguments to pass to the program.
    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<crate::env::OsStr>,
    {
        for arg in args {
            self.arg(arg.as_ref());
        }
        self
    }
    
    /// Adds an environment variable to pass to the program.
    pub fn env(&mut self, key: &str, val: &str) -> &mut Self {
        self.envs.push((String::from(key), String::from(val)));
        self
    }

    /// Executes the command as a child process, returning a handle to it.
    pub fn spawn(&mut self) -> Result<Child> {
        let buf = fs::read(&self.program)?;

        // Build a NUL-terminated name for the kernel from the program basename.
        // We use a 16-byte stack buffer that is zero-initialised; after copying
        // up to 15 bytes of the basename the byte at index copy_len (and beyond)
        // is always 0, acting as the NUL terminator the kernel expects when it
        // reads from name_buf[0] via the raw pointer.
        let name_start = self.program.rfind('/').map(|i| i + 1).unwrap_or(0);
        let base = &self.program[name_start..];
        let mut name_buf = [0u8; 16];
        let copy_len = base.len().min(15);
        name_buf[..copy_len].copy_from_slice(&base.as_bytes()[..copy_len]);
        // Validate the copied bytes as UTF-8 (program names are always ASCII in
        // practice; the fallback "?" ensures the process always gets a visible
        // name even if something unexpected occurs).
        let name = core::str::from_utf8(&name_buf[..copy_len]).unwrap_or("?");

        match eclipse_syscall::call::spawn(&buf, Some(name)) {
            Ok(pid) => Ok(Child { pid: pid as pid_t }),
            Err(_) => Err(Error::new(ErrorKind::Other, "spawn failed")),
        }
    }

    /// Executes the command replacing stdin, stdout, stderr with specific file descriptors
    pub fn spawn_with_stdio(&mut self, fd_in: usize, fd_out: usize, fd_err: usize) -> Result<Child> {
        let buf = fs::read(&self.program)?;
        
        match eclipse_syscall::call::spawn_with_stdio(&buf, Some(&self.program), fd_in, fd_out, fd_err) {
            Ok(pid) => Ok(Child { pid: pid as pid_t }),
            Err(_) => Err(Error::new(ErrorKind::Other, "spawn_with_stdio syscall failed")),
        }
    }

    /// Executes the command as a child process, waiting for it to finish and collecting its exit status.
    pub fn status(&mut self) -> Result<ExitStatus> {
        let mut child = self.spawn()?;
        child.wait()
    }
}

/// Representation of a running or exited child process.
pub struct Child {
    pid: pid_t,
}

impl Child {
    /// Returns the OS-assigned process identifier associated with this child.
    pub fn id(&self) -> u32 {
        self.pid as u32
    }
    
    /// Forces the child process to exit.
    pub fn kill(&mut self) -> Result<()> {
        match eclipse_syscall::call::kill(self.pid as usize, 9) {
            Ok(_) => Ok(()),
            Err(_) => Err(Error::new(ErrorKind::Other, "kill failed")),
        }
    }
    
    /// Waits for the child to exit completely, returning the status that it exited with.
    pub fn wait(&mut self) -> Result<ExitStatus> {
        let mut status = 0u32;
        // Esperar a ESTE hijo (no “cualquier hijo”), para evitar recolectar un thread/pthread.
        match eclipse_syscall::call::wait_pid(&mut status as *mut u32, self.pid as usize) {
            Ok(_ret_pid) => Ok(ExitStatus { code: (status >> 8) as i32 }),
            Err(_) => Err(Error::new(ErrorKind::Other, "waitpid failed")),
        }
    }
}

/// Describes the result of a process after it has terminated.
#[derive(Debug)]
pub struct ExitStatus {
    code: i32,
}

impl ExitStatus {
    /// Returns the exit code of the process, if any.
    pub fn code(&self) -> Option<i32> {
        Some(self.code)
    }
    
    /// Returns true if the exit status of the process is successful.
    pub fn success(&self) -> bool {
        self.code == 0
    }
}
