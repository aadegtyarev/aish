//! Colors and attributes. Defaults use only the 16 ANSI colors so the user's
//! terminal theme is respected.

/// SGR parameter strings (the part between `ESC[` and `m`, e.g. `"31"` or
/// `"1;36"`) for every styled element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    /// Colors and ghost text on. When false only [`selection`](Self::selection)
    /// and [`matched`](Self::matched) attributes are still used.
    pub enabled: bool,
    /// Unresolvable command word.
    pub error: String,
    /// Quoted strings.
    pub string: String,
    /// `$variables`.
    pub variable: String,
    /// Comments.
    pub comment: String,
    /// The first character of an intercepted line.
    pub accent: String,
    /// Suggestion (ghost) text.
    pub ghost: String,
    /// Selected row of a list (search, completion pager).
    pub selection: String,
    /// Matched characters in search results.
    pub matched: String,
    /// Secondary text, e.g. `… 5 more`.
    pub dim: String,
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            enabled: true,
            error: "31".into(),
            string: "33".into(),
            variable: "36".into(),
            comment: "2".into(),
            accent: "36".into(),
            ghost: "2".into(),
            selection: "7".into(),
            matched: "1".into(),
            dim: "2".into(),
        }
    }
}

impl Theme {
    /// No colors, no ghost text (reverse video / bold stay for lists).
    pub fn plain() -> Self {
        Theme {
            enabled: false,
            ..Theme::default()
        }
    }

    /// [`Theme::plain`] if `NO_COLOR` is set and non-empty, else the default.
    pub fn from_env() -> Self {
        if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
            Theme::plain()
        } else {
            Theme::default()
        }
    }
}
