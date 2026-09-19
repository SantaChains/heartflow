use std::collections::BTreeMap;

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionPromptDecision {
    Allow,
    Deny { reason: String },
}

pub trait PermissionPrompter: Send {
    fn decide(&mut self, request: &PermissionRequest) -> PermissionPromptDecision;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionOutcome {
    Allow,
    Deny { reason: String },
}

#[derive(Debug, Clone)]
pub struct PermissionPolicy {
    default_mode: PermissionMode,
    tool_modes: BTreeMap<String, PermissionMode>,
    prompt_gate: Option<fn(&str, &str) -> bool>,
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

    /// Install a confirmation gate consulted only for tools in `Prompt` mode:
    /// `gate(tool_name, input)` returns whether interactive confirmation is
    /// actually required. When it returns `false`, the call auto-runs (Allow),
    /// letting high-risk actions prompt while routine ones proceed.
    #[must_use]
    pub fn with_prompt_gate(mut self, gate: fn(&str, &str) -> bool) -> Self {
        self.prompt_gate = Some(gate);
        self
    }

    #[must_use]
    pub fn mode_for(&self, tool_name: &str) -> PermissionMode {
        self.tool_modes
            .get(tool_name)
            .copied()
            .unwrap_or(self.default_mode)
    }

    #[must_use]
    pub fn authorize(
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
                if self.prompt_gate.is_some_and(|gate| !gate(tool_name, input)) {
                    return PermissionOutcome::Allow;
                }
                match prompter.as_mut() {
                    Some(prompter) => match prompter.decide(&PermissionRequest {
                        tool_name: tool_name.to_string(),
                        input: input.to_string(),
                    }) {
                        PermissionPromptDecision::Allow => PermissionOutcome::Allow,
                        PermissionPromptDecision::Deny { reason } => {
                            PermissionOutcome::Deny { reason }
                        }
                    },
                    None => PermissionOutcome::Deny {
                        reason: format!("tool '{tool_name}' requires interactive approval"),
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

    struct AllowPrompter;

    impl PermissionPrompter for AllowPrompter {
        fn decide(&mut self, request: &PermissionRequest) -> PermissionPromptDecision {
            assert_eq!(request.tool_name, "bash");
            PermissionPromptDecision::Allow
        }
    }

    struct DenyPrompter;

    impl PermissionPrompter for DenyPrompter {
        fn decide(&mut self, _request: &PermissionRequest) -> PermissionPromptDecision {
            PermissionPromptDecision::Deny {
                reason: "user rejected".to_string(),
            }
        }
    }

    // Free functions so they coerce to the `fn(&str, &str) -> bool` gate type.
    fn gate_flags_only_rm(_tool: &str, input: &str) -> bool {
        input.contains("rm")
    }
    fn gate_flags_everything(_tool: &str, _input: &str) -> bool {
        true
    }

    #[test]
    fn uses_tool_specific_overrides() {
        let policy = PermissionPolicy::new(PermissionMode::Deny)
            .with_tool_mode("bash", PermissionMode::Prompt);

        let outcome = policy.authorize("bash", "echo hi", Some(&mut AllowPrompter));
        assert_eq!(outcome, PermissionOutcome::Allow);
        assert!(matches!(
            policy.authorize("edit", "x", None),
            PermissionOutcome::Deny { .. }
        ));
    }

    #[test]
    fn allow_mode_permits_every_tool_without_a_prompter() {
        let policy = PermissionPolicy::new(PermissionMode::Allow);
        assert_eq!(
            policy.authorize("bash", "anything", None),
            PermissionOutcome::Allow
        );
        assert_eq!(
            policy.authorize("unknown_tool", "", None),
            PermissionOutcome::Allow
        );
    }

    #[test]
    fn deny_default_overridden_to_allow_for_one_tool() {
        let policy = PermissionPolicy::new(PermissionMode::Deny)
            .with_tool_mode("read", PermissionMode::Allow);
        assert_eq!(
            policy.authorize("read", "x", None),
            PermissionOutcome::Allow
        );
        assert!(matches!(
            policy.authorize("write", "x", None),
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

    #[test]
    fn prompt_without_gate_uses_prompter() {
        let policy = PermissionPolicy::new(PermissionMode::Allow)
            .with_tool_mode("bash", PermissionMode::Prompt);
        assert_eq!(
            policy.authorize("bash", "echo", Some(&mut AllowPrompter)),
            PermissionOutcome::Allow
        );
        assert!(matches!(
            policy.authorize("bash", "echo", Some(&mut DenyPrompter)),
            PermissionOutcome::Deny { reason } if reason == "user rejected"
        ));
        assert!(matches!(
            policy.authorize("bash", "echo", None),
            PermissionOutcome::Deny { reason } if reason.contains("interactive")
        ));
    }

    #[test]
    fn prompt_gate_auto_allows_safe_and_prompts_dangerous() {
        let policy = PermissionPolicy::new(PermissionMode::Allow)
            .with_tool_mode("bash", PermissionMode::Prompt)
            .with_prompt_gate(gate_flags_only_rm);
        // Routine command: gate says no confirmation needed, so it runs unattended.
        assert_eq!(
            policy.authorize("bash", "ls -la", None),
            PermissionOutcome::Allow
        );
        // Dangerous command: gate demands confirmation; no prompter means we must deny.
        assert!(matches!(
            policy.authorize("bash", "rm file", None),
            PermissionOutcome::Deny { .. }
        ));
        // Dangerous command with an approving prompter is allowed.
        assert_eq!(
            policy.authorize("bash", "rm file", Some(&mut AllowPrompter)),
            PermissionOutcome::Allow
        );
    }

    #[test]
    fn gate_is_ignored_outside_prompt_mode() {
        let policy =
            PermissionPolicy::new(PermissionMode::Allow).with_prompt_gate(gate_flags_everything);
        assert_eq!(
            policy.authorize("bash", "rm -rf /", None),
            PermissionOutcome::Allow,
            "Allow mode must not consult the gate"
        );
        let policy =
            PermissionPolicy::new(PermissionMode::Deny).with_prompt_gate(gate_flags_everything);
        assert!(matches!(
            policy.authorize("bash", "ls", None),
            PermissionOutcome::Deny { .. }
        ));
    }
}
