//! WordCraft PDF export through the shared `craft-pdf` writer and Krilla drawing primitives.
//!
//! [`export`] lays the document out and draws each page's display list
//! ([`wordcraft_layout::display::page_display`]) into a PDF page:
//!
//! - **Text as real text:** glyph runs are written with embedded, subsetted fonts and a Unicode
//!   mapping taken from the paragraph text, so text is selectable, searchable and extractable.
//!   Synthetic bold (fill + stroke) and italic (skew) follow the renderer.
//! - **Graphics:** fills (highlight, shading, cell fills), rules and underlines (solid, dotted,
//!   dashed, double, wave), shapes, pictures (decoded with `image`; JPEG passed through, the rest
//!   embedded losslessly; WMF and EMF drawn as vector paths and embedded bitmaps) with cropping and
//!   opacity, the page colour and the watermark.
//! - **Interactive:** link annotations for hyperlinked runs (URLs and internal `#bookmark`
//!   links) and a document outline built from the headings (outline levels).
//! - **Metadata:** title, author, subject, keywords, language, creation date.
//! - **Tagged PDF (best effort):** one structure element per paragraph (`H1`–`H6` for headings,
//!   `P` otherwise), `Figure` for pictures with their alt text; headers, footers, decorations and
//!   the watermark are artifacts.
//!
//! Nothing here panics on hostile documents: sizes are clamped, unknown fonts or undecodable
//! pictures are skipped (a grey box stands in for a picture).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
#![forbid(unsafe_code)]

pub mod imposition;

#[cfg(feature = "writer")]
use std::collections::{HashMap, HashSet};
#[cfg(feature = "writer")]
use std::num::NonZeroU16;
#[cfg(feature = "writer")]
use std::sync::Arc;

#[cfg(feature = "writer")]
use craft_pdf::write::{self, Metadata, WriteError, Writer, backend as krilla};
#[cfg(feature = "writer")]
use krilla::annotation::{Annotation, LinkAnnotation, Target};
#[cfg(feature = "writer")]
use krilla::color::rgb;
#[cfg(feature = "writer")]
use krilla::destination::XyzDestination;
#[cfg(feature = "writer")]
use krilla::error::KrillaError;
#[cfg(feature = "writer")]
use krilla::geom::{Path, PathBuilder, Point, Size, Transform};
#[cfg(feature = "writer")]
use krilla::image::Image;
#[cfg(feature = "writer")]
use krilla::metadata::DateTime;
#[cfg(feature = "writer")]
use krilla::num::NormalizedF32;
#[cfg(feature = "writer")]
use krilla::outline::{Outline, OutlineNode};
#[cfg(feature = "writer")]
use krilla::page::PageSettings;
#[cfg(feature = "writer")]
use krilla::paint::{Fill, FillRule, LineCap, LineJoin, Stroke, StrokeDash};
#[cfg(feature = "writer")]
use krilla::surface::Surface;
#[cfg(feature = "writer")]
use krilla::tagging::{Artifact, ArtifactType, ContentTag, Identifier, Node, SpanTag, Tag, TagGroup, TagKind, TagTree};
#[cfg(feature = "writer")]
use krilla::text::{Font, GlyphId, KrillaGlyph};
#[cfg(feature = "writer")]
use wordcraft_doc::graphic::GraphicKind;
#[cfg(feature = "writer")]
use wordcraft_doc::para::{InlineObject, ShapeKind};
#[cfg(feature = "writer")]
use wordcraft_doc::{Document, Path as DocPath, Rgb, StoryRef};
#[cfg(feature = "writer")]
use wordcraft_fonts::FaceRef;
#[cfg(feature = "writer")]
use wordcraft_geom::Rect;
#[cfg(feature = "writer")]
use wordcraft_layout::display::Paint;
#[cfg(feature = "writer")]
use wordcraft_layout::display::{Draw, ItemDisplay, Stroke as LineStyle};
#[cfg(feature = "writer")]
use wordcraft_layout::para::{ClKind, LineEnd};
use wordcraft_layout::{DocLayout, NumeralMode};
#[cfg(feature = "writer")]
use wordcraft_layout::{LayoutCache, Page, Placed, layout};
#[cfg(feature = "writer")]
use wordcraft_metafile::{Picture, PlacedItem};

#[cfg(feature = "writer")]
mod export;
#[cfg(feature = "writer")]
mod exporter;
#[cfg(feature = "writer")]
mod glyphs;
#[cfg(feature = "writer")]
mod metadata;
mod options;
#[cfg(feature = "writer")]
mod paint;
#[cfg(feature = "writer")]
mod picture;
#[cfg(feature = "writer")]
mod tags;

#[cfg(test)]
#[cfg(feature = "writer")]
use export::Fit;
#[cfg(feature = "writer")]
pub use export::{export, export_layout};
#[cfg(not(feature = "writer"))]
pub fn export(_doc: &wordcraft_doc::Document, _opts: &PdfOptions) -> Result<Vec<u8>, PdfError> {
    Err(PdfError::Write("PDF export is not included in this build".into()))
}
#[cfg(not(feature = "writer"))]
pub fn export_layout(_doc: &wordcraft_doc::Document, _lay: &DocLayout, _opts: &PdfOptions) -> Result<Vec<u8>, PdfError> {
    Err(PdfError::Write("PDF export is not included in this build".into()))
}
#[cfg(test)]
#[cfg(feature = "writer")]
use exporter::Exporter;
#[cfg(test)]
#[cfg(feature = "writer")]
use glyphs::{baseline, cluster_ends, left_to_right, split_clusters, visual_order};
#[cfg(test)]
#[cfg(feature = "writer")]
use metadata::{civil, parse_iso};
pub use options::{PdfError, PdfOptions, WATERMARK_MAX_CHARS};
#[cfg(test)]
#[cfg(feature = "writer")]
use paint::shape_path;
#[cfg(test)]
#[cfg(feature = "writer")]
use picture::{load_image, load_image_within};

#[cfg(test)]
#[cfg(feature = "writer")]
mod tests;

#[cfg(test)]
#[cfg(feature = "writer")]
mod fill_alpha_tests;
