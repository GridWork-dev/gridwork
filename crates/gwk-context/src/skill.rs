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
//!
//! One accepted cost, named here so nobody relieves it in the wrong place. The
//! scan is line-local: it refuses any construct that carries state past the end
//! of a line, because handing a continuation line to a scanner whose state had
//! been reset is what admitted every escape the first four rounds of review
//! found. Every line is therefore scanned as if it began a node, and the price
//! is paid by a plain scalar wrapped onto a second line: the continuation is
//! refused when it leaves quote or flow state open, or when its first token is
//! a node indicator. The parser reads such a line as text, so these refusals
//! name this scanner's state rather than the document.
//!
//! State the rule rather than a list of the characters it happens to catch —
//! an earlier revision of this paragraph listed three and got two of them
//! wrong, because a BALANCED quote on a continuation (`'quoted' bit`) is
//! accepted and it is the unclosed one that is refused. Every shape has an
//! accepted spelling (a block scalar for prose, a block sequence for
//! `allowed-tools`), so no real manifest is blocked. Repairing it needs exactly
//! the cross-line state whose absence is the property above, so the trade is
//! deliberate: relieve it with a block scalar, never by loosening the indicator
//! list.

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
/// Maximum bytes in the `compatibility` field.
pub const SKILL_COMPATIBILITY_MAX_BYTES: usize = 128;
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
    /// A top-level key this scan read as text that the parsed document does not
    /// carry under that name.
    KeyNotInDocument(String),
    /// A plain key whose text a YAML 1.1 reader resolves as a non-string
    /// (F-8B-YAML, ruled (a) 2026-08-27): under such a reader it collapses
    /// onto its resolved twin and the later value silently wins.
    Yaml11AmbiguousKey(String),
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
            Self::KeyNotInDocument(key) => write!(
                f,
                "skill frontmatter key `{key}` is absent from the parsed document"
            ),
            Self::Yaml11AmbiguousKey(key) => write!(
                f,
                "skill frontmatter key `{key}` is a spelling a YAML 1.1 reader \
                 resolves as a non-string; quote or respell it"
            ),
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
        // `str::trim` strips the whole Unicode White_Space class; the
        // indentation above is ASCII, and the parser agrees with the ASCII
        // side — U+00A0 is content to it. Trimmed away, `\u{a0}license:` was
        // scanned under a name the document does not contain: the real field
        // vanished from the record, a phantom opaque key appeared in its
        // place, and the `\u{a0}metadata:` spelling skipped the GridWork
        // namespace's loud refusal entirely. Sliced instead, such a line
        // starts with a non-ASCII character at column zero, and
        // `is_plain_key` refuses it by name — exactly as the mid-key
        // spelling always was.
        let trimmed = line[indent..].trim_end_matches([' ', '\t']);
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if trimmed == "---" || trimmed == "..." || trimmed.starts_with('%') {
            return Err(SkillError::UnsupportedYaml("document marker or directive"));
        }
        let facts = scan_line(trimmed)?;
        // F-8B-YAML: a plain key whose text a 1.1 reader resolves as a
        // non-string is refused at ANY level, before the parser sees it. The
        // key must sit at the line's head for its text to be extractable here,
        // which covers every ordinary `key: value` spelling including
        // `metadata:`'s sub-keys; a QUOTED spelling does not satisfy
        // `is_plain_key` and stays legal, so nothing becomes inexpressible.
        // Two named residuals share one property — the ban sees only block
        // keys at their line's head: a key on its sequence-marker line
        // (`- yes: x`) and a flow-mapping key (`{yes: 1}`) are not inspected.
        // Both spell the same document down the page in the form this DOES
        // inspect, and neither appears in any portable manifest this repo has
        // seen.
        if let Some(end) = facts.key_end
            && let Some(key) = trimmed.get(..end)
            && is_plain_key(key)
            && yaml11_ambiguous_key(key)
        {
            return Err(SkillError::Yaml11AmbiguousKey(key.to_owned()));
        }
        if facts.block_scalar {
            // The skip floor is the line's indentation, and that is the column
            // of the node owning the scalar only when nothing on this line
            // moved it right. A `- ` marker does: in `  - k: |` the line
            // indents 2 while the mapping holding `k` starts at 4, and the
            // parser ends the scalar at the content indentation it detects (8).
            // A sibling key at column 4 then satisfies 4 > 2 and was skipped
            // here, while 4 < 8 kept it live there — so an anchor and its alias
            // were hidden from this scan and RESOLVED by the parser.
            //
            // Refused rather than measured, for the same reason `|2` is: the
            // skip agrees with the parser only while both find the same
            // indentation, and with no marker on the line that agreement holds
            // by construction. No portable manifest spells a block scalar this
            // way.
            if facts.sequence_markers > 0 {
                return Err(SkillError::UnsupportedYaml(
                    "block scalar behind a sequence marker",
                ));
            }
            block_scalar = Some(indent);
        }

        if indent == 0 {
            if facts.sequence_markers > 0 {
                // A block sequence may sit at its parent mapping's indentation,
                // so a column-zero `- ` line is the value of the key above it
                // rather than a key of its own — whatever the item holds. Read
                // as a key, `- k: 1` sliced to `- k` and refused for the space,
                // while its plain-scalar twin `- 1` was accepted.
                if keys.is_empty() {
                    return Err(SkillError::MalformedTopLevelKey);
                }
            } else {
                let end = facts.key_end.ok_or(SkillError::MalformedTopLevelKey)?;
                let key = trimmed.get(..end).ok_or(SkillError::MalformedTopLevelKey)?;
                if !is_plain_key(key) {
                    return Err(SkillError::MalformedTopLevelKey);
                }
                if !seen.insert(key.to_owned()) {
                    return Err(SkillError::DuplicateKey(key.to_owned()));
                }
                keys.push(key.to_owned());
            }
        }

        // Depth is the number of OPEN collection levels, and every level is
        // named by the COLUMN its content starts at — the sequence a `- ` opens
        // at the marker's own column, the mapping a `key:` opens at the key's.
        // `scan_line` reports those columns; this only has to keep the stack.
        //
        // Counting anything else made the bound a property of the spelling, and
        // three rounds of review each found another spelling it could not tell
        // apart. Dividing the column by an assumed two-space step let a
        // one-space ladder reach four levels against a declared bound of two.
        // Counting `- ` markers at the line's indentation instead of their own
        // measured `- - bb: 1` and its expansion two levels and three. Pushing
        // an unpoppable level for a mapping inside an item counted ITEMS, so a
        // two-entry list of mappings — `authors:` with two `- name:` — refused
        // a document its one-entry twin accepted.
        //
        // A level continues when a later line names its column with its kind,
        // and closes when a line names a column inside it. At the level's own
        // column the relation is asymmetric: a sequence named at an open
        // mapping's column is that mapping's value — a child — while a mapping
        // named at an open sequence's column ends that sequence, because item
        // content always sits right of the `- ` marker, so nothing inside an
        // item can share the sequence's own column. An earlier revision pushed
        // in both directions, so a column-zero sequence was never closed and a
        // fully portable manifest whose `allowed-tools:` list preceded
        // `metadata:` was refused at a true depth of one.
        for &(offset, is_sequence) in &facts.openings {
            let column = indent + offset;
            while stack.last().is_some_and(|&(open, kind)| {
                open > column || (open == column && kind && !is_sequence)
            }) {
                stack.pop();
            }
            match stack.last() {
                Some(&(open, kind)) if open == column && kind == is_sequence => {}
                _ => stack.push((column, is_sequence)),
            }
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
    /// `- ` element markers on this line. Only whether there are any matters to
    /// the caller; the levels they open are in `openings`.
    sequence_markers: usize,
    /// Collection levels this line names, in order, as `(offset from the line's
    /// first non-space byte, opened by a `- ` marker)`. A sequence is named at
    /// its marker's column and a mapping at its key's, which is what makes the
    /// bound independent of how the document is spelled.
    openings: Vec<(usize, bool)>,
    /// Deepest flow collection reached on this line. Flow nests like block, and
    /// a `:` inside one opens an implicit mapping that counts too.
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

// ============================================================
// F-8B-YAML — the 1.1-ambiguous plain-key ban (ruled (a), 2026-08-27)
// ============================================================
//
// The lookup-miss refusal in `SkillManifest::parse` covers one direction of
// cross-reader key drift: a spelling the PINNED parser resolves as a
// non-string never survives the lookup. This covers the other direction — a
// spelling that is a string HERE and a non-string to a YAML 1.1 reader, under
// which `yes: b` collapses onto a `y: a` twin and the later value silently
// wins. The duplicate check compares text, so that loss is invisible to it.
//
// The families are transliterated from the YAML 1.1 type-repository
// resolution regexes (yaml.org/type/{bool,int,float,timestamp}) — NEVER from
// the PyYAML oracle, which shares libyaml's scanner but resolves 1.1 and is
// wrong about this exact question in both directions (carryover row 12) —
// then narrowed twice, each cut measured rather than assumed:
//
// - to spellings `is_plain_key` admits: sexagesimals (`1:30`) carry a `:` and
//   are refused as `MalformedTopLevelKey` before any resolution question
//   arises, so a sexagesimal arm here would be unreachable;
// - minus spellings the pinned parser itself resolves, which already refuse
//   as `KeyNotInDocument`: measured against the pin (2026-08-27), that
//   excludes `true/false` casings, plain numerics, AND — beyond strict 1.2
//   core — `0b101` and `-0xff`. The tests pin those measurements, so a
//   narrower future pin surfaces as a red test, not a silent gap.
//
// What remains reachable: the 1.1 boolean case family, underscored numerics,
// and date-shaped tokens.

/// The y/yes/n/no/on/off casings of the 1.1 bool regex; the true/false
/// casings are omitted because the pinned parser resolves them itself.
fn yaml11_bool_not_pinned(key: &str) -> bool {
    matches!(
        key,
        "y" | "Y"
            | "yes"
            | "Yes"
            | "YES"
            | "n"
            | "N"
            | "no"
            | "No"
            | "NO"
            | "on"
            | "On"
            | "ON"
            | "off"
            | "Off"
            | "OFF"
    )
}

/// A 1.1 int or float spelled with `_` separators, which the pinned parser
/// keeps as a string. The 1.1 regexes require the body to START with a digit
/// (`_1` is a string in both readers), and the 1.1 float exponent requires a
/// sign, which `is_plain_key`'s charset cannot carry — so no exponent arm.
fn yaml11_underscored_number(key: &str) -> bool {
    let body = key.strip_prefix('-').unwrap_or(key);
    if !body.contains('_') || !body.starts_with(|c: char| c.is_ascii_digit()) {
        return false;
    }
    let compact: String = body.chars().filter(|&c| c != '_').collect();
    let all_digits = |s: &str| s.chars().all(|c| c.is_ascii_digit());
    let is_int = all_digits(&compact)
        || compact
            .strip_prefix("0x")
            .is_some_and(|hex| !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit()))
        || compact
            .strip_prefix("0b")
            .is_some_and(|bin| !bin.is_empty() && bin.chars().all(|c| matches!(c, '0' | '1')));
    let is_float = compact
        .split_once('.')
        .is_some_and(|(int, frac)| !int.is_empty() && all_digits(int) && all_digits(frac));
    is_int || is_float
}

/// The 1.1 timestamp regex's `ymd` arm. The full date-time arms carry `:` and
/// spaces, which `is_plain_key` already refuses.
fn yaml11_date(key: &str) -> bool {
    let bytes = key.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && [0usize, 1, 2, 3, 5, 6, 8, 9]
            .iter()
            .all(|&i| bytes[i].is_ascii_digit())
}

/// True when a 1.1 reader resolves this plain key as a non-string while the
/// pinned parser keeps it — the collapse hazard the ban exists for.
fn yaml11_ambiguous_key(key: &str) -> bool {
    yaml11_bool_not_pinned(key) || yaml11_underscored_number(key) || yaml11_date(key)
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
/// refused outright — bare `?` as well inside flow, where the parser's
/// dispatch table drops the blank requirement; a block scalar header ends the
/// line's YAML.
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
    // The last non-blank thing consumed was a closing quote. Blanks do not
    // clear it, because the parser reads `["a" :&x S]` exactly as `["a":&x S]`.
    let mut after_quoted = false;
    // The last non-blank thing consumed closed a flow collection. In block
    // prose `]` and `}` are ordinary characters — `see {this}` is a scalar —
    // so only a closer that actually decremented `flow` sets this.
    let mut after_flow_close = false;
    let mut flow: usize = 0;
    let mut flow_depth = 0usize;
    let mut sequence_markers = 0usize;
    let mut openings: Vec<(usize, bool)> = Vec::new();
    let mut node_offset = 0usize;
    let mut key_end: Option<usize> = None;
    let mut block_scalar = false;
    // The head of the line is a node start; so is every position below that
    // sets this back to true.
    let mut node_start = true;
    let mut i = 0usize;

    while i < chars.len() {
        let (at, c) = chars[i];
        let flow_before = flow;

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
                after_quoted = true;
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
            // The quoted scalar IS the node starting here. Skipping the
            // assignment left `node_offset` at the previous node start — behind
            // a `- ` marker, the marker's own column — so the mapping a quoted
            // key opens was named one level too far left, an over-refusal that
            // the pop rule above would have turned into a bound bypass.
            node_offset = at;
            quote = Some(c);
            node_start = false;
            i += 1;
            continue;
        }

        // Where the parser is between tokens: it skips blanks and consults its
        // dispatch table here, so an indicator in this position is a token to
        // it. `node_start` alone marks only the boundaries a node may OPEN at —
        // a closed quoted scalar and a closer that closed a flow are boundaries
        // too, and the parser reads them as such.
        let between_tokens = node_start || after_quoted || after_flow_close;

        // A `#` opening a comment ends the line. The rule is not "after a
        // blank": the parser skips to the next token before every fetch and
        // starts a comment at a bare `#` wherever it lands, so `[Read,#]` was a
        // comment to it and a plain scalar to a blank-only rule. This scan then
        // walked past the `#`, consumed a `]` the parser never saw, and ended
        // the line with `flow == 0` — the multi-line refusal below never fired,
        // the continuation was scanned as a fresh block line where `,` clears
        // `node_start`, and the alias behind it was RESOLVED into the record.
        // Mid-scalar is not a boundary, so `a#b` stays the plain scalar the
        // parser says it is.
        //
        // `i == 0` is the bounds guard for `chars[i - 1]`, not a third boundary
        // term: `node_start` starts true, so `between_tokens` already holds at
        // the head of the line and the index is never reached there. Deleting
        // it changes no verdict and no test can red it — it stops a panic that
        // the initializer currently makes unreachable, which is a guarantee
        // worth keeping local to this line.
        if c == '#' && (i == 0 || between_tokens || matches!(chars[i - 1].1, ' ' | '\t')) {
            break;
        }

        // Whitespace separates a node start from its node without ending it.
        if c == ' ' || c == '\t' {
            i += 1;
            continue;
        }

        // Where the node starting here begins. A level is named by its own
        // column — the sequence by its marker's, the mapping by its key's — so
        // that `- - bb: 1` and the same document written down the page measure
        // the same three levels.
        if node_start {
            node_offset = at;
        }

        if between_tokens {
            match c {
                '&' => return Err(SkillError::UnsupportedYaml("anchor")),
                '*' => return Err(SkillError::UnsupportedYaml("alias")),
                '!' => return Err(SkillError::UnsupportedYaml("tag")),
                '{' => return Err(SkillError::UnsupportedYaml("flow mapping")),
                '<' if chars.get(i + 1).map(|&(_, n)| n) == Some('<') => {
                    return Err(SkillError::UnsupportedYaml("merge key"));
                }
                // A deliberate duplicate of the in-flow refusal below: nothing
                // between here and there mutates `c` or `flow`, so no input can
                // reach one arm and not the other. Kept so `[` appears in this
                // indicator table rather than being refused two screens away.
                '[' if flow > 0 => return Err(SkillError::UnsupportedYaml("flow sequence node")),
                // `? ` opens a node exactly as `- ` does, and had no arm here:
                // `?` fell through to the catch-all, cleared `node_start`, and
                // the whitespace skip below does not restore it — so the anchor
                // in `? &s SECRET` was never examined. The blank after it is a
                // BLOCK-context requirement: inside a flow collection the
                // parser's dispatch table takes bare `?` as a KEY token, so
                // `[?&anc S]` was an explicit key to the parser and a plain
                // scalar to this scan, and the anchor rode through again — one
                // indicator over from the quoted-key spelling of the same
                // escape.
                '?' if flow > 0
                    || matches!(
                        chars.get(i + 1).map(|&(_, n)| n),
                        None | Some(' ') | Some('\t')
                    ) =>
                {
                    return Err(SkillError::UnsupportedYaml("explicit key"));
                }
                // A block scalar header ends the line's YAML; what follows is
                // literal text that `scan_subset` steps over. Unlike every arm
                // above it changes this scan's state instead of refusing it, so
                // it keeps the narrower `node_start` it has always had: a
                // header opens a node, and after `]` or a closed quoted scalar
                // no node can open. No test pins the difference because none
                // can — a `|` in either of those positions parses nowhere, so
                // both spellings refuse the same documents. Written down rather
                // than left as a silent widening.
                '|' | '>' if node_start && flow == 0 => {
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
            // Only where a node begins, exactly as `{` already is. A block
            // context plain scalar may contain `[` — the parser reads
            // `a[&b c]` as the text `a[&b c]`, anchor and all — so opening flow
            // at any position refused ordinary prose: `values in [0, 1)` came
            // back as a multi-line flow collection. The in-flow refusals above
            // still fire, because reaching them requires flow to be open.
            '[' if node_start => {
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
                // line. `12:30` and `http://x` are scalars, not mappings. Two
                // exceptions, both in flow context only, both the same parser
                // rule: the dispatch table takes bare `:` as a VALUE token
                // whenever a flow is open, and it is consulted after a quoted
                // scalar and wherever a node could start. So `["a":&x S]` is a
                // mapping, blanks before the `:` or not, and the anchor was
                // never at what this scan considered a node start — the parser
                // RESOLVED its alias into a second key's evidence. And in
                // `[:&a S]` no document parses — a value with no key is a
                // grammar error — but the parser scanned the anchor as an
                // ANCHOR TOKEN before refusing, which is what this gate exists
                // to keep away from the transpiled C. Mid-scalar the `:` is
                // absorbed by the plain-scalar rule, so `[a:1]` stays the
                // scalar the parser says it is.
                let next = chars.get(i + 1).map(|&(_, n)| n);
                if next.is_none()
                    || matches!(next, Some(' ') | Some('\t'))
                    || ((after_quoted || node_start) && flow > 0)
                {
                    if flow == 0 {
                        if key_end.is_none() {
                            key_end = Some(at);
                        }
                        // The mapping this key belongs to opens at the key, not
                        // at the line's indentation.
                        openings.push((node_offset, false));
                    } else {
                        // An implicit mapping inside a flow collection is a
                        // level with no column of its own. Uncounted, the flow
                        // spelling `- [bb: 1]` measured one level fewer than the
                        // block spelling of the same document.
                        flow_depth = flow_depth.max(flow + 1);
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
                    openings.push((node_offset, true));
                    // `- ` opens an element, so what follows is a node start.
                } else {
                    node_start = false;
                }
            }
            _ => node_start = false,
        }
        after_quoted = false;
        after_flow_close = matches!(c, ']' | '}') && flow_before > 0;
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
        openings,
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
        // Every other recognized string carries its own bound; this one fell
        // closed only at the 8 KiB frontmatter limit, which is a different
        // question with a different answer.
        let compatibility = string("compatibility", take("compatibility"))?
            .map(|v| bounded("compatibility", v, SKILL_COMPATIBILITY_MAX_BYTES))
            .transpose()?;

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
            // This scan reads a key as text; the parser resolves it as a YAML
            // scalar, and the two disagree on every spelling whose text is not
            // a string. `12:` is the integer 12 to the parser, `null:` is null,
            // `1e3:` is a float — so a lookup for the string `"12"` finds
            // nothing and the field was recorded with an EMPTY value. (An
            // earlier version of this comment claimed `2026-08-19:` is a date
            // to the parser; measured 2026-08-27, it is not — the pin keeps it
            // as a string, which is exactly why dates are in the
            // `yaml11_ambiguous_key` ban instead of relying on this miss.)
            // That is
            // the silently dropped field this module's header rules out, one
            // class over from the quoted key `is_plain_key` already refuses for
            // exactly the same reason.
            //
            // Refusing on the miss covers every spelling without this file
            // carrying a copy of YAML's scalar resolution rules — which is the
            // point, because that copy would be wrong. Resolution is the
            // reader's, not the scanner's, and readers disagree in BOTH
            // directions: the pinned crate resolves the 1.2 core schema, so
            // `0o17` and `1e3` are numbers here and strings to a YAML 1.1
            // reader, while `yes`, `on` and `2026-08-19` are strings here and a
            // boolean, a boolean and a date there. This check asks only whether
            // the parser kept the key this scan saw, so it keeps holding when
            // the pin's answer moves.
            let value = take(&key).ok_or_else(|| SkillError::KeyNotInDocument(key.clone()))?;
            opaque.push(OpaquePortableField {
                key,
                value: render_opaque(&value),
            });
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
    fn yaml11_ambiguous_plain_keys_are_refused_per_family() {
        // F-8B-YAML ruled (a): one arm per derived family, so deleting a
        // family's predicate reds its own named case rather than a shared one.
        // Family 1 — the 1.1 boolean case family.
        assert_eq!(
            manifest("yes: a"),
            Err(SkillError::Yaml11AmbiguousKey("yes".into()))
        );
        assert_eq!(
            manifest("ON: a"),
            Err(SkillError::Yaml11AmbiguousKey("ON".into()))
        );
        // Family 2 — underscored numerics (int and float arms).
        assert_eq!(
            manifest("1_000: a"),
            Err(SkillError::Yaml11AmbiguousKey("1_000".into()))
        );
        assert_eq!(
            manifest("1_0.5: a"),
            Err(SkillError::Yaml11AmbiguousKey("1_0.5".into()))
        );
        // Family 3 — date-shaped tokens.
        assert_eq!(
            manifest("2026-08-19: a"),
            Err(SkillError::Yaml11AmbiguousKey("2026-08-19".into()))
        );

        // The ban reaches sub-level plain keys too — `metadata:`'s mapping is
        // where a 1.1 reader collapses siblings just as readily.
        assert_eq!(
            manifest("metadata:\n  on: a"),
            Err(SkillError::Yaml11AmbiguousKey("on".into()))
        );
        // A QUOTED sub-key spelling of the same word stays legal, so the word
        // itself is still expressible where quoting is available.
        let quoted = manifest("name: review-diff\ndescription: d.\nmetadata:\n  \"on\": a")
            .expect("quoted sub-key is legal");
        assert_eq!(quoted.metadata.get("on").map(String::as_str), Some("a"));

        // Near-misses stay legal: not 1.1-resolvable, no refusal.
        for near_miss in ["yesterday", "v1_2", "_1", "2026-08-1"] {
            manifest(&format!(
                "name: review-diff\ndescription: d.\n{near_miss}: a"
            ))
            .unwrap_or_else(|e| panic!("{near_miss} must stay legal: {e}"));
        }
    }

    #[test]
    fn the_ban_list_is_cut_where_other_refusals_already_hold() {
        // Each excluded family is excluded because a DIFFERENT arm refuses it,
        // and this test names that arm — if either measurement moves (a
        // narrower pin, a widened key charset), the spelling would start
        // parsing clean and the assert here is what notices.
        //
        // Sexagesimals never reach resolution: `:` fails `is_plain_key`.
        assert_eq!(manifest("1:30: a"), Err(SkillError::MalformedTopLevelKey));
        // The pinned parser resolves these itself (measured 2026-08-27), so
        // the lookup-miss refusal owns them — broader than strict 1.2 core.
        for key in ["0b101", "-0xff", "true"] {
            assert_eq!(
                manifest(&format!("name: review-diff\ndescription: d.\n{key}: a")),
                Err(SkillError::KeyNotInDocument(key.to_owned())),
                "{key}"
            );
        }
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
            // `[` was the one indicator missing from this list, and it opened
            // flow state at any position while `{` was already position-gated.
            // The parser reads every line below as an ordinary plain scalar —
            // `a[&b c]` is the text `a[&b c]`, anchor and all — so refusing
            // them named this scanner's state rather than the document.
            "name: n\ndescription: d\nnote: accepts values in [0, 1) inclusive",
            "name: n\ndescription: d\nnote: a range of 1 [to 5",
            "name: n\ndescription: d\nnote: see array[0] for the index",
            "name: n\ndescription: d\nnote: a[&b c]",
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
        // The tail arm itself: junk after the indicator is refused by name, and
        // a comment there is not junk. Until now nothing red when this arm was
        // replaced with `Ok(())` — the parser refuses such a header on its own,
        // so the only delta was a named refusal degrading to a shapeless one,
        // which is exactly the drift a guard's own test exists to notice.
        assert_eq!(
            manifest("name: n\ndescription: d\nnote: | junk\n  text").expect_err("refused"),
            SkillError::UnsupportedYaml("block scalar header")
        );
        let skill = manifest("name: n\ndescription: d\nnote: | # a comment\n  text")
            .expect("a comment after the header is not junk");
        assert_eq!(skill.opaque[0].value, "text");
    }

    #[test]
    fn a_block_scalar_behind_a_sequence_marker_is_refused() {
        // The skip floor was the LINE's indentation, which is the column of the
        // node owning the scalar only when nothing on the line moved it right.
        // In `  - k: |` the line indents 2 while the mapping holding `k` starts
        // at 4, and the parser ends the scalar at the content indentation it
        // detects, 8. A sibling at column 4 satisfies 4 > 2 and was skipped
        // here, while 4 < 8 kept it live there — so these hid an anchor, an
        // alias, a tag, a merge key and four levels of nesting from the scan,
        // and the parser RESOLVED the alias across two top-level keys.
        for front in [
            "name: n\ndescription: d\naa:\n  - k: |\n      t\n    m: &zz S\nbb:\n  - k: |\n      t\n    m: *zz",
            "name: n\ndescription: d\naa:\n  - k: |\n      t\n    m: !!str 5",
            "name: n\ndescription: d\naa:\n  - k: |\n      t\n    <<: {a: 1}",
            "name: n\ndescription: d\naa:\n  - k: |\n      t\n    p:\n      q:\n        r:\n          s: 1",
            // The folded spelling reaches it identically.
            "name: n\ndescription: d\naa:\n  - k: >\n      t\n    m: &zz S",
        ] {
            assert_eq!(
                manifest(front).expect_err("refused"),
                SkillError::UnsupportedYaml("block scalar behind a sequence marker"),
                "{front}"
            );
        }
        // The two controls that isolate the cause to the marker offset alone:
        // the same anchor without the block scalar, and the same block scalar
        // without the marker, are both already refused for their own reasons.
        assert_eq!(
            manifest("name: n\ndescription: d\naa:\n  - k: t\n    m: &zz S").expect_err("refused"),
            SkillError::UnsupportedYaml("anchor")
        );
        assert_eq!(
            manifest("name: n\ndescription: d\naa:\n  k: |\n    t\n  m: &zz S")
                .expect_err("refused"),
            SkillError::UnsupportedYaml("anchor")
        );
        // And the accepted side is untouched: with no marker on the header line
        // the skip floor IS the owning node's column, by construction.
        manifest("name: n\ndescription: d\nmetadata:\n  note: |\n    text")
            .expect("an indented block scalar with no marker still parses");
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
        // A `key:` sharing a line with a `- ` marker opens a mapping at a column
        // no line announces. Left uncounted, these byte-different but
        // semantically identical twins measured two levels and three, and the
        // error compounded: the last document below carries five containers
        // against a bound of two.
        for front in [
            "name: n\ndescription: d\naa:\n  - - bb: 1",
            "name: n\ndescription: d\naa:\n  -\n    -\n      bb: 1",
            "name: n\ndescription: d\naa:\n  - bb:\n      cc: 1",
            "name: n\ndescription: d\naa:\n  - bb:\n      - cc: 1",
        ] {
            assert_eq!(
                manifest(front).expect_err("refused"),
                SkillError::TooDeeplyNested,
                "{front}"
            );
        }
        // The accepted control for that arm, so it is not "refuse any item
        // carrying a key": one mapping inside one sequence item is two levels.
        manifest("name: n\ndescription: d\naa:\n  - bb: 1")
            .expect("a mapping inside a sequence item is within the bound");
        // A flow collection nests exactly as its block spelling does. This pair
        // disagreed: the flow one was accepted at three levels while the block
        // one was refused at three.
        for front in [
            "name: n\ndescription: d\nm:\n  a:\n    b: [1, 2]",
            "name: n\ndescription: d\nm:\n  a:\n    b:\n    - 1",
            // A flow collection's implicit mapping is a level with no column of
            // its own. Counting only the brackets measured this one level fewer
            // than `aa:\n  - - bb: 1`, the block spelling refused above.
            "name: n\ndescription: d\naa:\n  - [bb: 1]",
        ] {
            assert_eq!(
                manifest(front).expect_err("refused"),
                SkillError::TooDeeplyNested,
                "{front}"
            );
        }
    }

    #[test]
    fn depth_counts_open_levels_rather_than_sequence_items() {
        // The level a mapping opens inside a sequence item used to be pushed at
        // the LINE's indentation, where no later line could pop it, so every
        // further item stacked two more: a list of mappings was accepted at one
        // entry and refused at two. An `authors:` list is the ordinary spelling
        // of that, and refusing it also made an unrecognized portable field
        // fatal — which this module's header rules out, because upstream
        // shipping a new field is not an attack.
        for front in [
            "name: n\ndescription: d\nextra:\n  - k: 1\n  - k: 2",
            "name: n\ndescription: d\nextra:\n  - k: 1\n  - k: 2\n  - k: 3",
            "name: n\ndescription: d\nauthors:\n  - name: a\n  - name: b",
            // One item carrying two keys reached it the same way: the
            // continuation key opened a level the item's own push had claimed.
            "name: n\ndescription: d\nextra:\n  - k: 1\n    j: 2",
            // And at column zero the item line was read as a key, sliced to
            // `- k`, and refused for the space — while `- 1` was accepted.
            "name: n\ndescription: d\nextra:\n- k: 1\n- k: 2",
        ] {
            manifest(front).unwrap_or_else(|e| panic!("{front}\nrefused as {e:?}"));
        }
        // The bound still bites on the accepted shape above, so this is not
        // "stop counting what sequences hold".
        assert_eq!(
            manifest("name: n\ndescription: d\nextra:\n  - k:\n      j:\n        i: 1")
                .expect_err("refused"),
            SkillError::TooDeeplyNested
        );
    }

    #[test]
    fn a_column_zero_sequence_closes_when_a_mapping_names_its_column() {
        // The pop was strictly `open > column`, so nothing pushed at column
        // zero was ever poppable and the arm below it pushed on a kind
        // mismatch instead of closing: every construct after a column-zero
        // sequence stacked on top of it, and a fully portable manifest —
        // `allowed-tools:` as a column-zero list followed by `metadata:` — was
        // refused at a true depth of one. Every other column-zero sequence in
        // the corpus was the last construct in its document, which is why the
        // suite could not see it.
        let m = manifest(
            "name: n\ndescription: d\nallowed-tools:\n- Read\n- Grep\nmetadata:\n  team: infra",
        )
        .expect("four portable-core keys at a true depth of one");
        let claimed: Vec<&str> = m
            .allowed_tools
            .claimed()
            .iter()
            .map(String::as_str)
            .collect();
        assert_eq!(claimed, ["Read", "Grep"]);
        assert_eq!(m.metadata.get("team").map(String::as_str), Some("infra"));
        // The block and flow spellings of the same document agree again.
        manifest("name: n\ndescription: d\naa:\n- 1\nbb:\n  cc: 1").expect("block spelling");
        manifest("name: n\ndescription: d\naa: [1]\nbb:\n  cc: 1").expect("flow spelling");
        manifest("name: n\ndescription: d\naa:\n- 1\nbb: 2").expect("a scalar key after");
        // Genuinely deep documents still refuse, so the pop is directional,
        // not disabled.
        assert_eq!(
            manifest("name: n\ndescription: d\nmetadata:\n  a:\n    b:\n      c: 1")
                .expect_err("refused"),
            SkillError::TooDeeplyNested
        );
    }

    #[test]
    fn a_quoted_key_is_named_at_its_own_column() {
        // The quote branch cleared `node_start` without recording where the
        // node began, so a quoted key behind a `- ` marker kept the marker's
        // column. At the old pop rule that was an over-refusal — the quoted
        // spelling of an accepted document refused — and under the directional
        // pop it would have become a bound BYPASS, the mapping named at the
        // marker's column popping the sequence that is genuinely open.
        manifest("name: n\ndescription: d\nauthors:\n  - \"q\": 1\n    k: 2")
            .expect("the quoted twin of an accepted document");
        // The deep pair agrees in the other direction: both spellings refuse.
        for front in [
            "name: n\ndescription: d\nx:\n- \"q\":\n    r:\n      s: 1",
            "name: n\ndescription: d\nx:\n- q:\n    r:\n      s: 1",
        ] {
            assert_eq!(
                manifest(front).expect_err("refused"),
                SkillError::TooDeeplyNested,
                "{front}"
            );
        }
    }

    #[test]
    fn a_quoted_flow_key_is_a_key() {
        // The parser takes `:` after a JSON-style quoted flow key as a key
        // indicator with no space required, so `&`, `*` and `!` behind one
        // were never at what the scan considered a node start — and the pinned
        // parser RESOLVED the alias, handing one key's evidence content its
        // author wrote under another.
        for (front, want) in [
            ("name: n\ndescription: d\nfirst: [\"a\":&anc S]", "anchor"),
            ("name: n\ndescription: d\nfirst: ['a':*anc]", "alias"),
            ("name: n\ndescription: d\nfirst: [\"a\":!!str 1]", "tag"),
            // Blanks between the quote and the `:` do not change the reading.
            ("name: n\ndescription: d\nfirst: [\"a\" :*anc]", "alias"),
        ] {
            assert_eq!(
                manifest(front).expect_err("refused"),
                SkillError::UnsupportedYaml(want),
                "{front}"
            );
        }
        // The controls are plain SCALARS to the parser, ampersand and all: a
        // `:` not behind a quote still needs its whitespace to be a key.
        manifest("name: n\ndescription: d\nx: [a:1]").expect("a plain scalar with a colon");
        manifest("name: n\ndescription: d\nx: [a :&anc S]")
            .expect("a plain scalar with a blank and an ampersand");
        // And the implicit mapping a quoted key opens is COUNTED, so the
        // quoted and plain spellings of one document measure the same depth.
        for front in [
            "name: n\ndescription: d\nextra:\n  m: [\"a\":1]",
            "name: n\ndescription: d\nextra:\n  m: [a: 1]",
        ] {
            assert_eq!(
                manifest(front).expect_err("refused"),
                SkillError::TooDeeplyNested,
                "{front}"
            );
        }
    }

    #[test]
    fn an_explicit_key_indicator_in_flow_is_refused() {
        // The blank this scan required after `?` is a block-context rule. The
        // parser's dispatch table fetches a KEY token on bare `?` whenever a
        // flow collection is open, so everything the indicator table refuses
        // rode in behind `[?` at depth one: the anchored key resolved into a
        // second key's evidence, the tag applied, and the merge indicator
        // reached the parser.
        for front in [
            "name: n\ndescription: d\nfirst: [?&anc SECRET]",
            "name: n\ndescription: d\nfirst: [?*anc]",
            "name: n\ndescription: d\nfirst: [?!!str 1]",
            "name: n\ndescription: d\nfirst: [?<<]",
            // A comma reopens a node start, so the relaxation fires mid-list.
            "name: n\ndescription: d\nfirst: [x, ?&anc S]",
            "name: n\ndescription: d\nfirst: [?q]",
        ] {
            assert_eq!(
                manifest(front).expect_err("refused"),
                SkillError::UnsupportedYaml("explicit key"),
                "{front}"
            );
        }
        // In block context the blank rule is the parser's own: `?what` is a
        // plain scalar there, not a key.
        manifest("name: n\ndescription: d\nx: ?what").expect("a block-context scalar");
    }

    #[test]
    fn a_value_indicator_at_a_flow_node_start_guards_its_node() {
        // The dispatch table's other flow relaxation, and with `?` fixed the
        // last one it holds: bare `:` fetches a VALUE token whenever a flow is
        // open, consulted wherever a node could start. No document in this
        // class parses — a value with no key is a grammar error — but the
        // anchor behind the `:` was scanned as an ANCHOR TOKEN by the
        // transpiled parser before the grammar refused, so the `:` takes the
        // indicator path here and the anchor is examined where it stands.
        for (front, want) in [
            ("name: n\ndescription: d\nx: [:&a S]", "anchor"),
            ("name: n\ndescription: d\nx: [w, :*a]", "alias"),
        ] {
            assert_eq!(
                manifest(front).expect_err("refused"),
                SkillError::UnsupportedYaml(want),
                "{front}"
            );
        }
        // Not at a node start the `:` is scalar content, exactly as the
        // parser reads it.
        manifest("name: n\ndescription: d\nx: [a:v]").expect("a plain scalar with a colon");
    }

    #[test]
    fn reserved_and_block_only_indicators_in_flow_refuse_downstream() {
        // The rest of the dispatch table, swept so the two rows above are
        // provably its only flow relaxations: `|`, `>`, `%`, `@` and backtick
        // cannot start any token in flow — the parser refuses the document
        // itself — so a line this scan accepts hands it nothing it resolves.
        for front in [
            "name: n\ndescription: d\nx: [| v]",
            "name: n\ndescription: d\nx: [> v]",
            "name: n\ndescription: d\nx: [%v]",
            "name: n\ndescription: d\nx: [@v]",
            "name: n\ndescription: d\nx: [`v]",
        ] {
            manifest(front).expect_err(front);
        }
    }

    #[test]
    fn a_unicode_space_before_a_key_is_refused_not_renamed() {
        // The indentation run is ASCII and the parser agrees — U+00A0 is
        // content to both — but `str::trim` swallowed it, so the key was
        // scanned under a name the document does not contain: the real field
        // dropped out of the record, a phantom opaque key stood in its place,
        // a tool claim went invisible, and the `metadata:` spelling skipped
        // the GridWork namespace's loud refusal.
        for front in [
            "name: n\ndescription: d\n\u{a0}license: PROPRIETARY",
            "name: n\ndescription: d\n\u{a0}allowed-tools:\n  - Read",
            "name: n\ndescription: d\n\u{a0}metadata:\n  team: x",
            // The mid-key spelling, pinned as the consistency control.
            "name: n\ndescription: d\nlic\u{a0}ense: MIT",
        ] {
            assert_eq!(
                manifest(front).expect_err("refused"),
                SkillError::MalformedTopLevelKey,
                "{front}"
            );
        }
    }

    #[test]
    fn a_comma_in_block_context_prose_is_content() {
        // `,` opens a node only inside a flow collection. Set unconditionally,
        // it would put the characters the indicator table refuses at a node
        // start inside ordinary prose — `Read, & write` refused as an anchor.
        let m = manifest("name: n\ndescription: d\nlicense: Read, & write")
            .expect("prose with a comma before an ampersand");
        assert_eq!(m.license.as_deref(), Some("Read, & write"));
        manifest("name: n\ndescription: d\nnote: hello, *world* is emphasis")
            .expect("prose with a comma before an asterisk");
    }

    #[test]
    fn a_comment_opens_wherever_the_parser_is_between_tokens() {
        // The parser skips to its next token before every fetch, and a bare `#`
        // there starts a comment — no preceding blank required. A blank-only
        // rule read `[Read,#]` as a plain scalar, walked past the `#`, consumed
        // a `]` the parser had inside a comment, and left the line at
        // `flow == 0`: the multi-line refusal never fired, and the continuation
        // was rescanned as a fresh block line where `,` clears `node_start`, so
        // the alias behind it was RESOLVED into the record. Every spelling of
        // the hole is the same hole — after `[`, after `,`, after a flow key's
        // `:`, and after a closed quoted scalar.
        for front in [
            "name: n\ndescription: d\nallowed-tools: [Read,#]\n  X, &s SECRET, *s]",
            "name: n\ndescription: d\nallowed-tools: [#]\n  Read, &s S, *s]",
            "name: n\ndescription: d\nallowed-tools: [Read,#]\n  X, !!str 7]",
            "name: n\ndescription: d\nallowed-tools: [Read,#]\n  X, <<]",
            "name: n\ndescription: d\nallowed-tools: [a, \"b\"#]\n  , &s S, *s]",
            "name: n\ndescription: d\nallowed-tools: [\"k\":#]\n  S]",
            "name: n\ndescription: d\nmetadata:\n  team: [a,#]\n    b, &s S, *s]",
        ] {
            assert_eq!(
                manifest(front).expect_err("refused"),
                SkillError::UnsupportedYaml("multi-line flow collection"),
                "{front}"
            );
        }
        // Mid-scalar is not a boundary, and the parser agrees: these are the
        // documents a comment rule that fired anywhere would have refused.
        let m = manifest("name: n\ndescription: see http://x/y#z\nlicense: a#b")
            .expect("a fragment and a hash inside plain scalars");
        assert_eq!(m.description, "see http://x/y#z");
        assert_eq!(m.license.as_deref(), Some("a#b"));
        let m = manifest("name: n\ndescription: d\nallowed-tools: [a#b, c]")
            .expect("a hash inside a flow scalar");
        assert_eq!(m.allowed_tools.claimed(), ["a#b", "c"]);
        // A closed flow at depth one leaves nothing open, so a comment after it
        // ends a line that was already complete.
        manifest("name: n\ndescription: d\nallowed-tools: [a]#trail").expect("a closed flow");
    }

    #[test]
    fn an_indicator_after_a_closed_node_is_refused_by_name() {
        // `node_start` marks where a node may OPEN, which is narrower than
        // where the parser is between tokens: a closed quoted scalar and a
        // closer that closed a flow are boundaries too, and the dispatch table
        // fetches ANCHOR/ALIAS/TAG there unconditionally. No document in this
        // class parses, but the transpiled scanner ran on the bytes before the
        // grammar refused — the same reason the `:` at a flow node start takes
        // the indicator path.
        for (front, want) in [
            ("name: n\ndescription: d\nx: [\"a\" &s S]", "anchor"),
            ("name: n\ndescription: d\nx: [\"a\" *s]", "alias"),
            ("name: n\ndescription: d\nx: [\"a\" !!str 1]", "tag"),
            ("name: n\ndescription: d\nx: \"v\" &s S", "anchor"),
            ("name: n\ndescription: d\nx: [a] &s S", "anchor"),
        ] {
            assert_eq!(
                manifest(front).expect_err("refused"),
                SkillError::UnsupportedYaml(want),
                "{front}"
            );
        }
        // A `]` or `}` in block prose closed nothing, so it is not a boundary
        // and what follows is content — the reason the flag is set only by a
        // closer that decremented the depth.
        let m = manifest("name: n\ndescription: d\nlicense: JSON like {\"a\":1}{\"b\":2} ok")
            .expect("adjacent braces in prose");
        assert_eq!(
            m.license.as_deref(),
            Some("JSON like {\"a\":1}{\"b\":2} ok")
        );
        manifest("name: n\ndescription: d\nx: [\"a\", b]").expect("a quoted flow entry");
    }

    #[test]
    fn a_key_the_parser_does_not_carry_as_a_string_is_refused_by_name() {
        // `is_plain_key` admits every one of these as text, and the parser
        // resolves none of them to the string this scan read. The lookup then
        // missed and the field was recorded with an EMPTY value.
        for key in [
            "12", "-7", "1.5", "1e3", "0x1f", "0o17", ".inf", ".nan", "true", "false", "True",
            "TRUE", "null", "Null", "NULL",
        ] {
            assert_eq!(
                manifest(&format!("name: n\ndescription: d\n{key}: v")).expect_err("refused"),
                SkillError::KeyNotInDocument(key.to_owned()),
                "{key}"
            );
        }
        // A spelling the pin carries as a string survives the lookup, so this
        // refusal tracks the parser's resolution rather than a guess at what
        // looks like a scalar.
        //
        // This arm used to accept `y`, `yes`, `on` and `2026-08-19` here and
        // recorded that acceptance as the guard's honest limit — those are
        // strings to the pin and a boolean, a boolean, a boolean and a date to
        // a YAML 1.1 reader. F-8B-YAML (ruled (a), 2026-08-27) closed that
        // limit: the 1.1-ambiguous spellings now refuse in `scan_subset`
        // before the parser runs, per-family coverage in
        // `yaml11_ambiguous_plain_keys_are_refused_per_family`.
        let m = manifest("name: n\ndescription: d\nv12: a").expect("string keys");
        assert_eq!(
            m.opaque
                .iter()
                .map(|f| (f.key.as_str(), f.value.as_str()))
                .collect::<Vec<_>>(),
            [("v12", "a")]
        );
    }

    #[test]
    fn a_comment_after_a_blank_ends_the_line() {
        // The boundary terms do not reach here: after a plain scalar nothing is
        // open, so a blank before the `#` is the only thing that makes this a
        // comment. Dropped, the scan reads the comment text as YAML — and the
        // `: ` inside it opens a node start, where the anchor behind it is
        // refused on a document the parser accepts with `description: hello`.
        let m = manifest("name: n\ndescription: hello # note: &anchor here")
            .expect("a trailing comment");
        assert_eq!(m.description, "hello");
    }

    #[test]
    fn a_closed_quoted_scalar_stops_being_a_boundary_at_the_next_character() {
        // `after_quoted` marks the position immediately after a closing quote
        // and is cleared at the end of every iteration. Left set, it makes the
        // rest of the line a boundary, and the indicator table then refuses an
        // ordinary plain scalar: `b!c` is a flow entry to the parser, because
        // `!` opens a tag only at the head of a node.
        let m = manifest("name: n\ndescription: d\nallowed-tools: [\"a\", b!c]")
            .expect("a plain entry after a quoted one");
        assert_eq!(m.allowed_tools.claimed(), ["a", "b!c"]);
    }

    #[test]
    fn a_whole_line_comment_is_not_a_key() {
        // `scan_subset` skips a comment line before `scan_line` ever sees it —
        // the third comment rule in this file. Without the skip the line
        // reaches the walk, breaks at the `#` with no key behind it, and a
        // document the parser accepts is refused as a malformed top-level key.
        let m = manifest("name: n\n# a note\ndescription: d\nmetadata:\n  # nested\n  team: infra")
            .expect("comment lines");
        assert_eq!(m.description, "d");
        assert_eq!(m.metadata.get("team").map(String::as_str), Some("infra"));
        assert!(m.opaque.is_empty(), "a comment is not an opaque field");
    }

    #[test]
    fn an_oversized_opaque_value_is_truncated_on_a_character_boundary() {
        // Evidence is bounded and lossy by design. The bound is in BYTES and
        // the truncation walks back to a character boundary, so a multi-byte
        // value cuts below the bound rather than at it — and unbounded, this
        // field is a byte pump into every record that carries a manifest.
        let wide = "\u{2603}".repeat(SKILL_OPAQUE_VALUE_MAX_BYTES); // 3 bytes each
        let m = manifest(&format!("name: n\ndescription: d\nvendor: {wide}")).expect("oversized");
        let [field] = &m.opaque[..] else {
            panic!("one opaque field, got {:?}", m.opaque)
        };
        assert_eq!(field.key, "vendor");
        assert_eq!(field.value.len(), 1023, "3 does not divide the bound");
        assert!(field.value.len() <= SKILL_OPAQUE_VALUE_MAX_BYTES);
        assert!(field.value.chars().all(|c| c == '\u{2603}'));
    }

    #[test]
    fn a_leading_sequence_with_no_key_above_is_refused_by_name() {
        // Without the guard its job falls to the deserializer, which reports
        // a shapeless `Malformed` — the refusal this gate exists to replace
        // with a named one.
        assert_eq!(
            manifest("- 1\nname: n").expect_err("refused"),
            SkillError::MalformedTopLevelKey
        );
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
