use std::sync::{Arc, Mutex};

use crate::plugin::{Ctx, Decoration, Plugin};

/// Text shown by [`RightHint`]; the host keeps a clone and changes it any
/// time (the new text appears with the next redraw).
#[derive(Clone, Default)]
pub struct HintText(Arc<Mutex<String>>);

impl HintText {
    /// A hint showing `text` (empty = nothing).
    pub fn new(text: &str) -> Self {
        HintText(Arc::new(Mutex::new(text.to_owned())))
    }
    /// Change the text.
    pub fn set(&self, text: &str) {
        if let Ok(mut g) = self.0.lock() {
            *g = text.to_owned();
        }
    }
}

/// A quiet marker at the right edge of the input row (see
/// [`Decoration::right_hint`]): hidden when the input gets close, when it
/// wraps, and with colors off; never part of the finished line.
pub struct RightHint(HintText);

impl RightHint {
    /// Show whatever `text` currently holds.
    pub fn new(text: HintText) -> Self {
        RightHint(text)
    }
}

impl Plugin for RightHint {
    fn name(&self) -> &'static str {
        "right-hint"
    }
    fn decorate(&mut self, _ctx: &mut Ctx, _width: u16, out: &mut Decoration) {
        if let Ok(g) = (self.0).0.lock() {
            if !g.is_empty() {
                out.right_hint(&g);
            }
        }
    }
}
