//! The typed mapping against the JSON bridge it replaced, on a corpus that sets every member.

mod depth;
mod json_bridge;
mod sample;
mod samples;

use std::sync::Arc;

use craft_wordmodel as wm;
use sample::{Sample, Seed};
use serde_json::Value;
use wordcraft_doc as doc;

use crate::{from_shared, to_shared};

/// Sampled documents in the corpus; enough for the largest enum to show every variant.
const SAMPLED: u64 = 160;

/// The documents every conversion is checked on: the sampled ones, then small ones built by hand
/// for the cases sampling does not make (defaults, shared pointers, empty containers).
fn corpus(seed: &mut Seed) -> Vec<(String, wm::Document)> {
    let mut documents: Vec<(String, wm::Document)> = (0..SAMPLED).map(|i| (format!("sampled {i}"), wm::Document::sample(seed))).collect();
    documents.push(("new".into(), wm::Document::new()));
    documents.push(("default".into(), wm::Document::default()));
    documents.push(("text".into(), wm::Document::from_text("Alpha\nBeta\n\nGamma")));
    documents.push(("nested".into(), nested()));
    documents.push(("shared graphic".into(), shared_graphic().0));
    documents
}

/// WordCraft documents for the write direction, sampled in their own right: what the read
/// direction returns has already been through its rules.
fn own_corpus(seed: &mut Seed) -> Vec<(String, doc::Document)> {
    let mut documents: Vec<(String, doc::Document)> = (0..SAMPLED).map(|i| (format!("own sampled {i}"), doc::Document::sample(seed))).collect();
    documents.push(("own new".into(), doc::Document::new()));
    documents.push(("own default".into(), doc::Document::default()));
    documents.push(("own text".into(), doc::Document::from_text("Alpha\nBeta\n\nGamma")));
    documents
}

/// A group inside a paragraph, a table, a text box story and a picture: the stories the bridge
/// walks by hand.
fn nested() -> wm::Document {
    let mut document = wm::Document::new();
    let mut paragraph = wm::para::Paragraph::with_text("body", Default::default());
    paragraph.objects.push(wm::para::InlineObject::Group {
        w: 32.0,
        h: 24.0,
        float: Default::default(),
        ch_w: 64.0,
        ch_h: 48.0,
        children: vec![wm::para::GroupChild {
            x: 4.0,
            y: 6.0,
            obj: wm::para::InlineObject::Field {
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
    let table = wm::table::Table {
        rows: vec![wm::table::Row { cells: vec![wm::table::Cell::with_text("nested cell")], ..Default::default() }],
        ..Default::default()
    };
    document.body = vec![Arc::new(wm::Block::Para(paragraph)), Arc::new(wm::Block::Table(table))];
    document.parts.insert(
        12,
        wm::Part {
            kind: wm::PartKind::TextBox,
            blocks: vec![wm::para_block(wm::para::Paragraph::with_text("part story", Default::default()))],
            ..Default::default()
        },
    );
    document.settings.compat_mode = 11;
    document.media.insert("image1.png".into(), Arc::new(vec![0x5a; 64]));
    document
}

/// One graphic drawn in two paragraphs and once more in a header, through one pointer.
fn shared_graphic() -> (wm::Document, Arc<wm::graphic::Graphic>) {
    let graphic = Arc::new(wm::graphic::Graphic::sample(&mut Seed::new(7)));
    let object =
        || wm::para::InlineObject::Graphic { w: 10.0, h: 20.0, alt: Default::default(), float: Default::default(), graphic: graphic.clone() };
    let paragraph = || {
        let mut paragraph = wm::para::Paragraph::with_text("\u{fffc}", Default::default());
        paragraph.objects.push(object());
        wm::para_block(paragraph)
    };
    let mut document = wm::Document::new();
    document.body = vec![paragraph(), paragraph()];
    document.parts.insert(3, wm::Part { blocks: vec![paragraph()], ..Default::default() });
    (document, graphic)
}

fn json<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("the model serializes")
}

/// The parts of the report that `Document`'s JSON and `==` leave out.
fn missing(m: &doc::MissingPictures) -> String {
    format!("{:?}", (&m.list, m.truncated, m.total, &m.main, &m.part_names, &m.media_parts))
}

fn missing_shared(m: &wm::MissingPictures) -> String {
    format!("{:?}", (&m.list, m.truncated, m.total, &m.main, &m.part_names, &m.media_parts))
}

/// Where two JSON values first differ, for a failure a person can act on.
fn first_difference(path: &str, new: &Value, old: &Value) -> Option<String> {
    match (new, old) {
        (Value::Object(a), Value::Object(b)) => {
            // Each key once: the keys of `a`, then those only `b` has.
            for key in a.keys().chain(b.keys().filter(|key| !a.contains_key(*key))) {
                match (a.get(key), b.get(key)) {
                    (Some(x), Some(y)) => {
                        if let Some(found) = first_difference(&format!("{path}.{key}"), x, y) {
                            return Some(found);
                        }
                    }
                    (x, y) => return Some(format!("{path}.{key}: typed {}, JSON bridge {}", short(&format!("{x:?}")), short(&format!("{y:?}")))),
                }
            }
            None
        }
        (Value::Array(a), Value::Array(b)) if a.len() == b.len() => {
            a.iter().zip(b).enumerate().find_map(|(i, (x, y))| first_difference(&format!("{path}[{i}]"), x, y))
        }
        (a, b) if a == b => None,
        (a, b) => Some(format!("{path}: typed {}, JSON bridge {}", short(&a.to_string()), short(&b.to_string()))),
    }
}

/// Where the `Debug` forms of two values first differ, with the lines that lead there.
fn debug_difference<T: std::fmt::Debug>(new: &T, old: &T) -> String {
    let (new, old) = (format!("{new:#?}"), format!("{old:#?}"));
    let (new, old): (Vec<&str>, Vec<&str>) = (new.lines().collect(), old.lines().collect());
    let at = new.iter().zip(&old).position(|(a, b)| a != b).unwrap_or(new.len().min(old.len()));
    let mut context: Vec<&str> = Vec::new();
    let mut indent = usize::MAX;
    for line in new[..at].iter().rev() {
        let this = line.len() - line.trim_start().len();
        if this < indent {
            indent = this;
            context.push(line.trim());
        }
    }
    context.reverse();
    format!("at {}: typed `{}`, JSON bridge `{}`", context.join(" > "), new.get(at).map_or("", |l| l.trim()), old.get(at).map_or("", |l| l.trim()))
}

fn short(text: &str) -> String {
    if text.chars().count() > 160 { format!("{}...", text.chars().take(160).collect::<String>()) } else { text.to_owned() }
}

/// AC1: for every document of the corpus, in both directions, the typed conversion gives what
/// the JSON bridge gave.
#[test]
fn the_typed_mapping_equals_the_json_bridge_in_both_directions() {
    let mut seed = Seed::new(0x5eed);
    let corpus = corpus(&mut seed);
    for (name, shared) in &corpus {
        // Read direction: shared model to WordCraft's.
        let old = json_bridge::from_shared_model(shared.clone()).unwrap_or_else(|e| panic!("{name}: JSON bridge read: {e}"));
        let new = from_shared(shared.clone()).unwrap_or_else(|e| panic!("{name}: typed read: {e}"));
        let (new_json, old_json) = (json(&new), json(&old));
        if let Some(found) = first_difference("document", &new_json, &old_json) {
            panic!("{name}: read differs at {found}");
        }
        assert!(new == old, "{name}: read: equal JSON but `==` says different: {}", debug_difference(&new, &old));
        assert_eq!(missing(&new.missing_pictures), missing(&old.missing_pictures), "{name}: read: missing pictures");

        // Write direction: back to the shared model, from the document just read.
        let old_back = json_bridge::to_shared_model(&old).unwrap_or_else(|e| panic!("{name}: JSON bridge write: {e}"));
        let new_back = to_shared(&old).unwrap_or_else(|e| panic!("{name}: typed write: {e}"));
        if let Some(found) = first_difference("document", &json(&new_back), &json(&old_back)) {
            panic!("{name}: write differs at {found}");
        }
        assert!(new_back == old_back, "{name}: write: equal JSON but `==` says different: {}", debug_difference(&new_back, &old_back));
        assert_eq!(missing_shared(&new_back.missing_pictures), missing_shared(&old_back.missing_pictures), "{name}: write: missing pictures");
    }

    // Write direction again, on WordCraft documents sampled in their own right.
    let mut own_seed = Seed::new(0x0dd5);
    let own = own_corpus(&mut own_seed);
    for (name, document) in &own {
        let old = json_bridge::to_shared_model(document).unwrap_or_else(|e| panic!("{name}: JSON bridge write: {e}"));
        let new = to_shared(document).unwrap_or_else(|e| panic!("{name}: typed write: {e}"));
        if let Some(found) = first_difference("document", &json(&new), &json(&old)) {
            panic!("{name}: write differs at {found}");
        }
        assert!(new == old, "{name}: write: equal JSON but `==` says different: {}", debug_difference(&new, &old));
        assert_eq!(missing_shared(&new.missing_pictures), missing_shared(&old.missing_pictures), "{name}: write: missing pictures");
    }

    // What the corpus exercised: every struct and every enum variant the samples know.
    let made =
        |all: &[(&str, usize)]| all.iter().filter(|(name, _)| seed.made.contains_key(name)).fold((0, 0), |(n, m), (_, members)| (n + 1, m + members));
    let (structs, struct_fields) = made(samples::shared::STRUCTS);
    let (variants, variant_fields) = made(samples::shared::VARIANTS);
    let own_never: Vec<&str> =
        samples::own::STRUCTS.iter().chain(samples::own::VARIANTS).map(|(name, _)| *name).filter(|name| !own_seed.made.contains_key(name)).collect();
    assert!(own_never.is_empty(), "sampled for the write direction but never made: {own_never:?}");
    println!(
        "corpus: {} shared-model documents ({SAMPLED} sampled, {} by hand) read and written back, and {} WordCraft documents written; every one of {structs} structs ({struct_fields} fields) and {variants} enum variants ({variant_fields} fields) of each model was made",
        corpus.len(),
        corpus.len() as u64 - SAMPLED,
        own.len(),
    );
    let never: Vec<&str> = samples::shared::STRUCTS
        .iter()
        .chain(samples::shared::VARIANTS)
        .map(|(name, _)| *name)
        .filter(|name| !seed.made.contains_key(name))
        .collect();
    assert!(never.is_empty(), "sampled but never made: {never:?}");
}

/// A paragraph holding `object`.
fn with_object(object: wm::para::InlineObject) -> wm::Document {
    let mut paragraph = wm::para::Paragraph::with_text("\u{fffc}", Default::default());
    paragraph.objects.push(object);
    let mut document = wm::Document::new();
    document.body = vec![wm::para_block(paragraph)];
    document
}

fn picture(w: f32) -> wm::para::InlineObject {
    wm::para::InlineObject::Image {
        media: "image1.png".into(),
        w,
        h: 10.0,
        alt: Default::default(),
        float: Default::default(),
        crop: [0.0, f32::NAN, 0.0, 0.0],
        fx: Default::default(),
        ole: None,
    }
}

/// Both bridges on a shared-model document: the same error, or the same document.
fn read_both(name: &str, shared: &wm::Document) -> Option<doc::Document> {
    let old = json_bridge::from_shared_model(shared.clone());
    let new = from_shared(shared.clone());
    assert_eq!(new.is_ok(), old.is_ok(), "{name}: typed {:?}, JSON bridge {:?}", new.as_ref().err(), old.as_ref().err());
    let (new, old) = (new.ok()?, old.ok()?);
    assert_eq!(first_difference("document", &json(&new), &json(&old)), None, "{name}");
    Some(new)
}

/// Both bridges on a WordCraft document: the same error, or the same document.
fn write_both(name: &str, document: &doc::Document) -> Option<wm::Document> {
    let old = json_bridge::to_shared_model(document);
    let new = to_shared(document);
    assert_eq!(new.is_ok(), old.is_ok(), "{name}: typed {:?}, JSON bridge {:?}", new.as_ref().err(), old.as_ref().err());
    let (new, old) = (new.ok()?, old.ok()?);
    assert_eq!(first_difference("document", &json(&new), &json(&old)), None, "{name}");
    Some(new)
}

/// JSON cannot write NaN or an infinity, so the JSON bridge refused a document that had one,
/// dropped it where the number was optional, and let through the few numbers it moved by hand.
/// The typed mapping does the same in each place.
#[test]
fn numbers_that_are_not_finite_convert_as_they_did() {
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        // A number that is not optional: refused.
        let mut shared = wm::Document::new();
        shared.last_section.page_w = bad;
        assert!(read_both("page width", &shared).is_none(), "{bad}: a page width is refused");
        assert_eq!(from_shared(shared).err(), Some(crate::Error::NotFinite));

        // An optional one: dropped.
        let mut shared = wm::Document::from_text("a");
        let mut styles = wm::StyleSheet::clone(&shared.styles);
        styles.default_chr.size = Some(bad);
        shared.styles = Arc::new(styles);
        let read = read_both("default size", &shared).expect("an optional number is dropped, not refused");
        assert_eq!(read.styles.default_chr.size, None);

        // A picture's size and crop, a group's geometry and a table's grid: moved as they are.
        let read = read_both("picture", &with_object(picture(bad))).expect("a picture's size is not looked at");
        assert!(
            matches!(&read.body[0].as_para().expect("paragraph").objects[0], doc::para::InlineObject::Image { w, crop, .. } if !w.is_finite() && crop[1].is_nan())
        );
        let group = wm::para::InlineObject::Group {
            w: bad,
            h: bad,
            float: Default::default(),
            ch_w: bad,
            ch_h: bad,
            children: vec![wm::para::GroupChild { x: bad, y: bad, obj: picture(1.0) }],
            canvas: None,
        };
        let read = read_both("group", &with_object(group)).expect("a group's geometry is not looked at");
        let mut shared = wm::Document::new();
        shared.body = vec![Arc::new(wm::Block::Table(wm::table::Table { grid: vec![bad, 1.0], ..Default::default() }))];
        let table = read_both("table grid", &shared).expect("a table's grid is not looked at");

        // The way out: a paragraph's own spacing and a run's size become zero, a picture's size too;
        // a group's geometry and a table's grid are copied as they are.
        let mut own = doc::Document::from_text("a");
        {
            let doc::Block::Para(paragraph) = Arc::make_mut(&mut own.body[0]) else { panic!("paragraph") };
            paragraph.props.indent_left = Some(bad);
            paragraph.props.space_after = Some(bad);
            paragraph.props.line_spacing = Some(doc::props::LineSpacing::Exactly(bad));
            paragraph.mark.size = Some(bad);
            paragraph.mark.spacing = Some(bad);
            // An optional number with no such rule is dropped.
            paragraph.mark.kern = Some(bad);
            paragraph.props.indent_left_chars = Some(bad);
        }
        let written = write_both("paragraph formatting", &own).expect("a paragraph's spacing is made finite");
        let paragraph = written.body[0].as_para().expect("paragraph");
        assert_eq!((paragraph.props.indent_left, paragraph.props.space_after), (Some(0.0), Some(0.0)));
        assert_eq!(paragraph.props.line_spacing, Some(wm::props::LineSpacing::Exactly(0.0)));
        assert_eq!((paragraph.mark.size, paragraph.mark.spacing, paragraph.mark.kern), (Some(0.0), Some(0.0), None));
        assert_eq!(paragraph.props.indent_left_chars, None);

        let written =
            write_both("picture out", &read_both("picture", &with_object(picture(bad))).expect("read")).expect("a picture's size is made finite");
        assert!(
            matches!(&written.body[0].as_para().expect("paragraph").objects[0], wm::para::InlineObject::Image { w, crop, .. } if *w == 0.0 && crop[1] == 0.0)
        );
        assert!(write_both("group out", &read).is_some());
        assert!(write_both("table grid out", &table).is_some());

        // A style's formatting has no such rule: an optional number is dropped, any other refused.
        let mut own = doc::Document::from_text("a");
        let mut styles = doc::StyleSheet::clone(&own.styles);
        styles.default_para.indent_left = Some(bad);
        styles.default_para.line_spacing = Some(doc::props::LineSpacing::Multiple(1.0));
        own.styles = Arc::new(styles);
        let written = write_both("style indent", &own).expect("dropped");
        assert_eq!(written.styles.default_para.indent_left, None);
        let mut styles = doc::StyleSheet::clone(&own.styles);
        styles.default_para.line_spacing = Some(doc::props::LineSpacing::Multiple(bad));
        own.styles = Arc::new(styles);
        assert!(write_both("style line spacing", &own).is_none(), "{bad}: a style's line spacing is refused");

        // A graphic's size is checked on the way out, though not on the way in.
        let (mut shared, _) = shared_graphic();
        if let wm::Block::Para(paragraph) = Arc::make_mut(&mut shared.body[0])
            && let wm::para::InlineObject::Graphic { w, .. } = &mut paragraph.objects[0]
        {
            *w = bad;
        }
        let read = read_both("graphic size", &shared).expect("not looked at on the way in");
        assert!(write_both("graphic size out", &read).is_none(), "{bad}: a graphic's size is refused on the way out");
    }

    // One conversion's refusal does not stay behind for the next.
    assert!(from_shared(wm::Document::from_text("a")).is_ok());
}

/// A chart's missing values: `None`, and a value that is not finite becomes one.
#[test]
fn chart_values_that_are_not_finite_become_missing() {
    let mut spec: doc::chart::ChartSpec = Sample::sample(&mut Seed::new(3));
    spec.series = vec![doc::chart::ChartSeries { name: "s".into(), values: vec![Some(1.5), None, Some(f64::NAN), Some(f64::INFINITY)] }];
    let old: wm::chart::ChartSpec = json_bridge::convert_ref(&spec).expect("JSON bridge");
    let new = crate::chart_to_shared(&spec).expect("typed");
    assert_eq!(json(&new), json(&old));
    assert_eq!(new.series[0].values, vec![Some(1.5), None, None, None]);
}

/// One graphic in three places is converted once, and pictures keep their buffers both ways.
#[test]
fn shared_graphics_stay_shared_and_buffers_are_not_copied() {
    let (mut shared, graphic) = shared_graphic();
    let image = Arc::new(vec![0x5a; 1 << 16]);
    let opaque = Arc::new(vec![1, 2, 3]);
    shared.media.insert("image1.png".into(), image.clone());
    shared.passthrough.insert("word/x.bin".into(), opaque.clone());
    let expected: doc::graphic::Graphic = json_bridge::convert_ref(&*graphic).expect("JSON bridge");
    drop(graphic);

    let read = from_shared(shared).expect("read");
    let graphic_of = |block: &doc::Block| match &block.as_para().expect("paragraph").objects[0] {
        doc::para::InlineObject::Graphic { graphic, .. } => graphic.clone(),
        other => panic!("expected a graphic, got {other:?}"),
    };
    let (first, second, header) = (graphic_of(&read.body[0]), graphic_of(&read.body[1]), graphic_of(&read.parts[&3].blocks[0]));
    assert!(Arc::ptr_eq(&first, &second) && Arc::ptr_eq(&first, &header), "one graphic, converted once");
    assert_eq!(*first, expected);
    assert!(Arc::ptr_eq(&read.media["image1.png"], &image), "media moved, not copied");
    assert!(Arc::ptr_eq(&read.passthrough["word/x.bin"], &opaque), "opaque parts moved, not copied");

    let written = to_shared(&read).expect("write");
    assert!(Arc::ptr_eq(&written.media["image1.png"], &image));
    assert!(Arc::ptr_eq(&written.passthrough["word/x.bin"], &opaque));

    // Two documents never share a converted graphic, however their source addresses fall.
    let (again, _) = shared_graphic();
    let again = from_shared(again).expect("read");
    assert!(!Arc::ptr_eq(&graphic_of(&again.body[0]), &first));
}

/// The helpers `wordcraft-docx` draws charts and SmartArt and reads bibliographies through.
#[test]
fn the_helpers_equal_the_json_bridge() {
    let mut seed = Seed::new(11);
    for _ in 0..40 {
        let chart: doc::chart::ChartSpec = Sample::sample(&mut seed);
        assert_eq!(
            json(&crate::chart_to_shared(&chart).expect("typed")),
            json(&json_bridge::convert_ref::<_, wm::chart::ChartSpec>(&chart).expect("JSON bridge"))
        );
        let art: doc::smart_art::SmartArtSpec = Sample::sample(&mut seed);
        assert_eq!(
            json(&crate::smart_art_to_shared(&art).expect("typed")),
            json(&json_bridge::convert_ref::<_, wm::smart_art::SmartArtSpec>(&art).expect("JSON bridge"))
        );
        let theme: Vec<doc::Rgb> = Sample::sample(&mut seed);
        assert_eq!(
            json(&crate::theme_to_shared(&theme)),
            json(&json_bridge::convert_ref::<_, Vec<wm::props::Rgb>>(&theme[..]).expect("JSON bridge"))
        );
        let items: Vec<wm::graphic::GraphicItem> = Sample::sample(&mut seed);
        let old: Vec<doc::graphic::GraphicItem> = json_bridge::convert_ref(&items).expect("JSON bridge");
        assert_eq!(json(&crate::graphic_items_from_shared(items).expect("typed")), json(&old));
        let sources: Vec<wm::Source> = Sample::sample(&mut seed);
        let old: Vec<doc::Source> = json_bridge::convert_ref(&sources).expect("JSON bridge");
        assert_eq!(json(&crate::sources_from_shared(sources).expect("typed")), json(&old));
    }
}

/// `Document` has a private field, so the compiler cannot check that the bridge names every
/// field of it. This does: a field added to either model fails here until `stories.rs` carries it
/// or says why not.
#[test]
fn the_bridge_names_every_document_field() {
    fn fields<T: std::fmt::Debug>(value: &T) -> Vec<String> {
        let mut names: Vec<String> = format!("{value:#?}")
            .lines()
            .filter(|line| line.starts_with("    ") && !line.starts_with("     "))
            .filter_map(|line| line.trim().split_once(':').map(|(name, _)| name.to_owned()))
            .collect();
        names.sort();
        names
    }
    fn sorted(mut names: Vec<&str>) -> Vec<&str> {
        names.sort();
        names
    }
    let carried = [
        "body",
        "last_section",
        "parts",
        "styles",
        "numbering",
        "comments",
        "revisions",
        "settings",
        "core",
        "sources",
        "custom_props",
        "footnote_separators",
        "endnote_separators",
        "media",
        "passthrough",
        "glossary",
        "missing_pictures",
    ];
    // Rebuilt or left empty on the other side; see `document_from_shared` and the test below.
    let not_carried = ["media_index", "kept"];
    let known: Vec<&str> = carried.iter().chain(&not_carried).copied().collect();
    assert_eq!(fields(&wm::Document::new()), sorted(known.clone()), "the shared model's document");
    // Only WordCraft's model has `fit_state`; `wordcraft-docx` carries it in a custom XML part.
    let own: Vec<&str> = known.iter().copied().chain(["fit_state"]).collect();
    assert_eq!(fields(&doc::Document::new()), sorted(own), "WordCraft's document");

    let report = ["list", "truncated", "total", "main", "part_names", "media_parts", "seen", "unlisted", "counted_chunks"];
    assert_eq!(fields(&wm::MissingPictures::default()), sorted(report.to_vec()), "the shared model's report of missing pictures");
    assert_eq!(fields(&doc::MissingPictures::default()), sorted(report.to_vec()), "WordCraft's report of missing pictures");
}

/// The two document fields the bridge does not carry, in each direction. `media_index` starts
/// empty on the other side and is rebuilt from `media` the first time a picture is added, so
/// adding a picture the document already has finds it rather than storing it twice. `kept` (the
/// source markup a reader kept, for a writer to write back what was not edited) starts empty:
/// after a conversion nothing a reader kept is written back. No reader or writer on either side
/// fills or reads it today, so this loses nothing yet.
#[test]
fn the_media_index_is_rebuilt_and_kept_markup_is_left_behind() {
    let bytes = vec![0x5a; 64];

    let mut shared = wm::Document::new();
    shared.media.insert("image1.png".into(), Arc::new(bytes.clone()));
    shared.kept = wm::kept::Kept::new(wm::kept::KeptXml { format: "docx".into(), ..Default::default() });
    let mut read = from_shared(shared).unwrap();
    assert!(read.kept.get().is_none(), "kept markup is left behind on the way in");
    assert_eq!(read.add_media(bytes.clone(), "png"), "image1.png", "the media index finds a picture read with the document");
    assert_eq!(read.media.len(), 1);
    assert_ne!(read.add_media(vec![0xa5; 64], "png"), "image1.png", "a new picture gets a key of its own");

    let mut own = doc::Document::new();
    own.media.insert("image1.png".into(), Arc::new(bytes.clone()));
    own.kept = doc::kept::Kept::new(doc::kept::KeptXml { format: "docx".into(), ..Default::default() });
    let mut written = to_shared(&own).unwrap();
    assert!(written.kept.get().is_none(), "kept markup is left behind on the way out");
    assert_eq!(written.add_media(bytes, "png"), "image1.png", "the media index finds a picture written with the document");
    assert_eq!(written.media.len(), 1);
    assert_ne!(written.add_media(vec![0xa5; 64], "png"), "image1.png", "a new picture gets a key of its own");
}
