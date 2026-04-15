use crate::Shell;
use std::prelude::v1::*;
use std::string::String;
use std::vec::Vec;

use eclipse_syscall;

pub fn expand(word: &str, shell: &Shell) -> Vec<String> {
    if !word.contains('*') && !word.contains('?') {
        return Vec::new();
    }

    // Split into directory and pattern
    let (dir_path, pattern) = if let Some(last_slash) = word.rfind('/') {
        (&word[..last_slash + 1], &word[last_slash + 1..])
    } else {
        (".", word)
    };
    
    let base_dir = if dir_path.is_empty() {
        shell.cwd.clone()
    } else {
        let mut abs = String::from(&shell.cwd);
        if !abs.ends_with('/') { abs.push('/'); }
        abs.push_str(dir_path);
        abs
    };

    let mut matches = Vec::new();
    let mut buf = [0u8; 8192];
    
    // readdir returns \n separated filenames
    if let Ok(n) = eclipse_syscall::call::readdir(&base_dir, &mut buf) {
        if let Ok(content) = core::str::from_utf8(&buf[..n]) {
            for entry in content.lines() {
                let entry = entry.trim();
                if entry.is_empty() || entry == "." || entry == ".." {
                    continue;
                }
                
                if matches_glob(pattern, entry) {
                    let mut full = String::from(dir_path);
                    full.push_str(entry);
                    matches.push(full);
                }
            }
        }
    }

    matches.sort();
    matches
}

fn matches_glob(pattern: &str, text: &str) -> bool {
    // Hidden files check: leading . must be explicit in pattern
    if text.starts_with('.') && !pattern.starts_with('.') {
        return false;
    }
    
    fn match_recursive(p: &[u8], t: &[u8]) -> bool {
        if p.is_empty() {
            return t.is_empty();
        }
        
        if p[0] == b'*' {
            // Try skipping * or consuming 1 from t
            return match_recursive(&p[1..], t) || (!t.is_empty() && match_recursive(p, &t[1..]));
        }
        
        if t.is_empty() {
            return false;
        }
        
        if p[0] == b'?' || p[0] == t[0] {
            return match_recursive(&p[1..], &t[1..]);
        }
        
        false
    }
    
    match_recursive(pattern.as_bytes(), text.as_bytes())
}
