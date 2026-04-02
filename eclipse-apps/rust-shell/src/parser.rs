use crate::lexer::Token;
use std::vec::Vec;
use std::string::String;
use std::boxed::Box;
use crate::Shell;

#[derive(Debug, PartialEq, Clone)]
pub enum Redirection {
    Output(usize, String), // fd, file
    Append(usize, String), // fd, file
    Input(usize, String),  // fd, file
}

#[derive(Debug, PartialEq, Clone)]
pub struct SimpleCommand {
    pub argv: Vec<String>,
    pub redirects: Vec<Redirection>,
}

impl SimpleCommand {
    pub fn expand_vars(&mut self, shell: &mut Shell) {
        // Phase 1: Variable and command substitution
        for arg in &mut self.argv {
            *arg = expand_word(arg, shell);
        }
        
        // Phase 2: Globbing
        let mut new_argv = Vec::new();
        for word in &self.argv {
            let expanded = crate::glob::expand(word, shell);
            if expanded.is_empty() {
                new_argv.push(word.clone());
            } else {
                new_argv.extend(expanded);
            }
        }
        self.argv = new_argv;
    }
}

#[derive(Debug, PartialEq, Clone)]
pub struct Pipeline {
    pub commands: Vec<SimpleCommand>,
    pub ampersand: bool,
}

impl Pipeline {
    pub fn expand_vars(&mut self, shell: &mut Shell) {
        for cmd in &mut self.commands {
            cmd.expand_vars(shell);
        }
    }
}

#[derive(Debug, PartialEq, Clone)]
pub enum LogicalOp {
    And,
    Or,
}

#[derive(Debug, PartialEq, Clone)]
pub struct AndOrList {
    pub head: Pipeline,
    pub tail: Vec<(LogicalOp, Pipeline)>,
}

#[derive(Debug, PartialEq, Clone)]
pub enum Statement {
    AndOrList(AndOrList),
    If {
        condition: Box<Statement>,
        then_block: Vec<Statement>,
        else_block: Option<Vec<Statement>>,
    },
    While {
        condition: Box<Statement>,
        body: Vec<Statement>,
    },
    For {
        var: String,
        list: Vec<String>,
        body: Vec<Statement>,
    },
    Block(Vec<Statement>),
    FunctionDef {
        name: String,
        body: Vec<Statement>,
    },
}

impl Statement {
    pub fn expand_vars(&mut self, shell: &mut Shell) {
        match self {
            Statement::AndOrList(list) => {
                list.head.expand_vars(shell);
                for (_, p) in &mut list.tail {
                    p.expand_vars(shell);
                }
            }
            Statement::If { condition, then_block, else_block } => {
                condition.expand_vars(shell);
                for s in then_block { s.expand_vars(shell); }
                if let Some(eb) = else_block {
                    for s in eb { s.expand_vars(shell); }
                }
            }
            Statement::While { condition, body } => {
                condition.expand_vars(shell);
                for s in body { s.expand_vars(shell); }
            }
            Statement::For { var: _, list, body } => {
                for item in list {
                    *item = expand_word(item, shell);
                }
                for s in body { s.expand_vars(shell); }
            }
            Statement::Block(stmts) => {
                for s in stmts { s.expand_vars(shell); }
            }
            Statement::FunctionDef { name: _, body: _ } => {}
        }
    }
}

pub fn expand_word(word: &str, shell: &mut Shell) -> String {
    let mut result = String::new();
    let mut i = 0;
    let bytes = word.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'$' {
            i += 1;
            let start = i;
            if i < bytes.len() && bytes[i] == b'(' {
                // Command substitution $(...)
                i += 1;
                let sub_start = i;
                let mut depth = 1;
                while i < bytes.len() && depth > 0 {
                    if bytes[i] == b'(' { depth += 1; }
                    else if bytes[i] == b')' { depth -= 1; }
                    if depth > 0 { i += 1; }
                }
                let cmd_str = core::str::from_utf8(&bytes[sub_start..i]).unwrap_or("");
                let output = crate::interp::capture_output(cmd_str, shell);
                result.push_str(output.trim_end());
                if i < bytes.len() && bytes[i] == b')' { i += 1; }
            } else if i < bytes.len() && bytes[i] == b'{' {
                // Bracketed expansion ${VAR}
                i += 1;
                let sub_start = i;
                while i < bytes.len() && bytes[i] != b'}' { i += 1; }
                let name = &word[sub_start..i];
                if let Ok(val) = std::env::var(name) {
                    result.push_str(&val);
                }
                if i < bytes.len() && bytes[i] == b'}' { i += 1; }
            } else {
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                if start == i {
                    if i < bytes.len() && bytes[i] == b'?' {
                        result.push_str(&::alloc::format!("{}", shell.last_status));
                        i += 1;
                    } else if i < bytes.len() && bytes[i] == b'$' {
                        #[cfg(target_vendor = "eclipse")]
                        result.push_str(&::alloc::format!("{}", eclipse_syscall::call::getpid()));
                        #[cfg(not(target_vendor = "eclipse"))]
                        result.push_str("1234"); // Dummy for testing
                        i += 1;
                    } else {
                        result.push('$');
                    }
                } else {
                    let name = &word[start..i];
                    if let Ok(idx) = name.parse::<usize>() {
                        if idx > 0 && idx <= shell.args.len() {
                            result.push_str(&shell.args[idx-1]);
                        }
                    } else if let Ok(val) = std::env::var(name) {
                        result.push_str(&val);
                    }
                }
            }
        } else {
            result.push(bytes[i] as char);
            i += 1;
        }
    }
    result
}

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    pub fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0 }
    }

    pub fn parse_statement(&mut self) -> Option<Statement> {
        let tok = self.peek().cloned()?;
        match tok {
            Token::Word(w) if w == "if" => self.parse_if(),
            Token::Word(w) if w == "while" => self.parse_while(),
            Token::Word(w) if w == "for" => self.parse_for(),
            Token::LBrace => self.parse_block().map(Statement::Block),
            Token::Word(name) => {
                if self.peek_at(1) == Some(&Token::LParen) && self.peek_at(2) == Some(&Token::RParen) {
                    let name = name.clone();
                    let _ = self.consume(); // name
                    let _ = self.consume(); // (
                    let _ = self.consume(); // )
                    if let Some(Token::LBrace) = self.peek() {
                         if let Some(body) = self.parse_block() {
                             return Some(Statement::FunctionDef { name, body });
                         }
                    }
                    None
                } else {
                    self.parse_and_or_list().map(Statement::AndOrList)
                }
            }
            _ => self.parse_and_or_list().map(Statement::AndOrList),
        }
    }

    fn parse_and_or_list(&mut self) -> Option<AndOrList> {
        let head = self.parse_pipeline()?;
        let mut tail = Vec::new();
        
        while let Some(tok) = self.peek().cloned() {
            match tok {
                Token::AndIf => {
                    let _ = self.consume();
                    if let Some(next) = self.parse_pipeline() {
                        tail.push((LogicalOp::And, next));
                    } else {
                        break;
                    }
                }
                Token::OrIf => {
                    let _ = self.consume();
                    if let Some(next) = self.parse_pipeline() {
                        tail.push((LogicalOp::Or, next));
                    } else {
                        break;
                    }
                }
                _ => break,
            }
        }
        
        Some(AndOrList { head, tail })
    }

    pub fn parse_pipeline(&mut self) -> Option<Pipeline> {
        let mut commands = Vec::new();
        let mut ampersand = false;

        while self.pos < self.tokens.len() {
            if let Some(cmd) = self.parse_simple_command() {
                commands.push(cmd);
            } else {
                break;
            }

            match self.peek() {
                Some(Token::Pipe) => { let _ = self.consume(); }
                Some(Token::Ampersand) => {
                    let _ = self.consume();
                    ampersand = true;
                    break;
                }
                Some(Token::Semi) => {
                    let _ = self.consume();
                    break;
                }
                _ => break,
            }
        }

        if commands.is_empty() { None } else { Some(Pipeline { commands, ampersand }) }
    }

    fn parse_simple_command(&mut self) -> Option<SimpleCommand> {
        let mut argv = Vec::new();
        let mut redirects = Vec::new();

        while let Some(tok) = self.peek().cloned() {
            match tok {
                Token::Word(w) => {
                    if ["if", "then", "else", "fi", "while", "do", "done", "for", "in"].contains(&w.as_str()) {
                        if argv.is_empty() { return None; }
                        break;
                    }
                    
                    // Check if it's a numeric FD prefix for redirection (e.g. 2>)
                    if let Some(tok_next) = self.peek_at(1) {
                        match tok_next {
                            Token::Greater | Token::DoubleGreater => {
                                if let Ok(fd) = w.parse::<usize>() {
                                    let _ = self.consume(); // Consume FD
                                    let is_append = if let Some(Token::DoubleGreater) = self.consume() { true } else { false };
                                    if let Some(Token::Word(file)) = self.consume() {
                                        if is_append {
                                            redirects.push(Redirection::Append(fd, file));
                                        } else {
                                            redirects.push(Redirection::Output(fd, file));
                                        }
                                        continue;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    
                    argv.push(w.clone());
                    let _ = self.consume();
                }
                Token::Greater => {
                    let _ = self.consume();
                    if let Some(Token::Word(file)) = self.consume() {
                        redirects.push(Redirection::Output(1, file));
                    }
                }
                Token::DoubleGreater => {
                    let _ = self.consume();
                    if let Some(Token::Word(file)) = self.consume() {
                        redirects.push(Redirection::Append(1, file));
                    }
                }
                Token::Less => {
                    let _ = self.consume();
                    if let Some(Token::Word(file)) = self.consume() {
                        redirects.push(Redirection::Input(0, file));
                    }
                }
                _ => break,
            }
        }

        if argv.is_empty() && redirects.is_empty() { None } else { Some(SimpleCommand { argv, redirects }) }
    }

    fn parse_if(&mut self) -> Option<Statement> {
        let _ = self.consume(); // "if"
        let condition = Box::new(self.parse_statement()?);
        if let Some(Token::Word(w)) = self.peek() { if w == "then" { let _ = self.consume(); } }
        
        let mut then_block = Vec::new();
        while let Some(tok) = self.peek() {
            if let Token::Word(w) = tok { if w == "else" || w == "fi" { break; } }
            if let Some(s) = self.parse_statement() { then_block.push(s); } else { break; }
        }
        
        let mut else_block = None;
        if let Some(Token::Word(w)) = self.peek() {
            if w == "else" {
                let _ = self.consume();
                let mut eb = Vec::new();
                while let Some(tok) = self.peek() {
                    if let Token::Word(w) = tok { if w == "fi" { break; } }
                    if let Some(s) = self.parse_statement() { eb.push(s); } else { break; }
                }
                else_block = Some(eb);
            }
        }
        
        if let Some(Token::Word(w)) = self.peek() { if w == "fi" { let _ = self.consume(); } }
        Some(Statement::If { condition, then_block, else_block })
    }

    fn parse_while(&mut self) -> Option<Statement> {
        let _ = self.consume(); // "while"
        let condition = Box::new(self.parse_statement()?);
        if let Some(Token::Word(w)) = self.peek() { if w == "do" { let _ = self.consume(); } }
        
        let mut body = Vec::new();
        while let Some(tok) = self.peek() {
            if let Token::Word(w) = tok { if w == "done" { break; } }
            if let Some(s) = self.parse_statement() { body.push(s); } else { break; }
        }
        
        if let Some(Token::Word(w)) = self.peek() { if w == "done" { let _ = self.consume(); } }
        Some(Statement::While { condition, body })
    }

    fn parse_for(&mut self) -> Option<Statement> {
        let _ = self.consume(); // "for"
        let var = if let Some(Token::Word(v)) = self.consume() { v } else { return None; };
        if let Some(Token::Word(w)) = self.peek() { if w == "in" { let _ = self.consume(); } }
        
        let mut list = Vec::new();
        while let Some(Token::Word(w)) = self.peek() {
            if w == "do" { break; }
            list.push(w.clone());
            let _ = self.consume();
        }
        
        if let Some(Token::Word(w)) = self.peek() { if w == "do" { let _ = self.consume(); } }
        let mut body = Vec::new();
        while let Some(tok) = self.peek() {
            if let Token::Word(w) = tok { if w == "done" { break; } }
            if let Some(s) = self.parse_statement() { body.push(s); } else { break; }
        }
        
        if let Some(Token::Word(w)) = self.peek() { if w == "done" { let _ = self.consume(); } }
        Some(Statement::For { var, list, body })
    }

    fn parse_block(&mut self) -> Option<Vec<Statement>> {
        let _ = self.consume(); // {
        let mut statements = Vec::new();
        while let Some(tok) = self.peek().cloned() {
            if tok == Token::RBrace { break; }
            if let Some(s) = self.parse_statement() {
                statements.push(s);
            } else {
                break;
            }
        }
        let _ = self.consume(); // }
        Some(statements)
    }

    fn peek(&self) -> Option<&Token> { self.tokens.get(self.pos) }
    fn peek_at(&self, offset: usize) -> Option<&Token> { self.tokens.get(self.pos + offset) }
    fn consume(&mut self) -> Option<Token> {
        if self.pos < self.tokens.len() {
            let tok = self.tokens[self.pos].clone();
            self.pos += 1;
            Some(tok)
        } else {
            None
        }
    }
}
