use super::key_label;
use crossterm::event::KeyCode;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum CommandOrigin {
    Workbench,
    Composer,
    Slash,
}

pub(super) struct CommandMenu {
    pub index: usize,
    pub query: String,
    pub origin: CommandOrigin,
}

impl CommandMenu {
    pub fn new(origin: CommandOrigin) -> Self {
        Self {
            index: 0,
            query: String::new(),
            origin,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum CommandAction {
    Shortcut(KeyCode),
    NewConversation,
    ModelProfiles,
    SupplierSettings,
    Language,
    ReadRules,
    ProjectGuide,
    ProjectReview,
    TogglePlan,
    Help,
}

pub(super) struct Command {
    pub label: &'static str,
    pub alias: Option<&'static str>,
    pub action: CommandAction,
    pub search_label: Option<&'static str>,
}

impl Command {
    pub fn shortcut(label: &'static str, key: KeyCode) -> Self {
        let alias = match key {
            KeyCode::Char('s') => Some("resume"),
            KeyCode::F(5) => Some("status"),
            KeyCode::F(7) => Some("deliverables"),
            _ => None,
        };
        Self {
            label,
            alias,
            action: CommandAction::Shortcut(key),
            search_label: None,
        }
    }

    pub fn with_search_label(mut self, label: &'static str) -> Self {
        self.search_label = Some(label);
        self
    }

    pub fn matches(&self, origin: CommandOrigin, query: &str) -> bool {
        if origin == CommandOrigin::Slash {
            return self.alias.is_some_and(|alias| alias.starts_with(query));
        }
        let label = self.label.to_ascii_lowercase();
        let english = self.search_label.map(str::to_ascii_lowercase);
        query.split_whitespace().all(|term| {
            label.contains(term)
                || english.as_ref().is_some_and(|label| label.contains(term))
                || self
                    .alias
                    .is_some_and(|alias| alias.contains(term.trim_start_matches('/')))
        })
    }

    fn action_is_shortcut(&self) -> bool {
        matches!(self.action, CommandAction::Shortcut(_))
    }

    pub fn caption(&self, width: u16) -> String {
        let trigger = self.alias.map_or_else(
            || match self.action {
                CommandAction::Shortcut(key) => key_label(key),
                _ => String::new(),
            },
            |alias| format!("/{alias}"),
        );
        let room = usize::from(width);
        let gap = "  ";
        if trigger.is_empty() {
            return super::ellipsis(self.label, room);
        }
        let trigger_width = gap.width() + trigger.width();
        if self.label.width() + trigger_width <= room {
            let padding = " ".repeat(room - self.label.width() - trigger_width);
            return format!("{}{padding}{gap}{trigger}", self.label);
        }
        let label_room = room.saturating_sub(trigger_width);
        if label_room < 18 && self.action_is_shortcut() {
            return super::ellipsis(self.label, room);
        }
        let label = super::ellipsis(self.label, label_room);
        format!("{label}{gap}{trigger}")
    }
}
