use std::collections::HashMap;

pub(crate) fn parse_env_file(text: &str) -> HashMap<String, String> {
    let mut values = HashMap::new();
    let mut lines = text.lines();

    while let Some(line) = lines.next() {
        let line = line.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let line = line
            .strip_prefix("export ")
            .map(str::trim_start)
            .unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }

        values.insert(
            key.to_string(),
            parse_env_value(value.trim_start(), &mut lines),
        );
    }

    values
}

fn parse_env_value<'a>(value: &str, lines: &mut impl Iterator<Item = &'a str>) -> String {
    if let Some(rest) = value.strip_prefix('"') {
        parse_quoted_env_value(rest, '"', lines)
    } else if let Some(rest) = value.strip_prefix('\'') {
        parse_quoted_env_value(rest, '\'', lines)
    } else {
        parse_unquoted_env_value(value)
    }
}

fn parse_quoted_env_value<'a>(
    first: &str,
    quote: char,
    lines: &mut impl Iterator<Item = &'a str>,
) -> String {
    let mut value = String::new();
    let mut current = first;
    let mut escaped = false;

    loop {
        for ch in current.chars() {
            if escaped {
                value.push(match ch {
                    'n' if quote == '"' => '\n',
                    'r' if quote == '"' => '\r',
                    't' if quote == '"' => '\t',
                    other => other,
                });
                escaped = false;
                continue;
            }

            if quote == '"' && ch == '\\' {
                escaped = true;
                continue;
            }

            if ch == quote {
                return value;
            }

            value.push(ch);
        }

        let Some(next) = lines.next() else {
            return value;
        };
        value.push('\n');
        current = next;
    }
}

fn parse_unquoted_env_value(value: &str) -> String {
    let mut output = String::new();
    let mut escaped = false;
    let mut previous_was_whitespace = false;

    for ch in value.chars() {
        if escaped {
            output.push(ch);
            previous_was_whitespace = ch.is_whitespace();
            escaped = false;
            continue;
        }

        if ch == '\\' {
            escaped = true;
            continue;
        }

        if ch == '#' && previous_was_whitespace {
            break;
        }

        output.push(ch);
        previous_was_whitespace = ch.is_whitespace();
    }

    output.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::parse_env_file;

    #[test]
    fn env_file_parser_handles_compose_dotenv_forms() {
        let values = parse_env_file(concat!(
            "# comment\n",
            "export REGISTRY=example.com # registry host\n",
            "TAG=\"1.2.3\"\n",
            "LITERAL_HASH=#literal\n",
            "SUFFIX=foo\\#bar # comment\n",
            "HASHED='pa#ss'\n",
            "QUOTED=\"hello \\\"there\\\"\"\n",
            "MULTILINE=\"line1\n",
            "line2\"\n",
        ));

        assert_eq!(
            values.get("REGISTRY").map(String::as_str),
            Some("example.com")
        );
        assert_eq!(values.get("TAG").map(String::as_str), Some("1.2.3"));
        assert_eq!(
            values.get("LITERAL_HASH").map(String::as_str),
            Some("#literal")
        );
        assert_eq!(values.get("SUFFIX").map(String::as_str), Some("foo#bar"));
        assert_eq!(values.get("HASHED").map(String::as_str), Some("pa#ss"));
        assert_eq!(
            values.get("QUOTED").map(String::as_str),
            Some("hello \"there\"")
        );
        assert_eq!(
            values.get("MULTILINE").map(String::as_str),
            Some("line1\nline2")
        );
    }
}
