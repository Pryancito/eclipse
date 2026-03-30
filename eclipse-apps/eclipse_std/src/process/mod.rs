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
}

impl Command {
    /// Constructs a new Command for launching the program at path program.
    pub fn new(program: &str) -> Self {
        Command {
            program: String::from(program),
            args: Vec::new(),
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
    
    /// Executes the command as a child process, returning a handle to it.
    pub fn spawn(&mut self) -> Result<Child> {
        // Eclipse OS currently spawns from an ELF buffer
        let _buf = fs::read(&self.program)?;
        
        unsafe {
            let pid = crate::libc::spawn(
                self.program.as_ptr() as *const c_char,
                core::ptr::null(),
                core::ptr::null()
            );
            if pid < 0 {
                return Err(Error::new(ErrorKind::Other, "spawn failed"));
            }
            
            Ok(Child { pid })
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
        // La syscall devuleve PID cuando retorna
        match eclipse_syscall::call::waitpid(&mut status as *mut u32) {
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
