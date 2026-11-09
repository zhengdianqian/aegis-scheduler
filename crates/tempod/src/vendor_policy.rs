// SPDX-License-Identifier: GPL-3.0-or-later
// Offline preparation only: no HAL timing evidence exists, so activation is forbidden.
use std::ops::Range;

pub const STATUS: &str = "disabled_unverified_hal_timing";
type Result<T> = std::result::Result<T, String>;

fn number(value: &str) -> Result<u32> {
    let value = value.trim();
    let parsed = if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        if hex.is_empty() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("invalid hexadecimal resource number".into());
        }
        u32::from_str_radix(hex, 16)
    } else if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        value.parse()
    } else {
        return Err("invalid unsigned resource number".into());
    };
    parsed.map_err(|_| "resource number out of range".into())
}

fn xml_text(text: &str) -> Result<()> {
    if text.chars().any(|c| !matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')) {
        return Err("invalid XML character".into());
    }
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        rest = &rest[at + 1..];
        let end = rest.find(';').ok_or("unterminated XML entity")?;
        let entity = &rest[..end];
        if !matches!(entity, "amp" | "lt" | "gt" | "apos" | "quot") {
            let code = if let Some(hex) = entity.strip_prefix("#x") {
                if !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                    u32::from_str_radix(hex, 16).ok()
                } else {
                    None
                }
            } else if let Some(decimal) = entity.strip_prefix('#') {
                if decimal.bytes().all(|b| b.is_ascii_digit()) {
                    decimal.parse().ok()
                } else {
                    None
                }
            } else {
                None
            };
            let c = code.and_then(char::from_u32).ok_or("invalid XML entity")?;
            if !matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')
            {
                return Err("invalid XML character reference".into());
            }
        }
        rest = &rest[end + 1..];
    }
    Ok(())
}

struct Scanner<'a> {
    text: &'a str,
    pos: usize,
}
impl<'a> Scanner<'a> {
    fn space(&mut self) -> bool {
        let start = self.pos;
        while self
            .text
            .as_bytes()
            .get(self.pos)
            .is_some_and(|b| matches!(b, b' ' | b'\r' | b'\n' | b'\t'))
        {
            self.pos += 1;
        }
        start != self.pos
    }
    fn name(&mut self) -> Result<&'a str> {
        let start = self.pos;
        let b = self
            .text
            .as_bytes()
            .get(self.pos)
            .ok_or("missing XML name")?;
        if !b.is_ascii_alphabetic() && *b != b'_' {
            return Err("unsupported XML name".into());
        }
        self.pos += 1;
        while self
            .text
            .as_bytes()
            .get(self.pos)
            .is_some_and(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
        {
            self.pos += 1;
        }
        Ok(&self.text[start..self.pos])
    }
    fn attributes(&mut self, end: &str) -> Result<Vec<(&'a str, Range<usize>)>> {
        let mut attrs = Vec::new();
        loop {
            let separated = self.space();
            if self.text[self.pos..].starts_with(end)
                || (end == ">" && self.text[self.pos..].starts_with("/>"))
            {
                return Ok(attrs);
            }
            if !separated {
                return Err("missing attribute separator".into());
            }
            let name = self.name()?;
            if attrs.iter().any(|(old, _)| *old == name) {
                return Err("duplicate XML attribute".into());
            }
            self.space();
            if self.text.as_bytes().get(self.pos) != Some(&b'=') {
                return Err("missing attribute equals".into());
            }
            self.pos += 1;
            self.space();
            let quote = *self
                .text
                .as_bytes()
                .get(self.pos)
                .ok_or("missing attribute quote")?;
            if !matches!(quote, b'\'' | b'"') {
                return Err("unquoted attribute".into());
            }
            self.pos += 1;
            let start = self.pos;
            let len = self.text[start..]
                .find(quote as char)
                .ok_or("unterminated attribute")?;
            self.pos += len;
            let value = &self.text[start..self.pos];
            if value.contains('<') {
                return Err("less-than in XML attribute".into());
            }
            xml_text(value)?;
            attrs.push((name, start..self.pos));
            self.pos += 1;
        }
    }
}

fn resources(raw: &str, remove: bool) -> Result<String> {
    if raw.trim().is_empty() {
        return Ok(raw.into());
    }
    let fields: Vec<_> = raw.split(',').collect();
    if fields.len() % 2 != 0 {
        return Err("odd Resources opcode/value count".into());
    }
    let mut kept = Vec::new();
    for pair in fields.chunks_exact(2) {
        let opcode = number(pair[0])?;
        number(pair[1])?;
        if !remove || !matches!(opcode, 0x40800000 | 0x40800100 | 0x40800200 | 0x40c00000) {
            kept.push(format!("{},{}", pair[0], pair[1]));
        }
    }
    Ok(kept.join(","))
}

/// A deliberately restricted XML reader: unsupported constructs (DTD, namespaces,
/// CDATA, arbitrary PI) fail closed rather than being guessed or reserialized.
/// Only Resources value ranges of the two approved IDs may be replaced.
pub fn transform(input: &[u8]) -> Result<Vec<u8>> {
    let text = std::str::from_utf8(input).map_err(|_| "XML is not UTF-8")?;
    let text_start = usize::from(text.starts_with('\u{feff}')) * 3;
    let mut s = Scanner {
        text,
        pos: text_start,
    };
    let mut stack = Vec::new();
    let mut root_seen = false;
    let mut edits = Vec::new();
    while s.pos < text.len() {
        if text[s.pos..].starts_with("<?xml ") && s.pos == text_start {
            s.pos += 5;
            let attrs = s.attributes("?>")?;
            let names: Vec<_> = attrs.iter().map(|(n, _)| *n).collect();
            if !matches!(
                names.as_slice(),
                ["version"]
                    | ["version", "encoding"]
                    | ["version", "standalone"]
                    | ["version", "encoding", "standalone"]
            ) {
                return Err("invalid XML declaration".into());
            }
            for (name, range) in attrs {
                let value = &text[range];
                if !match name {
                    "version" => value == "1.0",
                    "encoding" => value.eq_ignore_ascii_case("utf-8"),
                    "standalone" => matches!(value, "yes" | "no"),
                    _ => false,
                } {
                    return Err("unsupported XML declaration".into());
                }
            }
            s.pos += 2;
        } else if text[s.pos..].starts_with("<!--") {
            let start = s.pos + 4;
            let end = start + text[start..].find("-->").ok_or("unterminated comment")?;
            let comment = &text[start..end];
            if comment.contains("--") || comment.ends_with('-') {
                return Err("invalid XML comment".into());
            }
            xml_text(&comment.replace('&', ""))?;
            s.pos = end + 3;
        } else if text[s.pos..].starts_with("</") {
            s.pos += 2;
            let name = s.name()?;
            s.space();
            if !text[s.pos..].starts_with('>') || stack.pop() != Some(name) {
                return Err("mismatched XML closing tag".into());
            }
            s.pos += 1;
        } else if text[s.pos..].starts_with('<') {
            s.pos += 1;
            let name = s.name()?;
            if stack.is_empty() {
                if root_seen {
                    return Err("multiple XML roots".into());
                }
                root_seen = true;
            }
            let attrs = s.attributes(">")?;
            if name == "Config" {
                let id = attrs
                    .iter()
                    .find(|(n, _)| *n == "Id")
                    .map(|(_, r)| number(&text[r.clone()]))
                    .transpose()?;
                let target = matches!(id, Some(0x1080 | 0x1081));
                if target && !attrs.iter().any(|(n, _)| *n == "Resources") {
                    return Err("target Config missing Resources".into());
                }
                for (attr, range) in attrs {
                    if attr == "Resources" {
                        let replacement = resources(&text[range.clone()], target)?;
                        if replacement != text[range.clone()] {
                            edits.push((range, replacement));
                        }
                    }
                }
            }
            if text[s.pos..].starts_with("/>") {
                s.pos += 2;
            } else {
                stack.push(name);
                s.pos += 1;
            }
        } else {
            let end = s.pos + text[s.pos..].find('<').unwrap_or(text.len() - s.pos);
            let value = &text[s.pos..end];
            xml_text(value)?;
            if value.contains("]]>")
                || (stack.is_empty()
                    && !value
                        .bytes()
                        .all(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n')))
            {
                return Err("invalid XML text".into());
            }
            s.pos = end;
        }
    }
    if !root_seen || !stack.is_empty() {
        return Err("incomplete XML document".into());
    }
    let mut output = text.to_owned();
    for (range, replacement) in edits.into_iter().rev() {
        output.replace_range(range, &replacement);
    }
    Ok(output.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn run(s: &str) -> String {
        String::from_utf8(transform(s.as_bytes()).unwrap()).unwrap()
    }
    #[test]
    fn exact_scope_and_bytes() {
        let xml = "<?xml version='1.0'?>\r\n<R><!-- keep & raw -->\r\n<Config Type='201' Id='0x00001081' Resources='0x40800000, 9, 0x42804000, 0,\n 0x43498000, 4224000, 0x40804000, 0xFFF'/>\r\n<Config Id='0x1082' Resources='0x40800000, 7'/></R>";
        assert_eq!(run(xml), xml.replace("0x40800000, 9,", ""));
    }
    #[test]
    fn all_four_and_leading_zero_ids() {
        for id in ["0x1080", "0x00001081", "4224", "0X00001080"] {
            let xml = format!("<Config Id='{id}' Resources='0x40800000,1,0x40800100,2,0x40800200,3,0x40C00000,4'/>");
            assert_eq!(run(&xml), format!("<Config Id='{id}' Resources=''/>"));
        }
    }
    #[test]
    fn keep_other_scheduler_gpu_ddr_and_cpu_max() {
        let xml = "<Config Id='0x1080' Resources='0x40C00000,2,0x4080C000,998000,0x40808000,600000,0x4281C000,2000,0x4303C000,0xA6428,0x40804000,0xFFF'/>";
        assert_eq!(run(xml), xml.replace("0x40C00000,2,", ""));
    }
    #[test]
    fn remove_middle_and_tail_without_reformatting() {
        let xml = "<Config Resources=' 0x42804000 , 0 , 0x40800000, 3,\n0x43498000, 9,0x40C00000,2' Id='0x1080'/>";
        assert_eq!(
            run(xml),
            "<Config Resources=' 0x42804000 , 0 ,\n0x43498000, 9' Id='0x1080'/>"
        );
    }
    #[test]
    fn empty_and_idempotent() {
        let xml = "<R><Config Id='0x1081' Resources=''/><Config Id='0x1080' Resources='0x40C00000,1,0x42804000,0'/></R>";
        assert_eq!(run(&run(xml)), run(xml));
    }
    #[test]
    fn reject_malformed_documents() {
        for xml in [
            "",
            "<R>",
            "<R></X>",
            "<R/><R/>",
            "<R a='1' a='2'/>",
            "<R a='<'/>",
            "<R a='&bad;'/>",
            "<R>\u{1}</R>",
            "<R>&#0;</R>",
            "<R><!-- bad--comment --></R>",
            "<R><![CDATA[x]]></R>",
            "<!DOCTYPE R><R/>",
            "<R a='1'b='2'/>",
            "<?xml version='2.0'?><R/>",
            "<R>]]></R>",
            "text<R/>",
            "<R>\u{feff}</R>junk",
        ] {
            assert!(transform(xml.as_bytes()).is_err(), "accepted {xml:?}");
        }
        assert!(transform(&[0xff]).is_err());
    }
    #[test]
    fn reject_bad_resources_and_duplicate_attributes() {
        for xml in [
            "<Config Id='0x1080' Id='0x1081' Resources=''/>",
            "<Config Id='0x1080' Resources='' Resources=''/>",
            "<Config Id='0x1080'/>",
            "<Config Id='0x1080' Resources='1,2,3'/>",
            "<Config Id='0x1082' Resources='1'/>",
            "<Config Id='0x1080' Resources='0x40800000,no'/>",
            "<Config Id='0x1080' Resources='1,2,'/>",
            "<Config Id='0x1080' Resources='4294967296,1'/>",
        ] {
            assert!(transform(xml.as_bytes()).is_err(), "accepted {xml}");
        }
    }
    #[test]
    fn comments_are_not_configs() {
        let xml = "\u{feff}<R><!-- <Config Id='0x1080' Resources='1'/> --><Other Resources='opaque'/></R>";
        assert_eq!(run(xml), xml);
    }
}
