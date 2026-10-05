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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wrapped_requests_and_reordered_attributes_are_checked_not_comment_text() {
        let parsed = parse(r#"<protocol name="test">
            <!-- <interface name="templar_shell_manager_v1" version="99"/> -->
            <interface version="13" name="templar_shell_manager_v1"><event since="13" name="changed"/></interface>
            <interface name="templar_shell_toplevel_v1" version="13"><request
                since="14"
                name="new_request"/></interface>
        </protocol>"#).unwrap();
        assert_eq!(parsed.versions["templar_shell_manager_v1"], 13);
        assert_eq!(parsed.toplevel_requests, vec![14]);
        assert_eq!(parsed.problems.len(), 1);
    }
}
