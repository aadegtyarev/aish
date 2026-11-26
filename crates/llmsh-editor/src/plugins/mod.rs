//! Built-in plugins. Each is an ordinary [`Plugin`](crate::Plugin); the host
//! registers the ones it wants with `Editor::builder().with(..)`.

mod history;
mod intercept;
mod pager;
mod search;
mod syntax;

pub use history::{HistorySuggest, SharedHistory};
pub use intercept::Intercept;
pub use pager::CompletionPager;
pub use search::HistorySearch;
pub use syntax::SyntaxHighlight;
