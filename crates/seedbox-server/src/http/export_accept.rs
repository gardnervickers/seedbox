//! Negotiate the two checkpoint representations; JSON remains the default.
use axum::http::{header::ACCEPT, HeaderMap};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Representation {
    Json,
    Stream,
}

// Quoted parameter values may themselves contain commas or semicolons.
fn parts(value: &str, delimiter: char) -> Option<Vec<&str>> {
    let (mut quoted, mut escaped, mut start) = (false, false, 0);
    let mut parts = Vec::new();
    for (index, character) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if quoted && character == '\\' {
            escaped = true;
            continue;
        }
        if character == '"' {
            quoted = !quoted;
        }
        if !quoted && character == delimiter {
            parts.push(&value[start..index]);
            start = index + character.len_utf8();
        }
    }
    if quoted || escaped {
        return None;
    }
    parts.push(&value[start..]);
    Some(parts)
}

fn quality(value: &str) -> Option<u16> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if fraction.len() > 3 || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut fraction_value = 0u16;
    for byte in fraction.bytes() {
        fraction_value = fraction_value * 10 + u16::from(byte - b'0');
    }
    fraction_value *= 10u16.pow((3 - fraction.len()) as u32);
    match whole {
        "0" => Some(fraction_value),
        "1" if fraction_value == 0 => Some(1000),
        _ => None,
    }
}

pub(super) fn negotiate(headers: &HeaderMap) -> Option<Representation> {
    if !headers.contains_key(ACCEPT) {
        return Some(Representation::Json);
    }
    // Specific exclusions override broader wildcard ranges, regardless of q.
    let mut json = None;
    let mut stream = None;
    for value in headers.get_all(ACCEPT) {
        let Ok(value) = value.to_str() else {
            continue;
        };
        let Some(ranges) = parts(value, ',') else {
            continue;
        };
        for range in ranges {
            let Some(parameters) = parts(range, ';') else {
                continue;
            };
            let Some(media) = parameters.first() else {
                continue;
            };
            let media = media.trim().to_ascii_lowercase();
            let mut q = Some(1000);
            let mut seen_quality = false;
            let mut media_parameters = 0u8;
            for parameter in parameters.iter().skip(1) {
                let Some((name, value)) = parameter.trim().split_once('=') else {
                    if seen_quality && !parameter.trim().is_empty() {
                        continue;
                    }
                    q = None;
                    break;
                };
                if name.trim().eq_ignore_ascii_case("q") {
                    if seen_quality {
                        q = None;
                        break;
                    }
                    seen_quality = true;
                    q = quality(value.trim());
                } else if !seen_quality {
                    // Both representations are UTF-8. Other media constraints
                    // cannot be satisfied; extensions after q are not constraints.
                    let value = value.trim().trim_matches('"');
                    if !name.trim().eq_ignore_ascii_case("charset")
                        || !value.eq_ignore_ascii_case("utf-8")
                        || media_parameters != 0
                    {
                        q = None;
                        break;
                    }
                    media_parameters = 1;
                }
            }
            let Some(q) = q else {
                continue;
            };
            for (candidate, accepted) in [
                ("application/json", &mut json),
                ("text/event-stream", &mut stream),
            ] {
                let specificity = if media == candidate {
                    2
                } else if media == "*/*" {
                    0
                } else if media == format!("{}/*", candidate.split_once('/').unwrap().0) {
                    1
                } else {
                    continue;
                };
                if accepted
                    .is_none_or(|old: (u8, u8, u16)| (specificity, media_parameters, q) > old)
                {
                    *accepted = Some((specificity, media_parameters, q));
                }
            }
        }
    }
    let jq = json.map_or(0, |(_, _, q)| q);
    let sq = stream.map_or(0, |(_, _, q)| q);
    if sq > jq || (sq != 0 && sq == jq && stream.unwrap().0 > 0) {
        Some(Representation::Stream)
    } else if jq != 0 {
        Some(Representation::Json)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lists_parameters_quality_and_specific_exclusions() {
        let cases = [
            ("text/event-stream", Some(Representation::Stream)),
            (
                "application/json, TEXT/EVENT-STREAM; charset=utf-8",
                Some(Representation::Stream),
            ),
            (
                "text/event-stream; q=0.9; extension=\"a,b;c\", application/json;q=0.2",
                Some(Representation::Stream),
            ),
            (
                "text/event-stream;q=0.2, application/json;q=0.9",
                Some(Representation::Json),
            ),
            ("text/event-stream;q=0, */*;q=1", Some(Representation::Json)),
            (
                "application/json;q=0, */*;q=1",
                Some(Representation::Stream),
            ),
            ("*/*", Some(Representation::Json)),
            ("text/*", Some(Representation::Stream)),
            ("application/*", Some(Representation::Json)),
            ("text/event-stream;q=0, application/json;q=0", None),
            ("application/xml", None),
            ("text/event-stream;charset=ascii", None),
            ("text/event-stream;version=2", None),
            (
                "text/event-stream;q=1,text/event-stream;charset=utf-8;q=0",
                None,
            ),
            ("text/event-stream;q=1.001", None),
            ("text/event-stream;q=0.1234", None),
            ("text/event-stream;q=0.5;q=1", None),
            (
                "text/event-stream;q=1;extension",
                Some(Representation::Stream),
            ),
            ("text/event-stream;extension=\"unterminated", None),
        ];
        for (accept, expected) in cases {
            let mut headers = HeaderMap::new();
            headers.insert(ACCEPT, accept.parse().unwrap());
            assert_eq!(negotiate(&headers), expected, "{accept}");
        }
    }
    #[test]
    fn default_and_repeated_fields() {
        let mut headers = HeaderMap::new();
        assert_eq!(negotiate(&headers), Some(Representation::Json));
        headers.append(ACCEPT, "application/json;q=0.5".parse().unwrap());
        headers.append(ACCEPT, "text/event-stream;q=1".parse().unwrap());
        assert_eq!(negotiate(&headers), Some(Representation::Stream));
    }
}
