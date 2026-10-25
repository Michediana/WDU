//! A small owned XML tree on top of quick-xml.
//!
//! OEM catalogs are a few MB at most, and walking a tree keeps the parsers
//! far simpler than tracking paths in a streaming reader.

use anyhow::{Context, Result, bail};
use quick_xml::XmlVersion;
use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;

#[derive(Debug, Clone, Default)]
pub struct Element {
    /// Local name, namespace prefix stripped.
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Element>,
    text: String,
}

impl Element {
    /// Parses a document and returns its root element.
    pub fn parse(xml: &str) -> Result<Element> {
        let mut reader = Reader::from_str(xml);
        let mut stack: Vec<Element> = Vec::new();
        let mut root = None;

        loop {
            let event = reader
                .read_event()
                .with_context(|| format!("XML error at byte {}", reader.buffer_position()))?;
            match event {
                Event::Start(start) => stack.push(Element::from_start(&start)?),
                Event::Empty(start) => {
                    let element = Element::from_start(&start)?;
                    match stack.last_mut() {
                        Some(parent) => parent.children.push(element),
                        None => root = Some(element),
                    }
                }
                Event::End(_) => {
                    let element = stack.pop().context("unbalanced end tag")?;
                    match stack.last_mut() {
                        Some(parent) => parent.children.push(element),
                        None => root = Some(element),
                    }
                }
                Event::Text(text) => {
                    if let Some(current) = stack.last_mut() {
                        current.text.push_str(&text.xml10_content());
                    }
                }
                Event::CData(cdata) => {
                    if let Some(current) = stack.last_mut() {
                        current.text.push_str(&cdata.into_inner());
                    }
                }
                Event::GeneralRef(reference) => {
                    if let Some(current) = stack.last_mut() {
                        if let Some(ch) = reference.resolve_char_ref()? {
                            current.text.push(ch);
                        } else if let Some(s) = resolve_predefined_entity(&reference) {
                            current.text.push_str(s);
                        }
                    }
                }
                Event::Eof => break,
                _ => {}
            }
        }
        if !stack.is_empty() {
            bail!(
                "unexpected end of document inside <{}>",
                stack[stack.len() - 1].name
            );
        }
        root.context("document has no root element")
    }

    fn from_start(start: &BytesStart) -> Result<Element> {
        let mut attrs = Vec::new();
        for attr in start.attributes() {
            let attr = attr?;
            attrs.push((
                attr.key.local_name().as_ref().to_string(),
                attr.normalized_value(XmlVersion::Implicit1_0)?.into_owned(),
            ));
        }
        Ok(Element {
            name: start.local_name().as_ref().to_string(),
            attrs,
            ..Default::default()
        })
    }

    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.trim().is_empty())
    }

    /// Trimmed text content of this element only (not of its children).
    pub fn text(&self) -> Option<&str> {
        let t = self.text.trim();
        (!t.is_empty()).then_some(t)
    }

    pub fn child(&self, name: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.name == name)
    }

    pub fn children<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }

    /// Follows a path of child names, taking the first match at each step.
    pub fn at(&self, path: &[&str]) -> Option<&Element> {
        path.iter().try_fold(self, |e, name| e.child(name))
    }

    /// Text of the element at `path`.
    pub fn text_at(&self, path: &[&str]) -> Option<String> {
        self.at(path)?.text().map(str::to_string)
    }

    /// All elements named `name` anywhere below this one, depth first.
    pub fn descendants<'a>(&'a self, name: &'a str) -> Vec<&'a Element> {
        let mut out = Vec::new();
        let mut stack: Vec<&Element> = self.children.iter().rev().collect();
        while let Some(e) = stack.pop() {
            if e.name == name {
                out.push(e);
            }
            stack.extend(e.children.iter().rev());
        }
        out
    }
}

/// Decodes catalog bytes honouring a UTF-8 or UTF-16 BOM (Dell ships UTF-16LE).
pub fn decode_text(bytes: &[u8]) -> Result<String> {
    match bytes {
        [0xEF, 0xBB, 0xBF, rest @ ..] => Ok(std::str::from_utf8(rest)?.to_string()),
        [0xFF, 0xFE, rest @ ..] => decode_utf16(rest, u16::from_le_bytes),
        [0xFE, 0xFF, rest @ ..] => decode_utf16(rest, u16::from_be_bytes),
        _ => Ok(std::str::from_utf8(bytes)?.to_string()),
    }
}

fn decode_utf16(bytes: &[u8], from: fn([u8; 2]) -> u16) -> Result<String> {
    if !bytes.len().is_multiple_of(2) {
        bail!("odd length UTF-16 document");
    }
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| from([c[0], c[1]]))
        .collect();
    Ok(String::from_utf16(&units)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tree_cdata_and_entities() {
        let root = Element::parse(
            r#"<?xml version="1.0"?>
            <a:Root xmlns:a="urn:x" k="v &amp; w">
              <Item id="1"><Name><![CDATA[First & best]]></Name></Item>
              <Item id="2"><Name>Second &amp; &#x41;</Name><Empty/></Item>
            </a:Root>"#,
        )
        .unwrap();
        assert_eq!(root.name, "Root");
        assert_eq!(root.attr("k"), Some("v & w"));
        let items: Vec<_> = root.children("Item").collect();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].text_at(&["Name"]).as_deref(), Some("First & best"));
        assert_eq!(items[1].text_at(&["Name"]).as_deref(), Some("Second & A"));
        assert_eq!(root.descendants("Name").len(), 2);
        assert!(items[1].child("Empty").is_some());
    }

    #[test]
    fn decodes_boms() {
        let utf16: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain("<a/>".encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        assert_eq!(decode_text(&utf16).unwrap(), "<a/>");
        assert_eq!(decode_text(b"\xEF\xBB\xBF<a/>").unwrap(), "<a/>");
    }
}
