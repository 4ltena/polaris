//! Bounded RFC 6570 expansion for catalog resource templates. Expansion never
//! fetches a URI or changes the registered transport destination.
//! Reference: https://www.rfc-editor.org/rfc/rfc6570.html#section-3.2

use crate::{
    Error,
    registration::{MAX_JSON_BYTES, bounded_json},
};
use serde_json::Value;

const LIMIT: usize = 16 * 1024;
const RESERVED: &[u8] = b":/?#[]@!$&'()*+,;=";

pub(crate) fn expand(template: &str, variables: &Value) -> Result<String, Error> {
    if template.len() > LIMIT {
        return Err(Error::Limit);
    }
    bounded_json(variables, MAX_JSON_BYTES)?;
    let variables = variables.as_object().ok_or(Error::Protocol)?;
    let mut output = String::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        literal(&mut output, &rest[..start])?;
        rest = &rest[start + 1..];
        let end = rest.find('}').ok_or(Error::Protocol)?;
        expression(&mut output, &rest[..end], variables)?;
        rest = &rest[end + 1..];
    }
    literal(&mut output, rest)?;
    if output.is_empty() || url::Url::parse(&output).is_err() {
        return Err(Error::Protocol);
    }
    Ok(output)
}

fn push(output: &mut String, value: &str) -> Result<(), Error> {
    if value.len() > LIMIT.saturating_sub(output.len()) {
        return Err(Error::Limit);
    }
    output.push_str(value);
    Ok(())
}

fn unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"-._~".contains(&byte)
}
fn pct(bytes: &[u8], index: usize) -> bool {
    bytes.get(index) == Some(&b'%')
        && bytes.get(index + 1).is_some_and(u8::is_ascii_hexdigit)
        && bytes.get(index + 2).is_some_and(u8::is_ascii_hexdigit)
}

fn encode(value: &str, reserved: bool) -> Result<String, Error> {
    let mut encoded = String::new();
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if reserved && pct(bytes, index) {
            push(&mut encoded, &value[index..index + 3])?;
            index += 3;
            continue;
        }
        if unreserved(byte) || (reserved && RESERVED.contains(&byte)) {
            push(&mut encoded, &(byte as char).to_string())?;
        } else {
            push(&mut encoded, &format!("%{byte:02X}"))?;
        }
        index += 1;
    }
    Ok(encoded)
}

fn literal(output: &mut String, text: &str) -> Result<(), Error> {
    let mut index = 0;
    let bytes = text.as_bytes();
    while index < bytes.len() {
        if pct(bytes, index) {
            push(output, &text[index..index + 3])?;
            index += 3;
            continue;
        }
        let ch = text[index..].chars().next().ok_or(Error::Protocol)?;
        // RFC 6570 literals exclude apostrophe even though reserved expansion
        // permits it. Unicode allows ucschar/iprivate, not C1/noncharacters.
        let code = ch as u32;
        let unicode = matches!(code, 0xA0..=0xD7FF | 0xE000..=0xFDCF | 0xFDF0..=0xFFEF)
            || ((0x10000..=0xDFFFD).contains(&code) && code & 0xFFFF <= 0xFFFD)
            || ((0xE1000..=0x10FFFD).contains(&code) && code & 0xFFFF <= 0xFFFD);
        let ascii =
            ch.is_ascii() && ch != '\'' && (unreserved(ch as u8) || RESERVED.contains(&(ch as u8)));
        if !ascii && !unicode {
            return Err(Error::Protocol);
        }
        push(output, &encode(&ch.to_string(), true)?)?;
        index += ch.len_utf8();
    }
    Ok(())
}

fn valid_name(name: &str) -> bool {
    if name.is_empty() || name.starts_with('.') || name.ends_with('.') || name.contains("..") {
        return false;
    }
    let mut index = 0;
    let bytes = name.as_bytes();
    while index < bytes.len() {
        if pct(bytes, index) {
            index += 3;
        } else if bytes[index].is_ascii_alphanumeric() || b"_.".contains(&bytes[index]) {
            index += 1;
        } else {
            return false;
        }
    }
    true
}

fn scalar(value: &Value) -> Result<String, Error> {
    match value {
        Value::String(value) => Ok(value.clone()),
        Value::Bool(_) | Value::Number(_) => Ok(value.to_string()),
        _ => Err(Error::Protocol),
    }
}

// A percent-encoded UTF-8 character is an indivisible prefix unit, as is a
// literal Unicode scalar. Malformed percent sequences are encoded normally.
fn prefix(value: &str, count: usize) -> &str {
    let mut index = 0;
    for _ in 0..count {
        if index == value.len() {
            break;
        }
        if pct(value.as_bytes(), index) {
            let first = u8::from_str_radix(&value[index + 1..index + 3], 16).expect("checked hex");
            let width = match first {
                0xC2..=0xDF => 2,
                0xE0..=0xEF => 3,
                0xF0..=0xF4 => 4,
                _ => 1,
            };
            let mut decoded = Vec::new();
            for part in 0..width {
                let at = index + 3 * part;
                if !pct(value.as_bytes(), at) {
                    break;
                }
                decoded.push(u8::from_str_radix(&value[at + 1..at + 3], 16).expect("checked hex"));
            }
            index += if decoded.len() == width && std::str::from_utf8(&decoded).is_ok() {
                3 * width
            } else {
                3
            };
        } else {
            index += value[index..]
                .chars()
                .next()
                .expect("remaining text")
                .len_utf8();
        }
    }
    &value[..index]
}

fn expression(
    output: &mut String,
    expression: &str,
    variables: &serde_json::Map<String, Value>,
) -> Result<(), Error> {
    let first = expression.chars().next().ok_or(Error::Protocol)?;
    let (operator, variables_text) = if "+#./;?&".contains(first) {
        (first, &expression[1..])
    } else {
        ('\0', expression)
    };
    let (start, separator, named, if_empty, reserved) = match operator {
        '+' => ("", ",", false, "", true),
        '#' => ("#", ",", false, "", true),
        '.' => (".", ".", false, "", false),
        '/' => ("/", "/", false, "", false),
        ';' => (";", ";", true, "", false),
        '?' => ("?", "&", true, "=", false),
        '&' => ("&", "&", true, "=", false),
        _ => ("", ",", false, "", false),
    };
    let mut emitted = false;
    let mut emit = |value: String| -> Result<(), Error> {
        push(output, if emitted { separator } else { start })?;
        push(output, &value)?;
        emitted = true;
        Ok(())
    };
    let pair = |name: &str, value: String| -> Result<String, Error> {
        let mut result = name.to_owned();
        push(&mut result, if value.is_empty() { if_empty } else { "=" })?;
        push(&mut result, &value)?;
        Ok(result)
    };
    for specification in variables_text.split(',') {
        let explode = specification.ends_with('*');
        let specification = specification.strip_suffix('*').unwrap_or(specification);
        let (name, prefix_count) = if let Some((name, length)) = specification.split_once(':') {
            if explode
                || length.len() > 4
                || length.starts_with('0')
                || !length.bytes().all(|byte| byte.is_ascii_digit())
            {
                return Err(Error::Protocol);
            }
            let count = length.parse::<usize>().map_err(|_| Error::Protocol)?;
            if count == 0 {
                return Err(Error::Protocol);
            }
            (name, Some(count))
        } else {
            (specification, None)
        };
        if !valid_name(name) {
            return Err(Error::Protocol);
        }
        let Some(value) = variables.get(name).filter(|value| !value.is_null()) else {
            continue;
        };
        let mut pairs = Vec::new();
        let is_map = value.is_object();
        match value {
            Value::Array(values) => {
                if prefix_count.is_some() {
                    return Err(Error::Protocol);
                }
                for value in values {
                    pairs.push((name.to_owned(), encode(&scalar(value)?, reserved)?));
                }
            }
            Value::Object(values) => {
                if prefix_count.is_some() {
                    return Err(Error::Protocol);
                }
                for (key, value) in values {
                    if !value.is_null() {
                        pairs.push((encode(key, reserved)?, encode(&scalar(value)?, reserved)?));
                    }
                }
            }
            _ => {
                let value = scalar(value)?;
                let value = prefix_count.map_or(value.as_str(), |count| prefix(&value, count));
                let encoded = encode(value, reserved)?;
                emit(if named { pair(name, encoded)? } else { encoded })?;
                continue;
            }
        }
        if pairs.is_empty() {
            continue;
        }
        if explode {
            for (key, value) in pairs {
                emit(if is_map {
                    pair(&key, value)?
                } else if named {
                    pair(name, value)?
                } else {
                    value
                })?;
            }
        } else {
            let mut joined = String::new();
            for (index, (key, value)) in pairs.into_iter().enumerate() {
                if index != 0 {
                    push(&mut joined, ",")?;
                }
                if is_map {
                    push(&mut joined, &key)?;
                    push(&mut joined, ",")?;
                }
                push(&mut joined, &value)?;
            }
            emit(if named { pair(name, joined)? } else { joined })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn all_operators_lists_maps_prefixes_and_unicode_are_bounded_data() {
        let vars = json!({"word":"a b!","path":"/a/b","empty":"","list":["a","b"],"map":{"a":"1","b":"2"},"unicode":"日本語","encoded":"%E6%97%A5%E6%9C%AC"});
        for (template, expected) in [
            ("fixture:{word}", "fixture:a%20b%21"),
            ("fixture:{+path}", "fixture:/a/b"),
            ("fixture:x{#word}", "fixture:x#a%20b!"),
            ("fixture:x{.list*}", "fixture:x.a.b"),
            ("fixture:x{/list*}", "fixture:x/a/b"),
            ("fixture:x{;empty,list*}", "fixture:x;empty;list=a;list=b"),
            ("fixture:x{?map*}", "fixture:x?a=1&b=2"),
            ("fixture:x?old=1{&empty}", "fixture:x?old=1&empty="),
            ("fixture:{map}", "fixture:a,1,b,2"),
            ("fixture:{list}", "fixture:a,b"),
            ("fixture:{unicode:1}", "fixture:%E6%97%A5"),
            ("fixture:{+encoded:1}", "fixture:%E6%97%A5"),
            ("fixture:x{?missing}", "fixture:x"),
        ] {
            assert_eq!(expand(template, &vars).unwrap(), expected, "{template}");
        }
        assert_eq!(
            expand("fixture:{+path}", &json!({"path":"http://other.invalid/a"})).unwrap(),
            "fixture:http://other.invalid/a"
        );
    }
    #[test]
    fn empty_exploded_map_values_follow_the_operator() {
        for (template, expected) in [
            ("fixture:{map*}", "fixture:empty,full=v"),
            ("fixture:{+map*}", "fixture:empty,full=v"),
            ("fixture:x{#map*}", "fixture:x#empty,full=v"),
            ("fixture:x{.map*}", "fixture:x.empty.full=v"),
            ("fixture:x{/map*}", "fixture:x/empty/full=v"),
            ("fixture:x{;map*}", "fixture:x;empty;full=v"),
            ("fixture:x{?map*}", "fixture:x?empty=&full=v"),
            ("fixture:x?q=v{&map*}", "fixture:x?q=v&empty=&full=v"),
        ] {
            assert_eq!(
                expand(template, &json!({"map":{"empty":"","full":"v"}})).unwrap(),
                expected,
                "{template}"
            );
        }
    }
    #[test]
    fn literal_character_boundaries_reject_invalid_templates() {
        for invalid in [
            '\'',
            '\u{0085}',
            '\u{FDD0}',
            '\u{FFFF}',
            '\u{1FFFE}',
            '\u{E0000}',
            '\u{10FFFF}',
        ] {
            let template = format!("fixture:{invalid}{{x}}");
            assert!(
                expand(&template, &json!({"x":"ok"})).is_err(),
                "{invalid:?}"
            );
        }
        for valid in [
            '日',
            '\u{A0}',
            '\u{E000}',
            '\u{F8FF}',
            '\u{1FFFD}',
            '\u{E1000}',
            '\u{10FFFD}',
        ] {
            let template = format!("fixture:{valid}{{x}}");
            assert_eq!(
                expand(&template, &json!({"x":"ok"})).unwrap(),
                format!("fixture:{}ok", encode(&valid.to_string(), true).unwrap())
            );
        }
        assert_eq!(
            expand("fixture:{+x}", &json!({"x":"'"})).unwrap(),
            "fixture:'"
        );
    }
    #[test]
    fn malformed_templates_and_oversized_expansions_never_return_partial_uris() {
        for template in [
            "fixture:{",
            "fixture:}",
            "fixture:{}",
            "fixture:{=x}",
            "fixture:{x:0}",
            "fixture:{x:10000}",
            "fixture:{x:1*}",
            "fixture:{x,,y}",
            "fixture:bad%xy",
            "fixture:bad space",
        ] {
            assert!(expand(template, &json!({"x":"v"})).is_err(), "{template}");
        }
        assert!(expand("fixture:{x:1}", &json!({"x":["a"]})).is_err());
        assert!(expand("fixture:{x}", &json!({"x":[{"nested":true}]})).is_err());
        assert!(expand("fixture:{x}", &json!({"x":"🌟".repeat(LIMIT)})).is_err());
        assert_eq!(
            expand(
                "fixture:x{?empty,list,map}",
                &json!({"empty":"","list":[],"map":{"unset":null}})
            )
            .unwrap(),
            "fixture:x?empty="
        );
    }
}
