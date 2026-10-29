//! Pseudo-attributes on a processing instruction.
//!
//! A processing instruction's data is a list of `name = "value"` pairs. The
//! DOM exposes them as an ordered map and reserializes the whole map when it
//! changes; the document also loads style sheets from them for
//! `xml-stylesheet`, so the parse has to have exactly one answer for both
//! consumers
//! (<https://www.w3.org/TR/xml-stylesheet/#dt-parsing>).
//!
//! Parsing follows that grammar strictly: a duplicate name, `<` in a value, a
//! malformed reference, or a code point XML excludes is a parse error and the
//! whole map is empty. The map is derived from the data on demand, so no write
//! to the data can leave it stale.

use xml5ever::names::{is_char, is_name, is_name_char, is_name_start_char, is_space};

/// Parses `data` into an ordered attribute list.
///
/// `None` is a parse error: the caller uses an empty map and loads nothing
/// (<https://www.w3.org/TR/xml-stylesheet/#dt-parsing>).
pub fn parse(data: &str) -> Option<Vec<(String, String)>> {
    let mut chars = data.chars().peekable();
    let mut attributes = Vec::new();
    if chars.peek().is_none() {
        return Some(attributes);
    }
    loop {
        let name = parse_name(&mut chars)?;
        skip_space(&mut chars);
        if chars.next() != Some('=') {
            return None;
        }
        skip_space(&mut chars);
        let value = parse_value(&mut chars)?;
        if attributes.iter().any(|(existing, _)| existing == &name) {
            return None;
        }
        attributes.push((name, value));
        if chars.peek().is_none() {
            return Some(attributes);
        }
        if !take_space(&mut chars) {
            return None;
        }
        if chars.peek().is_none() {
            return Some(attributes);
        }
    }
}

/// Serializes an ordered attribute map. Escaping `&` first keeps the
/// references it inserts from being escaped again
/// (<https://dom.spec.whatwg.org/#update-data-from-attributes>).
pub fn serialize(attributes: &[(String, String)]) -> String {
    let mut data = String::new();
    for (index, (name, value)) in attributes.iter().enumerate() {
        if index > 0 {
            data.push(' ');
        }
        data.push_str(name);
        data.push('=');
        data.push('"');
        for character in value.chars() {
            match character {
                '&' => data.push_str("&amp;"),
                '<' => data.push_str("&lt;"),
                '>' => data.push_str("&gt;"),
                '"' => data.push_str("&quot;"),
                _ => data.push(character),
            }
        }
        data.push('"');
    }
    data
}

fn parse_name(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<String> {
    let first = chars.next()?;
    if !is_name_start_char(first) {
        return None;
    }
    let mut name = String::new();
    name.push(first);
    while chars
        .peek()
        .is_some_and(|character| is_name_char(*character))
    {
        name.push(chars.next()?);
    }
    if !is_name(&name) {
        return None;
    }
    Some(name)
}

fn parse_value(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<String> {
    let quote = chars.next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let mut value = String::new();
    loop {
        match chars.next()? {
            character if character == quote => return Some(value),
            '<' => return None,
            '&' => value.push(parse_reference(chars)?),
            character if is_char(character) => value.push(character),
            _ => return None,
        }
    }
}

fn parse_reference(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<char> {
    let mut token = String::new();
    loop {
        match chars.next()? {
            ';' => break,
            character
                if token.len() < 16 && (character.is_ascii_alphanumeric() || character == '#') =>
            {
                token.push(character);
            }
            _ => return None,
        }
    }
    let character = match token.as_str() {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        other => {
            let code = if let Some(hex) = other
                .strip_prefix("#x")
                .or_else(|| other.strip_prefix("#X"))
            {
                u32::from_str_radix(hex, 16).ok()?
            } else {
                let digits = other.strip_prefix('#')?;
                if digits.is_empty() || !digits.chars().all(|digit| digit.is_ascii_digit()) {
                    return None;
                }
                digits.parse().ok()?
            };
            char::from_u32(code).filter(|&character| is_char(character))?
        }
    };
    Some(character)
}

fn skip_space(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while chars.peek().is_some_and(|character| is_space(*character)) {
        chars.next();
    }
}

fn take_space(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> bool {
    if !chars.peek().is_some_and(|character| is_space(*character)) {
        return false;
    }
    skip_space(chars);
    true
}

#[cfg(test)]
mod tests {
    use super::{parse, serialize};

    #[test]
    fn parses_the_stylesheet_pseudo_attributes() {
        assert_eq!(
            parse(r#"href="a.css" type='text/css'"#),
            Some(vec![
                ("href".to_owned(), "a.css".to_owned()),
                ("type".to_owned(), "text/css".to_owned()),
            ])
        );
        assert_eq!(parse(""), Some(Vec::new()));
    }

    #[test]
    fn a_parse_error_leaves_no_map() {
        // Duplicate names, `<`, bad references, and a non-`Char` code point
        // are parse errors for the whole map.
        assert_eq!(parse(r#"href="a.css" href="b.css""#), None);
        assert_eq!(parse(r#"href="a<b.css""#), None);
        assert_eq!(parse(r#"href="a&b.css""#), None);
        assert_eq!(parse("href=\"\u{1}\""), None);
        // Form feed is not XML whitespace.
        assert_eq!(parse("href=\"a.css\"\u{C}type=\"text/css\""), None);
    }

    #[test]
    fn round_trips_through_the_serializer() {
        let attributes = vec![("href".to_owned(), "a&b<c>\".css".to_owned())];
        let data = serialize(&attributes);
        assert_eq!(parse(&data), Some(attributes));
    }
}
