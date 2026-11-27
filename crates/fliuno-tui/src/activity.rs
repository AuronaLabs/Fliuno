use fliuno_client::Notification;
use fliuno_protocol::ModelProgress;
use std::collections::VecDeque;

const PREVIEW_TASKS: usize = 4;
const PREVIEW_BYTES: usize = 16 * 1024;

#[derive(Clone)]
pub struct Preview {
    pub task_id: String,
    pub step: u32,
    pub text: String,
    pub clipped: bool,
}

#[derive(Default)]
pub struct Progress {
    previews: VecDeque<Preview>,
    dirty: bool,
    pub gap: bool,
}

impl Progress {
    pub fn observe(&mut self, notice: Notification) {
        if notice.method == "notifications/modelProgressGap" {
            self.previews.clear();
            self.dirty = true;
            self.gap = true;
            return;
        }
        if notice.method != "notifications/modelProgress" {
            return;
        }
        let Some(payload) = notice.params.get("progress") else {
            return;
        };
        let Ok(progress) = serde_json::from_value::<ModelProgress>(payload.clone()) else {
            return;
        };
        let (task_id, step, delta) = match progress {
            ModelProgress::TextDelta {
                task_id,
                step,
                text,
            } => (task_id, step, Some(text)),
            ModelProgress::Reset { task_id, step } => (task_id, step, None),
        };
        let existing = self
            .previews
            .iter()
            .position(|preview| preview.task_id == task_id)
            .and_then(|index| self.previews.remove(index));
        let mut preview = existing.unwrap_or_else(|| Preview {
            task_id,
            step,
            text: String::new(),
            clipped: false,
        });
        if step >= preview.step {
            if step != preview.step || delta.is_none() {
                preview.step = step;
                preview.text.clear();
                preview.clipped = false;
            }
            if let Some(text) = delta {
                let length = PREVIEW_BYTES
                    .saturating_sub(preview.text.len())
                    .min(text.len());
                let mut end = length;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                preview.text.push_str(&text[..end]);
                preview.clipped |= end < text.len();
            }
            self.dirty = true;
        }
        if self.previews.len() >= PREVIEW_TASKS {
            self.previews.pop_front();
        }
        self.previews.push_back(preview);
    }

    pub fn changed(&mut self) -> Option<Vec<Preview>> {
        std::mem::take(&mut self.dirty).then(|| self.previews.iter().cloned().collect())
    }
}
