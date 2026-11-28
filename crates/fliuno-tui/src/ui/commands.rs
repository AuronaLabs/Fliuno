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
    ReadRules,
    ProjectReview,
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

    pub fn caption(&self) -> String {
        let trigger = self.alias.map_or_else(
            || match self.action {
                CommandAction::Shortcut(key) => key_label(key),
                _ => String::new(),
            },
            |alias| format!("/{alias}"),
        );
        format!("{trigger:<15}{}", self.label)
    }
}
