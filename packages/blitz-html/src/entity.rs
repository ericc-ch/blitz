//! Internal general entity expansion for the XML frontend.
//!
//! xml5ever tokenizes but never defines entities: without this step every
//! `&name;` stays literal text. Replacement text that contains markup is
//! spliced into the source so the XML parser builds the corresponding nodes
//! (<https://www.w3.org/TR/xml/#intern-replacement>).
//!
//! Includes internal general entities in the document instance. External and
//! parameter entities stay unresolved.
//!
//! Billion-laughs protection: entity expansion is bounded three ways.
//! Declaration count caps HashMap memory; depth caps the call stack
//! (`expand_replacement` recurses per nesting level); the byte budget caps
//! total output (a doubling chain otherwise turns kilobytes into gigabytes).
//! Exhaustion degrades gracefully: references stay literal text.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

const MAX_ENTITY_DECLS: usize = 10_000;
const MAX_ENTITY_DEPTH: usize = 64;
const MAX_EXPANDED_BYTES: usize = 32 * 1024 * 1024;
const MAX_ENTITY_OVERHEAD: usize = 1024;

/// Expands internal general entities declared in the document type's
/// internal subset, returning the input borrowed when there is nothing to
/// expand.
pub(crate) fn expand_internal_general_entities(input: &str) -> Cow<'_, str> {
    let Some((body_start, entities)) = scan_internal_general_entities(input) else {
        return Cow::Borrowed(input);
    };
    if entities.is_empty() {
        return Cow::Borrowed(input);
    }
    let mut output = String::with_capacity(input.len());
    output.push_str(&input[..body_start]);
    let mut budget = MAX_EXPANDED_BYTES;
    expand_content(&input[body_start..], &entities, &mut output, &mut budget);
    Cow::Owned(output)
}

fn scan_internal_general_entities(input: &str) -> Option<(usize, HashMap<String, String>)> {
    let doctype = input.find("<!DOCTYPE")?;
    let after_keyword = doctype + "<!DOCTYPE".len();
    let mut quote = None;
    let mut subset_start = None;
    for (index, character) in input[after_keyword..].char_indices() {
        if let Some(open) = quote {
            if character == open {
                quote = None;
            }
            continue;
        }
        match character {
            '"' | '\'' => quote = Some(character),
            '[' => {
                subset_start = Some(after_keyword + index + 1);
                break;
            }
            '>' => return None,
            _ => {}
        }
    }
    let subset_start = subset_start?;
    let subset = &input[subset_start..];
    let mut quote = None;
    let mut depth = 0usize;
    let mut end = None;
    for (index, character) in subset.char_indices() {
        if let Some(open) = quote {
            if character == open {
                quote = None;
            }
            continue;
        }
        match character {
            '"' | '\'' => quote = Some(character),
            '[' => depth += 1,
            ']' => {
                if depth == 0 {
                    end = Some(index);
                    break;
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    let end = end?;
    let after_bracket = subset_start + end + 1;
    let mut body_start = after_bracket;
    while body_start < input.len() {
        let rest = &input[body_start..];
        if rest.starts_with('>') {
            body_start += 1;
            break;
        }
        let Some(next) = rest.chars().next() else {
            break;
        };
        body_start += next.len_utf8();
    }
    Some((
        body_start,
        parse_internal_general_entities(&input[subset_start..subset_start + end]),
    ))
}

fn parse_internal_general_entities(subset: &str) -> HashMap<String, String> {
    let mut entities = HashMap::new();
    let mut pos = 0;
    while pos < subset.len() {
        let rest = &subset[pos..];
        if let Some(inside) = rest.strip_prefix("<!--") {
            match inside.find("-->") {
                Some(end) => pos += 4 + end + 3,
                None => break,
            }
            continue;
        }
        if let Some(decl) = rest.strip_prefix("<!ENTITY") {
            let (consumed, entity) = parse_general_entity_decl(decl);
            if let Some((name, value)) = entity {
                // Beyond the cap further declarations are ignored: their
                // references stay literal, exactly like undefined entities.
                if entities.len() < MAX_ENTITY_DECLS {
                    entities.insert(name, value);
                }
            }
            pos += "<!ENTITY".len() + consumed;
            continue;
        }
        if rest.starts_with('<') {
            pos += skip_markup(rest);
            continue;
        }
        let Some(next) = rest.chars().next() else {
            break;
        };
        pos += next.len_utf8();
    }
    entities
}

fn parse_general_entity_decl(decl: &str) -> (usize, Option<(String, String)>) {
    let consumed = skip_decl_body(decl);
    let body = &decl[..consumed];
    let mut chars = body.chars().peekable();
    while chars.peek().is_some_and(|c| is_xml_whitespace(*c)) {
        chars.next();
    }
    if chars.peek() == Some(&'%') {
        return (consumed, None);
    }
    let mut name = String::new();
    while let Some(c) = chars.peek().copied() {
        if is_xml_whitespace(c) {
            break;
        }
        name.push(c);
        chars.next();
    }
    if name.is_empty() || !is_valid_name(&name) {
        return (consumed, None);
    }
    while chars.peek().is_some_and(|c| is_xml_whitespace(*c)) {
        chars.next();
    }
    if matches!(chars.peek(), Some('S' | 'P')) {
        return (consumed, None);
    }
    let Some(quote) = chars.next() else {
        return (consumed, None);
    };
    if quote != '"' && quote != '\'' {
        return (consumed, None);
    }
    let mut value = String::new();
    for c in chars.by_ref() {
        if c == quote {
            return (consumed, Some((name, value)));
        }
        value.push(c);
    }
    (consumed, None)
}

/// Copies `text` into `output` while budget remains. Returns false when the
/// budget ran out, telling the caller to copy the rest literally and stop
/// expanding.
fn push_budgeted(output: &mut String, text: &str, budget: &mut usize) -> bool {
    let take = (*budget).min(text.len());
    output.push_str(&text[..take]);
    *budget -= take;
    take == text.len()
}

fn expand_content(
    content: &str,
    entities: &HashMap<String, String>,
    output: &mut String,
    budget: &mut usize,
) {
    let mut pos = 0;
    while pos < content.len() {
        let rest = &content[pos..];
        if rest.starts_with("<!--")
            || rest.starts_with("<![CDATA[")
            || rest.starts_with("<?")
            || rest.starts_with('<')
        {
            let consumed = if rest.starts_with("<!--") {
                rest.find("-->").map_or(rest.len(), |end| end + 3)
            } else if rest.starts_with("<![CDATA[") {
                rest.find("]]>").map_or(rest.len(), |end| end + 3)
            } else if rest.starts_with("<?") {
                rest.find("?>").map_or(rest.len(), |end| end + 2)
            } else {
                skip_markup(rest)
            };
            if !push_budgeted(output, &rest[..consumed], budget) {
                // Budget gone: the rest of the document passes through
                // unexpanded rather than growing without bound.
                output.push_str(&content[pos + consumed..]);
                return;
            }
            pos += consumed;
            continue;
        }
        if rest.starts_with('&') {
            if let Some(consumed) = expand_entity_ref(rest, entities, output, budget, 0) {
                pos += consumed;
            } else {
                if !push_budgeted(output, "&", budget) {
                    output.push_str(&content[pos + 1..]);
                    return;
                }
                pos += 1;
            }
            continue;
        }
        let next = rest.find(['<', '&']).unwrap_or(rest.len());
        if !push_budgeted(output, &rest[..next], budget) {
            output.push_str(&content[pos + next..]);
            return;
        }
        pos += next;
    }
}

fn expand_entity_ref(
    rest: &str,
    entities: &HashMap<String, String>,
    output: &mut String,
    budget: &mut usize,
    depth: usize,
) -> Option<usize> {
    let body = rest.strip_prefix('&')?;
    if body.starts_with('#') {
        return None;
    }
    let name_end = body.find(';')?;
    let name = &body[..name_end];
    if !is_valid_name(name) {
        return None;
    }
    let value = entities.get(name)?.clone();
    let mut seen = HashSet::new();
    seen.insert(name.to_owned());
    expand_replacement(&value, entities, output, budget, &mut seen, depth);
    Some(name_end + 2)
}

fn expand_replacement(
    value: &str,
    entities: &HashMap<String, String>,
    output: &mut String,
    budget: &mut usize,
    seen: &mut HashSet<String>,
    depth: usize,
) {
    // The value is cloned at the call site so `entities` is never borrowed
    // while `output` grows; replacement text only ever appends.
    let mut pos = 0;
    while pos < value.len() {
        if *budget == 0 {
            return;
        }
        let rest = &value[pos..];
        if let Some(amp) = rest.find('&') {
            if !push_budgeted(output, &rest[..amp], budget) {
                return;
            }
            let ref_rest = &rest[amp..];
            if let Some(consumed) =
                expand_nested_ref(ref_rest, entities, output, budget, seen, depth)
            {
                pos += amp + consumed;
            } else {
                if !push_budgeted(output, "&", budget) {
                    return;
                }
                pos += amp + 1;
            }
        } else {
            push_budgeted(output, rest, budget);
            break;
        }
    }
}

fn expand_nested_ref(
    rest: &str,
    entities: &HashMap<String, String>,
    output: &mut String,
    budget: &mut usize,
    seen: &mut HashSet<String>,
    depth: usize,
) -> Option<usize> {
    let body = rest.strip_prefix('&')?;
    if body.starts_with('#') {
        return None;
    }
    let name_end = body.find(';')?;
    let name = &body[..name_end];
    // Depth caps the call stack; the `seen` set still rejects cycles below it.
    // The budget covers output bytes (via `push_budgeted`), but each nested
    // expansion also costs traversal work (HashSet insert/remove plus string
    // allocation per reference), so every expansion prepays a flat overhead:
    // without it an attacker trades bounded output for unbounded CPU.
    if *budget == 0
        || depth >= MAX_ENTITY_DEPTH
        || !is_valid_name(name)
        || !seen.insert(name.to_owned())
    {
        return None;
    }
    *budget = budget.saturating_sub(MAX_ENTITY_OVERHEAD);
    let value = entities.get(name)?.clone();
    expand_replacement(&value, entities, output, budget, seen, depth + 1);
    seen.remove(name);
    Some(name_end + 2)
}

fn skip_decl_body(decl: &str) -> usize {
    let mut quote = None;
    for (index, character) in decl.char_indices() {
        if let Some(open) = quote {
            if character == open {
                quote = None;
            }
            continue;
        }
        match character {
            '"' | '\'' => quote = Some(character),
            '>' => return index + character.len_utf8(),
            _ => {}
        }
    }
    decl.len()
}

fn skip_markup(rest: &str) -> usize {
    let mut quote = None;
    for (index, character) in rest.char_indices().skip(1) {
        if let Some(open) = quote {
            if character == open {
                quote = None;
            }
            continue;
        }
        match character {
            '"' | '\'' => quote = Some(character),
            '>' => return index + character.len_utf8(),
            _ => {}
        }
    }
    rest.len()
}

fn is_xml_whitespace(character: char) -> bool {
    matches!(character, ' ' | '\t' | '\r' | '\n')
}

/// Whether `name` matches the XML `Name` production
/// (<https://www.w3.org/TR/xml/#NT-Name>).
fn is_valid_name(name: &str) -> bool {
    let mut characters = name.chars();
    match characters.next() {
        Some(first) if is_name_start(first) => characters.all(is_name_char),
        _ => false,
    }
}

// https://www.w3.org/TR/xml/#NT-NameStartChar
fn is_name_start(character: char) -> bool {
    matches!(character, ':' | 'A'..='Z' | '_' | 'a'..='z' | '\u{C0}'..='\u{D6}' | '\u{D8}'..='\u{F6}' | '\u{F8}'..='\u{2FF}' | '\u{370}'..='\u{37D}' | '\u{37F}'..='\u{1FFF}' | '\u{200C}'..='\u{200D}' | '\u{2070}'..='\u{218F}' | '\u{2C00}'..='\u{2FEF}' | '\u{3001}'..='\u{D7FF}' | '\u{F900}'..='\u{FDCF}' | '\u{FDF0}'..='\u{FFFD}')
        || ('\u{10000}'..='\u{EFFFF}').contains(&character)
}

// https://www.w3.org/TR/xml/#NT-NameChar
fn is_name_char(character: char) -> bool {
    is_name_start(character)
        || matches!(character, '-' | '.' | '0'..='9' | '\u{B7}' | '\u{0300}'..='\u{036F}' | '\u{203F}'..='\u{2040}')
}

#[cfg(test)]
mod tests {
    use super::{MAX_EXPANDED_BYTES, expand_internal_general_entities};

    fn doctype_with(decls: &str, body: &str) -> String {
        format!("<!DOCTYPE r [{decls}]><r>{body}</r>")
    }

    #[test]
    fn legitimate_entities_still_expand() {
        let input = doctype_with("<!ENTITY name \"world\">", "hello &name;!");
        let expanded = expand_internal_general_entities(&input);
        assert!(
            expanded.contains("hello world!"),
            "unexpected expansion: {expanded:?}"
        );
    }

    #[test]
    fn entity_cycles_stay_literal() {
        let input = doctype_with(
            "<!ENTITY a \"&b;\"><!ENTITY b \"&a;\">",
            "&a;",
        );
        let expanded = expand_internal_general_entities(&input);
        assert!(
            expanded.len() < 1024,
            "cycle exploded: {} bytes",
            expanded.len()
        );
    }

    // Ten doubling levels turn ~100 bytes into ~100KB legitimately; without
    // a budget the same shape at depth 30+ exhausts memory (billion laughs).
    #[test]
    fn exponential_entity_chain_stays_bounded() {
        let mut decls = String::from("<!ENTITY a0 \"x\">");
        for level in 1..30 {
            decls.push_str(&format!(
                "<!ENTITY a{level} \"&a{};&a{};\">",
                level - 1,
                level - 1
            ));
        }
        let input = doctype_with(&decls, "&a29;");
        let start = std::time::Instant::now();
        let expanded = expand_internal_general_entities(&input);
        let elapsed = start.elapsed();
        assert!(
            expanded.len() <= input.len() + MAX_EXPANDED_BYTES,
            "expansion escaped the budget: {} bytes",
            expanded.len()
        );
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "expansion took too long: {elapsed:?}"
        );
    }
}
