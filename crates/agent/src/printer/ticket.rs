// SPDX-License-Identifier: GPL-3.0-only

//! PrintTickets of the client printers.
//!
//! The virtual printers' tickets hold the standard settings (`psk:`), the
//! client driver's own settings (in its namespaces: the printer's print
//! capabilities are the client driver's), and settings of the Microsoft
//! Virtual Print Class Driver (`PageDevmodeSnapshot` in its namespace) and
//! of the agent (its namespace), useless to the client. [`for_client`]
//! makes the ticket the client driver prints with out of that.

const PSK: &str = "http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords";
/// Settings of the guest side, left out of the client ticket: the private
/// settings of the Microsoft Virtual Print Class Driver (its
/// `PageDevmodeSnapshot`; the client driver's one, in its own `oemdriverpt`
/// namespace, stays) and the agent's.
const GUEST_NAMESPACES: [&str; 2] = [
    "http://schemas.microsoft.com/windows/printing/oemdriverpt/Microsoft_Virtual_Print",
    "http://schemas.ameleo.fr/2026/printing/client",
];

/// The parts of a PrintTicket: its root element name, `xmlns:` declarations
/// and top level elements (with their `name`).
struct Ticket<'a> {
    root: &'a str,
    declarations: Vec<(&'a str, &'a str)>,
    children: Vec<Child<'a>>,
}

struct Child<'a> {
    name: &'a str,
    xml: &'a str,
}

impl Child<'_> {
    fn prefix(&self) -> &str {
        self.name.split_once(':').map_or("", |(p, _)| p)
    }
}

fn attribute<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let mut rest = tag;
    let pattern = format!("{name}=");
    while let Some(i) = rest.find(&pattern) {
        let boundary = rest[..i].ends_with(|c: char| c.is_whitespace());
        let value = &rest[i + pattern.len()..];
        let quote = value.chars().next()?;
        if boundary && (quote == '"' || quote == '\'') {
            let value = &value[1..];
            return value.find(quote).map(|end| &value[..end]);
        }
        rest = &rest[i + pattern.len()..];
    }
    None
}

fn declarations(tag: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    let mut rest = tag;
    while let Some(i) = rest.find("xmlns:") {
        rest = &rest[i + 6..];
        let Some(eq) = rest.find('=') else { break };
        let prefix = &rest[..eq];
        let value = &rest[eq + 1..];
        let Some(quote) = value.chars().next().filter(|q| *q == '"' || *q == '\'') else {
            break;
        };
        let Some(end) = value[1..].find(quote) else {
            break;
        };
        out.push((prefix.trim(), &value[1..1 + end]));
        rest = &value[1 + end..];
    }
    out
}

/// End of the tag starting at `xml[start]` (`<`): the index after its `>`.
fn tag_end(xml: &str, start: usize) -> Option<usize> {
    let mut quote = None;
    for (i, c) in xml[start..].char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), _) if c == q => quote = None,
            (None, '>') => return Some(start + i + 1),
            _ => {}
        }
    }
    None
}

fn parse(xml: &str) -> Option<Ticket<'_>> {
    // The root: the first element.
    let mut at = 0;
    let (root_start, root_end) = loop {
        let i = at + xml[at..].find('<')?;
        let end = tag_end(xml, i)?;
        if !xml[i + 1..].starts_with(['?', '!']) {
            break (i, end);
        }
        at = end;
    };
    let root_tag = &xml[root_start..root_end];
    let root = root_tag[1..]
        .split(|c: char| c.is_whitespace() || c == '>' || c == '/')
        .next()?;
    if !root.ends_with("PrintTicket") {
        return None;
    }
    let mut children = Vec::new();
    if root_tag.ends_with("/>") {
        return Some(Ticket {
            root,
            declarations: declarations(root_tag),
            children,
        });
    }
    let mut depth = 0usize;
    let mut child_start = 0;
    let mut child_name = "";
    let mut at = root_end;
    while let Some(i) = xml[at..].find('<').map(|i| at + i) {
        let end = tag_end(xml, i)?;
        let tag = &xml[i..end];
        if tag.starts_with("<!--") {
            at = xml[i..].find("-->").map_or(xml.len(), |e| i + e + 3);
            continue;
        }
        if tag.starts_with("<?") || tag.starts_with("<!") {
            at = end;
            continue;
        }
        if tag.starts_with("</") {
            if depth == 0 {
                break; // the root's end
            }
            depth -= 1;
            if depth == 0 {
                children.push(Child {
                    name: child_name,
                    xml: &xml[child_start..end],
                });
            }
        } else {
            if depth == 0 {
                child_start = i;
                child_name = attribute(tag, "name").unwrap_or("");
            }
            if tag.ends_with("/>") {
                if depth == 0 {
                    children.push(Child {
                        name: child_name,
                        xml: tag,
                    });
                }
            } else {
                depth += 1;
            }
        }
        at = end;
    }
    Some(Ticket {
        root,
        declarations: declarations(root_tag),
        children,
    })
}

/// The ticket for the client driver: the guest ticket without the guest
/// side's settings. `None` if `guest` is not a PrintTicket.
pub fn for_client(guest: &str) -> Option<String> {
    let t = parse(guest)?;
    let namespace = |c: &Child| {
        t.declarations
            .iter()
            .find(|(p, _)| *p == c.prefix())
            .map(|(_, u)| *u)
            .unwrap_or("")
    };
    let guest_side = |c: &Child| GUEST_NAMESPACES.iter().any(|g| namespace(c).starts_with(g));
    let mut out = String::from(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    out.push('<');
    out.push_str(t.root);
    // Every declaration: option values may use any of them.
    for (p, u) in &t.declarations {
        out.push_str(&format!(" xmlns:{p}=\"{u}\""));
    }
    out.push_str(r#" version="1">"#);
    for c in t.children.iter().filter(|c| !guest_side(c)) {
        out.push_str(c.xml);
    }
    out.push_str("</");
    out.push_str(t.root);
    out.push('>');
    Some(out)
}

/// The `name` of the top level elements: `(standard, other)`.
pub fn summary(xml: &str) -> (Vec<String>, Vec<String>) {
    let Some(t) = parse(xml) else {
        return (Vec::new(), Vec::new());
    };
    let (mut standard, mut other) = (Vec::new(), Vec::new());
    for c in &t.children {
        let psk = t
            .declarations
            .iter()
            .any(|&(p, u)| p == c.prefix() && u == PSK);
        if psk {
            standard.push(c.name.to_string())
        } else {
            other.push(c.name.to_string())
        }
    }
    (standard, other)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A virtual printer job ticket (shape of a real one).
    const GUEST: &str = r#"<?xml version="1.0" encoding="UTF-8"?><psf:PrintTicket xmlns:psf="http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:xsd="http://www.w3.org/2001/XMLSchema" xmlns:ns0000="http://schemas.microsoft.com/windows/printing/oemdriverpt/Microsoft_Virtual_Print Class Driver/10.0.26100.9711/" xmlns:psk="http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords" xmlns:ns0001="http://schemas.ameleo.fr/2026/printing/client" xmlns:ns0002="http://example.com/sharp" xmlns:ns0003="http://schemas.microsoft.com/windows/printing/oemdriverpt/SHARP BP-50C45 PCL6/1.0/" version="1">
<psf:ParameterInit name="ns0003:PageDevmodeSnapshot"><psf:Value xsi:type="xsd:string">QkJC</psf:Value></psf:ParameterInit>
<psf:ParameterInit name="ns0000:PageDevmodeSnapshot"><psf:Value xsi:type="xsd:string">AAAA</psf:Value></psf:ParameterInit>
<psf:ParameterInit name="psk:JobCopiesAllDocuments"><psf:Value xsi:type="xsd:integer">2</psf:Value></psf:ParameterInit>
<psf:Feature name="psk:PageMediaSize"><psf:Option name="psk:ISOA4"><psf:ScoredProperty name="psk:MediaSizeWidth"><psf:Value xsi:type="xsd:integer">210000</psf:Value></psf:ScoredProperty></psf:Option></psf:Feature>
<psf:Feature name="psk:PageResolution"><psf:Option name="ns0001:Resolution600"/></psf:Feature>
<psf:Feature name="ns0001:JobTestFeature"><psf:Option name="ns0001:Off"/></psf:Feature>
<psf:Feature name="ns0002:PageWatermark"><psf:Option name="ns0002:Confidential"/></psf:Feature>
</psf:PrintTicket>"#;

    #[test]
    fn ticket_for_the_client_driver() {
        let t = for_client(GUEST).unwrap();
        let (standard, other) = summary(&t);
        assert_eq!(
            standard,
            [
                "psk:JobCopiesAllDocuments",
                "psk:PageMediaSize",
                "psk:PageResolution"
            ],
            "{t}"
        );
        // The client driver's snapshot stays, the virtual driver's goes.
        assert_eq!(
            other,
            ["ns0003:PageDevmodeSnapshot", "ns0002:PageWatermark"],
            "{t}"
        );
        assert!(t.contains("QkJC") && !t.contains("AAAA"), "{t}");
        // Declarations kept: option values may use them.
        assert!(
            t.contains(r#"xmlns:ns0001="http://schemas.ameleo.fr/2026/printing/client""#),
            "{t}"
        );
        assert!(
            t.contains("210000") && t.ends_with("</psf:PrintTicket>"),
            "{t}"
        );
        assert_eq!(for_client("not xml"), None);
    }
}
