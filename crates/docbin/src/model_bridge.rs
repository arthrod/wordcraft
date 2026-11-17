//! The shared Word model as WordCraft's document: `wordcraft-model-bridge` converts it member by
//! member. Stories are moved, not copied, and embedded media and opaque streams keep their buffers
//! because both models hold them as the same `Arc<Vec<u8>>`.

use crate::DocbinError;

/// Converts the shared reader's model without copying the full document or its embedded media.
///
/// WordCraft-only fit state is left empty: a `.doc` file has none.
pub fn from_shared_model(source: craft_wordmodel::Document) -> Result<wordcraft_doc::Document, DocbinError> {
    // The bridge carries `missing_pictures`, which the code before it dropped for a `.doc`. The
    // `.doc` reader never fills it today, so nothing changes yet; once it reports missing
    // pictures, they surface here.
    wordcraft_model_bridge::from_shared(source).map_err(|error| DocbinError::Malformed(error.to_string()))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn bridges_nested_stories_and_moves_media_without_copying() {
        let mut source = craft_wordmodel::Document::new();
        let long_text = "x".repeat(4 << 20);
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
        let image = Arc::new(vec![0x5a; 1 << 20]);
        source.media.insert("image1.png".into(), image.clone());

        let document = from_shared_model(source).expect("model conversion");
        assert_eq!(document.body.len(), 2);
        assert_eq!(document.body[0].as_para().map(|p| p.text.as_str()), Some(long_text.as_str()));
        let group = match &document.body[0].as_para().expect("paragraph").objects[0] {
            wordcraft_doc::para::InlineObject::Group { children, ch_w, .. } => {
                assert_eq!(*ch_w, 64.0);
                children
            }
            other => panic!("expected grouped object, got {other:?}"),
        };
        assert_eq!((group[0].x, group[0].y), (4.0, 6.0));
        assert!(
            matches!(&group[0].obj, wordcraft_doc::para::InlineObject::Field { instr, result, locked: true, .. } if instr == "PAGE" && result == "7")
        );
        let table = document.body[1].as_table().expect("table block");
        assert_eq!(table.rows[0].cells[0].blocks[0].as_para().map(|p| p.text.as_str()), Some("nested cell"));
        assert_eq!(document.parts[&12].blocks[0].as_para().map(|p| p.text.as_str()), Some("part story"));
        assert_eq!(document.settings.compat_mode, 11);
        assert!(Arc::ptr_eq(document.media.get("image1.png").expect("moved media"), &image));
        assert!(document.fit_state.is_none());
    }
}
