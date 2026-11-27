use crate::session::Session;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_creation_input_tokens: u32,
    pub cache_read_input_tokens: u32,
}

impl TokenUsage {
    #[must_use]
    pub fn total_tokens(self) -> u32 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_creation_input_tokens)
            .saturating_add(self.cache_read_input_tokens)
    }

    /// Size of the prompt the provider actually charged for. `input_tokens`
    /// alone excludes everything served from the prompt cache, so all three
    /// input counters must be summed before comparing against a context-window
    /// budget — otherwise a well-cached session looks arbitrarily small.
    /// Distinct from [`total_tokens`], which is billing-oriented and adds the
    /// model's output as well.
    #[must_use]
    pub fn context_input_tokens(self) -> u32 {
        self.input_tokens
            .saturating_add(self.cache_creation_input_tokens)
            .saturating_add(self.cache_read_input_tokens)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageTracker {
    latest_turn: TokenUsage,
    cumulative: TokenUsage,
    turns: u32,
}

impl UsageTracker {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuild the tracker from the usage attached to a session's messages.
    ///
    /// Best-effort after a reload: usage recorded for requests that never
    /// landed a message (a transport attempt whose blocks were dropped for
    /// the retry, the compaction-summary side call) has no on-disk anchor and
    /// drops out of the rebuilt totals — the in-memory tracker is the
    /// accurate view until the process exits.
    #[must_use]
    pub fn from_session(session: &Session) -> Self {
        let mut tracker = Self::new();
        for message in &session.messages {
            if let Some(usage) = message.usage {
                tracker.record(usage);
            }
        }
        tracker
    }

    pub fn record(&mut self, usage: TokenUsage) {
        self.latest_turn = usage;
        let cumulative = &mut self.cumulative;
        cumulative.input_tokens = cumulative.input_tokens.saturating_add(usage.input_tokens);
        cumulative.output_tokens = cumulative.output_tokens.saturating_add(usage.output_tokens);
        cumulative.cache_creation_input_tokens = cumulative
            .cache_creation_input_tokens
            .saturating_add(usage.cache_creation_input_tokens);
        cumulative.cache_read_input_tokens = cumulative
            .cache_read_input_tokens
            .saturating_add(usage.cache_read_input_tokens);
        self.turns += 1;
    }

    #[must_use]
    pub fn current_turn_usage(&self) -> TokenUsage {
        self.latest_turn
    }

    #[must_use]
    pub fn cumulative_usage(&self) -> TokenUsage {
        self.cumulative
    }

    #[must_use]
    pub fn turns(&self) -> u32 {
        self.turns
    }
}

#[cfg(test)]
mod tests {
    use super::{TokenUsage, UsageTracker};
    use crate::session::{ContentBlock, ConversationMessage, MessageRole, Session};

    #[test]
    fn context_input_counts_cached_tokens() {
        // A fully cached resend: `input_tokens` collapses to 0 while the real
        // prompt is entirely present in `cache_read`. Summing only
        // `input_tokens` would report an empty context.
        let usage = TokenUsage {
            input_tokens: 0,
            output_tokens: 42,
            cache_creation_input_tokens: 300,
            cache_read_input_tokens: 12_000,
        };
        assert_eq!(usage.context_input_tokens(), 12_300);
        assert_eq!(usage.total_tokens(), 12_342);
    }

    #[test]
    fn tracks_true_cumulative_usage() {
        let mut tracker = UsageTracker::new();
        tracker.record(TokenUsage {
            input_tokens: 10,
            output_tokens: 4,
            cache_creation_input_tokens: 2,
            cache_read_input_tokens: 1,
        });
        tracker.record(TokenUsage {
            input_tokens: 20,
            output_tokens: 6,
            cache_creation_input_tokens: 3,
            cache_read_input_tokens: 2,
        });

        assert_eq!(tracker.turns(), 2);
        assert_eq!(tracker.current_turn_usage().input_tokens, 20);
        assert_eq!(tracker.current_turn_usage().output_tokens, 6);
        assert_eq!(tracker.cumulative_usage().output_tokens, 10);
        assert_eq!(tracker.cumulative_usage().input_tokens, 30);
        assert_eq!(tracker.cumulative_usage().total_tokens(), 48);
    }

    #[test]
    fn reconstructs_usage_from_session_messages() {
        let session = Session {
            version: 1,
            messages: vec![ConversationMessage {
                role: MessageRole::Assistant,
                blocks: vec![ContentBlock::Text {
                    text: "done".to_string(),
                }],
                usage: Some(TokenUsage {
                    input_tokens: 5,
                    output_tokens: 2,
                    cache_creation_input_tokens: 1,
                    cache_read_input_tokens: 0,
                }),
                pinned: false,
            }],
        };

        let tracker = UsageTracker::from_session(&session);
        assert_eq!(tracker.turns(), 1);
        assert_eq!(tracker.cumulative_usage().total_tokens(), 8);
    }
}
