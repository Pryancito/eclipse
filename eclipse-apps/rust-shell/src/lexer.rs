use std::string::String;
use std::vec::Vec;
use std::iter::Peekable;
use std::str::Chars;

#[derive(Debug, PartialEq, Clone)]
pub enum Token {
    Word(String),
    Pipe,           // |
    Greater,        // >
    DoubleGreater,  // >>
    Less,           // <
    Ampersand,      // &
    Semi,           // ;
    AndIf,          // &&
    OrIf,           // ||
    LParen,         // (
    RParen,         // )
    LBrace,         // {
    RBrace,         // }
}

pub struct Lexer<'a> {
    input: Peekable<Chars<'a>>,
}

impl<'a> Lexer<'a> {
    pub fn new(input: &'a str) -> Self {
        Self { input: input.chars().peekable() }
    }

    pub fn tokenize(&mut self) -> Vec<Token> {
        let mut tokens = Vec::new();
        while let Some(tok) = self.next_token() {
            tokens.push(tok);
        }
        tokens
    }

    fn next_token(&mut self) -> Option<Token> {
        self.skip_whitespace();
        let c = self.input.next()?;

        match c {
            '|' => {
                if self.input.peek() == Some(&'|') {
                    let _ = self.input.next();
                    Some(Token::OrIf)
                } else {
                    Some(Token::Pipe)
                }
            }
            '>' => {
                if self.input.peek() == Some(&'>') {
                    let _ = self.input.next();
                    Some(Token::DoubleGreater)
                } else {
                    Some(Token::Greater)
                }
            }
            '<' => Some(Token::Less),
            '&' => {
                if self.input.peek() == Some(&'&') {
                    let _ = self.input.next();
                    Some(Token::AndIf)
                } else {
                    Some(Token::Ampersand)
                }
            }
            ';' => Some(Token::Semi),
            '(' => Some(Token::LParen),
            ')' => Some(Token::RParen),
            '{' => Some(Token::LBrace),
            '}' => Some(Token::RBrace),
            _ => {
                // Word handling
                let mut word = String::from(c);
                while let Some(&nc) = self.input.peek() {
                    if nc.is_whitespace() || "|><&;(){} ".contains(nc) {
                        break;
                    }
                    word.push(self.input.next().unwrap());
                }
                Some(Token::Word(word))
            }
        }
    }

    fn skip_whitespace(&mut self) {
        while let Some(&c) = self.input.peek() {
            if !c.is_whitespace() { break; }
            let _ = self.input.next();
        }
    }
}
