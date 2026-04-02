use std::prelude::v1::*;
use std::io::{self, Write};
use std::string::String;
use std::vec::Vec;

pub struct LineEditor {
    buffer: String,
    history: Vec<String>,
    history_index: usize,
    temp_buffer: String, // Buffer when navigating history
    cwd: String,
    prompt: String,
}

impl LineEditor {
    pub fn new(history: Vec<String>, cwd: String) -> Self {
        Self {
            buffer: String::new(),
            history,
            history_index: 0,
            temp_buffer: String::new(),
            cwd,
            prompt: String::new(),
        }
    }

    pub fn read_line(&mut self, prompt: &str) -> io::Result<String> {
        self.buffer.clear();
        self.history_index = self.history.len();
        self.temp_buffer.clear();
        self.prompt = String::from(prompt);

        print!("{}", self.prompt);
        let _ = io::stdout().flush();

        loop {
            let mut b = [0u8; 1];
            if eclipse_syscall::call::read(0, &mut b).unwrap_or(0) != 1 {
                return Ok(String::new()); // EOF or Error
            }

            match b[0] {
                b'\n' | b'\r' => {
                    println!("");
                    return Ok(self.buffer.clone());
                }
                8 | 127 => { // Backspace
                    if !self.buffer.is_empty() {
                        let _ = self.buffer.pop();
                        print!("\u{0008} \u{0008}"); // Backspace, space, backspace
                        let _ = io::stdout().flush();
                    }
                }
                27 => { // Escape sequence
                    self.handle_escape();
                }
                9 => { // Tab
                    self.handle_tab();
                }
                3 => { // Ctrl+C
                    println!("^C");
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "Interrupted"));
                }
                b if b >= 32 && b <= 126 => {
                    self.buffer.push(b as char);
                    print!("{}", b as char);
                    let _ = io::stdout().flush();
                }
                _ => {}
            }
        }
    }

    fn handle_escape(&mut self) {
        let mut b = [0u8; 1];
        if eclipse_syscall::call::read(0, &mut b).unwrap_or(0) == 1 && b[0] == b'[' {
            if eclipse_syscall::call::read(0, &mut b).unwrap_or(0) == 1 {
                match b[0] {
                    b'A' => self.navigate_history(-1), // Up
                    b'B' => self.navigate_history(1),  // Down
                    _ => {}
                }
            }
        }
    }

    fn navigate_history(&mut self, delta: i32) {
        let new_index = if delta < 0 {
            if self.history_index > 0 { (self.history_index - 1) as i32 } else { 0 }
        } else {
            if self.history_index < self.history.len() { (self.history_index + 1) as i32 } else { self.history.len() as i32 }
        } as usize;

        if new_index == self.history_index && delta != 0 { return; }

        // Clear current line (including prompt if we want but here we only clear the buffer part)
        for _ in 0..self.buffer.len() {
            print!("\u{0008} \u{0008}");
        }

        if self.history_index == self.history.len() && delta < 0 {
            self.temp_buffer = self.buffer.clone();
        }

        self.history_index = new_index;
        if self.history_index == self.history.len() {
            self.buffer = self.temp_buffer.clone();
        } else {
            self.buffer = self.history[self.history_index].clone();
        }

        print!("{}", self.buffer);
        let _ = io::stdout().flush();
    }

    fn handle_tab(&mut self) {
        let last_word = self.buffer.split_whitespace().last().unwrap_or("");
        let mut matches = Vec::new();

        if let Ok(files) = self.list_dir(&self.cwd) {
            for f in files {
                if f.starts_with(last_word) {
                    matches.push(f);
                }
            }
        }

        if !self.buffer.contains(' ') {
             if let Ok(path_env) = std::env::var("PATH") {
                for dir in path_env.split(':') {
                    if let Ok(files) = self.list_dir(dir) {
                        for f in files {
                            if f.starts_with(last_word) && !matches.contains(&f) {
                                matches.push(f);
                            }
                        }
                    }
                }
            }
        }

        if matches.len() == 1 {
            let completion = &matches[0][last_word.len()..];
            self.buffer.push_str(completion);
            print!("{}", completion);
            let _ = io::stdout().flush();
        } else if matches.len() > 1 {
            println!("");
            for m in matches {
                print!("{}  ", m);
            }
            println!("");
            print!("{}{}", self.prompt, self.buffer);
            let _ = io::stdout().flush();
        }
    }

    fn list_dir(&self, path: &str) -> io::Result<Vec<String>> {
        let mut buf = [0u8; 4096];
        match eclipse_syscall::call::readdir(path, &mut buf) {
            Ok(n) => {
                let s = core::str::from_utf8(&buf[..n]).unwrap_or("");
                Ok(s.split('\n').filter(|f| !f.is_empty()).map(String::from).collect())
            }
            Err(_) => Err(io::Error::new(io::ErrorKind::Other, "readdir failed")),
        }
    }
}

