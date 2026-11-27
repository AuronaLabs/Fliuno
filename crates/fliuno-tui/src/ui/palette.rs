use crate::options::Theme;
use ratatui::style::{Color, Modifier, Style};

pub(super) struct Palette {
    pub(super) base: Style,
    pub(super) rail: Style,
    pub(super) label: Style,
    pub(super) quiet: Style,
    pub(super) choice: Style,
    status_colors: Option<[Color; 3]>,
    mono: bool,
}

impl Palette {
    pub(super) fn new(theme: Theme) -> Self {
        let bold = |style: Style| style.add_modifier(Modifier::BOLD);
        match theme {
            Theme::Dark | Theme::Light => {
                let (bg, fg, layer, accent, quiet, selected) = if matches!(theme, Theme::Dark) {
                    (
                        (17, 20, 22),
                        (228, 231, 228),
                        (32, 38, 40),
                        (132, 224, 180),
                        (166, 177, 175),
                        (37, 68, 57),
                    )
                } else {
                    (
                        (249, 250, 247),
                        (29, 37, 33),
                        (230, 236, 230),
                        (13, 100, 77),
                        (75, 93, 86),
                        (205, 229, 217),
                    )
                };
                let rgb = |(r, g, b)| Color::Rgb(r, g, b);
                let base = Style::default().fg(rgb(fg)).bg(rgb(bg));
                Self {
                    base,
                    rail: base.bg(rgb(layer)),
                    label: bold(Style::default().fg(rgb(accent))),
                    quiet: Style::default().fg(rgb(quiet)),
                    choice: bold(base.bg(rgb(selected))),
                    status_colors: Some(if matches!(theme, Theme::Dark) {
                        [
                            rgb((247, 206, 121)),
                            rgb((244, 145, 145)),
                            rgb((138, 225, 182)),
                        ]
                    } else {
                        [rgb((124, 76, 16)), rgb((153, 46, 47)), rgb((17, 100, 68))]
                    }),
                    mono: false,
                }
            }
            Theme::Auto | Theme::Mono => {
                let mono = matches!(theme, Theme::Mono);
                let base = Style::default();
                Self {
                    base,
                    rail: base,
                    label: bold(if mono { base } else { base.fg(Color::Cyan) }),
                    quiet: base.add_modifier(Modifier::DIM),
                    choice: bold(base.add_modifier(Modifier::REVERSED)),
                    status_colors: None,
                    mono,
                }
            }
        }
    }

    pub(super) fn status(&self, color: Color) -> Style {
        if self.mono {
            self.label
        } else if let Some([warning, error, success]) = self.status_colors {
            match color {
                Color::Yellow => self.label.fg(warning),
                Color::Red => self.label.fg(error),
                Color::Green => self.label.fg(success),
                Color::Gray => self.quiet,
                _ => self.label,
            }
        } else {
            self.label.fg(color)
        }
    }
}
