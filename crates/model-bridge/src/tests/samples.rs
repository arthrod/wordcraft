//! `Sample` for every type of both models that a document can hold: one line per type, every
//! member named.
//!
//! First written from the model sources by a script, then kept by hand. The macros build each
//! value with a literal that names every field, so a new field stops the test build until it is
//! listed here. The two models have the same types under the same paths, so one list serves both.

/// The samples of the model crate `$m`.
macro_rules! model_samples {
    ($m:ident) => {
        use crate::tests::sample::{Sample, Seed, sample_enums, sample_structs};

        impl Sample for $m::Document {
            fn sample(seed: &mut Seed) -> Self {
                let mut document = $m::Document::new();
                seed.refill();
                document.body = Sample::sample(seed);
                seed.refill();
                document.last_section = Sample::sample(seed);
                seed.refill();
                document.parts = Sample::sample(seed);
                seed.refill();
                document.styles = Sample::sample(seed);
                seed.refill();
                document.numbering = Sample::sample(seed);
                seed.refill();
                document.comments = Sample::sample(seed);
                seed.refill();
                document.revisions = Sample::sample(seed);
                seed.refill();
                document.settings = Sample::sample(seed);
                seed.refill();
                document.core = Sample::sample(seed);
                seed.refill();
                document.sources = Sample::sample(seed);
                seed.refill();
                document.custom_props = Sample::sample(seed);
                seed.refill();
                document.footnote_separators = Sample::sample(seed);
                seed.refill();
                document.endnote_separators = Sample::sample(seed);
                seed.refill();
                document.media = Sample::sample(seed);
                seed.refill();
                document.passthrough = Sample::sample(seed);
                seed.refill();
                document.glossary = Sample::sample(seed);
                seed.refill();
                document.missing_pictures = Sample::sample(seed);
                document
            }
        }

        impl Sample for $m::MissingPictures {
            fn sample(seed: &mut Seed) -> Self {
                let mut missing = $m::MissingPictures::default();
                missing.list = Sample::sample(seed);
                missing.truncated = Sample::sample(seed);
                missing.total = Sample::sample(seed);
                missing.main = Sample::sample(seed);
                missing.part_names = Sample::sample(seed);
                missing.media_parts = Sample::sample(seed);
                missing
            }
        }

        impl Sample for $m::Path {
            fn sample(seed: &mut Seed) -> Self {
                $m::Path(Sample::sample(seed))
            }
        }

        impl Sample for $m::props::Rgb {
            fn sample(seed: &mut Seed) -> Self {
                $m::props::Rgb(Sample::sample(seed), Sample::sample(seed), Sample::sample(seed))
            }
        }

        impl Sample for $m::fill::Fill {
            fn sample(seed: &mut Seed) -> Self {
                match seed.pick("fill", 3) {
                    // Opaque: the form JSON writes as a bare colour.
                    0 => $m::fill::Fill::Solid { color: Sample::sample(seed), transparency: 0.0 },
                    1 => $m::fill::Fill::Solid { color: Sample::sample(seed), transparency: Sample::sample(seed) },
                    _ => $m::fill::Fill::Gradient(Sample::sample(seed)),
                }
            }
        }

        impl<T: Sample> Sample for $m::props::PropChange<T> {
            fn sample(seed: &mut Seed) -> Self {
                $m::props::PropChange { rev: Sample::sample(seed), old: Sample::sample(seed) }
            }
        }

        sample_structs! {
            $m::chart::ChartSeries { name, values }
            $m::chart::ChartSpec { kind, categories, series, title, legend, data_labels }
            $m::connector::ArrowStyle { kind, width, length }
            $m::connector::ConnEnd { id, site }
            $m::connector::HorizontalRule { pct, align, standard }
            $m::connector::ShapeExtra { id, warp, start, end, arrow_start, arrow_end, start_style, end_style, corner, dash, cap, outline_fill, odf_geometry, hr }
            $m::control::ContentControl { id, tag, title, lock, placeholder, showing_placeholder, temporary, kind, placeholder_text, block, rpr_xml, run_props, extra, end_pr_xml, form_size, form_raise, form_xml }
            $m::control::ControlWrap { open, close }
            $m::control::ListItem { display, value }
            $m::effects::Glow { color, size, transparency }
            $m::effects::Shadow { color, transparency, blur, distance, angle, rot_with_shape }
            $m::effects::ShapeEffects { shadow, glow, soft_edge }
            $m::fill::Gradient { stops, kind, tile, rot_with_shape, flip }
            $m::freeform::FreePath { pts, closed }
            $m::freeform::Freeform { w, h, paths, alpha, ink, order }
            $m::graphic::Embedded { xml, refs }
            $m::graphic::EmbeddedRef { id, kind, target, external }
            $m::graphic::Graphic { kind, items, w, h, source, smart_art, chart }
            $m::math::MRun { text, sty, scr, nor, lit, size, color, font, brk }
            $m::math::Math { nodes, omml, jc, linear }
            $m::math::MathProps { brk_bin, brk_bin_sub, wrap_indent, wrap_right }
            $m::MissingPicture { part, story, reason }
            $m::numbering::AbstractNum { id, name, levels, nsid, tmpl, multi_level_type }
            $m::numbering::Level { start, format, text, align, indent, hanging, suffix, chr, restart, restart_after, tab, legal, style }
            $m::numbering::Num { id, abstract_id, start_overrides, level_overrides }
            $m::numbering::Numbering { abstracts, nums }
            $m::para::Paragraph { text, runs, props, mark, objects, section, rev }
            $m::para::AltText { text, title, decorative }
            $m::para::Float { wrap, wrap_text, h_rel, v_rel, x, y, h_align, v_align, dist, dist_top, dist_bottom, effect, rot, flip_h, flip_v, wrap_polygon, alt, hidden, outside_cell, z, page, bare_turn }
            $m::para::CanvasStyle { fill, stroke, stroke_width }
            $m::para::GroupChild { x, y, obj }
            $m::para::Run { len, props }
            $m::NoteSeparators { separator, continuation_separator, continuation_notice }
            $m::Part { kind, blocks, body }
            $m::TextBody { vert, anchor, next, inset, auto_fit, auto_color }
            $m::picfx::PicFx { bright, contrast, transparency }
            $m::props::CharProps { style, font, sym_font, size, bold, italic, underline, underline_color, strike, double_strike, color, highlight, shading, border, vert_align, caps, small_caps, hidden, web_hidden, snap_to_grid, spacing, scale, position, kern, emphasis, ligatures, outline, shadow, emboss, engrave, text_effects, lang, no_proof, rtl, cs, font_cs, font_ea, size_cs, bold_cs, italic_cs, lang_bidi, lang_ea, east_asian_layout, link, ins, del, fmt_change }
            $m::props::EastAsianLayout { id, combine, combine_brackets, vert, vert_compress }
            $m::props::Ligatures { standard, contextual, historical, discretional }
            $m::props::NumChange { rev, original }
            $m::props::Border { style, width, color, space }
            $m::props::Borders { top, left, bottom, right, between, inside_v }
            $m::props::NumRef { num, level }
            $m::props::ParaProps { style, align, indent_left, indent_right, indent_first, indent_left_chars, indent_right_chars, indent_first_chars, space_before, space_after, space_before_auto, space_after_auto, line_spacing, contextual_spacing, mirror_indents, adjust_right_indent, keep_next, keep_lines, page_break_before, widow_control, outline_level, numbering, tabs, shading, borders, suppress_hyphens, suppress_line_numbers, bidi, text_direction, drop_cap, drop_cap_margin, drop_cap_distance, drop_cap_font, frame, kinsoku, word_wrap, overflow_punct, top_line_punct, auto_space_de, auto_space_dn, num_change, fmt_change, snap_to_grid, text_alignment }
            $m::props::TabStop { pos, align, leader }
            $m::props::TextFrame { w, h, h_exact, x, y, x_align, y_align, h_anchor, v_anchor, wrap, anchor_lock, h_space, v_space }
            $m::props::CellPicture { media, size, repeat, align }
            $m::props::CellProps { width, width_pct, span, vmerge, shading, borders, valign, margins, partial_margins, text_direction, text_direction_set, no_wrap, hide_mark, fmt_change, picture }
            $m::props::RowProps { height, height_rule, header, cant_split, exceptions, grid_before, w_before, grid_after, w_after, fmt_change }
            $m::props::TableFloat { h_rel, v_rel, x, y, h_align, v_align, dist, overlap }
            $m::props::TableLook { header_row, total_row, banded_rows, first_column, last_column, banded_columns }
            $m::props::TableProps { style, width, width_pct, align, indent, borders, cell_margins, fixed, look, shading, caption, description, float, fmt_change, bidi_visual, space_before, space_after }
            $m::BuildingBlock { name, gallery, category, description, guid, style, types, behaviors, blocks }
            $m::Comment { author, initials, date, parent, resolved, part, durable_id, date_utc }
            $m::Revision { kind, author, date, move_name }
            $m::section::Columns { count, space, separator, widths }
            $m::section::DocGrid { kind, line_pitch, char_space }
            $m::section::HeaderSet { default, first, even }
            $m::section::LineNumbering { count_by, start, distance, restart }
            $m::section::NoteProps { pos, num_fmt, num_start, num_restart }
            $m::section::SectionProps { start, page_w, page_h, landscape, margin_top, margin_bottom, margin_left, margin_right, header, footer, header_gap, footer_gap, gutter, columns, headers, footers, title_page, page_num_start, page_num_format, chapter_style, chapter_sep, page_num_set, line_numbers, valign, form_prot, page_borders, rtl, text_direction, text_direction_set, fmt_change, doc_grid, footnote_pr, endnote_pr, paper_src_first, paper_src_other, no_endnote, footnote_columns, balance_columns }
            $m::CoreProps { title, subject, creator, keywords, description, category, last_modified_by, created, modified, revision }
            $m::CustomProp { name, kind, value, link_target, odf_user_field }
            $m::OdfLayout { tabs_from_indent }
            $m::PagePicture { media, fill_type, title }
            $m::Settings { track_changes, track_moves, default_tab, even_odd_headers, mirror_margins, auto_hyphenation, hyphenation_zone, hyphenate_caps, consecutive_hyphen_limit, page_color, page_picture, watermark, major_font, minor_font, theme_colors, theme_name, footnote_format, endnote_format, footnote_pr, endnote_pr, protection, protect_formatting, protect_hash, compat_mode, roman_fonts, sum_paragraph_spacing, do_not_expand_shift_return, allow_space_of_same_style_in_table, grid_h, grid_v, math, bib_style, attached_template, link_styles, zoom, remove_personal_info, remove_date_time, odf }
            $m::Source { tag, kind, author, corporate, title, year, publisher, city, journal, volume, pages, url, editor, edition, issue, month, day, accessed, doi }
            $m::Watermark { text, font, color, diagonal, semitransparent, size }
            $m::Zoom { percent, fit }
            $m::smart_art::SmartArtItem { text, level, color }
            $m::smart_art::SmartArtSpec { layout, items, colors }
            $m::styles::PartFormat { chr, fill, borders, align, v_align }
            $m::styles::Style { id, name, kind, based_on, next, linked, para, chr, priority, quick, hidden, builtin, locked, default, table, para_change, chr_change }
            $m::styles::StyleSheet { default_chr, default_para, default_para_present, styles, latent_styles }
            $m::styles::TableStyleParts { borders, fill, cell_margins, header_fill, header_chr, header_borders, band_fill, band_chr, band_borders, band_size, col_band_size, first_col_chr, total_chr, total_border_top, first_col_fill, first_col_borders, last_col_fill, last_col_chr, last_col_borders, total_fill, total_borders, col_band_fill, col_band_chr, col_band_borders, regions }
            $m::table::Cell { props, blocks, controls }
            $m::table::Row { props, cells, controls }
            $m::table::Table { props, grid, rows, controls }
            $m::wordart::GradStop { pos, color, transparency }
            $m::wordart::Reflection { transparency, size, distance, blur }
            $m::wordart::TextEffects { fill, outline, shadow, glow, reflection }
            $m::wordart::TextOutline { color, width, transparency }
            $m::wordart::TextWarp { preset, adj }
            $m::wrap::WrapPolygon { edited, points }
        }

        sample_enums! {
            $m::Block { Para(a), Table(a) }
            $m::chart::ChartType {
                Column,
                ColumnStacked,
                Bar,
                BarStacked,
                Line,
                Area,
                Pie,
                Doughnut,
                Scatter,
            }
            $m::chart::LegendPos { Right, Left, Top, Bottom }
            $m::connector::ArrowKind { Triangle, Stealth, Diamond, Oval, Arrow }
            $m::connector::ArrowSize { Small, Medium, Large }
            $m::connector::LineCap { Flat, Square, Round }
            $m::control::ControlKind {
                RichText,
                Text { multi_line },
                CheckBox { checked, checked_char, checked_font, unchecked_char, unchecked_font },
                ComboBox { items, last_value },
                DropDown { items, last_value },
                Date { full_date, format, lid, calendar, store_as },
                Picture,
                Gallery { list, gallery, category, unique },
                RepeatingSection { title, no_insert_delete },
                RepeatingSectionItem,
            }
            $m::control::ControlLock { Unlocked, SdtLocked, ContentLocked, SdtContentLocked }
            $m::fill::GradientFlip { None, X, Y, Xy }
            $m::fill::GradientKind { Linear { angle, scaled }, Path { shape, focus } }
            $m::fill::PathShape { Circle, Rect, Shape }
            $m::freeform::InkTool { Pen, Pencil, Highlighter }
            $m::graphic::GraphicItem {
                Shape { rect, kind, fill, stroke, stroke_width },
                Path { segs, fill, stroke, stroke_width },
                Text { rect, text, size, color, bold, align, font },
                Image { rect, media },
            }
            $m::graphic::GraphicKind { Chart, Diagram, Ink }
            $m::graphic::PathSeg { Move(a, b), Line(a, b), Cubic(a, b, c, d, e, f), Close }
            $m::graphic::TextAlign { Left, Center, Right }
            $m::line_dash::LineDash {
                Solid,
                Dot,
                Dash,
                LongDash,
                DashDot,
                LongDashDot,
                LongDashDotDot,
                SystemDash,
                SystemDot,
                SystemDashDot,
                SystemDashDotDot,
                Custom(a),
            }
            $m::math::BrkBin { Before, After, Repeat }
            $m::math::BrkBinSub { MinusMinus, MinusPlus, PlusMinus }
            $m::math::ColJc { Center, Left, Right }
            $m::math::FracKind { Bar, Skewed, Linear, NoBar }
            $m::math::LimLoc { UndOvr, SubSup }
            $m::math::MNode {
                Run(a),
                Frac { kind, num, den },
                Script { kind, base, sub, sup },
                Rad { deg, deg_hide, e },
                Nary { chr, lim_loc, grow, sub_hide, sup_hide, sub, sup, e },
                Delim { beg, end, sep, grow, shp_match, elems },
                Func { name, e },
                Lim { upper, e, lim },
                Acc { chr, e },
                Bar { top, e },
                BorderBox { hide, strike, e },
                Boxed { e },
                GroupChr { chr, top, e },
                EqArr { rows },
                Matrix { rows, col_jc },
                Phant { show, zero_wid, zero_asc, zero_desc, e },
            }
            $m::math::MScr { Roman, Script, Fraktur, DoubleStruck, SansSerif, Monospace }
            $m::math::MSty { Plain, Bold, Italic, BoldItalic }
            $m::math::MathJc { CenterGroup, Center, Left, Right }
            $m::math::ScriptKind { Sup, Sub, SubSup, Pre }
            $m::MissingPictureReason { MissingPart, ExternalTarget, LinkOnly, Undecodable, UnsupportedFormat }
            $m::numbering::LevelSuffix { Tab, Space, Nothing }
            $m::para::Anchor {
                Column,
                Margin,
                Page,
                Paragraph,
                LeftMargin,
                RightMargin,
                TopMargin,
                BottomMargin,
                InsideMargin,
                OutsideMargin,
                Character,
                Line,
            }
            $m::para::FloatAlign { Start, Center, End, Inside, Outside }
            $m::para::Wrap {
                Inline,
                Square,
                Tight,
                Through,
                TopAndBottom,
                BehindText,
                InFrontOfText,
            }
            $m::para::WrapText { BothSides, Left, Right, Largest }
            $m::para::NoteKind { Footnote, Endnote }
            $m::para::PTabAlign { Left, Center, Right }
            $m::para::PTabBase { Margin, Indent }
            $m::para::RubyAlign { Center, DistributeLetter, DistributeSpace, Left, Right, RightVertical }
            $m::para::ShapeKind {
                Rectangle,
                RoundedRectangle,
                Ellipse,
                FlowchartOr,
                Triangle,
                RightTriangle,
                Diamond,
                Line,
                Arrow,
                ArrowLeft,
                ArrowUp,
                ArrowDown,
                Star,
                Heart,
                TextBox,
                Freeform,
                StraightConnector,
                ElbowConnector,
                CurvedConnector,
            }
            $m::para::InlineObject {
                Image { media, w, h, alt, float, crop, fx, ole },
                Graphic { w, h, alt, float, graphic },
                Shape { kind, w, h, fill, stroke, stroke_width, float, story, freeform, effects, extra },
                Group { w, h, float, ch_w, ch_h, children, canvas },
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
                Equation { linear, display, math },
                Ruby { base, ruby, align, size, raise, base_size, lang, props },
                PositionalTab { align, relative_to, leader },
                Opaque { format, xml, text },
            }
            $m::PartKind { Header, Footer, Footnote, Endnote, Comment, TextBox }
            $m::props::CombineBrackets { None, Round, Square, Angle, Curly }
            $m::props::EmphasisMark { None, Dot, Comma, Circle, UnderDot }
            $m::props::Highlight {
                None,
                Yellow,
                BrightGreen,
                Turquoise,
                Pink,
                Blue,
                Red,
                DarkBlue,
                Teal,
                Green,
                Violet,
                DarkRed,
                DarkYellow,
                Gray50,
                Gray25,
                Black,
                White,
            }
            $m::props::Underline {
                None,
                Single,
                Words,
                Double,
                Thick,
                Dotted,
                Dash,
                DotDash,
                DotDotDash,
                Wave,
                DoubleWave,
            }
            $m::props::VertAlign { Baseline, Superscript, Subscript }
            $m::props::TextColor { Auto, Rgb(a) }
            $m::props::Align { Left, Center, Right, Justify, Distribute }
            $m::props::BorderStyle {
                None,
                Single,
                Double,
                Dotted,
                Dashed,
                Thick,
                Triple,
                DotDash,
                Wave,
                ThinThickSmallGap,
                ThickThinSmallGap,
                ThinThickThinSmallGap,
                ThinThickMediumGap,
                ThickThinMediumGap,
                ThinThickThinMediumGap,
                ThinThickLargeGap,
                ThickThinLargeGap,
                ThinThickThinLargeGap,
            }
            $m::props::LineSpacing { Multiple(a), AtLeast(a), Exactly(a) }
            $m::props::ParagraphTextDirection {
                LrTb,
                LrTbV,
                Tb,
                TbRl,
                TbRlV,
                TbLr,
                TbLrV,
                BtLr,
                TbV,
                Rl,
                RlV,
                Lr,
                LrV,
            }
            $m::props::TabAlign { Left, Center, Right, Decimal, Bar, Clear }
            $m::props::TabLeader { None, Dot, Hyphen, Underscore, MiddleDot }
            $m::props::TextAlignment { Auto, Top, Center, Baseline, Bottom }
            $m::props::HeightRule { Auto, AtLeast, Exact }
            $m::props::PictureRepeat { None, Tile, Stretch }
            $m::props::TextDirection { Horizontal, Down, Up }
            $m::props::TextVert { Horz, Vert, Vert270, Stacked }
            $m::props::VAlign { Top, Center, Bottom }
            $m::props::VMerge { None, Restart, Continue }
            $m::RevisionKind { Insert, Delete, Format }
            $m::section::ChapterSep { Hyphen, Period, Colon, EmDash, EnDash }
            $m::section::DocGridType { Default, Lines, LinesAndChars, SnapToChars }
            $m::section::LineNumberRestart { Page, Section, Continuous }
            $m::section::NotePos { PageBottom, BeneathText, SectEnd, DocEnd }
            $m::section::NoteRestart { Continuous, EachSect, EachPage }
            $m::section::NumFormat {
                Decimal,
                UpperRoman,
                LowerRoman,
                UpperLetter,
                LowerLetter,
                Ordinal,
                CardinalText,
                OrdinalText,
                DecimalZero,
                Bullet,
                None,
                Hex,
                Chicago,
                IdeographDigital,
                JapaneseCounting,
                Aiueo,
                Iroha,
                DecimalFullWidth,
                DecimalHalfWidth,
                JapaneseLegal,
                JapaneseDigitalTenThousand,
                DecimalEnclosedCircle,
                DecimalFullWidth2,
                AiueoFullWidth,
                IrohaFullWidth,
                Ganada,
                Chosung,
                DecimalEnclosedFullstop,
                DecimalEnclosedParen,
                DecimalEnclosedCircleChinese,
                IdeographEnclosedCircle,
                IdeographTraditional,
                IdeographZodiac,
                IdeographZodiacTraditional,
                TaiwaneseCounting,
                IdeographLegalTraditional,
                TaiwaneseCountingThousand,
                TaiwaneseDigital,
                ChineseCounting,
                ChineseLegalSimplified,
                ChineseCountingThousand,
                KoreanDigital,
                KoreanCounting,
                KoreanLegal,
                KoreanDigital2,
                VietnameseCounting,
                RussianLower,
                RussianUpper,
                NumberInDash,
                Hebrew1,
                Hebrew2,
                ArabicAlpha,
                ArabicAbjad,
                HindiVowels,
                HindiConsonants,
                HindiNumbers,
                HindiCounting,
                ThaiLetters,
                ThaiNumbers,
                ThaiCounting,
                BahtText,
                DollarText,
            }
            $m::section::PageVAlign { Top, Center, Justify, Bottom }
            $m::section::SectionStart { NextPage, Continuous, EvenPage, OddPage, NextColumn }
            $m::ZoomFit { OnePage, PageWidth, TextWidth }
            $m::smart_art::SmartArtColors {
                Accent1,
                Accent2,
                Accent3,
                Accent4,
                Accent5,
                Accent6,
                Colorful,
            }
            $m::smart_art::SmartArtLayout {
                BasicList,
                Process,
                Cycle,
                Hierarchy,
                Pyramid,
                Radial,
                Matrix,
                Venn,
            }
            $m::styles::StyleKind { Paragraph, Character, Table, Numbering }
            $m::styles::TablePart {
                WholeTable,
                FirstRow,
                LastRow,
                FirstCol,
                LastCol,
                Band1Vert,
                Band2Vert,
                Band1Horz,
                Band2Horz,
                NeCell,
                NwCell,
                SeCell,
                SwCell,
            }
            $m::wordart::TextFill { None, Solid { color, transparency }, Gradient { stops, angle } }
        }
    };
}

/// The shared model's samples.
pub(crate) mod shared {
    use craft_wordmodel as m;

    model_samples!(m);
}

/// WordCraft's own.
pub(crate) mod own {
    use wordcraft_doc as m;

    model_samples!(m);
}
