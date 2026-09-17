//! Lazily initialized clipboard retained for the interactive session.

use anyhow::{Context, Result};

#[derive(Default)]
pub(crate) struct Clipboard {
    // Linux serves clipboard contents from the owning application, so keep the
    // handle alive between copies rather than dropping it after set_text.
    inner: Option<arboard::Clipboard>,
}

impl Clipboard {
    pub(crate) fn copy(&mut self, text: &str) -> Result<()> {
        let clipboard = match &mut self.inner {
            Some(clipboard) => clipboard,
            slot @ None => slot
                .insert(arboard::Clipboard::new().context("Cannot access the system clipboard")?),
        };
        clipboard
            .set_text(text)
            .context("Cannot copy the location to the clipboard")
    }
}
