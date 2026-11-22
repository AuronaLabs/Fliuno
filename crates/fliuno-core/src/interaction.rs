use super::*;
use rusqlite::{OptionalExtension, Transaction, params};
use serde::Deserialize;
use serde_json::{Value, json};

pub(crate) const PLAN: &str = "task_publish_plan";
pub(crate) const ASK: &str = "task_ask_user";

pub(crate) fn is_internal(name: &str) -> bool {
    matches!(name, PLAN | ASK)
}

pub(crate) fn enabled(task: &Task) -> bool {
    task.context
        .as_ref()
        .is_some_and(|ctx| ctx.options.enable_interaction)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanArgs {
    steps: Vec<TaskPlanStep>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QuestionArgs {
    question: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AnswerArgs {
    answer: String,
}

fn nonblank(text: &str, maximum: usize) -> bool {
    !text.trim().is_empty() && text.len() <= maximum
}

fn parse_plan(value: &Value) -> std::result::Result<PlanArgs, String> {
    let args: PlanArgs = serde_json::from_value(value.clone()).map_err(|_| "invalid plan steps")?;
    if !(1..=12).contains(&args.steps.len())
        || args.steps.iter().any(|step| !nonblank(&step.text, 512))
    {
        return Err("plan requires 1..12 steps of 1..512 UTF-8 bytes each".into());
    }
    Ok(args)
}

fn parse_question(value: &Value) -> std::result::Result<QuestionArgs, String> {
    let args: QuestionArgs =
        serde_json::from_value(value.clone()).map_err(|_| "invalid question")?;
    if !nonblank(&args.question, 1024) {
        return Err("question requires 1..1024 UTF-8 bytes".into());
    }
    Ok(args)
}

fn parse_answer(value: &Value) -> std::result::Result<AnswerArgs, String> {
    let args: AnswerArgs = serde_json::from_value(value.clone()).map_err(|_| "invalid answer")?;
    if !nonblank(&args.answer, 4096) {
        return Err("answer requires 1..4096 UTF-8 bytes".into());
    }
    Ok(args)
}

pub(crate) struct InteractionTool(pub(crate) &'static str);

impl Tool for InteractionTool {
    fn spec(&self) -> ToolSpec {
        let (description, schema) = if self.0 == PLAN {
            (
                "Publish or update a short plan for the user. A plan does not approve future operations.",
                json!({
                    "type":"object", "required":["steps"], "additionalProperties":false,
                    "properties":{"steps":{"type":"array", "minItems":1, "maxItems":12, "items":{
                        "type":"object", "required":["text","status"], "additionalProperties":false, "properties":{
                            "text":{"type":"string","maxLength":512}, "status":{"enum":["pending","in_progress","completed"]}
                        }
                    }}}
                }),
            )
        } else {
            (
                "Ask the user one necessary clarifying question and wait for their answer before proceeding. Do not request secrets.",
                json!({
                    "type":"object", "required":["question"], "additionalProperties":false,
                    "properties":{"question":{"type":"string","maxLength":1024}}
                }),
            )
        };
        ToolSpec {
            name: self.0.into(),
            description: description.into(),
            effect: ToolEffect::ArtifactWrite,
            input_schema: schema,
        }
    }

    fn validate(&self, arguments: &Value) -> std::result::Result<(), String> {
        if self.0 == PLAN {
            parse_plan(arguments).map(|_| ())
        } else {
            parse_question(arguments).map(|_| ())
        }
    }

    fn execute(
        &self,
        _: &ToolContext<'_>,
        call: &ToolCall,
        _: &CancellationToken,
    ) -> std::result::Result<ToolOutput, ToolError> {
        if self.0 == PLAN {
            return Ok(ToolOutput {
                output: json!({"published":true}),
                artifacts: vec![],
            });
        }
        let args = parse_question(&call.arguments)?;
        Err(ToolError::InputRequired {
            requests: json!({"kind":"clarification","question":args.question}),
            request_state: None,
        })
    }

    fn validate_input_request(
        &self,
        request: &ToolInputRequest,
        state: Option<&str>,
    ) -> std::result::Result<(), String> {
        if self.0 != ASK
            || state.is_some()
            || request.round != 1
            || request.requests.get("kind").and_then(Value::as_str) != Some("clarification")
            || !request
                .requests
                .get("question")
                .and_then(Value::as_str)
                .is_some_and(|q| nonblank(q, 1024))
        {
            return Err("invalid clarification request".into());
        }
        Ok(())
    }

    fn validate_input(
        &self,
        request: &ToolInputRequest,
        responses: &Value,
    ) -> std::result::Result<(), String> {
        self.validate_input_request(request, None)?;
        parse_answer(responses).map(|_| ())
    }

    fn resume(
        &self,
        _: &ToolContext<'_>,
        _: &ToolCall,
        _: Option<&str>,
        responses: &Value,
        _: &CancellationToken,
    ) -> std::result::Result<ToolOutput, ToolError> {
        let args = parse_answer(responses)?;
        Ok(ToolOutput {
            output: json!({"answer":args.answer}),
            artifacts: vec![],
        })
    }
}

pub(crate) fn plan_event(arguments: &Value, step: u32) -> Result<EventKind> {
    let args = parse_plan(arguments).map_err(Error::Invalid)?;
    Ok(EventKind::TaskPlanUpdated {
        plan: TaskPlan {
            step,
            steps: args.steps,
        },
    })
}

pub(crate) fn migrate(db: &Connection) -> Result<()> {
    db.execute_batch("BEGIN IMMEDIATE;
        CREATE TABLE IF NOT EXISTS task_plans(task_id TEXT PRIMARY KEY REFERENCES tasks(id), data TEXT NOT NULL);
        PRAGMA user_version=71; COMMIT;")?;
    Ok(())
}

pub(crate) fn record(tx: &Transaction<'_>, task_id: &str, event: &EventKind) -> Result<()> {
    if let EventKind::TaskPlanUpdated { plan } = event {
        tx.execute("INSERT INTO task_plans VALUES(?1,?2) ON CONFLICT(task_id) DO UPDATE SET data=excluded.data",
            params![task_id, serde_json::to_string(plan)?])?;
    }
    Ok(())
}

impl Kernel {
    pub fn task_plan(&self, task_id: &str) -> Result<Option<TaskPlan>> {
        self.get_task_summary(task_id)?;
        let data = self
            .db
            .query_row(
                "SELECT data FROM task_plans WHERE task_id=?1",
                [task_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        data.map(|value| serde_json::from_str(&value).map_err(Into::into))
            .transpose()
    }
}
