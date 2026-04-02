use std::vec::Vec;
use std::string::String;

#[cfg(target_vendor = "eclipse")]
use eclipse_syscall;

// ============================================================================
// Constantes
// ============================================================================
const TIOCGWINSZ: usize = 4;
const FIONREAD:   usize = 2;
const HISTORY_MAX: usize = 100;
const CMD_NOT_FOUND: i32 = 127;

// ============================================================================
// Estado global del shell
// ============================================================================

static mut CWD:       String = String::new();
static mut LAST_EXIT: i32  = 0;
static mut TERM_COLS: u16  = 80;
static mut TERM_ROWS: u16  = 24;

// ============================================================================
// Historial de comandos
// ============================================================================

struct History {
    entries: Vec<String>,
    pos: usize, // posición de navegación (entries.len() = "sin navegar")
}

impl History {
    fn new() -> Self {
        Self { entries: Vec::new(), pos: 0 }
    }

    fn push(&mut self, line: &str) {
        if line.is_empty() { return; }
        if self.entries.last().map(|s| s.as_str()) == Some(line) {
            self.reset_pos();
            return;
        }
        if self.entries.len() >= HISTORY_MAX {
            let _ = self.entries.remove(0);
        }
        self.entries.push(String::from(line));
        self.reset_pos();
    }

    fn reset_pos(&mut self) {
        self.pos = self.entries.len();
    }

    fn up(&mut self) -> Option<&str> {
        if self.entries.is_empty() { return None; }
        if self.pos > 0 { self.pos -= 1; }
        self.entries.get(self.pos).map(|s| s.as_str())
    }

    fn down(&mut self) -> &str {
        if self.pos < self.entries.len() { self.pos += 1; }
        if self.pos == self.entries.len() { "" } else { &self.entries[self.pos] }
    }
}

// ============================================================================
// Expansión de variables ($VAR, $?, $$)
// ============================================================================

fn expand_var(token: &str) -> String {
    let mut result = String::new();
    let bytes = token.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            i += 1;
            if i >= bytes.len() { result.push('$'); break; }
            if bytes[i] == b'?' {
                result.push_str(&format!("{}", unsafe { LAST_EXIT }));
                i += 1;
            } else if bytes[i] == b'$' {
                result.push_str(&format!("{}", eclipse_syscall::call::getpid()));
                i += 1;
            } else if bytes[i] == b'{' {
                i += 1;
                let start = i;
                while i < bytes.len() && bytes[i] != b'}' { i += 1; }
                if let Ok(name) = core::str::from_utf8(&bytes[start..i]) {
                    result.push_str(&lookup_var(name));
                }
                if i < bytes.len() { i += 1; }
            } else {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                if let Ok(name) = core::str::from_utf8(&bytes[start..i]) {
                    result.push_str(&lookup_var(name));
                }
            }
        } else {
            result.push(bytes[i] as char);
            i += 1;
        }
    }
    result
}

fn lookup_var(name: &str) -> String {
    std::env::var(name).unwrap_or_default()
}

// ============================================================================
// Resolución de rutas (CWD + rpath -> abspath)
// ============================================================================

fn resolve_path(rel: &str) -> String {
    if rel.starts_with('/') {
        return normalize_path(rel);
    }
    let joined = unsafe {
        if CWD.ends_with('/') {
            format!("{}{}", CWD, rel)
        } else {
            format!("{}/{}", CWD, rel)
        }
    };
    normalize_path(&joined)
}

fn normalize_path(path: &str) -> String {
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => { let _ = parts.pop(); }
            _ => { parts.push(part); }
        }
    }
    let mut res = String::from("/");
    res.push_str(&parts.join("/"));
    res
}

// ============================================================================
// Parsing
// ============================================================================

#[derive(Debug, Clone)]
enum Token {
    Word(String),
    Pipe,
    Redirect(u8), // 1 = >, 2 = >>
    Background,   // &
}

fn tokenize(line: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        if in_single {
            if c == '\'' { in_single = false; } else { current.push(c); }
        } else if in_double {
            if c == '"' { in_double = false; } else { current.push(c); }
        } else {
            match c {
                '\'' => in_single = true,
                '"'  => in_double = true,
                '|'  => {
                    if !current.is_empty() { tokens.push(Token::Word(current.clone())); current.clear(); }
                    tokens.push(Token::Pipe);
                }
                '&'  => {
                    if !current.is_empty() { tokens.push(Token::Word(current.clone())); current.clear(); }
                    tokens.push(Token::Background);
                }
                '>' => {
                    if !current.is_empty() { tokens.push(Token::Word(current.clone())); current.clear(); }
                    if chars.peek() == Some(&'>') {
                        let _ = chars.next();
                        tokens.push(Token::Redirect(2));
                    } else {
                        tokens.push(Token::Redirect(1));
                    }
                }
                ' ' | '\t' => {
                    if !current.is_empty() { tokens.push(Token::Word(current.clone())); current.clear(); }
                }
                _ => current.push(c),
            }
        }
    }
    if !current.is_empty() { tokens.push(Token::Word(current)); }
    tokens
}

#[derive(Default)]
struct SimpleCmd {
    argv: Vec<String>,
    redirect_out: Option<(String, bool)>, // (path, append)
}

struct Pipeline {
    cmds:       Vec<SimpleCmd>,
    background: bool,
}

fn parse_pipeline(tokens: Vec<Token>) -> Pipeline {
    let mut cmds: Vec<SimpleCmd> = Vec::new();
    let mut current = SimpleCmd::default();
    let mut next_is_redir: Option<u8> = None;
    let mut background = false;

    for token in tokens {
        if let Some(redir_type) = next_is_redir.take() {
            if let Token::Word(w) = token {
                current.redirect_out = Some((resolve_path(&expand_var(&w)), redir_type == 2));
            }
            continue;
        }
        match token {
            Token::Word(w)     => current.argv.push(expand_var(&w)),
            Token::Pipe        => {
                if !current.argv.is_empty() { cmds.push(current); current = SimpleCmd::default(); }
            }
            Token::Redirect(t) => { next_is_redir = Some(t); }
            Token::Background  => { background = true; }
        }
    }
    if !current.argv.is_empty() { cmds.push(current); }
    Pipeline { cmds, background }
}

// ============================================================================
// Terminal helper
// ============================================================================

fn update_terminal_size() {
    let mut winsz = [0u16; 4];
    if eclipse_syscall::call::ioctl(0, TIOCGWINSZ, winsz.as_mut_ptr() as usize).is_ok() {
        let rows = winsz[0];
        let cols = winsz[1];
        if rows > 0 && cols > 0 {
            unsafe { TERM_ROWS = rows; TERM_COLS = cols; }
            std::env::set_var("LINES",   format!("{}", rows));
            std::env::set_var("COLUMNS", format!("{}", cols));
        }
    }
}

fn sh_write(fd: usize, s: &str) {
    let _ = eclipse_syscall::call::write(fd, s.as_bytes());
}

fn sh_print(s: &str)   { sh_write(1, s); }
fn sh_println(s: &str) { sh_print(s); sh_print("\n"); }
fn sh_eprint(s: &str)  { sh_write(2, s); }
fn sh_eprintln(s: &str){ sh_eprint(s); sh_eprint("\n"); }

// ============================================================================
// Builtins
// ============================================================================

fn try_builtin(argv: &[String]) -> Option<i32> {
    if argv.is_empty() { return Some(0); }
    match argv[0].as_str() {
        "echo" => {
            let (no_newline, words) = if argv.get(1).map(|s| s.as_str()) == Some("-n") {
                (true, &argv[2..])
            } else {
                (false, &argv[1..])
            };
            sh_print(&words.join(" "));
            if !no_newline { sh_print("\n"); }
            Some(0)
        }
        "printf" => {
            let s = argv.get(1).map(|s| s.as_str()).unwrap_or("");
            let mut out = String::new();
            let mut cs = s.chars();
            while let Some(c) = cs.next() {
                if c == '\\' {
                    match cs.next() {
                        Some('n') => out.push('\n'),
                        Some('t') => out.push('\t'),
                        Some('\\') => out.push('\\'),
                        Some(o) => { out.push('\\'); out.push(o); }
                        None => out.push('\\'),
                    }
                } else { out.push(c); }
            }
            sh_print(&out);
            Some(0)
        }
        "env" | "printenv" => {
            for key in &["TERM", "HOME", "PATH", "LINES", "COLUMNS", "SHELL", "USER", "PWD", "EDITOR"] {
                if let Ok(val) = std::env::var(key) { sh_println(&format!("{}={}", key, val)); }
            }
            sh_println(&format!("?={}", unsafe { LAST_EXIT }));
            Some(0)
        }
        "pwd" => { sh_println(unsafe { &CWD }); Some(0) }
        "cd" => {
            let dest = argv.get(1).map(|s| s.as_str()).unwrap_or("/");
            let new_path = resolve_path(dest);
            let mut buf = [0u8; 1];
            match eclipse_syscall::call::readdir(&new_path, &mut buf) {
                Ok(_) => {
                    unsafe { CWD = new_path; }
                    std::env::set_var("PWD", unsafe { &CWD });
                }
                _ => sh_eprintln(&format!("cd: {}: no es un directorio", dest)),
            }
            Some(0)
        }
        "ls" => {
            let path = argv.get(1).map(|s| s.as_str()).unwrap_or(".");
            let abs_path = resolve_path(path);
            let mut buf = [0u8; 8192];
            match eclipse_syscall::call::readdir(&abs_path, &mut buf) {
                Ok(n) if n > 0 => {
                    let mut names: Vec<&str> = buf[..n].split(|&b| b == b'\n')
                        .filter_map(|s| core::str::from_utf8(s).ok()).filter(|s| !s.is_empty()).collect();
                    names.sort_unstable();
                    for name in names {
                        if abs_path.starts_with("/bin") { sh_print("\x1b[32m"); }
                        else if abs_path.starts_with("/tmp") { sh_print("\x1b[34;1m"); }
                        sh_print(name); sh_println("\x1b[0m");
                    }
                }
                _ => sh_eprintln(&format!("ls: {}: error", path)),
            }
            Some(0)
        }
        "cat" => {
            for path in &argv[1..] {
                let abs = resolve_path(path);
                if let Ok(fd) = eclipse_syscall::call::open(&abs, 0) {
                    let mut b = [0u8; 4096];
                    loop {
                        match eclipse_syscall::call::read(fd, &mut b) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => { let _ = eclipse_syscall::call::write(1, &b[..n]); }
                        }
                    }
                    let _ = eclipse_syscall::call::close(fd);
                } else { sh_eprintln(&format!("cat: {}: no existe", path)); }
            }
            Some(0)
        }
        "ps" => {
            let mut list = [eclipse_syscall::ProcessInfo::default(); 48];
            if let Ok(n) = eclipse_syscall::get_process_list(&mut list) {
                sh_println("  PID  STAT  NOMBRE\n  ---  ----  ------");
                for info in list.iter().take(n) {
                    if info.pid == 0 { continue; }
                    let end = info.name.iter().position(|&b| b == 0).unwrap_or(16);
                    let raw = core::str::from_utf8(&info.name[..end]).unwrap_or("?");
                    let name = raw.rsplit('/').next().unwrap_or(raw);
                    let stat = match info.state { 0|1 => "R", 2 => "S", 3 => "Z", _ => "?" };
                    sh_println(&format!("  {:3}  {:4}  {}", info.pid, stat, name));
                }
            }
            Some(0)
        }
        "exit" => {
            let code = argv.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
            eclipse_syscall::call::exit(code);
        }
        "help" => {
            sh_println("Builtins: echo, printf, env, pwd, cd, ls, cat, ps, exit, help, history...");
            Some(0)
        }
        _ => None,
    }
}

// ============================================================================
// Execution
// ============================================================================

fn wait_for_child(pid: usize) -> i32 {
    let mut status = 0u32;
    loop {
        match eclipse_syscall::call::wait_pid(&mut status, pid) {
            Ok(_) => return ((status >> 8) & 0xFF) as i32,
            Err(e) if e.errno == eclipse_syscall::error::EINTR => continue,
            Err(_) => {
                let mut st = 0u32;
                if let Ok(r) = eclipse_syscall::call::wait_pid_nohang(&mut st, pid) {
                    if r != 0 { return ((st >> 8) & 0xFF) as i32; }
                }
                let _ = eclipse_syscall::call::sched_yield();
            }
        }
    }
}

fn open_redirect_out(path: &str, append: bool) -> Option<usize> {
    let flags = if append {
        eclipse_syscall::flag::O_WRONLY | eclipse_syscall::flag::O_CREAT | eclipse_syscall::flag::O_APPEND
    } else {
        eclipse_syscall::flag::O_WRONLY | eclipse_syscall::flag::O_CREAT | eclipse_syscall::flag::O_TRUNC
    };
    eclipse_syscall::call::open(path, flags).ok()
}

fn spawn_stage(cmd: &SimpleCmd, fd_in: usize, fd_out: usize, fd_err: usize) -> Option<usize> {
    if cmd.argv.is_empty() { return None; }
    let prog = &cmd.argv[0];
    let mut path = format!("/bin/{}", prog);
    let buf = std::fs::read(&path).ok().or_else(|| {
        path = resolve_path(prog);
        std::fs::read(&path).ok()
    });

    if let Some(data) = buf {
        let eff_out = if let Some((ref p, app)) = cmd.redirect_out {
            open_redirect_out(p, app).unwrap_or(fd_out)
        } else { fd_out };

        let res = eclipse_syscall::call::spawn_with_stdio(&data, Some(prog), fd_in, eff_out, fd_err);
        if cmd.redirect_out.is_some() && eff_out != fd_out { let _ = eclipse_syscall::call::close(eff_out); }

        if let Ok(pid) = res {
            let mut ab = Vec::new();
            for a in &cmd.argv { ab.extend_from_slice(a.as_bytes()); ab.push(0); }
            let _ = eclipse_syscall::call::set_child_args(pid, &ab);
            return Some(pid);
        }
    } else { sh_eprintln(&format!("sh: {}: no encontrado", prog)); }
    None
}

fn run_pipeline(pl: &Pipeline, bg_pids: &mut Vec<usize>) -> i32 {
    let n = pl.cmds.len();
    if n == 0 { return 0; }
    if n == 1 {
        if let Some(c) = try_builtin(&pl.cmds[0].argv) { return c; }
        if let Some(pid) = spawn_stage(&pl.cmds[0], 0, 1, 2) {
            if pl.background {
                sh_println(&format!("[bg] {}", pid));
                bg_pids.push(pid);
                return 0;
            }
            let c = wait_for_child(pid); unsafe { LAST_EXIT = c; }
            return c;
        }
        unsafe { LAST_EXIT = CMD_NOT_FOUND; }
        return CMD_NOT_FOUND;
    }

    let mut pids = Vec::new();
    let mut prev_fd = 0;
    for (i, cmd) in pl.cmds.iter().enumerate() {
        let is_last = i == n - 1;
        if is_last {
            if let Some(pid) = spawn_stage(cmd, prev_fd, 1, 2) { pids.push(pid); }
            if prev_fd != 0 { let _ = eclipse_syscall::call::close(prev_fd); }
        } else {
            let mut fds = [0u32; 2];
            if eclipse_syscall::call::pipe(&mut fds).is_err() { break; }
            if let Some(pid) = spawn_stage(cmd, prev_fd, fds[1] as usize, 2) { pids.push(pid); }
            if prev_fd != 0 { let _ = eclipse_syscall::call::close(prev_fd); }
            let _ = eclipse_syscall::call::close(fds[1] as usize);
            prev_fd = fds[0] as usize;
        }
    }
    if pl.background {
        for &p in &pids {
            sh_println(&format!("[bg] {}", p));
            bg_pids.push(p);
        }
        return 0;
    }
    let mut lc = 0;
    for p in pids { lc = wait_for_child(p); }
    unsafe { LAST_EXIT = lc; }
    lc
}

// ============================================================================
// Readline / History / Completion
// ============================================================================

fn complete_at_cursor(input: &mut String) {
    let word_start = input.rfind(' ').map(|i| i + 1).unwrap_or(0);
    let prefix = &input[word_start..];
    if prefix.is_empty() { return; }

    let (dir_path, file_prefix) = if let Some(last_slash) = prefix.rfind('/') {
        (&prefix[..last_slash + 1], &prefix[last_slash + 1..])
    } else { (".", prefix) };

    let abs_dir = resolve_path(dir_path);
    let mut buf = [0u8; 8192];
    if let Ok(n) = eclipse_syscall::call::readdir(&abs_dir, &mut buf) {
        let matches: Vec<&str> = buf[..n].split(|&b| b == b'\n')
            .filter_map(|s| core::str::from_utf8(s).ok())
            .filter(|name| name.starts_with(file_prefix) && !name.is_empty())
            .collect();

        if matches.len() == 1 {
            let matched = matches[0];
            let remaining = &matched[file_prefix.len()..];
            input.push_str(remaining);
            sh_print(remaining);
            // If it's a directory (we don't know for sure but we can guess or just add space)
            // For now, don't add space automatically.
        } else if matches.len() > 1 {
            // Optional: Find common prefix and complete that
        }
    }
}

fn clear_line(curr: &str) { for _ in 0..curr.len() { sh_print("\x08 \x08"); } }

fn readline(hist: &mut History, prompt: &str) -> Option<String> {
    sh_print(prompt);
    let mut input = String::new();
    let mut b1 = [0u8; 1];
    let mut saved: Option<String> = None;

    loop {
        match eclipse_syscall::call::read(0, &mut b1) {
            Ok(0) => return None, // EOF: exit shell
            Ok(1) => match b1[0] {
                b'\n' | b'\r' => { sh_print("\n"); return Some(input); }
                3 => { sh_print("^C\n"); input.clear(); return Some(input); }
                4 => if input.is_empty() { sh_print("\n"); return None; } else { /* ignore EOT in middle of line */ },
                9 => complete_at_cursor(&mut input),
                8 | 127 => if !input.is_empty() { let _ = input.pop(); sh_print("\x08 \x08"); },
                27 => {
                    // ... (rest of ESC handling remains same)
                    let mut avail: usize = 0;
                    let _ = eclipse_syscall::call::ioctl(0, FIONREAD, &mut avail as *mut _ as usize);
                    if avail >= 1 {
                        let mut seq = [0u8; 2];
                        if eclipse_syscall::call::read(0, &mut seq[..1]).is_ok() && seq[0] == b'[' {
                            let mut avail2: usize = 0;
                            let _ = eclipse_syscall::call::ioctl(0, FIONREAD, &mut avail2 as *mut _ as usize);
                            if avail2 >= 1 && eclipse_syscall::call::read(0, &mut seq[..1]).is_ok() {
                                match seq[0] {
                                    b'A' => {
                                        if saved.is_none() { saved = Some(input.clone()); }
                                        if let Some(p) = hist.up() { clear_line(&input); input = p.to_string(); sh_print(&input); }
                                    }
                                    b'B' => {
                                        let n = hist.down().to_string();
                                        let n = if n.is_empty() { saved.take().unwrap_or_default() } else { saved = None; n };
                                        clear_line(&input); input = n; sh_print(&input);
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                }
                b if b >= 0x20 => { input.push(b as char); let _ = eclipse_syscall::call::write(1, &[b]); }
                _ => {}
            },
            Err(e) if e.errno == eclipse_syscall::error::EINTR => update_terminal_size(),
            Err(e) if e.errno == eclipse_syscall::error::EAGAIN => { let _ = eclipse_syscall::call::sched_yield(); }
            _ => return None, // Exit on permanent error
        }
    }
}

// ============================================================================
// Main
// ============================================================================

fn main() {
    sh_println("Eclipse OS v3 Shell (sh)");
    unsafe { CWD = String::from("/"); }
    std::env::set_var("SHELL", "/bin/sh");
    std::env::set_var("USER", "moebius");
    std::env::set_var("PWD", "/");
    update_terminal_size();

    let mut hist = History::new();
    let mut bg_pids = Vec::new();

    loop {
        bg_pids.retain(|&p: &usize| {
            let mut st = 0;
            match eclipse_syscall::call::wait_pid_nohang(&mut st, p) {
                Ok(0) => true,
                Ok(_) => { sh_println(&format!("[bg done] {}", p)); false }
                _ => true,
            }
        });

        let prompt = unsafe {
            if LAST_EXIT != 0 { format!("moebius@eclipse:{} [{}]$ ", CWD.as_str(), LAST_EXIT) }
            else { format!("moebius@eclipse:{}$ ", CWD.as_str()) }
        };

        let line = match readline(&mut hist, &prompt) { Some(l) => l, None => break };
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') { continue; }

        if line == "history" {
            for (i, e) in hist.entries.iter().enumerate() { sh_println(&format!("{:4}  {}", i+1, e)); }
            continue;
        }

        hist.push(line);
        let pl = parse_pipeline(tokenize(line));
        let _ = run_pipeline(&pl, &mut bg_pids);
    }
    eclipse_syscall::call::exit(0);
}
