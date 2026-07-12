// A small C-like expression evaluator for `print`/`set`. Supports integer and
// float literals, identifiers (resolved as debuggee variables via
// EvalContext), $register references, the usual arithmetic/bitwise/
// comparison operators, unary * (deref) / & (address-of), and the
// type-aware postfix operators . -> [].
//
// `.`/`->`/`[]` need to know field offsets and element sizes, which only the
// debug-info-backed context (dbghelp) knows, so EvalContext exposes an
// opaque TypeHandle plus shape()/element()/member() for the evaluator to
// navigate without itself depending on dbghelp.

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Int(i64),
    Float(f64),
    Ident(String),
    Register(String),
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Amp,
    Pipe,
    Caret,
    Tilde,
    Bang,
    Shl,
    Shr,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    Dot,
    Arrow,
    LParen,
    RParen,
    LBracket,
    RBracket,
}

fn lex(input: &str) -> Result<Vec<Token>, String> {
    let chars: Vec<char> = input.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }

        if c.is_ascii_digit() {
            let start = i;
            if c == '0' && chars.get(i + 1).is_some_and(|&c| c == 'x' || c == 'X') {
                i += 2;
                let hex_start = i;
                while i < chars.len() && chars[i].is_ascii_hexdigit() {
                    i += 1;
                }
                let value = i64::from_str_radix(&chars[hex_start..i].iter().collect::<String>(), 16)
                    .map_err(|_| format!("invalid hex literal: {}", &chars[start..i].iter().collect::<String>()))?;
                tokens.push(Token::Int(value));
            } else {
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
                if chars.get(i) == Some(&'.') && chars.get(i + 1).is_some_and(|c| c.is_ascii_digit()) {
                    i += 1;
                    while i < chars.len() && chars[i].is_ascii_digit() {
                        i += 1;
                    }
                    let text = chars[start..i].iter().collect::<String>();
                    let value = text
                        .parse::<f64>()
                        .map_err(|_| format!("invalid float literal: {}", text))?;
                    tokens.push(Token::Float(value));
                } else {
                    let text = chars[start..i].iter().collect::<String>();
                    let value = text
                        .parse::<i64>()
                        .map_err(|_| format!("invalid integer literal: {}", text))?;
                    tokens.push(Token::Int(value));
                }
            }
            continue;
        }

        if c == '$' {
            i += 1;
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            if start == i {
                return Err("expected register name after '$'".to_string());
            }
            tokens.push(Token::Register(chars[start..i].iter().collect()));
            continue;
        }

        if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            tokens.push(Token::Ident(chars[start..i].iter().collect()));
            continue;
        }

        macro_rules! two_char {
            ($next:expr, $two:expr, $one:expr) => {{
                if chars.get(i + 1) == Some(&$next) {
                    i += 2;
                    tokens.push($two);
                } else {
                    i += 1;
                    tokens.push($one);
                }
            }};
        }

        match c {
            '+' => {
                i += 1;
                tokens.push(Token::Plus);
            }
            '-' => {
                if chars.get(i + 1) == Some(&'>') {
                    i += 2;
                    tokens.push(Token::Arrow);
                } else {
                    i += 1;
                    tokens.push(Token::Minus);
                }
            }
            '*' => {
                i += 1;
                tokens.push(Token::Star);
            }
            '/' => {
                i += 1;
                tokens.push(Token::Slash);
            }
            '%' => {
                i += 1;
                tokens.push(Token::Percent);
            }
            '^' => {
                i += 1;
                tokens.push(Token::Caret);
            }
            '~' => {
                i += 1;
                tokens.push(Token::Tilde);
            }
            '.' => {
                i += 1;
                tokens.push(Token::Dot);
            }
            '(' => {
                i += 1;
                tokens.push(Token::LParen);
            }
            ')' => {
                i += 1;
                tokens.push(Token::RParen);
            }
            '[' => {
                i += 1;
                tokens.push(Token::LBracket);
            }
            ']' => {
                i += 1;
                tokens.push(Token::RBracket);
            }
            '&' => two_char!('&', Token::Amp, Token::Amp),
            '|' => two_char!('|', Token::Pipe, Token::Pipe),
            '=' => two_char!('=', Token::Eq, Token::Eq),
            '!' => two_char!('=', Token::Ne, Token::Bang),
            '<' => {
                if chars.get(i + 1) == Some(&'<') {
                    i += 2;
                    tokens.push(Token::Shl);
                } else {
                    two_char!('=', Token::Le, Token::Lt)
                }
            }
            '>' => {
                if chars.get(i + 1) == Some(&'>') {
                    i += 2;
                    tokens.push(Token::Shr);
                } else {
                    two_char!('=', Token::Ge, Token::Gt)
                }
            }
            _ => return Err(format!("unexpected character: '{}'", c)),
        }
    }

    Ok(tokens)
}

#[derive(Debug, Clone)]
pub enum Expr {
    Int(i64),
    Float(f64),
    Ident(String),
    Register(String),
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Member(Box<Expr>, String, bool), // bool: true for `->`, false for `.`
    Index(Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone, Copy)]
pub enum UnOp {
    Neg,
    Pos,
    Not,
    BitNot,
    Deref,
    AddrOf,
}

#[derive(Debug, Clone, Copy)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    And,
    Or,
    Xor,
    Shl,
    Shr,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.pos).cloned();
        self.pos += 1;
        t
    }

    fn expect(&mut self, tok: &Token) -> Result<(), String> {
        if self.peek() == Some(tok) {
            self.pos += 1;
            Ok(())
        } else {
            Err(format!("expected {:?}, found {:?}", tok, self.peek()))
        }
    }

    fn expect_ident(&mut self) -> Result<String, String> {
        match self.next() {
            Some(Token::Ident(name)) => Ok(name),
            other => Err(format!("expected a field name, found {:?}", other)),
        }
    }

    fn parse_expr(&mut self) -> Result<Expr, String> {
        self.parse_equality()
    }

    fn parse_equality(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_comparison()?;
        loop {
            let op = match self.peek() {
                Some(Token::Eq) => BinOp::Eq,
                Some(Token::Ne) => BinOp::Ne,
                _ => break,
            };
            self.pos += 1;
            let rhs = self.parse_comparison()?;
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_comparison(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_bitor()?;
        loop {
            let op = match self.peek() {
                Some(Token::Lt) => BinOp::Lt,
                Some(Token::Gt) => BinOp::Gt,
                Some(Token::Le) => BinOp::Le,
                Some(Token::Ge) => BinOp::Ge,
                _ => break,
            };
            self.pos += 1;
            let rhs = self.parse_bitor()?;
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_bitor(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_bitxor()?;
        while self.peek() == Some(&Token::Pipe) {
            self.pos += 1;
            let rhs = self.parse_bitxor()?;
            lhs = Expr::Binary(BinOp::Or, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_bitxor(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_bitand()?;
        while self.peek() == Some(&Token::Caret) {
            self.pos += 1;
            let rhs = self.parse_bitand()?;
            lhs = Expr::Binary(BinOp::Xor, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_bitand(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_shift()?;
        while self.peek() == Some(&Token::Amp) {
            self.pos += 1;
            let rhs = self.parse_shift()?;
            lhs = Expr::Binary(BinOp::And, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_shift(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_additive()?;
        loop {
            let op = match self.peek() {
                Some(Token::Shl) => BinOp::Shl,
                Some(Token::Shr) => BinOp::Shr,
                _ => break,
            };
            self.pos += 1;
            let rhs = self.parse_additive()?;
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_additive(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_multiplicative()?;
        loop {
            let op = match self.peek() {
                Some(Token::Plus) => BinOp::Add,
                Some(Token::Minus) => BinOp::Sub,
                _ => break,
            };
            self.pos += 1;
            let rhs = self.parse_multiplicative()?;
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_multiplicative(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Some(Token::Star) => BinOp::Mul,
                Some(Token::Slash) => BinOp::Div,
                Some(Token::Percent) => BinOp::Mod,
                _ => break,
            };
            self.pos += 1;
            let rhs = self.parse_unary()?;
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<Expr, String> {
        let op = match self.peek() {
            Some(Token::Minus) => Some(UnOp::Neg),
            Some(Token::Plus) => Some(UnOp::Pos),
            Some(Token::Bang) => Some(UnOp::Not),
            Some(Token::Tilde) => Some(UnOp::BitNot),
            Some(Token::Star) => Some(UnOp::Deref),
            Some(Token::Amp) => Some(UnOp::AddrOf),
            _ => None,
        };
        if let Some(op) = op {
            self.pos += 1;
            let operand = self.parse_unary()?;
            return Ok(Expr::Unary(op, Box::new(operand)));
        }
        self.parse_postfix()
    }

    fn parse_postfix(&mut self) -> Result<Expr, String> {
        let mut expr = self.parse_primary()?;
        loop {
            match self.peek() {
                Some(Token::Dot) => {
                    self.pos += 1;
                    let name = self.expect_ident()?;
                    expr = Expr::Member(Box::new(expr), name, false);
                }
                Some(Token::Arrow) => {
                    self.pos += 1;
                    let name = self.expect_ident()?;
                    expr = Expr::Member(Box::new(expr), name, true);
                }
                Some(Token::LBracket) => {
                    self.pos += 1;
                    let index = self.parse_expr()?;
                    self.expect(&Token::RBracket)?;
                    expr = Expr::Index(Box::new(expr), Box::new(index));
                }
                _ => break,
            }
        }
        Ok(expr)
    }

    fn parse_primary(&mut self) -> Result<Expr, String> {
        match self.next() {
            Some(Token::Int(n)) => Ok(Expr::Int(n)),
            Some(Token::Float(f)) => Ok(Expr::Float(f)),
            Some(Token::Ident(name)) => Ok(Expr::Ident(name)),
            Some(Token::Register(name)) => Ok(Expr::Register(name)),
            Some(Token::LParen) => {
                let expr = self.parse_expr()?;
                self.expect(&Token::RParen)?;
                Ok(expr)
            }
            other => Err(format!("unexpected token: {:?}", other)),
        }
    }
}

pub fn parse(input: &str) -> Result<Expr, String> {
    let tokens = lex(input)?;
    if tokens.is_empty() {
        return Err("empty expression".to_string());
    }
    let mut parser = Parser { tokens, pos: 0 };
    let expr = parser.parse_expr()?;
    if parser.pos != parser.tokens.len() {
        return Err(format!("unexpected trailing input near {:?}", parser.peek()));
    }
    Ok(expr)
}

// Splits "lhs = rhs" on the first assignment '=', taking care not to match
// inside "==", "!=", "<=", ">=".
pub fn split_assignment(input: &str) -> Option<(&str, &str)> {
    let bytes = input.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] != b'=' {
            continue;
        }
        let prev = if i > 0 { bytes[i - 1] } else { 0 };
        let next = bytes.get(i + 1).copied().unwrap_or(0);
        if next == b'=' || matches!(prev, b'=' | b'!' | b'<' | b'>') {
            continue;
        }
        return Some((&input[..i], &input[i + 1..]));
    }
    None
}

// ---- Values, types, and the debuggee bridge ----

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Value {
    Int(i64),
    Float(f64),
}

impl Value {
    pub fn as_i64(self) -> i64 {
        match self {
            Value::Int(n) => n,
            Value::Float(f) => f as i64,
        }
    }

    pub fn as_f64(self) -> f64 {
        match self {
            Value::Int(n) => n as f64,
            Value::Float(f) => f,
        }
    }

    fn is_float(self) -> bool {
        matches!(self, Value::Float(_))
    }
}

impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Int(n) => write!(f, "{} ({:#x})", n, *n as u64),
            Value::Float(x) => write!(f, "{}", x),
        }
    }
}

// Opaque handle for a debug-info type, round-tripped between EvalContext and
// the evaluator without the evaluator knowing what the fields mean (dbghelp
// module base + type index, in the debugger's implementation).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TypeHandle(pub u64, pub u32);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ValueKind {
    Int,
    Float,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TypeShape {
    Pointer,
    Array,
    Other,
}

#[derive(Debug, Clone, Copy)]
pub struct TypeMeta {
    pub size: u32,
    pub kind: ValueKind,
    pub ty: Option<TypeHandle>,
}

#[derive(Debug, Clone, Copy)]
pub struct TypedLocation {
    pub address: u64,
    pub size: u32,
    pub kind: ValueKind,
    pub ty: Option<TypeHandle>,
}

#[derive(Debug, Clone)]
pub enum Location {
    Memory(TypedLocation),
    Register(String),
}

pub trait EvalContext {
    fn read_typed(&self, loc: TypedLocation) -> Option<Value>;
    fn write_typed(&self, loc: TypedLocation, value: Value) -> bool;
    fn variable(&self, name: &str) -> Option<Location>;
    fn register(&self, name: &str) -> Option<i64>;
    fn write_register(&self, name: &str, value: i64) -> bool;

    /// Whether `ty` is a pointer, array, or neither (needed to tell apart
    /// `p[i]` where p is a pointer, indexing through its *value*, from
    /// `a[i]` where a is an array, indexing its own address).
    fn shape(&self, ty: TypeHandle) -> TypeShape;
    /// Pointee type for a pointer, or element type for an array.
    fn element(&self, ty: TypeHandle) -> Option<TypeMeta>;
    /// Offset and type of a named field of a struct/union type.
    fn member(&self, ty: TypeHandle, field: &str) -> Option<(u32, TypeMeta)>;
}

fn read_location(loc: &Location, ctx: &dyn EvalContext) -> Result<Value, String> {
    match loc {
        Location::Memory(tl) => ctx
            .read_typed(*tl)
            .ok_or_else(|| format!("cannot read memory at {:#x}", tl.address)),
        Location::Register(name) => ctx
            .register(name)
            .map(Value::Int)
            .ok_or_else(|| format!("unknown register: ${}", name)),
    }
}

pub fn eval(expr: &Expr, ctx: &dyn EvalContext) -> Result<Value, String> {
    match expr {
        Expr::Int(n) => Ok(Value::Int(*n)),
        Expr::Float(f) => Ok(Value::Float(*f)),
        Expr::Ident(_) | Expr::Unary(UnOp::Deref, _) | Expr::Member(..) | Expr::Index(..) => {
            read_location(&eval_location(expr, ctx)?, ctx)
        }
        Expr::Register(name) => ctx
            .register(name)
            .map(Value::Int)
            .ok_or_else(|| format!("unknown register: ${}", name)),
        Expr::Unary(op, inner) => match op {
            UnOp::AddrOf => match eval_location(inner, ctx)? {
                Location::Memory(tl) => Ok(Value::Int(tl.address as i64)),
                Location::Register(name) => Err(format!("cannot take the address of ${}", name)),
            },
            UnOp::Neg => {
                let v = eval(inner, ctx)?;
                Ok(if v.is_float() {
                    Value::Float(-v.as_f64())
                } else {
                    Value::Int(v.as_i64().wrapping_neg())
                })
            }
            UnOp::Pos => eval(inner, ctx),
            UnOp::Not => Ok(Value::Int(i64::from(eval(inner, ctx)?.as_i64() == 0))),
            UnOp::BitNot => Ok(Value::Int(!eval(inner, ctx)?.as_i64())),
            UnOp::Deref => unreachable!("handled above"),
        },
        Expr::Binary(op, lhs, rhs) => {
            let l = eval(lhs, ctx)?;
            let r = eval(rhs, ctx)?;
            eval_binary(*op, l, r)
        }
    }
}

fn eval_binary(op: BinOp, l: Value, r: Value) -> Result<Value, String> {
    if matches!(
        op,
        BinOp::And | BinOp::Or | BinOp::Xor | BinOp::Shl | BinOp::Shr
    ) {
        let (li, ri) = (l.as_i64(), r.as_i64());
        return Ok(Value::Int(match op {
            BinOp::And => li & ri,
            BinOp::Or => li | ri,
            BinOp::Xor => li ^ ri,
            BinOp::Shl => li.wrapping_shl(ri as u32),
            BinOp::Shr => li.wrapping_shr(ri as u32),
            _ => unreachable!(),
        }));
    }

    if l.is_float() || r.is_float() {
        let (lf, rf) = (l.as_f64(), r.as_f64());
        return Ok(match op {
            BinOp::Add => Value::Float(lf + rf),
            BinOp::Sub => Value::Float(lf - rf),
            BinOp::Mul => Value::Float(lf * rf),
            BinOp::Div => {
                if rf == 0.0 {
                    return Err("division by zero".to_string());
                }
                Value::Float(lf / rf)
            }
            BinOp::Mod => {
                if rf == 0.0 {
                    return Err("division by zero".to_string());
                }
                Value::Float(lf % rf)
            }
            BinOp::Eq => Value::Int(i64::from(lf == rf)),
            BinOp::Ne => Value::Int(i64::from(lf != rf)),
            BinOp::Lt => Value::Int(i64::from(lf < rf)),
            BinOp::Gt => Value::Int(i64::from(lf > rf)),
            BinOp::Le => Value::Int(i64::from(lf <= rf)),
            BinOp::Ge => Value::Int(i64::from(lf >= rf)),
            _ => unreachable!(),
        });
    }

    let (li, ri) = (l.as_i64(), r.as_i64());
    Ok(match op {
        BinOp::Add => Value::Int(li.wrapping_add(ri)),
        BinOp::Sub => Value::Int(li.wrapping_sub(ri)),
        BinOp::Mul => Value::Int(li.wrapping_mul(ri)),
        BinOp::Div => {
            if ri == 0 {
                return Err("division by zero".to_string());
            }
            Value::Int(li.wrapping_div(ri))
        }
        BinOp::Mod => {
            if ri == 0 {
                return Err("division by zero".to_string());
            }
            Value::Int(li.wrapping_rem(ri))
        }
        BinOp::Eq => Value::Int(i64::from(li == ri)),
        BinOp::Ne => Value::Int(i64::from(li != ri)),
        BinOp::Lt => Value::Int(i64::from(li < ri)),
        BinOp::Gt => Value::Int(i64::from(li > ri)),
        BinOp::Le => Value::Int(i64::from(li <= ri)),
        BinOp::Ge => Value::Int(i64::from(li >= ri)),
        _ => unreachable!(),
    })
}

pub fn eval_location(expr: &Expr, ctx: &dyn EvalContext) -> Result<Location, String> {
    match expr {
        Expr::Ident(name) => ctx
            .variable(name)
            .ok_or_else(|| format!("undefined variable: {}", name)),
        Expr::Register(name) => Ok(Location::Register(name.clone())),
        Expr::Unary(UnOp::Deref, inner) => {
            let addr = eval(inner, ctx)?.as_i64() as u64;
            // If `inner` is itself a typed pointer lvalue, use its pointee
            // type/size (e.g. `*p` where `p` is `double *`); otherwise fall
            // back to a plain 4-byte int read.
            let meta = eval_location(inner, ctx).ok().and_then(|loc| match loc {
                Location::Memory(tl) => tl.ty.and_then(|ty| ctx.element(ty)),
                Location::Register(_) => None,
            });
            let meta = meta.unwrap_or(TypeMeta {
                size: 4,
                kind: ValueKind::Int,
                ty: None,
            });
            Ok(Location::Memory(TypedLocation {
                address: addr,
                size: meta.size,
                kind: meta.kind,
                ty: meta.ty,
            }))
        }
        Expr::Member(base, field, is_arrow) => {
            let (struct_addr, struct_ty) = if *is_arrow {
                let base_ty = match eval_location(base, ctx) {
                    Ok(Location::Memory(tl)) => tl.ty,
                    _ => None,
                };
                let base_ty = base_ty.ok_or_else(|| "cannot determine pointer type for '->'".to_string())?;
                let pointee = ctx
                    .element(base_ty)
                    .ok_or_else(|| "'->' used on a non-pointer".to_string())?;
                let struct_ty = pointee
                    .ty
                    .ok_or_else(|| "unknown pointee type".to_string())?;
                let ptr_value = eval(base, ctx)?.as_i64() as u64;
                (ptr_value, struct_ty)
            } else {
                match eval_location(base, ctx)? {
                    Location::Memory(tl) => {
                        let ty = tl.ty.ok_or_else(|| "unknown struct type".to_string())?;
                        (tl.address, ty)
                    }
                    Location::Register(name) => {
                        return Err(format!("cannot use '.' on ${}", name));
                    }
                }
            };

            let (offset, member_meta) = ctx
                .member(struct_ty, field)
                .ok_or_else(|| format!("no member named '{}'", field))?;
            Ok(Location::Memory(TypedLocation {
                address: struct_addr.wrapping_add(offset as u64),
                size: member_meta.size,
                kind: member_meta.kind,
                ty: member_meta.ty,
            }))
        }
        Expr::Index(base, index) => {
            let base_loc = eval_location(base, ctx)?;
            let Location::Memory(tl) = base_loc else {
                return Err("expression is not indexable".to_string());
            };
            let base_ty = tl.ty.ok_or_else(|| "unknown type for indexing".to_string())?;
            let elem_meta = ctx
                .element(base_ty)
                .ok_or_else(|| "not an array or pointer".to_string())?;
            let base_address = match ctx.shape(base_ty) {
                TypeShape::Pointer => read_location(&Location::Memory(tl), ctx)?.as_i64() as u64,
                TypeShape::Array => tl.address,
                TypeShape::Other => return Err("not an array or pointer".to_string()),
            };
            let idx = eval(index, ctx)?.as_i64();
            let addr = (base_address as i64).wrapping_add(idx.wrapping_mul(elem_meta.size as i64));
            Ok(Location::Memory(TypedLocation {
                address: addr as u64,
                size: elem_meta.size,
                kind: elem_meta.kind,
                ty: elem_meta.ty,
            }))
        }
        _ => Err("expression is not assignable".to_string()),
    }
}
