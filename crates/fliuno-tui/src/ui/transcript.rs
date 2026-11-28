use super::*;
use crate::service::{TRANSCRIPT_MAX_TURNS, TRANSCRIPT_PAGE_TURNS, TranscriptPage};
use fliuno_protocol::{ReplyIdentity, SessionHistoryEntry, SessionHistoryListParams};
use std::cell::{Ref, RefCell};

use super::answer::SourcePosition;

struct Entry {
    value: SessionHistoryEntry,
    cached: RefCell<Option<Rendered>>,
}

struct Rendered {
    width: u16,
    lines: Vec<Line<'static>>,
    positions: Vec<LineStart>,
    rows: usize,
}

struct LineStart {
    source: SourcePosition,
    row: usize,
}

#[derive(Clone, Copy)]
pub(super) struct Anchor {
    sequence: u64,
    source: Option<SourcePosition>,
    // Prose uses an approximate display column within the same source line.
    wrapped_cells: usize,
}

struct TurnLayout<'a> {
    sequence: u64,
    start: usize,
    rendered: Ref<'a, Rendered>,
}

#[derive(Default)]
pub(super) struct LayoutAtWidth<'a> {
    turns: Vec<TurnLayout<'a>>,
    rows: usize,
}

impl LayoutAtWidth<'_> {
    pub(super) fn row_count(&self) -> usize {
        self.rows
    }

    pub(super) fn anchor_at(&self, row: usize) -> Option<Anchor> {
        if row >= self.rows {
            return None;
        }
        let index = self
            .turns
            .partition_point(|turn| turn.start + turn.rendered.rows <= row);
        let turn = self.turns.get(index)?;
        let line = row.checked_sub(turn.start).and_then(|offset| {
            let starts = &turn.rendered.positions;
            let index = starts.partition_point(|line| line.row <= offset);
            index.checked_sub(1).map(|index| (&starts[index], offset))
        });
        Some(Anchor {
            sequence: turn.sequence,
            source: line.map(|(line, _)| line.source),
            wrapped_cells: line.map_or(0, |(line, offset)| {
                (offset - line.row) * usize::from(turn.rendered.width)
            }),
        })
    }

    pub(super) fn resolve(&self, anchor: Anchor) -> Option<usize> {
        let index = self
            .turns
            .partition_point(|turn| turn.sequence < anchor.sequence);
        let turn = self.turns.get(index).or_else(|| self.turns.last())?;
        let row = match anchor.source {
            Some(source) if turn.sequence == anchor.sequence => {
                let starts = &turn.rendered.positions;
                let index = starts
                    .partition_point(|line| line.source <= source)
                    .saturating_sub(1);
                let row = starts[index].row;
                let end = starts
                    .get(index + 1)
                    .map_or(turn.rendered.rows, |line| line.row);
                let subrow = anchor.wrapped_cells / usize::from(turn.rendered.width.max(1));
                turn.start + (row + subrow).min(end.saturating_sub(1))
            }
            None if turn.sequence == anchor.sequence => turn.start.saturating_sub(1),
            _ => turn.start,
        };
        Some(row)
    }

    pub(super) fn visible_lines(&self, start: usize, end: usize) -> (Vec<Line<'static>>, usize) {
        let mut lines = Vec::new();
        let mut skip = 0;
        let first = self
            .turns
            .partition_point(|turn| turn.start + turn.rendered.rows <= start);
        for turn in &self.turns[first..] {
            if turn.start > start && turn.start - 1 < end {
                lines.push(Line::default());
            }
            if turn.start >= end {
                break;
            }
            let relative_start = start.saturating_sub(turn.start);
            let relative_end = end - turn.start;
            let positions = &turn.rendered.positions;
            let first_line = positions.partition_point(|line| line.row <= relative_start) - 1;
            let end_line = positions.partition_point(|line| line.row < relative_end);
            if lines.is_empty() {
                skip = relative_start - positions[first_line].row;
            }
            lines.extend_from_slice(&turn.rendered.lines[first_line..end_line]);
        }
        (lines, skip)
    }
}

impl Rendered {
    fn new(width: u16, lines: Vec<Line<'static>>, sources: Vec<SourcePosition>) -> Self {
        debug_assert_eq!(lines.len(), sources.len());
        let mut rows = 0;
        let positions = lines
            .iter()
            .zip(sources)
            .map(|(line, source)| {
                let start = LineStart { source, row: rows };
                rows += if line.width() <= usize::from(width) {
                    1
                } else {
                    Paragraph::new(line.clone())
                        .wrap(Wrap { trim: false })
                        .line_count(width)
                        .max(1)
                };
                start
            })
            .collect();
        Self {
            width,
            lines,
            positions,
            rows,
        }
    }
}

fn mark_plain(sources: &mut Vec<SourcePosition>, lines: &[Line<'static>]) {
    let count = lines.len() - sources.len();
    let next = sources.last().map_or(0, |last| last.line + 1);
    sources.extend((next..next + count).map(|line| SourcePosition { line, byte: 0 }));
}

impl From<SessionHistoryEntry> for Entry {
    fn from(value: SessionHistoryEntry) -> Self {
        Self {
            value,
            cached: RefCell::new(None),
        }
    }
}

#[derive(Default)]
pub(super) struct Transcript {
    entries: Vec<Entry>,
    oldest: Option<(u64, u64)>,
}

impl Transcript {
    pub(super) fn has_older(&self) -> bool {
        self.oldest.is_some() && self.entries.len() < TRANSCRIPT_MAX_TURNS
    }

    pub(super) fn older(&self, session_id: &str) -> Option<SessionHistoryListParams> {
        let (through, before) = self.oldest?;
        let limit = TRANSCRIPT_PAGE_TURNS.min((TRANSCRIPT_MAX_TURNS - self.entries.len()) as u32);
        (limit > 0).then(|| SessionHistoryListParams {
            session_id: session_id.to_owned(),
            through_sequence: Some(through),
            after_sequence: None,
            before_sequence: Some(before),
            limit: Some(limit),
            max_bytes: None,
        })
    }

    pub(super) fn install(&mut self, page: TranscriptPage) -> bool {
        match page {
            TranscriptPage::Recent(page) | TranscriptPage::CaughtUp(page) => {
                self.oldest = page
                    .next_before_sequence
                    .map(|before| (page.through_sequence, before));
                self.entries = page.entries.into_iter().rev().map(Entry::from).collect();
                true
            }
            TranscriptPage::Newer(page) => {
                self.entries
                    .extend(page.entries.into_iter().rev().map(Entry::from));
                let excess = self.entries.len().saturating_sub(TRANSCRIPT_MAX_TURNS);
                if excess != 0 {
                    self.entries.drain(..excess);
                    self.oldest = self
                        .entries
                        .first()
                        .map(|entry| (page.through_sequence, sequence(&entry.value)));
                }
                false
            }
            TranscriptPage::Older(page) => {
                if self
                    .oldest
                    .is_some_and(|(through, _)| through == page.through_sequence)
                {
                    self.oldest = page
                        .next_before_sequence
                        .map(|before| (page.through_sequence, before));
                    self.entries
                        .splice(..0, page.entries.into_iter().rev().map(Entry::from));
                }
                false
            }
        }
    }

    pub(super) fn layout(
        &self,
        palette: &Palette,
        width: u16,
        active: Option<&str>,
    ) -> (LayoutAtWidth<'_>, bool) {
        let mut layout = LayoutAtWidth::default();
        let mut active_rendered = false;
        for entry in &self.entries {
            match &entry.value {
                SessionHistoryEntry::Turn { turn } => {
                    active_rendered |= active == Some(&turn.task_id)
                }
                SessionHistoryEntry::Oversized { task_id, .. } if active == Some(task_id) => {
                    continue;
                }
                _ => {}
            }
            let mut cache = entry.cached.borrow_mut();
            if cache
                .as_ref()
                .is_none_or(|rendered| rendered.width != width)
            {
                *cache = Some(entry.render_at(palette, width));
            }
            drop(cache);
            let rendered = Ref::map(entry.cached.borrow(), |cache| {
                cache.as_ref().expect("turn is laid out")
            });
            if rendered.lines.is_empty() {
                continue;
            }
            let start = layout.rows + usize::from(!layout.turns.is_empty());
            layout.rows = start + rendered.rows;
            layout.turns.push(TurnLayout {
                sequence: sequence(&entry.value),
                start,
                rendered,
            });
        }
        (layout, active_rendered)
    }
}

impl Entry {
    fn render_at(&self, palette: &Palette, width: u16) -> Rendered {
        let mut lines = Vec::new();
        let mut sources = Vec::new();
        match &self.value {
            SessionHistoryEntry::Turn { turn } => {
                for message in &turn.messages {
                    match message {
                        ContextMessage::User { text } => {
                            message_lines(&mut lines, "You", text, palette.label);
                            mark_plain(&mut sources, &lines);
                        }
                        ContextMessage::Assistant { text, identity } => {
                            message_heading(
                                &mut lines,
                                reply_label(identity.as_ref(), width),
                                palette.label,
                            );
                            mark_plain(&mut sources, &lines);
                            let first_line = sources.last().map_or(0, |last| last.line + 1);
                            for fragment in answer::fragments(text, palette, width) {
                                lines.push(fragment.contents);
                                sources.push(SourcePosition {
                                    line: fragment.source.line + first_line,
                                    byte: fragment.source.byte,
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
            SessionHistoryEntry::Oversized { .. } => {
                lines.push(Line::styled(
                    "A previous turn is too large to display. Open history to review it.",
                    palette.quiet,
                ));
                mark_plain(&mut sources, &lines);
            }
        }
        Rendered::new(width, lines, sources)
    }
}

fn sequence(entry: &SessionHistoryEntry) -> u64 {
    match entry {
        SessionHistoryEntry::Turn { turn } => turn.completion_sequence,
        SessionHistoryEntry::Oversized {
            completion_sequence,
            ..
        } => *completion_sequence,
    }
}

pub(super) fn message_lines(
    lines: &mut Vec<Line<'static>>,
    name: &'static str,
    text: &str,
    label: Style,
) {
    message_heading(lines, name, label);
    append(lines, text);
}

pub(super) fn response_lines(
    lines: &mut Vec<Line<'static>>,
    name: impl Into<String>,
    text: &str,
    palette: &Palette,
    width: u16,
) {
    message_heading(lines, name, palette.label);
    lines.extend(answer::render(text, palette, width));
}

pub(super) fn reply_label(identity: Option<&ReplyIdentity>, width: u16) -> String {
    let Some(identity) = identity else {
        return "Assistant".into();
    };
    let label = clean(&format!("{}/{}", identity.supplier, identity.model)).replace('\n', " ");
    ellipsis(&label, usize::from(width))
}

fn message_heading(lines: &mut Vec<Line<'static>>, name: impl Into<String>, label: Style) {
    if !lines.is_empty() {
        lines.push(Line::default());
    }
    lines.push(Line::styled(name.into(), label));
}

#[cfg(test)]
mod tests {
    use super::*;
    use fliuno_protocol::{ContextTurn, TaskBackend};
    use ratatui::{buffer::Buffer, widgets::Widget};
    use std::time::Instant;

    #[test]
    fn long_history_viewport_matches_the_full_paragraph() {
        let palette = Palette::new(crate::options::Theme::Mono);
        let area = Rect::new(0, 0, 48, 18);
        let prose = "A long paragraph should continue on the next row without changing its source position. ";
        let code = "    let name = construct_project_filename_from_workspace_state_and_user_selection();\n";
        let history = Transcript {
            entries: (1..=TRANSCRIPT_MAX_TURNS)
                .map(|index| {
                    Entry::from(SessionHistoryEntry::Turn {
                        turn: ContextTurn {
                            task_id: index.to_string(),
                            completion_sequence: index as u64,
                            backend: TaskBackend::Model,
                            messages: vec![
                                ContextMessage::User {
                                    text: format!("Turn {index}: {}", prose.repeat(4)),
                                },
                                ContextMessage::Assistant {
                                    identity: None,
                                    text: format!("```rust\n{}```", code.repeat(24)),
                                },
                            ],
                            artifacts: Vec::new(),
                            host_tools: Vec::new(),
                        },
                    })
                })
                .collect(),
            oldest: None,
        };
        let cold = Instant::now();
        let (layout, _) = history.layout(&palette, area.width, None);
        let cold = cold.elapsed();
        let mut full = Vec::new();
        for turn in &layout.turns {
            if !full.is_empty() {
                full.push(Line::default());
            }
            full.extend_from_slice(&turn.rendered.lines);
        }
        let full = Paragraph::new(full).wrap(Wrap { trim: false });
        assert_eq!(full.line_count(area.width), layout.row_count());
        let middle = layout.turns[48].start;
        let end = layout.row_count() - usize::from(area.height);
        for start in [0, middle - 1, middle + 2, middle + 22, end] {
            let mut expected = Buffer::empty(area);
            full.clone()
                .scroll((start as u16, 0))
                .render(area, &mut expected);
            let (lines, skip) = layout.visible_lines(start, start + usize::from(area.height));
            let mut actual = Buffer::empty(area);
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((skip as u16, 0))
                .render(area, &mut actual);
            assert_eq!(actual, expected, "viewport starts at row {start}");
        }
        let code = layout.turns[48]
            .rendered
            .positions
            .iter()
            .find(|line| line.source.byte > 0)
            .unwrap();
        let start = middle + code.row;
        let anchor = layout.anchor_at(start).unwrap();
        drop(layout);
        let (wide, _) = history.layout(&palette, 110, None);
        let resolved = wide.resolve(anchor).unwrap();
        let wide_anchor = wide.anchor_at(resolved).unwrap();
        assert_eq!(wide_anchor.sequence, anchor.sequence);
        assert_eq!(
            wide_anchor.source.unwrap().line,
            anchor.source.unwrap().line
        );
        drop(wide);
        let (layout, _) = history.layout(&palette, area.width, None);
        assert_eq!(layout.resolve(anchor), Some(start));
        drop(layout);

        let warm = Instant::now();
        for _ in 0..30 {
            let (layout, _) = history.layout(&palette, area.width, None);
            let (lines, skip) = layout.visible_lines(end, end + usize::from(area.height));
            let mut buffer = Buffer::empty(area);
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((skip as u16, 0))
                .render(area, &mut buffer);
        }
        eprintln!(
            "96-turn layout: {cold:?}; mean cached viewport draw: {:?}",
            warm.elapsed() / 30
        );
    }
}
