#![cfg_attr(target_vendor = "eclipse", no_std)]
#![cfg_attr(not(target_vendor = "eclipse"), no_main)]

#[cfg(target_vendor = "eclipse")]
extern crate eclipse_std as std;

#[cfg(target_vendor = "eclipse")]
use std::prelude::v1::*;

// ============================================================================
// Constantes
// ============================================================================
const TIOCGWINSZ: usize = 4;
const HISTORY_MAX: usize = 100;

// ============================================================================
// Estado global del shell
// ============================================================================

static mut CWD:       &str = "/";
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
    const fn new() -> Self {
        Self { entries: Vec::new(), pos: 0 }
    }

    fn push(&mut self, line: &str) {
        if line.is_empty() { return; }
        // No duplicar línea consecutiva
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

    /// Navegar hacia arriba (pasado). Devuelve la entrada o None.
    fn up(&mut self) -> Option<&str> {
        if self.entries.is_empty() { return None; }
        if self.pos > 0 { self.pos -= 1; }
        self.entries.get(self.pos).map(|s| s.as_str())
    }

    /// Navegar hacia abajo (más reciente / línea vacía).
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
// Parsing de la línea de comandos
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
                current.redirect_out = Some((expand_var(&w), redir_type == 2));
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

/// Lee todo el contenido de un fd abierto como String UTF-8.
fn read_fd_to_string(fd: usize) -> String {
    let mut content = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match eclipse_syscall::call::read(fd, &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => content.extend_from_slice(&buf[..n]),
        }
    }
    String::from_utf8_lossy(&content).into_owned()
}

/// Parsea el flag `-n NUM` o `-NUM` de un slice de argumentos.
/// Devuelve (n_lines, &rest_of_args).
fn parse_n_flag<'a>(args: &'a [String], default: usize) -> (usize, &'a [String]) {
    if args.is_empty() { return (default, args); }
    let first = &args[0];
    // -n NUM
    if first == "-n" {
        let n = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(default);
        return (n, if args.len() > 2 { &args[2..] } else { &[] });
    }
    // -NUM  (e.g. -5)
    if first.starts_with('-') && first[1..].parse::<usize>().is_ok() {
        let n = first[1..].parse().unwrap_or(default);
        return (n, &args[1..]);
    }
    (default, args)
}

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
                        Some('n')  => out.push('\n'),
                        Some('t')  => out.push('\t'),
                        Some('\\') => out.push('\\'),
                        Some(other) => { out.push('\\'); out.push(other); }
                        None => { out.push('\\'); }
                    }
                } else {
                    out.push(c);
                }
            }
            sh_print(&out);
            Some(0)
        }

        "env" | "printenv" => {
            for key in &["TERM", "HOME", "PATH", "LINES", "COLUMNS", "SHELL", "USER", "PWD"] {
                if let Ok(val) = std::env::var(key) {
                    sh_println(&format!("{}={}", key, val));
                }
            }
            sh_println(&format!("?={}", unsafe { LAST_EXIT }));
            Some(0)
        }

        "pwd" => {
            sh_println(unsafe { CWD });
            Some(0)
        }

        "cd" => {
            let dest = argv.get(1).map(|s| s.as_str()).unwrap_or("/");
            unsafe { CWD = if dest == "/" || dest.is_empty() { "/" } else { "/bin" }; }
            std::env::set_var("PWD", unsafe { CWD });
            Some(0)
        }

        "export" => {
            for arg in &argv[1..] {
                if let Some(eq) = arg.find('=') {
                    std::env::set_var(&arg[..eq], &arg[eq+1..]);
                }
            }
            Some(0)
        }

        "unset" => {
            for arg in &argv[1..] { std::env::remove_var(arg); }
            Some(0)
        }

        "ls" => {
            let path = argv.get(1).map(|s| s.as_str()).unwrap_or("/bin");
            let mut buf = [0u8; 4096];
            match eclipse_syscall::call::readdir(path, &mut buf) {
                Ok(n) if n > 0 => {
                    let mut names: Vec<&str> = buf[..n]
                        .split(|&b| b == b'\n')
                        .filter_map(|s| core::str::from_utf8(s).ok())
                        .filter(|s| !s.is_empty())
                        .collect();
                    names.sort_unstable();
                    for name in names { sh_println(name); }
                }
                _ => sh_eprintln(&format!("ls: {}: no se puede listar", path)),
            }
            Some(0)
        }

        "cat" => {
            if argv.len() < 2 { sh_eprintln("Uso: cat <archivo>"); return Some(1); }
            let mut code = 0i32;
            for path in &argv[1..] {
                match eclipse_syscall::call::open(path, 0) {
                    Ok(fd) => {
                        let mut buf = [0u8; 4096];
                        loop {
                            match eclipse_syscall::call::read(fd, &mut buf) {
                                Ok(0) => break,
                                Ok(n) => { let _ = eclipse_syscall::call::write(1, &buf[..n]); }
                                Err(_) => break,
                            }
                        }
                        let _ = eclipse_syscall::call::close(fd);
                    }
                    Err(_) => { sh_eprintln(&format!("cat: {}: no existe", path)); code = 1; }
                }
            }
            Some(code)
        }

        "touch" => {
            if argv.len() < 2 { sh_eprintln("Uso: touch <archivo>"); return Some(1); }
            let mut code = 0i32;
            for path in &argv[1..] {
                let flags = eclipse_syscall::flag::O_WRONLY
                    | eclipse_syscall::flag::O_CREAT;
                match eclipse_syscall::call::open(path, flags) {
                    Ok(fd) => { let _ = eclipse_syscall::call::close(fd); }
                    Err(_) => { sh_eprintln(&format!("touch: {}: error", path)); code = 1; }
                }
            }
            Some(code)
        }

        "rm" => {
            if argv.len() < 2 { sh_eprintln("Uso: rm <archivo> [...]"); return Some(1); }
            let mut code = 0i32;
            for path in &argv[1..] {
                if path.starts_with('-') { continue; } // ignorar flags por ahora
                match eclipse_syscall::call::unlink(path) {
                    Ok(_) => {}
                    Err(e) if e.errno == 38 => {
                        // ENOSYS → solo /tmp soportado
                        sh_eprintln(&format!("rm: {}: solo se pueden borrar archivos en /tmp", path));
                        code = 1;
                    }
                    Err(_) => { sh_eprintln(&format!("rm: {}: no existe", path)); code = 1; }
                }
            }
            Some(code)
        }

        "mkdir" => {
            if argv.len() < 2 { sh_eprintln("Uso: mkdir <dir> [...]"); return Some(1); }
            let mut code = 0i32;
            for path in &argv[1..] {
                match eclipse_syscall::call::mkdir(path, 0o755) {
                    Ok(_) => {}
                    Err(e) if e.errno == 38 => {
                        sh_eprintln(&format!("mkdir: {}: solo se soporta bajo /tmp", path));
                        code = 1;
                    }
                    Err(_) => { sh_eprintln(&format!("mkdir: {}: error", path)); code = 1; }
                }
            }
            Some(code)
        }

        "ps" => {
            let mut list = [eclipse_syscall::ProcessInfo::default(); 48];
            match eclipse_syscall::get_process_list(&mut list) {
                Ok(n) => {
                    sh_println("  PID  STAT  NOMBRE");
                    sh_println("  ---  ----  ------");
                    for info in list.iter().take(n) {
                        if info.pid == 0 { continue; }
                        let name_end = info.name.iter().position(|&b| b == 0).unwrap_or(16);
                        let raw_name = core::str::from_utf8(&info.name[..name_end]).unwrap_or("?");
                        // Mostrar solo el basename (parte después del último '/')
                        let name = raw_name.rsplit('/').next().unwrap_or(raw_name);
                        let name = if name.is_empty() { "<sin nombre>" } else { name };
                        // Estado: 0=Running 1=Blocked 2=Terminated
                        let stat = match info.state {
                            0 => "R",
                            1 => "S",
                            2 => "Z",
                            _ => "?",
                        };
                        sh_println(&format!("  {:3}  {:4}  {}", info.pid, stat, name));
                    }
                }
                Err(_) => sh_eprintln("ps: error obteniendo lista de procesos"),
            }
            Some(0)
        }

        "wc" => {
            // wc [-l] [-w] [-c] [archivo...]  — sin archivo lee stdin (TODO)
            let mut show_lines = false;
            let mut show_words = false;
            let mut show_chars = false;
            let mut files: Vec<&str> = Vec::new();

            for arg in &argv[1..] {
                match arg.as_str() {
                    "-l" => show_lines = true,
                    "-w" => show_words = true,
                    "-c" | "-m" => show_chars = true,
                    _ => files.push(arg.as_str()),
                }
            }
            // Por defecto mostrar todo
            if !show_lines && !show_words && !show_chars {
                show_lines = true; show_words = true; show_chars = true;
            }

            let mut total_l = 0usize; let mut total_w = 0usize; let mut total_c = 0usize;
            let mut code = 0i32;

            for path in &files {
                match eclipse_syscall::call::open(path, 0) {
                    Ok(fd) => {
                        let mut content = Vec::new();
                        let mut buf = [0u8; 4096];
                        loop {
                            match eclipse_syscall::call::read(fd, &mut buf) {
                                Ok(0) => break,
                                Ok(n) => content.extend_from_slice(&buf[..n]),
                                Err(_) => break,
                            }
                        }
                        let _ = eclipse_syscall::call::close(fd);
                        let lines = content.iter().filter(|&&b| b == b'\n').count();
                        let words = core::str::from_utf8(&content).unwrap_or("")
                            .split_whitespace().count();
                        let chars = content.len();
                        total_l += lines; total_w += words; total_c += chars;
                        let mut out = String::new();
                        if show_lines { out.push_str(&format!("{:7} ", lines)); }
                        if show_words { out.push_str(&format!("{:7} ", words)); }
                        if show_chars { out.push_str(&format!("{:7} ", chars)); }
                        out.push_str(path);
                        sh_println(&out);
                    }
                    Err(_) => { sh_eprintln(&format!("wc: {}: no existe", path)); code = 1; }
                }
            }
            if files.len() > 1 {
                let mut out = String::new();
                if show_lines { out.push_str(&format!("{:7} ", total_l)); }
                if show_words { out.push_str(&format!("{:7} ", total_w)); }
                if show_chars { out.push_str(&format!("{:7} ", total_c)); }
                out.push_str("total");
                sh_println(&out);
            }
            Some(code)
        }

        "head" => {
            let (n_lines, paths) = parse_n_flag(&argv[1..], 10);
            let mut code = 0i32;
            for path in paths {
                match eclipse_syscall::call::open(path, 0) {
                    Ok(fd) => {
                        let content = read_fd_to_string(fd);
                        let _ = eclipse_syscall::call::close(fd);
                        for (i, line) in content.lines().enumerate() {
                            if i >= n_lines { break; }
                            sh_println(line);
                        }
                    }
                    Err(_) => { sh_eprintln(&format!("head: {}: no existe", path)); code = 1; }
                }
            }
            Some(code)
        }

        "tail" => {
            let (n_lines, paths) = parse_n_flag(&argv[1..], 10);
            let mut code = 0i32;
            for path in paths {
                match eclipse_syscall::call::open(path, 0) {
                    Ok(fd) => {
                        let content = read_fd_to_string(fd);
                        let _ = eclipse_syscall::call::close(fd);
                        let lines: Vec<&str> = content.lines().collect();
                        let start = if lines.len() > n_lines { lines.len() - n_lines } else { 0 };
                        for line in &lines[start..] { sh_println(line); }
                    }
                    Err(_) => { sh_eprintln(&format!("tail: {}: no existe", path)); code = 1; }
                }
            }
            Some(code)
        }

        "grep" => {
            if argv.len() < 2 { sh_eprintln("Uso: grep <patrón> [archivo...]"); return Some(1); }
            let pattern = &argv[1];
            let mut code = 1i32; // 1 si no encuentra nada
            let paths = &argv[2..];
            let process = |content: &str, label: &str| -> bool {
                let mut found = false;
                for line in content.lines() {
                    if line.contains(pattern.as_str()) {
                        if !label.is_empty() {
                            sh_print(label); sh_print(":");
                        }
                        sh_println(line);
                        found = true;
                    }
                }
                found
            };
            if paths.is_empty() {
                // Leer stdin — no soportado sin pipes; avisamos
                sh_eprintln("grep: leer desde stdin no soportado aún; usa: echo texto | grep pat");
            } else {
                let multi = paths.len() > 1;
                for path in paths {
                    match eclipse_syscall::call::open(path, 0) {
                        Ok(fd) => {
                            let content = read_fd_to_string(fd);
                            let _ = eclipse_syscall::call::close(fd);
                            let label = if multi { path.as_str() } else { "" };
                            if process(&content, label) { code = 0; }
                        }
                        Err(_) => { sh_eprintln(&format!("grep: {}: no existe", path)); }
                    }
                }
            }
            Some(code)
        }

        "cp" => {
            if argv.len() != 3 { sh_eprintln("Uso: cp <origen> <destino>"); return Some(1); }
            let src = &argv[1]; let dst = &argv[2];
            match eclipse_syscall::call::open(src, 0) {
                Ok(fd_r) => {
                    let flags = eclipse_syscall::flag::O_WRONLY
                        | eclipse_syscall::flag::O_CREAT
                        | eclipse_syscall::flag::O_TRUNC;
                    match eclipse_syscall::call::open(dst, flags) {
                        Ok(fd_w) => {
                            let mut buf = [0u8; 4096];
                            let mut ok = true;
                            loop {
                                match eclipse_syscall::call::read(fd_r, &mut buf) {
                                    Ok(0) => break,
                                    Ok(n) => {
                                        if eclipse_syscall::call::write(fd_w, &buf[..n]).is_err() {
                                            sh_eprintln(&format!("cp: error escribiendo {}", dst));
                                            ok = false; break;
                                        }
                                    }
                                    Err(_) => break,
                                }
                            }
                            let _ = eclipse_syscall::call::close(fd_r);
                            let _ = eclipse_syscall::call::close(fd_w);
                            Some(if ok { 0 } else { 1 })
                        }
                        Err(_) => { let _ = eclipse_syscall::call::close(fd_r);
                                    sh_eprintln(&format!("cp: no se puede crear {}", dst)); Some(1) }
                    }
                }
                Err(_) => { sh_eprintln(&format!("cp: {}: no existe", src)); Some(1) }
            }
        }

        "mv" => {
            if argv.len() != 3 { sh_eprintln("Uso: mv <origen> <destino>"); return Some(1); }
            let src = &argv[1]; let dst = &argv[2];
            // Intentar rename primero (solo funciona si ambos están en la misma ubicación)
            if eclipse_syscall::call::rename(src, dst).is_ok() {
                return Some(0);
            }
            // Fallback: cp + rm
            let mut cp_args: Vec<String> = Vec::new();
            cp_args.push(String::from("cp")); cp_args.push(src.clone()); cp_args.push(dst.clone());
            if let Some(0) = try_builtin(&cp_args) {
                let mut rm_args: Vec<String> = Vec::new();
                rm_args.push(String::from("rm")); rm_args.push(src.clone());
                let _ = try_builtin(&rm_args);
                Some(0)
            } else {
                sh_eprintln(&format!("mv: error moviendo {} → {}", src, dst));
                Some(1)
            }
        }

        "seq" => {
            // seq [inicio] fin [paso]
            let (start, end, step) = match argv.len() - 1 {
                1 => (1i64, argv[1].parse().unwrap_or(1), 1i64),
                2 => (argv[1].parse().unwrap_or(1), argv[2].parse().unwrap_or(1), 1i64),
                3 => (argv[1].parse().unwrap_or(1), argv[2].parse().unwrap_or(1),
                      argv[3].parse().unwrap_or(1)),
                _ => { sh_eprintln("Uso: seq [inicio] fin [paso]"); return Some(1); }
            };
            let mut i = start;
            while if step > 0 { i <= end } else { i >= end } {
                sh_println(&format!("{}", i));
                i += step;
            }
            Some(0)
        }

        "true"  => Some(0),
        "false" => Some(1),

        "exit" => {
            let code = argv.get(1).and_then(|s| s.parse::<i32>().ok()).unwrap_or(0);
            eclipse_syscall::call::exit(code);
        }

        "kill" => {
            if argv.len() < 2 { sh_eprintln("Uso: kill [-<sig>] <pid>"); return Some(1); }
            let (sig, pid_arg) = if argv[1].starts_with('-') {
                (argv[1][1..].parse::<usize>().unwrap_or(15), argv.get(2))
            } else {
                (15, argv.get(1))
            };
            if let Some(pid_str) = pid_arg {
                if let Ok(pid) = pid_str.parse::<usize>() {
                    let _ = eclipse_syscall::call::kill(pid, sig);
                }
            }
            Some(0)
        }

        "jobs" => {
            // El estado de los jobs en background lo mostramos desde el contexto global
            // (no disponible aquí — imprimimos aviso)
            sh_println("jobs: use 'ps' para ver procesos del sistema");
            Some(0)
        }

        "history" => {
            // El historial se imprime desde el contexto del shell; aquí delegamos a None
            None // Manejado en main directamente
        }

        "help" => {
            sh_println("Builtins de sh (Eclipse OS):");
            sh_println("  echo [-n] [args...]    Imprimir texto (expande $VAR)");
            sh_println("  printf fmt             Imprimir con secuencias de escape");
            sh_println("  env / printenv         Variables de entorno");
            sh_println("  export VAR=val         Definir variable");
            sh_println("  unset VAR              Eliminar variable");
            sh_println("  pwd                    Directorio actual");
            sh_println("  cd [dir]               Cambiar directorio");
            sh_println("  ls [dir]               Listar directorio (/bin si no se da)");
            sh_println("  cat <arch>             Mostrar contenido de archivo");
            sh_println("  touch <arch>           Crear archivo vacío");
            sh_println("  cp <src> <dst>         Copiar archivo");
            sh_println("  mv <src> <dst>         Mover/renombrar archivo");
            sh_println("  rm <arch> [...]        Eliminar archivo (solo /tmp)");
            sh_println("  mkdir <dir> [...]      Crear directorio (solo /tmp)");
            sh_println("  ps                     Listar procesos (PID STAT NOMBRE)");
            sh_println("  kill [-sig] <pid>      Enviar señal a proceso");
            sh_println("  wc [-l|-w|-c] <arch>   Contar líneas/palabras/bytes");
            sh_println("  head [-n N] <arch>     Primeras N líneas (default 10)");
            sh_println("  tail [-n N] <arch>     Últimas N líneas (default 10)");
            sh_println("  grep <pat> [arch...]   Buscar patrón en archivos");
            sh_println("  seq [ini] fin [paso]   Generar secuencia numérica");
            sh_println("  history                Mostrar historial (Tab completa comandos)");
            sh_println("  exit [code]            Salir");
            sh_println("  help                   Esta ayuda");
            sh_println("");
            sh_println("Sintaxis:");
            sh_println("  cmd1 | cmd2            Pipeline");
            sh_println("  cmd > file             Redirigir stdout (crear/truncar)");
            sh_println("  cmd >> file            Redirigir stdout (añadir)");
            sh_println("  cmd &                  Ejecutar en background");
            sh_println("  $VAR  ${VAR}  $?  $$   Expansión de variables");
            sh_println("  [Tab]                  Completar comando");
            Some(0)
        }

        _ => None,
    }
}

// ============================================================================
// Ejecución externa
// ============================================================================

fn open_redirect_out(path: &str, append: bool) -> Option<usize> {
    let flags = if append {
        eclipse_syscall::flag::O_WRONLY | eclipse_syscall::flag::O_CREAT | 0x0400
    } else {
        eclipse_syscall::flag::O_WRONLY | eclipse_syscall::flag::O_CREAT | eclipse_syscall::flag::O_TRUNC
    };
    eclipse_syscall::call::open(path, flags).ok()
}

fn spawn_stage(cmd: &SimpleCmd, fd_in: usize, fd_out: usize, fd_err: usize) -> Option<usize> {
    if cmd.argv.is_empty() { return None; }
    let program = &cmd.argv[0];
    let bin_path = format!("/bin/{}", program);

    let buf = match std::fs::read(&bin_path) {
        Ok(b) => b,
        Err(_) => {
            sh_eprintln(&format!("sh: {}: comando no encontrado", program));
            unsafe { LAST_EXIT = 127; }
            return None;
        }
    };

    let effective_out = if let Some((ref path, append)) = cmd.redirect_out {
        match open_redirect_out(path, append) {
            Some(fd) => fd,
            None => {
                sh_eprintln(&format!("sh: no se puede abrir '{}'", path));
                unsafe { LAST_EXIT = 1; }
                return None;
            }
        }
    } else {
        fd_out
    };

    let result = eclipse_syscall::call::spawn_with_stdio(&buf, Some(&bin_path), fd_in, effective_out, fd_err);

    // Cerrar FD de redirección que abrimos en el padre
    if cmd.redirect_out.is_some() && effective_out != fd_out {
        let _ = eclipse_syscall::call::close(effective_out);
    }

    match result {
        Ok(pid) => {
            // Pasar argv al hijo ANTES de que el scheduler lo ejecute.
            // Formato: argv[0]\0argv[1]\0...  (NUL-separados)
            if !cmd.argv.is_empty() {
                let mut args_buf: Vec<u8> = Vec::new();
                for arg in &cmd.argv {
                    args_buf.extend_from_slice(arg.as_bytes());
                    args_buf.push(0);
                }
                let _ = eclipse_syscall::call::set_child_args(pid, &args_buf);
            }
            Some(pid)
        }
        Err(_) => {
            sh_eprintln(&format!("sh: error lanzando '{}'", program));
            None
        }
    }
}

fn run_simple(cmd: &SimpleCmd, background: bool) -> i32 {
    // Builtin (no se puede lanzar en background desde aquí, pero lo ejecutamos)
    if let Some(code) = try_builtin(&cmd.argv) {
        return code;
    }
    if let Some(pid) = spawn_stage(cmd, 0, 1, 2) {
        if background {
            sh_println(&format!("[bg] pid={}", pid));
            0
        } else {
            let mut status = 0u32;
            let _ = eclipse_syscall::call::wait_pid(&mut status, pid);
            let code = ((status >> 8) & 0xFF) as i32;
            unsafe { LAST_EXIT = code; }
            code
        }
    } else {
        unsafe { LAST_EXIT }
    }
}

fn run_pipeline(pipeline: &Pipeline) -> i32 {
    let cmds = &pipeline.cmds;
    let n = cmds.len();
    if n == 0 { return 0; }
    if n == 1 { return run_simple(&cmds[0], pipeline.background); }

    let mut pids: Vec<usize> = Vec::new();
    let mut prev_read_fd: usize = 0;

    for (i, cmd) in cmds.iter().enumerate() {
        let is_last = i == n - 1;

        if is_last {
            if let Some(pid) = spawn_stage(cmd, prev_read_fd, 1, 2) {
                pids.push(pid);
            }
            if prev_read_fd != 0 { let _ = eclipse_syscall::call::close(prev_read_fd); }
        } else {
            let mut pipe_fds = [0u32; 2];
            if eclipse_syscall::call::pipe(&mut pipe_fds).is_err() {
                sh_eprintln("sh: error creando pipe");
                break;
            }
            let read_fd  = pipe_fds[0] as usize;
            let write_fd = pipe_fds[1] as usize;

            if let Some(pid) = spawn_stage(cmd, prev_read_fd, write_fd, 2) {
                pids.push(pid);
            }
            if prev_read_fd != 0 { let _ = eclipse_syscall::call::close(prev_read_fd); }
            let _ = eclipse_syscall::call::close(write_fd);
            prev_read_fd = read_fd;
        }
    }

    if pipeline.background {
        sh_println(&format!("[bg] {} procesos lanzados", pids.len()));
        return 0;
    }

    let mut last_status = 0u32;
    for pid in &pids {
        let _ = eclipse_syscall::call::wait_pid(&mut last_status, *pid);
    }
    let code = ((last_status >> 8) & 0xFF) as i32;
    unsafe { LAST_EXIT = code; }
    code
}

// ============================================================================
// Readline con historial y teclas de flecha
// ============================================================================

// ============================================================================
// Tab completion
// ============================================================================

/// Lista de builtins del shell para el completado por Tab.
const SHELL_BUILTINS: &[&str] = &[
    "echo", "printf", "env", "printenv", "pwd", "cd", "ls", "cat",
    "touch", "rm", "mkdir", "cp", "mv", "ps", "kill", "export", "unset",
    "history", "help", "exit", "true", "false", "jobs",
    "wc", "head", "tail", "grep", "seq",
];

/// Completa el comando o ruta actual en `input`.
/// Modifica `input` in-place y re-dibuja en la terminal.
fn tab_complete(input: &mut String) {
    // Obtener el token actual (última palabra, o línea vacía = primer token)
    let (prefix_before, word) = if let Some(sp) = input.rfind(' ') {
        (&input[..sp + 1], &input[sp + 1..])
    } else {
        ("", input.as_str())
    };
    let is_cmd = prefix_before.is_empty();

    // Recoger candidatos
    let mut candidates: Vec<String> = Vec::new();

    if is_cmd {
        // Completar comandos: builtins + binarios en /bin
        for b in SHELL_BUILTINS {
            if b.starts_with(word) {
                candidates.push(String::from(*b));
            }
        }
        let mut buf = [0u8; 4096];
        if let Ok(n) = eclipse_syscall::call::readdir("/bin", &mut buf) {
            for name in buf[..n].split(|&b| b == b'\n')
                .filter_map(|s| core::str::from_utf8(s).ok())
                .filter(|s| !s.is_empty() && s.starts_with(word))
            {
                // Evitar duplicados con builtins
                if !candidates.iter().any(|c| c.as_str() == name) {
                    candidates.push(String::from(name));
                }
            }
        }
    } else {
        // Completar rutas: buscar en la misma carpeta del prefijo del word
        let (dir, file_prefix) = if let Some(sl) = word.rfind('/') {
            (&word[..sl + 1], &word[sl + 1..])
        } else {
            ("/bin/", word) // por defecto buscar en /bin si no hay ruta
        };
        let mut buf = [0u8; 4096];
        if let Ok(n) = eclipse_syscall::call::readdir(dir, &mut buf) {
            for name in buf[..n].split(|&b| b == b'\n')
                .filter_map(|s| core::str::from_utf8(s).ok())
                .filter(|s| !s.is_empty() && s.starts_with(file_prefix))
            {
                candidates.push(format!("{}{}", dir, name));
            }
        }
    }

    candidates.sort_unstable();

    match candidates.len() {
        0 => {
            // Sin candidatos: bip (BEL)
            sh_print("\x07");
        }
        1 => {
            // Único candidato: completar
            let suffix = &candidates[0][word.len()..];
            sh_print(suffix);
            let new_word = candidates[0].clone();
            *input = format!("{}{} ", prefix_before, new_word);
            // Ya imprimimos suffix; añadir espacio
            sh_print(" ");
        }
        _ => {
            // Varios candidatos: mostrar en columnas debajo del prompt y redibujar
            sh_print("\n");
            for (i, c) in candidates.iter().enumerate() {
                sh_print(c);
                if (i + 1) % 4 == 0 { sh_print("\n"); } else { sh_print("  "); }
            }
            if candidates.len() % 4 != 0 { sh_print("\n"); }
            // Completar el prefijo común
            let common = common_prefix(&candidates);
            if common.len() > word.len() {
                let suffix = &common[word.len()..];
                sh_print(suffix);
                *input = format!("{}{}", prefix_before, common);
            }
            // Re-dibujar prompt + input actual (sin el prompt — ya fue impreso antes)
            // Solo reimprimimos el input
            sh_print(input);
        }
    }
}

/// Calcula el prefijo común de un conjunto de cadenas.
fn common_prefix(strings: &[String]) -> String {
    if strings.is_empty() { return String::new(); }
    let first = &strings[0];
    let mut len = first.len();
    for s in &strings[1..] {
        len = len.min(
            first.chars().zip(s.chars())
                .take_while(|(a, b)| a == b)
                .count()
        );
    }
    String::from(&first[..len])
}

/// Lee una línea de stdin con soporte de edición básica:
/// - Backspace/DEL para borrar
/// - Flechas arriba/abajo para navegar el historial
/// - Ctrl+C para cancelar la línea actual
fn readline(history: &mut History, prompt: &str) -> Option<String> {
    sh_print(prompt);

    let mut input = String::new();
    let mut buf1 = [0u8; 1];
    // Guardamos la línea "en curso" cuando navegamos hacia arriba
    let mut saved_input: Option<String> = None;

    loop {
        match eclipse_syscall::call::read(0, &mut buf1) {
            Ok(1) => {
                let c = buf1[0];
                match c {
                    // Enter
                    b'\n' | b'\r' => {
                        sh_print("\n");
                        return Some(input);
                    }
                    // Ctrl+C
                    3 => {
                        sh_print("^C\n");
                        return Some(String::new());
                    }
                    // Ctrl+D (EOF)
                    4 => {
                        if input.is_empty() {
                            sh_print("\n");
                            return None; // señal de salida
                        }
                    }
                    // Backspace / DEL
                    8 | 127 => {
                        if !input.is_empty() {
                            let _ = input.pop();
                            sh_print("\x08 \x08");
                        }
                    }
                    // Tab — completado de comandos
                    9 => {
                        tab_complete(&mut input);
                    }
                    // ESC — inicio de secuencia de escape
                    27 => {
                        // Leer el siguiente byte (debería ser '[')
                        let mut seq = [0u8; 2];
                        if eclipse_syscall::call::read(0, &mut seq[..1]).is_ok()
                            && seq[0] == b'['
                            && eclipse_syscall::call::read(0, &mut seq[..1]).is_ok()
                        {
                            match seq[0] {
                                // Flecha arriba → entrada anterior del historial
                                b'A' => {
                                    if saved_input.is_none() {
                                        saved_input = Some(input.clone());
                                    }
                                    if let Some(prev) = history.up() {
                                        let prev = String::from(prev);
                                        // Borrar línea actual en la terminal
                                        clear_line(&input);
                                        input = prev;
                                        sh_print(&input);
                                    }
                                }
                                // Flecha abajo → entrada más reciente / vacía
                                b'B' => {
                                    let next = String::from(history.down());
                                    // Si volvemos al final, restaurar la línea guardada
                                    let next = if next.is_empty() {
                                        saved_input.take().unwrap_or_default()
                                    } else {
                                        saved_input = None;
                                        next
                                    };
                                    clear_line(&input);
                                    input = next;
                                    sh_print(&input);
                                }
                                // Flecha izquierda/derecha — ignorar por ahora
                                b'C' | b'D' => {}
                                // Ignorar otras secuencias
                                _ => {}
                            }
                        }
                    }
                    // Carácter normal imprimible
                    b if b >= 0x20 => {
                        input.push(c as char);
                        let _ = eclipse_syscall::call::write(1, &[c]);
                    }
                    _ => {}
                }
            }
            Err(e) if e.errno == eclipse_syscall::error::EINTR => {
                update_terminal_size();
            }
            _ => {
                let _ = eclipse_syscall::call::sched_yield();
            }
        }
    }
}

/// Borra `n` caracteres en la terminal y mueve el cursor al principio de ellos.
fn clear_line(current: &str) {
    // Retroceder `current.len()` posiciones, sobreescribir con espacios y volver
    for _ in 0..current.len() { sh_print("\x08"); }
    for _ in 0..current.len() { sh_print(" "); }
    for _ in 0..current.len() { sh_print("\x08"); }
}

// ============================================================================
// Bucle principal
// ============================================================================

#[cfg(target_vendor = "eclipse")]
fn main() {
    println!("Eclipse OS v3 Shell (sh)");

    std::env::set_var("SHELL", "/bin/sh");
    std::env::set_var("USER",  "moebius");
    std::env::set_var("PWD",   "/");
    update_terminal_size();

    let mut history = History::new();
    // PIDs de procesos en background (para recogerlos con WNOHANG periódicamente)
    let mut bg_pids: Vec<usize> = Vec::new();

    loop {
        // Cosechar procesos en background que hayan terminado
        bg_pids.retain(|&pid| {
            let mut status = 0u32;
            match eclipse_syscall::call::wait_pid_nohang(&mut status, pid) {
                Ok(0) => true,  // todavía en ejecución
                Ok(_) => {
                    let code = ((status >> 8) & 0xFF) as i32;
                    sh_println(&format!("[bg done] pid={} exit={}", pid, code));
                    false
                }
                Err(_) => false, // ya no existe
            }
        });

        // Construir el prompt: rojo si el último comando falló
        let last_exit = unsafe { LAST_EXIT };
        let prompt = if last_exit != 0 {
            // Prompt con código de error en rojo: "moebius@eclipse:/[42]$ "
            format!("\x1b[31mmoebius@eclipse:{} [{}]\x1b[0m$ ", unsafe { CWD }, last_exit)
        } else {
            format!("moebius@eclipse:{}$ ", unsafe { CWD })
        };

        // Leer línea con historial
        let line = match readline(&mut history, &prompt) {
            Some(l) => l,
            None => break, // Ctrl+D
        };

        let line = line.trim().to_string();
        if line.is_empty() { continue; }
        if line.starts_with('#') { continue; }

        // Builtin especial "history" aquí para tener acceso al vector
        if line == "history" {
            for (i, entry) in history.entries.iter().enumerate() {
                sh_println(&format!("{:4}  {}", i + 1, entry));
            }
            continue;
        }

        // Guardar en historial
        history.push(&line);

        // Tokenizar, parsear y ejecutar
        let tokens = tokenize(&line);
        if tokens.is_empty() { continue; }
        let pipeline = parse_pipeline(tokens);

        let background = pipeline.background;
        // En background: lanzar y guardar PID en bg_pids
        if background && pipeline.cmds.len() == 1 {
            let cmd = &pipeline.cmds[0];
            // Builtin en background no tiene sentido — ejecutar directo
            if try_builtin(&cmd.argv).is_some() {
                let _ = run_pipeline(&pipeline);
            } else if let Some(pid) = spawn_stage(cmd, 0, 1, 2) {
                sh_println(&format!("[bg] pid={}", pid));
                bg_pids.push(pid);
            }
        } else {
            let _ = run_pipeline(&pipeline);
        }
    }

    sh_println("logout");
    eclipse_syscall::call::exit(0);
}

#[cfg(not(target_vendor = "eclipse"))]
fn main() {
    println!("Solo soportado en Eclipse OS");
}
