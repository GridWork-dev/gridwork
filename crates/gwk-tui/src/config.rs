//! The Config lens — a git-backed reconciler over four policy files.
//!
//! Git remains canonical. A form edit writes one allowlisted file, commits
//! only that path, then returns a typed `RecordEvidence` carrying the commit
//! SHA. The caller submits that command to the kernel, whose `config_change`
//! risk class mints the literal receipt in the same transaction. If the
//! process stops between commit and submit, [`ConfigRepository::reconcile_startup`]
//! compares the newest config commit with the newest recorded evidence and
//! returns `RaiseAttention`; it never rewrites or reorders the history.
//!
//! The allowlist is a closed type, not a prefix check. In particular,
//! `identity/authority-policy.toml` and any allowlisted file whose current or
//! proposed contents source `~/.gridwork/env` cannot enter the form path.
//! [`ConfigRepository::commit_editor`] opens the exact `$EDITOR` executable
//! and lets `git commit` collect an operator-typed message. Neither process is
//! reached through a shell.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, OpenOptions, TryLockError};
use std::io::{self, Write as _};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

use gwk_domain::command::KernelCommand;
use gwk_domain::entity::Evidence;
use gwk_domain::ids::{AttentionItemId, EvidenceId};
use gwk_theme::marks::{GlyphSet, StateBinding};
use gwk_theme::tier::ColorTier;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use crate::input::HitMap;
use crate::theme;

const CONFIG_CHANGE_KIND: &str = "config_change";
const DIVERGENCE_KIND: &str = "config_divergence";
const DIVERGENCE_SUBJECT: &str = "config/repository";
const ENV_SOURCE: &str = "~/.gridwork/env";
const MAX_CONFIG_BYTES: usize = 1024 * 1024;
const MAX_COMMIT_MESSAGE_BYTES: usize = 256;

/// The complete editable set. A string outside these four values cannot become
/// a `ConfigPath`, so filesystem and git operations never receive a caller's
/// unchecked path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ConfigPath {
    AuthorityPolicy,
    Capabilities,
    NamespaceScopes,
    Orchestration,
}

impl ConfigPath {
    pub const ALL: [Self; 4] = [
        Self::AuthorityPolicy,
        Self::Capabilities,
        Self::NamespaceScopes,
        Self::Orchestration,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AuthorityPolicy => "identity/authority-policy.toml",
            Self::Capabilities => "identity/capabilities.toml",
            Self::NamespaceScopes => "identity/namespace-scopes.toml",
            Self::Orchestration => "identity/orchestration.toml",
        }
    }

    pub fn parse(path: &str) -> Result<Self, ConfigError> {
        match path {
            "identity/authority-policy.toml" => Ok(Self::AuthorityPolicy),
            "identity/capabilities.toml" => Ok(Self::Capabilities),
            "identity/namespace-scopes.toml" => Ok(Self::NamespaceScopes),
            "identity/orchestration.toml" => Ok(Self::Orchestration),
            _ => Err(ConfigError::PathNotEditable {
                path: path.to_owned(),
            }),
        }
    }
}

impl std::fmt::Display for ConfigPath {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Which editing affordance may touch a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditRoute {
    /// Serialized output from the file's validated schema form.
    Form,
    /// `$EDITOR`, followed by a message typed into `git commit` by the operator.
    Editor,
}

impl EditRoute {
    /// The exact words the lens paints for this route; `/` filters match
    /// against the same text the operator reads.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Form => "validated form",
            Self::Editor => "$EDITOR + typed commit",
        }
    }
}

/// The exact TOML shape a generated form owns for one allowlisted file.
///
/// The template carries every accepted key and its value kind. Validation
/// refuses unknown, missing, or differently typed fields before it can mint a
/// [`ValidatedForm`]. This keeps the write API honest: a raw string cannot call
/// [`ConfigRepository::commit_form`].
#[derive(Debug)]
pub struct ConfigFormSchema {
    path: ConfigPath,
    template: toml::Value,
    baseline_head: Option<String>,
    baseline_contents: String,
    lock: ConfigOperationLock,
}

impl ConfigFormSchema {
    pub fn validate(self, serialized: &str) -> Result<ValidatedForm, ConfigError> {
        validate_config(self.path, serialized)?;
        let value = parse_toml(self.path, serialized)?;
        validate_shape(self.path, "", &self.template, &value)?;
        Ok(ValidatedForm {
            path: self.path,
            serialized: serialized.to_owned(),
            baseline_head: self.baseline_head.clone(),
            baseline_contents: self.baseline_contents.clone(),
            lock: self.lock,
        })
    }
}

/// One editable leaf from the incumbent TOML document.
///
/// Paths and kinds are derived from the locked schema. The form never accepts
/// a caller-provided field name, so it cannot offer an add/remove operation the
/// validator would later reject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigFormField {
    segments: Vec<String>,
    path: String,
    kind: &'static str,
    original: String,
    value: String,
}

impl ConfigFormField {
    pub fn path(&self) -> &str {
        &self.path
    }

    pub const fn kind(&self) -> &'static str {
        self.kind
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    pub fn changed(&self) -> bool {
        self.value != self.original
    }
}

/// A live generated-form session. Owning this value owns the repository's
/// exclusive config-operation lock until cancel or commit.
#[derive(Debug)]
pub struct ConfigForm {
    schema: ConfigFormSchema,
    fields: Vec<ConfigFormField>,
    selected: usize,
    commit_message: String,
    replace_on_type: bool,
}

impl ConfigForm {
    fn new(schema: ConfigFormSchema) -> Result<Self, ConfigError> {
        let mut fields = Vec::new();
        collect_form_fields(&schema.template, &mut Vec::new(), &mut fields);
        let spans = assignment_value_spans(schema.path, &schema.baseline_contents)?;
        if let Some(field) = fields
            .iter()
            .find(|field| !spans.contains_key(&field.segments))
        {
            return Err(ConfigError::FormSourceMismatch {
                path: schema.path,
                field: field.path.clone(),
            });
        }
        Ok(Self {
            schema,
            fields,
            selected: 0,
            commit_message: String::new(),
            replace_on_type: true,
        })
    }

    pub fn path(&self) -> ConfigPath {
        self.schema.path
    }

    pub fn fields(&self) -> &[ConfigFormField] {
        &self.fields
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn commit_selected(&self) -> bool {
        self.selected == self.fields.len()
    }

    pub fn commit_message(&self) -> &str {
        &self.commit_message
    }

    pub fn select(&mut self, index: usize) {
        self.selected = index.min(self.fields.len());
        self.replace_on_type = true;
    }

    pub fn move_selection(&mut self, delta: i8) {
        let count = self.fields.len() + 1;
        self.selected = if delta < 0 {
            self.selected.checked_sub(1).unwrap_or(count - 1)
        } else {
            (self.selected + 1) % count
        };
        self.replace_on_type = true;
    }

    pub fn replace_selected(&mut self, value: impl Into<String>) {
        let value = value.into();
        if let Some(field) = self.fields.get_mut(self.selected) {
            field.value = value;
        } else {
            self.commit_message = value;
        }
        self.replace_on_type = false;
    }

    pub fn set_commit_message(&mut self, message: impl Into<String>) {
        self.commit_message = message.into();
    }

    pub fn insert(&mut self, character: char) {
        if self.replace_on_type {
            self.replace_selected(String::new());
        }
        if let Some(field) = self.fields.get_mut(self.selected) {
            field.value.push(character);
        } else {
            self.commit_message.push(character);
        }
    }

    pub fn backspace(&mut self) {
        if let Some(field) = self.fields.get_mut(self.selected) {
            field.value.pop();
        } else {
            self.commit_message.pop();
        }
        self.replace_on_type = false;
    }

    pub fn toggle_boolean(&mut self) -> bool {
        let Some(field) = self.fields.get_mut(self.selected) else {
            return false;
        };
        if field.kind != "boolean" {
            return false;
        }
        field.value = if field.value == "true" {
            "false".to_owned()
        } else {
            "true".to_owned()
        };
        self.replace_on_type = false;
        true
    }

    pub fn validate_current(&self) -> Result<(), ConfigError> {
        validate_commit_message(&self.commit_message)?;
        let serialized = self.serialized()?;
        let value = parse_toml(self.schema.path, &serialized)?;
        validate_shape(self.schema.path, "", &self.schema.template, &value)
    }

    pub fn finish(self) -> Result<(ValidatedForm, String), ConfigError> {
        if !self.fields.iter().any(ConfigFormField::changed) {
            return Err(ConfigError::NoFormChanges {
                path: self.schema.path,
            });
        }
        self.validate_current()?;
        let serialized = self.serialized()?;
        let message = self.commit_message;
        self.schema
            .validate(&serialized)
            .map(|form| (form, message))
    }

    fn serialized(&self) -> Result<String, ConfigError> {
        let spans = assignment_value_spans(self.schema.path, &self.schema.baseline_contents)?;
        let mut replacements = Vec::new();
        for field in self.fields.iter().filter(|field| field.changed()) {
            let value = parse_form_value(self.schema.path, field)?;
            let span = spans.get(&field.segments).cloned().ok_or_else(|| {
                ConfigError::FormSourceMismatch {
                    path: self.schema.path,
                    field: field.path.clone(),
                }
            })?;
            replacements.push((span, value.to_string()));
        }
        replacements.sort_by_key(|replacement| std::cmp::Reverse(replacement.0.start));
        let mut serialized = self.schema.baseline_contents.clone();
        for (span, value) in replacements {
            serialized.replace_range(span, &value);
        }
        Ok(serialized)
    }
}

/// Serialized form output that has passed its path-specific schema.
#[derive(Debug)]
pub struct ValidatedForm {
    path: ConfigPath,
    serialized: String,
    baseline_head: Option<String>,
    baseline_contents: String,
    lock: ConfigOperationLock,
}

impl ValidatedForm {
    pub fn path(&self) -> ConfigPath {
        self.path
    }

    pub fn as_str(&self) -> &str {
        &self.serialized
    }
}

/// One allowlisted file as the lens paints it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigFileState {
    pub path: ConfigPath,
    pub route: EditRoute,
    /// Bounded current TOML. The selected file's schema form consumes this;
    /// rendering remains a pure function of the state.
    pub contents: String,
}

/// The complete frame value. Rendering does no I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigState {
    pub files: Vec<ConfigFileState>,
    pub config_head: Option<String>,
    pub last_evidence_ref: Option<String>,
    pub dirty: bool,
    pub divergent: bool,
}

/// A file row reachable through either the keyboard walk or the frame hit map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigTarget {
    File(ConfigPath),
}

/// Typed boundary failures. Most importantly, an off-allowlist path and an
/// attempted form edit of an editor-only file remain distinguishable from I/O.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config path is not editable: {path}")]
    PathNotEditable { path: String },
    #[error("config path requires $EDITOR and an operator-typed commit: {path}")]
    EditorRequired { path: ConfigPath },
    #[error("$EDITOR is not configured")]
    EditorNotConfigured,
    #[error("$EDITOR failed for {path} with status {code:?}")]
    EditorFailed { path: ConfigPath, code: Option<i32> },
    #[error("config text for {path} exceeds {limit} bytes")]
    ConfigTooLarge { path: ConfigPath, limit: usize },
    #[error("config text for {path} contains a NUL byte")]
    ConfigContainsNul { path: ConfigPath },
    #[error("config text for {path} is not UTF-8")]
    ConfigNotUtf8 { path: ConfigPath },
    #[error("config text for {path} is not valid TOML: {source}")]
    InvalidToml {
        path: ConfigPath,
        #[source]
        source: toml::de::Error,
    },
    #[error("config form schema mismatch for {path} at {field}: expected {expected}, got {actual}")]
    SchemaMismatch {
        path: ConfigPath,
        field: String,
        expected: &'static str,
        actual: &'static str,
    },
    #[error("config form value for {path} at {field} is not a valid {expected}")]
    InvalidFormValue {
        path: ConfigPath,
        field: String,
        expected: &'static str,
    },
    #[error("config form cannot safely address {path} at {field}; use $EDITOR")]
    FormSourceMismatch { path: ConfigPath, field: String },
    #[error("config form for {path} has no value changes")]
    NoFormChanges { path: ConfigPath },
    #[error("could not serialize config form for {path}: {source}")]
    SerializeForm {
        path: ConfigPath,
        #[source]
        source: toml::ser::Error,
    },
    #[error("commit message must be 1..={limit} printable ASCII bytes")]
    InvalidCommitMessage { limit: usize },
    #[error("config path has uncommitted changes: {path}")]
    DirtyPath { path: ConfigPath },
    #[error("expected config_change evidence, got {kind}")]
    WrongEvidenceKind { kind: String },
    #[error("config repository changed while editing {path}")]
    ConcurrentChange { path: ConfigPath },
    #[error("another config edit is already in progress")]
    EditInProgress,
    #[error("{operation} failed at {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("allowlisted config path resolves outside the repository: {path}")]
    OutsideRepository { path: PathBuf },
    #[error("allowlisted config path must not be a symlink or directory alias: {path}")]
    AliasedPath { path: PathBuf },
    #[error("allowlisted config path is not a regular file: {path}")]
    NotAFile { path: PathBuf },
    #[error("git {operation} failed with status {code:?}")]
    GitFailed {
        operation: &'static str,
        code: Option<i32>,
    },
    #[error("git {operation} returned non-UTF-8 or malformed output")]
    InvalidGitOutput { operation: &'static str },
}

/// A discovered git repository containing the four editable files.
#[derive(Debug, Clone)]
pub struct ConfigRepository {
    root: PathBuf,
    git_dir: PathBuf,
}

#[derive(Debug)]
struct ConfigOperationLock {
    _file: fs::File,
}

impl Drop for ConfigOperationLock {
    fn drop(&mut self) {
        let _ = self._file.unlock();
    }
}

impl ConfigRepository {
    /// Discover and canonicalize the git toplevel containing `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let requested = canonicalize("canonicalize repository", path.as_ref())?;
        let output = git_output_at(
            &requested,
            "discover repository",
            &["rev-parse", "--show-toplevel"],
        )?;
        let root = canonicalize("canonicalize git toplevel", Path::new(output.trim()))?;
        let git_dir = git_output_at(
            &root,
            "discover shared git directory",
            &["rev-parse", "--git-common-dir"],
        )?;
        let git_dir = Path::new(git_dir.trim());
        let git_dir = if git_dir.is_absolute() {
            git_dir.to_owned()
        } else {
            root.join(git_dir)
        };
        let git_dir = canonicalize("canonicalize shared git directory", &git_dir)?;
        Ok(Self { root, git_dir })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Inspect the current file to decide which editing affordance is legal.
    pub fn edit_route(&self, path: ConfigPath) -> Result<EditRoute, ConfigError> {
        let contents = self.read(path)?;
        Ok(route_for(path, &contents))
    }

    /// Read one allowlisted file under the same bound the form write enforces.
    pub fn read(&self, path: ConfigPath) -> Result<String, ConfigError> {
        let resolved = self.resolve(path)?;
        read_config(path, &resolved)
    }

    /// Mint the only schema token accepted by [`Self::commit_form`]. The
    /// current clean document is the canonical shape: forms may change values,
    /// but adding/removing fields is a structural edit and takes the editor
    /// route instead.
    pub fn form_schema(&self, path: ConfigPath) -> Result<ConfigFormSchema, ConfigError> {
        let lock = self.lock()?;
        self.require_clean(path)?;
        let baseline_contents = self.read(path)?;
        if route_for(path, &baseline_contents) == EditRoute::Editor {
            return Err(ConfigError::EditorRequired { path });
        }
        let template = parse_toml(path, &baseline_contents)?;
        Ok(ConfigFormSchema {
            path,
            template,
            baseline_head: self.config_head()?,
            baseline_contents,
            lock,
        })
    }

    /// Open the lock-backed generated form for one form-routed file.
    pub fn generated_form(&self, path: ConfigPath) -> Result<ConfigForm, ConfigError> {
        self.form_schema(path).and_then(ConfigForm::new)
    }

    /// Build the pure frame value from git, the files, and projected evidence.
    /// The caller supplies the last `config_change` evidence from its complete
    /// projection read; accepting an arbitrary page here would pretend a
    /// paginated slice can establish global recency.
    pub fn state(&self, last_evidence: Option<&Evidence>) -> Result<ConfigState, ConfigError> {
        let mut files = Vec::with_capacity(ConfigPath::ALL.len());
        for path in ConfigPath::ALL {
            let contents = self.read(path)?;
            files.push(ConfigFileState {
                path,
                route: route_for(path, &contents),
                contents,
            });
        }
        let config_head = self.config_head()?;
        let last_evidence_ref = config_evidence_ref(last_evidence)?.map(str::to_owned);
        let dirty = self.config_dirty()?;
        let divergent = dirty || config_head.as_deref() != last_evidence_ref.as_deref();
        Ok(ConfigState {
            files,
            config_head,
            last_evidence_ref,
            dirty,
            divergent,
        })
    }

    /// Write validated form output, commit exactly that file, then construct
    /// the evidence command for the resulting SHA.
    ///
    /// The schema-generated form owns field validation; this boundary owns the
    /// hard size/NUL bounds and re-checks both old and proposed text for the
    /// editor-only env source before any write occurs.
    pub fn commit_form(
        &self,
        form: ValidatedForm,
        commit_message: &str,
        evidence_id: EvidenceId,
    ) -> Result<KernelCommand, ConfigError> {
        let ValidatedForm {
            path,
            serialized: serialized_form,
            baseline_head,
            baseline_contents,
            lock: _lock,
        } = form;
        validate_config(path, &serialized_form)?;
        validate_commit_message(commit_message)?;
        self.require_clean(path)?;
        let resolved = self.resolve(path)?;
        let current = read_config(path, &resolved)?;
        if current != baseline_contents || self.config_head()? != baseline_head {
            return Err(ConfigError::ConcurrentChange { path });
        }
        if route_for(path, &current) == EditRoute::Editor
            || route_for(path, &serialized_form) == EditRoute::Editor
        {
            return Err(ConfigError::EditorRequired { path });
        }
        atomic_replace(&resolved, serialized_form.as_bytes())?;
        match self.commit(path, Some(commit_message), evidence_id) {
            Ok(command) => Ok(command),
            Err(error) => {
                self.restore(path, current.as_bytes())?;
                Err(error)
            }
        }
    }

    /// Open one allowlisted file in the exact executable named by `$EDITOR`,
    /// then commit only that path without `-m`, forcing git to ask the operator
    /// for the commit message.
    pub fn commit_editor(
        &self,
        path: ConfigPath,
        evidence_id: EvidenceId,
    ) -> Result<KernelCommand, ConfigError> {
        let _lock = self.lock()?;
        self.require_clean(path)?;
        let resolved = self.resolve(path)?;
        let current = read_config(path, &resolved)?;
        let baseline_head = self.config_head()?;
        let scratch = temporary_copy(&resolved, current.as_bytes())?;
        let editor = editor()?;
        let status = Command::new(editor)
            .current_dir(&self.root)
            .arg(scratch.path())
            .status()
            .map_err(|source| ConfigError::Io {
                operation: "launch $EDITOR",
                path: scratch.path().to_owned(),
                source,
            })?;
        if !status.success() {
            return Err(ConfigError::EditorFailed {
                path,
                code: status.code(),
            });
        }
        let edited = read_editor_output(path, scratch.path())?;
        validate_config(path, &edited)?;
        // The live path stayed untouched while the editor ran. Re-resolve it
        // immediately before replacement so an alias introduced meanwhile is
        // refused rather than followed.
        let resolved = self.resolve(path)?;
        if self.config_head()? != baseline_head || read_config(path, &resolved)? != current {
            return Err(ConfigError::ConcurrentChange { path });
        }
        self.require_clean(path).map_err(|error| match error {
            ConfigError::DirtyPath { .. } => ConfigError::ConcurrentChange { path },
            other => other,
        })?;
        persist(scratch, &resolved)?;
        match self.commit(path, None, evidence_id) {
            Ok(command) => Ok(command),
            Err(error) => {
                self.restore(path, current.as_bytes())?;
                Err(error)
            }
        }
    }

    /// Compare path-filtered git history to the newest `config_change`
    /// evidence. The returned command is submitted by the startup caller.
    pub fn reconcile_startup(
        &self,
        last_evidence: Option<&Evidence>,
        attention_item_id: AttentionItemId,
    ) -> Result<Option<KernelCommand>, ConfigError> {
        if self.config_dirty()? {
            return Ok(Some(divergence_attention(
                attention_item_id,
                "config paths have uncommitted changes outside config evidence".into(),
            )));
        }
        let config_head = self.config_head()?;
        let last_evidence = config_evidence_ref(last_evidence)?;
        if config_head.as_deref() == last_evidence {
            return Ok(None);
        }

        let summary = match (config_head.as_deref(), last_evidence) {
            (Some(head), Some(recorded)) => {
                format!("config commit {head} differs from recorded evidence {recorded}")
            }
            (Some(head), None) => format!("config commit {head} has no config_change evidence"),
            (None, Some(recorded)) => {
                format!("recorded config evidence {recorded} has no config commit")
            }
            (None, None) => return Ok(None),
        };
        Ok(Some(divergence_attention(attention_item_id, summary)))
    }

    fn lock(&self) -> Result<ConfigOperationLock, ConfigError> {
        let path = self.git_dir.join("gwk-config-lens.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| ConfigError::Io {
                operation: "open config edit lock",
                path: path.clone(),
                source,
            })?;
        match file.try_lock() {
            Ok(()) => Ok(ConfigOperationLock { _file: file }),
            Err(TryLockError::WouldBlock) => Err(ConfigError::EditInProgress),
            Err(TryLockError::Error(source)) => Err(ConfigError::Io {
                operation: "lock config edit",
                path,
                source,
            }),
        }
    }

    fn require_clean(&self, path: ConfigPath) -> Result<(), ConfigError> {
        let dirty = git_output_at(
            &self.root,
            "inspect config path status",
            &[
                "status",
                "--porcelain=v1",
                "--untracked-files=all",
                "-z",
                "--",
                path.as_str(),
            ],
        )?;
        if dirty.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::DirtyPath { path })
        }
    }

    fn config_dirty(&self) -> Result<bool, ConfigError> {
        let mut arguments = vec![
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "-z",
            "--",
        ];
        arguments.extend(ConfigPath::ALL.map(ConfigPath::as_str));
        Ok(!git_output_at(&self.root, "inspect config status", &arguments)?.is_empty())
    }

    fn restore(&self, path: ConfigPath, contents: &[u8]) -> Result<(), ConfigError> {
        let resolved = self.resolve(path)?;
        atomic_replace(&resolved, contents)?;
        git_status_at(
            &self.root,
            "unstage failed config commit",
            &["reset", "-q", "HEAD", "--", path.as_str()],
        )
    }

    fn resolve(&self, path: ConfigPath) -> Result<PathBuf, ConfigError> {
        let joined = self.root.join(path.as_str());
        let resolved = canonicalize("canonicalize config path", &joined)?;
        if !resolved.starts_with(&self.root) {
            return Err(ConfigError::OutsideRepository { path: resolved });
        }
        if resolved != joined {
            return Err(ConfigError::AliasedPath { path: joined });
        }
        let metadata = fs::metadata(&resolved).map_err(|source| ConfigError::Io {
            operation: "inspect config path",
            path: resolved.clone(),
            source,
        })?;
        if !metadata.is_file() {
            return Err(ConfigError::NotAFile { path: resolved });
        }
        Ok(resolved)
    }

    fn config_head(&self) -> Result<Option<String>, ConfigError> {
        let mut arguments = vec!["log", "-1", "--format=%H", "--"];
        arguments.extend(ConfigPath::ALL.map(ConfigPath::as_str));
        let revision = git_output_at(&self.root, "read config history", &arguments)?;
        let revision = revision.trim();
        if revision.is_empty() {
            return Ok(None);
        }
        if !is_revision(revision) {
            return Err(ConfigError::InvalidGitOutput {
                operation: "read config history",
            });
        }
        Ok(Some(revision.to_owned()))
    }

    fn commit(
        &self,
        path: ConfigPath,
        message: Option<&str>,
        evidence_id: EvidenceId,
    ) -> Result<KernelCommand, ConfigError> {
        git_status_at(&self.root, "stage config", &["add", "--", path.as_str()])?;
        match message {
            Some(message) => git_status_at(
                &self.root,
                "commit config",
                &["commit", "-m", message, "--", path.as_str()],
            )?,
            None => git_status_at(
                &self.root,
                "commit config with operator message",
                &["commit", "--", path.as_str()],
            )?,
        }
        // Ask the same path-filtered history startup reconciliation uses. An
        // unrelated concurrent commit may advance HEAD after ours; it must not
        // become the evidence ref for this config write.
        let revision = self.config_head()?.ok_or(ConfigError::InvalidGitOutput {
            operation: "read committed config revision",
        })?;
        Ok(KernelCommand::RecordEvidence {
            evidence_id,
            kind: CONFIG_CHANGE_KIND.to_owned(),
            r#ref: revision,
            digest: None,
            byte_size: None,
        })
    }
}

fn canonicalize(operation: &'static str, path: &Path) -> Result<PathBuf, ConfigError> {
    fs::canonicalize(path).map_err(|source| ConfigError::Io {
        operation,
        path: path.to_owned(),
        source,
    })
}

fn route_for(path: ConfigPath, contents: &str) -> EditRoute {
    if path == ConfigPath::AuthorityPolicy {
        return EditRoute::Editor;
    }
    match toml::from_str::<toml::Value>(contents) {
        Ok(value) if toml_sources_env(&value) => EditRoute::Editor,
        // An invalid incumbent needs the free-form repair path, not a form
        // whose schema assumes it can first decode the document.
        Err(_) => EditRoute::Editor,
        Ok(value) => {
            let mut fields = Vec::new();
            collect_form_fields(&value, &mut Vec::new(), &mut fields);
            match assignment_value_spans(path, contents) {
                Ok(spans)
                    if fields
                        .iter()
                        .all(|field| spans.contains_key(&field.segments)) =>
                {
                    EditRoute::Form
                }
                Ok(_) | Err(_) => EditRoute::Editor,
            }
        }
    }
}

fn toml_sources_env(value: &toml::Value) -> bool {
    match value {
        toml::Value::String(value) => string_sources_env(value),
        toml::Value::Array(values) => values.iter().any(toml_sources_env),
        toml::Value::Table(values) => values.values().any(toml_sources_env),
        toml::Value::Integer(_)
        | toml::Value::Float(_)
        | toml::Value::Boolean(_)
        | toml::Value::Datetime(_) => false,
    }
}

fn string_sources_env(value: &str) -> bool {
    let expanded = value
        .replace('\\', "/")
        .replace("${HOME}", "~")
        .replace("$HOME", "~")
        .replace(['\'', '"'], "");
    expanded
        .split(|character: char| {
            character.is_whitespace()
                || matches!(
                    character,
                    ';' | '|' | '&' | '(' | ')' | '[' | ']' | ',' | '='
                )
        })
        .filter(|part| !part.is_empty())
        .any(normalized_env_path)
}

fn normalized_env_path(candidate: &str) -> bool {
    let start = candidate.find("~/").or_else(|| candidate.find("/home/"));
    let Some(start) = start else {
        return false;
    };
    let mut components: Vec<&str> = Vec::new();
    for component in candidate[start..].split('/') {
        match component {
            "" | "." => {}
            ".." => {
                components.pop();
            }
            value => components.push(value),
        }
    }
    let normalized = components.join("/");
    normalized == ENV_SOURCE || normalized.ends_with("/.gridwork/env")
}

fn parse_toml(path: ConfigPath, contents: &str) -> Result<toml::Value, ConfigError> {
    toml::from_str(contents).map_err(|source| ConfigError::InvalidToml { path, source })
}

fn validate_shape(
    path: ConfigPath,
    field: &str,
    expected: &toml::Value,
    actual: &toml::Value,
) -> Result<(), ConfigError> {
    match (expected, actual) {
        (toml::Value::Table(expected), toml::Value::Table(actual)) => {
            for (name, expected_value) in expected {
                let nested = nested_field(field, name);
                let Some(actual_value) = actual.get(name) else {
                    return Err(ConfigError::SchemaMismatch {
                        path,
                        field: nested,
                        expected: value_kind(expected_value),
                        actual: "missing",
                    });
                };
                validate_shape(path, &nested, expected_value, actual_value)?;
            }
            for (name, actual_value) in actual {
                if !expected.contains_key(name) {
                    return Err(ConfigError::SchemaMismatch {
                        path,
                        field: nested_field(field, name),
                        expected: "absent",
                        actual: value_kind(actual_value),
                    });
                }
            }
            Ok(())
        }
        (toml::Value::Array(expected), toml::Value::Array(actual)) => {
            if expected.len() != actual.len() {
                return Err(ConfigError::SchemaMismatch {
                    path,
                    field: display_field(field).to_owned(),
                    expected: "same-length array",
                    actual: "different-length array",
                });
            }
            for (index, (item_schema, value)) in expected.iter().zip(actual).enumerate() {
                validate_shape(
                    path,
                    &format!("{}[{index}]", display_field(field)),
                    item_schema,
                    value,
                )?;
            }
            Ok(())
        }
        (expected, actual) if value_kind(expected) == value_kind(actual) => Ok(()),
        (expected, actual) => Err(ConfigError::SchemaMismatch {
            path,
            field: display_field(field).to_owned(),
            expected: value_kind(expected),
            actual: value_kind(actual),
        }),
    }
}

fn nested_field(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}.{name}")
    }
}

fn display_field(field: &str) -> &str {
    if field.is_empty() { "<root>" } else { field }
}

fn collect_form_fields(
    value: &toml::Value,
    path: &mut Vec<String>,
    fields: &mut Vec<ConfigFormField>,
) {
    if let toml::Value::Table(table) = value {
        for (name, value) in table {
            path.push(name.clone());
            collect_form_fields(value, path, fields);
            path.pop();
        }
        return;
    }

    let rendered = match value {
        toml::Value::String(value) => value.clone(),
        _ => value.to_string(),
    };
    fields.push(ConfigFormField {
        segments: path.clone(),
        path: path.join("."),
        kind: value_kind(value),
        original: rendered.clone(),
        value: rendered,
    });
}

fn parse_form_value(path: ConfigPath, field: &ConfigFormField) -> Result<toml::Value, ConfigError> {
    let invalid = || ConfigError::InvalidFormValue {
        path,
        field: field.path.clone(),
        expected: field.kind,
    };
    match field.kind {
        "string" => Ok(toml::Value::String(field.value.clone())),
        "integer" => field
            .value
            .parse::<i64>()
            .map(toml::Value::Integer)
            .map_err(|_| invalid()),
        "float" => field
            .value
            .parse::<f64>()
            .map(toml::Value::Float)
            .map_err(|_| invalid()),
        "boolean" => field
            .value
            .parse::<bool>()
            .map(toml::Value::Boolean)
            .map_err(|_| invalid()),
        "datetime" | "array" => {
            let document = format!("value = {}\n", field.value);
            parse_toml(path, &document)
                .ok()
                .and_then(|value| value.get("value").cloned())
                .filter(|value| value_kind(value) == field.kind)
                .ok_or_else(invalid)
        }
        _ => Err(invalid()),
    }
}

fn assignment_value_spans(
    path: ConfigPath,
    contents: &str,
) -> Result<BTreeMap<Vec<String>, Range<usize>>, ConfigError> {
    let bytes = contents.as_bytes();
    let mut spans = BTreeMap::new();
    let mut table = Vec::new();
    let mut index = 0usize;

    while index < bytes.len() {
        match bytes[index] {
            b' ' | b'\t' | b'\r' | b'\n' => {
                index += 1;
            }
            b'#' => index = next_line(bytes, index),
            b'[' => {
                let array_table = bytes.get(index + 1) == Some(&b'[');
                let open = if array_table { 2 } else { 1 };
                let close = find_header_close(bytes, index + open, array_table)
                    .ok_or_else(|| unsupported_form_source(path))?;
                table = parse_key_path(path, contents[index + open..close].trim())?;
                index = next_line(bytes, close + open);
            }
            _ => {
                let equals = find_assignment_equals(bytes, index)
                    .ok_or_else(|| unsupported_form_source(path))?;
                let mut logical = table.clone();
                logical.extend(parse_key_path(path, contents[index..equals].trim())?);
                let mut value_start = equals + 1;
                while matches!(bytes.get(value_start), Some(b' ' | b'\t')) {
                    value_start += 1;
                }
                let (value_end, next) = find_value_end(bytes, value_start);
                spans.insert(logical, value_start..value_end);
                index = next;
            }
        }
    }

    Ok(spans)
}

fn unsupported_form_source(path: ConfigPath) -> ConfigError {
    ConfigError::FormSourceMismatch {
        path,
        field: "<document>".to_owned(),
    }
}

fn next_line(bytes: &[u8], index: usize) -> usize {
    bytes[index..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(bytes.len(), |offset| index + offset + 1)
}

fn find_header_close(bytes: &[u8], mut index: usize, array_table: bool) -> Option<usize> {
    let mut quote = None;
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(delimiter) = quote {
            if delimiter == b'"' && byte == b'\\' && !escaped {
                escaped = true;
            } else {
                if byte == delimiter && !escaped {
                    quote = None;
                }
                escaped = false;
            }
        } else {
            match byte {
                b'"' | b'\'' => quote = Some(byte),
                b']' if !array_table || bytes.get(index + 1).is_some_and(|next| *next == b']') => {
                    return Some(index);
                }
                b'\n' => return None,
                _ => {}
            }
        }
        index += 1;
    }
    None
}

fn find_assignment_equals(bytes: &[u8], mut index: usize) -> Option<usize> {
    let mut quote = None;
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(delimiter) = quote {
            if delimiter == b'"' && byte == b'\\' && !escaped {
                escaped = true;
            } else {
                if byte == delimiter && !escaped {
                    quote = None;
                }
                escaped = false;
            }
        } else {
            match byte {
                b'"' | b'\'' => quote = Some(byte),
                b'=' => return Some(index),
                b'\n' | b'#' => return None,
                _ => {}
            }
        }
        index += 1;
    }
    None
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ValueQuote {
    Basic,
    Literal,
    MultilineBasic,
    MultilineLiteral,
}

fn find_value_end(bytes: &[u8], start: usize) -> (usize, usize) {
    let mut index = start;
    let mut square = 0usize;
    let mut curly = 0usize;
    let mut quote = None;
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(mode) = quote {
            match mode {
                ValueQuote::Basic => {
                    if byte == b'\\' && !escaped {
                        escaped = true;
                    } else {
                        if byte == b'"' && !escaped {
                            quote = None;
                        }
                        escaped = false;
                    }
                }
                ValueQuote::Literal => {
                    if byte == b'\'' {
                        quote = None;
                    }
                }
                ValueQuote::MultilineBasic => {
                    if bytes.get(index..index + 3) == Some(b"\"\"\"") && !escaped {
                        // TOML: up to two quotes adjacent to the closing
                        // delimiter are content — the delimiter is the last
                        // three of the run, so shift past the content quotes.
                        let mut extra = 0;
                        while extra < 2 && bytes.get(index + 3 + extra) == Some(&b'"') {
                            extra += 1;
                        }
                        quote = None;
                        index += 2 + extra;
                    } else {
                        escaped = byte == b'\\' && !escaped;
                    }
                }
                ValueQuote::MultilineLiteral => {
                    if bytes.get(index..index + 3) == Some(b"'''") {
                        let mut extra = 0;
                        while extra < 2 && bytes.get(index + 3 + extra) == Some(&b'\'') {
                            extra += 1;
                        }
                        quote = None;
                        index += 2 + extra;
                    }
                }
            }
            index += 1;
            continue;
        }

        match byte {
            b'"' if bytes.get(index..index + 3) == Some(b"\"\"\"") => {
                quote = Some(ValueQuote::MultilineBasic);
                index += 3;
            }
            b'\'' if bytes.get(index..index + 3) == Some(b"'''") => {
                quote = Some(ValueQuote::MultilineLiteral);
                index += 3;
            }
            b'"' => {
                quote = Some(ValueQuote::Basic);
                index += 1;
            }
            b'\'' => {
                quote = Some(ValueQuote::Literal);
                index += 1;
            }
            b'[' => {
                square += 1;
                index += 1;
            }
            b']' => {
                square = square.saturating_sub(1);
                index += 1;
            }
            b'{' => {
                curly += 1;
                index += 1;
            }
            b'}' => {
                curly = curly.saturating_sub(1);
                index += 1;
            }
            b'#' if square == 0 && curly == 0 => {
                return (trim_value_end(bytes, start, index), next_line(bytes, index));
            }
            b'#' => index = next_line(bytes, index),
            b'\n' if square == 0 && curly == 0 => {
                return (trim_value_end(bytes, start, index), index + 1);
            }
            _ => index += 1,
        }
    }
    (trim_value_end(bytes, start, bytes.len()), bytes.len())
}

fn trim_value_end(bytes: &[u8], start: usize, mut end: usize) -> usize {
    while end > start && matches!(bytes[end - 1], b' ' | b'\t' | b'\r') {
        end -= 1;
    }
    end
}

fn parse_key_path(path: ConfigPath, key: &str) -> Result<Vec<String>, ConfigError> {
    let parsed = parse_toml(path, &format!("{key} = 0\n"))?;
    let mut segments = Vec::new();
    let mut value = &parsed;
    while let toml::Value::Table(table) = value {
        let Some((name, nested)) = table.iter().next() else {
            break;
        };
        segments.push(name.clone());
        value = nested;
    }
    Ok(segments)
}

fn value_kind(value: &toml::Value) -> &'static str {
    match value {
        toml::Value::String(_) => "string",
        toml::Value::Integer(_) => "integer",
        toml::Value::Float(_) => "float",
        toml::Value::Boolean(_) => "boolean",
        toml::Value::Datetime(_) => "datetime",
        toml::Value::Array(_) => "array",
        toml::Value::Table(_) => "table",
    }
}

fn validate_config(path: ConfigPath, contents: &str) -> Result<(), ConfigError> {
    if contents.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError::ConfigTooLarge {
            path,
            limit: MAX_CONFIG_BYTES,
        });
    }
    if contents.contains('\0') {
        return Err(ConfigError::ConfigContainsNul { path });
    }
    parse_toml(path, contents).map(|_| ())
}

fn read_config(path: ConfigPath, resolved: &Path) -> Result<String, ConfigError> {
    let metadata = fs::metadata(resolved).map_err(|source| ConfigError::Io {
        operation: "inspect config size",
        path: resolved.to_owned(),
        source,
    })?;
    if metadata.len() > MAX_CONFIG_BYTES as u64 {
        return Err(ConfigError::ConfigTooLarge {
            path,
            limit: MAX_CONFIG_BYTES,
        });
    }
    let bytes = fs::read(resolved).map_err(|source| ConfigError::Io {
        operation: "read config",
        path: resolved.to_owned(),
        source,
    })?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError::ConfigTooLarge {
            path,
            limit: MAX_CONFIG_BYTES,
        });
    }
    let contents = String::from_utf8(bytes).map_err(|_| ConfigError::ConfigNotUtf8 { path })?;
    if contents.contains('\0') {
        return Err(ConfigError::ConfigContainsNul { path });
    }
    Ok(contents)
}

fn read_editor_output(path: ConfigPath, scratch: &Path) -> Result<String, ConfigError> {
    let metadata = fs::symlink_metadata(scratch).map_err(|source| ConfigError::Io {
        operation: "inspect editor output",
        path: scratch.to_owned(),
        source,
    })?;
    if !metadata.file_type().is_file() {
        return Err(ConfigError::NotAFile {
            path: scratch.to_owned(),
        });
    }
    read_config(path, scratch)
}

fn temporary_copy(target: &Path, contents: &[u8]) -> Result<tempfile::NamedTempFile, ConfigError> {
    let parent = target
        .parent()
        .ok_or_else(|| ConfigError::OutsideRepository {
            path: target.to_owned(),
        })?;
    let permissions = fs::metadata(target)
        .map_err(|source| ConfigError::Io {
            operation: "inspect config permissions",
            path: target.to_owned(),
            source,
        })?
        .permissions();
    let mut scratch = tempfile::Builder::new()
        .prefix(".gwk-config-")
        .tempfile_in(parent)
        .map_err(|source| ConfigError::Io {
            operation: "create config scratch file",
            path: parent.to_owned(),
            source,
        })?;
    scratch
        .as_file()
        .set_permissions(permissions)
        .map_err(|source| ConfigError::Io {
            operation: "set config scratch permissions",
            path: scratch.path().to_owned(),
            source,
        })?;
    scratch
        .write_all(contents)
        .map_err(|source| ConfigError::Io {
            operation: "write config scratch file",
            path: scratch.path().to_owned(),
            source,
        })?;
    scratch
        .as_file()
        .sync_all()
        .map_err(|source| ConfigError::Io {
            operation: "sync config scratch file",
            path: scratch.path().to_owned(),
            source,
        })?;
    Ok(scratch)
}

fn persist(scratch: tempfile::NamedTempFile, target: &Path) -> Result<(), ConfigError> {
    scratch
        .persist(target)
        .map(|_| ())
        .map_err(|failure| ConfigError::Io {
            operation: "replace config file",
            path: target.to_owned(),
            source: failure.error,
        })
}

fn atomic_replace(target: &Path, contents: &[u8]) -> Result<(), ConfigError> {
    let scratch = temporary_copy(target, contents)?;
    persist(scratch, target)
}

fn divergence_attention(attention_item_id: AttentionItemId, summary: String) -> KernelCommand {
    KernelCommand::RaiseAttention {
        attention_item_id,
        kind: DIVERGENCE_KIND.to_owned(),
        summary,
        subject_ref: Some(DIVERGENCE_SUBJECT.to_owned()),
        raised_by: None,
        priority: Some(0),
    }
}

fn config_evidence_ref(evidence: Option<&Evidence>) -> Result<Option<&str>, ConfigError> {
    match evidence {
        Some(item) if item.kind == CONFIG_CHANGE_KIND => Ok(Some(item.r#ref.as_str())),
        Some(item) => Err(ConfigError::WrongEvidenceKind {
            kind: item.kind.clone(),
        }),
        None => Ok(None),
    }
}

fn validate_commit_message(message: &str) -> Result<(), ConfigError> {
    if message.trim().is_empty()
        || message.len() > MAX_COMMIT_MESSAGE_BYTES
        || !message
            .bytes()
            .all(|byte| byte.is_ascii_graphic() || byte == b' ')
    {
        return Err(ConfigError::InvalidCommitMessage {
            limit: MAX_COMMIT_MESSAGE_BYTES,
        });
    }
    Ok(())
}

fn editor() -> Result<OsString, ConfigError> {
    std::env::var_os("EDITOR")
        .filter(|value| !value.is_empty())
        .ok_or(ConfigError::EditorNotConfigured)
}

fn git_command(root: &Path) -> Command {
    const SELECTORS: &[&str] = &[
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CEILING_DIRECTORIES",
        "GIT_COMMON_DIR",
        "GIT_CONFIG",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_NOSYSTEM",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_SYSTEM",
        "GIT_DIR",
        "GIT_DISCOVERY_ACROSS_FILESYSTEM",
        "GIT_EXEC_PATH",
        "GIT_INDEX_FILE",
        "GIT_NAMESPACE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_WORK_TREE",
    ];
    let mut command = Command::new("git");
    command.current_dir(root);
    for variable in SELECTORS {
        command.env_remove(variable);
    }
    for (variable, _) in std::env::vars_os() {
        let name = variable.to_string_lossy();
        if name.starts_with("GIT_CONFIG_KEY_") || name.starts_with("GIT_CONFIG_VALUE_") {
            command.env_remove(variable);
        }
    }
    command
}

fn git_output_at(
    root: &Path,
    operation: &'static str,
    arguments: &[&str],
) -> Result<String, ConfigError> {
    let output = git_command(root)
        .args(arguments)
        .output()
        .map_err(|source| ConfigError::Io {
            operation,
            path: root.to_owned(),
            source,
        })?;
    require_success(operation, output.status)?;
    String::from_utf8(output.stdout).map_err(|_| ConfigError::InvalidGitOutput { operation })
}

fn git_status_at(
    root: &Path,
    operation: &'static str,
    arguments: &[&str],
) -> Result<(), ConfigError> {
    let status = git_command(root)
        .args(arguments)
        .status()
        .map_err(|source| ConfigError::Io {
            operation,
            path: root.to_owned(),
            source,
        })?;
    require_success(operation, status)
}

fn require_success(operation: &'static str, status: ExitStatus) -> Result<(), ConfigError> {
    if status.success() {
        Ok(())
    } else {
        Err(ConfigError::GitFailed {
            operation,
            code: status.code(),
        })
    }
}

fn is_revision(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Paint the generated editor for one locked form session.
pub fn render_form(area: Rect, buffer: &mut Buffer, form: &ConfigForm, tier: ColorTier) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let field_style = config_style("gws_fg", tier);
    let muted = config_style("gws_muted", tier);
    let focus = config_style("gws_focus", tier).add_modifier(Modifier::BOLD);
    let warning = config_style("gws_warn", tier);

    form_put(
        buffer,
        area,
        1,
        0,
        "FORM",
        field_style.add_modifier(Modifier::BOLD),
    );
    form_put(
        buffer,
        area,
        8,
        0,
        &format!("{}   shape is fixed by the incumbent file", form.path()),
        muted,
    );
    form_put(buffer, area, 2, 2, "FIELD", muted);
    form_put(buffer, area, 34, 2, "TYPE", muted);
    form_put(buffer, area, 44, 2, "VALUE", muted);

    let capacity = usize::from(area.height.saturating_sub(12)).max(1);
    let field_selected = form.selected().min(form.fields().len().saturating_sub(1));
    let start = field_selected
        .saturating_sub(capacity.saturating_sub(1))
        .min(form.fields().len().saturating_sub(capacity));
    let end = (start + capacity).min(form.fields().len());
    let mut y = 3u16;
    for (index, field) in form.fields()[start..end].iter().enumerate() {
        let absolute = start + index;
        let selected = !form.commit_selected() && form.selected() == absolute;
        form_put(buffer, area, 0, y, if selected { ">" } else { " " }, focus);
        form_put(buffer, area, 2, y, field.path(), field_style);
        form_put(buffer, area, 34, y, field.kind(), muted);
        form_put(
            buffer,
            area,
            44,
            y,
            field.value(),
            if selected { focus } else { field_style },
        );
        if selected {
            // The painted value goes through safe_text, which expands
            // non-admissible chars into multi-cell \u{HEX} escapes; the
            // cursor rides the painted width, not the raw char count.
            let painted = theme::safe_text(field.value(), area.width.saturating_sub(44) as usize);
            form_put(
                buffer,
                area,
                44u16.saturating_add(u16::try_from(painted.chars().count()).unwrap_or(u16::MAX)),
                y,
                "_",
                focus,
            );
        }
        if field.changed() {
            form_put(
                buffer,
                area,
                area.width.saturating_sub(16),
                y,
                "changed",
                warning,
            );
        }
        y = y.saturating_add(1);
    }
    if start > 0 || end < form.fields().len() {
        // The ruled `+` grammar states real omitted counts — never "+0".
        let more = form.fields().len() - end;
        let window = if start == 0 {
            format!("+{more} more")
        } else if more == 0 {
            format!("+{start} before")
        } else {
            format!("+{start} before  +{more} more")
        };
        form_put(buffer, area, 2, y, &window, muted);
        y = y.saturating_add(1);
    }

    y = y.saturating_add(1);
    form_put(
        buffer,
        area,
        2,
        y,
        "no field can be added or removed -- the validator rejects shape changes",
        muted,
    );
    y = y.saturating_add(2);
    form_put(
        buffer,
        area,
        0,
        y,
        if form.commit_selected() { ">" } else { " " },
        focus,
    );
    form_put(
        buffer,
        area,
        2,
        y,
        "COMMIT",
        field_style.add_modifier(Modifier::BOLD),
    );
    y = y.saturating_add(1);
    form_put(buffer, area, 2, y, form.commit_message(), field_style);
    if form.commit_selected() {
        let painted =
            theme::safe_text(form.commit_message(), area.width.saturating_sub(2) as usize);
        form_put(
            buffer,
            area,
            2u16.saturating_add(u16::try_from(painted.chars().count()).unwrap_or(u16::MAX)),
            y,
            "_",
            focus,
        );
    }
    y = y.saturating_add(1);
    form_put(
        buffer,
        area,
        2,
        y,
        "one file, one commit, one config_change evidence record",
        muted,
    );
    form_put(
        buffer,
        area,
        1,
        area.height.saturating_sub(1),
        "tab field   enter commit   esc cancel   exclusive lock is held; concurrent edits are refused",
        muted,
    );
}

fn form_put(buffer: &mut Buffer, area: Rect, x: u16, y: u16, text: &str, paint: Style) {
    if x >= area.width || y >= area.height {
        return;
    }
    let width = area.width.saturating_sub(x) as usize;
    let safe = theme::safe_text(text, width);
    buffer.set_stringn(area.x + x, area.y + y, safe.as_ref(), width, paint);
}

fn config_style(name: &str, tier: ColorTier) -> Style {
    let token = gwk_theme::SIGNAL
        .iter()
        .find(|token| token.name == name)
        .expect("the SIGNAL token inventory is pinned");
    theme::token_style(token, tier)
}

/// Every actionable target in stable keyboard order.
pub fn target_order(state: &ConfigState) -> Vec<ConfigTarget> {
    state
        .files
        .iter()
        .map(|file| ConfigTarget::File(file.path))
        .collect()
}

struct Row {
    binding: &'static StateBinding,
    text: String,
    right: Option<&'static str>,
    target: Option<ConfigTarget>,
}

fn rows(state: &ConfigState) -> Vec<Row> {
    let mut rows = Vec::with_capacity(state.files.len() + 1);
    if state.dirty {
        rows.push(Row {
            binding: theme::binding("needs_attention"),
            text: "dirty -- uncommitted config requires attention".into(),
            right: None,
            target: None,
        });
    } else if state.divergent {
        rows.push(Row {
            binding: theme::binding("needs_attention"),
            text: "diverged -- startup attention is required".into(),
            right: None,
            target: None,
        });
    } else if state.config_head.is_none() {
        rows.push(Row {
            binding: theme::binding("idle"),
            text: "no config commits or evidence".into(),
            right: None,
            target: None,
        });
    } else {
        rows.push(Row {
            binding: theme::binding("done"),
            text: "in sync -- latest config commit has evidence".into(),
            right: None,
            target: None,
        });
    }

    for file in &state.files {
        rows.push(Row {
            binding: theme::binding("idle"),
            text: file.path.as_str().to_owned(),
            right: Some(file.route.as_str()),
            target: Some(ConfigTarget::File(file.path)),
        });
    }
    rows
}

/// Paint the Config lens inside `area` and register every visible file row.
/// Hidden rows remain reachable through [`target_order`], matching the Board's
/// windowed-lens keyboard/mouse contract.
pub fn render(
    area: Rect,
    buffer: &mut Buffer,
    state: &ConfigState,
    selected: Option<&ConfigTarget>,
    tier: ColorTier,
    glyphs: GlyphSet,
    hits: &mut HitMap<ConfigTarget>,
) {
    hits.clear();
    if area.width == 0 || area.height == 0 {
        return;
    }

    let built = rows(state);
    let right_edge = area.x.saturating_add(area.width);
    let body_height = area.height - 1;
    let pinned = usize::from(body_height > 1);
    let file_rows = &built[1..];
    let file_height = body_height.saturating_sub(pinned as u16);
    let needs_notice = file_rows.len() > file_height as usize && file_height > 1;
    let visible = (file_height as usize)
        .saturating_sub(usize::from(needs_notice))
        .min(file_rows.len());
    let selected_row = selected.and_then(|target| {
        file_rows
            .iter()
            .position(|row| row.target.as_ref() == Some(target))
    });
    let start = if visible == 0 {
        0
    } else {
        selected_row
            .filter(|index| *index >= visible)
            .map_or(0, |index| index + 1 - visible)
            .min(file_rows.len().saturating_sub(visible))
    };
    let end = (start + visible).min(file_rows.len());
    let selection_style = gwk_theme::SIGNAL
        .iter()
        .find(|token| token.name == "gws_selection")
        .map(|token| theme::token_style(token, tier));

    if pinned == 1 {
        paint_row(
            area,
            right_edge,
            area.y,
            buffer,
            &built[0],
            false,
            selection_style,
            tier,
            glyphs,
        );
    }

    for (index, row) in file_rows[start..end].iter().enumerate() {
        let y = area
            .y
            .saturating_add(pinned as u16)
            .saturating_add(u16::try_from(index).unwrap_or(u16::MAX));
        let selected_row = matches!(
            (selected, &row.target),
            (Some(selected), Some(target)) if selected == target
        );
        paint_row(
            area,
            right_edge,
            y,
            buffer,
            row,
            selected_row,
            selection_style,
            tier,
            glyphs,
        );
        if let Some(target) = &row.target {
            hits.register(Rect::new(area.x, y, area.width, 1), target.clone());
        }
    }

    if needs_notice {
        let y = area
            .y
            .saturating_add(pinned as u16)
            .saturating_add(file_height.saturating_sub(1));
        let notice = if start == 0 {
            format!("+{} more", file_rows.len() - end)
        } else if end == file_rows.len() {
            format!("+{start} before")
        } else {
            format!("+{start} before  +{} more", file_rows.len() - end)
        };
        let safe_notice = theme::safe_text(&notice, area.width.saturating_sub(1) as usize);
        buffer.set_stringn(
            area.x.saturating_add(1),
            y,
            safe_notice.as_ref(),
            area.width.saturating_sub(1) as usize,
            theme::state_style(theme::binding("idle"), tier),
        );
    }

    let status = format!(
        "CONFIG  {} files  commit {}  evidence {}{}",
        state.files.len(),
        short_ref(state.config_head.as_deref()),
        short_ref(state.last_evidence_ref.as_deref()),
        if state.divergent {
            "  ! divergence"
        } else {
            ""
        }
    );
    let safe_status = theme::safe_text(&status, area.width as usize);
    buffer.set_stringn(
        area.x,
        area.y.saturating_add(area.height - 1),
        safe_status.as_ref(),
        area.width as usize,
        Style::default(),
    );
}

#[allow(clippy::too_many_arguments)]
fn paint_row(
    area: Rect,
    right_edge: u16,
    y: u16,
    buffer: &mut Buffer,
    row: &Row,
    selected: bool,
    selection_style: Option<Style>,
    tier: ColorTier,
    glyphs: GlyphSet,
) {
    let row_style = theme::state_style(row.binding, tier);
    let text_style = match (selected, selection_style) {
        (true, Some(selection)) => row_style.patch(selection),
        _ => row_style,
    };
    if selected {
        buffer.set_string(
            area.x,
            y,
            ">",
            Style::default().add_modifier(Modifier::BOLD),
        );
    }

    let mark = gwk_theme::marks::mark(row.binding.mark)
        .expect("the state binding mark inventory is pinned");
    let mark_x = area.x.saturating_add(1);
    let mark_budget = right_edge.saturating_sub(mark_x);
    buffer.set_stringn(
        mark_x,
        y,
        theme::glyph(mark, 0, glyphs).to_string(),
        mark_budget as usize,
        row_style,
    );
    let text_x = mark_x.saturating_add(2);
    let text_budget = right_edge.saturating_sub(text_x);
    let safe_text = theme::safe_text(&row.text, text_budget as usize);
    buffer.set_stringn(
        text_x,
        y,
        safe_text.as_ref(),
        text_budget as usize,
        text_style,
    );

    if let Some(right) = row.right {
        crate::row::paint_tail(
            buffer,
            area,
            y,
            text_x.saturating_add(u16::try_from(safe_text.chars().count()).unwrap_or(u16::MAX)),
            right,
            text_style,
        );
    }
}

fn short_ref(value: Option<&str>) -> &str {
    value.map_or("-", |revision| revision.get(..8).unwrap_or(revision))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_adjacent_closing_delimiters_stay_inside_their_own_value() {
        // TOML permits one or two quotes adjacent to a multiline closing
        // delimiter as content; a scanner that closes on the FIRST run of
        // three swallows every following assignment into the first span.
        for contents in [
            "a = \"\"\"x\"\"\"\"\nb = 1\n",
            "a = \"\"\"x\"\"\"\"\"\nb = 1\n",
            "a = '''x''''\nb = 1\n",
            "a = '''x'''''\nb = 1\n",
        ] {
            let spans = assignment_value_spans(ConfigPath::Capabilities, contents).expect("spans");
            assert!(
                spans.contains_key(&vec!["b".to_owned()]),
                "second assignment lost in {contents:?}"
            );
        }
    }
}
