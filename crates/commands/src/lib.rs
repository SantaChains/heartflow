use runtime::{compact_session_in_place, CompactionConfig, MessageRole, Session};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandManifestEntry {
    pub name: String,
    pub source: CommandSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandSource {
    Builtin,
    InternalOnly,
    FeatureGated,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandRegistry {
    entries: Vec<CommandManifestEntry>,
}

impl CommandRegistry {
    #[must_use]
    pub fn new(entries: Vec<CommandManifestEntry>) -> Self {
        Self { entries }
    }

    #[must_use]
    pub fn entries(&self) -> &[CommandManifestEntry] {
        &self.entries
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCommandResult {
    pub message: String,
    pub session: Session,
}

#[must_use]
pub fn handle_slash_command(
    input: &str,
    session: &Session,
    compaction: CompactionConfig,
) -> Option<SlashCommandResult> {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') {
        return None;
    }

    match trimmed.split_whitespace().next() {
        Some("/compact") => {
            let mut session = session.clone();
            let message = if runtime::should_compact(&session, compaction) {
                let result = compact_session_in_place(&mut session, compaction);
                format!(
                    "Compacted {} messages into a resumable system summary.",
                    result.removed_message_count
                )
            } else {
                "Compaction skipped: session is below the compaction threshold.".to_string()
            };
            Some(SlashCommandResult { message, session })
        }
        Some("/pin") => {
            let mut session = session.clone();
            match session
                .messages
                .iter_mut()
                .rev()
                .find(|message| message.role != MessageRole::System)
            {
                Some(message) => {
                    message.pinned = !message.pinned;
                    let verb = if message.pinned { "pinned" } else { "unpinned" };
                    let count = session.messages.iter().filter(|m| m.pinned).count();
                    Some(SlashCommandResult {
                        message: format!("{verb} the last message ({count} pinned total)."),
                        session,
                    })
                }
                None => Some(SlashCommandResult {
                    message: "nothing to pin - session has no pinnable messages.".to_string(),
                    session,
                }),
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::handle_slash_command;
    use runtime::{CompactionConfig, ContentBlock, ConversationMessage, MessageRole, Session};

    #[test]
    fn compacts_sessions_via_slash_command() {
        let session = Session {
            version: 1,
            messages: vec![
                ConversationMessage::user_text("a ".repeat(200)),
                ConversationMessage::assistant(vec![ContentBlock::Text {
                    text: "b ".repeat(200),
                }]),
                ConversationMessage::tool_result("1", "bash", "ok ".repeat(200), false),
                ConversationMessage::assistant(vec![ContentBlock::Text {
                    text: "recent".to_string(),
                }]),
            ],
        };

        let result = handle_slash_command(
            "/compact",
            &session,
            CompactionConfig {
                preserve_recent_messages: 2,
                max_estimated_tokens: 1,
                ..CompactionConfig::default()
            },
        )
        .expect("slash command should be handled");

        // preserve_recent_messages=2 would cut right before the tool_result at
        // index 2; boundary alignment walks back over it so the preserved tail
        // never opens on an orphaned result, folding only the leading message.
        assert!(result.message.contains("Compacted 1 messages"));
        assert_eq!(result.session.messages[0].role, MessageRole::System);
    }

    #[test]
    fn ignores_unknown_slash_commands() {
        let session = Session::new();
        assert!(handle_slash_command("/unknown", &session, CompactionConfig::default()).is_none());
    }

    #[test]
    fn pin_toggles_the_last_message_offline() {
        let session = Session {
            version: 1,
            messages: vec![
                ConversationMessage::user_text("first"),
                ConversationMessage::assistant(vec![ContentBlock::Text {
                    text: "last".to_string(),
                }]),
            ],
        };

        let pinned = handle_slash_command("/pin", &session, CompactionConfig::default())
            .expect("pin handled");
        assert!(pinned.message.contains("pinned the last message"));
        assert!(pinned.session.messages[1].pinned);
        assert!(!pinned.session.messages[0].pinned);

        // Toggling again clears it.
        let unpinned = handle_slash_command("/pin", &pinned.session, CompactionConfig::default())
            .expect("unpin handled");
        assert!(unpinned.message.contains("unpinned the last message"));
        assert!(!unpinned.session.messages[1].pinned);
    }
}
