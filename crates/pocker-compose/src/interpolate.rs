use std::collections::HashMap;

use crate::{ComposeError, Result};

pub(super) fn interpolate(text: &str, values: &HashMap<String, String>) -> Result<String> {
    let mut output = String::with_capacity(text.len());
    let mut chars = text.char_indices().peekable();
    while let Some((_, ch)) = chars.next() {
        if ch != '$' {
            output.push(ch);
            continue;
        }
        let Some((_, next)) = chars.peek().copied() else {
            output.push('$');
            continue;
        };
        if next == '$' {
            chars.next();
            output.push('$');
            continue;
        }
        if next == '{' {
            chars.next();
            let mut expr = String::new();
            let mut closed = false;
            for (_, expr_ch) in chars.by_ref() {
                if expr_ch == '}' {
                    closed = true;
                    break;
                }
                expr.push(expr_ch);
            }
            if !closed {
                return Err(ComposeError::InvalidInput(
                    "unterminated compose variable interpolation".into(),
                ));
            }
            output.push_str(&resolve_variable(&expr, values)?);
            continue;
        }
        if !is_compose_variable_start(next) {
            output.push('$');
            continue;
        }
        let mut expr = String::new();
        while let Some((_, var_ch)) = chars.peek().copied() {
            if !is_compose_variable_char(var_ch) {
                break;
            }
            chars.next();
            expr.push(var_ch);
        }
        output.push_str(&resolve_variable(&expr, values)?);
    }
    Ok(output)
}

fn is_compose_variable_start(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphabetic()
}

fn is_compose_variable_char(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric()
}

fn resolve_variable(expr: &str, values: &HashMap<String, String>) -> Result<String> {
    let operators = [":?", "?", ":-", ":+", "-", "+"];
    for operator in operators {
        if let Some((name, extra)) = expr.split_once(operator) {
            return resolve_variable_with_operator(name, operator, extra, values);
        }
    }
    Ok(values.get(expr).cloned().unwrap_or_default())
}

fn resolve_variable_with_operator(
    name: &str,
    operator: &str,
    extra: &str,
    values: &HashMap<String, String>,
) -> Result<String> {
    let value = values.get(name);
    let set = value.is_some();
    let non_empty = value.is_some_and(|value| !value.is_empty());
    match operator {
        ":-" if !non_empty => Ok(extra.to_string()),
        "-" if !set => Ok(extra.to_string()),
        ":?" if !non_empty => Err(ComposeError::InvalidInput(format!(
            "compose variable `{name}` is required: {extra}"
        ))),
        "?" if !set => Err(ComposeError::InvalidInput(format!(
            "compose variable `{name}` is required: {extra}"
        ))),
        ":+" if non_empty => Ok(extra.to_string()),
        "+" if set => Ok(extra.to_string()),
        _ => Ok(value.cloned().unwrap_or_default()),
    }
}
