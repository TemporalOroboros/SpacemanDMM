//! Minimalist parser which turns a token stream into an object tree.

use linked_hash_map::LinkedHashMap;

use super::{DMError, Location, HasLocation};
use super::lexer::{LocatedToken, Token, Punctuation};
use super::objtree::ObjectTree;
use super::ast::*;

pub fn parse<I>(iter: I) -> Result<ObjectTree, DMError> where
    I: IntoIterator<Item=Result<LocatedToken, DMError>>,
    I::IntoIter: HasLocation
{
    let mut parser = Parser::new(iter.into_iter());
    let mut tree = match parser.root()? {
        Some(()) => parser.tree,
        None => return parser.parse_error(),
    };
    tree.finalize()?;
    Ok(tree)
}

type Ident = String;

// ----------------------------------------------------------------------------
// Error handling

type Status<T> = Result<Option<T>, DMError>;
// Ok(Some(x)) = the thing was parsed successfully
// Ok(None) = there isn't one of those here, nothing was read, try another
// Err(e) = an unrecoverable error

#[inline]
fn success<T>(t: T) -> Status<T> {
    Ok(Some(t))
}

const SUCCESS: Status<()> = Ok(Some(()));

macro_rules! require {
    ($self:ident.$($rest:tt)*) => {{
        let v = $self.$($rest)*;
        $self.require(v)?
    }}
}

macro_rules! leading {
    ($e:expr) => {
        match $e? {
            Some(x) => x,
            None => return Ok(None),
        }
    }
}

// ----------------------------------------------------------------------------
// Path stack and iterator over parts so far

#[derive(Debug, Copy, Clone)]
struct PathStack<'a> {
    parent: Option<&'a PathStack<'a>>,
    parts: &'a [String],
}

impl<'a> PathStack<'a> {
    fn iter(&self) -> PathStackIter<'a> {
        let mut rest = Vec::new();
        let mut current = Some(self);
        while let Some(c) = current {
            rest.push(c.parts);
            current = c.parent;
        }
        PathStackIter {
            current: &[],
            rest,
        }
    }
}

struct PathStackIter<'a> {
    current: &'a [String],
    rest: Vec<&'a [String]>,
}

impl<'a> Iterator for PathStackIter<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        loop {
            match {self.current}.split_first() {
                Some((first, rest)) => {
                    self.current = rest;
                    return Some(first);
                }
                None => match self.rest.pop() {
                    Some(c) => self.current = c,
                    None => return None,
                }
            }
        }
    }
}

// ----------------------------------------------------------------------------
// Operator precedence table

#[derive(Debug, Clone, Copy)]
struct OpInfo {
    strength: u8,
    right_binding: bool,
    token: Punctuation,
    oper: Op,
}

#[derive(Debug, Clone, Copy)]
enum Op {
    BinaryOp(BinaryOp),
    AssignOp(AssignOp),
}

impl Op {
    fn build(self, lhs: Box<Expression>, rhs: Box<Expression>) -> Expression {
        match self {
            Op::BinaryOp(op) => Expression::BinaryOp { op: op, lhs: lhs, rhs: rhs },
            Op::AssignOp(op) => Expression::AssignOp { op: op, lhs: lhs, rhs: rhs },
        }
    }
}

macro_rules! oper_table {
    (@elem ($strength:expr, $right:expr, $kind:ident, $op:ident)) => {
        OpInfo {
            strength: $strength,
            right_binding: $right,
            token: Punctuation::$op,
            oper: Op::$kind($kind::$op),
        }
    };
    (@elem ($strength:expr, $right:expr, $kind:ident, $op:ident = $punct:ident)) => {
        OpInfo {
            strength: $strength,
            right_binding: $right,
            token: Punctuation::$punct,
            oper: Op::$kind($kind::$op),
        }
    };
    ($name:ident; $($child:tt,)*) => {
        const $name: &'static [OpInfo] = &[ $(oper_table!(@elem $child),)* ];
    }
}

oper_table! { BINARY_OPS;
    (11, false, BinaryOp, Pow), //
    (10, false, BinaryOp, Mul), //
    (10, false, BinaryOp, Div = Slash), //
    (10, false, BinaryOp, Mod),
    (9,  false, BinaryOp, Add),
    (9,  false, BinaryOp, Sub),
    (8,  false, BinaryOp, Less),
    (8,  false, BinaryOp, Greater),
    (8,  false, BinaryOp, LessEq),
    (8,  false, BinaryOp, GreaterEq),
    (7,  false, BinaryOp, LShift),
    (7,  false, BinaryOp, RShift),
    (6,  false, BinaryOp, Eq),
    (6,  false, BinaryOp, NotEq), //
    (6,  false, BinaryOp, NotEq = LessGreater),
    (5,  false, BinaryOp, BitAnd),
    (5,  false, BinaryOp, BitXor),
    (5,  false, BinaryOp, BitOr),
    (4,  false, BinaryOp, And),
    (3,  false, BinaryOp, Or),
    // TODO: tertiary op here
    (0,  true,  AssignOp, Assign),
    (0,  true,  AssignOp, AddAssign),
    (0,  true,  AssignOp, SubAssign),
    (0,  true,  AssignOp, MulAssign),
    (0,  true,  AssignOp, DivAssign),
    (0,  true,  AssignOp, BitAndAssign),
    (0,  true,  AssignOp, BitOrAssign),
    (0,  true,  AssignOp, BitXorAssign),
    (0,  true,  AssignOp, LShiftAssign),
    (0,  true,  AssignOp, RShiftAssign),
}

// ----------------------------------------------------------------------------
// The parser

pub struct Parser<I> {
    tree: ObjectTree,

    input: I,
    eof: bool,
    next: Option<Token>,
    location: Location,
    expected: Vec<String>,
}

impl<I> HasLocation for Parser<I> {
    fn location(&self) -> Location {
        self.location
    }
}

impl<I> Parser<I> where
    I: Iterator<Item=Result<LocatedToken, DMError>>
{
    pub fn new(input: I) -> Parser<I> {
        Parser {
            tree: ObjectTree::with_builtins(),

            input,
            eof: false,
            next: None,
            location: Default::default(),
            expected: Vec::new(),
        }
    }

    // ------------------------------------------------------------------------
    // Basic setup

    fn parse_error<T>(&mut self) -> Result<T, DMError> {
        let expected = self.expected.join(", ");
        let got = self.next("");
        let message;
        if let Ok(got) = got {
            message = format!("got {:?}, expected one of: {}", got, expected);
            self.put_back(got);
        } else {
            message = format!("i/o error, expected one of: {}", expected);
        }
        Err(self.error(message))
    }

    fn require<T>(&mut self, t: Result<Option<T>, DMError>) -> Result<T, DMError> {
        match t {
            Ok(Some(v)) => Ok(v),
            Ok(None) => self.parse_error(),
            Err(e) => Err(e)
        }
    }

    fn next<S: Into<String>>(&mut self, expected: S) -> Result<Token, DMError> {
        let tok = self.next.take().map_or_else(|| match self.input.next() {
            Some(Ok(token)) => {
                self.expected.clear();
                self.location = token.location;
                Ok(token.token)
            }
            Some(Err(e)) => Err(e),
            None => {
                if !self.eof {
                    self.eof = true;
                    Ok(Token::Eof)
                } else {
                    Err(DMError::new(self.location, "read-after-EOF"))
                }
            }
        }, Ok);
        self.expected.push(expected.into());
        tok
    }

    fn put_back(&mut self, tok: Token) {
        if self.next.is_some() {
            panic!("cannot put_back twice")
        }
        self.next = Some(tok);
    }

    fn try_another<T>(&mut self, tok: Token) -> Status<T> {
        self.put_back(tok);
        Ok(None)
    }

    fn exact(&mut self, tok: Token) -> Status<()> {
        let next = self.next(format!("{}", tok))?;
        if next == tok {
            SUCCESS
        } else {
            self.try_another(next)
        }
    }

    fn ident(&mut self) -> Status<Ident> {
        match self.next("identifier")? {
            Token::Ident(i, _) => Ok(Some(i)),
            other => self.try_another(other),
        }
    }

    // ------------------------------------------------------------------------
    // Object tree

    fn tree_path(&mut self) -> Status<(bool, Vec<Ident>)> {
        // path :: '/'? ident (path_sep ident?)*
        // path_sep :: '/' | '.'
        let mut absolute = false;
        let mut parts = Vec::new();

        // handle leading slash
        if let Some(_) = self.exact(Token::Punct(Punctuation::Slash))? {
            absolute = true;
        }

        // expect at least one ident
        parts.push(match self.ident()? {
            Some(i) => i,
            None if !absolute => return Ok(None),
            None => return self.parse_error(),
        });
        // followed by ('/' ident)*
        loop {
            match self.next("path separator")? {
                Token::Punct(Punctuation::Slash) => {}
                //Token::Punct(Punctuation::Dot) => {}
                //Token::Punct(Punctuation::Colon) => {}
                t => { self.put_back(t); break; }
            }
            if let Some(i) = self.ident()? {
                parts.push(i);
            }
        }

        success((absolute, parts))
    }

    fn tree_entry(&mut self, parent: PathStack) -> Status<()> {
        // tree_entry :: path ';'
        // tree_entry :: path tree_block
        // tree_entry :: path '=' expression ';'
        // tree_entry :: path '(' argument_list ')' ';'
        // tree_entry :: path '(' argument_list ')' code_block

        use super::lexer::Token::*;
        use super::lexer::Punctuation::*;

        // read and calculate the current path
        let (absolute, path) = leading!(self.tree_path());
        if absolute && parent.parent.is_some() {
            println!("WARNING: {:?} inside {:?}", path, parent);
        }
        let new_stack = PathStack {
            parent: if absolute { None } else { Some(&parent) },
            parts: &path
        };

        // discard list size declaration
        match self.next("[")? {
            t @ Punct(LBracket) => {
                self.put_back(t);
                require!(self.ignore_group(LBracket, RBracket));
            }
            t => self.put_back(t),
        }

        // read the contents for real
        match self.next("contents")? {
            t @ Punct(LBrace) => {
                self.tree.add_entry(self.location, new_stack.iter())?;
                self.put_back(t);
                require!(self.tree_block(new_stack));
                SUCCESS
            }
            Punct(Assign) => {
                let expr = require!(self.expression(false));
                require!(self.exact(Punct(Semicolon)));
                self.tree.add_var(self.location, new_stack.iter(), expr)?;
                SUCCESS
            }
            t @ Punct(LParen) => {
                self.tree.add_proc(self.location, new_stack.iter())?;
                self.put_back(t);
                require!(self.ignore_group(LParen, RParen));
                match self.next("contents2")? {
                    t @ Punct(LBrace) => {
                        self.put_back(t);
                        require!(self.ignore_group(LBrace, RBrace));
                        SUCCESS
                    }
                    t => { self.put_back(t); SUCCESS }
                }
            }
            other => {
                self.tree.add_entry(self.location, new_stack.iter())?;
                self.put_back(other);
                SUCCESS
            }
        }
    }

    fn tree_entries(&mut self, parent: PathStack, terminator: Token) -> Status<()> {
        loop {
            let next = self.next(format!("newline or {:?}", terminator))?;
            if next == terminator || next == Token::Eof {
                break
            } else if next == Token::Punct(Punctuation::Semicolon) {
                continue
            }
            self.put_back(next);
            /*push*/ require!(self.tree_entry(parent));
        }
        SUCCESS
    }

    fn tree_block(&mut self, parent: PathStack) -> Status<()> {
        leading!(self.exact(Token::Punct(Punctuation::LBrace)));
        Ok(Some(require!(self.tree_entries(parent, Token::Punct(Punctuation::RBrace)))))
    }

    fn root(&mut self) -> Status<()> {
        let root = PathStack { parent: None, parts: &[] };
        self.tree_entries(root, Token::Eof)
    }

    // ------------------------------------------------------------------------
    // Expressions

    fn path_separator(&mut self) -> Status<PathOp> {
        success(match self.next("path separator")? {
            Token::Punct(Punctuation::Slash) => PathOp::Slash,
            Token::Punct(Punctuation::Dot) => PathOp::Dot,
            Token::Punct(Punctuation::Colon) => PathOp::Colon,
            other => { self.put_back(other); return Ok(None); }
        })
    }

    // distinct from a tree_path, path must begin with a path separator and can
    // use any path separator rather than just slash, AND can be followed by vars
    fn prefab(&mut self) -> Status<Prefab> {
        // path :: path_sep ident (path_sep ident?)*
        // path_sep :: '/' | '.' | ':'

        // expect at least one path element
        let mut parts = Vec::new();
        parts.push((
            leading!(self.path_separator()),
            require!(self.ident()),
        ));

        // followed by more path elements, empty ones ignored
        loop {
            if let Some(sep) = self.path_separator()? {
                if let Some(ident) = self.ident()? {
                    parts.push((sep, ident));
                }
            } else {
                break;
            }
        }

        // parse vars if we find them
        let mut vars = LinkedHashMap::default();
        if self.exact(Token::Punct(Punctuation::LBrace))?.is_some() {
            self.separated(Punctuation::Semicolon, Punctuation::RBrace, |this| {
                let key = require!(this.ident());
                require!(this.exact(Token::Punct(Punctuation::Assign)));
                let value = require!(this.expression(false));
                vars.insert(key, value);
                SUCCESS
            })?;
        }

        success(Prefab { path: parts, vars })
    }

    pub fn expression(&mut self, disallow_assign: bool) -> Status<Expression> {
        let mut expr = leading!(self.group());
        loop {
            // try to read the next operator
            let next = self.next("binary operator")?;
            let &info = match match next {
                Token::Punct(Punctuation::Assign) if disallow_assign => None,
                Token::Punct(p) => BINARY_OPS.iter().find(|op| op.token == p),
                _ => None,
            } {
                Some(info) => info,
                None => {
                    self.put_back(next);
                    return success(expr);
                }
            };

            // trampoline high-strength expression parts as the lhs of the newly found op
            expr = require!(self.expression_part(expr, info, disallow_assign));
        }
    }

    fn expression_part(&mut self, lhs: Expression, prev_op: OpInfo, disallow_assign: bool) -> Status<Expression> {
        use std::cmp::Ordering;

        let mut bits = vec![lhs];
        let mut ops = vec![prev_op.oper];
        let mut rhs = require!(self.group());
        loop {
            // try to read the next operator...
            let next = self.next("binary operator")?;
            let &info = match match next {
                Token::Punct(Punctuation::Assign) if disallow_assign => None,
                Token::Punct(p) => BINARY_OPS.iter().find(|op| op.token == p),
                _ => None
            } {
                Some(info) => info,
                None => {
                    self.put_back(next);
                    break
                }
            };

            match info.strength.cmp(&prev_op.strength) {
                Ordering::Greater => {
                    // the operator is stronger than us... recurse down
                    rhs = require!(self.expression_part(rhs, info, disallow_assign));
                }
                Ordering::Less => {
                    // the operator is weaker than us... return up
                    self.put_back(Token::Punct(info.token));
                    break;
                }
                Ordering::Equal => {
                    // the same strength... push it to the list
                    ops.push(info.oper);
                    bits.push(rhs);
                    rhs = require!(self.group());
                }
            }
        }

        // everything in 'ops' should be the same strength
        success(if prev_op.right_binding {
            let mut result = rhs;
            for (op, bit) in ops.into_iter().zip(bits.into_iter()).rev() {
                result = op.build(Box::new(bit), Box::new(result));
            }
            result
        } else {
            let mut iter = bits.into_iter();
            let mut ops_iter = ops.into_iter();
            let mut result = iter.next().unwrap();
            for (item, op) in iter.zip(&mut ops_iter) {
                result = op.build(Box::new(result), Box::new(item));
            }
            ops_iter.next().unwrap().build(Box::new(result), Box::new(rhs))
        })
    }

    fn group(&mut self) -> Status<Expression> {
        let mut unary_ops = Vec::new();
        loop {
            match self.next("unary operator")? {
                Token::Punct(Punctuation::Sub) => unary_ops.push(UnaryOp::Neg),
                Token::Punct(Punctuation::Not) => unary_ops.push(UnaryOp::Not),
                Token::Punct(Punctuation::BitNot) => unary_ops.push(UnaryOp::BitNot),
                other => { self.put_back(other); break }
            }
        }

        let term = if unary_ops.len() > 0 {
            require!(self.term())
        } else {
            leading!(self.term())
        };

        let mut follow = Vec::new();
        loop {
            match self.follow()? {
                Some(f) => follow.push(f),
                None => break,
            }
        }

        success(Expression::Base {
            unary: unary_ops,
            term: term,
            follow: follow,
        })
    }

    fn term(&mut self) -> Status<Term> {
        use super::lexer::Punctuation::*;

        success(match self.next("term")? {
            // term :: 'new' (ident | abs-path)? arglist?
            Token::Ident(ref i, _) if i == "new" => {
                // try to read an ident or path
                let t = if let Some(ident) = self.ident()? {
                    NewType::Ident(ident)
                } else if let Some(path) = self.prefab()? {
                    NewType::Prefab(path)
                } else {
                    NewType::Implicit
                };

                // try to read an arglist
                let a = self.arguments()?;

                Term::New {
                    type_: t,
                    args: a,
                }
            },

            // term :: 'list' list_lit
            Token::Ident(ref i, _) if i == "list" => {
                match self.list_arguments()? {
                    Some(args) => Term::List(args),
                    None => Term::Ident("list".to_owned()),
                }
            },

            // term :: path_lit
            t @ Token::Punct(Punctuation::Slash) |
            t @ Token::Punct(Punctuation::Dot) |
            t @ Token::Punct(Punctuation::Colon) => {
                self.put_back(t);
                Term::Prefab(require!(self.prefab()))
            }

            // term :: ident | str_lit | num_lit
            Token::Ident(val, _) => {
                match self.arguments()? {
                    Some(args) => Term::Call(val, args),
                    None => Term::Ident(val),
                }
            },
            Token::String(val) => Term::String(val),
            Token::Resource(val) => Term::Resource(val),
            Token::Int(val) => Term::Int(val),
            Token::Float(val) => Term::Float(val),

            // term :: '(' expression ')'
            Token::Punct(LParen) => {
                let expr = require!(self.expression(false));
                require!(self.exact(Token::Punct(Punctuation::RParen)));
                Term::Expr(Box::new(expr))
            },

            other => return self.try_another(other),
        })
    }

    fn follow(&mut self) -> Status<Follow> {
        success(match self.next("field, index, or function call")? {
            // follow :: '.' ident
            Token::Punct(Punctuation::Dot) => {
                let ident = require!(self.ident());
                match self.arguments()? {
                    Some(args) => Follow::Call(ident, args),
                    None => Follow::Field(ident),
                }
            }
            // follow :: '[' expression ']'
            Token::Punct(Punctuation::LBracket) => {
                let expr = require!(self.expression(false));
                require!(self.exact(Token::Punct(Punctuation::RBracket)));
                Follow::Index(Box::new(expr))
            },
            // follow :: 'as' ident
            Token::Ident(ref ident, _) if ident == "as" => {
                let cast = require!(self.ident());
                Follow::Cast(cast)
            }
            other => return self.try_another(other)
        })
    }

    fn arguments(&mut self) -> Status<Vec<Expression>> {
        // a parenthesized, comma-separated list of expressions
        leading!(self.exact(Token::Punct(Punctuation::LParen)));
        success(require!(self.comma_sep(Punctuation::RParen, |this| this.expression(false))))
    }

    fn list_arguments(&mut self) -> Status<Vec<(Expression, Option<Expression>)>> {
        // a parenthesized, comma-separated list of expressions
        leading!(self.exact(Token::Punct(Punctuation::LParen)));
        success(require!(self.comma_sep(Punctuation::RParen, |this| {
            let first_expr = require!(this.expression(true));
            success(if this.exact(Token::Punct(Punctuation::Assign))?.is_some() {
                let second_expr = require!(this.expression(false));
                (first_expr, Some(second_expr))
            } else {
                (first_expr, None)
            })
        })))
    }

    #[inline]
    fn comma_sep<R, F: FnMut(&mut Self) -> Status<R>>(&mut self, terminator: Punctuation, f: F) -> Status<Vec<R>> {
        self.separated(Punctuation::Comma, terminator, f)
    }

    fn separated<R, F: FnMut(&mut Self) -> Status<R>>(&mut self, sep: Punctuation, terminator: Punctuation, mut f: F) -> Status<Vec<R>> {
        let mut comma_legal = false;
        let mut elems = Vec::new();
        loop {
            let next = self.next("list element")?;
            if next == Token::Punct(terminator) {
                return success(elems);
            } else if comma_legal && next == Token::Punct(sep) {
                comma_legal = false;
            } else if !comma_legal {
                self.put_back(next);
                let v = f(self);
                elems.push(self.require(v)?);
                comma_legal = true;
            } else {
                self.put_back(next);
                return self.parse_error();
            }
        }
    }

    // ------------------------------------------------------------------------
    // Procs

    #[allow(dead_code)]
    fn read_any_tt(&mut self, target: &mut Vec<Token>) -> Status<()> {
        // read a single arbitrary "token tree", either a group or a single token
        let start = self.next("anything")?;
        let end = match start {
            Token::Punct(Punctuation::LParen) => Punctuation::RParen,
            Token::Punct(Punctuation::LBrace) => Punctuation::RBrace,
            Token::Punct(Punctuation::LBracket) => Punctuation::RBracket,
            other => { target.push(other); return SUCCESS; }
        };
        target.push(start);
        loop {
            match self.next("anything")? {
                Token::Punct(p) if p == end => {
                    target.push(Token::Punct(p));
                    return SUCCESS;
                }
                other => {
                    self.put_back(other);
                    require!(self.read_any_tt(target));
                }
            }
        }
    }

    fn ignore_group(&mut self, left: Punctuation, right: Punctuation) -> Status<()> {
        leading!(self.exact(Token::Punct(left)));
        let mut depth = 1;
        while depth > 0 {
            let n = self.next("anything")?;
            match n {
                Token::Punct(p) if p == left => depth += 1,
                Token::Punct(p) if p == right => depth -= 1,
                _ => {}
            }
        }
        SUCCESS
    }
}