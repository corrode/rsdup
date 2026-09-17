//! Semantic browser and syntax styles derived from ratatui-themes palettes.

use ratatui::style::{Color, Modifier, Style};
pub use ratatui_themes::ThemeName;

#[derive(Clone, Copy, Debug)]
pub(crate) struct DiffStyle {
    pub row: Style,
    pub gutter: Style,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Theme {
    pub base: Style,
    pub panel: Style,
    pub muted: Style,
    pub context: Style,
    pub border: Style,
    pub accent: Style,
    pub location: Style,
    pub warning: Style,
    pub error: Style,
    pub selection: Style,
    pub gutter: Style,
    pub removed: DiffStyle,
    pub added: DiffStyle,
    pub changed: DiffStyle,
    pub foreground: Color,
    pub comment: Color,
    pub keyword: Color,
    pub string: Color,
    pub number: Color,
    pub function: Color,
}

impl Theme {
    pub(crate) fn new(name: ThemeName) -> Self {
        let palette = name.palette();
        let muted = Style::default().fg(palette.muted);
        let diff = |color| {
            let bg = diff_background(palette.bg, color);
            DiffStyle {
                // Row decoration must not replace syntax foregrounds.
                row: Style::default().bg(bg),
                gutter: Style::default().bg(bg).fg(palette.muted),
            }
        };
        Self {
            base: Style::default().bg(palette.bg).fg(palette.fg),
            panel: Style::default().bg(palette.bg).fg(palette.fg),
            muted,
            context: muted.bg(palette.bg).remove_modifier(Modifier::all()),
            border: muted,
            accent: Style::default().fg(palette.accent),
            location: Style::default().fg(palette.info),
            warning: Style::default().fg(palette.warning),
            error: Style::default().fg(palette.error),
            selection: Style::default()
                .bg(palette.selection)
                .fg(palette.fg)
                .add_modifier(Modifier::BOLD),
            gutter: muted,
            removed: diff(palette.error),
            added: diff(palette.success),
            changed: diff(palette.warning),
            foreground: palette.fg,
            comment: palette.muted,
            keyword: palette.secondary,
            string: palette.success,
            number: palette.warning,
            function: palette.info,
        }
    }
}

fn diff_background(background: Color, tint: Color) -> Color {
    match (background, tint) {
        (Color::Rgb(r, g, b), Color::Rgb(tr, tg, tb)) => {
            // Keep 7/8 of the background so syntax remains legible on diff rows.
            let blend = |base: u8, tint: u8| ((u16::from(base) * 7 + u16::from(tint)) / 8) as u8;
            Color::Rgb(blend(r, tr), blend(g, tg), blend(b, tb))
        }
        // Future non-RGB palettes should retain their background, not a saturated tint.
        _ => background,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_palettes_supply_every_role() {
        assert_eq!(ThemeName::default(), ThemeName::Dracula);
        for &name in ThemeName::all() {
            let palette = name.palette();
            let theme = Theme::new(name);
            assert_eq!(name.slug().parse::<ThemeName>().unwrap(), name);
            assert_eq!(theme.base.fg, Some(palette.fg));
            assert_eq!(theme.panel.bg, Some(palette.bg));
            assert_eq!(theme.selection.fg, Some(palette.fg));
            assert_eq!(theme.selection.bg, Some(palette.selection));
            assert_eq!(theme.keyword, palette.secondary);
            assert_eq!(theme.string, palette.success);
            assert_eq!(theme.number, palette.warning);
            assert_eq!(theme.function, palette.info);
            for (style, tint) in [
                (theme.removed, palette.error),
                (theme.added, palette.success),
                (theme.changed, palette.warning),
            ] {
                assert_eq!(style.row.fg, None);
                assert_eq!(style.row.bg, Some(diff_background(palette.bg, tint)));
                assert_ne!(style.row.bg, Some(tint));
            }
        }
    }
}
