use fliuno_protocol::{
    ObjectPage, TaskStatus, TaskSummary, TaskSummaryListParams, TaskSummaryOrder,
};

const FILTERS: &[Option<TaskStatus>] = &[
    None,
    Some(TaskStatus::WaitingApproval),
    Some(TaskStatus::WaitingInput),
    Some(TaskStatus::Running),
    Some(TaskStatus::Interrupted),
    Some(TaskStatus::Completed),
    Some(TaskStatus::Failed),
    Some(TaskStatus::Cancelled),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageAction {
    First,
    Next,
    Previous,
    CycleFilter,
}

pub struct TaskBrowser {
    pub session: String,
    pub index: usize,
    pub filter: usize,
    pub cursors: Vec<u64>,
    pub page: Option<ObjectPage<TaskSummary>>,
}

impl TaskBrowser {
    pub fn new(session: String) -> Self {
        Self {
            session,
            index: 0,
            filter: 0,
            cursors: vec![0],
            page: None,
        }
    }

    pub fn selected(&self) -> Option<&TaskSummary> {
        self.page.as_ref()?.items.get(self.index)
    }

    pub fn query(&self, action: PageAction) -> Option<TaskSummaryListParams> {
        let (after, filter) = match action {
            PageAction::First => (0, self.filter),
            PageAction::CycleFilter => (0, (self.filter + 1) % FILTERS.len()),
            PageAction::Next => {
                let page = self.page.as_ref().filter(|p| p.has_more)?;
                (page.next_after, self.filter)
            }
            PageAction::Previous => (
                self.cursors
                    .get(self.cursors.len().checked_sub(2)?)
                    .copied()?,
                self.filter,
            ),
        };
        Some(TaskSummaryListParams {
            session_id: self.session.clone(),
            order: Some(TaskSummaryOrder::NewestFirst),
            status: FILTERS[filter],
            after,
            limit: 20,
            target_bytes: None,
        })
    }

    pub fn status(&self) -> Option<TaskStatus> {
        FILTERS[self.filter]
    }

    pub fn install(&mut self, action: PageAction, after: u64, page: ObjectPage<TaskSummary>) {
        match action {
            PageAction::CycleFilter => {
                self.filter = (self.filter + 1) % FILTERS.len();
                self.cursors.truncate(1);
            }
            PageAction::First => self.cursors.truncate(1),
            PageAction::Next => self.cursors.push(after),
            PageAction::Previous => {
                self.cursors.pop();
            }
        }
        self.index = 0;
        self.page = Some(page);
    }
}
