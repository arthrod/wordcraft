//! Which of the three ways a page runs: the host's choice, kept free of the DOM so the rule
//! is tested on the host.

/// How the page loads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The viewer only. The editor is never fetched and no edit control is offered.
    ViewOnly,
    /// The viewer paints the document; the editor is fetched on the first edit action. The
    /// default.
    ViewerFirst,
    /// The editor is fetched at once, as the page did before the viewer existed. The viewer
    /// is used only if the editor cannot start.
    EditorFirst,
}

impl Mode {
    /// The mode a host names: `view`, `viewer-first` or `edit` (and the longer spellings
    /// `view-only`, `viewer`, `editor`, `editor-first`). Case and surrounding space are
    /// ignored; anything else is `None`.
    pub fn parse(name: &str) -> Option<Mode> {
        match name.trim().to_ascii_lowercase().as_str() {
            "view" | "view-only" | "viewer" => Some(Mode::ViewOnly),
            "viewer-first" => Some(Mode::ViewerFirst),
            "edit" | "editor" | "editor-first" => Some(Mode::EditorFirst),
            _ => None,
        }
    }

    /// The name [`Mode::parse`] reads back, written to the page as `data-mode`.
    pub fn name(self) -> &'static str {
        match self {
            Mode::ViewOnly => "view",
            Mode::ViewerFirst => "viewer-first",
            Mode::EditorFirst => "edit",
        }
    }

    /// The mode of a page, from most to least specific:
    ///
    /// 1. `explicit`: the `mode` option of `start(options)`, or the mount element's
    ///    `data-mode` attribute.
    /// 2. `query_mode`: the `?mode=` URL parameter.
    /// 3. `hosted`: the URL carries `?host=`, the editor's own embedding switch. A page that
    ///    embedded the editor before the viewer existed keeps getting the editor.
    /// 4. Viewer-first.
    ///
    /// A name that does not parse is skipped, not an error: the next rule decides.
    pub fn resolve(explicit: Option<&str>, query_mode: Option<&str>, hosted: bool) -> Mode {
        explicit.and_then(Mode::parse).or_else(|| query_mode.and_then(Mode::parse)).unwrap_or(if hosted {
            Mode::EditorFirst
        } else {
            Mode::ViewerFirst
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_parse_and_read_back() {
        for mode in [Mode::ViewOnly, Mode::ViewerFirst, Mode::EditorFirst] {
            assert_eq!(Mode::parse(mode.name()), Some(mode));
        }
        assert_eq!(Mode::parse(" View-Only "), Some(Mode::ViewOnly));
        assert_eq!(Mode::parse("EDITOR-FIRST"), Some(Mode::EditorFirst));
        for junk in ["", "true", "1", "viewer first", "edit\0", "🙂"] {
            assert_eq!(Mode::parse(junk), None, "{junk:?}");
        }
    }

    #[test]
    fn the_default_is_viewer_first() {
        assert_eq!(Mode::resolve(None, None, false), Mode::ViewerFirst);
    }

    #[test]
    fn the_hosts_explicit_choice_wins_over_the_url() {
        assert_eq!(Mode::resolve(Some("view"), Some("edit"), true), Mode::ViewOnly);
        assert_eq!(Mode::resolve(Some("edit"), Some("view"), false), Mode::EditorFirst);
        assert_eq!(Mode::resolve(None, Some("view"), true), Mode::ViewOnly);
        assert_eq!(Mode::resolve(None, Some("edit"), false), Mode::EditorFirst);
    }

    #[test]
    fn an_embedding_that_asks_for_the_editors_host_bridge_keeps_the_editor() {
        assert_eq!(Mode::resolve(None, None, true), Mode::EditorFirst);
    }

    #[test]
    fn a_name_that_does_not_parse_falls_through() {
        assert_eq!(Mode::resolve(Some("fast"), Some("view"), false), Mode::ViewOnly);
        assert_eq!(Mode::resolve(Some("fast"), Some("faster"), false), Mode::ViewerFirst);
        assert_eq!(Mode::resolve(None, Some(""), true), Mode::EditorFirst);
    }
}
