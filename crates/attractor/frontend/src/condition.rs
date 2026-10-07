//! Fabro's edge-condition grammar, parsed and lowered onto the engine's
//! expression language.
//!
//! ```text
//! Expr   ::= Or
//! Or     ::= And ('||' And)*
//! And    ::= Unary ('&&' Unary)*
//! Unary  ::= '!' Unary | Clause
//! Clause ::= Key Op Literal | Key            (a bare key is a truthiness test)
//! Op     ::= '=' | '!=' | '>' | '<' | '>=' | '<=' | 'contains' | 'matches'
//! ```
//!
//! Keys: `outcome` (the stage outcome), `preferred_label` (the reported
//! label), `nodes.<id>.<field>` (a completed node's record: `status`,
//! `output`, `generation`, `attempts`, `success_like`), and anything else
//! is a flat run-context key — `context.K` and bare `K` both read `kv.K`.
//! Every clause lowers onto Fabro's documented comparison semantics,
//! spelled out as expressions rather than as new builtins; see [`lower`].

use frontend::{Diagnostics, Span};
use ir::{BinOp, ExprId, ExprTable, UnOp};

use crate::kinds::{COMPAT_SUNSET, OUTCOMES};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    Eq,
    NotEq,
    Gt,
    Lt,
    Gte,
    Lte,
    Contains,
    Matches,
    Truthy,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Clause {
    pub key:   String,
    pub op:    Op,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Condition {
    Clause(Clause),
    Not(Box<Self>),
    And(Vec<Self>),
    Or(Vec<Self>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    Word(String),
    Eq,
    NotEq,
    Gt,
    Lt,
    Gte,
    Lte,
    And,
    Or,
    Not,
    Contains,
    Matches,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ConditionError(String);

fn tokenize(input: &str) -> Result<Vec<Token>, ConditionError> {
    let chars: Vec<char> = input.trim().chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        let next = chars.get(i + 1).copied();
        let two = match (c, next) {
            ('&', Some('&')) => Some(Token::And),
            ('|', Some('|')) => Some(Token::Or),
            ('!', Some('=')) => Some(Token::NotEq),
            ('>', Some('=')) => Some(Token::Gte),
            ('<', Some('=')) => Some(Token::Lte),
            _ => None,
        };
        if let Some(token) = two {
            tokens.push(token);
            i += 2;
            continue;
        }
        let one = match c {
            '=' => Some(Token::Eq),
            '>' => Some(Token::Gt),
            '<' => Some(Token::Lt),
            '!' => Some(Token::Not),
            _ => None,
        };
        if let Some(token) = one {
            tokens.push(token);
            i += 1;
            continue;
        }
        if c == '"' {
            let start = i;
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' {
                    i += 1;
                }
                i += 1;
            }
            if i >= chars.len() {
                return Err(ConditionError("unterminated string".into()));
            }
            i += 1;
            tokens.push(Token::Word(chars[start..i].iter().collect()));
            continue;
        }
        let start = i;
        while i < chars.len()
            && !chars[i].is_whitespace()
            && !matches!(chars[i], '=' | '!' | '>' | '<' | '&' | '|' | '"')
        {
            i += 1;
        }
        if i == start {
            return Err(ConditionError(format!("unexpected character {c:?}")));
        }
        let word: String = chars[start..i].iter().collect();
        let after_word = matches!(tokens.last(), Some(Token::Word(_)));
        tokens.push(match word.as_str() {
            "contains" if after_word => Token::Contains,
            "matches" if after_word => Token::Matches,
            _ => Token::Word(word),
        });
    }
    Ok(tokens)
}

struct Parser {
    tokens: Vec<Token>,
    pos:    usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn advance(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.pos).cloned();
        if token.is_some() {
            self.pos += 1;
        }
        token
    }

    fn or(&mut self) -> Result<Condition, ConditionError> {
        let mut children = vec![self.and()?];
        while self.peek() == Some(&Token::Or) {
            self.advance();
            children.push(self.and()?);
        }
        Ok(if children.len() == 1 {
            children.pop().expect("one child")
        } else {
            Condition::Or(children)
        })
    }

    fn and(&mut self) -> Result<Condition, ConditionError> {
        let mut children = vec![self.unary()?];
        while self.peek() == Some(&Token::And) {
            self.advance();
            children.push(self.unary()?);
        }
        Ok(if children.len() == 1 {
            children.pop().expect("one child")
        } else {
            Condition::And(children)
        })
    }

    fn unary(&mut self) -> Result<Condition, ConditionError> {
        if self.peek() == Some(&Token::Not) {
            self.advance();
            return Ok(Condition::Not(Box::new(self.unary()?)));
        }
        self.clause()
    }

    fn clause(&mut self) -> Result<Condition, ConditionError> {
        let key = match self.advance() {
            Some(Token::Word(w)) => w,
            Some(other) => return Err(ConditionError(format!("expected a key, found {other:?}"))),
            None => return Err(ConditionError("unexpected end of condition".into())),
        };
        let op = match self.peek() {
            Some(Token::Eq) => Op::Eq,
            Some(Token::NotEq) => Op::NotEq,
            Some(Token::Gt) => Op::Gt,
            Some(Token::Lt) => Op::Lt,
            Some(Token::Gte) => Op::Gte,
            Some(Token::Lte) => Op::Lte,
            Some(Token::Contains) => Op::Contains,
            Some(Token::Matches) => Op::Matches,
            _ => {
                return Ok(Condition::Clause(Clause {
                    key,
                    op: Op::Truthy,
                    value: String::new(),
                }));
            }
        };
        self.advance();
        let value = match self.advance() {
            Some(Token::Word(w)) => literal(&w),
            Some(other) => {
                return Err(ConditionError(format!(
                    "expected a value after the operator, found {other:?}"
                )));
            }
            None if matches!(op, Op::Eq | Op::NotEq) => String::new(),
            None => return Err(ConditionError("expected a value after the operator".into())),
        };
        Ok(Condition::Clause(Clause { key, op, value }))
    }
}

/// A literal: quotes stripped and unescaped when present, else as written.
fn literal(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        trimmed[1..trimmed.len() - 1]
            .replace("\\\"", "\"")
            .replace("\\\\", "\\")
    } else {
        trimmed.to_string()
    }
}

/// Parse a condition. The empty condition is `And([])`: always true.
pub fn parse(text: &str) -> Result<Condition, ConditionError> {
    let tokens = tokenize(text)?;
    if tokens.is_empty() {
        return Ok(Condition::And(Vec::new()));
    }
    let mut parser = Parser { tokens, pos: 0 };
    let condition = parser.or()?;
    if parser.pos < parser.tokens.len() {
        return Err(ConditionError(format!(
            "unexpected {:?} in condition",
            parser.tokens[parser.pos]
        )));
    }
    Ok(condition)
}

/// The engine expressions the lowering builds on.
struct Builder<'a> {
    table:   &'a mut ExprTable,
    diags:   &'a mut Diagnostics,
    span:    &'a Span,
    ok:      bool,
    /// The node's failure policy is `succeed`: a failure it promoted is a
    /// partial status whose reported outcome is not `partially_succeeded`,
    /// and it reads as `succeeded`.
    succeed: bool,
}

impl Builder<'_> {
    fn error(&mut self, code: &str, message: String) {
        self.diags.error(code, self.span.clone(), message);
        self.ok = false;
    }

    /// Fabro's text form of a value: strings as they are, `null` as the empty
    /// string, everything else as compact JSON. `to_string(default(v, ""))`.
    fn text_of(&mut self, value: ExprId) -> ExprId {
        let empty = self.table.lit("");
        let defaulted = self.table.call("default", vec![value, empty]);
        self.table.call("to_string", vec![defaulted])
    }

    /// What a key names: the run-context value, the reported label, or a
    /// completed node's record.
    fn key_value(&mut self, key: &str) -> ExprId {
        if key == "preferred_label" {
            return self.table.path("output", &["preferred_label"]);
        }
        // `nodes.<id>.<field>`: the completed record of node `<id>` —
        // `generation` is the loop-guard family's counter (fabro-51ad:
        // this lowered onto a literal kv lookup that never exists, so a
        // `nodes.<id>.generation >= N` guard could never fire). The last
        // dot separates the field, so node names keep their own dots.
        if let Some(rest) = key.strip_prefix("nodes.")
            && let Some((node, field)) = rest.rsplit_once('.')
        {
            return self.table.path("nodes", &[node, field]);
        }
        let key = key.strip_prefix("context.").unwrap_or(key);
        let kv = self.table.var("kv");
        let name = self.table.lit(key);
        self.table.call("get", vec![kv, name])
    }

    fn status_is(&mut self, tag: &str) -> ExprId {
        let status = self.table.var("status");
        let want = self.table.lit(tag);
        self.table.binary(BinOp::Eq, status, want)
    }

    /// A partial status the `succeed` policy made out of a failure: the step
    /// reported anything but a genuine `partially_succeeded` (the step
    /// boundary says `succeeded`; exhausted retries leave `failed`).
    fn converted_failure(&mut self) -> ExprId {
        let partial = self.status_is("partial_success");
        let reported = self.table.path("output", &["outcome"]);
        let empty = self.table.lit("");
        let reported = self.table.call("default", vec![reported, empty]);
        let genuine = self.table.lit("partially_succeeded");
        let converted = self.table.binary(BinOp::Ne, reported, genuine);
        self.table.binary(BinOp::And, partial, converted)
    }

    /// `outcome=X`: `failed` covers every non-success terminal status the
    /// engine distinguishes, since Fabro folds them all into `failed`.
    fn outcome_is(&mut self, value: &str) -> Option<ExprId> {
        match value {
            "succeeded" if self.succeed => {
                let success = self.status_is("success");
                let converted = self.converted_failure();
                Some(self.table.binary(BinOp::Or, success, converted))
            }
            "partially_succeeded" if self.succeed => {
                let partial = self.status_is("partial_success");
                let converted = self.converted_failure();
                let genuine = self.table.unary(UnOp::Not, converted);
                Some(self.table.binary(BinOp::And, partial, genuine))
            }
            "succeeded" => Some(self.status_is("success")),
            "partially_succeeded" => Some(self.status_is("partial_success")),
            "skipped" => Some(self.status_is("skipped")),
            // REMOVE AFTER 2026-10-04: refuse the alias again.
            "success" => {
                self.diags.warning(
                    "deprecated.outcome_alias",
                    self.span.clone(),
                    format!(
                        "`outcome=success` is not a stage outcome (Fabro never matches it); read \
                         as `outcome=succeeded` until {COMPAT_SUNSET}"
                    ),
                );
                self.outcome_is("succeeded")
            }
            "failed" => {
                let failure = self.status_is("failure");
                let cancelled = self.status_is("cancelled");
                let timed_out = self.status_is("timed_out");
                let either = self.table.binary(BinOp::Or, failure, cancelled);
                Some(self.table.binary(BinOp::Or, either, timed_out))
            }
            other => {
                self.error(
                    "unsupported.outcome_value",
                    format!(
                        "`outcome={other}` names a value that is not a stage outcome; the \
                         outcomes are {}. A domain-specific routing signal belongs in \
                         `context_updates`, read as `context.{other}` or a bare `{other}`",
                        OUTCOMES.join(", ")
                    ),
                );
                None
            }
        }
    }

    fn clause(&mut self, clause: &Clause) -> ExprId {
        let falsy = self.table.lit(false);
        if clause.key == "outcome" {
            return match clause.op {
                Op::Eq => self.outcome_is(&clause.value).unwrap_or(falsy),
                Op::NotEq => match self.outcome_is(&clause.value) {
                    Some(is) => self.table.unary(UnOp::Not, is),
                    None => falsy,
                },
                // The outcome is never empty, so a bare `outcome` is true.
                Op::Truthy => self.table.lit(true),
                _ => {
                    self.error(
                        "attractor.condition.outcome_op",
                        "`outcome` supports only `=` and `!=`".into(),
                    );
                    falsy
                }
            };
        }
        let value = self.key_value(&clause.key);
        let text = self.text_of(value);
        match clause.op {
            Op::Truthy => {
                // Fabro: non-empty, not "false", not "0".
                let empty = self.table.lit("");
                let f = self.table.lit("false");
                let zero = self.table.lit("0");
                let not_empty = self.table.binary(BinOp::Ne, text, empty);
                let not_false = self.table.binary(BinOp::Ne, text, f);
                let not_zero = self.table.binary(BinOp::Ne, text, zero);
                let both = self.table.binary(BinOp::And, not_empty, not_false);
                self.table.binary(BinOp::And, both, not_zero)
            }
            Op::Eq => {
                let want = self.table.lit(clause.value.as_str());
                self.table.binary(BinOp::Eq, text, want)
            }
            Op::NotEq => {
                let want = self.table.lit(clause.value.as_str());
                self.table.binary(BinOp::Ne, text, want)
            }
            Op::Gt | Op::Lt | Op::Gte | Op::Lte => {
                // Both sides parse as numbers, or the clause is false. A
                // literal that is not a number makes the clause statically
                // false, which Fabro also does — but silently.
                let Ok(number) = clause.value.trim().parse::<f64>() else {
                    self.diags.warning(
                        "attractor.condition.non_numeric",
                        self.span.clone(),
                        format!(
                            "`{}` compares against `{}`, which is not a number, so the clause \
                             is always false",
                            clause.key, clause.value
                        ),
                    );
                    return falsy;
                };
                let empty = self.table.lit("");
                let present = self.table.binary(BinOp::Ne, text, empty);
                let lhs = self.table.call("loose_number", vec![text]);
                let rhs = self.table.lit(number);
                let function = match clause.op {
                    Op::Gt => "loose_gt",
                    Op::Lt => "loose_lt",
                    Op::Gte => "loose_ge",
                    _ => "loose_le",
                };
                let compare = self.table.call(function, vec![lhs, rhs]);
                self.table.binary(BinOp::And, present, compare)
            }
            Op::Contains => {
                // An array: element equality. Anything else: substring of the text.
                let needle = self.table.lit(clause.value.as_str());
                let empty = self.table.lit("");
                let defaulted = self.table.call("default", vec![value, empty]);
                let is_array = {
                    let json = self.table.call("to_json", vec![defaulted]);
                    let bracket = self.table.lit("[");
                    self.table.call("starts_with", vec![json, bracket])
                };
                let in_array = self.table.call("contains", vec![defaulted, needle]);
                let in_text = self.table.call("contains", vec![text, needle]);
                self.table.cond(is_array, in_array, in_text)
            }
            Op::Matches => {
                if let Err(error) = regex::Regex::new(&clause.value) {
                    self.error(
                        "attractor.condition.regex",
                        format!("`{}` is not a valid regex: {error}", clause.value),
                    );
                    return falsy;
                }
                let pattern = self.table.lit(clause.value.as_str());
                self.table.call("matches", vec![text, pattern])
            }
        }
    }

    fn condition(&mut self, condition: &Condition) -> ExprId {
        match condition {
            Condition::Clause(clause) => self.clause(clause),
            Condition::Not(inner) => {
                let inner = self.condition(inner);
                self.table.unary(UnOp::Not, inner)
            }
            Condition::And(children) => {
                let mut acc = self.table.lit(true);
                for (index, child) in children.iter().enumerate() {
                    let child = self.condition(child);
                    acc = if index == 0 {
                        child
                    } else {
                        self.table.binary(BinOp::And, acc, child)
                    };
                }
                acc
            }
            Condition::Or(children) => {
                let mut acc = self.table.lit(false);
                for (index, child) in children.iter().enumerate() {
                    let child = self.condition(child);
                    acc = if index == 0 {
                        child
                    } else {
                        self.table.binary(BinOp::Or, acc, child)
                    };
                }
                acc
            }
        }
    }
}

/// Parse and lower one condition. `None` means a diagnostic was emitted.
/// `succeed` says the node converts failures under the `succeed` policy, so
/// a converted failure reads as `succeeded`.
pub fn lower(
    text: &str,
    table: &mut ExprTable,
    span: &Span,
    diags: &mut Diagnostics,
    succeed: bool,
) -> Option<ExprId> {
    let condition = match parse(text) {
        Ok(condition) => condition,
        Err(error) => {
            diags.error(
                "attractor.condition.syntax",
                span.clone(),
                format!("condition `{text}`: {error}"),
            );
            return None;
        }
    };
    let mut builder = Builder {
        table,
        diags,
        span,
        ok: true,
        succeed,
    };
    let id = builder.condition(&condition);
    builder.ok.then_some(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clause(key: &str, op: Op, value: &str) -> Condition {
        Condition::Clause(Clause {
            key: key.into(),
            op,
            value: value.into(),
        })
    }

    #[test]
    fn parses_precedence_and_quotes() {
        assert_eq!(
            parse("a=1 && b=2 || c=3").expect("parses"),
            Condition::Or(vec![
                Condition::And(vec![clause("a", Op::Eq, "1"), clause("b", Op::Eq, "2")]),
                clause("c", Op::Eq, "3"),
            ])
        );
        assert_eq!(
            parse(r#"outcome="succeeded""#).expect("parses"),
            clause("outcome", Op::Eq, "succeeded")
        );
        assert_eq!(
            parse("!flag").expect("parses"),
            Condition::Not(Box::new(clause("flag", Op::Truthy, "")))
        );
        assert_eq!(
            parse("x contains y").expect("parses"),
            clause("x", Op::Contains, "y")
        );
        assert_eq!(
            parse("missing=").expect("parses"),
            clause("missing", Op::Eq, "")
        );
        assert_eq!(parse("").expect("parses"), Condition::And(Vec::new()));
    }

    #[test]
    fn rejects_syntax_errors() {
        assert!(parse("=x").is_err());
        assert!(parse("a > ").is_err());
        assert!(parse("a=1 b=2").is_err());
        assert!(parse("\"open").is_err());
    }
}
