use crate::lexer::Lexer;
use crate::parser::Parser;
use crate::executor;
use crate::Shell;
use std::prelude::v1::*;
use std::string::String;
use std::fs;

use eclipse_syscall;

pub fn execute_line(line: &str, shell: &mut Shell) {
    let mut lex = Lexer::new(line);
    let tokens = lex.tokenize();
    let mut par = Parser::new(tokens);
    
    while let Some(mut stmt) = par.parse_statement() {
        stmt.expand_vars(shell);
        shell.last_status = executor::execute_statement(stmt, shell);
    }
}

pub fn capture_output(line: &str, shell: &mut Shell) -> String {
    let mut pipe_fds = [0u32; 2];
    if eclipse_syscall::call::pipe(&mut pipe_fds).is_err() {
        return String::new();
    }
    
    let read_fd = pipe_fds[0] as usize;
    let write_fd = pipe_fds[1] as usize;
    
    // Define a dummy command to trigger subshell for the whole line
    let shell_path = "/bin/rust-shell";
    if let Ok(data) = fs::read(shell_path) {
        // Spawn rust-shell -c "line"
        // fd_in = 0, fd_out = write_fd, fd_err = 2
        if let Ok(pid) = eclipse_syscall::call::spawn_with_stdio(&data, Some("rust-shell"), 0, write_fd, 2) {
            let mut arg_bytes = Vec::new();
            for arg in &["rust-shell", "-c", line] {
                arg_bytes.extend_from_slice(arg.as_bytes());
                arg_bytes.push(0);
            }
            let _ = eclipse_syscall::call::set_child_args(pid, &arg_bytes);
            
            // Close write end in parent
            let _ = eclipse_syscall::call::close(write_fd);
            
            // Read from read end
            let mut captured = String::new();
            let mut buf = [0u8; 1024];
            loop {
                match eclipse_syscall::call::read(read_fd, &mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Ok(s) = core::str::from_utf8(&buf[..n]) {
                            captured.push_str(s);
                        }
                    }
                    Err(_) => break,
                }
            }
            
            let _ = eclipse_syscall::call::close(read_fd);
            let _ = executor::wait_for_child(pid, shell, line);
            return captured;
        }
    }
    
    let _ = eclipse_syscall::call::close(read_fd);
    let _ = eclipse_syscall::call::close(write_fd);
    String::new()
}
