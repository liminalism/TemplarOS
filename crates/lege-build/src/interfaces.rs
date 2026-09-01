//! D-Bus interface signatures: the shape of what is shipped, and what changed.
//!
//! `interfaces/README.md` has always said that changing a shipped signature means a new versioned
//! interface rather than an edit. These are the types that let that be checked instead of
//! remembered — by `cargo xtask check-interface-signatures` against the committed baseline, and
//! by `lege dev deploy` against the previous build set, which is the same question asked at the
//! other end: can this new build's daemons be restarted under the clients the old set left
//! running, or does the whole session have to come round together?
//!
//! Parsing the XML lives in the dev tool (it needs an XML parser no service should link). This is
//! only the vocabulary and the comparison, which both ends share so they cannot disagree about
//! what "changed" means.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// How a member's signature is written down.
///
/// `(in types)->(out types)` for a method, `(types)` for a signal, `type access` for a property —
/// D-Bus type codes throughout, which is what a client is actually compiled against. Argument
/// *names* are deliberately excluded: renaming an argument breaks no caller, and a check that
/// failed on it would train people to ignore it.
#[must_use]
pub fn method_signature(inputs: &str, outputs: &str) -> String {
    format!("({inputs})->({outputs})")
}

/// Every member of one interface, by kind, each mapped to its signature.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct InterfaceSignatures {
    #[serde(default)]
    pub methods: BTreeMap<String, String>,
    #[serde(default)]
    pub signals: BTreeMap<String, String>,
    #[serde(default)]
    pub properties: BTreeMap<String, String>,
}

/// The committed baseline.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Baseline {
    pub version: u32,
    pub interfaces: BTreeMap<String, InterfaceSignatures>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bump_advice_names_the_next_version_of_the_interface() {
        // The sentence someone needs at the moment the gate stops them, not a general
        // instruction they then have to apply.
        assert_eq!(
            bump_advice("org.legeos.Files.Browse1"),
            "Add org.legeos.Files.Browse2 instead."
        );
        assert_eq!(
            bump_advice("org.legeos.Thing9"),
            "Add org.legeos.Thing10 instead."
        );
    }

    #[test]
    fn only_a_change_or_a_removal_breaks_a_client() {
        assert!(!Change::InterfaceAdded("org.legeos.New1".to_owned()).is_breaking());
        assert!(
            !Change::MemberAdded {
                interface: "org.legeos.Test1".to_owned(),
                member: "Extra".to_owned(),
            }
            .is_breaking()
        );
        assert!(
            Change::MemberRemoved {
                interface: "org.legeos.Test1".to_owned(),
                member: "Status".to_owned(),
            }
            .is_breaking()
        );
        assert!(
            Change::SignatureChanged {
                interface: "org.legeos.Test1".to_owned(),
                member: "Status".to_owned(),
                was: "()->(s)".to_owned(),
                now: "()->(ss)".to_owned(),
            }
            .is_breaking()
        );
        assert!(Change::InterfaceRemoved("org.legeos.Test1".to_owned()).is_breaking());
    }
}
