use super::*;

#[derive(Default)]
pub(crate) struct SessionOverview {
    pub(crate) user_turns: usize,
    pub(crate) assistant_messages: usize,
    pub(crate) tool_messages: usize,
    pub(crate) completed_tools: usize,
    pub(crate) denied_tools: usize,
    pub(crate) failed_tools: usize,
    pub(crate) invalid_tool_calls: usize,
    pub(crate) tool_usage: BTreeMap<String, usize>,
}

pub(crate) fn print_session_overview(session: &zetta_protocol::SessionSnapshot) {
    for line in session_overview_lines(session) {
        println!("{line}");
    }
}

pub(crate) fn session_overview_text(session: &zetta_protocol::SessionSnapshot) -> String {
    session_overview_lines(session).join("\n")
}

fn session_overview_lines(session: &zetta_protocol::SessionSnapshot) -> Vec<String> {
    let overview = build_session_overview(session);
    let mut lines = vec![
        format!("session_id: {}", session.session_id),
        format!("updated_at: {}", session.updated_at.to_rfc3339()),
        format!("messages: {}", session.messages.len()),
        format!("user_turns: {}", overview.user_turns),
        format!("assistant_messages: {}", overview.assistant_messages),
        format!("tool_messages: {}", overview.tool_messages),
        format!("tool_completed: {}", overview.completed_tools),
        format!("tool_denied: {}", overview.denied_tools),
        format!("tool_failed: {}", overview.failed_tools),
        format!("tool_invalid: {}", overview.invalid_tool_calls),
    ];
    if !overview.tool_usage.is_empty() {
        lines.push(format!(
            "tool_usage: {}",
            overview
                .tool_usage
                .iter()
                .map(|(name, count)| format!("{name}={count}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !session.tags.is_empty() {
        lines.push(format!("tags: {}", session.tags.join(", ")));
    }
    if !session.metadata.is_empty() {
        lines.push(format!(
            "metadata: {}",
            session
                .metadata
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    lines
}

pub(crate) fn build_session_overview(session: &zetta_protocol::SessionSnapshot) -> SessionOverview {
    let mut overview = SessionOverview::default();

    for message in &session.messages {
        match message.role {
            zetta_protocol::MessageRole::User => overview.user_turns += 1,
            zetta_protocol::MessageRole::Assistant => overview.assistant_messages += 1,
            zetta_protocol::MessageRole::Tool => {
                overview.tool_messages += 1;
                if let Some((tool_name, status)) = parse_tool_message_metadata(&message.content) {
                    *overview.tool_usage.entry(tool_name).or_insert(0) += 1;
                    match status.as_str() {
                        "completed" => overview.completed_tools += 1,
                        "denied" => overview.denied_tools += 1,
                        "failed" => overview.failed_tools += 1,
                        "invalid_call" => overview.invalid_tool_calls += 1,
                        _ => {}
                    }
                }
            }
            zetta_protocol::MessageRole::System => {}
        }
    }

    overview
}

fn parse_tool_message_metadata(content: &str) -> Option<(String, String)> {
    let value = serde_json::from_str::<Value>(content).ok()?;
    let object = value.as_object()?;
    if object.get("type")?.as_str()? != "tool_result" {
        return None;
    }
    Some((
        object.get("tool_name")?.as_str()?.to_string(),
        object
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("completed")
            .to_string(),
    ))
}
