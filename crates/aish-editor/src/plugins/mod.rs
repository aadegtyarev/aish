//! Built-in plugins. Each is an ordinary [`Plugin`](crate::Plugin); the host
//! registers the ones it wants with `Editor::builder().with(..)`.

mod history;
mod intercept;
mod pager;
mod right_hint;
mod search;
mod syntax;

pub use history::{HistorySuggest, SharedHistory};
pub use intercept::Intercept;
pub use pager::CompletionPager;
pub use right_hint::{HintText, RightHint};
pub use search::HistorySearch;
pub use syntax::SyntaxHighlight;
