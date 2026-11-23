use crossterm::event::KeyCode;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Default)]
pub struct Editor {
    text: String,
    cursor: usize,
}

impl Editor {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    #[must_use]
    pub fn insert(&mut self, value: &str, limit: usize) -> bool {
        let remaining = limit.saturating_sub(self.text.len());
        let end = value
            .grapheme_indices(true)
            .map(|(i, grapheme)| i + grapheme.len())
            .take_while(|end| *end <= remaining)
            .last()
            .unwrap_or(0);
        if end == 0 {
            return false;
        }
        self.text.insert_str(self.cursor, &value[..end]);
        // Inserting can join clusters on either side of the cursor.
        self.cursor = self.at_or_after(self.cursor + end);
        true
    }

    pub fn key(&mut self, key: KeyCode) -> bool {
        match key {
            KeyCode::Left => self.cursor = self.previous(),
            KeyCode::Right => self.cursor = self.next(),
            KeyCode::Home => {
                self.cursor = self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
            }
            KeyCode::End => self.cursor = self.line_end(self.cursor),
            KeyCode::Up | KeyCode::Down => self.move_line(key == KeyCode::Down),
            KeyCode::Backspace | KeyCode::Delete => {
                let (start, end) = if key == KeyCode::Backspace {
                    (self.previous(), self.cursor)
                } else {
                    (self.cursor, self.next())
                };
                if start != end {
                    self.text.replace_range(start..end, "");
                    self.cursor = self.at_or_after(start);
                    return true;
                }
            }
            _ => {}
        }
        false
    }

    fn at_or_after(&self, target: usize) -> usize {
        self.text
            .grapheme_indices(true)
            .find_map(|(i, _)| (i >= target).then_some(i))
            .unwrap_or(self.text.len())
    }

    fn previous(&self) -> usize {
        self.text[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    fn next(&self) -> usize {
        self.cursor
            + self.text[self.cursor..]
                .graphemes(true)
                .next()
                .map_or(0, str::len)
    }

    fn line_end(&self, from: usize) -> usize {
        self.text[from..]
            .find('\n')
            .map_or(self.text.len(), |i| from + i)
    }

    fn move_line(&mut self, forward: bool) {
        let current = self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
        let column = self.text[current..self.cursor].width();
        let target = if forward {
            let end = self.line_end(self.cursor);
            if end == self.text.len() {
                return;
            }
            end + 1
        } else if current > 0 {
            self.text[..current - 1].rfind('\n').map_or(0, |i| i + 1)
        } else {
            return;
        };
        let mut width = 0;
        self.cursor = target;
        for cluster in self.text[target..self.line_end(target)].graphemes(true) {
            width += cluster.width();
            if width > column {
                break;
            }
            self.cursor += cluster.len();
        }
    }

    pub fn viewport(&self, width: u16, height: u16) -> (Vec<String>, u16, u16) {
        let width = usize::from(width.max(1));
        let mut lines = vec![String::new()];
        let mut column = 0;
        let mut caret = (0, 0);
        for (i, cluster) in self.text.grapheme_indices(true) {
            let newline = cluster == "\n";
            let cells = cluster.width().max(1);
            if !newline && column > 0 && column + cells > width {
                lines.push(String::new());
                column = 0;
            }
            if i == self.cursor {
                caret = (column.min(width - 1), lines.len() - 1);
            }
            if newline {
                lines.push(String::new());
                column = 0;
            } else {
                lines
                    .last_mut()
                    .expect("editor line")
                    .push_str(if cells > width { "?" } else { cluster });
                column += cells.min(width);
            }
        }
        if self.cursor == self.text.len() {
            if column >= width {
                lines.push(String::new());
                column = 0;
            }
            caret = (column, lines.len() - 1);
        }
        let height = usize::from(height.max(1));
        let top = (caret.1 + 1).saturating_sub(height);
        (
            lines.into_iter().skip(top).take(height).collect(),
            caret.0 as u16,
            (caret.1 - top) as u16,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_clusters_keeps_utf8_and_cursor_valid() {
        let mut editor = Editor::default();
        assert!(editor.insert("中e\u{301}尾", 20));
        editor.key(KeyCode::Left);
        assert!(editor.key(KeyCode::Backspace));
        assert!(editor.insert("文", 20));
        assert_eq!(editor.text(), "中文尾");
        assert!(editor.key(KeyCode::Delete));
        assert_eq!(
            editor.viewport(4, 2),
            (vec!["中文".into(), "".into()], 0, 1)
        );
        assert!(!editor.insert("e\u{301}", 7));
    }
}
