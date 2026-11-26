//! Syntax highlighting with command resolution (PATH, builtins, aliases...).

use lineedit_wip::plugins::SyntaxHighlight;
use lineedit_wip::Plugin;

use crate::editor::resolve::{scan_path, Resolver};
use crate::module::{Env, Module};

#[derive(Default)]
pub struct HighlightModule {
    resolver: Option<Resolver>,
}

impl Module for HighlightModule {
    fn name(&self) -> &'static str {
        "highlight"
    }

    fn editor_plugins(&mut self, env: &Env) -> Vec<Box<dyn Plugin>> {
        let r = env.resolver.clone();
        self.resolver = Some(r.clone());
        let scan = r.clone();
        std::thread::spawn(move || {
            let (p, c) = scan_path(None);
            scan.set_path(p, c, false);
        });
        vec![Box::new(SyntaxHighlight::new(move |w| r.resolves(w)))]
    }

    fn on_prompt(&mut self) {
        // Cheap: compares PATH strings, rescans only when it changed.
        if let Some(r) = &self.resolver {
            r.refresh_if_stale();
        }
    }
}
