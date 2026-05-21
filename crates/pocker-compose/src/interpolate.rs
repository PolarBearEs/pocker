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
            let expr = collect_braced_expression(&mut chars)?;
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

fn collect_braced_expression(
    chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
) -> Result<String> {
    // Keep the expression body raw; operator extras are intentionally handed
    // back to `interpolate` later so bare $VAR and $$ use the same rules.
    let mut expr = String::new();
    let mut nested_depth = 0usize;

    while let Some((_, ch)) = chars.next() {
        if ch == '$' && chars.peek().is_some_and(|(_, next)| *next == '{') {
            chars.next();
            nested_depth += 1;
            expr.push_str("${");
            continue;
        }

        if ch == '}' {
            if nested_depth == 0 {
                return Ok(expr);
            }
            nested_depth -= 1;
        }

        expr.push(ch);
    }

    Err(ComposeError::InvalidInput(
        "unterminated compose variable interpolation".into(),
    ))
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
        ":-" if !non_empty => interpolate(extra, values),
        "-" if !set => interpolate(extra, values),
        ":?" if !non_empty => Err(ComposeError::InvalidInput(format!(
            "compose variable `{name}` is required: {}",
            interpolate(extra, values)?
        ))),
        "?" if !set => Err(ComposeError::InvalidInput(format!(
            "compose variable `{name}` is required: {}",
            interpolate(extra, values)?
        ))),
        ":+" if non_empty => interpolate(extra, values),
        "+" if set => interpolate(extra, values),
        _ => Ok(value.cloned().unwrap_or_default()),
    }
}
