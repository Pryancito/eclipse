//! The arithmetic lunarrun answers while you type: `2+2`, `sqrt(2)`,
//! `2^10`, `(3,5 * 4) / 7`.
//!
//! A small recursive-descent evaluator over `f64`, nothing more: `+ - * / %`
//! (`×`, `x` between numbers, `÷` too), `^` (right-associative), parentheses,
//! decimals with a point or a comma, the constants `pi` and `e`, and `sqrt`,
//! `abs`, `ln`, `log`, `sin`, `cos`, `tan`, `round`, `floor`, `ceil`.
//!
//! [`answer`] only answers what is plainly a sum: a bare number, a bare word
//! or an application name is not, so typing `e` still finds Emacs.

/// The value of `s`, or `None` when it is not an expression this reads or its
/// value is not a finite number.
pub fn eval(s: &str) -> Option<f64> {
    let toks = lex(s)?;
    let mut p = Parser { t: &toks, i: 0, depth: 0 };
    let v = p.expr()?;
    (p.i == toks.len() && v.is_finite()).then_some(v)
}

/// What lunarrun shows for `s`: its value, formatted, when `s` is a sum
/// (it has an operator or a function, not only a number or a word).
pub fn answer(s: &str, comma: bool) -> Option<String> {
    let s = s.trim();
    let toks = lex(s)?;
    let sum = toks.iter().any(|t| matches!(t, Tok::Op(_) | Tok::Open))
        && toks.iter().any(|t| matches!(t, Tok::Num(_) | Tok::Word(_)));
    // `-5` alone is a number, not a sum.
    let lone_sign = toks.len() == 2 && matches!(toks[0], Tok::Op('-' | '+'));
    if !sum || lone_sign {
        return None;
    }
    eval(s).map(|v| format(v, comma))
}

/// `v` as a person writes it: an integer without a fraction, otherwise at most
/// ten significant decimals with trailing zeros gone; a comma for the point
/// when `comma`.
pub fn format(v: f64, comma: bool) -> String {
    let v = if v == 0.0 { 0.0 } else { v }; // no "-0"
    let s = if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{v:.0}")
    } else if v.abs() >= 1e15 || v.abs() < 1e-6 {
        format!("{v:e}")
    } else {
        let digits = (10 - v.abs().log10().floor() as i32 - 1).clamp(0, 12) as usize;
        let s = format!("{v:.digits$}");
        let s = s.trim_end_matches('0').trim_end_matches('.');
        s.to_string()
    };
    if comma {
        s.replace('.', ",")
    } else {
        s
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Num(f64),
    Word(String),
    Op(char),
    Open,
    Close,
}

fn lex(s: &str) -> Option<Vec<Tok>> {
    let c: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        let ch = c[i];
        if ch.is_whitespace() {
            i += 1;
        } else if ch.is_ascii_digit() || ((ch == '.' || ch == ',') && c.get(i + 1).is_some_and(|d| d.is_ascii_digit())) {
            let start = i;
            let mut point = false;
            while i < c.len() && (c[i].is_ascii_digit() || (!point && (c[i] == '.' || c[i] == ','))) {
                point |= c[i] == '.' || c[i] == ',';
                i += 1;
            }
            let n: String = c[start..i].iter().map(|&d| if d == ',' { '.' } else { d }).collect();
            out.push(Tok::Num(n.parse().ok()?));
        } else if ch.is_alphabetic() {
            let start = i;
            while i < c.len() && c[i].is_alphanumeric() {
                i += 1;
            }
            let w: String = c[start..i].iter().collect::<String>().to_lowercase();
            // `x` between two numbers is a times sign.
            if w == "x" && matches!(out.last(), Some(Tok::Num(_) | Tok::Close)) {
                out.push(Tok::Op('*'));
            } else {
                out.push(Tok::Word(w));
            }
        } else {
            out.push(match ch {
                '+' | '-' | '*' | '/' | '%' | '^' => Tok::Op(ch),
                '×' | '·' => Tok::Op('*'),
                '÷' => Tok::Op('/'),
                '−' => Tok::Op('-'),
                '(' => Tok::Open,
                ')' => Tok::Close,
                _ => return None,
            });
            i += 1;
        }
    }
    (!out.is_empty()).then_some(out)
}

struct Parser<'a> {
    t: &'a [Tok],
    i: usize,
    /// Nesting, bounded: `((((...` typed by a cat must not blow the stack.
    depth: u32,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.t.get(self.i)
    }

    fn op(&mut self, set: &[char]) -> Option<char> {
        match self.peek() {
            Some(Tok::Op(o)) if set.contains(o) => {
                let o = *o;
                self.i += 1;
                Some(o)
            }
            _ => None,
        }
    }

    fn expr(&mut self) -> Option<f64> {
        let mut v = self.term()?;
        while let Some(o) = self.op(&['+', '-']) {
            let r = self.term()?;
            v = if o == '+' { v + r } else { v - r };
        }
        Some(v)
    }

    fn term(&mut self) -> Option<f64> {
        let mut v = self.unary()?;
        while let Some(o) = self.op(&['*', '/', '%']) {
            let r = self.unary()?;
            v = match o {
                '*' => v * r,
                '/' => v / r,
                _ => v % r,
            };
        }
        Some(v)
    }

    /// `^` binds tighter than a sign on its left (`-2^2` is -4) and takes
    /// one on its right (`2^-1`), and is right-associative.
    fn power(&mut self) -> Option<f64> {
        let b = self.primary()?;
        if self.op(&['^']).is_some() {
            self.depth += 1;
            if self.depth > 64 {
                return None;
            }
            let e = self.unary()?;
            self.depth -= 1;
            return Some(b.powf(e));
        }
        Some(b)
    }

    fn unary(&mut self) -> Option<f64> {
        match self.op(&['-', '+']) {
            Some(o) => {
                self.depth += 1;
                if self.depth > 64 {
                    return None;
                }
                let v = self.unary()?;
                self.depth -= 1;
                Some(if o == '-' { -v } else { v })
            }
            None => self.power(),
        }
    }

    fn group(&mut self) -> Option<f64> {
        // The opening parenthesis is already taken.
        self.depth += 1;
        if self.depth > 64 {
            return None;
        }
        let v = self.expr()?;
        self.depth -= 1;
        match self.peek() {
            Some(Tok::Close) => {
                self.i += 1;
                Some(v)
            }
            _ => None,
        }
    }

    fn primary(&mut self) -> Option<f64> {
        match self.peek()?.clone() {
            Tok::Num(n) => {
                self.i += 1;
                Some(n)
            }
            Tok::Open => {
                self.i += 1;
                self.group()
            }
            Tok::Word(w) => {
                self.i += 1;
                match w.as_str() {
                    "pi" | "π" => return Some(std::f64::consts::PI),
                    "e" => return Some(std::f64::consts::E),
                    _ => {}
                }
                if self.peek() != Some(&Tok::Open) {
                    return None;
                }
                self.i += 1;
                let a = self.group()?;
                Some(match w.as_str() {
                    "sqrt" | "raiz" => a.sqrt(),
                    "abs" => a.abs(),
                    "ln" => a.ln(),
                    "log" => a.log10(),
                    "sin" | "sen" => a.sin(),
                    "cos" => a.cos(),
                    "tan" => a.tan(),
                    "round" => a.round(),
                    "floor" => a.floor(),
                    "ceil" => a.ceil(),
                    _ => return None,
                })
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_does_the_sums_the_way_school_does() {
        assert_eq!(eval("2+2"), Some(4.0));
        assert_eq!(eval("2+3*4"), Some(14.0));
        assert_eq!(eval("(2+3)*4"), Some(20.0));
        assert_eq!(eval("2^3^2"), Some(512.0), "power is right-associative");
        assert_eq!(eval("-2^2"), Some(-4.0), "the sign applies to the power");
        assert_eq!(eval("2^-1"), Some(0.5));
        assert_eq!(eval("10 % 3"), Some(1.0));
        assert_eq!(eval("3,5 * 2"), Some(7.0), "a decimal comma");
        assert_eq!(eval("3.5 x 2"), Some(7.0));
        assert_eq!(eval("9 ÷ 3 × 2"), Some(6.0));
        assert_eq!(eval("sqrt(16) + abs(-1)"), Some(5.0));
        assert!((eval("2*pi").unwrap() - std::f64::consts::TAU).abs() < 1e-12);
    }

    #[test]
    fn what_is_not_a_sum_gets_no_answer() {
        for s in ["", "firefox", "e", "pi", "42", "-5", "1/0", "sqrt(-1)", "2+", "(2", "2)", "foo(2)", "ls -la", "((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((1"] {
            assert_eq!(answer(s, true), None, "{s:?}");
        }
        assert_eq!(answer("2+2", false).as_deref(), Some("4"));
        assert_eq!(answer("sqrt(2)", true).as_deref(), Some("1,414213562"));
        assert_eq!(answer("1/3", false).as_deref(), Some("0.3333333333"));
        assert_eq!(answer("0.1+0.2", false).as_deref(), Some("0.3"));
        assert_eq!(answer("-2*0", false).as_deref(), Some("0"));
    }
}
