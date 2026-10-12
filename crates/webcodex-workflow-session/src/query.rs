//! Read-only session aggregates: message summary, discussion, inbox hints.
use super::model::{
    SessionDiscussionCounts, SessionDiscussionSummary, SessionInboxHint, SessionInboxOpenCounts,
    SessionMessage, SessionMessageCompletionSummary, SessionMessageError, SessionMessageKind,
    SessionMessagePriority, SessionMessageStatus, SessionMessagesSummary, SessionRecord,
    MAX_MESSAGE_CHARS, MAX_MESSAGE_RESOLUTION_CHARS, MAX_MESSAGE_SUMMARY_CHARS, MAX_MESSAGE_TAGS,
    MAX_MESSAGE_TAG_CHARS, SESSION_INBOX_ACK_REQUIRED_ATTENTION_INSTRUCTION,
    SESSION_INBOX_ACK_REQUIRED_ATTENTION_REASON, SUMMARY_MESSAGE_GROUP_LIMIT,
};
use super::util::bound_chars;

pub(super) fn build_messages_summary(record: &SessionRecord) -> SessionMessagesSummary {
    let counts = build_discussion_counts(record);
    SessionMessagesSummary {
        total: counts.total,
        open: counts.open,
        resolved: counts.resolved,
        pending_guidance: counts.open_guidance,
        open_questions: counts.open_questions,
        open_risks: counts.open_risks,
        open_todos: counts.open_todos,
        recent_progress: take_recent_kind(
            record,
            SessionMessageKind::Progress,
            None,
            SUMMARY_MESSAGE_GROUP_LIMIT,
        ),
        guidance: counts.guidance,
        progress: counts.progress,
        risk: counts.risk,
        todo: counts.todo,
        question: counts.question,
        decision: counts.decision,
    }
}

pub(super) fn build_discussion_counts(record: &SessionRecord) -> SessionDiscussionCounts {
    let mut counts = SessionDiscussionCounts {
        total: record.messages.len(),
        open: 0,
        resolved: 0,
        guidance: 0,
        progress: 0,
        risk: 0,
        todo: 0,
        question: 0,
        answer: 0,
        decision: 0,
        open_guidance: 0,
        open_questions: 0,
        open_risks: 0,
        open_todos: 0,
    };
    // One pass for both summary and discussion projections. Retain the original
    // unknown-kind behavior and count every non-open status as resolved.
    for message in &record.messages {
        let open = message.status == SessionMessageStatus::Open;
        counts.open += usize::from(open);
        match message.kind {
            SessionMessageKind::Guidance => {
                counts.guidance += 1;
                counts.open_guidance += usize::from(open);
            }
            SessionMessageKind::Progress => counts.progress += 1,
            SessionMessageKind::Risk => {
                counts.risk += 1;
                counts.open_risks += usize::from(open);
            }
            SessionMessageKind::Todo => {
                counts.todo += 1;
                counts.open_todos += usize::from(open);
            }
            SessionMessageKind::Question => {
                counts.question += 1;
                counts.open_questions += usize::from(open);
            }
            SessionMessageKind::Answer => counts.answer += 1,
            SessionMessageKind::Decision => counts.decision += 1,
            SessionMessageKind::Note | SessionMessageKind::Proposal => {}
        }
    }
    counts.resolved = counts.total.saturating_sub(counts.open);
    counts
}

pub(super) fn build_discussion_summary(
    record: &SessionRecord,
    limit: usize,
) -> SessionDiscussionSummary {
    SessionDiscussionSummary {
        counts: build_discussion_counts(record),
        open_guidance: take_recent_kind(
            record,
            SessionMessageKind::Guidance,
            Some(SessionMessageStatus::Open),
            limit.min(SUMMARY_MESSAGE_GROUP_LIMIT),
        ),
        open_questions: take_recent_kind(
            record,
            SessionMessageKind::Question,
            Some(SessionMessageStatus::Open),
            limit.min(SUMMARY_MESSAGE_GROUP_LIMIT),
        ),
        open_risks: take_recent_kind(
            record,
            SessionMessageKind::Risk,
            Some(SessionMessageStatus::Open),
            limit.min(SUMMARY_MESSAGE_GROUP_LIMIT),
        ),
        open_todos: take_recent_kind(
            record,
            SessionMessageKind::Todo,
            Some(SessionMessageStatus::Open),
            limit.min(SUMMARY_MESSAGE_GROUP_LIMIT),
        ),
        high_priority_open_todos: take_recent_messages(
            record,
            limit.min(SUMMARY_MESSAGE_GROUP_LIMIT),
            |message| {
                message.kind == SessionMessageKind::Todo
                    && message.status == SessionMessageStatus::Open
                    && message.priority == SessionMessagePriority::High
            },
        ),
        recent_answers: take_recent_kind(
            record,
            SessionMessageKind::Answer,
            None,
            limit.min(SUMMARY_MESSAGE_GROUP_LIMIT),
        ),
        recent_completions: take_recent_completions(record, limit.min(SUMMARY_MESSAGE_GROUP_LIMIT)),
        recent_progress: take_recent_kind(record, SessionMessageKind::Progress, None, limit),
        recent_decisions: take_recent_kind(record, SessionMessageKind::Decision, None, limit),
    }
}

pub(super) fn build_inbox_hint(record: &SessionRecord) -> Option<SessionInboxHint> {
    let mut counts = SessionInboxOpenCounts::default();
    let mut highest_priority = None;
    let mut ack_required_message = false;

    for message in record
        .messages
        .iter()
        .filter(|message| message.status == SessionMessageStatus::Open)
    {
        ack_required_message |= message.requires_ack;
        let counted = match message.kind {
            SessionMessageKind::Guidance => {
                counts.guidance += 1;
                true
            }
            SessionMessageKind::Question => {
                counts.question += 1;
                true
            }
            SessionMessageKind::Todo => {
                counts.todo += 1;
                true
            }
            SessionMessageKind::Risk => {
                counts.risk += 1;
                true
            }
            _ => false,
        };
        if (counted || message.requires_ack)
            && highest_priority
                .is_none_or(|priority| priority_rank(message.priority) > priority_rank(priority))
        {
            highest_priority = Some(message.priority);
        }
    }

    highest_priority.map(|priority| SessionInboxHint {
        has_open_messages: true,
        open_counts: counts,
        highest_priority: priority,
        attention_required: ack_required_message.then_some(true),
        attention_reason: ack_required_message
            .then_some(SESSION_INBOX_ACK_REQUIRED_ATTENTION_REASON),
        attention_instruction: ack_required_message
            .then_some(SESSION_INBOX_ACK_REQUIRED_ATTENTION_INSTRUCTION),
        suggested_next_tool: "read_session_discussion_summary",
    })
}

pub(super) fn priority_rank(priority: SessionMessagePriority) -> u8 {
    match priority {
        SessionMessagePriority::Low => 0,
        SessionMessagePriority::Normal => 1,
        SessionMessagePriority::High => 2,
    }
}

fn take_recent_messages(
    record: &SessionRecord,
    limit: usize,
    predicate: impl Fn(&SessionMessage) -> bool,
) -> Vec<SessionMessage> {
    record
        .messages
        .iter()
        .rev()
        .filter(|message| predicate(message))
        .take(limit)
        .map(|message| message.as_ref().clone())
        .map(bound_message_for_summary)
        .collect()
}

fn take_recent_completions(
    record: &SessionRecord,
    limit: usize,
) -> Vec<SessionMessageCompletionSummary> {
    record
        .messages
        .iter()
        .rev()
        .filter(|message| {
            message.kind == SessionMessageKind::Todo
                && message.status == SessionMessageStatus::Resolved
                && message.resolved_by_message_id.is_some()
        })
        .filter_map(|todo| {
            let answer_message_id = todo.resolved_by_message_id.as_ref()?;
            let answer = record.messages.iter().find(|message| {
                message.message_id == *answer_message_id
                    && message.kind == SessionMessageKind::Answer
                    && message.reply_to.as_deref() == Some(todo.message_id.as_str())
            })?;
            Some(SessionMessageCompletionSummary {
                todo_message_id: todo.message_id.clone(),
                answer_message_id: answer.message_id.clone(),
                author_session_id: answer.author_session_id.clone(),
                completed_at: todo.resolved_at,
            })
        })
        .take(limit)
        .collect()
}

pub(super) fn take_recent_kind(
    record: &SessionRecord,
    kind: SessionMessageKind,
    status: Option<SessionMessageStatus>,
    limit: usize,
) -> Vec<SessionMessage> {
    record
        .messages
        .iter()
        .rev()
        .filter(|message| message.kind == kind)
        .filter(|message| status.is_none_or(|status| message.status == status))
        .take(limit)
        .map(|message| message.as_ref().clone())
        .map(bound_message_for_summary)
        .collect()
}

pub(super) fn bound_message_for_summary(mut message: SessionMessage) -> SessionMessage {
    message.message = bound_chars(&message.message, MAX_MESSAGE_SUMMARY_CHARS);
    if let Some(resolution) = message.resolution.as_mut() {
        *resolution = bound_chars(resolution, MAX_MESSAGE_SUMMARY_CHARS);
    }
    message
}

pub(super) fn is_valid_completion_id(value: &str) -> bool {
    value.len() == super::model::MESSAGE_COMPLETION_FINGERPRINT_HEX_CHARS
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(super) fn validate_message_text(value: String) -> Result<String, SessionMessageError> {
    let value = value.trim().to_string();
    if value.is_empty() {
        return Err(SessionMessageError::InvalidInput(
            "message must not be empty".to_string(),
        ));
    }
    if value.chars().count() > MAX_MESSAGE_CHARS {
        return Err(SessionMessageError::InvalidInput(format!(
            "message exceeds {MAX_MESSAGE_CHARS} chars"
        )));
    }
    Ok(value)
}

pub(super) fn validate_resolution_text(value: String) -> Result<String, SessionMessageError> {
    let value = value.trim().to_string();
    if value.chars().count() > MAX_MESSAGE_RESOLUTION_CHARS {
        return Err(SessionMessageError::InvalidInput(format!(
            "resolution exceeds {MAX_MESSAGE_RESOLUTION_CHARS} chars"
        )));
    }
    Ok(value)
}

pub(super) fn validate_message_tags(
    values: Vec<String>,
) -> Result<Vec<String>, SessionMessageError> {
    if values.len() > MAX_MESSAGE_TAGS {
        return Err(SessionMessageError::InvalidInput(format!(
            "tags exceed {MAX_MESSAGE_TAGS} items"
        )));
    }
    let mut tags = Vec::new();
    for value in values {
        let value = value.trim().to_string();
        if value.is_empty() {
            continue;
        }
        if value.chars().count() > MAX_MESSAGE_TAG_CHARS {
            return Err(SessionMessageError::InvalidInput(format!(
                "tag exceeds {MAX_MESSAGE_TAG_CHARS} chars"
            )));
        }
        if !tags.iter().any(|tag| tag == &value) {
            tags.push(value);
        }
    }
    Ok(tags)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PostSessionMessageInput;
    use crate::store::SessionStore;
    use std::sync::Arc;

    fn mixed_messages(count: usize) -> SessionRecord {
        let store = SessionStore::new(1, 10);
        let session = store.start_session(Some("project".into()), Some("messages".into()));
        let id = session.session_id;
        let template = store
            .post_message(PostSessionMessageInput {
                session_id: id.clone(),
                kind: SessionMessageKind::Progress,
                message: "progress".into(),
                tags: Vec::new(),
                reply_to: None,
                priority: SessionMessagePriority::Normal,
            })
            .unwrap();
        let mut record = store
            .with_record_for_query(&id, |record, _| record.clone())
            .unwrap();
        record.messages.clear();
        let kinds = [
            SessionMessageKind::Note,
            SessionMessageKind::Proposal,
            SessionMessageKind::Question,
            SessionMessageKind::Answer,
            SessionMessageKind::Decision,
            SessionMessageKind::Risk,
            SessionMessageKind::Progress,
            SessionMessageKind::Guidance,
            SessionMessageKind::Todo,
        ];
        for index in 0..count {
            let mut message = template.clone();
            message.kind = kinds[index % kinds.len()];
            message.status = if index % 3 == 0 {
                SessionMessageStatus::Resolved
            } else {
                SessionMessageStatus::Open
            };
            message.message_id = format!("message-{index}");
            message.message = format!("progress-{index}");
            record.messages.push_back(Arc::new(message));
        }
        record
    }

    fn reference_counts(record: &SessionRecord) -> SessionDiscussionCounts {
        let total = record.messages.len();
        let open = record
            .messages
            .iter()
            .filter(|m| m.status == SessionMessageStatus::Open)
            .count();
        let count = |kind| record.messages.iter().filter(|m| m.kind == kind).count();
        let open_kind = |kind| {
            record
                .messages
                .iter()
                .filter(|m| m.kind == kind && m.status == SessionMessageStatus::Open)
                .count()
        };
        SessionDiscussionCounts {
            total,
            open,
            resolved: total - open,
            guidance: count(SessionMessageKind::Guidance),
            progress: count(SessionMessageKind::Progress),
            risk: count(SessionMessageKind::Risk),
            todo: count(SessionMessageKind::Todo),
            question: count(SessionMessageKind::Question),
            answer: count(SessionMessageKind::Answer),
            decision: count(SessionMessageKind::Decision),
            open_guidance: open_kind(SessionMessageKind::Guidance),
            open_questions: open_kind(SessionMessageKind::Question),
            open_risks: open_kind(SessionMessageKind::Risk),
            open_todos: open_kind(SessionMessageKind::Todo),
        }
    }

    #[test]
    fn message_summary_single_pass_preserves_all_counts_and_recent_progress() {
        for size in [0, 1, 9, 100, 200] {
            let record = mixed_messages(size);
            let original = reference_counts(&record);
            let optimized = build_discussion_counts(&record);
            assert_eq!(
                serde_json::to_value(original).unwrap(),
                serde_json::to_value(&optimized).unwrap()
            );
            let summary = build_messages_summary(&record);
            assert_eq!(summary.total, optimized.total);
            assert_eq!(summary.open, optimized.open);
            assert_eq!(summary.resolved, optimized.resolved);
            assert_eq!(summary.pending_guidance, optimized.open_guidance);
            assert_eq!(summary.open_questions, optimized.open_questions);
            assert_eq!(summary.open_risks, optimized.open_risks);
            assert_eq!(summary.open_todos, optimized.open_todos);
            assert_eq!(summary.guidance, optimized.guidance);
            assert_eq!(summary.progress, optimized.progress);
            assert_eq!(summary.risk, optimized.risk);
            assert_eq!(summary.todo, optimized.todo);
            assert_eq!(summary.question, optimized.question);
            assert_eq!(summary.decision, optimized.decision);
            let recent = take_recent_kind(
                &record,
                SessionMessageKind::Progress,
                None,
                SUMMARY_MESSAGE_GROUP_LIMIT,
            );
            assert_eq!(summary.recent_progress, recent);
        }
    }
}
