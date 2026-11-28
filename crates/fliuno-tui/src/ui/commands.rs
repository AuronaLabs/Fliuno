use super::key_label;
use crossterm::event::KeyCode;

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
    ReadRules,
    ProjectReview,
    TogglePlan,
    Help,
}

pub(super) struct Command {
    pub label: &'static str,
    pub alias: Option<&'static str>,
    pub action: CommandAction,
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
        }
    }

    pub fn matches(&self, origin: CommandOrigin, query: &str) -> bool {
        if origin == CommandOrigin::Slash {
            return self.alias.is_some_and(|alias| alias.starts_with(query));
        }
        let label = self.label.to_ascii_lowercase();
        query.split_whitespace().all(|term| {
            label.contains(term)
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
        if self.label.len() + gap.len() + trigger.len() <= room {
            return format!(
                "{:<pad$}{gap}{trigger}",
                self.label,
                pad = room - gap.len() - trigger.len()
            );
        }
        let label_room = room.saturating_sub(gap.len() + trigger.len());
        if label_room < 18 && self.action_is_shortcut() {
            return super::ellipsis(self.label, room);
        }
        let label = super::ellipsis(self.label, label_room);
        format!("{label}{gap}{trigger}")
    }
}
