use super::{Language, LanguageText, clean, render_list, step};
use crossterm::event::KeyCode;
use fliuno_config::ModelCatalogDraft;
use fliuno_protocol::{
    MAX_MODEL_CATALOG_PREVIEW_BYTES, ModelCatalogAccess, ModelCatalogPreview, ModelCatalogStatus,
    ProviderModel, ProviderModelPage,
};
use ratatui::{Frame, layout::Rect, style::Style};
use std::{
    collections::BTreeSet,
    io::{self, Read, Write},
    path::Path,
    process::{Child, Command, Stdio},
    sync::mpsc::{Receiver, TryRecvError, sync_channel},
    thread,
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

const MAX_PAGES: usize = 4;

#[derive(Default)]
pub(super) struct Catalog {
    lookup: Option<Lookup>,
    models: Vec<ProviderModel>,
    next_cursor: Option<String>,
    requested_cursors: BTreeSet<String>,
    status: CatalogStatus,
    index: usize,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum CatalogStatus {
    #[default]
    Ready,
    StartFailed,
    QueryTimedOut,
    QueryFailed,
    Unauthorized,
    Forbidden,
    NoEndpoint,
    TimedOut,
    Unavailable,
    Loading,
    Limit,
    Empty,
    NoMatch,
}

impl CatalogStatus {
    fn text(self, language: Language) -> &'static str {
        match self {
            Self::Ready => "",
            Self::StartFailed => language.text(
                "Could not start the model query. Enter a model ID to continue.",
                "无法启动查询。可手填模型 ID。",
            ),
            Self::QueryTimedOut => language.text(
                "Model query timed out. Enter an ID or press F5 to retry.",
                "模型查询超时。可手填 ID 或按 F5 重试。",
            ),
            Self::QueryFailed => language.text(
                "Model query failed. Enter a model ID or press F5 to retry.",
                "模型查询失败。可手填 ID 或按 F5 重试。",
            ),
            Self::Unauthorized => language.text(
                "API key rejected. Check the connection credentials.",
                "API 密钥被拒绝。请检查连接凭据。",
            ),
            Self::Forbidden => language.text(
                "Catalog access denied. Check key permissions.",
                "无法访问目录。请检查密钥权限。",
            ),
            Self::NoEndpoint => language.text(
                "No model catalog at this URL. Enter a model ID.",
                "该地址没有模型目录。请手填模型 ID。",
            ),
            Self::TimedOut => language.text(
                "Catalog timed out. Enter an ID or press F5 to retry.",
                "目录查询超时。可手填 ID 或按 F5 重试。",
            ),
            Self::Unavailable => language.text(
                "Catalog unavailable. Enter a model ID or press F5 to retry.",
                "目录不可用。可手填 ID 或按 F5 重试。",
            ),
            Self::Loading => language.text("Loading the model catalog…", "正在读取模型目录…"),
            Self::Limit => language.text(
                "Catalog limit reached. You can enter any model ID.",
                "已达目录显示上限。仍可手填模型 ID。",
            ),
            Self::Empty => language.text(
                "The catalog is empty. Enter a model ID to continue.",
                "目录为空。请手填模型 ID。",
            ),
            Self::NoMatch => language.text("No catalog match.", "没有匹配的模型。"),
        }
    }
}

impl Catalog {
    pub(super) fn loading(&self) -> bool {
        self.lookup.is_some()
    }

    pub(super) fn cursor(&self) -> Option<&str> {
        self.next_cursor.as_deref()
    }

    pub(super) fn request(
        &mut self,
        server: &Path,
        draft: ModelCatalogDraft,
        key: Zeroizing<String>,
    ) {
        if draft.cursor.is_none() {
            *self = Self::default();
        }
        let cursor = draft.cursor.clone();
        match Lookup::spawn(server, draft, key) {
            Ok(lookup) => {
                self.page_requested(cursor);
                self.lookup = Some(lookup);
            }
            Err(_) => self.status = CatalogStatus::StartFailed,
        }
    }

    pub(super) fn poll(&mut self) {
        let Some(lookup) = &self.lookup else { return };
        let preview = match lookup.result.try_recv() {
            Ok(preview) => preview,
            Err(TryRecvError::Empty) if lookup.started.elapsed() < Duration::from_secs(8) => return,
            Err(TryRecvError::Empty) => {
                self.lookup = None;
                self.status = CatalogStatus::QueryTimedOut;
                return;
            }
            Err(TryRecvError::Disconnected) => None,
        };
        self.lookup = None;
        let Some(preview) = preview else {
            self.status = CatalogStatus::QueryFailed;
            return;
        };
        self.install_preview(preview);
    }

    pub(super) fn install_preview(&mut self, preview: ModelCatalogPreview) {
        if let Some(page) = preview.page {
            self.install_page(page);
        } else {
            self.status = match (preview.catalog_access, preview.catalog) {
                (ModelCatalogAccess::Unauthorized, _) => CatalogStatus::Unauthorized,
                (ModelCatalogAccess::Forbidden, _) => CatalogStatus::Forbidden,
                (_, ModelCatalogStatus::Unavailable) => CatalogStatus::NoEndpoint,
                (_, ModelCatalogStatus::TimedOut) => CatalogStatus::TimedOut,
                _ => CatalogStatus::Unavailable,
            };
        }
    }

    pub(super) fn page_requested(&mut self, cursor: Option<String>) {
        if let Some(cursor) = cursor {
            self.requested_cursors.insert(cursor);
            self.next_cursor = None;
        } else {
            *self = Self::default();
        }
        self.status = CatalogStatus::Loading;
    }

    pub(super) fn install_page(&mut self, page: ProviderModelPage) {
        let seen: BTreeSet<_> = self.models.iter().map(|entry| entry.id.as_str()).collect();
        let new = page
            .models
            .into_iter()
            .filter(|entry| !seen.contains(entry.id.as_str()))
            .collect::<Vec<_>>();
        self.models.extend(new);
        let limit_reached = self.requested_cursors.len() + 1 >= MAX_PAGES;
        let repeated = page
            .next_cursor
            .as_ref()
            .is_some_and(|cursor| self.requested_cursors.contains(cursor));
        let missing_rows =
            page.truncated || ((limit_reached || repeated) && page.next_cursor.is_some());
        self.next_cursor = page
            .next_cursor
            .filter(|cursor| !limit_reached && !self.requested_cursors.contains(cursor));
        self.status = if missing_rows {
            CatalogStatus::Limit
        } else if self.models.is_empty() && self.next_cursor.is_none() {
            CatalogStatus::Empty
        } else {
            CatalogStatus::Ready
        };
    }

    fn matches<'a>(&'a self, text: &str) -> impl Iterator<Item = &'a ProviderModel> {
        let query = text.trim().to_lowercase();
        self.models.iter().filter(move |entry| {
            query.is_empty()
                || entry.id.to_lowercase().contains(&query)
                || entry
                    .display_name
                    .as_ref()
                    .is_some_and(|name| name.to_lowercase().contains(&query))
        })
    }

    pub(super) fn visible_rows(&self, text: &str) -> u16 {
        self.matches(text).take(7).count() as u16
    }

    pub(super) fn selected(&self, text: &str) -> Option<&str> {
        self.matches(text)
            .nth(self.index)
            .map(|entry| entry.id.as_str())
    }

    pub(super) fn reset_selection(&mut self) {
        self.index = 0;
    }

    pub(super) fn navigate(&mut self, key: KeyCode, text: &str) {
        let count = self.matches(text).count();
        step(key, &mut self.index, count);
    }

    pub(super) fn status(&self, text: &str) -> &'static str {
        self.localized_status(text, Language::English)
    }

    pub(super) fn localized_status(&self, text: &str, language: Language) -> &'static str {
        let status = if self.status == CatalogStatus::Ready
            && !self.models.is_empty()
            && self.matches(text).next().is_none()
        {
            CatalogStatus::NoMatch
        } else {
            self.status
        };
        status.text(language)
    }

    pub(super) fn draw(&self, frame: &mut Frame<'_>, area: Rect, text: &str, choice: Style) {
        if self.matches(text).next().is_none() {
            return;
        }
        let labels = self
            .matches(text)
            .map(|entry| {
                let id = clean(&entry.id);
                match &entry.display_name {
                    Some(label) if *label != entry.id => format!("{id}  ·  {}", clean(label)),
                    _ => id,
                }
            })
            .collect();
        render_list(frame, area, labels, self.index, choice);
    }
}

struct Lookup {
    child: Child,
    started: Instant,
    result: Receiver<Option<ModelCatalogPreview>>,
}

impl Lookup {
    fn spawn(server: &Path, draft: ModelCatalogDraft, key: Zeroizing<String>) -> io::Result<Self> {
        let mut command = Command::new(server);
        command
            .arg("--preview-models")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let child = command.spawn()?;
        let (out, result) = sync_channel(1);
        let mut lookup = Self {
            child,
            started: Instant::now(),
            result,
        };
        let mut stdin = lookup.child.stdin.take().expect("piped stdin");
        let stdout = lookup.child.stdout.take().expect("piped stdout");
        thread::Builder::new()
            .name("fliuno-model-catalog".into())
            .spawn(move || {
                let written = serde_json::to_writer(&mut stdin, &draft).is_ok()
                    && stdin
                        .write_all(b"\n")
                        .and_then(|()| stdin.write_all(key.as_bytes()))
                        .is_ok();
                drop(key);
                drop(stdin);
                let mut payload = Vec::new();
                let preview = if written
                    && stdout
                        .take(MAX_MODEL_CATALOG_PREVIEW_BYTES + 1)
                        .read_to_end(&mut payload)
                        .is_ok()
                    && payload.len() as u64 <= MAX_MODEL_CATALOG_PREVIEW_BYTES
                {
                    serde_json::from_slice(&payload).ok()
                } else {
                    None
                };
                let _ = out.send(preview);
            })?;
        Ok(lookup)
    }
}

impl Drop for Lookup {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
