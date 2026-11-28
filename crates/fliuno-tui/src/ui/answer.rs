use super::{Line, Palette, Span, Style, UnicodeSegmentation, UnicodeWidthStr, clean, ellipsis};
use pulldown_cmark::{CodeBlockKind, Event, Parser, Tag, TagEnd};
use ratatui::style::Modifier;

const SOURCE_LINE_LIMIT: usize = 500;

#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct SourcePosition {
    pub line: usize,
    pub byte: usize,
}

pub(super) struct Fragment {
    pub contents: Line<'static>,
    pub source: SourcePosition,
}

pub(super) fn render(text: &str, palette: &Palette, width: u16) -> Vec<Line<'static>> {
    fragments(text, palette, width)
        .into_iter()
        .map(|fragment| fragment.contents)
        .collect()
}

pub(super) fn fragments(text: &str, palette: &Palette, width: u16) -> Vec<Fragment> {
    let end = text
        .match_indices('\n')
        .nth(SOURCE_LINE_LIMIT - 1)
        .map(|(end, _)| end);
    let mut answer = Answer::new(palette, usize::from(width));
    // Sanitize each emitted span after parsing. Entity escapes can also produce controls.
    for event in Parser::new(&text[..end.unwrap_or(text.len())]) {
        answer.push(event);
    }
    answer.flush();
    while answer
        .lines
        .last()
        .is_some_and(|line| line.contents.width() == 0)
    {
        answer.lines.pop();
    }
    if end.is_some_and(|offset| offset + 1 < text.len()) {
        answer.line(Line::styled("Reply continues in history", palette.quiet));
    }
    answer.lines
}

struct Answer<'a> {
    lines: Vec<Fragment>,
    source_line: usize,
    spans: Vec<Span<'static>>,
    lists: Vec<Option<u64>>,
    code: Option<String>,
    emphasis: usize,
    strong: usize,
    heading: bool,
    palette: &'a Palette,
    width: usize,
}

impl<'a> Answer<'a> {
    fn new(palette: &'a Palette, width: usize) -> Self {
        Self {
            lines: Vec::new(),
            source_line: 0,
            spans: Vec::new(),
            lists: Vec::new(),
            code: None,
            emphasis: 0,
            strong: 0,
            heading: false,
            palette,
            width,
        }
    }

    fn line(&mut self, contents: Line<'static>) {
        self.lines.push(Fragment {
            contents,
            source: SourcePosition {
                line: self.source_line,
                byte: 0,
            },
        });
        self.source_line += 1;
    }

    fn flush(&mut self) {
        if !self.spans.is_empty() {
            let spans = std::mem::take(&mut self.spans);
            self.line(Line::from(spans));
        }
    }

    fn separate(&mut self) {
        self.flush();
        if self
            .lines
            .last()
            .is_some_and(|line| line.contents.width() > 0)
        {
            self.line(Line::default());
        }
    }

    fn style(&self) -> Style {
        let mut style = if self.heading {
            self.palette.label
        } else {
            Style::default()
        };
        if self.strong > 0 {
            style = style.add_modifier(Modifier::BOLD);
        }
        if self.emphasis > 0 {
            style = style.add_modifier(Modifier::ITALIC);
        }
        style
    }

    fn text(&mut self, text: &str, style: Style) {
        for (index, part) in clean(text).split('\n').enumerate() {
            if index > 0 {
                self.flush();
            }
            if !part.is_empty() {
                self.spans.push(Span::styled(part.to_owned(), style));
            }
        }
    }

    fn push(&mut self, event: Event<'_>) {
        if let Some(code) = &mut self.code {
            match event {
                Event::Text(text) => code.push_str(&text),
                Event::End(TagEnd::CodeBlock) => {
                    let code = self.code.take().expect("open code block");
                    self.finish_code(&code);
                }
                _ => {}
            }
            return;
        }
        match event {
            Event::Start(Tag::Heading { .. }) => {
                self.separate();
                self.heading = true;
            }
            Event::End(TagEnd::Heading(_)) => {
                self.heading = false;
                self.separate();
            }
            Event::Start(Tag::List(start)) => self.lists.push(start),
            Event::End(TagEnd::List(_)) => {
                self.lists.pop();
                if self.lists.is_empty() {
                    self.separate();
                }
            }
            Event::Start(Tag::Item) => {
                self.flush();
                let indent = "  ".repeat(self.lists.len().saturating_sub(1).min(4));
                let suffix = if let Some(Some(number)) = self.lists.last_mut() {
                    let label = format!("{number}. ");
                    *number = number.saturating_add(1);
                    label
                } else {
                    "• ".to_owned()
                };
                self.spans.push(Span::styled(
                    format!("{indent}{suffix}"),
                    self.palette.quiet,
                ));
            }
            Event::End(TagEnd::Item) => self.flush(),
            Event::End(TagEnd::Paragraph) if self.lists.is_empty() => self.separate(),
            Event::End(TagEnd::Paragraph) => self.flush(),
            Event::Start(Tag::Strong) => self.strong += 1,
            Event::End(TagEnd::Strong) => self.strong = self.strong.saturating_sub(1),
            Event::Start(Tag::Emphasis) => self.emphasis += 1,
            Event::End(TagEnd::Emphasis) => self.emphasis = self.emphasis.saturating_sub(1),
            Event::Start(Tag::CodeBlock(kind)) => {
                self.separate();
                let language = match &kind {
                    CodeBlockKind::Fenced(info) => info.split_whitespace().next().unwrap_or("Code"),
                    CodeBlockKind::Indented => "Code",
                };
                self.line(Line::styled(
                    ellipsis(&clean(language), self.width),
                    self.palette.label,
                ));
                self.code = Some(String::new());
            }
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                self.text(&text, self.style())
            }
            Event::Code(text) => self.text(&text, self.palette.rail.patch(self.palette.label)),
            Event::SoftBreak | Event::HardBreak => self.flush(),
            Event::Rule => self.separate(),
            _ => {}
        }
    }

    fn finish_code(&mut self, code: &str) {
        let prefix = if self.width >= 4 { "│ " } else { "" };
        let available = self.width.saturating_sub(prefix.width()).max(1);
        for source in clean(code).split_terminator('\n') {
            let mut cells = 0;
            let mut start = 0;
            for (offset, grapheme) in source.grapheme_indices(true) {
                let next = grapheme.width();
                if cells > 0 && cells + next > available {
                    self.code_line(prefix, &source[start..offset], start);
                    start = offset;
                    cells = 0;
                }
                cells += next;
            }
            self.code_line(prefix, &source[start..], start);
            self.source_line += 1;
        }
        self.separate();
    }

    fn code_line(&mut self, prefix: &str, source: &str, byte: usize) {
        self.lines.push(Fragment {
            contents: Line::from(vec![
                Span::styled(prefix.to_owned(), self.palette.quiet),
                Span::styled(source.to_owned(), self.palette.rail),
            ]),
            source: SourcePosition {
                line: self.source_line,
                byte,
            },
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::Theme;

    #[test]
    fn partial_fences_wrap_wide_code_and_preserve_indentation() {
        let palette = Palette::new(Theme::Mono);
        let body = "Use `std::fs`:\n\n2. List item\n\n```rust\n    文件名和长变量";
        let partial = render(body, &palette, 12);
        let complete = render(&format!("{body}\n```"), &palette, 12);
        assert_eq!(partial, complete);
        let plain: Vec<_> = complete.iter().map(ToString::to_string).collect();
        assert!(plain.contains(&"Use std::fs:".to_owned()));
        assert!(plain.contains(&"2. List item".to_owned()));
        assert!(plain.contains(&"rust".to_owned()));
        let code: String = plain
            .iter()
            .filter_map(|line| line.strip_prefix("│ "))
            .collect();
        assert_eq!(code, "    文件名和长变量");
        assert!(complete.iter().all(|line| line.width() <= 12));
    }

    #[test]
    fn terminal_controls_are_removed_from_text_and_code() {
        let palette = Palette::new(Theme::Dark);
        let body = "before&#27; and \u{202e}after\n\n```\nprintln!(\"\u{1b}[31m\");\n```";
        for line in render(body, &palette, 80) {
            assert!(!line.to_string().contains(['\u{1b}', '\u{202e}']));
        }
    }
}
