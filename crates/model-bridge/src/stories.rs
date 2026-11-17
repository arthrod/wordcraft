//! The document, its stories and the types written by hand.
//!
//! Stories convert one way each: the shared model is consumed on the way in (text, runs and
//! objects are moved, not copied), and WordCraft's document is only read on the way out. Each
//! place where the two directions, or the two models, differ is one commented line.

use std::sync::Arc;

use craft_convert::{Convert, ConvertRef, convert_types};
use craft_wordmodel as wm;
use wordcraft_doc as doc;

use crate::{Bridge, session};

convert_types! {
    marker = Bridge;

    enum wm::Block => doc::Block { Para(a), Table(a) }
    enum &doc::Block => wm::Block { Para(a), Table(a) }

    struct wm::para::Paragraph => doc::para::Paragraph {
        text,
        runs,
        props,
        mark,
        objects,
        section,
        // The revision stamp is this process's own (`serde(skip)`): a paragraph made here takes a new one.
        rev => _,
        _ => rev = doc::para::next_rev(),
    }
    struct &doc::para::Paragraph => wm::para::Paragraph {
        text,
        // Formatting written to a file has its spacing and sizes made finite first; see `finite_para` and `finite_char`.
        runs = runs.iter().map(|run| wm::para::Run { len: run.len, props: finite_char(&run.props) }).collect(),
        props = finite_para(props),
        mark = finite_char(mark),
        objects,
        section,
        // The revision stamp is this process's own (`serde(skip)`): a paragraph made here takes a new one.
        rev => _,
        _ => rev = wm::para::next_rev(),
    }
    struct wm::para::Run => doc::para::Run { len, props }

    enum wm::para::InlineObject => doc::para::InlineObject {
        // A picture's size and crop are moved as they are: the JSON bridge never looked at them on the way in.
        Image { media, w = w, h = h, alt, float, crop = crop, fx, ole },
        // Size as for a picture. One graphic drawn in several places stays one graphic.
        Graphic { w = w, h = h, alt, float, graphic = session::graphic(graphic) },
        Shape { kind, w, h, fill, stroke, stroke_width, float, story, freeform, effects, extra },
        // A group's size and coordinate space are moved as they are, as for a picture.
        Group { w = w, h = h, float, ch_w = ch_w, ch_h = ch_h, children, canvas },
        Field { instr, result, locked, dirty, code, form },
        FieldStart { instr, locked, dirty },
        FieldEnd,
        ControlStart { control },
        ControlEnd,
        NoteRef { kind, id, custom },
        BookmarkStart { name },
        BookmarkEnd { name },
        CommentStart { id },
        CommentEnd { id },
        PermStart { id, group, editor },
        PermEnd { id },
        // An equation with no structure loses the rest of `math` (its source XML, justification, linear flag):
        // JSON left out a `math` whose `nodes` were empty, and it read back as the default. Kept as it was.
        Equation { linear, display, math = if math.nodes.is_empty() { doc::math::Math::default() } else { Convert::<_, Bridge>::convert(math) } },
        Ruby { base, ruby, align, size, raise, base_size, lang, props },
        PositionalTab { align, relative_to, leader },
        Opaque { format, xml, text },
    }
    enum &doc::para::InlineObject => wm::para::InlineObject {
        // A picture's size and crop are made finite on the way out (zero stands in), where every other number is refused.
        Image { media, w = finite(*w), h = finite(*h), alt, float, crop = crop.map(finite), fx, ole },
        // Unlike the way in: a graphic's size is checked like any number, and each object gets its own copy of the graphic.
        Graphic { w, h, alt, float, graphic },
        Shape { kind, w, h, fill, stroke, stroke_width, float, story, freeform, effects, extra },
        // A group's size and coordinate space are copied as they are, as on the way in.
        Group { w = *w, h = *h, float, ch_w = *ch_w, ch_h = *ch_h, children, canvas },
        Field { instr, result, locked, dirty, code, form },
        FieldStart { instr, locked, dirty },
        FieldEnd,
        ControlStart { control },
        ControlEnd,
        NoteRef { kind, id, custom },
        BookmarkStart { name },
        BookmarkEnd { name },
        CommentStart { id },
        CommentEnd { id },
        PermStart { id, group, editor },
        PermEnd { id },
        // As on the way in: an equation with no structure loses the rest of `math`.
        Equation { linear, display, math = if math.nodes.is_empty() { wm::math::Math::default() } else { ConvertRef::<_, Bridge>::convert_ref(math) } },
        Ruby { base, ruby, align, size, raise, base_size, lang, props },
        PositionalTab { align, relative_to, leader },
        Opaque { format, xml, text },
    }

    // A group member's position is moved as it is, like the group's size.
    struct wm::para::GroupChild => doc::para::GroupChild { x = x, y = y, obj }
    struct &doc::para::GroupChild => wm::para::GroupChild { x = *x, y = *y, obj }

    // A table's column widths are moved as they are: the JSON bridge never looked at them.
    struct wm::table::Table => doc::table::Table { props, grid = grid, rows, controls }
    struct &doc::table::Table => wm::table::Table { props, grid = grid.clone(), rows, controls }
    struct wm::table::Row => doc::table::Row { props, cells, controls }
    struct &doc::table::Row => wm::table::Row { props, cells, controls }
    struct wm::table::Cell => doc::table::Cell { props, blocks, controls }
    struct &doc::table::Cell => wm::table::Cell { props, blocks, controls }

    struct wm::Part => doc::Part { kind, blocks, body }
    struct &doc::Part => wm::Part { kind, blocks, body }
    struct wm::NoteSeparators => doc::NoteSeparators { separator, continuation_separator, continuation_notice }
    struct &doc::NoteSeparators => wm::NoteSeparators { separator, continuation_separator, continuation_notice }
    struct wm::BuildingBlock => doc::BuildingBlock { name, gallery, category, description, guid, style, types, behaviors, blocks }
    struct &doc::BuildingBlock => wm::BuildingBlock { name, gallery, category, description, guid, style, types, behaviors, blocks }

    enum wm::fill::Fill = doc::fill::Fill {
        // `-0.0` becomes `0.0`: an opaque fill went through JSON as a bare colour.
        Solid { color, transparency = crate::json::zero(transparency) },
        Gradient(gradient),
    }
}

fn finite(value: f32) -> f32 {
    if value.is_finite() { value } else { 0.0 }
}

/// A paragraph's own formatting for the shared model. Its line spacing, indents and spacing are
/// made finite first (zero stands in), so a paragraph with a bad number can still be saved. Only
/// a paragraph's own formatting gets this, not a style's.
fn finite_para(props: &doc::ParaProps) -> wm::ParaProps {
    use doc::props::LineSpacing;

    let mut props = props.clone();
    if let Some(LineSpacing::Multiple(v) | LineSpacing::Exactly(v) | LineSpacing::AtLeast(v)) = &mut props.line_spacing {
        *v = finite(*v);
    }
    for field in [&mut props.indent_left, &mut props.indent_right, &mut props.indent_first, &mut props.space_before, &mut props.space_after] {
        *field = field.map(finite);
    }
    ConvertRef::<_, Bridge>::convert_ref(&props)
}

/// A run's or a paragraph mark's formatting for the shared model, with its size and character
/// spacing made finite first, as in [`finite_para`].
fn finite_char(props: &doc::CharProps) -> wm::CharProps {
    let mut props = props.clone();
    for field in [&mut props.size, &mut props.spacing] {
        *field = field.map(finite);
    }
    ConvertRef::<_, Bridge>::convert_ref(&props)
}

// `Rgb` is a tuple struct, which the macro does not take; the patterns name all three parts.
impl Convert<doc::Rgb, Bridge> for wm::props::Rgb {
    fn convert(self) -> doc::Rgb {
        let wm::props::Rgb(r, g, b) = self;
        doc::Rgb(r, g, b)
    }
}

impl ConvertRef<doc::Rgb, Bridge> for wm::props::Rgb {
    fn convert_ref(&self) -> doc::Rgb {
        let wm::props::Rgb(r, g, b) = *self;
        doc::Rgb(r, g, b)
    }
}

impl Convert<wm::props::Rgb, Bridge> for doc::Rgb {
    fn convert(self) -> wm::props::Rgb {
        let doc::Rgb(r, g, b) = self;
        wm::props::Rgb(r, g, b)
    }
}

impl ConvertRef<wm::props::Rgb, Bridge> for doc::Rgb {
    fn convert_ref(&self) -> wm::props::Rgb {
        let doc::Rgb(r, g, b) = *self;
        wm::props::Rgb(r, g, b)
    }
}

// `PropChange<T>` is generic, which the macro does not take; the patterns name both fields.
impl<A: Convert<B, Bridge>, B> Convert<doc::props::PropChange<B>, Bridge> for wm::props::PropChange<A> {
    fn convert(self) -> doc::props::PropChange<B> {
        let wm::props::PropChange { rev, old } = self;
        doc::props::PropChange { rev, old: old.convert() }
    }
}

impl<A: ConvertRef<B, Bridge>, B> ConvertRef<doc::props::PropChange<B>, Bridge> for wm::props::PropChange<A> {
    fn convert_ref(&self) -> doc::props::PropChange<B> {
        let wm::props::PropChange { rev, old } = self;
        doc::props::PropChange { rev: *rev, old: old.convert_ref() }
    }
}

impl<A: Convert<B, Bridge>, B> Convert<wm::props::PropChange<B>, Bridge> for doc::props::PropChange<A> {
    fn convert(self) -> wm::props::PropChange<B> {
        let doc::props::PropChange { rev, old } = self;
        wm::props::PropChange { rev, old: old.convert() }
    }
}

impl<A: ConvertRef<B, Bridge>, B> ConvertRef<wm::props::PropChange<B>, Bridge> for doc::props::PropChange<A> {
    fn convert_ref(&self) -> wm::props::PropChange<B> {
        let doc::props::PropChange { rev, old } = self;
        wm::props::PropChange { rev: *rev, old: old.convert_ref() }
    }
}

/// The report of pictures a reader could not load. It has private bookkeeping (what it has
/// already counted), which starts empty on the other side, as it did before.
fn missing_from_shared(source: wm::MissingPictures) -> doc::MissingPictures {
    let mut target = doc::MissingPictures::default();
    target.list = Convert::<_, Bridge>::convert(source.list);
    target.truncated = source.truncated;
    target.total = source.total;
    target.main = source.main;
    target.part_names = source.part_names;
    target.media_parts = source.media_parts;
    target
}

fn missing_to_shared(source: &doc::MissingPictures) -> wm::MissingPictures {
    let mut target = wm::MissingPictures::default();
    target.list = ConvertRef::<_, Bridge>::convert_ref(&source.list);
    target.truncated = source.truncated;
    target.total = source.total;
    target.main = source.main.clone();
    target.part_names = source.part_names.clone();
    target.media_parts = source.media_parts.clone();
    target
}

/// The shared model's document as WordCraft's.
///
/// `Document` has a private field (its media index), so it can be neither taken apart nor built
/// by naming every field; the fields are assigned one by one, and the test
/// `the_bridge_names_every_document_field` fails when either model gains one.
pub(crate) fn document_from_shared(source: wm::Document) -> doc::Document {
    let wm::Document {
        body,
        last_section,
        parts,
        styles,
        numbering,
        comments,
        revisions,
        settings,
        core,
        sources,
        custom_props,
        footnote_separators,
        endnote_separators,
        media,
        passthrough,
        glossary,
        missing_pictures,
        // The rest is not carried: `kept` (what a reader kept to write a part back unchanged belongs to the
        // document it read) and the private media index.
        ..
    } = source;

    let _graphics = session::SharedGraphics::start();
    let mut document = doc::Document::new();
    document.body = Convert::<_, Bridge>::convert(body);
    document.last_section = Convert::<_, Bridge>::convert(last_section);
    document.parts = Convert::<_, Bridge>::convert(parts);
    document.styles = Convert::<_, Bridge>::convert(styles);
    document.numbering = Convert::<_, Bridge>::convert(numbering);
    document.comments = Convert::<_, Bridge>::convert(comments);
    document.revisions = Convert::<_, Bridge>::convert(revisions);
    document.settings = Convert::<_, Bridge>::convert(settings);
    document.core = Convert::<_, Bridge>::convert(core);
    document.sources = Convert::<_, Bridge>::convert(sources);
    document.custom_props = Convert::<_, Bridge>::convert(custom_props);
    document.footnote_separators = Convert::<_, Bridge>::convert(footnote_separators);
    document.endnote_separators = Convert::<_, Bridge>::convert(endnote_separators);
    // Pictures and opaque parts are the same buffers on both sides: the pointers are moved, the bytes never copied.
    document.media = media;
    document.passthrough = passthrough;
    document.glossary = Convert::<_, Bridge>::convert(glossary);
    document.missing_pictures = missing_from_shared(missing_pictures);
    // `fit_state` exists only in WordCraft's model. It travels in a custom XML part, which `wordcraft-docx` reads and writes.
    document
}

/// WordCraft's document as the shared model's. See [`document_from_shared`].
pub(crate) fn document_to_shared(source: &doc::Document) -> wm::Document {
    let mut document = wm::Document::new();
    document.body = ConvertRef::<_, Bridge>::convert_ref(&source.body);
    document.last_section = ConvertRef::<_, Bridge>::convert_ref(&source.last_section);
    document.parts = ConvertRef::<_, Bridge>::convert_ref(&source.parts);
    document.styles = ConvertRef::<_, Bridge>::convert_ref(&source.styles);
    document.numbering = ConvertRef::<_, Bridge>::convert_ref(&source.numbering);
    document.comments = ConvertRef::<_, Bridge>::convert_ref(&source.comments);
    document.revisions = ConvertRef::<_, Bridge>::convert_ref(&source.revisions);
    document.settings = ConvertRef::<_, Bridge>::convert_ref(&source.settings);
    document.core = ConvertRef::<_, Bridge>::convert_ref(&source.core);
    document.sources = ConvertRef::<_, Bridge>::convert_ref(&source.sources);
    document.custom_props = ConvertRef::<_, Bridge>::convert_ref(&source.custom_props);
    document.footnote_separators = ConvertRef::<_, Bridge>::convert_ref(&source.footnote_separators);
    document.endnote_separators = ConvertRef::<_, Bridge>::convert_ref(&source.endnote_separators);
    // Pictures and opaque parts are the same buffers on both sides: the pointers are shared, the bytes never copied.
    document.media = clone_buffers(&source.media);
    document.passthrough = clone_buffers(&source.passthrough);
    document.glossary = ConvertRef::<_, Bridge>::convert_ref(&source.glossary);
    document.missing_pictures = missing_to_shared(&source.missing_pictures);
    // Not carried: `kept` (what a reader kept belongs to the document it read) and `fit_state`
    // (only in WordCraft's model; `wordcraft-docx` writes it as a custom XML part).
    document
}

fn clone_buffers(buffers: &std::collections::BTreeMap<String, Arc<Vec<u8>>>) -> std::collections::BTreeMap<String, Arc<Vec<u8>>> {
    buffers.clone()
}
