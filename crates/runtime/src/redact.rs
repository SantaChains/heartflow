//! Persistence-boundary redaction.
//!
//! The transcript that reaches disk (the authoritative per-session JSON
//! snapshot and the SQLite search mirror) must never carry live credentials:
//! users paste keys into prompts, and tool output (curl, git, build logs)
//! echoes secrets. Redaction runs on a *clone* at the single save choke-point,
//! so the in-memory session that talks to the provider is never mutated — the
//! model still sees whatever it legitimately needs within the live turn.
//!
//! Two complementary layers:
//! 1. Structural patterns (URL userinfo, `Authorization: Bearer`, well-known
//!    key prefixes, labeled `api_key = "..."` assignments) that catch secrets
//!    even when we never saw the value.
//! 2. Exact literals of the currently configured credentials, passed in by the
//!    caller, so a key that does not match any prefix rule is still scrubbed.

use regex::Regex;
use std::sync::OnceLock;

use crate::session::{ContentBlock, ConversationMessage, Session};

/// Replacement marker left in place of every scrubbed secret.
pub const REDACTED: &str = "[REDACTED]";

/// One compiled structural rule: match everything but only replace the secret
/// portion, so surrounding context (`scheme://`, `Authorization: Bearer `)
/// stays readable for debugging.
struct Rule {
    pattern: &'static Regex,
    replacement: &'static str,
}

fn url_userinfo() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `scheme://user:password@` -> `scheme://[REDACTED]@` (password gone,
        // scheme preserved so the endpoint is still identifiable).
        Regex::new(r"([A-Za-z][A-Za-z0-9+.-]*://)[^\s/@:][^\s/@]*:[^\s@/]*@")
            .expect("valid url-userinfo regex")
    })
}

fn auth_header() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `Authorization: Bearer <tok>` / `... Basic <tok>` (header or `=` form).
        Regex::new(r"(?i)\b(authorization\s*[:=]\s*(?:bearer|basic)\s+)[A-Za-z0-9._~+/=-]{6,}")
            .expect("valid auth-header regex")
    })
}

fn labeled_secret() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // An explicit secret label assigned a value. Bare `token` is excluded so
        // `max_tokens = 4096` is not scrubbed; only compound labels match.
        Regex::new(
            r#"(?i)(\b(?:api[_-]?key|apikey|secret[_-]?key|access[_-]?token|refresh[_-]?token|auth[_-]?token|client[_-]?secret|password|passwd)\b\s*[:=]\s*["']?)([^\s"',;}\]]{6,})"#,
        )
        .expect("valid labeled-secret regex")
    })
}

fn prefixed_key() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Well-known high-entropy key prefixes (Anthropic/OpenAI/DeepSeek `sk-`,
        // GitHub `ghp_`/`gho_`/..., AWS `AKIA...`).
        Regex::new(r"\b(?:sk-[A-Za-z0-9_-]{16,}|gh[pousr]_[A-Za-z0-9]{20,}|AKIA[0-9A-Z]{16})")
            .expect("valid prefixed-key regex")
    })
}

fn rules() -> Vec<Rule> {
    vec![
        Rule {
            pattern: url_userinfo(),
            replacement: "${1}[REDACTED]@",
        },
        Rule {
            pattern: auth_header(),
            replacement: "${1}[REDACTED]",
        },
        Rule {
            pattern: labeled_secret(),
            replacement: "${1}[REDACTED]",
        },
        Rule {
            pattern: prefixed_key(),
            replacement: "[REDACTED]",
        },
    ]
}

/// Scrub structural secret shapes and, optionally, a set of exact literal
/// values (the active credentials) from a single string. Literals are replaced
/// first so a value that matches no prefix rule is still removed.
#[must_use]
pub fn redact_text(text: &str, literals: &[String]) -> String {
    let mut out = text.to_string();
    for literal in literals {
        let needle = literal.trim();
        // Skip empties and trivially short strings that would over-redact
        // (a 1-3 char "key" is far more likely to be ordinary text).
        if needle.len() < 4 {
            continue;
        }
        if out.contains(needle) {
            out = out.replace(needle, REDACTED);
        }
    }
    for rule in rules() {
        if rule.pattern.is_match(&out) {
            out = rule
                .pattern
                .replace_all(&out, rule.replacement)
                .into_owned();
        }
    }
    out
}

fn redact_block(block: &ContentBlock, literals: &[String]) -> ContentBlock {
    match block {
        ContentBlock::Text { text } => ContentBlock::Text {
            text: redact_text(text, literals),
        },
        ContentBlock::ToolUse { id, name, input } => ContentBlock::ToolUse {
            id: id.clone(),
            name: name.clone(),
            input: redact_text(input, literals),
        },
        ContentBlock::ToolResult {
            tool_use_id,
            tool_name,
            output,
            is_error,
        } => ContentBlock::ToolResult {
            tool_use_id: tool_use_id.clone(),
            tool_name: tool_name.clone(),
            output: redact_text(output, literals),
            is_error: *is_error,
        },
    }
}

fn redact_message(message: &ConversationMessage, literals: &[String]) -> ConversationMessage {
    ConversationMessage {
        role: message.role,
        blocks: message
            .blocks
            .iter()
            .map(|block| redact_block(block, literals))
            .collect(),
        usage: message.usage,
        pinned: message.pinned,
    }
}

/// Return a copy of `session` with every credential-shaped string scrubbed from
/// message content. Ids, roles, tool names and usage are preserved byte-for-byte
/// so the redacted transcript still round-trips and mirrors cleanly.
#[must_use]
pub fn redact_session(session: &Session, literals: &[String]) -> Session {
    Session {
        version: session.version,
        messages: session
            .messages
            .iter()
            .map(|message| redact_message(message, literals))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::{redact_session, redact_text, REDACTED};
    use crate::session::{ContentBlock, ConversationMessage, Session};

    #[test]
    fn scrubs_url_userinfo_but_keeps_scheme() {
        let out = redact_text("clone https://alice:s3cr3tpw@github.com/x/y.git", &[]);
        assert!(out.contains("https://[REDACTED]@github.com"));
        assert!(!out.contains("s3cr3tpw"));
        assert!(!out.contains("alice"));
    }

    #[test]
    fn scrubs_bearer_and_basic_authorization() {
        let out = redact_text("Authorization: Bearer abcdef123456xyz", &[]);
        assert!(out.contains("Authorization: Bearer [REDACTED]"));
        assert!(!out.contains("abcdef123456xyz"));

        let basic = redact_text("authorization = Basic Zm9vOmJhcg==", &[]);
        assert!(!basic.contains("Zm9vOmJhcg"));
    }

    #[test]
    fn scrubs_prefixed_keys() {
        let sk = redact_text("use sk-proj-abcdefghijklmnop1234 now", &[]);
        assert!(sk.contains(REDACTED));
        assert!(!sk.contains("abcdefghijklmnop"));

        let gh = redact_text("token ghp_abcdefghijklmnopqrstuvwxyz123456", &[]);
        assert!(!gh.contains("ghp_abcdefghijklmnopqrstuvwxyz"));

        let aws = redact_text("key AKIAIOSFODNN7EXAMPLE here", &[]);
        assert!(!aws.contains("AKIAIOSFODNN7EXAMPLE"));
    }

    #[test]
    fn scrubs_labeled_assignments_but_not_max_tokens() {
        let out = redact_text(r#"api_key = "sk-thisisasecretvalue""#, &[]);
        assert!(out.contains("api_key"));
        assert!(!out.contains("thisisasecretvalue"));

        // A benign max_tokens setting must survive untouched.
        let keep = redact_text("max_tokens = 4096", &[]);
        assert_eq!(keep, "max_tokens = 4096");
    }

    #[test]
    fn scrubs_exact_configured_literal() {
        // A key that matches no structural prefix is still removed by literal.
        let literal = String::from("weirdvalue-not-a-known-shape");
        let out = redact_text("the key is weirdvalue-not-a-known-shape ok", &[literal]);
        assert!(out.contains(REDACTED));
        assert!(!out.contains("weirdvalue-not-a-known-shape"));
    }

    #[test]
    fn ignores_trivially_short_literals() {
        // A 2-char "secret" is ordinary text; must not be scrubbed.
        let out = redact_text("ab is fine, nothing here", &[String::from("ab")]);
        assert_eq!(out, "ab is fine, nothing here");
    }

    #[test]
    fn redact_session_preserves_structure_and_ids() {
        let session = Session {
            version: 1,
            messages: vec![
                ConversationMessage::user_text("run curl -u bob:hunter2 https://x"),
                ConversationMessage::assistant(vec![ContentBlock::ToolUse {
                    id: "tool-1".to_string(),
                    name: "bash".to_string(),
                    input: "echo sk-abcdefghijklmnopqrstuv".to_string(),
                }]),
                ConversationMessage::tool_result("tool-1", "bash", "https://u:p@host done", false),
            ],
        };
        let redacted = redact_session(&session, &[]);

        // Same shape preserved.
        assert_eq!(redacted.version, session.version);
        assert_eq!(redacted.messages.len(), session.messages.len());
        assert_eq!(redacted.messages[1].role, session.messages[1].role);
        match &redacted.messages[1].blocks[0] {
            ContentBlock::ToolUse { id, name, .. } => {
                assert_eq!(id, "tool-1");
                assert_eq!(name, "bash");
            }
            other => panic!("expected tool_use, got {other:?}"),
        }
        match &redacted.messages[2].blocks[0] {
            ContentBlock::ToolResult {
                tool_use_id,
                tool_name,
                output,
                ..
            } => {
                assert_eq!(tool_use_id, "tool-1");
                assert_eq!(tool_name, "bash");
                assert!(output.contains("[REDACTED]@host"));
                assert!(!output.contains("hunter2") && !output.contains(":p@"));
            }
            other => panic!("expected tool_result, got {other:?}"),
        }
    }

    #[test]
    fn original_session_is_not_mutated() {
        let session = Session {
            version: 1,
            messages: vec![ConversationMessage::user_text(
                "secret sk-abcdefghijklmnopqrstuv",
            )],
        };
        let before = match &session.messages[0].blocks[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => unreachable!(),
        };
        let _ = redact_session(&session, &[]);
        let after = match &session.messages[0].blocks[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => unreachable!(),
        };
        assert_eq!(before, after, "caller's session must stay verbatim");
    }
}
