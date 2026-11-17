use super::{CharProps, Document, Paragraph, ResolvedChar};

#[cfg(any(feature = "inline-proof", feature = "hyphenation"))]
pub use wordcraft_proof::ProofLang;

/// Placeholder proofing language when `inline-proof` and `hyphenation` are compiled out.
#[cfg(not(any(feature = "inline-proof", feature = "hyphenation")))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct ProofLang;

#[cfg(not(any(feature = "inline-proof", feature = "hyphenation")))]
impl ProofLang {
    pub fn from_tag(_tag: Option<&str>) -> Self {
        Self
    }
}

/// One run's proofing: its bytes in the paragraph, its language (`w:lang`, resolved through the
/// styles: the document default, paragraph and character styles, direct formatting) and whether
/// it is left unchecked ("Do not check spelling or grammar", hidden text, hyperlinks).
#[derive(Clone, Debug, PartialEq)]
pub struct ProofRun {
    pub range: std::ops::Range<usize>,
    pub lang: ProofLang,
    pub skip: bool,
}

impl ProofRun {
    pub(super) fn of(range: std::ops::Range<usize>, rc: &ResolvedChar) -> ProofRun {
        ProofRun { range, lang: ProofLang::from_tag(rc.lang.as_deref()), skip: rc.no_proof || rc.hidden || rc.link.is_some() }
    }
}

/// The proofing runs of a paragraph, in order. `table` is the cell's table-style character
/// formatting ([`CellText::chr`]) for a paragraph in a table, as layout resolves it.
pub fn proof_runs(doc: &Document, p: &Paragraph, table: Option<&CharProps>) -> Vec<ProofRun> {
    let style = p.props.style.as_deref();
    p.run_ranges().map(|(r, c)| ProofRun::of(r, &doc.styles.resolve_char_in(style, table, c))).collect()
}

/// The language of the run holding byte `off` (English past the last run).
#[cfg_attr(not(any(feature = "inline-proof", feature = "hyphenation")), allow(dead_code))]
pub(super) fn lang_at(runs: &[ProofRun], off: usize) -> ProofLang {
    let k = runs.partition_point(|r| r.range.end <= off);
    runs.get(k).filter(|r| r.range.start <= off).map(|r| r.lang).unwrap_or_default()
}

/// Spelling and grammar issues in a paragraph, in order: each word in its run's language
/// ([`wordcraft_proof::check_text_with`]: what spelling skips, and grammar only with `grammar`),
/// and none touching a run left unchecked.
///
/// `list_item` (see [`is_list_item`]) turns off the capital-first-word grammar rule.
#[cfg(feature = "inline-proof")]
pub fn paragraph_issues(
    p: &Paragraph,
    runs: &[ProofRun],
    spelling: &wordcraft_proof::Options,
    grammar: bool,
    list_item: bool,
) -> Vec<wordcraft_proof::Issue> {
    if p.text.trim().is_empty() || p.text.len() > 100_000 {
        return Vec::new();
    }
    let text = proof_text(p);
    let spans: Vec<(std::ops::Range<usize>, ProofLang)> = runs.iter().map(|r| (r.range.clone(), r.lang)).collect();
    let skipped = skipped_ranges(runs);
    let issues = wordcraft_proof::check_text_with(&text, &spans, spelling, grammar, list_item);
    if skipped.is_empty() {
        return issues;
    }
    issues.into_iter().filter(|i| !overlaps(&skipped, i.start, i.end)).collect()
}

/// The byte ranges of the runs left unchecked, in order, those that overlap merged: their
/// starts and ends both rise, so [`overlaps`] finds an issue's by binary search instead of
/// looking at every run for every issue.
#[cfg(any(test, feature = "inline-proof"))]
fn skipped_ranges(runs: &[ProofRun]) -> Vec<(usize, usize)> {
    let mut ranges: Vec<(usize, usize)> = runs.iter().filter(|r| r.skip).map(|r| (r.range.start, r.range.end)).collect();
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (s, e) in ranges {
        crate::work::steps(1);
        match merged.last_mut() {
            Some(last) if s < last.1 => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    merged
}

/// Whether bytes `a..b` overlap one of `skipped` ([`skipped_ranges`]): the first range ending
/// after `a` is the only one that can, as every later one starts no earlier.
#[cfg(any(test, feature = "inline-proof"))]
fn overlaps(skipped: &[(usize, usize)], a: usize, b: usize) -> bool {
    crate::work::steps(1);
    let k = skipped.partition_point(|&(_, e)| e <= a);
    skipped.get(k).is_some_and(|&(s, _)| s < b)
}

/// Whether the paragraph is a list item: its own numbering, or its style's, names a list (not 0).
pub fn is_list_item(doc: &Document, p: &Paragraph) -> bool {
    p.props.numbering.or_else(|| doc.styles.resolve_para(&p.props).numbering).is_some_and(|n| n.num != 0)
}

/// Spelling and grammar issues as squiggles (start, end, is grammar), less spelling issues overlapping `typing`.
#[cfg(feature = "inline-proof")]
pub(super) fn proof_issues(
    p: &Paragraph,
    runs: &[ProofRun],
    opts: &crate::ProofOptions,
    typing: Option<(usize, usize)>,
    list_item: bool,
) -> Vec<(usize, usize, bool)> {
    paragraph_issues(p, runs, &opts.spelling, opts.grammar, list_item)
        .into_iter()
        .map(|i| (i.start, i.end, i.kind == wordcraft_proof::IssueKind::Grammar))
        .filter(|&(a, b, grammar)| grammar || !typing.is_some_and(|(s, e)| a < e && s < b))
        .collect()
}

/// Empty squiggles when `inline-proof` is disabled (background workers or core builds without proofing).
#[cfg(not(feature = "inline-proof"))]
pub(super) fn proof_issues(
    _p: &Paragraph,
    _runs: &[ProofRun],
    _opts: &crate::ProofOptions,
    _typing: Option<(usize, usize)>,
    _list_item: bool,
) -> Vec<(usize, usize, bool)> {
    Vec::new()
}

/// The parts a *worker* proof check of `p` needs (deferred-items plan §3.2): the proof text,
/// the language of each byte range as a BCP 47 tag, the byte ranges left unchecked, and the
/// list-item flag — exactly what [`paragraph_issues`] checks inline, in the same byte space, so
/// a reply's spans are `ParaLayout::issues`-shaped and the display can draw them as-is.
///
/// The whole paragraph in one string, bounded like the inline check: `None` for a paragraph
/// that is empty or past 100,000 bytes (nothing to check; a worker request would refuse it).
#[cfg(any(feature = "inline-proof", feature = "hyphenation"))]
pub fn check_parts(doc: &Document, p: &Paragraph) -> Option<(String, Vec<(u32, u32, String)>, Vec<(u32, u32)>, bool)> {
    if p.text.trim().is_empty() || p.text.len() > 100_000 {
        return None;
    }
    let runs = proof_runs(doc, p, None);
    let text = proof_text(p);
    let mut spans: Vec<(u32, u32, String)> = Vec::with_capacity(runs.len());
    let mut skip: Vec<(u32, u32)> = Vec::new();
    for r in &runs {
        let (Some(start), Some(end)) = (u32::try_from(r.range.start).ok(), u32::try_from(r.range.end).ok()) else { continue };
        if r.skip {
            skip.push((start, end));
        } else {
            spans.push((start, end, r.lang.tag().to_string()));
        }
    }
    // The worker applies its own `skip` filter; `overlaps` there (like the inline check's) is
    // easiest on merged, ordered ranges.
    skip.sort_unstable();
    let mut merged: Vec<(u32, u32)> = Vec::with_capacity(skip.len());
    for (s, e) in skip {
        match merged.last_mut() {
            Some(last) if s < last.1 => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    Some((text, spans, merged, is_list_item(doc, p)))
}

/// Text for proofing with the same byte offsets: inline objects and tracked deletions become
/// U+0001 bytes (neither words nor spaces).
pub fn proof_text(p: &wordcraft_doc::Paragraph) -> String {
    let mut out = String::with_capacity(p.text.len());
    // The run holding each character ([`Paragraph::props_of_char`]), found by walking the runs
    // along with the text rather than from the first run for every character.
    let mut runs = p.runs.iter();
    let mut run = runs.next();
    let mut end = run.map_or(0, |r| r.len);
    // Each run this walk looks at is a step ([`crate::work`]), so the linear-time test counts
    // them instead of timing the walk: a caller that went back to the first run for every
    // character would count the runs once per character and show up as a quadratic count (#5185).
    crate::work::steps(usize::from(run.is_some()));
    for (i, c) in p.text.char_indices() {
        while run.is_some() && i >= end {
            run = runs.next();
            end = end.saturating_add(run.map_or(0, |r| r.len));
            crate::work::steps(usize::from(run.is_some()));
        }
        let props = run.map_or(&p.mark, |r| &r.props);
        let deleted = c == wordcraft_doc::para::OBJ || props.del.is_some();
        if deleted {
            for _ in 0..c.len_utf8() {
                out.push('\u{1}');
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(range: std::ops::Range<usize>, skip: bool) -> ProofRun {
        ProofRun { range, lang: Default::default(), skip }
    }

    /// The lookup finds what a look at every run finds, overlapping and empty runs included.
    #[test]
    fn skipped_lookup_matches_a_scan_of_every_run() {
        let runs = [run(0..4, false), run(4..9, true), run(5..6, true), run(9..9, true), run(9..12, false), run(12..20, true), run(14..30, true)];
        let skipped = skipped_ranges(&runs);
        for a in 0..32 {
            for b in a..32 {
                let scan = runs.iter().any(|r| r.skip && r.range.start < b && a < r.range.end);
                assert_eq!(overlaps(&skipped, a, b), scan, "{a}..{b}");
            }
        }
        assert!(skipped_ranges(&[run(0..5, false)]).is_empty());
    }
}
