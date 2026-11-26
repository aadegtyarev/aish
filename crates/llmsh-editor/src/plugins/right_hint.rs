use std::sync::{Arc, Mutex};

use crate::plugin::{Ctx, Decoration, Handle, Plugin};

#[derive(Default)]
struct Inner {
    text: String,
    /// Set once the hint has been drawn: how to ask for a redraw.
    handle: Option<Handle>,
}

/// Text shown by [`RightHint`]; the host keeps a clone and changes it any
/// time from any thread: while the editor is showing the line, the new text
/// is drawn right away (the host is woken like for any late plugin result).
#[derive(Clone, Default)]
pub struct HintText(Arc<Mutex<Inner>>);

impl HintText {
    /// A hint showing `text` (empty = nothing).
    pub fn new(text: &str) -> Self {
        HintText(Arc::new(Mutex::new(Inner {
            text: text.to_owned(),
            handle: None,
        })))
    }

    /// Change the text; triggers a redraw if it differs from the current one.
    pub fn set(&self, text: &str) {
        let handle = {
            let Ok(mut g) = self.0.lock() else { return };
            if g.text == text {
                return;
            }
            g.text = text.to_owned();
            g.handle.clone()
        };
        // outside the lock: the waker may do anything quick
        if let Some(h) = handle {
            h.deliver_any(Box::new(()));
        }
    }

    /// The current text.
    pub fn get(&self) -> String {
        self.0.lock().map(|g| g.text.clone()).unwrap_or_default()
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
    fn decorate(&mut self, ctx: &mut Ctx, _width: u16, out: &mut Decoration) {
        if let Ok(mut g) = (self.0).0.lock() {
            if g.handle.is_none() {
                g.handle = Some(ctx.handle());
            }
            if !g.text.is_empty() {
                out.right_hint(&g.text);
            }
        }
    }
}
