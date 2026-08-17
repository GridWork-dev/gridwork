//! Portable Agent Skill manifests, parsed asymmetrically.
//!
//! A skill arrives as third-party YAML frontmatter authored by somebody else,
//! for a format somebody else versions. Two pressures point in opposite
//! directions: a field this parser does not recognize might be legitimate
//! upstream drift, and it might be a caller inventing contract inside a
//! namespace we own. Ruling R13 resolves that by refusing to answer both with
//! one policy.
//!
//! - **Portable core** — unknown fields are captured as bounded opaque
//!   evidence. Never dropped, because a dropped field is a silent difference
//!   between what the author wrote and what the compiler saw. Never trusted,
//!   because nothing branches on them. Never fatal, because upstream shipping a
//!   new field is not an attack.
//! - **The GridWork namespace** — [`GridworkExt`] is `deny_unknown_fields`. A
//!   key we do not know inside a namespace we own is a version skew or an
//!   invention, and both should be loud (R35).
//!
//! The extension rides as a bounded JSON string at `metadata.gridwork`, which
//! is closer to forced than chosen: upstream `metadata` is `dict[str, str]`, so
//! structured extension data is either one nested parse or a hand-rolled dotted
//! key convention that would need its own unknown-key rejection — serde's
//! `deny_unknown_fields` cannot govern keys collected out of a map, and pairing
//! it with `flatten` is unsound. A top-level `x-gridwork` key is unavailable:
//! the reference validator's field set is exhaustive and hard-errors on any
//! other top-level key.
//!
//! ## What this module does not do
//!
//! It does not read the filesystem, spawn anything, or sandbox anything.
//! [`BundleEntry::classify`] takes the entry kind from its caller precisely so
//! that parsing a hostile manifest cannot itself become the thing that touches
//! a hostile tree. It also does not decide authority: `allowed-tools` becomes
//! [`AllowedToolsEvidence`], which is a record of what a manifest *claimed* and
//! exposes no way to become a grant. Authority is resolved upstream of context
//! compilation and context may narrow it, never widen it.
//!
//! A manifest's own claim of first-party authorship is ignored here. Origin is
//! a caller-owned input; R29 enforces it in 8C.
//!
//! ## What sits underneath, and why the subset gate is first
//!
//! `yaml_serde` parses through `libyaml-rs`, which is libyaml transpiled from C
//! by c2rust. This workspace forbids `unsafe_code` in its own crates and that
//! rule does not reach a dependency: the bytes of a hostile skill manifest end
//! up inside machine-translated C, keeping C's memory model without C's decades
//! of fuzzing aimed at this particular artifact. Every serde-shaped YAML crate
//! in Rust shares that lineage, so this is the state of the art rather than a
//! poor pick — but it is worth writing down where someone weighing the trust
//! boundary will read it.
//!
//! It is also the second reason `scan_subset` runs before the deserializer
//! rather than after it. The first is that the accepted subset becomes a
//! property of this file instead of a property of whichever YAML crate is
//! pinned — and that is not theoretical here. Removing the duplicate-key check
//! from the scan makes the duplicate-key test pass, which is to say `yaml_serde`
//! accepts a repeated key and keeps the last one, silently. The second reason is
//! that the gate bounds what reaches the transpiled parser at all: no anchors,
//! no aliases, no merge keys, no tags, no directives, and at most
//! [`SKILL_FRONTMATTER_MAX_BYTES`]. It narrows the surface; it does not remove
//! it.

use std::collections::{BTreeMap, BTreeSet};

/// Maximum bytes in a whole `SKILL.md` before the parser refuses to look.
pub const SKILL_INPUT_MAX_BYTES: usize = 64 * 1024;
/// Maximum bytes in the YAML frontmatter block alone.
pub const SKILL_FRONTMATTER_MAX_BYTES: usize = 8 * 1024;
/// Maximum bytes in a skill name.
pub const SKILL_NAME_MAX_BYTES: usize = 64;
/// Maximum bytes in a skill description.
pub const SKILL_DESCRIPTION_MAX_BYTES: usize = 1024;
/// Maximum bytes in the `license` field.
pub const SKILL_LICENSE_MAX_BYTES: usize = 128;
/// Maximum `metadata` entries, `gridwork` included.
pub const SKILL_METADATA_MAX_ENTRIES: usize = 32;
/// Maximum bytes in one metadata key.
pub const SKILL_METADATA_KEY_MAX_BYTES: usize = 64;
/// Maximum bytes in one metadata value, including the GridWork JSON string.
pub const SKILL_METADATA_VALUE_MAX_BYTES: usize = 4 * 1024;
/// Maximum entries in `allowed-tools`.
pub const SKILL_ALLOWED_TOOLS_MAX_COUNT: usize = 64;
/// Maximum bytes in one `allowed-tools` entry.
pub const SKILL_ALLOWED_TOOL_MAX_BYTES: usize = 128;
/// Maximum unrecognized portable-core fields kept as opaque evidence.
pub const SKILL_OPAQUE_FIELD_MAX_COUNT: usize = 32;
/// Maximum bytes in one opaque field's rendered value.
pub const SKILL_OPAQUE_VALUE_MAX_BYTES: usize = 1024;
/// Maximum open nesting levels inside the frontmatter mapping.
///
/// Levels, not indentation columns. Measuring it as a column count divided by
/// an assumed two-space step made the bound a property of the author's
/// formatting: a one-space ladder reached four levels against this two while
/// its two-space twin was refused at three.
pub const SKILL_MAX_NESTING_DEPTH: usize = 2;
/// Maximum bundle entries inventoried for one skill.
pub const SKILL_BUNDLE_MAX_ENTRIES: usize = 256;
/// Maximum bytes in a bundle entry's relative path.
pub const SKILL_BUNDLE_PATH_MAX_BYTES: usize = 256;

/// The exhaustive upstream portable-core field set.
///
/// Exhaustive is the operative word and it is why `x-gridwork` is not an
/// option: the reference validator hard-errors on any top-level key outside
/// this list, so an extension key alongside them is rejected before this parser
/// ever sees the document. Fields absent here are still *accepted* — they land
/// in [`SkillManifest::opaque`] — because this list records what upstream
/// defines today, not what upstream is allowed to define tomorrow.
pub const PORTABLE_CORE_FIELDS: [&str; 6] = [
    "allowed-tools",
    "compatibility",
    "description",
    "license",
    "metadata",
    "name",
];

/// The one metadata key GridWork owns.
pub const GRIDWORK_METADATA_KEY: &str = "gridwork";

/// Why a skill manifest was refused.
///
/// Closed, and each variant names one refusal rather than one location, so a
/// caller can branch on what was wrong without parsing a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillError {
    /// The input exceeds [`SKILL_INPUT_MAX_BYTES`].
    InputTooLarge,
    /// No `---` frontmatter block, or it is never closed.
    MissingFrontmatter,
    /// The frontmatter block exceeds [`SKILL_FRONTMATTER_MAX_BYTES`].
    FrontmatterTooLarge,
    /// A C0/C1 control character outside tab and newline.
    ControlCharacter,
    /// A tab used for indentation, which YAML forbids outright.
    TabIndentation,
    /// The same top-level key appears twice.
    DuplicateKey(String),
    /// An anchor, alias, tag, merge key, or directive.
    UnsupportedYaml(&'static str),
    /// Mapping nesting beyond [`SKILL_MAX_NESTING_DEPTH`].
    TooDeeplyNested,
    /// A top-level line that is not `key:` at column zero.
    MalformedTopLevelKey,
    /// The YAML did not decode into the expected shape.
    Malformed(String),
    /// `name` is empty, oversized, or outside `[a-z0-9-]`.
    InvalidName,
    /// `name` disagrees with the directory containing the manifest.
    NameDirectoryMismatch,
    /// A bounded string field exceeded its limit.
    FieldTooLong(&'static str),
    /// A count field exceeded its limit.
    TooManyEntries(&'static str),
    /// A `metadata` value that is not a string.
    MetadataValueNotString(String),
    /// `metadata.gridwork` is not valid JSON, or carries an unknown key.
    InvalidGridworkExtension(String),
    /// A bundle entry that is not declarative content.
    RefusedBundleEntry(BundleRefusal),
}

impl std::fmt::Display for SkillError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InputTooLarge => f.write_str("skill manifest exceeds its byte bound"),
            Self::MissingFrontmatter => {
                f.write_str("skill manifest has no closed `---` frontmatter block")
            }
            Self::FrontmatterTooLarge => f.write_str("skill frontmatter exceeds its byte bound"),
            Self::ControlCharacter => f.write_str("skill frontmatter carries a control character"),
            Self::TabIndentation => f.write_str("skill frontmatter indents with a tab"),
            Self::DuplicateKey(key) => write!(f, "skill frontmatter repeats the key `{key}`"),
            Self::UnsupportedYaml(what) => {
                write!(
                    f,
                    "skill frontmatter uses an unsupported YAML feature: {what}"
                )
            }
            Self::TooDeeplyNested => f.write_str("skill frontmatter nests beyond its depth bound"),
            Self::MalformedTopLevelKey => {
                f.write_str("skill frontmatter has a top-level line that is not `key:`")
            }
            Self::Malformed(why) => write!(f, "skill frontmatter did not decode: {why}"),
            Self::InvalidName => {
                f.write_str("skill name must be 1-64 bytes of lowercase letters, digits, and `-`")
            }
            Self::NameDirectoryMismatch => {
                f.write_str("skill name must match the directory that contains it")
            }
            Self::FieldTooLong(field) => write!(f, "skill field `{field}` exceeds its byte bound"),
            Self::TooManyEntries(field) => write!(f, "skill field `{field}` has too many entries"),
            Self::MetadataValueNotString(key) => {
                write!(f, "skill metadata value for `{key}` is not a string")
            }
            Self::InvalidGridworkExtension(why) => {
                write!(
                    f,
                    "skill `metadata.gridwork` is not a valid extension: {why}"
                )
            }
            Self::RefusedBundleEntry(refusal) => write!(f, "skill bundle entry refused: {refusal}"),
        }
    }
}

impl std::error::Error for SkillError {}

/// What a manifest claimed under `allowed-tools`.
///
/// A newtype with no conversion, and the absence is the design. Upstream treats
/// this field as a permission list; here it is one third party's claim about
/// itself, recorded so Explain can show it and a reviewer can read it. Anything
/// that turned it into a grant would let a manifest widen its own authority by
/// asserting it, which inverts the direction D3 fixes: context narrows
/// authority, never widens it. There is deliberately no `into_grants`, no
/// `Deref`, and no `IntoIterator` yielding a capability type.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct AllowedToolsEvidence {
    claimed: Vec<String>,
}

impl AllowedToolsEvidence {
    /// The raw claimed strings, for display and review only.
    pub fn claimed(&self) -> &[String] {
        &self.claimed
    }

    /// How many tools the manifest claimed.
    pub fn len(&self) -> usize {
        self.claimed.len()
    }

    /// True when the manifest claimed no tools.
    pub fn is_empty(&self) -> bool {
        self.claimed.is_empty()
    }
}

/// One unrecognized portable-core field, kept rather than dropped.
///
/// `value` is a rendered, bounded, lossy string — the point is evidence that
/// the field was present and roughly what it said, not a re-parsable copy.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OpaquePortableField {
    pub key: String,
    pub value: String,
}

/// GridWork's own extension block, decoded from `metadata.gridwork`.
///
/// `deny_unknown_fields` is the whole asymmetry: this namespace is ours, so an
/// unrecognized key here is never upstream drift.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GridworkExt {
    /// Route names this skill declares itself eligible for. Advisory: 8C
    /// decides eligibility, this only records the claim.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<String>,
    /// A declared token budget hint, if the author stated one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u32>,
    /// Free-text note carried into Explain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// A parsed portable skill manifest.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SkillManifest {
    pub name: String,
    pub description: String,
    pub license: Option<String>,
    pub compatibility: Option<String>,
    /// Claimed, never granted. See [`AllowedToolsEvidence`].
    pub allowed_tools: AllowedToolsEvidence,
    /// Upstream metadata with the GridWork key removed — it is decoded into
    /// `gridwork` instead of sitting here twice under two interpretations.
    pub metadata: BTreeMap<String, String>,
    /// Present only when the author wrote `metadata.gridwork`.
    pub gridwork: Option<GridworkExt>,
    /// Unrecognized portable-core fields, in key order.
    pub opaque: Vec<OpaquePortableField>,
}

/// The kind of a bundle entry, as reported by whatever walked the tree.
///
/// Supplied by the caller rather than read here: classification must be able to
/// refuse a symlink or a device without this module ever having touched one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawEntryKind {
    File { executable: bool },
    Directory,
    Symlink,
    Other,
}

/// Why a bundle entry is not declarative content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BundleRefusal {
    /// `..` anywhere in the path.
    ParentTraversal,
    /// A leading `/`, or a Windows-style drive prefix.
    AbsolutePath,
    /// A symlink, device, socket, or fifo.
    NotARegularFile,
    /// The executable bit is set.
    Executable,
    /// A suffix this parser knows to be code.
    ExecutableSuffix(String),
    /// A suffix with no declarative meaning assigned.
    Unclassified(String),
    /// The path exceeds [`SKILL_BUNDLE_PATH_MAX_BYTES`], is empty, or carries a
    /// control character or a `\` separator.
    MalformedPath,
}

impl std::fmt::Display for BundleRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ParentTraversal => f.write_str("path escapes the skill directory"),
            Self::AbsolutePath => f.write_str("path is absolute"),
            Self::NotARegularFile => f.write_str("entry is not a regular file"),
            Self::Executable => f.write_str("entry is executable"),
            Self::ExecutableSuffix(s) => write!(f, "`{s}` is code, not declarative content"),
            Self::Unclassified(s) => write!(f, "`{s}` has no declared content kind"),
            Self::MalformedPath => f.write_str("path is empty, oversized, or malformed"),
        }
    }
}

/// What a bundle entry is, once it has been allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundleEntryKind {
    /// Markdown prose the skill points a reader at.
    Reference,
    /// Structured declarative data.
    Data,
    /// Plain text.
    Text,
}

/// One inventoried bundle entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleEntry {
    pub path: String,
    pub kind: BundleEntryKind,
}

/// Suffixes that are code. Refused by name so the refusal reads as a decision
/// rather than as a gap in the allow list.
const EXECUTABLE_SUFFIXES: [&str; 12] = [
    "bat", "cmd", "com", "dll", "exe", "js", "mjs", "pl", "ps1", "py", "rb", "sh",
];

impl BundleEntry {
    /// Classify one entry by its relative path and caller-reported kind.
    ///
    /// Refusal is the default. An unknown suffix is [`BundleRefusal::Unclassified`]
    /// rather than a permissive fallthrough, because the failure mode of the
    /// other choice is a payload riding in under a suffix nobody thought of.
    pub fn classify(path: &str, kind: RawEntryKind) -> Result<Self, SkillError> {
        let refuse = |r: BundleRefusal| Err(SkillError::RefusedBundleEntry(r));

        if path.is_empty()
            || path.len() > SKILL_BUNDLE_PATH_MAX_BYTES
            || path.contains('\\')
            || path.chars().any(|c| c.is_control())
        {
            return refuse(BundleRefusal::MalformedPath);
        }
        if path.starts_with('/') || path.chars().nth(1) == Some(':') {
            return refuse(BundleRefusal::AbsolutePath);
        }
        if path.split('/').any(|segment| segment == "..") {
            return refuse(BundleRefusal::ParentTraversal);
        }
        match kind {
            RawEntryKind::Symlink | RawEntryKind::Other | RawEntryKind::Directory => {
                return refuse(BundleRefusal::NotARegularFile);
            }
            RawEntryKind::File { executable: true } => return refuse(BundleRefusal::Executable),
            RawEntryKind::File { executable: false } => {}
        }

        let suffix = path
            .rsplit_once('.')
            .map(|(_, s)| s.to_ascii_lowercase())
            .unwrap_or_default();
        if EXECUTABLE_SUFFIXES.contains(&suffix.as_str()) {
            return refuse(BundleRefusal::ExecutableSuffix(suffix));
        }
        let kind = match suffix.as_str() {
            "md" | "markdown" => BundleEntryKind::Reference,
            "json" | "yaml" | "yml" | "toml" => BundleEntryKind::Data,
            "txt" => BundleEntryKind::Text,
            _ => return refuse(BundleRefusal::Unclassified(suffix)),
        };
        Ok(Self {
            path: path.to_owned(),
            kind,
        })
    }

    /// Inventory a whole bundle, refusing on the first entry that is not
    /// declarative content.
    pub fn inventory<'a>(
        entries: impl IntoIterator<Item = (&'a str, RawEntryKind)>,
    ) -> Result<Vec<Self>, SkillError> {
        let mut out = Vec::new();
        for (path, kind) in entries {
            if out.len() == SKILL_BUNDLE_MAX_ENTRIES {
                return Err(SkillError::TooManyEntries("bundle"));
            }
            out.push(Self::classify(path, kind)?);
        }
        Ok(out)
    }
}

/// Split a `SKILL.md` into its frontmatter and its body.
///
/// The body is returned untouched and unparsed. Markdown is the author's, and
/// nothing downstream of this module interprets it — a parser that started
/// reading the body would be inventing a second, undocumented surface for a
/// hostile document to reach.
pub fn split_frontmatter(input: &str) -> Result<(&str, &str), SkillError> {
    if input.len() > SKILL_INPUT_MAX_BYTES {
        return Err(SkillError::InputTooLarge);
    }
    let rest = input
        .strip_prefix("---\n")
        .or_else(|| input.strip_prefix("---\r\n"))
        .ok_or(SkillError::MissingFrontmatter)?;
    // The closing fence is a `---` line of its own, which is why this looks for
    // the newline-prefixed form: a `---` inside a value must not end the block.
    let (front, body) = rest
        .split_once("\n---\n")
        .or_else(|| rest.split_once("\n---\r\n"))
        .or_else(|| rest.strip_suffix("\n---").map(|front| (front, "")))
        .ok_or(SkillError::MissingFrontmatter)?;
    if front.len() > SKILL_FRONTMATTER_MAX_BYTES {
        return Err(SkillError::FrontmatterTooLarge);
    }
    Ok((front, body))
}

/// Enforce the accepted YAML subset before a YAML parser ever sees the input.
///
/// This runs first on purpose. Every property below could in principle be
/// asked of the deserializer, and each answer would then be a property of
/// whichever YAML crate is pinned this month — duplicate-key handling in
/// particular is a place serde-shaped YAML libraries have historically differed
/// and changed. Checking here makes the accepted subset a property of this
/// file, verifiable by reading it, and it makes the refusals nameable.
///
/// Returns the top-level keys in document order.
fn scan_subset(front: &str) -> Result<Vec<String>, SkillError> {
    let mut keys = Vec::new();
    let mut seen = BTreeSet::new();
    // Each open level, and whether a `- ` marker opened it. One column can hold
    // two, because a block sequence may sit at its parent mapping's indentation.
    let mut stack: Vec<(usize, bool)> = Vec::new();
    // The indentation of an open block scalar header. Its content is literal
    // text rather than YAML.
    let mut block_scalar: Option<usize> = None;

    for raw in front.lines() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        // U+2028 and U+2029 are category Zl/Zp, so `is_control` does not see
        // them — and `str::lines()` does not break on them while libyaml does.
        // That disagreement is a whole second line this scan never inspects:
        // `z: a\u{2028}w: &a b` presented one line here and two to the parser,
        // anchor included.
        if line
            .chars()
            .any(|c| (c.is_control() && c != '\t') || matches!(c, '\u{2028}' | '\u{2029}'))
        {
            return Err(SkillError::ControlCharacter);
        }
        // The indentation run measured with tabs included, so a tab anywhere in
        // it is refused rather than only at column zero. Measuring it with
        // spaces alone made both of the conditions that used to stand here
        // unreachable — `line[..indent]` was all spaces by construction, and
        // `trim_start` eats tabs before `starts_with('\t')` can see one — so
        // `  \tteam: infra` reached the deserializer and came back as a
        // shapeless `Malformed` instead of the named refusal this gate exists
        // to give.
        let indent = line.len() - line.trim_start_matches([' ', '\t']).len();

        // A block scalar's content is a string to the parser, so scanning it as
        // YAML refused ordinary prose: a description beginning `*bold*` was an
        // alias, `&c` an anchor, `!important` a tag, and a nested markdown
        // bullet list ran past the depth bound. It ends where the indentation
        // returns to the header's, which is the rule the parser applies too.
        if let Some(header) = block_scalar {
            if line.trim().is_empty() || indent > header {
                continue;
            }
            block_scalar = None;
        }

        if line[..indent].contains('\t') {
            return Err(SkillError::TabIndentation);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if trimmed == "---" || trimmed == "..." || trimmed.starts_with('%') {
            return Err(SkillError::UnsupportedYaml("document marker or directive"));
        }
        let facts = scan_line(trimmed)?;
        if facts.block_scalar {
            block_scalar = Some(indent);
        }

        if indent == 0 {
            match facts.key_end {
                Some(end) => {
                    let key = trimmed.get(..end).ok_or(SkillError::MalformedTopLevelKey)?;
                    if !is_plain_key(key) {
                        return Err(SkillError::MalformedTopLevelKey);
                    }
                    if !seen.insert(key.to_owned()) {
                        return Err(SkillError::DuplicateKey(key.to_owned()));
                    }
                    keys.push(key.to_owned());
                }
                // A block sequence may sit at its parent mapping's indentation,
                // so a column-zero `- ` line is the value of the key above it
                // rather than a malformed key of its own.
                None if facts.sequence_markers > 0 && !keys.is_empty() => {}
                None => return Err(SkillError::MalformedTopLevelKey),
            }
        }

        // Depth is the number of OPEN levels, not a column count. Dividing the
        // column by an assumed two-space step made the bound a property of the
        // author's formatting: a one-space ladder reached four levels against a
        // declared bound of two, while its two-space twin was refused at three.
        while stack.last().is_some_and(|&(open, _)| indent < open) {
            stack.pop();
        }
        // A line at this column closes any sequence open AT it, and its own
        // markers reopen one. Without this, consecutive `- ` items each stacked
        // another level and a three-item list read as three deep.
        while stack
            .last()
            .is_some_and(|&(open, seq)| seq && open == indent)
        {
            stack.pop();
        }
        let opened_by_indent = match stack.last() {
            Some(&(open, _)) if open >= indent => false,
            _ => {
                stack.push((indent, false));
                true
            }
        };
        // `- ` markers open a level each without moving the column, which is how
        // `z:\n - - - - 1` nested four deep at indentation 1. The first one
        // names the level the indentation increase already opened, if it did:
        // `a:\n    - 1` and `a:\n  - 1` are the same document, and counting both
        // measured them three levels and two.
        for _ in 0..facts
            .sequence_markers
            .saturating_sub(usize::from(opened_by_indent))
        {
            stack.push((indent, true));
        }
        // A flow collection nests exactly as a block one does. Leaving it out
        // let `a:\n  b:\n    c: [1, 2]` through at three levels while its block
        // spelling was refused at three.
        let depth = stack.len().saturating_sub(1) + facts.flow_depth;
        if depth > SKILL_MAX_NESTING_DEPTH {
            return Err(SkillError::TooDeeplyNested);
        }
    }

    if keys.is_empty() {
        return Err(SkillError::MalformedTopLevelKey);
    }
    Ok(keys)
}

/// What one scanned line yields to its caller.
struct LineFacts {
    /// Byte offset of the `:` terminating a top-level key, when the line has one.
    key_end: Option<usize>,
    /// `- ` element markers opened on this line, which nest without indenting.
    sequence_markers: usize,
    /// Deepest flow collection reached on this line. Flow nests like block.
    flow_depth: usize,
    /// The line ends with a block scalar header, so what follows it at a deeper
    /// indentation is literal text rather than YAML.
    block_scalar: bool,
}

/// Is this a plain, unquoted, unadorned mapping key?
///
/// Narrow on purpose. A quoted key is legal YAML and no portable manifest uses
/// one, and admitting it cost more than it was worth: `"x: y": &anc HIDDEN`
/// parsed, and the evidence record came back keyed `"x` with an empty value
/// because the later lookup searched for a key that was never in the document.
/// A silently dropped field is the one outcome this module's own header rules
/// out.
fn is_plain_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Walk one frontmatter line, refusing every node indicator at a node start.
///
/// The predecessor derived a single candidate node as everything after the
/// FIRST `": "` in the line and tested only that slice's first character. Three
/// independent reviews landed on the same line, and the escape is one inert
/// leading pair: in `- {u: 1, v: &a hello}` the first `": "` belongs to `u`, so
/// the slice begins `1, v: &a hello}` and the head test examines a digit. An
/// anchor and its alias reached the transpiled-C parser and were RESOLVED
/// across two top-level keys — `v: *z` came back carrying the anchored value —
/// which is precisely the expansion this gate exists to prevent.
///
/// So there is no candidate slice any more. This walks the line left to right
/// tracking quote state and flow depth, and refuses an indicator wherever a
/// node can begin: at the head, after `- `, after a key terminator, and after
/// `[`, `{`, or `,` inside a flow collection. `? ` opens a node too and is
/// refused outright; a block scalar header ends the line's YAML.
///
/// One line is the whole unit. Every construct that would carry state onto the
/// next line — an unclosed flow collection, an unclosed quoted scalar — is
/// refused at the end of this function, because the caller scans line by line
/// and hands this one a fresh state each time. Treating a continuation line as
/// a first line is what let `aa: [` / `  0, &s SECRET]` through after every
/// single-line spelling of it was closed.
///
/// Quote-awareness is load-bearing rather than decoration. A blanket search for
/// `{` would refuse `gridwork: '{"budget_tokens": 4096}'`, the single-quoted
/// JSON string the whole GridWork extension rides on. Token-awareness earns the
/// other direction too: `*` inside `[Bash(git *)]` is a glob in a plain scalar,
/// not an alias, and is left alone.
fn scan_line(trimmed: &str) -> Result<LineFacts, SkillError> {
    let chars: Vec<(usize, char)> = trimmed.char_indices().collect();
    let mut quote: Option<char> = None;
    let mut flow: usize = 0;
    let mut flow_depth = 0usize;
    let mut sequence_markers = 0usize;
    let mut key_end: Option<usize> = None;
    let mut block_scalar = false;
    // The head of the line is a node start; so is every position below that
    // sets this back to true.
    let mut node_start = true;
    let mut i = 0usize;

    while i < chars.len() {
        let (at, c) = chars[i];

        if let Some(q) = quote {
            if q == '"' && c == '\\' {
                i += 2;
                continue;
            }
            if c == q {
                // `''` inside a single-quoted scalar is an escaped quote, not
                // the end of one.
                if q == '\'' && chars.get(i + 1).map(|&(_, n)| n) == Some('\'') {
                    i += 2;
                    continue;
                }
                quote = None;
            }
            i += 1;
            continue;
        }

        // A quote opens a scalar only where a node can begin. YAML permits `'`
        // and `"` inside a plain scalar and forbids them only at its head, so
        // opening quote state at any position handed the rest of the line to a
        // branch that examines nothing: `[don't, &a hello]` was accepted, and
        // the alias in its twin key was RESOLVED.
        if (c == '\'' || c == '"') && node_start {
            quote = Some(c);
            node_start = false;
            i += 1;
            continue;
        }

        // A `#` opening a comment ends the line. It only opens one after
        // whitespace or at the head — `a#b` is a plain scalar.
        if c == '#' && (i == 0 || matches!(chars[i - 1].1, ' ' | '\t')) {
            break;
        }

        // Whitespace separates a node start from its node without ending it.
        if c == ' ' || c == '\t' {
            i += 1;
            continue;
        }

        if node_start {
            match c {
                '&' => return Err(SkillError::UnsupportedYaml("anchor")),
                '*' => return Err(SkillError::UnsupportedYaml("alias")),
                '!' => return Err(SkillError::UnsupportedYaml("tag")),
                '{' => return Err(SkillError::UnsupportedYaml("flow mapping")),
                '<' if chars.get(i + 1).map(|&(_, n)| n) == Some('<') => {
                    return Err(SkillError::UnsupportedYaml("merge key"));
                }
                '[' if flow > 0 => return Err(SkillError::UnsupportedYaml("flow sequence node")),
                // `? ` opens a node exactly as `- ` does, and had no arm here:
                // `?` fell through to the catch-all, cleared `node_start`, and
                // the whitespace skip below does not restore it — so the anchor
                // in `? &s SECRET` was never examined.
                '?' if matches!(
                    chars.get(i + 1).map(|&(_, n)| n),
                    None | Some(' ') | Some('\t')
                ) =>
                {
                    return Err(SkillError::UnsupportedYaml("explicit key"));
                }
                // A block scalar header ends the line's YAML; what follows is
                // literal text that `scan_subset` steps over.
                '|' | '>' if flow == 0 => {
                    check_block_scalar_header(&chars, i)?;
                    block_scalar = true;
                    break;
                }
                _ => {}
            }
        }
        // Inside a flow collection these are structural wherever they appear —
        // a plain scalar in flow context cannot contain them — so a `{` that is
        // not at a node start is still a mapping there, while in block context
        // `description: use {braces}` is an ordinary scalar.
        if flow > 0 {
            match c {
                '{' => return Err(SkillError::UnsupportedYaml("flow mapping")),
                '[' => return Err(SkillError::UnsupportedYaml("flow sequence node")),
                _ => {}
            }
        }

        match c {
            '[' => {
                flow += 1;
                flow_depth = flow_depth.max(flow);
                node_start = true;
            }
            ']' | '}' => {
                flow = flow.saturating_sub(1);
                node_start = false;
            }
            ',' => node_start = flow > 0,
            ':' => {
                // A key terminator is `:` followed by whitespace or end of
                // line. `12:30` and `http://x` are scalars, not mappings.
                let next = chars.get(i + 1).map(|&(_, n)| n);
                if next.is_none() || matches!(next, Some(' ') | Some('\t')) {
                    if flow == 0 && key_end.is_none() {
                        key_end = Some(at);
                    }
                    node_start = true;
                } else {
                    node_start = false;
                }
            }
            '-' => {
                let next = chars.get(i + 1).map(|&(_, n)| n);
                if node_start && (next.is_none() || matches!(next, Some(' ') | Some('\t'))) {
                    sequence_markers += 1;
                    // `- ` opens an element, so what follows is a node start.
                } else {
                    node_start = false;
                }
            }
            _ => node_start = false,
        }
        i += 1;
    }

    // State left open at the end of a line is a construct that continues onto
    // the next one, and this scan is line-at-a-time: `scan_subset` hands it each
    // line with the state reset. That gap admitted everything the walk above
    // refuses. `aa: [` then `  0, &s SECRET]` presented a continuation line
    // whose `flow` read zero, so the `,` cleared `node_start` instead of setting
    // it and the anchor went through — resolved, again, into a second key. A
    // quoted scalar spanning two lines did the mirror of it, registering a
    // top-level key the document does not contain.
    //
    // Refusing here is what keeps the accepted subset line-local, so no reader
    // has to reason about state crossing a line to know what this admits.
    if quote.is_some() {
        return Err(SkillError::UnsupportedYaml("multi-line quoted scalar"));
    }
    if flow > 0 {
        return Err(SkillError::UnsupportedYaml("multi-line flow collection"));
    }

    Ok(LineFacts {
        key_end,
        sequence_markers,
        flow_depth,
        block_scalar,
    })
}

/// Check the tail of a block scalar header (`|`, `>`, chomping, indentation).
///
/// The explicit indentation indicator (`|2`) is refused. With it, content
/// indentation is a number in the header rather than the first content line's
/// own indentation, and `scan_subset` ends the scalar where the indentation
/// returns to the header's — a rule that agrees with the parser only while both
/// detect that indentation the same way.
fn check_block_scalar_header(chars: &[(usize, char)], start: usize) -> Result<(), SkillError> {
    let mut i = start + 1;
    while let Some(&(_, c)) = chars.get(i) {
        match c {
            '-' | '+' => i += 1,
            '0'..='9' => {
                return Err(SkillError::UnsupportedYaml(
                    "block scalar indentation indicator",
                ));
            }
            _ => break,
        }
    }
    while matches!(chars.get(i), Some(&(_, ' ' | '\t'))) {
        i += 1;
    }
    match chars.get(i) {
        None | Some(&(_, '#')) => Ok(()),
        Some(_) => Err(SkillError::UnsupportedYaml("block scalar header")),
    }
}

/// Render one YAML value as bounded, lossy evidence.
fn render_opaque(value: &yaml_serde::Value) -> String {
    let mut rendered = match value {
        yaml_serde::Value::String(s) => s.clone(),
        other => yaml_serde::to_string(other)
            .unwrap_or_else(|_| "<unrenderable>".to_owned())
            .trim_end()
            .to_owned(),
    };
    if rendered.len() > SKILL_OPAQUE_VALUE_MAX_BYTES {
        rendered.truncate(
            (0..=SKILL_OPAQUE_VALUE_MAX_BYTES)
                .rev()
                .find(|&i| rendered.is_char_boundary(i))
                .unwrap_or(0),
        );
    }
    rendered
}

fn bounded(field: &'static str, value: String, max: usize) -> Result<String, SkillError> {
    if value.len() > max {
        return Err(SkillError::FieldTooLong(field));
    }
    Ok(value)
}

impl SkillManifest {
    /// Parse a `SKILL.md`, without checking it against a directory name.
    pub fn parse(input: &str) -> Result<Self, SkillError> {
        let (front, _body) = split_frontmatter(input)?;
        let keys = scan_subset(front)?;
        let doc: yaml_serde::Mapping =
            yaml_serde::from_str(front).map_err(|e| SkillError::Malformed(e.to_string()))?;

        let take = |k: &str| doc.get(yaml_serde::Value::String(k.to_owned())).cloned();
        let string = |field: &'static str, v: Option<yaml_serde::Value>| match v {
            None => Ok(None),
            Some(yaml_serde::Value::String(s)) => Ok(Some(s)),
            Some(_) => Err(SkillError::Malformed(format!("`{field}` must be a string"))),
        };

        let name = string("name", take("name"))?
            .ok_or_else(|| SkillError::Malformed("`name` is required".to_owned()))?;
        if name.is_empty()
            || name.len() > SKILL_NAME_MAX_BYTES
            || !name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(SkillError::InvalidName);
        }

        let description = bounded(
            "description",
            string("description", take("description"))?
                .ok_or_else(|| SkillError::Malformed("`description` is required".to_owned()))?,
            SKILL_DESCRIPTION_MAX_BYTES,
        )?;
        let license = string("license", take("license"))?
            .map(|v| bounded("license", v, SKILL_LICENSE_MAX_BYTES))
            .transpose()?;
        let compatibility = string("compatibility", take("compatibility"))?;

        let allowed_tools = match take("allowed-tools") {
            None => AllowedToolsEvidence::default(),
            Some(yaml_serde::Value::Sequence(items)) => {
                if items.len() > SKILL_ALLOWED_TOOLS_MAX_COUNT {
                    return Err(SkillError::TooManyEntries("allowed-tools"));
                }
                let mut claimed = Vec::with_capacity(items.len());
                for item in items {
                    let yaml_serde::Value::String(tool) = item else {
                        return Err(SkillError::Malformed(
                            "`allowed-tools` entries must be strings".to_owned(),
                        ));
                    };
                    claimed.push(bounded(
                        "allowed-tools",
                        tool,
                        SKILL_ALLOWED_TOOL_MAX_BYTES,
                    )?);
                }
                AllowedToolsEvidence { claimed }
            }
            Some(_) => {
                return Err(SkillError::Malformed(
                    "`allowed-tools` must be a sequence".to_owned(),
                ));
            }
        };

        let mut metadata = BTreeMap::new();
        let mut gridwork = None;
        if let Some(value) = take("metadata") {
            let yaml_serde::Value::Mapping(map) = value else {
                return Err(SkillError::Malformed(
                    "`metadata` must be a mapping".to_owned(),
                ));
            };
            if map.len() > SKILL_METADATA_MAX_ENTRIES {
                return Err(SkillError::TooManyEntries("metadata"));
            }
            for (k, v) in map {
                let yaml_serde::Value::String(key) = k else {
                    return Err(SkillError::Malformed(
                        "`metadata` keys must be strings".to_owned(),
                    ));
                };
                if key.len() > SKILL_METADATA_KEY_MAX_BYTES {
                    return Err(SkillError::FieldTooLong("metadata key"));
                }
                // Upstream types this as dict[str, str]; a number or a nested
                // mapping here is either a different format or an author
                // assuming a richer one, and both are worth saying out loud.
                let yaml_serde::Value::String(value) = v else {
                    return Err(SkillError::MetadataValueNotString(key));
                };
                if value.len() > SKILL_METADATA_VALUE_MAX_BYTES {
                    return Err(SkillError::FieldTooLong("metadata value"));
                }
                if key == GRIDWORK_METADATA_KEY {
                    gridwork = Some(
                        serde_json::from_str::<GridworkExt>(&value)
                            .map_err(|e| SkillError::InvalidGridworkExtension(e.to_string()))?,
                    );
                } else {
                    metadata.insert(key, value);
                }
            }
        }

        let mut opaque = Vec::new();
        for key in keys {
            if PORTABLE_CORE_FIELDS.contains(&key.as_str()) {
                continue;
            }
            if opaque.len() == SKILL_OPAQUE_FIELD_MAX_COUNT {
                return Err(SkillError::TooManyEntries("unrecognized fields"));
            }
            let value = take(&key).map(|v| render_opaque(&v)).unwrap_or_default();
            opaque.push(OpaquePortableField { key, value });
        }

        Ok(Self {
            name,
            description,
            license,
            compatibility,
            allowed_tools,
            metadata,
            gridwork,
            opaque,
        })
    }

    /// Parse a `SKILL.md` that lives in `directory`, requiring the two to agree.
    ///
    /// The mismatch matters because the directory name is how a skill is
    /// addressed and the manifest `name` is how it identifies itself. Letting
    /// them differ means one skill answers to two names, and the one a
    /// reviewer read is not necessarily the one a route resolved.
    pub fn parse_in(directory: &str, input: &str) -> Result<Self, SkillError> {
        let manifest = Self::parse(input)?;
        if manifest.name != directory {
            return Err(SkillError::NameDirectoryMismatch);
        }
        Ok(manifest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = "---\nname: review-diff\ndescription: Review a diff.\n---\nBody.\n";

    fn manifest(front: &str) -> Result<SkillManifest, SkillError> {
        SkillManifest::parse(&format!("---\n{front}\n---\nBody.\n"))
    }

    #[test]
    fn a_minimal_portable_manifest_parses() {
        let skill = SkillManifest::parse(MINIMAL).expect("minimal manifest");
        assert_eq!(skill.name, "review-diff");
        assert_eq!(skill.description, "Review a diff.");
        assert!(skill.license.is_none());
        assert!(skill.gridwork.is_none());
        assert!(skill.opaque.is_empty());
        assert!(skill.allowed_tools.is_empty());
    }

    #[test]
    fn the_body_is_returned_untouched_and_never_interpreted() {
        let input = "---\nname: n\ndescription: d\n---\n# Heading\n\nname: not-a-field\n";
        let (_front, body) = split_frontmatter(input).expect("split");
        assert_eq!(body, "# Heading\n\nname: not-a-field\n");
        // The body's `name:` line is prose, and the parser must not have read it.
        assert_eq!(SkillManifest::parse(input).expect("parse").name, "n");
    }

    #[test]
    fn an_unknown_portable_field_is_kept_as_evidence_not_dropped_or_refused() {
        let skill = manifest("name: n\ndescription: d\nfuture-field: hello").expect("accepted");
        assert_eq!(
            skill.opaque,
            vec![OpaquePortableField {
                key: "future-field".to_owned(),
                value: "hello".to_owned(),
            }]
        );
    }

    #[test]
    fn an_unknown_gridwork_extension_field_is_a_hard_error() {
        // The asymmetry, in one test: the same document shape is accepted with
        // an unknown key in the portable core and refused with one in ours.
        let err =
            manifest("name: n\ndescription: d\nmetadata:\n  gridwork: '{\"invented\": true}'")
                .expect_err("refused");
        assert!(
            matches!(err, SkillError::InvalidGridworkExtension(_)),
            "{err:?}"
        );

        let ok =
            manifest("name: n\ndescription: d\nmetadata:\n  gridwork: '{\"routes\": [\"a\"]}'")
                .expect("known key accepted");
        assert_eq!(ok.gridwork.expect("present").routes, vec!["a".to_owned()]);
    }

    #[test]
    fn the_gridwork_key_does_not_also_survive_as_raw_metadata() {
        let skill = manifest("name: n\ndescription: d\nmetadata:\n  gridwork: '{}'\n  team: infra")
            .expect("accepted");
        assert!(skill.gridwork.is_some());
        assert_eq!(
            skill.metadata.get("team").map(String::as_str),
            Some("infra")
        );
        assert!(!skill.metadata.contains_key("gridwork"));
    }

    #[test]
    fn duplicate_top_level_keys_are_refused() {
        let err = manifest("name: first\ndescription: d\nname: second").expect_err("refused");
        assert_eq!(err, SkillError::DuplicateKey("name".to_owned()));
    }

    #[test]
    fn anchors_aliases_tags_and_merge_keys_are_refused() {
        for (front, what) in [
            ("name: n\ndescription: &a d", "anchor"),
            ("name: n\ndescription: *a", "alias"),
            ("name: n\ndescription: !!str d", "tag"),
            ("name: n\ndescription: d\n<<: *base", "merge key"),
            (
                "name: n\ndescription: d\n%YAML 1.2",
                "document marker or directive",
            ),
        ] {
            assert_eq!(
                manifest(front).expect_err("refused"),
                SkillError::UnsupportedYaml(what),
                "{front}"
            );
        }
    }

    #[test]
    fn the_same_indicators_are_refused_in_flow_style() {
        // Every case above was reachable in flow style, where the indicator
        // sits inside the node instead of at its head. Each case here is the
        // flow spelling of one the block-style test refuses, so the two read as
        // one property rather than two lists that can drift apart.
        for (front, what) in [
            (
                "name: n\ndescription: d\nextra: {v: &a hello}",
                "flow mapping",
            ),
            ("name: n\ndescription: d\nm: {<<: *base}", "flow mapping"),
            (
                "name: n\ndescription: d\nx: {a: {b: {c: 1}}}",
                "flow mapping",
            ),
            // These two now name the indicator itself rather than the shape
            // carrying it: the walk sees the `*` and the `!` where they sit,
            // instead of inferring "something is in here somewhere".
            ("name: n\ndescription: d\nboom: [*a, *a]", "alias"),
            ("name: n\ndescription: d\nt: [!!str 5]", "tag"),
            ("name: n\ndescription: d\nn: [[1, 2]]", "flow sequence node"),
            // The escapes round two found. One inert leading pair moves the
            // first `": "` off the flow collection, and the predecessor derived
            // its entire candidate node from that offset — so the head test
            // examined a digit and every indicator behind it went through.
            (
                "name: n\ndescription: d\nx:\n  - {u: 1, v: &a hello}",
                "flow mapping",
            ),
            ("name: n\ndescription: d\nx:\n  - [1, &a hello]", "anchor"),
            (
                "name: n\ndescription: d\nx:\n  - {u: 1, <<: q}",
                "flow mapping",
            ),
            // A tab as the key separator leaves no `": "` in the line at all,
            // so the candidate node fell back to the whole line.
            ("name: n\ndescription: d\nx:\t&a hello", "anchor"),
            // A quoted key containing a separator put the split inside the key,
            // so the anchor after the REAL separator was never examined.
            ("name: n\ndescription: d\n\"x: y\": &anc HIDDEN", "anchor"),
            // Inside a flow collection a brace or bracket is structural
            // wherever it appears, because a plain scalar in flow context
            // cannot contain one — unlike `extra: use {braces} here` in block
            // context, which is an ordinary scalar and stays accepted. Without
            // these two the in-flow branch was covered by nothing: deleting it
            // left the whole suite green.
            ("name: n\ndescription: d\nx: [a{b: c}]", "flow mapping"),
            ("name: n\ndescription: d\nx: [a[b]]", "flow sequence node"),
        ] {
            assert_eq!(
                manifest(front).expect_err("refused"),
                SkillError::UnsupportedYaml(what),
                "{front}"
            );
        }
    }

    #[test]
    fn indicators_inside_a_plain_scalar_are_not_indicators() {
        // The gate refuses `&`, `*`, `!`, `{` where a node can START. In the
        // middle of a plain scalar they are text. This is the half a positional
        // scan buys that a blanket search cannot: without it the gate refuses
        // ordinary prose and ordinary tool globs, and the pressure to loosen it
        // lands on the indicator list rather than on the position logic.
        for front in [
            "name: n\ndescription: d\nlicense: Read & write",
            "name: n\ndescription: d\nallowed-tools: [Bash(git *), Read]",
            "name: n\ndescription: d\nextra: use {braces} here",
            "name: n\ndescription: d\nextra: glob*pattern",
            "name: n\ndescription: d\nextra: hey!there",
            "name: n\ndescription: d\nextra: see http://example.com/x",
            "name: n\ndescription: d\nextra: 12:30",
            // The single-quoted JSON the whole GridWork extension rides on. A
            // blanket `contains('{')` would refuse this document.
            "name: n\ndescription: d\nmetadata:\n  gridwork: '{\"note\": \"x: y\"}'",
        ] {
            manifest(front).unwrap_or_else(|e| panic!("{front}\nrefused as {e:?}"));
        }
    }

    #[test]
    fn depth_is_open_levels_rather_than_indentation_columns() {
        // Dividing the column by an assumed two-space step made the bound a
        // property of the author's formatting: this ladder reached four levels
        // against a declared bound of two, while its two-space twin was refused
        // at three.
        assert_eq!(
            manifest("name: n\ndescription: d\nm:\n a:\n  b:\n   c:\n    d: 1")
                .expect_err("refused"),
            SkillError::TooDeeplyNested
        );
        // A compact block sequence nests without moving the column at all.
        assert_eq!(
            manifest("name: n\ndescription: d\nz:\n - - - - 1").expect_err("refused"),
            SkillError::TooDeeplyNested
        );
        // The accepted control, so the bound is not just "refuse sequences".
        manifest("name: n\ndescription: d\nallowed-tools:\n  - Read\n  - Grep")
            .expect("an ordinary block sequence is one level");
    }

    #[test]
    fn a_line_terminator_libyaml_splits_on_is_refused() {
        // `str::lines()` does not break on U+2028 and libyaml does, so one line
        // here was two lines there — a whole document line this scan never
        // inspected, anchor included.
        assert_eq!(
            manifest("name: n\ndescription: d\nz: a\u{2028}w: &a b").expect_err("refused"),
            SkillError::ControlCharacter
        );
    }

    #[test]
    fn a_quoted_top_level_key_is_refused_rather_than_repaired() {
        // Legal YAML no portable manifest uses, and admitting it dropped the
        // field: the record came back keyed `"x` with an empty value because the
        // later lookup searched for a key the document does not contain.
        assert_eq!(
            manifest("name: n\ndescription: d\n\"x: y\": plain").expect_err("refused"),
            SkillError::MalformedTopLevelKey
        );
    }

    #[test]
    fn a_flat_flow_sequence_of_plain_scalars_is_still_accepted() {
        // The positive control those refusals need. Without it the branch would
        // pass just as well if it refused every flow sequence — and that would
        // refuse `allowed-tools: [Read, Grep]`, the spelling upstream uses and
        // the whole reason this is not a blanket refusal.
        let skill = manifest("name: n\ndescription: d\nallowed-tools: [Read, Grep]")
            .expect("a flat flow sequence of plain scalars parses");
        let claimed: Vec<&str> = skill
            .allowed_tools
            .claimed()
            .iter()
            .map(String::as_str)
            .collect();
        assert_eq!(claimed, ["Read", "Grep"]);
    }

    #[test]
    fn nesting_past_the_depth_bound_is_refused() {
        // `SKILL_MAX_NESTING_DEPTH` had no test at all: deleting the branch
        // that enforces it left the suite green, so the bound was decoration.
        assert_eq!(
            manifest("name: n\ndescription: d\nmetadata:\n  a:\n    b:\n      c: 1")
                .expect_err("refused"),
            SkillError::TooDeeplyNested
        );
        manifest("name: n\ndescription: d\nmetadata:\n  team: infra")
            .expect("the accepted edge, so the bound is pinned from both sides");
    }

    #[test]
    fn a_construct_that_continues_onto_the_next_line_is_refused() {
        // This scan is line-at-a-time and YAML is not. `scan_line` is handed
        // each line with its state reset, so on a continuation line inside an
        // open flow collection `flow` read zero: the `,` cleared `node_start`
        // instead of setting it, every indicator branch went dead, and the
        // anchor reached the parser — which RESOLVED it into a second key.
        // Every single-line spelling below was already refused. The line break
        // was the whole escape, and no fixture in the corpus had one.
        for (front, what) in [
            (
                "name: n\ndescription: d\naa: [\n  0, &s SECRET]\nbb: [\n  0, *s]",
                "multi-line flow collection",
            ),
            (
                "name: n\ndescription: d\nz: [\n  1, !!str 5]",
                "multi-line flow collection",
            ),
            (
                "name: n\ndescription: d\nz: [\n  1, {a: 1}]",
                "multi-line flow collection",
            ),
            // The alias landing in the one field with security meaning.
            (
                "name: n\ndescription: d\nz: [\n  0, &t Bash(rm -rf /)]\nallowed-tools: [\n  Read, *t]",
                "multi-line flow collection",
            ),
            // A column-zero continuation additionally registered a top-level key
            // the document does not contain.
            (
                "name: n\ndescription: d\nz: [a,\nb: c]",
                "multi-line flow collection",
            ),
            // The mirror of it in a quoted scalar: the parser folds these two
            // lines into one and never produces `def`, while the scan recorded
            // `def` as a field, with an empty value, that nothing backs.
            (
                "name: n\ndescription: \"abc\ndef: hello\"",
                "multi-line quoted scalar",
            ),
        ] {
            assert_eq!(
                manifest(front).expect_err("refused"),
                SkillError::UnsupportedYaml(what),
                "{front}"
            );
        }
        // The accepted control, so this is a refusal of the line break and not
        // of the collection.
        manifest("name: n\ndescription: d\nz: [0, 1]")
            .expect("a flow sequence on one line is the subset");
    }

    #[test]
    fn a_quote_inside_a_plain_scalar_is_text_rather_than_a_quoted_scalar() {
        // YAML permits `'` and `"` inside a plain scalar and forbids them only
        // at its head, so opening quote state at any position handed the rest of
        // the line to the one region the indicator branches do not examine. The
        // control differs by a single character and refuses correctly.
        for (front, what) in [
            ("name: n\ndescription: d\nx: [don't, &a hello]", "anchor"),
            ("name: n\ndescription: d\nx: [say\"hi, &a hello]", "anchor"),
            (
                "name: n\ndescription: d\nx: it's fine\ny: &a hello",
                "anchor",
            ),
            (
                "name: n\ndescription: d\nx: [don't, {a: {b: 1}}]",
                "flow mapping",
            ),
        ] {
            assert_eq!(
                manifest(front).expect_err("refused"),
                SkillError::UnsupportedYaml(what),
                "{front}"
            );
        }
        // The other direction, which is why this is a position gate rather than
        // a refusal: an apostrophe in prose is text, and a quote where a node
        // does begin still opens a scalar.
        manifest("name: n\ndescription: d\nextra: it's fine")
            .expect("an apostrophe inside a plain scalar is text");
        manifest("name: n\ndescription: d\nextra: 'quoted, {braces}'")
            .expect("a quote at a node start still opens a scalar");
    }

    #[test]
    fn an_explicit_key_is_refused_rather_than_walked_past() {
        // `- ` has its own arm because it opens a node. `? ` opens one too and
        // had none: it fell to the catch-all that clears `node_start`, and the
        // whitespace skip does not restore it, so the anchor after it was never
        // examined. In the `metadata:` spelling the anchored value reaches TYPED
        // output rather than opaque evidence.
        for front in [
            "name: n\ndescription: d\naa:\n  ? &s SECRET\n  : 1\nbb:\n  ? *s\n  : 2",
            "name: n\ndescription: d\nmetadata:\n  ? &s hello\n  : v",
            "name: n\ndescription: d\nz:\n  ? !!str k\n  : v",
        ] {
            assert_eq!(
                manifest(front).expect_err("refused"),
                SkillError::UnsupportedYaml("explicit key"),
                "{front}"
            );
        }
        // `?` is an indicator only where a node begins; inside a scalar it is
        // text, and refusing it there would refuse ordinary prose.
        manifest("name: n\ndescription: d\nextra: really?")
            .expect("a question mark inside a scalar is text");
    }

    #[test]
    fn block_scalar_content_is_text_rather_than_yaml() {
        // Scanning literal content as YAML refused ordinary prose, and every one
        // of those refusals put pressure on the indicator list — the one place
        // where relieving it reopens the escapes the list exists to close.
        for (front, want) in [
            (
                "name: n\ndescription: d\nnote: |\n  &c is an entity.",
                "&c is an entity.",
            ),
            (
                "name: n\ndescription: d\nnote: |\n  *bold* start of line.",
                "*bold* start of line.",
            ),
            (
                "name: n\ndescription: d\nnote: |\n  !important note.",
                "!important note.",
            ),
            (
                "name: n\ndescription: d\nnote: |\n  ---\n  still content",
                "---\nstill content",
            ),
            (
                "name: n\ndescription: d\nnote: |\n  - a\n    - b\n      - c",
                "- a\n  - b\n    - c",
            ),
        ] {
            let skill = manifest(front).unwrap_or_else(|e| panic!("{front}\nrefused as {e:?}"));
            assert_eq!(skill.opaque[0].value, want, "{front}");
        }
        // The explicit indentation indicator is refused: with it the content
        // indentation is a number in the header rather than the first content
        // line's own, and the rule that ends the scalar here would stop agreeing
        // with the parser's.
        assert_eq!(
            manifest("name: n\ndescription: d\nnote: |2\n  explicit").expect_err("refused"),
            SkillError::UnsupportedYaml("block scalar indentation indicator")
        );
        // And the skip ENDS: an indicator after the scalar is still an indicator.
        // Without this the fix would be a hole rather than a repair.
        assert_eq!(
            manifest("name: n\ndescription: d\nnote: |\n  text\nz: &a hello").expect_err("refused"),
            SkillError::UnsupportedYaml("anchor")
        );
    }

    #[test]
    fn the_same_document_measures_the_same_depth_in_either_spelling() {
        // A block sequence may sit at its parent mapping's indentation or
        // deeper; both spellings are the same document. Counting the indentation
        // push and the `- ` marker separately measured them three levels and
        // two, so the bound was still a property of the author's formatting —
        // the thing the stack replaced column division to stop being.
        manifest("name: n\ndescription: d\nm:\n  a:\n    - 1").expect("the indented spelling");
        manifest("name: n\ndescription: d\nm:\n  a:\n  - 1").expect("the compact spelling");
        // Consecutive items are one sequence, not one level each.
        manifest("name: n\ndescription: d\na:\n- 1\n- 2\n- 3")
            .expect("a column-zero block sequence is the value of the key above it");
        // A flow collection nests exactly as its block spelling does. This pair
        // disagreed: the flow one was accepted at three levels while the block
        // one was refused at three.
        for front in [
            "name: n\ndescription: d\nm:\n  a:\n    b: [1, 2]",
            "name: n\ndescription: d\nm:\n  a:\n    b:\n    - 1",
        ] {
            assert_eq!(
                manifest(front).expect_err("refused"),
                SkillError::TooDeeplyNested,
                "{front}"
            );
        }
    }

    #[test]
    fn a_tab_anywhere_in_the_indentation_run_is_refused_by_name() {
        // Not only at column zero. Measured with spaces alone, the second case
        // reached the deserializer and came back `Malformed`, which is the
        // refusal this gate exists to replace with a named one.
        for front in [
            "name: n\ndescription: d\n\tteam: infra",
            "name: n\ndescription: d\nmetadata:\n  \tteam: infra",
        ] {
            assert_eq!(
                manifest(front).expect_err("refused"),
                SkillError::TabIndentation,
                "{front}"
            );
        }
    }

    #[test]
    fn oversized_inputs_are_refused_before_parsing() {
        let huge = "x".repeat(SKILL_INPUT_MAX_BYTES + 1);
        assert_eq!(
            SkillManifest::parse(&huge).expect_err("refused"),
            SkillError::InputTooLarge
        );
        let long_description = format!("name: n\ndescription: {}", "d".repeat(2000));
        assert_eq!(
            manifest(&long_description).expect_err("refused"),
            SkillError::FieldTooLong("description")
        );
    }

    #[test]
    fn invalid_names_are_refused() {
        for name in ["", "Review", "review diff", "review_diff", &"a".repeat(65)] {
            let front = format!("name: '{name}'\ndescription: d");
            assert_eq!(
                manifest(&front).expect_err("refused"),
                SkillError::InvalidName,
                "{name:?}"
            );
        }
    }

    #[test]
    fn a_name_that_disagrees_with_its_directory_is_refused() {
        assert_eq!(
            SkillManifest::parse_in("other-dir", MINIMAL).expect_err("refused"),
            SkillError::NameDirectoryMismatch
        );
        assert!(SkillManifest::parse_in("review-diff", MINIMAL).is_ok());
    }

    #[test]
    fn non_string_metadata_values_are_refused() {
        let err = manifest("name: n\ndescription: d\nmetadata:\n  count: 3").expect_err("refused");
        assert_eq!(err, SkillError::MetadataValueNotString("count".to_owned()));
    }

    #[test]
    fn control_characters_and_tab_indentation_are_refused() {
        assert_eq!(
            manifest("name: n\ndescription: d\u{0007}").expect_err("refused"),
            SkillError::ControlCharacter
        );
        assert_eq!(
            manifest("name: n\ndescription: d\nmetadata:\n\tteam: infra").expect_err("refused"),
            SkillError::TabIndentation
        );
    }

    #[test]
    fn missing_frontmatter_is_refused() {
        assert_eq!(
            SkillManifest::parse("name: n\n").expect_err("refused"),
            SkillError::MissingFrontmatter
        );
        assert_eq!(
            SkillManifest::parse("---\nname: n\n").expect_err("refused"),
            SkillError::MissingFrontmatter
        );
    }

    #[test]
    fn allowed_tools_is_evidence_with_no_route_to_a_grant() {
        let skill = manifest("name: n\ndescription: d\nallowed-tools:\n  - Read\n  - Bash")
            .expect("accepted");
        assert_eq!(skill.allowed_tools.len(), 2);
        assert_eq!(skill.allowed_tools.claimed(), ["Read", "Bash"]);
        // The type exposes `claimed()` and nothing that yields a capability.
        // A mutation that added such a method has to be written by hand, which
        // is the point: there is no accidental path from claim to grant.
    }

    /// Stand-in for the authority resolved upstream of context compilation.
    ///
    /// Deliberately takes `&[String]` rather than `AllowedToolsEvidence`: the
    /// evidence type has no conversion into this, and writing one is the
    /// mutation this test exists to catch.
    fn effective_tools<'a>(granted: &[&'a str], claimed: &'a [String]) -> BTreeSet<&'a str> {
        let granted: BTreeSet<&str> = granted.iter().copied().collect();
        let claimed: BTreeSet<&str> = claimed.iter().map(String::as_str).collect();
        // INTERSECTION, not union. D3: context narrows authority, never widens
        // it. Swapping this one operator is the whole attack — a manifest would
        // then grant itself whatever it asked for by asking.
        claimed.intersection(&granted).copied().collect()
    }

    #[test]
    fn a_manifest_cannot_widen_the_authority_it_was_handed() {
        let skill = manifest("name: n\ndescription: d\nallowed-tools:\n  - Read\n  - Bash")
            .expect("accepted");
        let granted = ["Read", "Grep"];
        let effective = effective_tools(&granted, skill.allowed_tools.claimed());

        assert_eq!(effective, BTreeSet::from(["Read"]));
        assert!(
            !effective.contains("Bash"),
            "a tool the manifest claimed but was never granted became effective"
        );
        assert!(
            !effective.contains("Grep"),
            "a granted tool the manifest never claimed became effective"
        );
        // And the narrowing holds when the manifest claims everything.
        let greedy =
            manifest("name: n\ndescription: d\nallowed-tools:\n  - Read\n  - Grep\n  - Bash")
                .expect("accepted");
        assert_eq!(
            effective_tools(&granted, greedy.allowed_tools.claimed()),
            BTreeSet::from(["Read", "Grep"])
        );
    }

    #[test]
    fn bundle_entries_are_inventoried_by_kind_and_refused_by_default() {
        let ok = BundleEntry::inventory([
            ("REFERENCE.md", RawEntryKind::File { executable: false }),
            ("data/table.json", RawEntryKind::File { executable: false }),
        ])
        .expect("declarative bundle");
        assert_eq!(ok[0].kind, BundleEntryKind::Reference);
        assert_eq!(ok[1].kind, BundleEntryKind::Data);

        for (path, kind, refusal) in [
            (
                "run.sh",
                RawEntryKind::File { executable: false },
                BundleRefusal::ExecutableSuffix("sh".to_owned()),
            ),
            (
                "notes.md",
                RawEntryKind::File { executable: true },
                BundleRefusal::Executable,
            ),
            (
                "link.md",
                RawEntryKind::Symlink,
                BundleRefusal::NotARegularFile,
            ),
            (
                "../escape.md",
                RawEntryKind::File { executable: false },
                BundleRefusal::ParentTraversal,
            ),
            (
                "/etc/passwd",
                RawEntryKind::File { executable: false },
                BundleRefusal::AbsolutePath,
            ),
            (
                "payload.bin",
                RawEntryKind::File { executable: false },
                BundleRefusal::Unclassified("bin".to_owned()),
            ),
            (
                "no-suffix",
                RawEntryKind::File { executable: false },
                BundleRefusal::Unclassified(String::new()),
            ),
        ] {
            assert_eq!(
                BundleEntry::classify(path, kind).expect_err("refused"),
                SkillError::RefusedBundleEntry(refusal),
                "{path}"
            );
        }
    }
}
