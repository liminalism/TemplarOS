//! Parse XML structurally so line wrapping, attribute order and comments cannot bypass the gate.
use anyhow::{Context, Result};
use quick_xml::events::Event;
use std::collections::BTreeMap;

#[derive(Debug, Default)]
pub struct Protocol {
    pub versions: BTreeMap<String, u32>,
    pub toplevel_requests: Vec<u32>,
    pub versioned_members: usize,
    pub problems: Vec<String>,
}

pub fn parse(text: &str) -> Result<Protocol> {
    let mut reader = quick_xml::Reader::from_str(text);
    let mut protocol = Protocol::default();
    let mut current: Option<(String, u32)> = None;
    loop {
        let element = match reader.read_event().context("reading Wayland XML")? {
            Event::Eof => break,
            Event::Start(element) | Event::Empty(element) => element,
            Event::End(element) if element.name().as_ref() == b"interface" => {
                current = None;
                continue;
            }
            _ => continue,
        };
        let is_interface = element.name().as_ref() == b"interface";
        let mut name = None;
        let mut version = None;
        let mut since = None;
        for attribute in element.attributes() {
            let attribute = attribute.context("reading Wayland XML attribute")?;
            match attribute.key.as_ref() {
                b"name" if is_interface => {
                    name = Some(std::str::from_utf8(attribute.value.as_ref())?.to_owned());
                }
                b"version" => {
                    version = Some(std::str::from_utf8(attribute.value.as_ref())?.parse::<u32>()?);
                }
                b"since" => {
                    since = Some(std::str::from_utf8(attribute.value.as_ref())?.parse::<u32>()?);
                }
                _ => {}
            }
        }
        if is_interface {
            let name = name.context("interface has no name")?;
            let version = version.context("interface has no version")?;
            protocol.versions.insert(name.clone(), version);
            current = Some((name, version));
            continue;
        }
        let Some((interface, version)) = current.as_ref() else {
            continue;
        };
        if interface == "templar_shell_toplevel_v1" && element.name().as_ref() == b"request" {
            protocol.toplevel_requests.push(since.unwrap_or(1));
        }
        if let Some(since) = since {
            protocol.versioned_members += 1;
            if since > *version {
                protocol.problems.push(format!("{interface} is version {version} but declares a member since={since}: no peer can reach it"));
            }
        }
    }
    Ok(protocol)
}

/// Named XML entity references (`&lt;`, `&amp;`, …) in a protocol file, as `(line, entity)`
/// pairs. `wayland-scanner` 0.31 panics on any of them with no line number and no mention of
/// the entity, taking the whole shell's generated bindings down — so the gate names them.
/// Numeric character references (`&#60;`) are not entity references and are left alone, as is
/// anything inside an XML comment.
pub fn entity_references(text: &str) -> Vec<(usize, String)> {
    let mut stripped = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<!--") {
        stripped.push_str(&rest[..start]);
        // Keep line numbers stable: a comment still occupies its newlines.
        let after = &rest[start + "<!--".len()..];
        if let Some(end) = after.find("-->") {
            stripped.push_str(&after[..end].replace(|c| c != '\n', " "));
            stripped.push_str("   ");
            rest = &after[end + "-->".len()..];
        } else {
            stripped.push_str(&after.replace(|c| c != '\n', " "));
            rest = "";
        }
    }
    stripped.push_str(rest);
    let mut found = Vec::new();
    for (index, line) in stripped.split('\n').enumerate() {
        for (at, c) in line.char_indices() {
            if c != '&' {
                continue;
            }
            let rest = &line[at + '&'.len_utf8()..];
            // ASCII-only take, so byte indexing below cannot split a character.
            let entity: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '#' || *c == '_')
                .collect();
            if entity.is_empty() || entity.starts_with('#') {
                continue;
            }
            if rest[entity.len()..].starts_with(';') {
                found.push((index + 1, entity));
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn named_entities_are_reported_with_line_numbers() {
        let hits = entity_references(
            "<protocol>\n  <arg summary=\"a &lt; b &amp; c\"/>\n  <!-- &quot; in a comment -->\n  <arg summary=\"&#60; numeric\"/>\n</protocol>\n",
        );
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0], (2, "lt".to_owned()));
        assert_eq!(hits[1], (2, "amp".to_owned()));
    }
    #[test]
    fn wrapped_requests_and_reordered_attributes_are_checked_not_comment_text() {
        let parsed = parse(r#"<protocol name="test">
            <!-- <interface name="templar_shell_manager_v2" version="99"/> -->
            <interface version="13" name="templar_shell_manager_v2"><event since="13" name="changed"/></interface>
            <interface name="templar_shell_toplevel_v1" version="13"><request
                since="14"
                name="new_request"/></interface>
        </protocol>"#).unwrap();
        assert_eq!(parsed.versions["templar_shell_manager_v2"], 13);
        assert_eq!(parsed.toplevel_requests, vec![14]);
        assert_eq!(parsed.problems.len(), 1);
    }
}
