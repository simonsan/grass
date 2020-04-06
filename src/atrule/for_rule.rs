use std::iter::Peekable;

use num_traits::cast::ToPrimitive;

use super::parse::eat_stmts;
use super::AtRule;

use crate::error::SassResult;
use crate::scope::Scope;
use crate::selector::Selector;
use crate::unit::Unit;
use crate::utils::{
    devour_whitespace, eat_ident, read_until_closing_curly_brace, read_until_open_curly_brace,
};
use crate::value::{Number, Value};
use crate::Token;

pub(crate) fn parse_for<I: Iterator<Item = Token>>(
    toks: &mut Peekable<I>,
    scope: &mut Scope,
    super_selector: &Selector,
) -> SassResult<AtRule> {
    let mut stmts = Vec::new();
    devour_whitespace(toks);
    let var = match toks.next().ok_or("expected \"$\".")?.kind {
        '$' => eat_ident(toks, scope, super_selector)?,
        _ => return Err("expected \"$\".".into()),
    };
    devour_whitespace(toks);
    if toks.peek().is_none()
        || eat_ident(toks, scope, super_selector)?.to_ascii_lowercase() != "from"
    {
        return Err("Expected \"from\".".into());
    }
    devour_whitespace(toks);
    let mut from_toks = Vec::new();
    let mut through = 0;
    while let Some(tok) = toks.next() {
        let mut these_toks = vec![tok];
        match these_toks[0].kind.to_ascii_lowercase() {
            't' => {
                these_toks.push(toks.next().unwrap());
                match these_toks[1].kind.to_ascii_lowercase() {
                    'h' => {
                        let r = toks.next().unwrap();
                        these_toks.push(r);
                        if r.kind != 'r' {
                            from_toks.extend(these_toks);
                            continue;
                        }
                        let o = toks.next().unwrap();
                        these_toks.push(o);
                        if o.kind != 'o' {
                            from_toks.extend(these_toks);
                            continue;
                        }
                        let u = toks.next().unwrap();
                        these_toks.push(u);
                        if u.kind != 'u' {
                            from_toks.extend(these_toks);
                            continue;
                        }
                        let g = toks.next().unwrap();
                        these_toks.push(g);
                        if g.kind != 'g' {
                            from_toks.extend(these_toks);
                            continue;
                        }
                        let h = toks.next().unwrap();
                        these_toks.push(h);
                        if h.kind != 'h' {
                            from_toks.extend(these_toks);
                            continue;
                        }
                        let peek = toks.peek().unwrap().kind;
                        if peek.is_alphanumeric() || peek == '\\' {
                            from_toks.extend(these_toks);
                            continue;
                        }
                        through = 1;
                        break;
                    }
                    'o' => {
                        if toks.peek().unwrap().kind.is_whitespace() {
                            break;
                        } else {
                            from_toks.extend(these_toks);
                        }
                    }
                    _ => {
                        from_toks.extend(these_toks);
                    }
                }
            }
            '{' => {
                return Err("Expected \"to\" or \"through\".".into());
            }
            _ => from_toks.extend(these_toks),
        }
    }
    let from =
        match Value::from_tokens(&mut from_toks.into_iter().peekable(), scope, super_selector)? {
            Value::Dimension(n, _) => match n.to_integer().to_usize() {
                Some(v) => v,
                None => return Err(format!("{} is not a int.", n).into()),
            },
            v => return Err(format!("{} is not an integer.", v).into()),
        };
    devour_whitespace(toks);
    let to_toks = read_until_open_curly_brace(toks);
    toks.next();
    let to = match Value::from_tokens(&mut to_toks.into_iter().peekable(), scope, super_selector)? {
        Value::Dimension(n, _) => match n.to_integer().to_usize() {
            Some(v) => v,
            None => return Err(format!("{} is not a int.", n).into()),
        },
        v => return Err(format!("{} is not an integer.", v).into()),
    };
    let body = read_until_closing_curly_brace(toks);
    toks.next();

    devour_whitespace(toks);

    let (mut x, mut y);
    let iter: &mut dyn std::iter::Iterator<Item = usize> = if from < to {
        x = from..(to + through);
        &mut x
    } else {
        y = ((to - through)..(from + 1)).skip(1).rev();
        &mut y
    };

    for i in iter {
        scope.insert_var(&var, Value::Dimension(Number::from(i), Unit::None))?;
        stmts.extend(eat_stmts(
            &mut body.clone().into_iter().peekable(),
            scope,
            super_selector,
        )?);
    }

    Ok(AtRule::For(stmts))
}