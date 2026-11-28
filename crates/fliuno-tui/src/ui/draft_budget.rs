use super::*;
use fliuno_protocol::{DraftContextReport, DraftMemoryMode};

pub(super) fn summary(report: &DraftContextReport, lang: Language) -> Vec<String> {
    let mut lines = Vec::new();
    let bytes = &report.normalized_input_bytes;
    // These are independent fragments, not the encoded request or an exact token count.
    if let Some(budget) = &report.budget {
        if let Some(limit) = budget.max_input_bytes {
            lines.push(format!(
                "{} {limit} B",
                lang.text("Local input limit:", "本地输入上限：")
            ));
        } else if let Some(limit) = budget.max_input_tokens {
            lines.push(format!(
                "{} {limit} tokens",
                lang.text("Provider input limit:", "供应商输入上限：")
            ));
        }
    } else {
        lines.push(
            lang.text("No model budget configured", "未配置模型预算")
                .into(),
        );
    }
    if report.tool_count > 0 {
        lines.push(format!(
            "{} {} · {} B",
            lang.text("Tools:", "工具："),
            report.tool_count,
            bytes.tool_definitions
        ));
    }
    lines
}

pub(super) fn details(report: &DraftContextReport, lang: Language, attachment: bool) -> String {
    let mut lines = Vec::new();
    let bytes = &report.normalized_input_bytes;
    lines.push(format!(
        "{} {} B · {} {} B",
        lang.text("Draft:", "草稿："),
        bytes.current_text,
        lang.text("History:", "历史："),
        bytes.history
    ));
    if let Some(guide) = &report.project_guide {
        lines.push(format!(
            "{} {} · {} B",
            lang.text("Guide:", "指南："),
            clean(&guide.path),
            guide.bytes
        ));
    }
    if report.instructions_bytes != 0 {
        lines.push(format!(
            "{} {} B",
            lang.text("Workspace notes:", "工作区说明："),
            report.instructions_bytes
        ));
    }
    match report.memory_mode {
        DraftMemoryMode::Off => {}
        DraftMemoryMode::Explicit | DraftMemoryMode::KeywordPreview => {
            let selection = if report.memory_mode == DraftMemoryMode::Explicit {
                lang.text("Pinned memories:", "已固定的记忆：")
            } else {
                lang.text("Keyword matches:", "关键词记忆：")
            };
            lines.push(format!(
                "{selection} {} · {} B",
                report.memory_count, bytes.memory
            ));
        }
    }
    if let Some(model) = &report.model {
        lines.push(format!(
            "{}/{}",
            clean(&model.supplier),
            clean(&model.model)
        ));
    }
    if attachment {
        lines.push(
            lang.text("Attachment size is not included.", "未计入附件大小。")
                .into(),
        );
    }
    lines.push(
        lang.text(
            "Bytes by fragment; no token count yet.",
            "显示各项字节数，未计算 Token。",
        )
        .into(),
    );
    lines.join("\n")
}
