//! Inline calculator: a query that is a complete arithmetic expression gets a result row.

/// The value of `query` if it is a complete expression of numbers and symbols with at
/// least one operation (`6x5`, `2(3+4)`, `50%`). Anything else is None: plain numbers,
/// unfinished input (`4x`, `3+`), words, division by zero.
pub fn evaluate(query: &str) -> Option<f64> {
    let mut parser = Parser { tokens: tokenize(query)?, pos: 0, ops: 0, depth: 0 };
    let value = parser.expr()?;
    let complete = parser.pos == parser.tokens.len() && parser.ops > 0;
    (complete && value.is_finite()).then_some(value)
}

/// The result as shown and copied: integers without a decimal point, otherwise up to
/// 10 significant digits.
pub fn format(value: f64) -> String {
    if value == 0.0 {
        return "0".into();
    }
    let abs = value.abs();
    if !(1e-6..1e15).contains(&abs) {
        let s = std::format!("{value:.9e}");
        let (mantissa, exp) = s.split_once('e').unwrap_or((&s, "0"));
        return std::format!("{}e{exp}", trim_zeros(mantissa));
    }
    let decimals = (9 - abs.log10().floor() as i32).max(0) as usize;
    trim_zeros(&std::format!("{value:.decimals$}")).to_string()
}

/// The query as displayed above the result: real math symbols, single spaces around
/// binary operators.
pub fn pretty(query: &str) -> String {
    let mut out = String::new();
    for c in query.chars().filter(|c| !c.is_whitespace()) {
        let symbol = match c {
            'x' | 'X' | '*' | '\u{d7}' => '\u{d7}',
            '/' | '\u{f7}' => '\u{f7}',
            '-' | '\u{2212}' => '\u{2212}',
            '+' => '+',
            '^' => '^',
            c => {
                out.push(c);
                continue;
            }
        };
        // Binary only when it follows a number, ")" or "%"; otherwise it's a sign (-3).
        let binary = symbol != '^' && out.ends_with(|p: char| p.is_ascii_digit() || p == '.' || p == ')' || p == '%');
        if binary {
            out.push(' ');
            out.push(symbol);
            out.push(' ');
        } else {
            out.push(symbol);
        }
    }
    out
}

fn trim_zeros(s: &str) -> &str {
    if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.') } else { s }
}

#[derive(Clone, Copy, PartialEq)]
enum Token {
    Num(f64),
    Plus,
    Minus,
    Mul,
    Div,
    Pow,
    Percent,
    Open,
    Close,
}

/// None if the query has anything but numbers, spaces and operator symbols.
fn tokenize(query: &str) -> Option<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut chars = query.chars().peekable();
    while let Some(c) = chars.next() {
        let token = match c {
            c if c.is_whitespace() => continue,
            '0'..='9' | '.' => {
                let mut num = String::from(c);
                while let Some(&d) = chars.peek().filter(|d| d.is_ascii_digit() || **d == '.') {
                    num.push(d);
                    chars.next();
                }
                Token::Num(num.parse().ok()?)
            }
            '+' => Token::Plus,
            '-' | '\u{2212}' => Token::Minus,
            '*' | 'x' | 'X' | '\u{d7}' => Token::Mul,
            '/' | '\u{f7}' => Token::Div,
            '^' => Token::Pow,
            '%' => Token::Percent,
            '(' => Token::Open,
            ')' => Token::Close,
            _ => return None,
        };
        tokens.push(token);
    }
    Some(tokens)
}

/// Parentheses, unary signs and powers recurse. Past this depth, parsing returns None:
/// a stack overflow from a pasted wall of `(` aborts the process, it isn't a panic.
const MAX_DEPTH: usize = 64;

/// Recursive descent, usual precedence: `+ -` < `× ÷` (and implicit `2(…)`) < unary minus
/// < `^` (right-associative) < postfix `%`. `ops` counts operations, so a bare number
/// or `-5` isn't treated as a calculation.
struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    ops: usize,
    depth: usize,
}

impl Parser {
    fn peek(&self) -> Option<Token> {
        self.tokens.get(self.pos).copied()
    }

    fn eat(&mut self, token: Token) -> bool {
        let found = self.peek() == Some(token);
        if found {
            self.pos += 1;
        }
        found
    }

    /// One more recursive step (a parenthesis, a unary sign, or a power). None past the cap.
    fn nest(&mut self) -> Option<()> {
        self.depth += 1;
        (self.depth <= MAX_DEPTH).then_some(())
    }

    fn expr(&mut self) -> Option<f64> {
        let mut value = self.term()?;
        loop {
            if self.eat(Token::Plus) {
                value += self.term()?;
            } else if self.eat(Token::Minus) {
                value -= self.term()?;
            } else {
                return Some(value);
            }
            self.ops += 1;
        }
    }

    fn term(&mut self) -> Option<f64> {
        let mut value = self.unary()?;
        loop {
            if self.eat(Token::Mul) || self.peek() == Some(Token::Open) {
                value *= self.unary()?;
            } else if self.eat(Token::Div) {
                let divisor = self.unary()?;
                if divisor == 0.0 {
                    return None;
                }
                value /= divisor;
            } else {
                return Some(value);
            }
            self.ops += 1;
        }
    }

    fn unary(&mut self) -> Option<f64> {
        if self.eat(Token::Minus) {
            self.nest()?;
            let value = -self.unary()?;
            self.depth -= 1;
            Some(value)
        } else if self.eat(Token::Plus) {
            self.nest()?;
            let value = self.unary()?;
            self.depth -= 1;
            Some(value)
        } else {
            self.power()
        }
    }

    fn power(&mut self) -> Option<f64> {
        let base = self.postfix()?;
        if !self.eat(Token::Pow) {
            return Some(base);
        }
        self.ops += 1;
        self.nest()?;
        let exp = self.unary()?;
        self.depth -= 1;
        Some(base.powf(exp))
    }

    fn postfix(&mut self) -> Option<f64> {
        let mut value = match self.peek()? {
            Token::Num(n) => {
                self.pos += 1;
                n
            }
            Token::Open => {
                self.pos += 1;
                self.nest()?;
                let inner = self.expr()?;
                self.depth -= 1;
                self.eat(Token::Close).then_some(inner)?
            }
            _ => return None,
        };
        while self.eat(Token::Percent) {
            self.ops += 1;
            value /= 100.0;
        }
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(q: &str) -> Option<String> {
        evaluate(q).map(format)
    }

    #[test]
    fn complete_expressions() {
        assert_eq!(eval("2x2").as_deref(), Some("4"));
        assert_eq!(eval("6x5").as_deref(), Some("30"));
        assert_eq!(eval("6 X 5").as_deref(), Some("30"));
        assert_eq!(eval("3+3").as_deref(), Some("6"));
        assert_eq!(eval("3 × 4").as_deref(), Some("12"));
        assert_eq!(eval("10÷4").as_deref(), Some("2.5"));
        assert_eq!(eval("10/4").as_deref(), Some("2.5"));
        assert_eq!(eval("2^10").as_deref(), Some("1024"));
        assert_eq!(eval("2^3^2").as_deref(), Some("512"));
        assert_eq!(eval("-2^2").as_deref(), Some("-4"));
        assert_eq!(eval("(3+3)*2").as_deref(), Some("12"));
        assert_eq!(eval("2(3+4)").as_deref(), Some("14"));
        assert_eq!(eval("(1+1)(2+2)").as_deref(), Some("8"));
        assert_eq!(eval("1+2*3").as_deref(), Some("7"));
        assert_eq!(eval("50%").as_deref(), Some("0.5"));
        assert_eq!(eval("200*15%").as_deref(), Some("30"));
        assert_eq!(eval("-3+5").as_deref(), Some("2"));
        assert_eq!(eval("5--3").as_deref(), Some("8"));
        assert_eq!(eval("1/3").as_deref(), Some("0.3333333333"));
        assert_eq!(eval(".5+.25").as_deref(), Some("0.75"));
        assert_eq!(eval("0.1+0.2").as_deref(), Some("0.3"));
        assert_eq!(eval("5−3").as_deref(), Some("2"));
    }

    #[test]
    fn not_calculations() {
        for q in [
            "", "4", "42", "4x", "6x", "x4", "x", "-5", "(5)", "3+", "+", "*3", "2 3", "1..2", "3+3)",
            "(3+3", "15% of 80", "firefox", "2x2 apples", "1/0", "0/0", "2^99999", "1,000+1", "4xx5",
        ] {
            assert_eq!(evaluate(q), None, "{q:?} should not be a calculation");
        }
    }

    #[test]
    fn pretty_expression() {
        assert_eq!(pretty("6x5"), "6 \u{d7} 5");
        assert_eq!(pretty("10/4"), "10 \u{f7} 4");
        assert_eq!(pretty("-3+5"), "\u{2212}3 + 5");
        assert_eq!(pretty("5--3"), "5 \u{2212} \u{2212}3");
        assert_eq!(pretty("2^10"), "2^10");
        assert_eq!(pretty("(3+3) * 2"), "(3 + 3) \u{d7} 2");
        assert_eq!(pretty("200*15%"), "200 \u{d7} 15%");
    }

    #[test]
    fn deep_parentheses_are_not_a_calculation() {
        // A few hundred, not enough to overflow the stack. Without the cap this is 2.
        let deep = format!("{}1+1{}", "(".repeat(300), ")".repeat(300));
        assert_eq!(evaluate(&deep), None);
        assert_eq!(eval("6x5").as_deref(), Some("30"));
        assert_eq!(eval("2(3+4)").as_deref(), Some("14"));
        assert_eq!(eval("200*15%").as_deref(), Some("30"));
        assert_eq!(eval("2^10").as_deref(), Some("1024"));
    }

    #[test]
    fn formatting() {
        assert_eq!(format(-0.0), "0");
        assert_eq!(format(1e20), "1e20");
        assert_eq!(format(1.5e-9), "1.5e-9");
        assert_eq!(format(123456789012.0), "123456789012");
    }
}
