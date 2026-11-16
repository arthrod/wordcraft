//! Bounded conversion between craft-wordmodel and WordCraft's document model.

use std::sync::Arc;

use craft_docx::DocxError;
use serde::Serialize;
use serde::de::DeserializeOwned;

pub(crate) fn convert<T, U>(value: T) -> Result<U, DocxError>
where
    T: Serialize,
    U: DeserializeOwned,
{
    let bytes = serde_json::to_vec(&value).map_err(|error| DocxError::Xml(format!("shared model conversion: {error}")))?;
    drop(value);
    serde_json::from_slice(&bytes).map_err(|error| DocxError::Xml(format!("shared model conversion: {error}")))
}

pub(crate) fn convert_ref<T, U>(value: &T) -> Result<U, DocxError>
where
    T: Serialize + ?Sized,
    U: DeserializeOwned,
{
    let bytes = serde_json::to_vec(value).map_err(|error| DocxError::Xml(format!("shared model conversion: {error}")))?;
    serde_json::from_slice(&bytes).map_err(|error| DocxError::Xml(format!("shared model conversion: {error}")))
}

/// Converts the shared reader's model without copying the full document or its embedded media.
pub fn from_shared_model(mut source: craft_wordmodel::Document) -> Result<wordcraft_doc::Document, DocxError> {
    let mut fit_state = None;
    let fit_key = source.passthrough.keys().find(|k| k.eq_ignore_ascii_case(crate::fit::PART)).cloned();
    if let Some(key) = fit_key
        && let Some(xml_bytes) = source.passthrough.get(&key)
        && let Some(state) = crate::fit::read_from_bytes(xml_bytes)
    {
        fit_state = Some(state);
        source.passthrough.remove(&key);
    }

    let mut document = wordcraft_model_bridge::from_shared(source).map_err(|error| DocxError::Xml(error.to_string()))?;
    document.fit_state = fit_state;
    Ok(document)
}

/// Converts a WordCraft document to the shared model for serialization.
pub fn to_shared_model(source: &wordcraft_doc::Document) -> Result<craft_wordmodel::Document, DocxError> {
    let mut document = wordcraft_model_bridge::to_shared(source).map_err(|error| DocxError::Xml(error.to_string()))?;

    // Handle fit_state preservation through custom XML passthrough
    if let Some(fit) = &source.fit_state {
        let xml = crate::fit::write(fit)?;
        document.passthrough.insert(crate::fit::PART.into(), Arc::new(xml));
        const CUSTOM_XML_ROOTS: &str = "wordcraft:customXml.roots";
        const EMBEDDED_PARTS: &str = "wordcraft:embedded.parts";

        let manifest_entry = format!("P\t{}\tapplication/xml\n", crate::fit::PART);
        let manifest = match document.passthrough.get_mut(EMBEDDED_PARTS) {
            Some(existing) => {
                let mut text = String::from_utf8_lossy(existing).to_string();
                if !text.contains(crate::fit::PART) {
                    text.push_str(&manifest_entry);
                }
                Arc::new(text.into_bytes())
            }
            None => Arc::new(manifest_entry.into_bytes()),
        };
        document.passthrough.insert(EMBEDDED_PARTS.into(), manifest);

        let root_entry = format!("D\thttp://schemas.openxmlformats.org/officeDocument/2006/relationships/customXml\t{}\n", crate::fit::PART);
        let roots = match document.passthrough.get_mut(CUSTOM_XML_ROOTS) {
            Some(existing) => {
                let mut text = String::from_utf8_lossy(existing).to_string();
                if !text.contains(crate::fit::PART) {
                    text.push_str(&root_entry);
                }
                Arc::new(text.into_bytes())
            }
            None => Arc::new(root_entry.into_bytes()),
        };
        document.passthrough.insert(CUSTOM_XML_ROOTS.into(), roots);
    }

    Ok(document)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridges_nested_stories_and_moves_media_both_directions() {
        let mut source = craft_wordmodel::Document::new();
        let long_text = "x".repeat(1 << 16);
        let mut body_para = craft_wordmodel::para::Paragraph::with_text(&long_text, Default::default());
        body_para.objects.push(craft_wordmodel::para::InlineObject::Group {
            w: 32.0,
            h: 24.0,
            float: Default::default(),
            ch_w: 64.0,
            ch_h: 48.0,
            children: vec![craft_wordmodel::para::GroupChild {
                x: 4.0,
                y: 6.0,
                obj: craft_wordmodel::para::InlineObject::Field {
                    instr: "PAGE".into(),
                    result: "7".into(),
                    locked: true,
                    dirty: false,
                    code: false,
                    form: String::new(),
                },
            }],
            canvas: None,
        });
        let cell = craft_wordmodel::table::Cell::with_text("nested cell");
        let table = craft_wordmodel::table::Table {
            rows: vec![craft_wordmodel::table::Row { cells: vec![cell], ..Default::default() }],
            ..Default::default()
        };
        source.body = vec![Arc::new(craft_wordmodel::Block::Para(body_para)), Arc::new(craft_wordmodel::Block::Table(table))];
        source.parts.insert(
            12,
            craft_wordmodel::Part {
                kind: craft_wordmodel::PartKind::TextBox,
                blocks: vec![craft_wordmodel::para_block(craft_wordmodel::para::Paragraph::with_text("part story", Default::default()))],
                ..Default::default()
            },
        );
        source.settings.compat_mode = 11;
        let image = Arc::new(vec![0x5a; 1 << 16]);
        source.media.insert("image1.png".into(), image.clone());

        let doc = from_shared_model(source).expect("from_shared_model");
        assert_eq!(doc.body.len(), 2);
        assert_eq!(doc.body[0].as_para().map(|p| p.text.as_str()), Some(long_text.as_str()));
        assert!(Arc::ptr_eq(doc.media.get("image1.png").expect("media"), &image));

        let back = to_shared_model(&doc).expect("to_shared_model");
        assert_eq!(back.body.len(), 2);
        assert_eq!(back.body[0].as_para().map(|p| p.text.as_str()), Some(long_text.as_str()));
        assert!(Arc::ptr_eq(back.media.get("image1.png").expect("media"), &image));
    }
}
