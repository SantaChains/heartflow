//! Interactive terminal primitives for the blocking REPL: the permission
//! prompter and the `ask_user` question flow. Extracted from `main.rs`
//! (cli-thinning) so the assembly layer stays focused on wiring while these own
//! the "talk to the user over a real terminal" concern. The full-screen shell
//! has its own overlay-based prompters in `tui.rs` and does not use these.

use std::future::Future;
use std::io::{self, Write};
use std::pin::Pin;

use inquire::{MultiSelect, Select, Text};
use runtime::{PermissionPromptDecision, PermissionPrompter, PermissionRequest};

use crate::stdin_is_terminal;

/// Terminal-backed permission prompt. `allow_all` latches for the rest of
/// the session once the user answers "all".
pub(crate) struct CliPermissionPrompter {
    allow_all: bool,
}

impl CliPermissionPrompter {
    pub(crate) fn new() -> Self {
        Self { allow_all: false }
    }
}

impl PermissionPrompter for CliPermissionPrompter {
    fn decide<'a>(
        &'a mut self,
        request: &'a PermissionRequest,
    ) -> Pin<Box<dyn Future<Output = PermissionPromptDecision> + 'a>> {
        Box::pin(async move {
            if self.allow_all {
                return PermissionPromptDecision::Allow;
            }
            let deny = || PermissionPromptDecision::Deny {
                reason: "user denied the tool call".to_string(),
            };
            let preview: String = request.input.chars().take(200).collect();
            let mut stdout = io::stdout();
            let _ = writeln!(stdout, "\npermission requested: {}", request.tool_name);
            let _ = writeln!(stdout, "  {preview}");

            if stdin_is_terminal() {
                let options = vec![
                    "Allow once".to_string(),
                    "Allow all for this session".to_string(),
                    "Deny".to_string(),
                ];
                let chosen =
                    Select::new(&format!("Allow `{}`?", request.tool_name), options).prompt();
                return match chosen.as_deref() {
                    Ok("Allow once") => PermissionPromptDecision::Allow,
                    Ok("Allow all for this session") => {
                        self.allow_all = true;
                        PermissionPromptDecision::Allow
                    }
                    _ => deny(),
                };
            }

            // Non-interactive fallback: single-line y/a/n read over a plain stream.
            let _ = write!(stdout, "allow? [y]es / [a]ll / [n]o: ");
            let _ = stdout.flush();
            let mut line = String::new();
            if io::stdin().read_line(&mut line).is_err() {
                return PermissionPromptDecision::Deny {
                    reason: "stdin unavailable".to_string(),
                };
            }
            match line.trim().to_ascii_lowercase().as_str() {
                "y" | "yes" => PermissionPromptDecision::Allow,
                "a" | "all" => {
                    self.allow_all = true;
                    PermissionPromptDecision::Allow
                }
                _ => deny(),
            }
        })
    }
}

/// One option rendered by the `ask_user` tool.
pub(crate) struct QuestionOption {
    pub(crate) label: String,
    pub(crate) description: Option<String>,
}

/// Terminal-backed question flow for the `ask_user` tool.
pub(crate) trait UserQuestioner: Send + Sync {
    fn ask(
        &self,
        question: &str,
        options: &[QuestionOption],
        multi: bool,
    ) -> Result<String, String>;
}

pub(crate) struct InteractiveQuestioner;

impl UserQuestioner for InteractiveQuestioner {
    fn ask(
        &self,
        question: &str,
        options: &[QuestionOption],
        multi: bool,
    ) -> Result<String, String> {
        let answers = if stdin_is_terminal() {
            Self::ask_inquire(question, options, multi)?
        } else {
            Self::ask_manual(question, options, multi)?
        };
        serde_json::to_string(&serde_json::json!({ "answers": answers }))
            .map_err(|error| error.to_string())
    }
}

impl InteractiveQuestioner {
    /// Interactive path backed by `inquire` list/text prompts. A trailing
    /// sentinel lets the user reject the offered options and type freely.
    fn ask_inquire(
        question: &str,
        options: &[QuestionOption],
        multi: bool,
    ) -> Result<Vec<String>, String> {
        const CUSTOM: &str = "Type your own answer";
        let cancelled = || "answer cancelled".to_string();
        let prompt_custom = || Text::new("Your answer").prompt().map_err(|_| cancelled());

        if options.is_empty() {
            let text = Text::new(question).prompt().map_err(|_| cancelled())?;
            return Ok(vec![text]);
        }

        let labels: Vec<String> = options
            .iter()
            .map(|option| match &option.description {
                Some(desc) => format!("{} - {}", option.label, desc),
                None => option.label.clone(),
            })
            .collect();
        let label_at = |display: &str| {
            labels
                .iter()
                .position(|label| label == display)
                .and_then(|index| options.get(index))
                .map_or_else(|| display.to_string(), |option| option.label.clone())
        };

        if multi {
            let mut choices = labels.clone();
            choices.push(CUSTOM.to_string());
            let picked = MultiSelect::new(question, choices)
                .prompt()
                .map_err(|_| cancelled())?;
            let mut answers = Vec::new();
            for choice in &picked {
                if choice == CUSTOM {
                    answers.push(prompt_custom()?);
                } else {
                    answers.push(label_at(choice));
                }
            }
            if answers.is_empty() {
                return Err("no option selected".to_string());
            }
            Ok(answers)
        } else {
            let mut choices = labels.clone();
            choices.push(CUSTOM.to_string());
            let picked = Select::new(question, choices)
                .prompt()
                .map_err(|_| cancelled())?;
            if picked == CUSTOM {
                return Ok(vec![prompt_custom()?]);
            }
            Ok(vec![label_at(&picked)])
        }
    }

    /// Non-interactive fallback: numbered selection or free text on a plain
    /// stream, preserved for piped input and automated runs.
    fn ask_manual(
        question: &str,
        options: &[QuestionOption],
        multi: bool,
    ) -> Result<Vec<String>, String> {
        let mut stdout = io::stdout();
        let _ = writeln!(stdout);
        let _ = writeln!(stdout, "? {question}");
        for (index, option) in options.iter().enumerate() {
            match &option.description {
                Some(text) => {
                    let _ = writeln!(stdout, "  {}) {} - {text}", index + 1, option.label);
                }
                None => {
                    let _ = writeln!(stdout, "  {}) {}", index + 1, option.label);
                }
            }
        }
        if multi {
            let _ = write!(
                stdout,
                "select numbers (comma-separated), or type your own answer: "
            );
        } else {
            let _ = write!(
                stdout,
                "select a number, press Enter for 1, or type your own answer: "
            );
        }
        stdout.flush().map_err(|error| error.to_string())?;

        let mut line = String::new();
        io::stdin()
            .read_line(&mut line)
            .map_err(|error| error.to_string())?;
        let answer = line.trim();
        if answer.is_empty() {
            return Ok(vec![options
                .first()
                .map(|option| option.label.clone())
                .ok_or("no options offered; type an answer")?]);
        }
        if looks_like_selection(answer) && !options.is_empty() {
            let indices: Vec<usize> = answer
                .split(',')
                .filter(|token| !token.trim().is_empty())
                .map(|token| token.trim().parse::<usize>())
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| "invalid selection".to_string())?;
            return Ok(indices
                .into_iter()
                .map(|index| {
                    options
                        .get(index.wrapping_sub(1))
                        .map_or_else(|| index.to_string(), |option| option.label.clone())
                })
                .collect());
        }
        Ok(vec![answer.to_string()])
    }
}

fn looks_like_selection(answer: &str) -> bool {
    !answer.is_empty()
        && answer
            .chars()
            .all(|c| c.is_ascii_digit() || c == ',' || c == ' ')
}
