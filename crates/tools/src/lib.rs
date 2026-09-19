use runtime::{
    apply_patch, edit_file, execute_bash, glob_search, grep_search, read_file, search_files,
    write_file, BashCommandInput, GrepSearchInput, PatchChange,
};
use schemars::generate::SchemaSettings;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

mod graphics;
mod image;
mod todo;
mod web;

pub use graphics::{verify_graphics, GraphicsReport};
pub use image::{generate_image, GenerateImageInput, GenerateImageReport, ImageConfig};
pub use todo::{task_id, todo_tool_spec, TodoItem, TodoLedger, TodoStatus};
pub use web::{web_fetch, WebFetchInput, WebFetchReport};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolManifestEntry {
    pub name: String,
    pub source: ToolSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSource {
    Base,
    Conditional,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolRegistry {
    entries: Vec<ToolManifestEntry>,
}

impl ToolRegistry {
    #[must_use]
    pub fn new(entries: Vec<ToolManifestEntry>) -> Self {
        Self { entries }
    }

    #[must_use]
    pub fn entries(&self) -> &[ToolManifestEntry] {
        &self.entries
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
}

/// Render a `T`'s schemars-derived schema into a provider-ready JSON-Schema
/// object: no `$schema` keyword and subschemas inlined (no `$defs`/`$ref`), so
/// it drops straight into a tool's `input_schema`. Used for the newer tools so
/// their field docs and the wire schema can never drift apart.
fn tool_input_schema<T: JsonSchema>() -> Value {
    let settings = SchemaSettings::default().with(|s| {
        s.meta_schema = None;
        s.inline_subschemas = true;
    });
    let schema = settings.into_generator().into_root_schema_for::<T>();
    serde_json::to_value(schema).unwrap_or_else(|_| json!({ "type": "object" }))
}

/// Input to `search_files`. Field docs become the schema descriptions.
#[derive(Debug, Deserialize, JsonSchema)]
struct SearchFilesInput {
    /// Fuzzy query matched against each file path (folder and file name),
    /// ranked like `fzf`. Non-interactive: returns text, never a picker UI.
    query: String,
    /// Directory to search under; defaults to the current working directory.
    #[serde(default)]
    path: Option<String>,
    /// Maximum number of best-ranked paths to return (default 50, max 500).
    #[serde(default)]
    limit: Option<usize>,
}

/// One edit in an `apply_patch` batch.
#[derive(Debug, Deserialize, JsonSchema)]
struct ApplyPatchChange {
    /// File to create or edit.
    path: String,
    /// Exact text to locate. Leave empty to write `new_string` as the whole file.
    #[serde(default)]
    old_string: String,
    /// Replacement text, or the full file content when `old_string` is empty.
    new_string: String,
    /// Replace every match of `old_string` (default: it must match exactly once).
    #[serde(default)]
    replace_all: bool,
}

/// Input to `apply_patch`: an ordered, atomic batch of edits across files.
#[derive(Debug, Deserialize, JsonSchema)]
struct ApplyPatchInput {
    /// Edits to apply together. All are validated first; if any `old_string`
    /// is missing or ambiguous nothing is written.
    changes: Vec<ApplyPatchChange>,
}

#[must_use]
pub fn mvp_tool_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "bash",
            description: "Execute a shell command in the current workspace. Uses PowerShell on Windows (pwsh preferred) and sh elsewhere; write portable commands.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "timeout": { "type": "integer", "minimum": 1 },
                    "description": { "type": "string" },
                    "run_in_background": { "type": "boolean" },
                    "dangerouslyDisableSandbox": { "type": "boolean" }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "read_file",
            description: "Read a text file from the workspace. When you already know the region you need, pass offset and limit to read only those lines instead of the whole file; locate code with grep_search or search_files first.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "offset": { "type": "integer", "minimum": 0 },
                    "limit": { "type": "integer", "minimum": 1 }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "write_file",
            description: "Write a text file in the workspace.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "edit_file",
            description: "Replace text in a workspace file.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old_string": { "type": "string" },
                    "new_string": { "type": "string" },
                    "replace_all": { "type": "boolean" }
                },
                "required": ["path", "old_string", "new_string"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "glob_search",
            description: "Find files by glob pattern.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "path": { "type": "string" }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "grep_search",
            description: "Search file contents with a regex pattern.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "path": { "type": "string" },
                    "glob": { "type": "string" },
                    "output_mode": { "type": "string" },
                    "-B": { "type": "integer", "minimum": 0 },
                    "-A": { "type": "integer", "minimum": 0 },
                    "-C": { "type": "integer", "minimum": 0 },
                    "context": { "type": "integer", "minimum": 0 },
                    "-n": { "type": "boolean" },
                    "-i": { "type": "boolean" },
                    "type": { "type": "string" },
                    "head_limit": { "type": "integer", "minimum": 1 },
                    "offset": { "type": "integer", "minimum": 0 },
                    "multiline": { "type": "boolean" }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        },
        search_files_tool_spec(),
        apply_patch_tool_spec(),
    ]
}

/// `apply_patch` spec: schema generated from `ApplyPatchInput` via schemars.
#[must_use]
fn apply_patch_tool_spec() -> ToolSpec {
    ToolSpec {
        name: "apply_patch",
        description: "Apply a batch of edits across one or more files in a single call. Each change is a precise string replacement (or a whole-file write when old_string is empty); multiple changes may touch the same file. All changes are validated before any write, so a missing or ambiguous old_string aborts the whole batch. Prefer this over many edit_file calls for a coordinated multi-file change; it returns a real diff per file.",
        input_schema: tool_input_schema::<ApplyPatchInput>(),
    }
}

/// `search_files` spec: its `input_schema` is generated from `SearchFilesInput`
/// via schemars. Split out so `mvp_tool_specs` stays within the line budget.
#[must_use]
fn search_files_tool_spec() -> ToolSpec {
    ToolSpec {
        name: "search_files",
        description: "Find files by fuzzy-matching a query against their paths, best match first. Use when you roughly remember a name and want the closest files; honours .gitignore. Returns plain text paths (no interactive picker).",
        input_schema: tool_input_schema::<SearchFilesInput>(),
    }
}

/// Ask-the-user tool: the model pauses and requests a decision from the human.
/// Execution lives in the CLI layer (it needs a real terminal); this crate
/// only owns the wire spec.
#[must_use]
pub fn ask_user_tool_spec() -> ToolSpec {
    ToolSpec {
        name: "ask_user",
        description: "Ask the user a question and wait for their answer. Use when a decision needs human judgment: ambiguous requirements, destructive actions, or missing information that cannot be inferred. Offer concise options when possible; the user can always answer in free text.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "question": { "type": "string" },
                "options": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "label": { "type": "string" },
                            "description": { "type": "string" }
                        },
                        "required": ["label"],
                        "additionalProperties": false
                    }
                },
                "multi": { "type": "boolean" }
            },
            "required": ["question"],
            "additionalProperties": false
        }),
    }
}

/// Structural check for model-generated graphic files: magic bytes for real
/// images, XML parsing for SVG, tag balance for HTML.
#[must_use]
pub fn verify_graphics_tool_spec() -> ToolSpec {
    ToolSpec {
        name: "verify_graphics",
        description: "Verify a graphic file you produced: image signatures and real dimensions (png/jpg/gif/webp/bmp), strict XML parsing for SVG, tag balance for HTML. Always run this after writing a graphic artifact; you cannot see the file, this can.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" }
            },
            "required": ["path"],
            "additionalProperties": false
        }),
    }
}

/// Fetch an external URL over HTTP(S) and return extracted text. SSRF-guarded:
/// only http/https, and loopback/private/link-local/metadata hosts are rejected.
#[must_use]
pub fn web_fetch_tool_spec() -> ToolSpec {
    ToolSpec {
        name: "web_fetch",
        description: "Fetch a URL over HTTP(S) and return its extracted text (page title included). Use to read documentation, APIs, or web pages when you need external information. Non-http(s) URLs and private/loopback/cloud-metadata hosts are refused for safety; output is size-limited. Set raw=true for the untransformed body.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "url": { "type": "string" },
                "raw": { "type": "boolean" }
            },
            "required": ["url"],
            "additionalProperties": false
        }),
    }
}

pub fn execute_tool(name: &str, input: &Value) -> Result<String, String> {
    match name {
        "bash" => from_value::<BashCommandInput>(input).and_then(run_bash),
        "read_file" => from_value::<ReadFileInput>(input).and_then(run_read_file),
        "write_file" => from_value::<WriteFileInput>(input).and_then(run_write_file),
        "edit_file" => from_value::<EditFileInput>(input).and_then(run_edit_file),
        "glob_search" => from_value::<GlobSearchInputValue>(input).and_then(run_glob_search),
        "grep_search" => from_value::<GrepSearchInput>(input).and_then(run_grep_search),
        "search_files" => from_value::<SearchFilesInput>(input).and_then(run_search_files),
        "apply_patch" => from_value::<ApplyPatchInput>(input).and_then(run_apply_patch),
        "verify_graphics" => from_value::<VerifyGraphicsInput>(input).and_then(run_verify_graphics),
        "web_fetch" => from_value::<WebFetchInput>(input).and_then(run_web_fetch),
        "generate_image" => from_value::<GenerateImageInput>(input).and_then(run_generate_image),
        _ => Err(format!("unsupported tool: {name}")),
    }
}

fn from_value<T: for<'de> Deserialize<'de>>(input: &Value) -> Result<T, String> {
    serde_json::from_value(input.clone()).map_err(|error| error.to_string())
}

fn run_bash(input: BashCommandInput) -> Result<String, String> {
    serde_json::to_string_pretty(&execute_bash(input).map_err(|error| error.to_string())?)
        .map_err(|error| error.to_string())
}

#[allow(clippy::needless_pass_by_value)]
fn run_read_file(input: ReadFileInput) -> Result<String, String> {
    to_pretty_json(read_file(&input.path, input.offset, input.limit).map_err(|e| io_to_string(&e))?)
}

#[allow(clippy::needless_pass_by_value)]
fn run_write_file(input: WriteFileInput) -> Result<String, String> {
    to_pretty_json(write_file(&input.path, &input.content).map_err(|e| io_to_string(&e))?)
}

#[allow(clippy::needless_pass_by_value)]
fn run_edit_file(input: EditFileInput) -> Result<String, String> {
    to_pretty_json(
        edit_file(
            &input.path,
            &input.old_string,
            &input.new_string,
            input.replace_all.unwrap_or(false),
        )
        .map_err(|e| io_to_string(&e))?,
    )
}

#[allow(clippy::needless_pass_by_value)]
fn run_glob_search(input: GlobSearchInputValue) -> Result<String, String> {
    to_pretty_json(
        glob_search(&input.pattern, input.path.as_deref()).map_err(|e| io_to_string(&e))?,
    )
}

#[allow(clippy::needless_pass_by_value)]
fn run_grep_search(input: GrepSearchInput) -> Result<String, String> {
    to_pretty_json(grep_search(&input).map_err(|e| io_to_string(&e))?)
}

#[allow(clippy::needless_pass_by_value)]
fn run_search_files(input: SearchFilesInput) -> Result<String, String> {
    to_pretty_json(
        search_files(&input.query, input.path.as_deref(), input.limit)
            .map_err(|e| io_to_string(&e))?,
    )
}

#[allow(clippy::needless_pass_by_value)]
fn run_apply_patch(input: ApplyPatchInput) -> Result<String, String> {
    let changes: Vec<PatchChange> = input
        .changes
        .into_iter()
        .map(|change| PatchChange {
            path: change.path,
            old_string: change.old_string,
            new_string: change.new_string,
            replace_all: change.replace_all,
        })
        .collect();
    to_pretty_json(apply_patch(&changes).map_err(|e| io_to_string(&e))?)
}

fn to_pretty_json<T: serde::Serialize>(value: T) -> Result<String, String> {
    serde_json::to_string_pretty(&value).map_err(|error| error.to_string())
}

fn io_to_string(error: &std::io::Error) -> String {
    error.to_string()
}

#[derive(Debug, Deserialize)]
struct ReadFileInput {
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct WriteFileInput {
    path: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct EditFileInput {
    path: String,
    old_string: String,
    new_string: String,
    replace_all: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct GlobSearchInputValue {
    pattern: String,
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct VerifyGraphicsInput {
    path: String,
}

#[allow(clippy::needless_pass_by_value)]
fn run_verify_graphics(input: VerifyGraphicsInput) -> Result<String, String> {
    to_pretty_json(verify_graphics(&input.path).map_err(|error| error.to_string())?)
}

#[allow(clippy::needless_pass_by_value)]
fn run_web_fetch(input: WebFetchInput) -> Result<String, String> {
    to_pretty_json(web_fetch(&input).map_err(|error| error.to_string())?)
}

/// Text-to-image tool spec. Provider-agnostic: uses an OpenAI-compatible
/// `/images/generations` endpoint configured through the environment, so it stays
/// opt-in and never shadows the chat provider's credentials.
#[must_use]
pub fn generate_image_tool_spec() -> ToolSpec {
    ToolSpec {
        name: "generate_image",
        description: "Generate an image from a text prompt via an OpenAI-compatible image endpoint and save it to a file. Only available when the operator configures HEARTFLOW_IMAGE_API_KEY (optional HEARTFLOW_IMAGE_BASE_URL, default model/size). Returns the written path; follow up with verify_graphics to confirm the file. Do not claim an image was produced unless this tool returns a path.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "prompt": { "type": "string" },
                "model": { "type": "string" },
                "size": { "type": "string", "description": "e.g. 1024x1024" },
                "output_path": { "type": "string" }
            },
            "required": ["prompt"],
            "additionalProperties": false
        }),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_generate_image(input: GenerateImageInput) -> Result<String, String> {
    to_pretty_json(generate_image(&input).map_err(|error| error.to_string())?)
}

#[cfg(test)]
mod tests {
    use super::{execute_tool, mvp_tool_specs};
    use serde_json::json;

    #[test]
    fn exposes_mvp_tools() {
        let names = mvp_tool_specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        assert!(names.contains(&"bash"));
        assert!(names.contains(&"read_file"));
    }

    #[test]
    fn rejects_unknown_tool_names() {
        let error = execute_tool("nope", &json!({})).expect_err("tool should be rejected");
        assert!(error.contains("unsupported tool"));
    }

    #[test]
    fn search_files_schema_is_provider_ready() {
        let spec = mvp_tool_specs()
            .into_iter()
            .find(|spec| spec.name == "search_files")
            .expect("search_files should be advertised");
        let schema = &spec.input_schema;
        // schemars output must be a bare object schema: no `$schema` keyword and
        // `query` required, so it drops into an Anthropic tool definition as-is.
        assert_eq!(schema["type"], json!("object"));
        assert!(schema.get("$schema").is_none(), "$schema must be stripped");
        assert_eq!(schema["required"], json!(["query"]));
        assert!(schema["properties"]["query"]["description"].is_string());
    }
}
