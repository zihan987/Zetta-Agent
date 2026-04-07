use super::*;

pub(crate) fn default_openai_system_prompt(visible_tools: &[ToolDefinition]) -> String {
    let tool_block = if visible_tools.is_empty() {
        "No tools are currently available.".to_string()
    } else {
        visible_tools
            .iter()
            .map(|tool| {
                format!(
                    "- {} [{}]: {}",
                    tool.name,
                    tool.capability.as_str(),
                    tool.description
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let tool_names = visible_tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<std::collections::HashSet<_>>();
    let workflow_block = build_default_workflow_guidance(&tool_names);
    let example_block = build_default_tool_examples(&tool_names);

    format!(
        "You are a coding agent operating in a CLI environment.\n\
Use tools when they materially help. Prefer native tool-calling when the model/runtime supports it. If native tool-calling is unavailable, respond with exactly one line in the form `/tool <name> <payload>` and no extra text.\n\
Use JSON payloads for structured arguments.\n\
{workflow_block}\n\
When you have enough information, reply normally with the final answer instead of calling another tool.\n\
\nAvailable tools:\n{tool_block}\n\
\nRecommended call patterns:\n{example_block}"
    )
}

fn build_default_workflow_guidance(tool_names: &std::collections::HashSet<&str>) -> String {
    let mut guidance = vec![
        "Prefer the smallest useful read before acting.".to_string(),
        "Do not rewrite whole files when a localized edit is enough.".to_string(),
    ];

    if tool_names.contains("file_read_lines") && tool_names.contains("file_edit_lines") {
        guidance.push(
            "For local code changes, first inspect with `file_read_lines`, then modify with `file_edit_lines`."
                .to_string(),
        );
        guidance.push(
            "If the user explicitly asked for an edit, do not stop after inspection; make the edit once you have enough context."
                .to_string(),
        );
    }

    if tool_names.contains("glob") {
        guidance.push(
            "For repository structure, file lists, or extension-based discovery, prefer `glob` instead of `bash`."
                .to_string(),
        );
        guidance.push(
            "Keep `glob` patterns focused and set `max_results` when listing broad directories to avoid oversized outputs."
                .to_string(),
        );
    }

    if tool_names.contains("grep") {
        guidance.push(
            "For code or text search, prefer `grep` instead of shell `find`/`grep` pipelines."
                .to_string(),
        );
    }

    if tool_names.contains("grep") || tool_names.contains("glob") {
        guidance.push(
            "Use `grep` or `glob` to find the right file before opening large files blindly."
                .to_string(),
        );
    }

    if tool_names.contains("bash") {
        guidance.push(
            "Use `bash` for verification commands only when file tools are insufficient or when you need to run project commands."
                .to_string(),
        );
        guidance.push(
            "Keep `bash` calls to a single non-destructive command. Do not use shell pipelines, chaining, redirection, or `find ... | head` style constructs."
                .to_string(),
        );
        guidance.push(
            "If a `bash` call is denied, immediately rewrite the plan using `glob`, `grep`, `file_read`, or `file_read_lines` instead of retrying the same shell pattern."
                .to_string(),
        );
    }

    guidance.join(" ")
}

fn build_default_tool_examples(tool_names: &std::collections::HashSet<&str>) -> String {
    let mut examples = Vec::new();

    if tool_names.contains("file_read_lines") {
        examples.push(
            r#"- Read a focused range: /tool file_read_lines {"path":"src/main.rs","start_line":10,"end_line":40}"#
                .to_string(),
        );
    }
    if tool_names.contains("file_edit_lines") {
        examples.push(
            r#"- Replace a range: /tool file_edit_lines {"path":"src/main.rs","start_line":18,"end_line":22,"new_text":"replacement lines"}"#
                .to_string(),
        );
    }
    if tool_names.contains("file_edit") {
        examples.push(
            r#"- Replace exact text: /tool file_edit {"path":"src/main.rs","old_text":"before","new_text":"after"}"#
                .to_string(),
        );
    }
    if tool_names.contains("glob") {
        examples.push(
            r#"- List Rust files: /tool glob {"pattern":"crates/**/*.rs","max_results":40}"#
                .to_string(),
        );
    }
    if tool_names.contains("grep") {
        examples.push(
            r#"- Search by content: /tool grep {"pattern":"MyFunction","max_results":20}"#
                .to_string(),
        );
    }
    if tool_names.contains("bash") {
        examples.push(
            r#"- Run one verification command: /tool bash {"command":"cargo test"}"#.to_string(),
        );
    }

    if examples.is_empty() {
        "- No tool examples available.".to_string()
    } else {
        examples.join("\n")
    }
}
