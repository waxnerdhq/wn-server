//! Renders a response as Subsonic XML.
//!
//! The response models are serialised to `serde_json::Value`. This module
//! walks that value, so one set of models serves JSON and XML.

use std::borrow::Cow;
use std::io;

use quick_xml::Writer;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use serde_json::{Map, Value};

/// The name of the root element.
const ROOT: &str = "subsonic-response";

/// The namespace of the Subsonic REST API.
const XMLNS: &str = "http://subsonic.org/restapi";

/// The key whose scalar value becomes the text content of its element.
const TEXT_KEY: &str = "value";

/// Renders a Subsonic XML document.
///
/// The document starts with an XML declaration that names UTF-8. The root
/// element is `subsonic-response`. It carries `xmlns` and then `root_attrs` in
/// the given order. `payload` is the JSON object whose entries fill the root
/// element. A payload that is not an object adds nothing.
///
/// The entries of an object are rendered by these rules:
///
/// - A string, number or boolean becomes an attribute named after its key.
///   Booleans are written as `true` and `false`.
/// - A string, number or boolean under the key `value` becomes the text
///   content of the element.
/// - An object becomes a child element named after its key.
/// - An array becomes one child element per item. Each is named after the key
///   of the array. An item that is a string, number or boolean becomes an
///   element with that item as text content.
/// - `null` writes nothing. An empty array writes nothing.
///
/// Attributes and child elements follow the key order of the JSON object.
/// Attribute values and text are escaped. Characters that XML 1.0 does not
/// allow in a document are dropped. Keys are written as they are, so every key
/// must be an XML name.
pub fn to_xml(root_attrs: &[(&str, &str)], payload: &Value) -> String {
    let mut writer = Writer::new(Vec::new());
    write_document(&mut writer, root_attrs, payload).expect("writing to a Vec<u8> does not fail");
    String::from_utf8(writer.into_inner()).expect("the document is built from UTF-8 strings")
}

fn write_document(
    writer: &mut Writer<Vec<u8>>,
    root_attrs: &[(&str, &str)],
    payload: &Value,
) -> io::Result<()> {
    writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))?;
    let mut root = BytesStart::new(ROOT);
    root.push_attribute(("xmlns", XMLNS));
    for (name, value) in root_attrs {
        root.push_attribute((*name, xml_chars(value).as_ref()));
    }
    let no_fields = Map::new();
    write_element(writer, root, payload.as_object().unwrap_or(&no_fields))
}

/// Writes one element. `start` holds its name and the attributes it has so
/// far. `fields` adds attributes, text content and child elements.
fn write_element(
    writer: &mut Writer<Vec<u8>>,
    mut start: BytesStart<'_>,
    fields: &Map<String, Value>,
) -> io::Result<()> {
    let mut text = None;
    let mut children = Vec::new();
    for (key, value) in fields {
        match scalar_text(value) {
            Some(scalar) if key == TEXT_KEY => text = Some(scalar),
            Some(scalar) => start.push_attribute((key.as_str(), scalar.as_ref())),
            None if writes_element(value) => children.push((key, value)),
            None => {}
        }
    }
    if text.is_none() && children.is_empty() {
        return writer.write_event(Event::Empty(start));
    }
    writer.write_event(Event::Start(start.borrow()))?;
    if let Some(text) = &text {
        writer.write_event(Event::Text(BytesText::new(text)))?;
    }
    for (key, value) in children {
        write_named(writer, key, value)?;
    }
    writer.write_event(Event::End(start.to_end()))
}

/// Writes `value` as elements named `name`: one element for an object or a
/// scalar, one element per item for an array, and nothing for `null`.
fn write_named(writer: &mut Writer<Vec<u8>>, name: &str, value: &Value) -> io::Result<()> {
    if let Some(text) = scalar_text(value) {
        writer.write_event(Event::Start(BytesStart::new(name)))?;
        writer.write_event(Event::Text(BytesText::new(&text)))?;
        return writer.write_event(Event::End(BytesEnd::new(name)));
    }
    match value {
        Value::Array(items) => items
            .iter()
            .try_for_each(|item| write_named(writer, name, item)),
        Value::Object(fields) => write_element(writer, BytesStart::new(name), fields),
        _ => Ok(()),
    }
}

/// Whether `value` writes at least one element. `null` writes none. An array
/// writes none when it is empty or when none of its items writes one.
fn writes_element(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Array(items) => items.iter().any(writes_element),
        _ => true,
    }
}

/// The text of a string, number or boolean. `None` for any other value.
fn scalar_text(value: &Value) -> Option<Cow<'_, str>> {
    match value {
        Value::Bool(true) => Some(Cow::Borrowed("true")),
        Value::Bool(false) => Some(Cow::Borrowed("false")),
        Value::Number(number) => Some(Cow::Owned(number.to_string())),
        Value::String(text) => Some(xml_chars(text)),
        Value::Null | Value::Array(_) | Value::Object(_) => None,
    }
}

/// Returns `text` without the characters that XML 1.0 does not allow. A strict
/// parser rejects a document that holds one, escaped or not.
fn xml_chars(text: &str) -> Cow<'_, str> {
    if text.chars().all(is_xml_char) {
        Cow::Borrowed(text)
    } else {
        Cow::Owned(text.chars().filter(|c| is_xml_char(*c)).collect())
    }
}

/// Whether XML 1.0 allows `c` in a document. This is the `Char` production of
/// the XML 1.0 specification. A Rust `char` is never a surrogate.
fn is_xml_char(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\r' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..
    )
}

#[cfg(test)]
mod tests {
    use quick_xml::escape::resolve_predefined_entity;
    use quick_xml::{Reader, XmlVersion};
    use serde_json::json;

    use super::*;

    const DECLARATION: &str = r#"<?xml version="1.0" encoding="UTF-8"?>"#;

    /// Text with every character the renderer escapes, and non-ASCII text.
    const SPECIAL: &str = "Tom & Jerry <\"live\"> 'B' Björk — 日本語 🎵";

    /// One element of a parsed document.
    #[derive(Debug, Default, PartialEq)]
    struct Element {
        name: String,
        attrs: Vec<(String, String)>,
        text: String,
        children: Vec<Element>,
    }

    impl Element {
        fn attr(&self, name: &str) -> Option<&str> {
            self.attrs
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
        }
    }

    fn element(start: &BytesStart<'_>) -> Element {
        let attrs = start
            .attributes()
            .map(|attr| {
                let attr = attr.expect("the attribute is well-formed");
                let value = attr
                    .normalized_value(XmlVersion::Explicit1_0)
                    .expect("the attribute value is well-formed");
                (attr.key.0.to_owned(), value.into_owned())
            })
            .collect();
        Element {
            name: start.name().0.to_owned(),
            attrs,
            ..Element::default()
        }
    }

    /// Parses `xml` with `quick_xml::Reader` and returns the root element.
    /// Panics when the reader reports an error, when an element stays open,
    /// or when the document does not hold exactly one root element.
    fn parse(xml: &str) -> Element {
        let mut reader = Reader::from_str(xml);
        // The first entry collects the root element. Every other entry is an
        // open element.
        let mut open = vec![Element::default()];
        loop {
            let event = reader.read_event().expect("the document is well-formed");
            match event {
                Event::Decl(_) => {}
                Event::Start(start) => open.push(element(&start)),
                Event::Empty(start) => {
                    let parent = open.last_mut().expect("the collector is present");
                    parent.children.push(element(&start));
                }
                Event::End(_) => {
                    let closed = open.pop().expect("an element is open");
                    let parent = open.last_mut().expect("the end tag has a start tag");
                    parent.children.push(closed);
                }
                Event::Text(text) => {
                    let parent = open.last_mut().expect("the collector is present");
                    parent.text.push_str(&text.xml10_content());
                }
                Event::GeneralRef(reference) => {
                    let parent = open.last_mut().expect("the collector is present");
                    match reference
                        .resolve_char_ref()
                        .expect("the reference is well-formed")
                    {
                        Some(c) => parent.text.push(c),
                        None => parent.text.push_str(
                            resolve_predefined_entity(&reference)
                                .expect("the entity is predefined"),
                        ),
                    }
                }
                Event::Eof => break,
                other => panic!("unexpected event: {other:?}"),
            }
        }
        assert_eq!(open.len(), 1, "an element stays open");
        let mut roots = open.pop().expect("the collector is present").children;
        assert_eq!(roots.len(), 1, "the document has one root element");
        roots.remove(0)
    }

    /// Renders `payload` without root attributes and returns what the root
    /// element contains.
    fn content(payload: Value) -> String {
        let xml = to_xml(&[], &payload);
        let open = format!(r#"{DECLARATION}<{ROOT} xmlns="{XMLNS}">"#);
        let close = format!("</{ROOT}>");
        xml.strip_prefix(&open)
            .and_then(|rest| rest.strip_suffix(&close))
            .unwrap_or_else(|| panic!("the root element has no content: {xml}"))
            .to_owned()
    }

    #[test]
    fn document_has_utf8_declaration_and_namespaced_root() {
        assert_eq!(
            to_xml(&[], &json!({})),
            concat!(
                r#"<?xml version="1.0" encoding="UTF-8"?>"#,
                r#"<subsonic-response xmlns="http://subsonic.org/restapi"/>"#
            )
        );
    }

    #[test]
    fn root_attributes_follow_xmlns_in_the_given_order() {
        let xml = to_xml(&[("status", "ok"), ("b", "2"), ("a", "1")], &json!({}));
        assert_eq!(
            xml,
            format!(r#"{DECLARATION}<{ROOT} xmlns="{XMLNS}" status="ok" b="2" a="1"/>"#)
        );
    }

    #[test]
    fn payload_that_is_not_an_object_adds_nothing() {
        let empty = to_xml(&[], &json!({}));
        for payload in [json!(null), json!("text"), json!(7), json!([{ "a": 1 }])] {
            assert_eq!(to_xml(&[], &payload), empty);
        }
    }

    #[test]
    fn scalars_become_attributes() {
        let payload = json!({ "song": {
            "bitRate": 320,
            "id": "tr-1",
            "rating": 4.5,
            "starred": false,
            "year": -1,
        }});
        assert_eq!(
            content(payload),
            r#"<song bitRate="320" id="tr-1" rating="4.5" starred="false" year="-1"/>"#
        );
    }

    #[test]
    fn booleans_are_written_as_true_and_false() {
        let payload = json!({ "license": { "trial": false, "valid": true } });
        assert_eq!(content(payload), r#"<license trial="false" valid="true"/>"#);
    }

    #[test]
    fn objects_become_child_elements() {
        let payload = json!({ "album": {
            "id": "al-1",
            "replayGain": { "peak": { "track": 1 } },
        }});
        assert_eq!(
            content(payload),
            r#"<album id="al-1"><replayGain><peak track="1"/></replayGain></album>"#
        );
    }

    #[test]
    fn arrays_become_repeated_elements_named_after_the_key() {
        let payload = json!({ "artists": { "index": [
            { "artist": [{ "id": "ar-1" }, { "id": "ar-2" }], "name": "A" },
            { "artist": [{ "id": "ar-3" }], "name": "B" },
        ]}});
        assert_eq!(
            content(payload),
            concat!(
                r#"<artists>"#,
                r#"<index name="A"><artist id="ar-1"/><artist id="ar-2"/></index>"#,
                r#"<index name="B"><artist id="ar-3"/></index>"#,
                r#"</artists>"#
            )
        );
    }

    #[test]
    fn array_at_the_payload_key_repeats_the_element_in_the_root() {
        let payload = json!({ "extension": [{ "name": "a" }, { "name": "b" }] });
        assert_eq!(
            content(payload),
            r#"<extension name="a"/><extension name="b"/>"#
        );
    }

    #[test]
    fn scalar_array_items_become_elements_with_text() {
        let payload = json!({ "extension": { "name": "songLyrics", "versions": [1, 2] } });
        assert_eq!(
            content(payload),
            concat!(
                r#"<extension name="songLyrics">"#,
                r#"<versions>1</versions><versions>2</versions>"#,
                r#"</extension>"#
            )
        );
    }

    #[test]
    fn value_key_becomes_text_content() {
        let payload = json!({ "genres": { "genre": [
            { "albumCount": 1, "songCount": 3, "value": "Rock" },
            { "albumCount": 2, "songCount": 9, "value": "Jazz" },
        ]}});
        assert_eq!(
            content(payload),
            concat!(
                r#"<genres>"#,
                r#"<genre albumCount="1" songCount="3">Rock</genre>"#,
                r#"<genre albumCount="2" songCount="9">Jazz</genre>"#,
                r#"</genres>"#
            )
        );
    }

    #[test]
    fn value_key_that_is_not_a_scalar_becomes_a_child_element() {
        let payload = json!({ "entry": { "value": { "id": 1 } } });
        assert_eq!(content(payload), r#"<entry><value id="1"/></entry>"#);
    }

    #[test]
    fn null_and_empty_arrays_write_nothing() {
        let payload = json!({ "album": {
            "coverArt": null,
            "id": "al-1",
            "song": [],
            "value": null,
        }});
        assert_eq!(content(payload), r#"<album id="al-1"/>"#);

        let payload = json!({ "album": { "song": [null, { "id": "tr-1" }, []] } });
        assert_eq!(content(payload), r#"<album><song id="tr-1"/></album>"#);
    }

    #[test]
    fn special_characters_are_escaped() {
        let payload =
            json!({ "song": { "title": "a & b < c > d \" e", "value": "a & b < c > d \" e" } });
        assert_eq!(
            content(payload),
            concat!(
                r#"<song title="a &amp; b &lt; c &gt; d &quot; e">"#,
                r#"a &amp; b &lt; c &gt; d &quot; e"#,
                r#"</song>"#
            )
        );
    }

    #[test]
    fn special_and_non_ascii_characters_round_trip() {
        let payload = json!({ "song": {
            "comment": [SPECIAL, "second"],
            "title": SPECIAL,
            "value": SPECIAL,
        }});
        let xml = to_xml(&[("message", SPECIAL)], &payload);

        let root = parse(&xml);
        assert_eq!(root.name, ROOT);
        assert_eq!(root.attr("xmlns"), Some(XMLNS));
        assert_eq!(root.attr("message"), Some(SPECIAL));
        assert_eq!(root.children.len(), 1);

        let song = &root.children[0];
        assert_eq!(song.name, "song");
        assert_eq!(song.attrs, [("title".to_owned(), SPECIAL.to_owned())]);
        assert_eq!(song.text, SPECIAL);

        let comments: Vec<&str> = song
            .children
            .iter()
            .map(|comment| {
                assert_eq!(comment.name, "comment");
                comment.text.as_str()
            })
            .collect();
        assert_eq!(comments, [SPECIAL, "second"]);
    }

    #[test]
    fn line_breaks_and_tabs_round_trip() {
        let text = "one\ntwo\r\nthree\tfour\rfive";
        let payload = json!({ "lyrics": { "title": text, "value": text } });

        let root = parse(&to_xml(&[], &payload));
        let lyrics = &root.children[0];
        assert_eq!(lyrics.attr("title"), Some(text));
        assert_eq!(lyrics.text, text);
    }

    #[test]
    fn characters_that_xml_does_not_allow_are_dropped() {
        let text = "a\u{0}b\u{1}c\u{1F}d\u{FFFE}e\u{FFFF}f\tg\u{7F}h";
        let kept = "abcdef\tg\u{7F}h";
        let payload = json!({ "song": { "title": text, "value": text } });

        let xml = to_xml(&[("message", text)], &payload);
        assert!(xml.chars().all(is_xml_char), "{xml:?}");

        let root = parse(&xml);
        assert_eq!(root.attr("message"), Some(kept));
        let song = &root.children[0];
        assert_eq!(song.attr("title"), Some(kept));
        assert_eq!(song.text, kept);
    }

    /// The round-trip tests rely on `parse` to reject a malformed document.
    /// The reader accepts a raw `<` in an attribute value, so
    /// `special_characters_are_escaped` compares that escape as a string.
    #[test]
    fn parse_rejects_malformed_documents() {
        let malformed = [
            r#"<a>x & y</a>"#,
            r#"<a>x < y</a>"#,
            r#"<a b="x & y"/>"#,
            r#"<a b="x " y"/>"#,
            r#"<a b="1" b="2"/>"#,
            r#"<a><b></a>"#,
            r#"<a><b/>"#,
            r#"<a/><b/>"#,
        ];
        for xml in malformed {
            let result = std::panic::catch_unwind(|| parse(xml));
            assert!(result.is_err(), "the parser accepts {xml}");
        }
    }
}
