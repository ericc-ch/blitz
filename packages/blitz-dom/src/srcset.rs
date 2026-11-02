//! Parsing an `<img srcset>` attribute and picking one of its candidates.
//!
//! The parser follows "parse a srcset attribute" exactly, including its
//! descriptor state machine and error rules
//! (<https://html.spec.whatwg.org/multipage/images.html#parsing-a-srcset-attribute>).
//! The selection follows "select an image source from a source set": the spec
//! leaves the choice implementation-defined after removing duplicate densities,
//! and this engine picks the smallest density at or above the device pixel
//! ratio, or the largest density when every candidate is below it
//! (<https://html.spec.whatwg.org/multipage/images.html#selecting-an-image-source>).
//!
//! Width descriptors are a parse error here, by the spec's own branch for a
//! user agent without `sizes` support: "If the user agent does not support the
//! `sizes` attribute, let error be yes"
//! (<https://html.spec.whatwg.org/multipage/images.html#parsing-a-srcset-attribute>).
//! This engine does not parse `sizes` yet, so a candidate with a width
//! descriptor drops out and `src` remains the 1x fallback, which is the
//! incremental path that branch exists for.

/// One image candidate string from a `srcset` attribute.
#[derive(Clone, Debug, PartialEq)]
pub struct ImageCandidate {
    /// The URL as the attribute spelled it. Resolution against the document
    /// base happens when the request is made, so a candidate can still fail
    /// there.
    pub url: String,
    /// The candidate's pixel density descriptor; `1.0` when it omits one
    /// (<https://html.spec.whatwg.org/multipage/images.html#normalizing-the-source-densities>).
    pub density: f64,
}

/// The state of the descriptor tokenizer
/// (<https://html.spec.whatwg.org/multipage/images.html#parsing-a-srcset-attribute>).
#[derive(Clone, Copy, PartialEq, Eq)]
enum DescriptorState {
    /// Inside a descriptor.
    InDescriptor,
    /// Inside a parenthesized part of a descriptor, where ASCII whitespace and
    /// commas are just characters.
    InParens,
    /// After a descriptor ended at ASCII whitespace; only whitespace or a
    /// comma may follow before the next descriptor starts.
    AfterDescriptor,
}

/// Parse a `srcset` attribute value into its image candidates, dropping every
/// candidate the algorithm marks as a parse error
/// (<https://html.spec.whatwg.org/multipage/images.html#parsing-a-srcset-attribute>).
pub fn parse_srcset(input: &str) -> Vec<ImageCandidate> {
    let chars: Vec<char> = input.chars().collect();
    let mut position = 0;
    let mut candidates = Vec::new();

    loop {
        // Splitting loop: collect ASCII whitespace and commas. A comma here is
        // a parse error but does not stop the loop.
        while position < chars.len()
            && (chars[position].is_ascii_whitespace() || chars[position] == ',')
        {
            position += 1;
        }
        if position >= chars.len() {
            return candidates;
        }

        // Collect a sequence of characters that are not ASCII whitespace. A URL
        // may contain commas; only trailing ones split the candidate.
        let start = position;
        while position < chars.len() && !chars[position].is_ascii_whitespace() {
            position += 1;
        }
        let mut url: String = chars[start..position].iter().collect();
        let mut descriptors: Vec<String> = Vec::new();

        if url.ends_with(',') {
            // Remove all trailing commas; more than one is a parse error, but
            // the URL still names a candidate.
            while url.ends_with(',') {
                url.pop();
            }
        } else {
            descriptors = tokenize_descriptors(&chars, &mut position);
        }

        if let Some(density) = density_from_descriptors(&descriptors) {
            candidates.push(ImageCandidate { url, density });
        }
        // A candidate the descriptor parser rejected is a parse error; the loop
        // continues with the next one.
    }
}

/// The descriptor tokenizer, from the character after the URL to the comma or
/// end of input that closes the candidate. `position` is advanced past the
/// closing comma.
fn tokenize_descriptors(chars: &[char], position: &mut usize) -> Vec<String> {
    let mut descriptors = Vec::new();
    let mut current = String::new();
    let mut state = DescriptorState::InDescriptor;

    loop {
        if state != DescriptorState::InParens {
            while *position < chars.len() && chars[*position].is_ascii_whitespace() {
                if state == DescriptorState::InDescriptor && !current.is_empty() {
                    descriptors.push(std::mem::take(&mut current));
                    state = DescriptorState::AfterDescriptor;
                }
                *position += 1;
            }
        }

        let Some(&character) = chars.get(*position) else {
            if !current.is_empty() {
                descriptors.push(current);
            }
            return descriptors;
        };

        match state {
            DescriptorState::InParens => {
                current.push(character);
                *position += 1;
                if character == ')' {
                    state = DescriptorState::InDescriptor;
                }
            }
            DescriptorState::InDescriptor | DescriptorState::AfterDescriptor => {
                if character == ',' {
                    *position += 1;
                    if !current.is_empty() {
                        descriptors.push(current);
                    }
                    return descriptors;
                }
                current.push(character);
                *position += 1;
                state = if character == '(' {
                    DescriptorState::InParens
                } else {
                    DescriptorState::InDescriptor
                };
            }
        }
    }
}

/// The descriptor parser. Returns the candidate's density, or `None` when the
/// algorithm sets error
/// (<https://html.spec.whatwg.org/multipage/images.html#parsing-a-srcset-attribute>).
fn density_from_descriptors(descriptors: &[String]) -> Option<f64> {
    let mut error = false;
    let mut width: Option<u64> = None;
    let mut density: Option<f64> = None;
    let mut future_compat_h: Option<u64> = None;

    for descriptor in descriptors {
        if let Some(prefix) = descriptor.strip_suffix('w') {
            if let Some(value) = parse_non_negative_integer(prefix) {
                // This engine does not support `sizes`, which the spec makes a
                // parse error for a width descriptor.
                error = true;
                if width.is_some() || density.is_some() {
                    error = true;
                }
                if value == 0 {
                    error = true;
                } else {
                    width = Some(value);
                }
                continue;
            }
        }
        if let Some(prefix) = descriptor.strip_suffix('x') {
            if let Some(value) = parse_floating_point(prefix) {
                if width.is_some() || density.is_some() || future_compat_h.is_some() {
                    error = true;
                }
                if value < 0.0 {
                    error = true;
                } else {
                    density = Some(value);
                }
                continue;
            }
        }
        if let Some(prefix) = descriptor.strip_suffix('h') {
            if let Some(value) = parse_non_negative_integer(prefix) {
                if future_compat_h.is_some() || density.is_some() {
                    error = true;
                }
                if value == 0 {
                    error = true;
                } else {
                    future_compat_h = Some(value);
                }
                continue;
            }
        }
        error = true;
    }

    // An `h` descriptor only makes sense alongside a width descriptor.
    if future_compat_h.is_some() && width.is_none() {
        error = true;
    }
    if error {
        return None;
    }
    Some(density.unwrap_or(1.0))
}

/// "Rules for parsing non-negative integers", restricted to the "valid" form a
/// descriptor prefix must have: one or more ASCII digits and nothing else.
fn parse_non_negative_integer(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// A "valid floating-point number": an optional minus sign, digits, an optional
/// fractional part, and an optional exponent
/// (<https://html.spec.whatwg.org/multipage/common-microsyntaxes.html#valid-floating-point-number>).
fn parse_floating_point(text: &str) -> Option<f64> {
    let bytes = text.as_bytes();
    let mut index = 0;
    if bytes.first() == Some(&b'-') {
        index += 1;
    }
    let integer_start = index;
    while bytes.get(index).is_some_and(u8::is_ascii_digit) {
        index += 1;
    }
    if index == integer_start {
        return None;
    }
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        let fraction_start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if index == fraction_start {
            return None;
        }
    }
    if bytes
        .get(index)
        .is_some_and(|byte| matches!(byte, b'e' | b'E'))
    {
        index += 1;
        if bytes
            .get(index)
            .is_some_and(|byte| matches!(byte, b'+' | b'-'))
        {
            index += 1;
        }
        let exponent_start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if index == exponent_start {
            return None;
        }
    }
    if index != bytes.len() {
        return None;
    }
    text.parse().ok()
}

/// Pick one candidate for `device_pixel_ratio`, returning its index.
///
/// The spec removes candidates whose density repeats an earlier one and then
/// chooses in an implementation-defined manner
/// (<https://html.spec.whatwg.org/multipage/images.html#selecting-an-image-source>).
/// This picks the smallest density at or above the ratio, and the largest
/// density when every candidate is below it: the same rule Chromium implements
/// in `HTMLSrcsetParser::PickBestSource`.
pub fn choose(candidates: &[ImageCandidate], device_pixel_ratio: f64) -> Option<usize> {
    let densities: Vec<f64> = candidates
        .iter()
        .map(|candidate| candidate.density)
        .collect();
    choose_density(&densities, device_pixel_ratio)
}

/// [`choose`] over bare densities, for a caller that has already resolved
/// candidate URLs and kept the densities beside them.
pub fn choose_density(densities: &[f64], device_pixel_ratio: f64) -> Option<usize> {
    let mut best: Option<usize> = None;
    for (index, &density) in densities.iter().enumerate() {
        // A repeated density is ignored: the first candidate with a density
        // wins.
        if densities[..index].contains(&density) {
            continue;
        }
        best = Some(match best {
            None => index,
            Some(current) => {
                let current_density = densities[current];
                let candidate_ge = density >= device_pixel_ratio;
                let current_ge = current_density >= device_pixel_ratio;
                if candidate_ge {
                    // Prefer the smallest density at or above the ratio.
                    if !current_ge || density < current_density {
                        index
                    } else {
                        current
                    }
                } else if current_ge {
                    current
                } else if density > current_density {
                    // Every candidate is below the ratio: take the largest.
                    index
                } else {
                    current
                }
            }
        });
    }
    best
}

#[cfg(test)]
mod tests {
    use super::{ImageCandidate, choose, parse_srcset};

    fn parsed(input: &str) -> Vec<(String, f64)> {
        parse_srcset(input)
            .into_iter()
            .map(|candidate| (candidate.url, candidate.density))
            .collect()
    }

    #[test]
    fn parses_density_descriptors_and_a_bare_url() {
        assert_eq!(
            parsed("a.png 1x, b.png 2x,c.png"),
            vec![
                ("a.png".to_owned(), 1.0),
                ("b.png".to_owned(), 2.0),
                ("c.png".to_owned(), 1.0),
            ]
        );
    }

    #[test]
    fn a_comma_inside_a_url_does_not_split_the_candidate() {
        assert_eq!(
            parsed("data:image/png;base64,AAAA 1x, b.png 2x"),
            vec![
                ("data:image/png;base64,AAAA".to_owned(), 1.0),
                ("b.png".to_owned(), 2.0),
            ]
        );
    }

    #[test]
    fn trailing_commas_belong_to_the_separator() {
        assert_eq!(
            parsed("a.png,, b.png 2x,"),
            vec![("a.png".to_owned(), 1.0), ("b.png".to_owned(), 2.0)]
        );
    }

    #[test]
    fn density_may_be_fractional_or_exponent_form() {
        assert_eq!(
            parsed("a.png 1.5x, b.png 1e2x"),
            vec![("a.png".to_owned(), 1.5), ("b.png".to_owned(), 100.0)]
        );
    }

    #[test]
    fn an_invalid_density_drops_the_candidate() {
        for input in [
            "a.png -1x",
            "a.png .5x",
            "a.png 1x 2x",
            "a.png 2x 1h",
            "a.png 1h",
            "a.png 0h",
            "a.png nope",
            "a.png 1X",
        ] {
            assert!(parsed(input).is_empty(), "{input} must be a parse error");
        }
    }

    #[test]
    fn a_zero_density_is_accepted_and_normalized_by_selection() {
        assert_eq!(parsed("a.png 0x"), vec![("a.png".to_owned(), 0.0)]);
    }

    #[test]
    fn a_width_descriptor_is_a_parse_error_without_sizes_support() {
        assert!(parsed("a.png 100w, b.png 200w").is_empty());
        // The density candidate beside it still stands.
        assert_eq!(parsed("a.png 100w, b.png 2x"), vec![("b.png".to_owned(), 2.0)]);
    }

    #[test]
    fn empty_input_has_no_candidates() {
        assert!(parse_srcset("").is_empty());
        assert!(parse_srcset(" , , ").is_empty());
    }

    fn candidates(densities: &[f64]) -> Vec<ImageCandidate> {
        densities
            .iter()
            .map(|density| ImageCandidate {
                url: format!("{density}x.png"),
                density: *density,
            })
            .collect()
    }

    #[test]
    fn selection_prefers_an_exact_density() {
        let set = candidates(&[1.0, 2.0, 3.0]);
        assert_eq!(choose(&set, 1.0).map(|index| set[index].density), Some(1.0));
        assert_eq!(choose(&set, 2.0).map(|index| set[index].density), Some(2.0));
    }

    #[test]
    fn selection_takes_the_smallest_density_at_or_above_the_ratio() {
        let set = candidates(&[1.0, 2.0, 3.0]);
        assert_eq!(
            choose(&set, 1.5).map(|index| set[index].density),
            Some(2.0)
        );
    }

    #[test]
    fn selection_falls_back_to_the_largest_density() {
        let set = candidates(&[1.0, 2.0]);
        assert_eq!(choose(&set, 4.0).map(|index| set[index].density), Some(2.0));
    }

    #[test]
    fn selection_ignores_a_repeated_density() {
        let set = candidates(&[2.0, 2.0, 3.0]);
        assert_eq!(choose(&set, 2.0), Some(0));
    }

    #[test]
    fn selection_with_no_candidates_is_none() {
        assert_eq!(choose(&[], 1.0), None);
    }
}
