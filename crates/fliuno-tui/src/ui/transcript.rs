use super::*;
use crate::service::{TRANSCRIPT_MAX_TURNS, TRANSCRIPT_PAGE_TURNS, TranscriptPage};
use fliuno_protocol::{SessionHistoryEntry, SessionHistoryListParams};
use std::cell::RefCell;

struct Entry {
    value: SessionHistoryEntry,
    cached: RefCell<Option<Rendered>>,
}

struct Rendered {
    width: u16,
    lines: Vec<Line<'static>>,
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
    prepended: Cell<usize>,
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
                self.prepended.set(0);
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
                    self.prepended.set(page.entries.len());
                    self.entries
                        .splice(..0, page.entries.into_iter().rev().map(Entry::from));
                }
                false
            }
        }
    }

    pub(super) fn render(
        &self,
        palette: &Palette,
        width: u16,
        active: Option<&str>,
    ) -> (Vec<Line<'static>>, usize, bool) {
        let prepended = self.prepended.replace(0);
        let mut prefix_lines = 0;
        let mut active_rendered = false;
        let mut lines = Vec::new();
        for (index, entry) in self.entries.iter().enumerate() {
            match &entry.value {
                SessionHistoryEntry::Turn { turn } => {
                    active_rendered |= active == Some(&turn.task_id);
                    let mut cached = entry.cached.borrow_mut();
                    if cached.as_ref().is_none_or(|entry| entry.width != width) {
                        let mut rendered = Vec::new();
                        for message in &turn.messages {
                            match message {
                                ContextMessage::User { text } => {
                                    message_lines(&mut rendered, "You", text, palette.label);
                                }
                                ContextMessage::Assistant { text } => {
                                    response_lines(&mut rendered, "Fliuno", text, palette, width);
                                }
                                _ => {}
                            }
                        }
                        *cached = Some(Rendered {
                            width,
                            lines: rendered,
                        });
                    }
                    if let Some(rendered) = &*cached
                        && !rendered.lines.is_empty()
                    {
                        if !lines.is_empty() {
                            lines.push(Line::default());
                        }
                        lines.extend(rendered.lines.iter().cloned());
                    }
                }
                SessionHistoryEntry::Oversized { task_id, .. } if active != Some(task_id) => {
                    if !lines.is_empty() {
                        lines.push(Line::raw(""));
                    }
                    lines.push(Line::styled(
                        "A previous turn is too large to display. Open history to review it.",
                        palette.quiet,
                    ));
                }
                _ => {}
            }
            if index < prepended {
                prefix_lines = lines.len();
            }
        }
        (lines, prefix_lines, active_rendered)
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
    name: &'static str,
    text: &str,
    palette: &Palette,
    width: u16,
) {
    message_heading(lines, name, palette.label);
    lines.extend(answer::render(text, palette, width));
}

fn message_heading(lines: &mut Vec<Line<'static>>, name: &'static str, label: Style) {
    if !lines.is_empty() {
        lines.push(Line::default());
    }
    lines.push(Line::styled(name, label));
}
