extern crate alloc;

use std::prelude::v1::*;
use std::string::String;
use std::vec::Vec;
use std::fs;
use std::collections::BTreeMap;
use crate::parser::Statement;
use std::io::Write;

mod lexer;
mod parser;
mod executor;
mod builtins;
mod line_editor;
mod glob;
pub mod interp;
pub mod signals;



#[derive(Debug, Clone, PartialEq)]
pub enum JobStatus {
    Running,
    Stopped,
}

#[derive(Debug, Clone)]
pub struct Job {
    pub id: usize,
    pub pid: usize,
    pub cmd: String,
    pub status: JobStatus,
}

pub struct Shell {
    pub cwd: String,
    pub last_status: i32,
    pub history: Vec<String>,
    pub args: Vec<String>,
    pub functions: BTreeMap<String, Vec<Statement>>,
    pub aliases: BTreeMap<String, String>,
    pub jobs: Vec<Job>,
    pub next_job_id: usize,
}

impl Shell {
    pub fn new() -> Self {
        let mut shell = Self {
            cwd: String::from("/"),
            last_status: 0,
            history: Vec::new(),
            args: Vec::new(),
            functions: BTreeMap::new(),
            aliases: BTreeMap::new(),
            jobs: Vec::new(),
            next_job_id: 1,
        };

        signals::setup_signals();
        shell.init_env();
        shell.update_terminal_size();
        shell
    }

    fn init_env(&self) {
        std::env::set_var("SHELL", "/bin/rust-shell");
        std::env::set_var("USER", "moebius");
        std::env::set_var("PWD", &self.cwd);
        std::env::set_var("PATH", "/bin:/usr/bin");
        std::env::set_var("TERM", "xterm-256color");
    }

    pub fn update_terminal_size(&self) {
        #[cfg(target_vendor = "eclipse")]
        {
            let mut winsz = [0u16; 4];
            if eclipse_syscall::call::ioctl(0, 4, winsz.as_mut_ptr() as usize).is_ok() {
                let rows = winsz[0];
                let cols = winsz[1];
                if rows > 0 && cols > 0 {
                    std::env::set_var("LINES", ::alloc::format!("{}", rows));
                    std::env::set_var("COLUMNS", ::alloc::format!("{}", cols));
                }
            }
        }
    }

    pub fn add_history(&mut self, line: &str) {
        let trimmed = line.trim();
        if trimmed.is_empty() { return; }
        
        if let Some(last) = self.history.last() {
            if last == trimmed { return; }
        }
        
        self.history.push(String::from(trimmed));
        if self.history.len() > 1000 {
            let _ = self.history.remove(0);
        }
    }
}

fn main() {
    let mut shell = Shell::new();
    let args = std::env::args(); 
    
    if args.len() > 1 {
        if args[1] == "-c" && args.len() > 2 {
            interp::execute_line(&args[2], &mut shell);
        } else {
            shell.args = if args.len() > 2 { args[2..].to_vec() } else { Vec::new() };
            run_script(&args[1], &mut shell);
        }
    } else {
        run_interactive(&mut shell);
    }
}

fn run_interactive(shell: &mut Shell) {
    println!("Eclipse RustShell v0.1.0");
    loop {
        shell.update_terminal_size();
        let mut editor = line_editor::LineEditor::new(shell.history.clone(), shell.cwd.clone());
        
        let prompt = format!("\x1b[32mmoebius@eclipse\x1b[0m:\x1b[34;1m{}\x1b[0m$ ", shell.cwd);
        
        match editor.read_line(&prompt) {
            Ok(input) if !input.trim().is_empty() => {
                let trimmed = input.trim();
                shell.add_history(trimmed);
                interp::execute_line(trimmed, shell);
            }
            Ok(_) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                println!("^C");
                continue;
            }
            Err(_) => break,
        }
    }
}

fn run_script(path: &str, shell: &mut Shell) {
    if let Ok(content) = fs::read_to_string(path) {
        interp::execute_line(&content, shell);
    } else {
        eprintln!("rs: could not read script: {}", path);
    }
}
