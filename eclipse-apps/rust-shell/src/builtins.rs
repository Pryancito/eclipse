use crate::parser::SimpleCommand;
use crate::Shell;
use std::prelude::v1::*;
use std::env;
use std::fs;
use libc;

pub fn is_builtin(name: &str) -> bool {
    match name {
        "cd" | "pwd" | "exit" | "export" | "alias" | "unalias" | "history" | "jobs" | "fg" | "bg" |
        "ls" | "ps" | "cat" | "echo" | "printf" | "mkdir" | "rm" | "env" | "printenv" | "which" | "unset" => true,
        _ => false,
    }
}

pub fn try_execute(cmd: &SimpleCommand, shell: &mut Shell) -> Option<i32> {
    if cmd.argv.is_empty() { return None; }
    
    match cmd.argv[0].as_str() {
        "cd" => {
            let target = if cmd.argv.len() > 1 { &cmd.argv[1] } else { "/" };
            if target.starts_with('/') {
                shell.cwd = String::from(target);
            } else {
                if !shell.cwd.ends_with('/') { shell.cwd.push('/'); }
                shell.cwd.push_str(target);
            }
            Some(0)
        }
        "pwd" => {
            println!("{}", shell.cwd);
            Some(0)
        }
        "exit" => {
            let code = if cmd.argv.len() > 1 {
                cmd.argv[1].parse::<i32>().unwrap_or(0)
            } else {
                0
            };
            unsafe {
                libc::exit(code);
            }
        }
        "export" => {
            for arg in cmd.argv.iter().skip(1) {
                let parts: Vec<&str> = arg.splitn(2, '=').collect();
                if parts.len() == 2 {
                    env::set_var(parts[0], parts[1]);
                }
                // export VAR (no value) — already in env, nothing to do
            }
            Some(0)
        }
        "alias" => {
            if cmd.argv.len() == 1 {
                for (name, value) in &shell.aliases {
                    println!("alias {}='{}'", name, value);
                }
            } else {
                for arg in &cmd.argv[1..] {
                    if let Some(pos) = arg.find('=') {
                        let name = &arg[..pos];
                        let value = &arg[pos+1..];
                        let _ = shell.aliases.insert(String::from(name), String::from(value));
                    } else {
                        if let Some(value) = shell.aliases.get(arg) {
                            println!("alias {}='{}'", arg, value);
                        } else {
                            eprintln!("rs: alias: {}: not found", arg);
                        }
                    }
                }
            }
            Some(0)
        }
        "unalias" => {
            if cmd.argv.len() > 1 {
                for arg in &cmd.argv[1..] {
                    let _ = shell.aliases.remove(arg);
                }
            } else {
                eprintln!("unalias: usage: unalias name [name ...]");
            }
            Some(0)
        }
        "history" => {
            for (i, line) in shell.history.iter().enumerate() {
                println!("{:4}  {}", i + 1, line);
            }
            Some(0)
        }
        "jobs" => {
            for job in &shell.jobs {
                let status_str = match job.status {
                    crate::JobStatus::Running => "Running",
                    crate::JobStatus::Stopped => "Stopped",
                };
                println!("[{}] {} {} {}", job.id, job.pid, status_str, job.cmd);
            }
            Some(0)
        }
        "fg" => {
            if cmd.argv.len() < 2 {
                eprintln!("fg: usage: fg %job_id");
                return Some(1);
            }
            let job_id_str = if cmd.argv[1].starts_with('%') { &cmd.argv[1][1..] } else { &cmd.argv[1] };
            if let Ok(id) = job_id_str.parse::<usize>() {
                if let Some(pos) = shell.jobs.iter().position(|j| j.id == id) {
                    let job = shell.jobs.remove(pos);
                    println!("{}", job.cmd);
                    // Send SIGCONT
                    let _ = eclipse_syscall::call::kill(job.pid, 18);
                    crate::signals::set_fg_pid(job.pid);
                    let status = crate::executor::wait_for_child(job.pid, shell, &job.cmd);
                    crate::signals::set_fg_pid(0);
                    Some(status)
                } else {
                    eprintln!("fg: {}: no such job", cmd.argv[1]);
                    Some(1)
                }
            } else {
                eprintln!("fg: {}: invalid job id", cmd.argv[1]);
                Some(1)
            }
        }
        "ls" => {
            let path = if cmd.argv.len() > 1 { &cmd.argv[1] } else { "." };
            // Ensure we use the resolved path relative to CWD
            let abs_path = if path.starts_with('/') { 
                String::from(path) 
            } else {
                let mut p = shell.cwd.clone();
                if !p.ends_with('/') { p.push('/'); }
                p.push_str(path);
                p
            };
            
            let mut buf = [0u8; 8192];
            match eclipse_syscall::call::readdir(&abs_path, &mut buf) {
                Ok(n) if n > 0 => {
                    let mut names: Vec<&str> = buf[..n].split(|&b| b == b'\n')
                        .filter_map(|s| core::str::from_utf8(s).ok())
                        .filter(|s| !s.is_empty())
                        .collect();
                    names.sort_unstable();
                    for name in names {
                        if abs_path.starts_with("/bin") { print!("\x1b[32m"); }
                        else if abs_path.starts_with("/tmp") { print!("\x1b[34;1m"); }
                        print!("{}", name); println!("\x1b[0m");
                    }
                }
                _ => eprintln!("rs: ls: {}: error reading directory", path),
            }
            Some(0)
        }
        "ps" => {
            {
                let mut list = [eclipse_syscall::ProcessInfo::default(); 48];
                if let Ok(n) = eclipse_syscall::get_process_list(&mut list) {
                    println!("  PID  STAT  NAME\n  ---  ----  ----");
                    for info in list.iter().take(n) {
                        if info.pid == 0 { continue; }
                        let end = info.name.iter().position(|&b| b == 0).unwrap_or(16);
                        let raw = core::str::from_utf8(&info.name[..end]).unwrap_or("?");
                        let name = raw.rsplit('/').next().unwrap_or(raw);
                        let stat = match info.state { 
                            0 | 1 => "R", // Ready / Running
                            2 => "S",     // Blocked (Sleeping)
                            3 => "Z",     // Terminated (Zombie)
                            4 => "T",     // Stopped (Tracing/Stopped)
                            _ => "?" 
                        };
                        println!("  {:3}  {:4}  {}", info.pid, stat, name);
                    }
                }
            }
            Some(0)
        }
        "cat" => {
            for path in &cmd.argv[1..] {
                // Resolved path relative to CWD
                let abs_path = if path.starts_with('/') { 
                    String::from(path) 
                } else {
                    let mut p = shell.cwd.clone();
                    if !p.ends_with('/') { p.push('/'); }
                    p.push_str(path);
                    p
                };
                if let Ok(content) = std::fs::read_to_string(&abs_path) {
                    print!("{}", content);
                } else {
                    eprintln!("rs: cat: {}: no such file", path);
                }
            }
            Some(0)
        }
        "echo" => {
            let (no_newline, start) = if cmd.argv.len() > 1 && cmd.argv[1] == "-n" {
                (true, 2)
            } else {
                (false, 1)
            };
            for (i, arg) in cmd.argv.iter().enumerate().skip(start) {
                if i > start { print!(" "); }
                print!("{}", arg);
            }
            if !no_newline { println!(); }
            Some(0)
        }
        "printf" => {
            if cmd.argv.len() < 2 { return Some(0); }
            let fmt = &cmd.argv[1];
            let mut out = String::new();
            let mut chars = fmt.chars().peekable();
            while let Some(c) = chars.next() {
                if c == '\\' {
                    match chars.next() {
                        Some('n') => out.push('\n'),
                        Some('t') => out.push('\t'),
                        Some('r') => out.push('\r'),
                        Some('\\') => out.push('\\'),
                        Some(other) => { out.push('\\'); out.push(other); }
                        None => out.push('\\'),
                    }
                } else {
                    out.push(c);
                }
            }
            print!("{}", out);
            Some(0)
        }
        "bg" => {
            if cmd.argv.len() < 2 {
                eprintln!("bg: usage: bg %job_id");
                return Some(1);
            }
            let job_id_str = if cmd.argv[1].starts_with('%') { &cmd.argv[1][1..] } else { &cmd.argv[1] };
            if let Ok(id) = job_id_str.parse::<usize>() {
                if let Some(job) = shell.jobs.iter_mut().find(|j| j.id == id) {
                    println!("[{}] {} &", job.id, job.cmd);
                    job.status = crate::JobStatus::Running;
                    // Send SIGCONT
                    let _ = eclipse_syscall::call::kill(job.pid, 18);
                    Some(0)
                } else {
                    eprintln!("bg: {}: no such job", cmd.argv[1]);
                    Some(1)
                }
            } else {
                eprintln!("bg: {}: invalid job id", cmd.argv[1]);
                Some(1)
            }
        }
        "mkdir" => {
            for path in &cmd.argv[1..] {
                let abs_path = if path.starts_with('/') { String::from(path) } else {
                    let mut p = shell.cwd.clone();
                    if !p.ends_with('/') { p.push('/'); }
                    p.push_str(path);
                    p
                };
                if let Err(_) = eclipse_syscall::call::mkdir(&abs_path, 0o755) {
                    eprintln!("rs: mkdir: {}: error", path);
                }
            }
            Some(0)
        }
        "rm" => {
            for path in &cmd.argv[1..] {
                let abs_path = if path.starts_with('/') { String::from(path) } else {
                    let mut p = shell.cwd.clone();
                    if !p.ends_with('/') { p.push('/'); }
                    p.push_str(path);
                    p
                };
                if let Err(_) = eclipse_syscall::call::unlink(&abs_path) {
                    eprintln!("rs: rm: {}: error", path);
                }
            }
            Some(0)
        }
        "env" | "printenv" => {
            for (key, value) in std::env::vars() {
                println!("{}={}", key, value);
            }
            Some(0)
        }
        "unset" => {
            for arg in cmd.argv.iter().skip(1) {
                std::env::remove_var(arg.as_str());
            }
            Some(0)
        }
        "which" => {
            let path_env = std::env::var("PATH").unwrap_or_default();
            for prog in cmd.argv.iter().skip(1) {
                let mut found = false;
                for dir in path_env.split(':') {
                    if dir.is_empty() { continue; }
                    let full = if dir.ends_with('/') {
                        format!("{}{}", dir, prog)
                    } else {
                        format!("{}/{}", dir, prog)
                    };
                    if fs::metadata(&full).is_ok() {
                        println!("{}", full);
                        found = true;
                        break;
                    }
                }
                if !found {
                    eprintln!("rs: which: {}: not found", prog);
                }
            }
            Some(0)
        }
        _ => None,
    }
}
