//! `cargo xtask interface-baseline` / `check-interface-signatures` — the frozen-signature
//! convention, enforced.
//!
//! `ARCHITECTURE.md` says shipped D-Bus signatures are frozen: change a member's arguments and you
//! bump the interface version rather than editing it in place. Nothing checked that.
//! `check-interfaces` compares *names* — that every `#[zbus::interface]` has an XML file — which
//! catches a whole interface appearing without documentation and nothing else. A method quietly
//! gaining an argument would pass every gate in the repository and break every client built
//! against the old signature, at run time, on someone else's machine.
//!
//! So: `interfaces/baseline.json` records the signature of every member of every shipped
//! interface, and this check diffs the XML against it.
//!
//! * **A new member passes.** Additive growth on a shipped version is the established practice
//!   here — `AppIdentity1` gained `ListApplications` without a bump — and refusing it would make
//!   the convention stricter than the project actually is. The check says how many are new and
//!   how to freeze them.
//! * **A changed or removed member fails**, naming the interface to bump to.
//!
//! The parser is `quick-xml` and lives in the dev tool only; no service links it. It reads the
//! introspection XML rather than the Rust, because the XML is the contract
//! (`interfaces/README.md`) and a check that read the implementation would only ever confirm that
//! the code agrees with itself.

use anyhow::{Context, Result, bail};
use quick_xml::events::Event;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// Where the frozen signatures live.
pub const BASELINE: &str = "interfaces/baseline.json";

/// Every member of one interface, by kind, each mapped to its signature.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct InterfaceSignatures {
    #[serde(default)]
    pub methods: BTreeMap<String, String>,
    #[serde(default)]
    pub signals: BTreeMap<String, String>,
    #[serde(default)]
    pub properties: BTreeMap<String, String>,
}

/// The committed baseline.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Baseline {
    pub version: u32,
    pub interfaces: BTreeMap<String, InterfaceSignatures>,
}

/// How a member's signature is written down.
///
/// `In(types)->(out types)` for a method, `(types)` for a signal, `type access` for a property —
/// D-Bus type codes throughout, which is what a client is actually compiled against. Argument
/// *names* are deliberately excluded: renaming an argument breaks no caller, and a check that
/// failed on it would train people to ignore it.
fn method_signature(inputs: &str, outputs: &str) -> String {
    format!("({inputs})->({outputs})")
}

/// Parses every `interfaces/dbus/*.xml` into signatures.
///
/// # Errors
/// If the directory cannot be read, a file is not well-formed XML, or two files disagree about
/// the same interface.
pub fn parse_tree(root: &Path) -> Result<BTreeMap<String, InterfaceSignatures>> {
    let dir = root.join("interfaces/dbus");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "xml"))
        .collect();
    files.sort();

    let mut interfaces: BTreeMap<String, InterfaceSignatures> = BTreeMap::new();
    for file in &files {
        let text =
            std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
        let parsed = parse_xml(&text).with_context(|| format!("parsing {}", file.display()))?;
        for (name, signatures) in parsed {
            match interfaces.get(&name) {
                Some(existing) if *existing != signatures => {
                    bail!("{name} is declared in two files, with different members");
                }
                _ => {
                    interfaces.insert(name, signatures);
                }
            }
        }
    }
    Ok(interfaces)
}

/// Parses one introspection document.
///
/// # Errors
/// If the XML is not well-formed.
pub fn parse_xml(text: &str) -> Result<BTreeMap<String, InterfaceSignatures>> {
    let mut reader = quick_xml::Reader::from_str(text);
    reader.config_mut().trim_text(true);

    let mut interfaces: BTreeMap<String, InterfaceSignatures> = BTreeMap::new();
    let mut interface: Option<String> = None;
    // The element name and member name of the method or signal currently open.
    let mut member: Option<(String, String)> = None;
    let mut inputs = String::new();
    let mut outputs = String::new();

    loop {
        // `Start` and `Empty` have to be told apart: `<method name="X"/>` never gets an `End`, so
        // its signature is written out on the spot.
        let (element, self_closing) = match reader.read_event().context("reading XML")? {
            Event::Eof => break,
            Event::Start(element) => (element, false),
            Event::Empty(element) => (element, true),
            Event::End(element) => {
                let name = String::from_utf8_lossy(element.name().as_ref()).into_owned();
                match name.as_str() {
                    "interface" => interface = None,
                    "method" | "signal" => {
                        if let (Some(interface), Some((kind, member_name))) =
                            (interface.as_ref(), member.take())
                        {
                            flush(
                                &mut interfaces,
                                interface,
                                &kind,
                                member_name,
                                &inputs,
                                &outputs,
                            );
                        }
                        inputs.clear();
                        outputs.clear();
                    }
                    _ => {}
                }
                continue;
            }
            _ => continue,
        };

        let name = String::from_utf8_lossy(element.name().as_ref()).into_owned();
        match name.as_str() {
            "interface" => {
                interface = attribute(&element, "name");
                if let Some(name) = &interface {
                    interfaces.entry(name.clone()).or_default();
                }
            }
            "method" | "signal" => {
                inputs.clear();
                outputs.clear();
                let member_name = attribute(&element, "name");
                if self_closing {
                    if let (Some(interface), Some(member_name)) = (interface.as_ref(), member_name)
                    {
                        flush(&mut interfaces, interface, &name, member_name, "", "");
                    }
                } else {
                    member = member_name.map(|member| (name.clone(), member));
                }
            }
            "property" => {
                if let (Some(interface), Some(property), Some(kind)) = (
                    interface.as_ref(),
                    attribute(&element, "name"),
                    attribute(&element, "type"),
                ) {
                    let access = attribute(&element, "access").unwrap_or_else(|| "read".to_owned());
                    interfaces
                        .entry(interface.clone())
                        .or_default()
                        .properties
                        .insert(property, format!("{kind} {access}"));
                }
            }
            "arg" => {
                if let Some(kind) = attribute(&element, "type") {
                    // A signal's arguments carry no direction and are all outbound; a method's
                    // D-Bus default is "in".
                    let outbound = match attribute(&element, "direction").as_deref() {
                        Some("out") => true,
                        Some(_) => false,
                        None => member.as_ref().is_some_and(|(kind, _)| kind == "signal"),
                    };
                    if outbound {
                        outputs.push_str(&kind);
                    } else {
                        inputs.push_str(&kind);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(interfaces)
}

/// Records one finished member's signature.
fn flush(
    interfaces: &mut BTreeMap<String, InterfaceSignatures>,
    interface: &str,
    kind: &str,
    member: String,
    inputs: &str,
    outputs: &str,
) {
    let entry = interfaces.entry(interface.to_owned()).or_default();
    if kind == "method" {
        entry
            .methods
            .insert(member, method_signature(inputs, outputs));
    } else {
        entry.signals.insert(member, format!("({outputs})"));
    }
}

fn attribute(element: &quick_xml::events::BytesStart<'_>, key: &str) -> Option<String> {
    element.attributes().flatten().find_map(|attribute| {
        (attribute.key.as_ref() == key.as_bytes())
            .then(|| String::from_utf8_lossy(&attribute.value).into_owned())
    })
}

/// What changed between the baseline and the tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Change {
    /// A whole interface that was not there before. Fine.
    InterfaceAdded(String),
    /// A member that was not there before. Fine — additive growth on a shipped version.
    MemberAdded { interface: String, member: String },
    /// A member whose arguments changed. Breaks every client built against the old one.
    SignatureChanged {
        interface: String,
        member: String,
        was: String,
        now: String,
    },
    /// A member that is gone.
    MemberRemoved { interface: String, member: String },
    /// A whole interface that is gone.
    InterfaceRemoved(String),
}

impl Change {
    /// Whether this change breaks a client compiled against the shipped interface.
    #[must_use]
    pub fn is_breaking(&self) -> bool {
        matches!(
            self,
            Self::SignatureChanged { .. } | Self::MemberRemoved { .. } | Self::InterfaceRemoved(_)
        )
    }

    /// What to tell the person who made it.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::InterfaceAdded(interface) => format!("{interface}: new interface"),
            Self::MemberAdded { interface, member } => format!("{interface}.{member}: new member"),
            Self::SignatureChanged {
                interface,
                member,
                was,
                now,
            } => format!(
                "{interface}.{member}: signature changed from {was} to {now} — this breaks every \
                 client built against it. {}",
                bump_advice(interface)
            ),
            Self::MemberRemoved { interface, member } => format!(
                "{interface}.{member}: removed — clients still call it. {}",
                bump_advice(interface)
            ),
            Self::InterfaceRemoved(interface) => format!(
                "{interface}: the whole interface is gone. {}",
                bump_advice(interface)
            ),
        }
    }
}

/// "Bump to `org.legeos.Files.Browse2`" — the actual next name, not a general instruction.
fn bump_advice(interface: &str) -> String {
    let trimmed = interface.trim_end_matches(|c: char| c.is_ascii_digit());
    let version: u32 = interface[trimmed.len()..].parse().unwrap_or(1);
    format!("Add {trimmed}{} instead.", version + 1)
}

/// Every difference between a baseline and a parsed tree, in a stable order.
#[must_use]
pub fn diff(
    baseline: &BTreeMap<String, InterfaceSignatures>,
    current: &BTreeMap<String, InterfaceSignatures>,
) -> Vec<Change> {
    let mut changes = Vec::new();
    for (interface, was) in baseline {
        let Some(now) = current.get(interface) else {
            changes.push(Change::InterfaceRemoved(interface.clone()));
            continue;
        };
        for (kind_was, kind_now) in [
            (&was.methods, &now.methods),
            (&was.signals, &now.signals),
            (&was.properties, &now.properties),
        ] {
            for (member, signature) in kind_was {
                match kind_now.get(member) {
                    None => changes.push(Change::MemberRemoved {
                        interface: interface.clone(),
                        member: member.clone(),
                    }),
                    Some(current) if current != signature => {
                        changes.push(Change::SignatureChanged {
                            interface: interface.clone(),
                            member: member.clone(),
                            was: signature.clone(),
                            now: current.clone(),
                        });
                    }
                    Some(_) => {}
                }
            }
            for member in kind_now.keys() {
                if !kind_was.contains_key(member) {
                    changes.push(Change::MemberAdded {
                        interface: interface.clone(),
                        member: member.clone(),
                    });
                }
            }
        }
    }
    for interface in current.keys() {
        if !baseline.contains_key(interface) {
            changes.push(Change::InterfaceAdded(interface.clone()));
        }
    }
    changes
}

/// `cargo xtask interface-baseline` — freeze today's signatures.
///
/// # Errors
/// If the tree cannot be parsed or the baseline cannot be written.
pub fn write_baseline(root: &Path) -> Result<()> {
    let interfaces = parse_tree(root)?;
    let members: usize = interfaces
        .values()
        .map(|i| i.methods.len() + i.signals.len() + i.properties.len())
        .sum();
    let baseline = Baseline {
        version: 1,
        interfaces,
    };
    let path = root.join(BASELINE);
    let mut json = serde_json::to_string_pretty(&baseline)?;
    json.push('\n');
    std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;
    println!(
        "interface-baseline: froze {} member(s) across {} interface(s) in {BASELINE}",
        members,
        baseline.interfaces.len()
    );
    Ok(())
}

/// `cargo xtask check-interface-signatures`.
///
/// # Errors
/// If the baseline is missing, or any shipped signature changed or disappeared.
pub fn check(root: &Path) -> Result<()> {
    let path = root.join(BASELINE);
    let body = std::fs::read(&path).with_context(|| {
        format!(
            "reading {} — run `cargo run -p xtask -- interface-baseline` to create it",
            path.display()
        )
    })?;
    let baseline: Baseline =
        serde_json::from_slice(&body).with_context(|| format!("parsing {}", path.display()))?;
    let current = parse_tree(root)?;
    let changes = diff(&baseline.interfaces, &current);

    let breaking: Vec<&Change> = changes.iter().filter(|c| c.is_breaking()).collect();
    let additions = changes.len() - breaking.len();
    if !breaking.is_empty() {
        for change in &breaking {
            eprintln!("check-interface-signatures: {}", change.describe());
        }
        bail!(
            "{} shipped signature(s) changed. Shipped signatures are frozen (ARCHITECTURE.md); \
             if a change really is intended and the old interface is genuinely unused, re-freeze \
             with `cargo run -p xtask -- interface-baseline`.",
            breaking.len()
        );
    }
    let members: usize = current
        .values()
        .map(|i| i.methods.len() + i.signals.len() + i.properties.len())
        .sum();
    if additions > 0 {
        println!(
            "check-interface-signatures: {members} member(s) across {} interface(s) unchanged; \
             {additions} new — run `cargo run -p xtask -- interface-baseline` to freeze them",
            current.len()
        );
    } else {
        println!(
            "check-interface-signatures: {members} shipped member(s) across {} interface(s), all \
             unchanged",
            current.len()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = r#"
<node>
  <interface name="org.legeos.Test1">
    <method name="Status">
      <arg name="level" type="s" direction="out"/>
      <arg name="observe_only" type="b" direction="out"/>
    </method>
    <method name="SetHints">
      <arg name="instance_id" type="s" direction="in"/>
      <arg name="hints_json" type="s" direction="in"/>
    </method>
    <signal name="Changed">
      <arg name="what" type="s"/>
    </signal>
    <property name="Level" type="s" access="read"/>
  </interface>
</node>
"#;

    fn parsed() -> BTreeMap<String, InterfaceSignatures> {
        parse_xml(XML).unwrap()
    }

    #[test]
    fn a_method_signature_is_its_argument_types_in_and_out() {
        let interfaces = parsed();
        let test = &interfaces["org.legeos.Test1"];
        assert_eq!(test.methods["Status"], "()->(sb)");
        assert_eq!(test.methods["SetHints"], "(ss)->()");
        assert_eq!(test.signals["Changed"], "(s)");
        assert_eq!(test.properties["Level"], "s read");
    }

    #[test]
    fn argument_names_are_not_part_of_a_signature() {
        // Renaming an argument breaks no caller. A check that failed on it would be noise, and
        // noise is what teaches people to re-freeze without reading.
        let renamed = XML.replace("name=\"instance_id\"", "name=\"instance\"");
        assert_eq!(parse_xml(&renamed).unwrap(), parsed());
    }

    #[test]
    fn a_new_member_is_not_a_breaking_change() {
        let mut baseline = parsed();
        baseline
            .get_mut("org.legeos.Test1")
            .unwrap()
            .methods
            .remove("SetHints");
        let changes = diff(&baseline, &parsed());
        assert_eq!(changes.len(), 1);
        assert!(!changes[0].is_breaking(), "{changes:?}");
    }

    #[test]
    fn an_added_argument_is_a_breaking_change_that_names_the_next_interface() {
        let widened = XML.replace(
            "<arg name=\"hints_json\" type=\"s\" direction=\"in\"/>",
            "<arg name=\"hints_json\" type=\"s\" direction=\"in\"/>\n\
             <arg name=\"extra\" type=\"u\" direction=\"in\"/>",
        );
        let changes = diff(&parsed(), &parse_xml(&widened).unwrap());
        assert_eq!(changes.len(), 1);
        assert!(changes[0].is_breaking());
        let described = changes[0].describe();
        assert!(described.contains("(ss)->() to (ssu)->()"), "{described}");
        assert!(described.contains("org.legeos.Test2"), "{described}");
    }

    #[test]
    fn a_removed_member_is_a_breaking_change() {
        let shrunk = XML.replace(
            "<signal name=\"Changed\">\n      <arg name=\"what\" type=\"s\"/>\n    </signal>",
            "",
        );
        let changes = diff(&parsed(), &parse_xml(&shrunk).unwrap());
        assert!(changes.iter().any(
            |change| matches!(change, Change::MemberRemoved { member, .. } if member == "Changed")
        ));
        assert!(changes.iter().any(Change::is_breaking));
    }

    #[test]
    fn a_changed_property_type_is_breaking() {
        let retyped = XML.replace(
            "<property name=\"Level\" type=\"s\"",
            "<property name=\"Level\" type=\"u\"",
        );
        let changes = diff(&parsed(), &parse_xml(&retyped).unwrap());
        assert_eq!(changes.len(), 1);
        assert!(changes[0].is_breaking());
    }

    #[test]
    fn a_whole_interface_disappearing_is_breaking() {
        let changes = diff(&parsed(), &BTreeMap::new());
        assert_eq!(changes.len(), 1);
        assert!(matches!(changes[0], Change::InterfaceRemoved(_)));
        assert!(changes[0].describe().contains("org.legeos.Test2"));
    }

    #[test]
    fn a_new_interface_is_not_breaking() {
        let changes = diff(&BTreeMap::new(), &parsed());
        assert_eq!(changes.len(), 1);
        assert!(!changes[0].is_breaking());
    }

    #[test]
    fn the_committed_baseline_still_matches_the_shipped_interfaces() {
        // The gate runs as `cargo xtask check-interface-signatures`; this makes `cargo test`
        // catch the same thing, because a broken shipped signature is the kind of mistake that
        // should not need a separate command to be noticed.
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("repository root");
        let body = std::fs::read(root.join(BASELINE)).expect("interfaces/baseline.json");
        let baseline: Baseline = serde_json::from_slice(&body).expect("parsing the baseline");
        let current = parse_tree(&root).expect("parsing interfaces/dbus");
        let breaking: Vec<String> = diff(&baseline.interfaces, &current)
            .iter()
            .filter(|change| change.is_breaking())
            .map(Change::describe)
            .collect();
        assert!(breaking.is_empty(), "{}", breaking.join("\n"));
    }

    #[test]
    fn every_shipped_interface_parses_and_has_at_least_one_member() {
        // A file that silently parsed to nothing would freeze an empty baseline and then never
        // object to anything, which is the worst failure this check could have.
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("repository root");
        let current = parse_tree(&root).expect("parsing interfaces/dbus");
        assert!(
            current.len() >= 32,
            "only {} interfaces parsed",
            current.len()
        );
        for (name, signatures) in &current {
            assert!(
                !signatures.methods.is_empty()
                    || !signatures.signals.is_empty()
                    || !signatures.properties.is_empty(),
                "{name} parsed to no members at all"
            );
        }
    }

    #[test]
    fn the_bump_advice_names_the_next_version_of_a_multi_digit_interface() {
        assert_eq!(
            bump_advice("org.legeos.Files.Browse1"),
            "Add org.legeos.Files.Browse2 instead."
        );
        assert_eq!(
            bump_advice("org.legeos.Thing9"),
            "Add org.legeos.Thing10 instead."
        );
    }
}
