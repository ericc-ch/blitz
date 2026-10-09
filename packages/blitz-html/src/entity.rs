//! Internal general entity expansion for the XML frontend.
//!
//! xml5ever tokenizes but never defines entities: without this step every
//! `&name;` stays literal text. Replacement text that contains markup is
//! spliced into the source so the XML parser builds the corresponding nodes
//! (<https://www.w3.org/TR/xml/#intern-replacement>).
//!
//! Includes internal general entities in the document instance. External and
//! parameter entities stay unresolved: referencing one is a well-formedness
//! violation and surfaces as a parse error (see `errors` out-param).
//!
//! Billion-laughs protection: entity expansion is bounded three ways.
//! Declaration count caps HashMap memory; depth caps the call stack
//! (`expand_replacement` recurses per nesting level); the byte budget caps
//! total output (a doubling chain otherwise turns kilobytes into gigabytes).
//! Exhaustion surfaces one parse error and leaves the rest literal so the
//! document fails closed into a `parsererror` instead of a silently wrong
//! tree.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

const MAX_ENTITY_DECLS: usize = 10_000;
const MAX_ENTITY_DEPTH: usize = 64;
const MAX_EXPANDED_BYTES: usize = 32 * 1024 * 1024;
const MAX_ENTITY_OVERHEAD: usize = 1024;

/// Expands internal general entities declared in the document type's
/// internal subset, returning the input borrowed when there is no subset to
/// expand. Parse errors (undefined references, budget exhaustion) are
/// collected for the sink to drain into a `parsererror` document
/// (<https://www.w3.org/TR/xml/#sec-references>).
pub(crate) fn expand_internal_general_entities_with_errors(
    input: &str,
) -> (Cow<'_, str>, Vec<String>) {
    let mut errors = Vec::new();
    let Some((body_start, entities)) = scan_internal_general_entities(input) else {
        return (Cow::Borrowed(input), errors);
    };
    let mut output = String::with_capacity(input.len());
    output.push_str(&input[..body_start]);
    let mut budget = MAX_EXPANDED_BYTES;
    expand_content(
        &input[body_start..],
        &entities,
        &mut output,
        &mut budget,
        &mut errors,
    );
    if budget == 0 && !errors.iter().any(|e| e.contains("budget")) {
        errors.push("entity expansion budget exhausted".to_string());
    }
    (Cow::Owned(output), errors)
}

fn find_doctype_keyword(input: &str) -> Option<usize> {
    // Case-insensitive to match the tokenizer's `eat` (which uses
    // `eq_ignore_ascii_case`): the XML spec wants uppercase-only
    // `DOCTYPE` (<https://www.w3.org/TR/xml/#NT-doctypedecl>), but both
    // layers stay lenient together so entity expansion and tokenizing agree.
    let upper = "<!DOCTYPE";
    if input.len() < upper.len() {
        return None;
    }
    input
        .as_bytes()
        .windows(upper.len())
        .position(|w| w.eq_ignore_ascii_case(upper.as_bytes()))
}

fn scan_internal_general_entities(input: &str) -> Option<(usize, HashMap<String, String>)> {
    let doctype = find_doctype_keyword(input)?;
    let after_keyword = doctype + "<!DOCTYPE".len();
    // `<!DOCTYPE` requires whitespace before the name
    // (<https://www.w3.org/TR/xml/#NT-doctypedecl>).
    if !input[after_keyword..]
        .chars()
        .next()
        .is_some_and(is_xml_whitespace)
    {
        return None;
    }
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
    let end = find_subset_end(subset)?;
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

/// Finds the `]` ending the internal subset, skipping quoted literals,
/// comments, and processing instructions. The first `]` outside those ends
/// the subset; `[` nesting is not tracked (internal-subset markup cannot
/// contain a bare `[` outside a literal).
fn find_subset_end(subset: &str) -> Option<usize> {
    let bytes = subset.as_bytes();
    let mut i = 0;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
            if b == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        if b == b'"' || b == b'\'' {
            quote = Some(b);
            i += 1;
            continue;
        }
        if subset[i..].starts_with("<!--") {
            match subset[i..].find("-->") {
                Some(end) => {
                    i += end + 3;
                    continue;
                }
                None => return None,
            }
        }
        if subset[i..].starts_with("<?") {
            match subset[i..].find("?>") {
                Some(end) => {
                    i += end + 2;
                    continue;
                }
                None => return None,
            }
        }
        if b == b']' {
            return Some(i);
        }
        // Advance by char to stay on UTF-8 boundaries.
        let ch_len = subset[i..].chars().next().map_or(1, |c| c.len_utf8());
        i += ch_len;
    }
    None
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
        if rest.starts_with("<?") {
            match rest.find("?>") {
                Some(end) => pos += end + 2,
                None => break,
            }
            continue;
        }
        if let Some(after) = rest.strip_prefix("<!ENTITY") {
            // `S` is required after the keyword; `<!ENTITYfoo>` is not a
            // declaration (<https://www.w3.org/TR/xml/#sec-entity-decl>).
            // `%` introduces a parameter entity, which we skip.
            match after.chars().next() {
                Some(c) if is_xml_whitespace(c) || c == '%' => {}
                _ => {
                    pos += skip_markup(rest);
                    continue;
                }
            }
            let (consumed, entity) = parse_general_entity_decl(after);
            if let Some((name, value)) = entity {
                // First declaration wins
                // (<https://www.w3.org/TR/xml/#sec-entity-decl>). Beyond the
                // cap further declarations are ignored: their references stay
                // literal, exactly like undefined entities.
                if entities.len() < MAX_ENTITY_DECLS {
                    entities.entry(name).or_insert(value);
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
    // External IDs start with the SYSTEM/PUBLIC keywords; anything else must
    // be the quoted internal value
    // (<https://www.w3.org/TR/xml/#sec-entity-decl>).
    let rest: String = chars.clone().collect();
    if rest.starts_with("SYSTEM")
        && rest["SYSTEM".len()..]
            .chars()
            .next()
            .is_none_or(is_xml_whitespace)
    {
        return (consumed, None);
    }
    if rest.starts_with("PUBLIC")
        && rest["PUBLIC".len()..]
            .chars()
            .next()
            .is_none_or(is_xml_whitespace)
    {
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
            // An unparsed (`NDATA`) entity is not an internal general entity:
            // trailing content after the literal (beyond the `>` terminator)
            // disqualifies it
            // (<https://www.w3.org/TR/xml/#sec-entity-decl>).
            let trailing: String = chars.clone().collect();
            let trailing = trailing.trim_start_matches(is_xml_whitespace);
            let trailing = trailing.strip_suffix('>').unwrap_or(trailing).trim_end_matches(is_xml_whitespace);
            if !trailing.is_empty() {
                return (consumed, None);
            }
            return (consumed, Some((name, value)));
        }
        value.push(c);
    }
    (consumed, None)
}

/// Copies `text` into `output` while budget remains. Returns false when the
/// budget ran out, telling the caller to copy the rest literally and stop
/// expanding. `take` is floored to a char boundary so multi-byte text near
/// exhaustion cannot panic.
fn push_budgeted(output: &mut String, text: &str, budget: &mut usize) -> bool {
    let mut take = (*budget).min(text.len());
    while take > 0 && !text.is_char_boundary(take) {
        take -= 1;
    }
    output.push_str(&text[..take]);
    *budget -= take;
    take == text.len()
}

fn expand_content(
    content: &str,
    entities: &HashMap<String, String>,
    output: &mut String,
    budget: &mut usize,
    errors: &mut Vec<String>,
) {
    let mut pos = 0;
    while pos < content.len() {
        let rest = &content[pos..];
        if rest.starts_with("<!--")
            || rest.starts_with("<![CDATA[")
            || rest.starts_with("<?")
        {
            let consumed = if rest.starts_with("<!--") {
                rest.find("-->").map_or(rest.len(), |end| end + 3)
            } else if rest.starts_with("<![CDATA[") {
                rest.find("]]>").map_or(rest.len(), |end| end + 3)
            } else {
                rest.find("?>").map_or(rest.len(), |end| end + 2)
            };
            if !push_budgeted(output, &rest[..consumed], budget) {
                output.push_str(&content[pos + consumed..]);
                return;
            }
            pos += consumed;
            continue;
        }
        if rest.starts_with('<') {
            let consumed = skip_markup(rest);
            if !expand_tag(&rest[..consumed], entities, output, budget, errors) {
                output.push_str(&content[pos + consumed..]);
                return;
            }
            pos += consumed;
            continue;
        }
        if rest.starts_with('&') {
            if let Some(consumed) =
                expand_entity_ref(rest, entities, output, budget, errors, 0)
            {
                pos += consumed;
            } else {
                // Malformed references stay literal here; the XML tokenizer's
                // char-ref path reports the well-formedness error downstream.
                // Undefined valid-shaped `&name;` already pushed an error.
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

/// Copies a tag verbatim while expanding entity references inside quoted
/// attribute values (<https://www.w3.org/TR/xml/#AVNormalize>). Markup in
/// replacement text is *not* re-tokenized here: attribute values cannot
/// contain `<` (<https://www.w3.org/TR/xml/#forbidden>), and a `<`-bearing
/// value stays literal for the tokenizer to reject. Returns false when the
/// budget ran out.
fn expand_tag(
    tag: &str,
    entities: &HashMap<String, String>,
    output: &mut String,
    budget: &mut usize,
    errors: &mut Vec<String>,
) -> bool {
    let mut quote: Option<char> = None;
    let mut i = 0;
    let mut text_start = 0;
    while i < tag.len() {
        let rest = &tag[i..];
        let c = rest.chars().next().unwrap();
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else if c == '&' {
                if !push_budgeted(output, &tag[text_start..i], budget) {
                    return false;
                }
                if let Some(consumed) =
                    expand_entity_ref(rest, entities, output, budget, errors, 0)
                {
                    i += consumed;
                    text_start = i;
                    continue;
                }
                if !push_budgeted(output, "&", budget) {
                    return false;
                }
                i += 1;
                text_start = i;
                continue;
            }
            i += c.len_utf8();
            continue;
        }
        if c == '"' || c == '\'' {
            quote = Some(c);
        }
        i += c.len_utf8();
    }
    push_budgeted(output, &tag[text_start..], budget)
}

fn expand_entity_ref(
    rest: &str,
    entities: &HashMap<String, String>,
    output: &mut String,
    budget: &mut usize,
    errors: &mut Vec<String>,
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
    if is_predefined_entity(name) {
        return None;
    }
    let Some(value) = entities.get(name).cloned() else {
        // Undeclared general entity: well-formedness violation, fail closed
        // (<https://www.w3.org/TR/xml/#sec-references>).
        errors.push(format!("undefined entity &{name};"));
        return None;
    };
    // Each top-level reference gets a fresh cycle set; it guards only within
    // one expansion, so diamond (non-cyclic) sharing across siblings
    // re-expands from scratch and the output budget (not `seen`) bounds it.
    let mut seen = HashSet::new();
    seen.insert(name.to_owned());
    expand_replacement(&value, entities, output, budget, errors, &mut seen, depth);
    Some(name_end + 2)
}

fn expand_replacement(
    value: &str,
    entities: &HashMap<String, String>,
    output: &mut String,
    budget: &mut usize,
    errors: &mut Vec<String>,
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
                expand_nested_ref(ref_rest, entities, output, budget, errors, seen, depth)
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
    errors: &mut Vec<String>,
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
    if *budget == 0 || depth >= MAX_ENTITY_DEPTH || !is_valid_name(name) {
        return None;
    }
    if is_predefined_entity(name) {
        return None;
    }
    let Some(value) = entities.get(name).cloned() else {
        errors.push(format!("undefined entity &{name};"));
        return None;
    };
    if !seen.insert(name.to_owned()) {
        errors.push(format!("cyclic entity &{name};"));
        return None;
    }
    *budget = budget.saturating_sub(MAX_ENTITY_OVERHEAD);
    expand_replacement(&value, entities, output, budget, errors, seen, depth + 1);
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

/// Predefined XML entities handled by the tokenizer's char-ref path, not by
/// general-entity expansion (<https://www.w3.org/TR/xml/#sec-predefined-ent>).
fn is_predefined_entity(name: &str) -> bool {
    matches!(name, "amp" | "lt" | "gt" | "apos" | "quot")
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
    use super::{MAX_EXPANDED_BYTES, expand_internal_general_entities_with_errors};

    fn expand(input: &str) -> String {
        expand_internal_general_entities_with_errors(input).0.into_owned()
    }

    fn doctype_with(decls: &str, body: &str) -> String {
        format!("<!DOCTYPE r [{decls}]><r>{body}</r>")
    }

    #[test]
    fn legitimate_entities_still_expand() {
        let input = doctype_with("<!ENTITY name \"world\">", "hello &name;!");
        let expanded = expand(&input);
        assert!(
            expanded.contains("hello world!"),
            "unexpected expansion: {expanded:?}"
        );
    }

    #[test]
    fn attribute_value_entities_expand() {
        let input = doctype_with("<!ENTITY a \"X\">", "<r attr=\"&a;\"/>");
        let expanded = expand(&input);
        assert!(
            expanded.contains("attr=\"X\""),
            "attribute value not expanded: {expanded:?}"
        );
    }

    #[test]
    fn first_declaration_wins() {
        let input = doctype_with(
            "<!ENTITY a \"1\"><!ENTITY a \"2\">",
            "&a;",
        );
        let expanded = expand(&input);
        assert!(
            expanded.contains(">1<"),
            "last-wins, expected first: {expanded:?}"
        );
    }

    #[test]
    fn ndata_entities_do_not_expand() {
        let input = doctype_with("<!ENTITY logo \"x\" NDATA png>", "&logo;");
        let expanded = expand(&input);
        assert!(
            expanded.contains("&logo;"),
            "unparsed entity expanded: {expanded:?}"
        );
    }

    #[test]
    fn undefined_entities_report_errors() {
        let input = doctype_with("", "&nosuch;");
        let (expanded, errors) =
            super::expand_internal_general_entities_with_errors(&input);
        assert!(expanded.contains("&nosuch;"));
        assert!(
            errors.iter().any(|e| e.contains("nosuch")),
            "missing undefined-entity error: {errors:?}"
        );
    }

    #[test]
    fn multibyte_tail_does_not_panic() {
        let input = doctype_with("<!ENTITY a \"é\">", "&a;");
        let mut out = String::new();
        let mut budget = 1;
        assert!(!super::push_budgeted(&mut out, "éé", &mut budget));
        let _ = input;
    }

    #[test]
    fn entity_cycles_stay_literal() {
        let input = doctype_with(
            "<!ENTITY a \"&b;\"><!ENTITY b \"&a;\">",
            "&a;",
        );
        let expanded = expand(&input);
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
        let expanded = expand(&input);
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
