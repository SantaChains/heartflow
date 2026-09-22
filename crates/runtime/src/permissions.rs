use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionMode {
    Allow,
    Deny,
    Prompt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRequest {
    pub tool_name: String,
    pub input: String,
    /// Why the policy stopped to ask. Set by the prompt gate when it has a
    /// specific reason (a path leaving the workspace, a destructive command);
    /// `None` when the tool is in `Prompt` mode for its own sake, e.g. network
    /// egress. Prompters render it so the human sees *why*, not just the raw
    /// JSON of a tool call.
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionPromptDecision {
    Allow,
    Deny { reason: String },
}

pub trait PermissionPrompter: Send {
    /// Async so a full-screen UI can render a permission overlay and keep
    /// pumping keys while the decision is pending. The future borrows `self`
    /// and `request` for `'a`; it is intentionally not `Send` because the
    /// consumer turn future is already non-`Send` (the runtime carries a
    /// `Cell`), so requiring `Send` here would only add friction.
    fn decide<'a>(
        &'a mut self,
        request: &'a PermissionRequest,
    ) -> Pin<Box<dyn Future<Output = PermissionPromptDecision> + 'a>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionOutcome {
    Allow,
    Deny { reason: String },
}

/// Prompt gate: given `(tool_name, input)` it either reports "no confirmation
/// needed" (`None`) or demands one and says why (`Some(reason)`).
///
/// Boxed rather than a bare `fn` pointer so a gate can capture process state —
/// write confinement needs the workspace root, which a `fn` cannot carry.
pub type PromptGate = Arc<dyn Fn(&str, &str) -> Option<String> + Send + Sync>;

#[derive(Clone)]
pub struct PermissionPolicy {
    default_mode: PermissionMode,
    tool_modes: BTreeMap<String, PermissionMode>,
    prompt_gate: Option<PromptGate>,
}

/// Hand-written because `dyn Fn` is not `Debug`; the gate is reported as an
/// opaque marker so `{:?}` on a policy stays useful without leaking its state.
impl fmt::Debug for PermissionPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PermissionPolicy")
            .field("default_mode", &self.default_mode)
            .field("tool_modes", &self.tool_modes)
            .field("prompt_gate", &self.prompt_gate.as_ref().map(|_| "<gate>"))
            .finish()
    }
}

impl PermissionPolicy {
    #[must_use]
    pub fn new(default_mode: PermissionMode) -> Self {
        Self {
            default_mode,
            tool_modes: BTreeMap::new(),
            prompt_gate: None,
        }
    }

    #[must_use]
    pub fn with_tool_mode(mut self, tool_name: impl Into<String>, mode: PermissionMode) -> Self {
        self.tool_modes.insert(tool_name.into(), mode);
        self
    }

    /// Install a confirmation gate consulted only for tools in `Prompt` mode.
    /// `gate(tool_name, input)` returns `None` to auto-run the call (Allow) or
    /// `Some(reason)` to require confirmation, with `reason` handed to the
    /// prompter. That lets high-risk actions prompt while routine ones proceed —
    /// and lets the prompt explain itself instead of showing a bare tool call.
    #[must_use]
    pub fn with_prompt_gate<F>(mut self, gate: F) -> Self
    where
        F: Fn(&str, &str) -> Option<String> + Send + Sync + 'static,
    {
        self.prompt_gate = Some(Arc::new(gate));
        self
    }

    #[must_use]
    pub fn mode_for(&self, tool_name: &str) -> PermissionMode {
        self.tool_modes
            .get(tool_name)
            .copied()
            .unwrap_or(self.default_mode)
    }

    pub async fn authorize(
        &self,
        tool_name: &str,
        input: &str,
        mut prompter: Option<&mut dyn PermissionPrompter>,
    ) -> PermissionOutcome {
        match self.mode_for(tool_name) {
            PermissionMode::Allow => PermissionOutcome::Allow,
            PermissionMode::Deny => PermissionOutcome::Deny {
                reason: format!("tool '{tool_name}' denied by permission policy"),
            },
            PermissionMode::Prompt => {
                // The gate is consulted first: `None` means "this one does not
                // need a human", `Some(reason)` proceeds to the prompt carrying
                // the explanation. No gate at all means we always ask.
                let reason = match &self.prompt_gate {
                    Some(gate) => match gate(tool_name, input) {
                        None => return PermissionOutcome::Allow,
                        Some(reason) => Some(reason),
                    },
                    None => None,
                };
                match prompter.as_mut() {
                    Some(prompter) => {
                        match prompter
                            .decide(&PermissionRequest {
                                tool_name: tool_name.to_string(),
                                input: input.to_string(),
                                reason,
                            })
                            .await
                        {
                            PermissionPromptDecision::Allow => PermissionOutcome::Allow,
                            PermissionPromptDecision::Deny { reason } => {
                                PermissionOutcome::Deny { reason }
                            }
                        }
                    }
                    // Unattended: the reason is still worth reporting, since it
                    // is the difference between "denied" and "denied because it
                    // pointed outside the workspace".
                    None => PermissionOutcome::Deny {
                        reason: match reason {
                            Some(reason) => format!(
                                "tool '{tool_name}' requires interactive approval: {reason}"
                            ),
                            None => format!("tool '{tool_name}' requires interactive approval"),
                        },
                    },
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PermissionMode, PermissionOutcome, PermissionPolicy, PermissionPromptDecision,
        PermissionPrompter, PermissionRequest,
    };
    use std::future::Future;
    use std::pin::Pin;

    struct AllowPrompter;

    impl PermissionPrompter for AllowPrompter {
        fn decide<'a>(
            &'a mut self,
            request: &'a PermissionRequest,
        ) -> Pin<Box<dyn Future<Output = PermissionPromptDecision> + 'a>> {
            Box::pin(async move {
                assert_eq!(request.tool_name, "bash");
                PermissionPromptDecision::Allow
            })
        }
    }

    struct DenyPrompter;

    impl PermissionPrompter for DenyPrompter {
        fn decide<'a>(
            &'a mut self,
            _request: &'a PermissionRequest,
        ) -> Pin<Box<dyn Future<Output = PermissionPromptDecision> + 'a>> {
            Box::pin(async move {
                PermissionPromptDecision::Deny {
                    reason: "user rejected".to_string(),
                }
            })
        }
    }

    // Free functions so they coerce to the `PromptGate` type; a `fn` item is
    // itself `Fn`, so the generic `with_prompt_gate` accepts them unchanged.
    fn gate_flags_only_rm(_tool: &str, input: &str) -> Option<String> {
        input
            .contains("rm")
            .then(|| "the command looks destructive".to_string())
    }
    fn gate_flags_everything(_tool: &str, _input: &str) -> Option<String> {
        Some("always confirm".to_string())
    }

    #[tokio::test]
    async fn uses_tool_specific_overrides() {
        let policy = PermissionPolicy::new(PermissionMode::Deny)
            .with_tool_mode("bash", PermissionMode::Prompt);

        let outcome = policy
            .authorize("bash", "echo hi", Some(&mut AllowPrompter))
            .await;
        assert_eq!(outcome, PermissionOutcome::Allow);
        assert!(matches!(
            policy.authorize("edit", "x", None).await,
            PermissionOutcome::Deny { .. }
        ));
    }

    #[tokio::test]
    async fn allow_mode_permits_every_tool_without_a_prompter() {
        let policy = PermissionPolicy::new(PermissionMode::Allow);
        assert_eq!(
            policy.authorize("bash", "anything", None).await,
            PermissionOutcome::Allow
        );
        assert_eq!(
            policy.authorize("unknown_tool", "", None).await,
            PermissionOutcome::Allow
        );
    }

    #[tokio::test]
    async fn deny_default_overridden_to_allow_for_one_tool() {
        let policy = PermissionPolicy::new(PermissionMode::Deny)
            .with_tool_mode("read", PermissionMode::Allow);
        assert_eq!(
            policy.authorize("read", "x", None).await,
            PermissionOutcome::Allow
        );
        assert!(matches!(
            policy.authorize("write", "x", None).await,
            PermissionOutcome::Deny { reason } if reason.contains("write")
        ));
    }

    #[test]
    fn mode_for_falls_back_to_default() {
        let policy = PermissionPolicy::new(PermissionMode::Prompt)
            .with_tool_mode("bash", PermissionMode::Allow);
        assert_eq!(policy.mode_for("bash"), PermissionMode::Allow);
        assert_eq!(policy.mode_for("edit"), PermissionMode::Prompt);
    }

    #[tokio::test]
    async fn prompt_without_gate_uses_prompter() {
        let policy = PermissionPolicy::new(PermissionMode::Allow)
            .with_tool_mode("bash", PermissionMode::Prompt);
        assert_eq!(
            policy
                .authorize("bash", "echo", Some(&mut AllowPrompter))
                .await,
            PermissionOutcome::Allow
        );
        assert!(matches!(
            policy.authorize("bash", "echo", Some(&mut DenyPrompter)).await,
            PermissionOutcome::Deny { reason } if reason == "user rejected"
        ));
        assert!(matches!(
            policy.authorize("bash", "echo", None).await,
            PermissionOutcome::Deny { reason } if reason.contains("interactive")
        ));
    }

    #[tokio::test]
    async fn prompt_gate_auto_allows_safe_and_prompts_dangerous() {
        let policy = PermissionPolicy::new(PermissionMode::Allow)
            .with_tool_mode("bash", PermissionMode::Prompt)
            .with_prompt_gate(gate_flags_only_rm);
        // Routine command: gate says no confirmation needed, so it runs unattended.
        assert_eq!(
            policy.authorize("bash", "ls -la", None).await,
            PermissionOutcome::Allow
        );
        // Dangerous command: gate demands confirmation; no prompter means we must deny.
        assert!(matches!(
            policy.authorize("bash", "rm file", None).await,
            PermissionOutcome::Deny { .. }
        ));
        // Dangerous command with an approving prompter is allowed.
        assert_eq!(
            policy
                .authorize("bash", "rm file", Some(&mut AllowPrompter))
                .await,
            PermissionOutcome::Allow
        );
    }

    #[tokio::test]
    async fn gate_is_ignored_outside_prompt_mode() {
        let policy =
            PermissionPolicy::new(PermissionMode::Allow).with_prompt_gate(gate_flags_everything);
        assert_eq!(
            policy.authorize("bash", "rm -rf /", None).await,
            PermissionOutcome::Allow,
            "Allow mode must not consult the gate"
        );
        let policy =
            PermissionPolicy::new(PermissionMode::Deny).with_prompt_gate(gate_flags_everything);
        assert!(matches!(
            policy.authorize("bash", "ls", None).await,
            PermissionOutcome::Deny { .. }
        ));
    }

    #[tokio::test]
    async fn gate_reason_reaches_the_prompter() {
        struct ReasonPrompter;

        impl PermissionPrompter for ReasonPrompter {
            fn decide<'a>(
                &'a mut self,
                request: &'a PermissionRequest,
            ) -> Pin<Box<dyn Future<Output = PermissionPromptDecision> + 'a>> {
                Box::pin(async move {
                    assert_eq!(
                        request.reason.as_deref(),
                        Some("points outside the workspace")
                    );
                    PermissionPromptDecision::Allow
                })
            }
        }

        let policy = PermissionPolicy::new(PermissionMode::Allow)
            .with_tool_mode("write_file", PermissionMode::Prompt)
            .with_prompt_gate(|_tool, _input| Some("points outside the workspace".to_string()));
        assert_eq!(
            policy
                .authorize("write_file", "{}", Some(&mut ReasonPrompter))
                .await,
            PermissionOutcome::Allow
        );
    }

    /// The gate is boxed precisely so it can capture state — the CLI captures the
    /// workspace root; this proves a closure (not just a `fn`) is accepted.
    #[tokio::test]
    async fn a_capturing_gate_uses_outer_state() {
        let root = std::path::PathBuf::from("/workspace");
        let policy = PermissionPolicy::new(PermissionMode::Allow)
            .with_tool_mode("write_file", PermissionMode::Prompt)
            .with_prompt_gate(move |_tool, input| {
                (!input.starts_with(root.to_string_lossy().as_ref()))
                    .then(|| format!("{input} is outside {}", root.display()))
            });

        assert_eq!(
            policy
                .authorize("write_file", "/workspace/a.txt", None)
                .await,
            PermissionOutcome::Allow
        );
        assert!(matches!(
            policy.authorize("write_file", "/etc/passwd", None).await,
            PermissionOutcome::Deny { reason } if reason.contains("/etc/passwd")
        ));
    }
}
