//! A bounded element tree of an XML document, for `pom.xml` and the MSBuild project files.
//!
//! Only elements, attributes and text are kept. The predefined entities and character references are
//! resolved; any other entity, and every document type declaration, is ignored or refused, so nothing in
//! the document is fetched or expanded.

use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};

/// Deepest nesting read.
pub const MAX_DEPTH: usize = 64;
/// Most elements in one document.
pub const MAX_ELEMENTS: usize = 500_000;

#[derive(Debug, Default)]
pub struct Node {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub text: String,
    pub children: Vec<Node>,
}

impl Node {
    pub fn attr(&self, key: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name == name)
    }

    pub fn named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> {
        self.children.iter().filter(move |c| c.name == name)
    }

    /// The trimmed text of the child `name`, when there is one and it is not empty.
    pub fn text_of(&self, name: &str) -> Option<String> {
        let t = self.child(name)?.text.trim();
        (!t.is_empty()).then(|| t.to_owned())
    }
}

fn local(name: &str) -> &str {
    name.rsplit_once(':').map_or(name, |(_, l)| l)
}

fn open(e: &BytesStart<'_>) -> Result<Node, String> {
    let mut attrs = Vec::new();
    for a in e.attributes() {
        let a = a.map_err(|e| e.to_string())?;
        let value = a
            .normalized_value(XmlVersion::Implicit1_0)
            .map_err(|e| e.to_string())?
            .into_owned();
        attrs.push((local(a.key.as_ref()).to_owned(), value));
    }
    Ok(Node {
        name: local(e.name().as_ref()).to_owned(),
        attrs,
        text: String::new(),
        children: Vec::new(),
    })
}

/// Parses `text` into its root element.
pub fn parse(text: &str) -> Result<Node, String> {
    let mut reader = Reader::from_str(text);
    let mut stack: Vec<Node> = Vec::new();
    let mut root: Option<Node> = None;
    let mut count = 0usize;
    let mut close = |n: Node, stack: &mut Vec<Node>| -> Result<(), String> {
        match stack.last_mut() {
            Some(parent) => parent.children.push(n),
            None if root.is_none() => root = Some(n),
            None => return Err("more than one root element".into()),
        }
        Ok(())
    };
    loop {
        let ev = reader.read_event().map_err(|e| e.to_string())?;
        match ev {
            Event::Start(e) => {
                count += 1;
                if count > MAX_ELEMENTS || stack.len() >= MAX_DEPTH {
                    return Err("the document is too large or too deeply nested".into());
                }
                stack.push(open(&e)?);
            }
            Event::Empty(e) => {
                count += 1;
                if count > MAX_ELEMENTS || stack.len() >= MAX_DEPTH {
                    return Err("the document is too large or too deeply nested".into());
                }
                let n = open(&e)?;
                close(n, &mut stack)?;
            }
            Event::End(_) => {
                let n = stack.pop().ok_or("unbalanced end tag")?;
                close(n, &mut stack)?;
            }
            Event::Text(t) => {
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&t.xml10_content());
                }
            }
            Event::CData(t) => {
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&t.xml10_content());
                }
            }
            Event::GeneralRef(r) => {
                let resolved = match r.resolve_char_ref() {
                    Ok(Some(c)) => c.to_string(),
                    Ok(None) => resolve_predefined_entity(&r)
                        .map(str::to_owned)
                        .ok_or_else(|| format!("unknown entity &{};", &*r))?,
                    Err(e) => return Err(e.to_string()),
                };
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&resolved);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !stack.is_empty() {
        return Err("an element is not closed".into());
    }
    root.ok_or_else(|| "the document has no root element".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elements_attributes_and_text_are_kept() {
        let n = parse(
            "<?xml version=\"1.0\"?>\n<!-- c --><project xmlns=\"u\"><a x=\"1&amp;2\">t &lt; <![CDATA[<c>]]></a><b/><m:n>z</m:n></project>",
        )
        .unwrap();
        assert_eq!(n.name, "project");
        let a = n.child("a").unwrap();
        assert_eq!(a.attr("x"), Some("1&2"));
        assert_eq!(a.text, "t < <c>");
        assert!(n.child("b").is_some());
        assert_eq!(n.text_of("n").as_deref(), Some("z"));
        assert_eq!(n.named("a").count(), 1);
    }

    #[test]
    fn malformed_and_hostile_documents_are_errors() {
        for bad in [
            "",
            "<a>",
            "<a></b>",
            "<a/><b/>",
            "<a>&bomb;</a>",
            "just text",
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        let deep = format!(
            "{}{}",
            "<a>".repeat(MAX_DEPTH + 1),
            "</a>".repeat(MAX_DEPTH + 1)
        );
        assert!(parse(&deep).is_err());
        let wide = format!("<r>{}</r>", "<e/>".repeat(MAX_ELEMENTS));
        assert!(parse(&wide).is_err());
        // A DOCTYPE with an entity is not expanded: the reference is unknown.
        let xxe = "<!DOCTYPE r [<!ENTITY x SYSTEM \"file:///etc/passwd\">]><r>&x;</r>";
        assert!(parse(xxe).is_err());
    }
}
