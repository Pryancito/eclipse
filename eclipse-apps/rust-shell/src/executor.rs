use crate::parser::{Pipeline, SimpleCommand, Redirection, Statement, LogicalOp};
use crate::Shell;
use std::prelude::v1::*;
use std::vec::Vec;
use std::string::String;
use std::fs;
use std::collections::BTreeSet;

use eclipse_syscall;

pub fn execute_statement(statement: Statement, shell: &mut Shell) -> i32 {
    match statement {
        Statement::AndOrList(list) => {
            let mut status = execute_pipeline(list.head, shell);
            for (op, next) in list.tail {
                match op {
                    LogicalOp::And => {
                        if status == 0 {
                            status = execute_pipeline(next, shell);
                        }
                    }
                    LogicalOp::Or => {
                        if status != 0 {
                            status = execute_pipeline(next, shell);
                        }
                    }
                }
            }
            status
        }
        Statement::If { condition, then_block, else_block } => {
            let status = execute_statement(*condition, shell);
            if status == 0 {
                let mut last = 0;
                for s in then_block { last = execute_statement(s, shell); }
                last
            } else if let Some(eb) = else_block {
                let mut last = 0;
                for s in eb { last = execute_statement(s, shell); }
                last
            } else {
                0
            }
        }
        Statement::While { condition, body } => {
            let mut last = 0;
            while execute_statement((*condition).clone(), shell) == 0 {
                for s in body.clone() { last = execute_statement(s, shell); }
            }
            last
        }
        Statement::For { var, list, body } => {
            let mut last = 0;
            for item in list {
                std::env::set_var(&var, &item);
                for s in body.clone() { last = execute_statement(s, shell); }
            }
            last
        }
        Statement::Block(stmts) => {
            let mut last = 0;
            for s in stmts { last = execute_statement(s, shell); }
            last
        }
        Statement::FunctionDef { name, body } => {
            let _ = shell.functions.insert(name, body);
            0
        }
    }
}

pub fn execute_pipeline(mut pipeline: Pipeline, shell: &mut Shell) -> i32 {
    let n = pipeline.commands.len();
    if n == 0 { return 0; }

    for cmd in &mut pipeline.commands {
        expand_aliases(cmd, shell);
    }

    // Optimization: Single command builtins/functions run in-process
    // ONLY if there are no redirections.
    if n == 1 {
        let cmd = &pipeline.commands[0];
        if !cmd.argv.is_empty() && cmd.redirects.is_empty() {
            let name = &cmd.argv[0];
            if let Some(body) = shell.functions.get(name).cloned() {
                let old_args = shell.args.clone();
                shell.args = cmd.argv[1..].to_vec();
                let mut last = 0;
                for s in body {
                    let mut s_exec = s.clone();
                    s_exec.expand_vars(shell);
                    last = crate::executor::execute_statement(s_exec, shell);
                }
                shell.args = old_args;
                return last;
            }
            if let Some(status) = crate::builtins::try_execute(cmd, shell) {
                return status;
            }
        }
    }

    let mut pids = Vec::new();
    let mut prev_fd = 0; 

    for (i, cmd) in pipeline.commands.iter().enumerate() {
        let is_last = i == n - 1;
        let mut fd_out = 1; 

        let mut pipe_fds = [0u32; 2];
        if !is_last {
            if eclipse_syscall::call::pipe(&mut pipe_fds).is_ok() {
                fd_out = pipe_fds[1] as usize;
            }
        }

        if let Some(pid) = spawn_command(cmd, prev_fd, fd_out, 2, shell) {
            pids.push(pid);
        }

        if prev_fd != 0 { let _ = eclipse_syscall::call::close(prev_fd); }
        if !is_last {
            let _ = eclipse_syscall::call::close(pipe_fds[1] as usize);
            prev_fd = pipe_fds[0] as usize;
        }
    }

    let mut cmd_str = String::new();
    for (i, cmd) in pipeline.commands.iter().enumerate() {
        if i > 0 { cmd_str.push_str(" | "); }
        cmd_str.push_str(&cmd.argv.join(" "));
    }

    if pipeline.ampersand {
        for pid in pids {
            let job = crate::Job {
                id: shell.next_job_id,
                pid,
                cmd: cmd_str.clone(),
                status: crate::JobStatus::Running,
            };
            shell.jobs.push(job);
            println!("[{}] {}", shell.next_job_id, pid);
            shell.next_job_id += 1;
        }
        return 0;
    }

    let mut last_status = 0;
    for pid in pids {
        crate::signals::set_fg_pid(pid);
        last_status = wait_for_child(pid, shell, &cmd_str);
        crate::signals::set_fg_pid(0);
    }
    last_status
}

fn expand_aliases(cmd: &mut SimpleCommand, shell: &Shell) {
    let mut expanded = BTreeSet::new();
    loop {
        if cmd.argv.is_empty() { break; }
        let name = &cmd.argv[0];
        if expanded.contains(name) { break; }
        
        if let Some(value) = shell.aliases.get(name) {
            let _ = expanded.insert(name.clone());
            let parts: Vec<String> = value.split_whitespace().map(String::from).collect();
            if parts.is_empty() { 
                let _ = cmd.argv.remove(0);
            } else {
                let rest = cmd.argv.split_off(1);
                cmd.argv = parts;
                cmd.argv.extend(rest);
            }
        } else {
            break;
        }
    }
}

fn spawn_command(cmd: &SimpleCommand, mut fd_in: usize, mut fd_out: usize, mut fd_err: usize, shell: &mut Shell) -> Option<usize> {
    if cmd.argv.is_empty() { return None; }
    
    let prog = &cmd.argv[0];
    
    // Check if it's a builtin or function that needs a subshell
    if crate::builtins::is_builtin(prog) || shell.functions.contains_key(prog) {
        return spawn_subshell(cmd, fd_in, fd_out, fd_err, shell);
    }

    let path = resolve_path(prog, &shell.cwd);
    let data = fs::read(&path).ok()?;
    
    if data.starts_with(b"#! ") || data.starts_with(b"#!/") {
        if let Ok(content) = core::str::from_utf8(&data) {
           crate::interp::execute_line(content, shell);
           return None;
        }
    }
    
    let mut owned_fds = Vec::new();
    for redir in &cmd.redirects {
        match redir {
            Redirection::Output(fd, file) => {
                let fpath = resolve_absolute(file, &shell.cwd);
                if let Ok(new_fd) = eclipse_syscall::call::open(fpath.as_str(), eclipse_syscall::flag::O_WRONLY | eclipse_syscall::flag::O_CREAT | eclipse_syscall::flag::O_TRUNC) {
                    if *fd == 0 { fd_in = new_fd; }
                    else if *fd == 1 { fd_out = new_fd; }
                    else if *fd == 2 { fd_err = new_fd; }
                    owned_fds.push(new_fd);
                }
            }
            Redirection::Append(fd, file) => {
                let fpath = resolve_absolute(file, &shell.cwd);
                if let Ok(new_fd) = eclipse_syscall::call::open(fpath.as_str(), eclipse_syscall::flag::O_WRONLY | eclipse_syscall::flag::O_CREAT | eclipse_syscall::flag::O_APPEND) {
                    if *fd == 0 { fd_in = new_fd; }
                    else if *fd == 1 { fd_out = new_fd; }
                    else if *fd == 2 { fd_err = new_fd; }
                    owned_fds.push(new_fd);
                }
            }
            Redirection::Input(fd, file) => {
                let fpath = resolve_absolute(file, &shell.cwd);
                if let Ok(new_fd) = eclipse_syscall::call::open(fpath.as_str(), eclipse_syscall::flag::O_RDONLY) {
                    if *fd == 0 { fd_in = new_fd; }
                    else if *fd == 1 { fd_out = new_fd; }
                    else if *fd == 2 { fd_err = new_fd; }
                    owned_fds.push(new_fd);
                }
            }
        }
    }

    let res = eclipse_syscall::call::spawn_with_stdio(&data, Some(prog), fd_in, fd_out, fd_err);
    
    // Cleanup temporary redirections in parent
    for fd in owned_fds { let _ = eclipse_syscall::call::close(fd); }

    if let Ok(pid) = res {
        let mut arg_bytes = Vec::new();
        for arg in &cmd.argv {
            arg_bytes.extend_from_slice(arg.as_bytes());
            arg_bytes.push(0);
        }
        let _ = eclipse_syscall::call::set_child_args(pid, &arg_bytes);
        Some(pid)
    } else {
        None
    }
}

fn spawn_subshell(cmd: &SimpleCommand, fd_in: usize, fd_out: usize, fd_err: usize, _shell: &Shell) -> Option<usize> {
    // Construct command string: name arg1 arg2...
    let mut cmd_str = String::new();
    for (i, arg) in cmd.argv.iter().enumerate() {
        if i > 0 { cmd_str.push(' '); }
        cmd_str.push_str(arg);
    }
    
    let shell_path = "/bin/rust-shell";
    let data = fs::read(shell_path).ok()?;
    
    let res = eclipse_syscall::call::spawn_with_stdio(&data, Some("rust-shell"), fd_in, fd_out, fd_err);
    if let Ok(pid) = res {
        let mut arg_bytes = Vec::new();
        // argv: ["rust-shell", "-c", cmd_str]
        for arg in &["rust-shell", "-c", &cmd_str] {
            arg_bytes.extend_from_slice(arg.as_bytes());
            arg_bytes.push(0);
        }
        let _ = eclipse_syscall::call::set_child_args(pid, &arg_bytes);
        Some(pid)
    } else {
        None
    }
}

fn resolve_path(prog: &str, cwd: &str) -> String {
    if prog.is_empty() { return String::new(); }
    if prog.starts_with('/') { return String::from(prog); }
    if prog.starts_with("./") {
        let mut path = String::from(cwd);
        if !path.ends_with('/') { path.push('/'); }
        path.push_str(&prog[2..]);
        return path;
    }
    if let Ok(path_env) = std::env::var("PATH") {
        for dir in path_env.split(':') {
            let mut full_path = String::from(dir);
            if !full_path.ends_with('/') { full_path.push('/'); }
            full_path.push_str(prog);
            if fs::metadata(&full_path).is_ok() { return full_path; }
        }
    }
    format!("/bin/{}", prog)
}

fn resolve_absolute(file: &str, cwd: &str) -> String {
    if file.starts_with('/') { return String::from(file); }
    let mut path = String::from(cwd);
    if !path.ends_with('/') { path.push('/'); }
    path.push_str(file);
    path
}

pub fn wait_for_child(pid: usize, shell: &mut crate::Shell, cmd: &str) -> i32 {
    let mut status = 0u32;
    // Flag 2 is WUNTRACED. Since we are NOT passing WNOHANG (flag 1),
    // this syscall will BLOCK until the child state changes.
    let res = unsafe { eclipse_syscall::syscall3(538, &mut status as *mut _ as usize, pid, 2) };
    
    if res as usize == pid {
        // Check if stopped (Ctrl+Z)
        if (status & 0xFF) == 0x7F {
            let sig = (status >> 8) & 0xFF;
            let job = crate::Job {
                id: shell.next_job_id,
                pid,
                cmd: cmd.to_string(),
                status: crate::JobStatus::Stopped,
            };
            shell.jobs.push(job);
            println!("\n[{}] Stopped: {}", shell.next_job_id, cmd);
            shell.next_job_id += 1;
            return 148; // Common exit code for stopped job (128 + SIGTSTP)
        }
        // Normal exit or signal termination
        return ((status >> 8) & 0xFF) as i32;
    } else {
        // Error or process already harvested
        eprintln!("waitpid returned unexpected: {}", res);
        return 1;
    }
}

