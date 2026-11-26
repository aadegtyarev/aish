use crate::plugin::{Ctx, Plugin, SubmitAction};

/// Lets the host claim lines: when `pred(line)` is true the line is reported
/// as `Event::Intercepted` instead of `Event::Submit`.
pub struct Intercept<F: Fn(&str) -> bool + Send>(pub F);

impl<F: Fn(&str) -> bool + Send> Intercept<F> {
    /// Intercept lines for which `pred` is true.
    pub fn new(pred: F) -> Self {
        Intercept(pred)
    }
}

impl<F: Fn(&str) -> bool + Send> Plugin for Intercept<F> {
    fn name(&self) -> &'static str {
        "intercept"
    }
    fn on_submit(&mut self, _ctx: &mut Ctx, line: &str) -> SubmitAction {
        if (self.0)(line) {
            SubmitAction::Intercept
        } else {
            SubmitAction::Submit
        }
    }
}
