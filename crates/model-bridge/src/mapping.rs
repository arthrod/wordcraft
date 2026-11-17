//! The members of every type the two models share, one line per type.
//!
//! `A = B` writes all four conversions (by value and by reference, each way) from one list of
//! members. The lists were first written from the two crates' sources by a script and are kept
//! by hand: the macro takes each struct apart with a pattern that names every field and builds
//! its twin with a literal that names every field, so when either model gains or loses a member
//! this file stops compiling at the line to change.
//!
//! `name = json::opt(name)` marks an optional number: one that is not finite becomes `None`,
//! which is what the JSON bridge this mapping replaced did (`null` read back as "absent"). A
//! number that is not optional is checked in `Bridge`'s `Floats`. `name = json::zero(name)` marks a
//! number JSON left out when zero: `-0.0` becomes `0.0`.
//!
//! The types that hold the document's stories (blocks, paragraphs, tables, inline objects) and
//! the few written by hand are in `stories.rs`.

use craft_convert::convert_types;
use craft_wordmodel as wm;
use wordcraft_doc as doc;

use crate::{Bridge, json};

// `chart`
convert_types! {
    marker = Bridge;
    enum wm::chart::ChartType = doc::chart::ChartType {
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
    enum wm::chart::LegendPos = doc::chart::LegendPos { Right, Left, Top, Bottom }
    struct wm::chart::ChartSeries = doc::chart::ChartSeries { name, values = json::opt(values) }
    struct wm::chart::ChartSpec = doc::chart::ChartSpec { kind, categories, series, title, legend, data_labels }
}

// `connector`
convert_types! {
    marker = Bridge;
    enum wm::connector::LineCap = doc::connector::LineCap { Flat, Square, Round }
    enum wm::connector::ArrowSize = doc::connector::ArrowSize { Small, Medium, Large }
    enum wm::connector::ArrowKind = doc::connector::ArrowKind { Triangle, Stealth, Diamond, Oval, Arrow }
    struct wm::connector::ArrowStyle = doc::connector::ArrowStyle { kind, width, length }
    struct wm::connector::ShapeExtra = doc::connector::ShapeExtra { id, warp, start, end, arrow_start, arrow_end, start_style, end_style, corner, dash, cap, outline_fill, odf_geometry, hr }
    struct wm::connector::HorizontalRule = doc::connector::HorizontalRule { pct, align, standard }
    struct wm::connector::ConnEnd = doc::connector::ConnEnd { id, site }
}

// `control`
convert_types! {
    marker = Bridge;
    enum wm::control::ControlLock = doc::control::ControlLock { Unlocked, SdtLocked, ContentLocked, SdtContentLocked }
    struct wm::control::ListItem = doc::control::ListItem { display, value }
    enum wm::control::ControlKind = doc::control::ControlKind {
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
    struct wm::control::ContentControl = doc::control::ContentControl { id, tag, title, lock, placeholder, showing_placeholder, temporary, kind, placeholder_text, block, rpr_xml, run_props, extra, end_pr_xml, form_size, form_raise = json::opt(form_raise), form_xml }
    struct wm::control::ControlWrap = doc::control::ControlWrap { open, close }
}

// `effects`
convert_types! {
    marker = Bridge;
    struct wm::effects::ShapeEffects = doc::effects::ShapeEffects { shadow, glow, soft_edge = json::opt(soft_edge) }
    struct wm::effects::Shadow = doc::effects::Shadow { color, transparency, blur, distance, angle, rot_with_shape }
    struct wm::effects::Glow = doc::effects::Glow { color, size, transparency }
}

// `fill`
convert_types! {
    marker = Bridge;
    struct wm::fill::Gradient = doc::fill::Gradient { stops, kind, tile, rot_with_shape, flip }
    enum wm::fill::GradientKind = doc::fill::GradientKind { Linear { angle, scaled }, Path { shape, focus } }
    enum wm::fill::PathShape = doc::fill::PathShape { Circle, Rect, Shape }
    enum wm::fill::GradientFlip = doc::fill::GradientFlip { None, X, Y, Xy }
}

// `freeform`
convert_types! {
    marker = Bridge;
    enum wm::freeform::InkTool = doc::freeform::InkTool { Pen, Pencil, Highlighter }
    struct wm::freeform::FreePath = doc::freeform::FreePath { pts, closed }
    struct wm::freeform::Freeform = doc::freeform::Freeform { w, h, paths, alpha, ink, order }
}

// `graphic`
convert_types! {
    marker = Bridge;
    struct wm::graphic::Graphic = doc::graphic::Graphic { kind, items, w, h, source, smart_art, chart }
    struct wm::graphic::Embedded = doc::graphic::Embedded { xml, refs }
    struct wm::graphic::EmbeddedRef = doc::graphic::EmbeddedRef { id, kind, target, external }
    enum wm::graphic::GraphicKind = doc::graphic::GraphicKind { Chart, Diagram, Ink }
    enum wm::graphic::GraphicItem = doc::graphic::GraphicItem {
        Shape { rect, kind, fill, stroke, stroke_width },
        Path { segs, fill, stroke, stroke_width },
        Text { rect, text, size, color, bold, align, font },
        Image { rect, media },
    }
    enum wm::graphic::TextAlign = doc::graphic::TextAlign { Left, Center, Right }
    enum wm::graphic::PathSeg = doc::graphic::PathSeg { Move(a, b), Line(a, b), Cubic(a, b, c, d, e, f), Close }
}

// `line_dash`
convert_types! {
    marker = Bridge;
    enum wm::line_dash::LineDash = doc::line_dash::LineDash {
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
}

// `math`
convert_types! {
    marker = Bridge;
    struct wm::math::Math = doc::math::Math { nodes, omml, jc, linear }
    enum wm::math::MathJc = doc::math::MathJc { CenterGroup, Center, Left, Right }
    enum wm::math::BrkBin = doc::math::BrkBin { Before, After, Repeat }
    enum wm::math::BrkBinSub = doc::math::BrkBinSub { MinusMinus, MinusPlus, PlusMinus }
    struct wm::math::MathProps = doc::math::MathProps { brk_bin, brk_bin_sub, wrap_indent, wrap_right }
    enum wm::math::MSty = doc::math::MSty { Plain, Bold, Italic, BoldItalic }
    enum wm::math::MScr = doc::math::MScr { Roman, Script, Fraktur, DoubleStruck, SansSerif, Monospace }
    struct wm::math::MRun = doc::math::MRun { text, sty, scr, nor, lit, size = json::opt(size), color, font, brk }
    enum wm::math::FracKind = doc::math::FracKind { Bar, Skewed, Linear, NoBar }
    enum wm::math::ScriptKind = doc::math::ScriptKind { Sup, Sub, SubSup, Pre }
    enum wm::math::LimLoc = doc::math::LimLoc { UndOvr, SubSup }
    enum wm::math::ColJc = doc::math::ColJc { Center, Left, Right }
    enum wm::math::MNode = doc::math::MNode {
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
}

// `missing`
convert_types! {
    marker = Bridge;
    enum wm::MissingPictureReason = doc::MissingPictureReason { MissingPart, ExternalTarget, LinkOnly, Undecodable, UnsupportedFormat }
    struct wm::MissingPicture = doc::MissingPicture { part, story, reason }
}

// `numbering`
convert_types! {
    marker = Bridge;
    enum wm::numbering::LevelSuffix = doc::numbering::LevelSuffix { Tab, Space, Nothing }
    struct wm::numbering::Level = doc::numbering::Level { start, format, text, align, indent, hanging, suffix, chr, restart, restart_after, tab = json::opt(tab), legal, style }
    struct wm::numbering::AbstractNum = doc::numbering::AbstractNum { id, name, levels, nsid, tmpl, multi_level_type }
    struct wm::numbering::Num = doc::numbering::Num { id, abstract_id, start_overrides, level_overrides }
    struct wm::numbering::Numbering = doc::numbering::Numbering { abstracts, nums }
}

// `para/float`
convert_types! {
    marker = Bridge;
    enum wm::para::Wrap = doc::para::Wrap {
        Inline,
        Square,
        Tight,
        Through,
        TopAndBottom,
        BehindText,
        InFrontOfText,
    }
    enum wm::para::WrapText = doc::para::WrapText { BothSides, Left, Right, Largest }
    enum wm::para::Anchor = doc::para::Anchor {
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
    enum wm::para::FloatAlign = doc::para::FloatAlign { Start, Center, End, Inside, Outside }
    struct wm::para::Float = doc::para::Float { wrap, wrap_text, h_rel, v_rel, x, y, h_align, v_align, dist, dist_top, dist_bottom, effect, rot = json::zero(rot), flip_h, flip_v, wrap_polygon, alt, hidden, outside_cell, z, page, bare_turn }
    struct wm::para::AltText = doc::para::AltText { text, title, decorative }
}

// `para/inline_kinds`
convert_types! {
    marker = Bridge;
    enum wm::para::ShapeKind = doc::para::ShapeKind {
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
    enum wm::para::NoteKind = doc::para::NoteKind { Footnote, Endnote }
    enum wm::para::PTabAlign = doc::para::PTabAlign { Left, Center, Right }
    enum wm::para::PTabBase = doc::para::PTabBase { Margin, Indent }
    enum wm::para::RubyAlign = doc::para::RubyAlign { Center, DistributeLetter, DistributeSpace, Left, Right, RightVertical }
}

// `para/inline_object`
convert_types! {
    marker = Bridge;
    struct wm::para::CanvasStyle = doc::para::CanvasStyle { fill, stroke, stroke_width }
}

// `parts`
convert_types! {
    marker = Bridge;
    enum wm::PartKind = doc::PartKind { Header, Footer, Footnote, Endnote, Comment, TextBox }
    struct wm::TextBody = doc::TextBody { vert, anchor, next, inset, auto_fit, auto_color }
}

// `picfx`
convert_types! {
    marker = Bridge;
    struct wm::picfx::PicFx = doc::picfx::PicFx { bright, contrast, transparency }
}

// `props/char_props`
convert_types! {
    marker = Bridge;
    enum wm::props::Underline = doc::props::Underline {
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
    struct wm::props::Ligatures = doc::props::Ligatures { standard, contextual, historical, discretional }
    enum wm::props::VertAlign = doc::props::VertAlign { Baseline, Superscript, Subscript }
    enum wm::props::Highlight = doc::props::Highlight {
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
    struct wm::props::CharProps = doc::props::CharProps { style, font, sym_font, size = json::opt(size), bold, italic, underline, underline_color, strike, double_strike, color, highlight, shading, border, vert_align, caps, small_caps, hidden, web_hidden, snap_to_grid, spacing = json::opt(spacing), scale = json::opt(scale), position = json::opt(position), kern = json::opt(kern), emphasis, ligatures, outline, shadow, emboss, engrave, text_effects, lang, no_proof, rtl, cs, font_cs, font_ea, size_cs = json::opt(size_cs), bold_cs, italic_cs, lang_bidi, lang_ea, east_asian_layout, link, ins, del, fmt_change }
    struct wm::props::EastAsianLayout = doc::props::EastAsianLayout { id, combine, combine_brackets, vert, vert_compress }
    enum wm::props::EmphasisMark = doc::props::EmphasisMark { None, Dot, Comma, Circle, UnderDot }
    enum wm::props::CombineBrackets = doc::props::CombineBrackets { None, Round, Square, Angle, Curly }
    struct wm::props::NumChange = doc::props::NumChange { rev, original }
}

// `props/color`
convert_types! {
    marker = Bridge;
    enum wm::props::TextColor = doc::props::TextColor { Auto, Rgb(a) }
}

// `props/para_props`
convert_types! {
    marker = Bridge;
    enum wm::props::Align = doc::props::Align { Left, Center, Right, Justify, Distribute }
    enum wm::props::ParagraphTextDirection = doc::props::ParagraphTextDirection {
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
    enum wm::props::TextAlignment = doc::props::TextAlignment { Auto, Top, Center, Baseline, Bottom }
    enum wm::props::LineSpacing = doc::props::LineSpacing { Multiple(a), AtLeast(a), Exactly(a) }
    enum wm::props::TabAlign = doc::props::TabAlign { Left, Center, Right, Decimal, Bar, Clear }
    enum wm::props::TabLeader = doc::props::TabLeader { None, Dot, Hyphen, Underscore, MiddleDot }
    struct wm::props::TabStop = doc::props::TabStop { pos, align, leader }
    enum wm::props::BorderStyle = doc::props::BorderStyle {
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
    struct wm::props::Border = doc::props::Border { style, width, color, space }
    struct wm::props::Borders = doc::props::Borders { top, left, bottom, right, between, inside_v }
    struct wm::props::NumRef = doc::props::NumRef { num, level }
    struct wm::props::TextFrame = doc::props::TextFrame { w = json::opt(w), h = json::opt(h), h_exact, x, y, x_align, y_align, h_anchor, v_anchor, wrap, anchor_lock, h_space, v_space }
    struct wm::props::ParaProps = doc::props::ParaProps { style, align, indent_left = json::opt(indent_left), indent_right = json::opt(indent_right), indent_first = json::opt(indent_first), indent_left_chars = json::opt(indent_left_chars), indent_right_chars = json::opt(indent_right_chars), indent_first_chars = json::opt(indent_first_chars), space_before = json::opt(space_before), space_after = json::opt(space_after), space_before_auto, space_after_auto, line_spacing, contextual_spacing, mirror_indents, adjust_right_indent, keep_next, keep_lines, page_break_before, widow_control, outline_level, numbering, tabs, shading, borders, suppress_hyphens, suppress_line_numbers, bidi, text_direction, drop_cap, drop_cap_margin, drop_cap_distance = json::opt(drop_cap_distance), drop_cap_font, frame, kinsoku, word_wrap, overflow_punct, top_line_punct, auto_space_de, auto_space_dn, num_change, fmt_change, snap_to_grid, text_alignment }
}

// `props/table_props`
convert_types! {
    marker = Bridge;
    enum wm::props::VAlign = doc::props::VAlign { Top, Center, Bottom }
    enum wm::props::TextDirection = doc::props::TextDirection { Horizontal, Down, Up }
    enum wm::props::TextVert = doc::props::TextVert { Horz, Vert, Vert270, Stacked }
    struct wm::props::TableProps = doc::props::TableProps { style, width = json::opt(width), width_pct = json::opt(width_pct), align, indent = json::opt(indent), borders, cell_margins, fixed, look, shading, caption, description, float, fmt_change, bidi_visual, space_before = json::opt(space_before), space_after = json::opt(space_after) }
    struct wm::props::TableFloat = doc::props::TableFloat { h_rel, v_rel, x, y, h_align, v_align, dist, overlap }
    struct wm::props::TableLook = doc::props::TableLook { header_row, total_row, banded_rows, first_column, last_column, banded_columns }
    enum wm::props::HeightRule = doc::props::HeightRule { Auto, AtLeast, Exact }
    struct wm::props::RowProps = doc::props::RowProps { height = json::opt(height), height_rule, header, cant_split, exceptions, grid_before, w_before = json::opt(w_before), grid_after, w_after = json::opt(w_after), fmt_change }
    enum wm::props::VMerge = doc::props::VMerge { None, Restart, Continue }
    struct wm::props::CellProps = doc::props::CellProps { width = json::opt(width), width_pct = json::opt(width_pct), span, vmerge, shading, borders, valign, margins, partial_margins = json::opt(partial_margins), text_direction, text_direction_set, no_wrap, hide_mark, fmt_change, picture }
    struct wm::props::CellPicture = doc::props::CellPicture { media, size, repeat, align }
    enum wm::props::PictureRepeat = doc::props::PictureRepeat { None, Tile, Stretch }
}

// `review`
convert_types! {
    marker = Bridge;
    struct wm::Comment = doc::Comment { author, initials, date, parent, resolved, part, durable_id, date_utc }
    enum wm::RevisionKind = doc::RevisionKind { Insert, Delete, Format }
    struct wm::Revision = doc::Revision { kind, author, date, move_name }
}

// `section`
convert_types! {
    marker = Bridge;
    enum wm::section::PageVAlign = doc::section::PageVAlign { Top, Center, Justify, Bottom }
    enum wm::section::SectionStart = doc::section::SectionStart { NextPage, Continuous, EvenPage, OddPage, NextColumn }
    enum wm::section::NumFormat = doc::section::NumFormat {
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
    enum wm::section::ChapterSep = doc::section::ChapterSep { Hyphen, Period, Colon, EmDash, EnDash }
    enum wm::section::NotePos = doc::section::NotePos { PageBottom, BeneathText, SectEnd, DocEnd }
    enum wm::section::NoteRestart = doc::section::NoteRestart { Continuous, EachSect, EachPage }
    struct wm::section::NoteProps = doc::section::NoteProps { pos, num_fmt, num_start, num_restart }
    struct wm::section::HeaderSet = doc::section::HeaderSet { default, first, even }
    struct wm::section::Columns = doc::section::Columns { count, space, separator, widths }
    struct wm::section::LineNumbering = doc::section::LineNumbering { count_by, start, distance, restart }
    enum wm::section::LineNumberRestart = doc::section::LineNumberRestart { Page, Section, Continuous }
    enum wm::section::DocGridType = doc::section::DocGridType { Default, Lines, LinesAndChars, SnapToChars }
    struct wm::section::DocGrid = doc::section::DocGrid { kind, line_pitch, char_space }
    struct wm::section::SectionProps = doc::section::SectionProps { start, page_w, page_h, landscape, margin_top, margin_bottom, margin_left, margin_right, header, footer, header_gap = json::zero(header_gap), footer_gap = json::zero(footer_gap), gutter, columns, headers, footers, title_page, page_num_start, page_num_format, chapter_style, chapter_sep, page_num_set, line_numbers, valign, form_prot, page_borders, rtl, text_direction, text_direction_set, fmt_change, doc_grid, footnote_pr, endnote_pr, paper_src_first, paper_src_other, no_endnote, footnote_columns, balance_columns }
}

// `settings`
convert_types! {
    marker = Bridge;
    struct wm::Source = doc::Source { tag, kind, author, corporate, title, year, publisher, city, journal, volume, pages, url, editor, edition, issue, month, day, accessed, doi }
    struct wm::CustomProp = doc::CustomProp { name, kind, value, link_target, odf_user_field }
    struct wm::CoreProps = doc::CoreProps { title, subject, creator, keywords, description, category, last_modified_by, created, modified, revision }
    struct wm::Settings = doc::Settings { track_changes, track_moves, default_tab, even_odd_headers, mirror_margins, auto_hyphenation, hyphenation_zone, hyphenate_caps, consecutive_hyphen_limit, page_color, page_picture, watermark, major_font, minor_font, theme_colors, theme_name, footnote_format, endnote_format, footnote_pr, endnote_pr, protection, protect_formatting, protect_hash, compat_mode, roman_fonts, sum_paragraph_spacing, do_not_expand_shift_return, allow_space_of_same_style_in_table, grid_h, grid_v, math, bib_style, attached_template, link_styles, zoom, remove_personal_info, remove_date_time, odf }
    struct wm::OdfLayout = doc::OdfLayout { tabs_from_indent }
    struct wm::Zoom = doc::Zoom { percent, fit }
    enum wm::ZoomFit = doc::ZoomFit { OnePage, PageWidth, TextWidth }
    struct wm::PagePicture = doc::PagePicture { media, fill_type, title }
    struct wm::Watermark = doc::Watermark { text, font, color, diagonal, semitransparent, size }
}

// `smart_art`
convert_types! {
    marker = Bridge;
    enum wm::smart_art::SmartArtLayout = doc::smart_art::SmartArtLayout {
        BasicList,
        Process,
        Cycle,
        Hierarchy,
        Pyramid,
        Radial,
        Matrix,
        Venn,
    }
    enum wm::smart_art::SmartArtColors = doc::smart_art::SmartArtColors {
        Accent1,
        Accent2,
        Accent3,
        Accent4,
        Accent5,
        Accent6,
        Colorful,
    }
    struct wm::smart_art::SmartArtItem = doc::smart_art::SmartArtItem { text, level, color }
    struct wm::smart_art::SmartArtSpec = doc::smart_art::SmartArtSpec { layout, items, colors }
}

// `styles`
convert_types! {
    marker = Bridge;
    enum wm::styles::StyleKind = doc::styles::StyleKind { Paragraph, Character, Table, Numbering }
    struct wm::styles::Style = doc::styles::Style { id, name, kind, based_on, next, linked, para, chr, priority, quick, hidden, builtin, locked, para_change, chr_change, default, table }
    struct wm::styles::TableStyleParts = doc::styles::TableStyleParts { borders, fill, cell_margins, header_fill, header_chr, header_borders, band_fill, band_chr, band_borders, band_size, col_band_size, first_col_chr, total_chr, total_border_top, first_col_fill, first_col_borders, last_col_fill, last_col_chr, last_col_borders, total_fill, total_borders, col_band_fill, col_band_chr, col_band_borders, regions }
    enum wm::styles::TablePart = doc::styles::TablePart {
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
    struct wm::styles::PartFormat = doc::styles::PartFormat { chr, fill, borders, align, v_align }
    struct wm::styles::StyleSheet = doc::styles::StyleSheet { default_chr, default_para, default_para_present, styles, latent_styles }
}

// `wordart`
convert_types! {
    marker = Bridge;
    struct wm::wordart::TextEffects = doc::wordart::TextEffects { fill, outline, shadow, glow, reflection }
    enum wm::wordart::TextFill = doc::wordart::TextFill { None, Solid { color, transparency }, Gradient { stops, angle } }
    struct wm::wordart::GradStop = doc::wordart::GradStop { pos, color, transparency }
    struct wm::wordart::TextOutline = doc::wordart::TextOutline { color, width, transparency }
    struct wm::wordart::Reflection = doc::wordart::Reflection { transparency, size, distance, blur }
    struct wm::wordart::TextWarp = doc::wordart::TextWarp { preset, adj }
}

// `wrap`
convert_types! {
    marker = Bridge;
    struct wm::wrap::WrapPolygon = doc::wrap::WrapPolygon { edited, points }
}
