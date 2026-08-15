//! The command line, parsed by hand.
//!
//! No argument-parsing dependency, and that is a decision rather than an
//! omission. The tree is closed for v1 and flat — a verb, sometimes a subverb, a
//! positional or two, and a handful of named flags. List verbs choose a table
//! only after parsing, when stdout's terminal state is known; their wire answer
//! remains JSON. What is left here is still a `match`.
//!
//! Flags and positionals are separated in ONE pass against the closed set of
//! flag names, before any verb is resolved. That is what makes their order free
//! — `projection list --limit 5 task` and `projection list task --limit 5` are
//! the same invocation — and it is what lets a flag nobody claimed be refused
//! rather than ignored: a mistyped `--cursor` becomes exit 2 instead of a
//! silent full-table read.

use std::path::PathBuf;

use gwk_domain::blob::BlobAddress;
use gwk_domain::entity::Budget;
use gwk_domain::ids::{CostMicros, Seq};
use gwk_domain::ingestion::IngestionKind;
use gwk_domain::protocol::ProjectionKind;
use gwk_tui::board::BoardView;
use gwk_tui::hall::MotionMode;

use crate::admin::Retention;
use crate::exit::Failure;

/// Every flag in the tree that takes a value.
const VALUE_FLAGS: &[&str] = &[
    "--aggregate-type",
    "--archive-manifest-sha256",
    "--base",
    "--body",
    "--body-file",
    "--cursor",
    "--cutover-id",
    "--event-type",
    "--expected-version",
    "--file",
    "--from",
    "--head",
    "--key",
    "--kind",
    "--limit",
    "--media-type",
    "--max-cost-micros",
    "--max-tokens",
    "--max-tool-calls",
    "--max-wall-ms",
    "--motion",
    "--output",
    "--project",
    "--reason",
    "--repo",
    "--resolution",
    "--scratch-database",
    "--strategy",
    "--title",
    "--to",
    "--until",
    "--view",
];

/// Every flag that is its own answer.
const SWITCHES: &[&str] = &[
    "--dry-run",
    "--help",
    "--json",
    "--pretty",
    "--version",
    "-V",
    "-h",
];

/// Rows an event page asks for when the caller names no limit.
const DEFAULT_EVENT_LIMIT: u32 = 256;

/// Where a JSON document comes from.
#[derive(Debug, PartialEq, Eq)]
pub enum Source {
    Stdin,
    File(PathBuf),
}

/// Where blob bytes go.
#[derive(Debug, PartialEq, Eq)]
pub enum Sink {
    Stdout,
    File(PathBuf),
}

/// One invocation, resolved.
#[derive(Debug, PartialEq, Eq)]
pub struct Invocation {
    pub verb: Verb,
    /// Formatting only. Never content — a pretty answer and a compact one decode
    /// to the same value.
    pub pretty: bool,
    /// Keep the protocol JSON even when stdout is a terminal.
    pub json: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Verb {
    Help,
    BuildInfo,

    /// The service. The only verb that does not return.
    Daemon,
    AdminInit,
    AdminVerify,
    AdminRebuildProjections {
        scratch: String,
    },
    AdminBlob {
        what: crate::admin::Retention,
    },
    /// The machine half of a driving-window receipt. `to` absent means the
    /// database's clock at read time.
    AdminReceipt {
        from: String,
        to: Option<String>,
    },

    Health,
    Status,
    Watermark,
    VerifySealed,
    Activate {
        cutover_id: String,
        manifest_sha256: String,
    },

    CommandSubmit {
        source: Source,
    },

    ProjectionGet {
        kind: ProjectionKind,
        id: String,
    },
    ProjectionList {
        kind: ProjectionKind,
        cursor: Option<String>,
        limit: Option<u32>,
    },

    EventRead {
        cursor: Option<Seq>,
        limit: u32,
    },
    EventFollow {
        cursor: Option<Seq>,
    },
    EventTail {
        cursor: Option<Seq>,
        aggregate_type: Option<String>,
        event_type: Option<String>,
        /// Which Board panel opens first. Every panel remains one keystroke
        /// away once the loop is up; this only names the opening one.
        view: BoardView,
        /// Three entry verbs open ONE workspace; each accepts the same
        /// motion dial instead of hardcoding a policy per spelling.
        motion: MotionMode,
    },

    /// A coherent fold over the projections that describe current work.
    EstateOverview,
    /// The newest projection facts and the work those same pages leave owed.
    ActivityBrief,
    CostRollup,
    AgentFleet,
    AttemptStop {
        id: String,
    },
    AttemptBudget {
        id: String,
    },
    AttemptBudgetUpdate {
        id: String,
        expected_version: u32,
        budget: Budget,
    },
    TermTail {
        id: String,
    },
    TermAttach {
        id: String,
        motion: MotionMode,
    },

    Tui {
        motion: MotionMode,
    },

    /// The resolved workspace chrome theme — every role, the Signal token it
    /// carries, and whether an operator file moved it.
    Theme,

    /// Stamp the item seen. Quiets the row; leaves the item open, so the
    /// dedup slot stays held and the same problem cannot raise twice.
    AttentionAck {
        id: String,
    },
    /// Quiet the item until an instant. Same discipline as ack — quiet is
    /// presentation, resolve is the only close.
    AttentionMute {
        id: String,
        until: String,
    },
    /// Make the item audible again, by stamping a deadline already gone.
    AttentionUnmute {
        id: String,
    },
    AttentionResolve {
        id: String,
        resolution: Option<String>,
    },
    AuthorityGrant {
        source: Source,
    },
    AuthorityRevoke {
        id: String,
        reason: Option<String>,
    },

    BlobPut {
        source: Source,
        media_type: String,
    },
    BlobGet {
        address: BlobAddress,
        output: Sink,
    },
    BlobStat {
        address: BlobAddress,
    },

    IngestSubmit {
        kind: IngestionKind,
        source: Source,
        project: Option<String>,
        key: Option<String>,
    },

    /// The one verb that speaks to `gh` instead of the socket ([`crate::pr`]).
    Pr {
        what: crate::pr::Gh,
        dry_run: bool,
    },
}

/// The one output a human reads instead of a machine.
pub const HELP: &str = "\
gw — the GridWork kernel's command line

  gw build-info
  gw daemon
  gw admin init
  gw admin verify
  gw admin rebuild-projections --scratch-database <name>
  gw admin receipt --from <rfc3339> [--to <rfc3339>]
  gw admin blob pin <address> <evidence-id>
  gw admin blob unpin <address> <evidence-id>
  gw admin blob sweep
  gw admin blob shred <address>
  gw admin blob rotate
  gw kernel health|status|watermark|verify-sealed
  gw kernel activate --cutover-id <id> --archive-manifest-sha256 <hex>
  gw command submit --file <path|->
  gw projection get <type> <id>
  gw projection list <type> [--cursor <key>] [--limit <n>]
  gw event read [--cursor <seq>] [--limit <n>]
  gw event follow [--cursor <seq>]
  gw event tail [--cursor <seq>] [--aggregate-type <type>] [--event-type <type>] [--view <name>]
      [--motion=off|reduced|full]
  gw board [--view <name>] [--motion=off|reduced|full]
  gw estate overview
  gw activity brief
  gw cost rollup
  gw agent fleet
  gw attempt list [--cursor <key>] [--limit <n>]
  gw attempt stop <attempt-id>
  gw attempt budget <attempt-id>
  gw attempt budget set <attempt-id> --expected-version <n>
      [--max-tokens <n>] [--max-tool-calls <n>] [--max-wall-ms <n>] [--max-cost-micros <n>]
  gw attempt budget clear <attempt-id> --expected-version <n>
  gw session list [--cursor <key>] [--limit <n>]
  gw session inspect <engine-session-id>
  gw term list [--cursor <key>] [--limit <n>]
  gw term tail <pty-session-id>
  gw term attach <pty-session-id> [--motion=off|reduced|full]
  gw tui [--motion=off|reduced|full]
  gw theme
  gw attention list [--cursor <key>] [--limit <n>]
  gw attention ack <id>
  gw attention mute <id> --until <rfc3339>
  gw attention unmute <id>
  gw attention resolve <id> [--resolution <text>]
  gw authority list [--cursor <key>] [--limit <n>]
  gw authority grant --file <path|->
  gw authority revoke <id> [--reason <text>]
  gw blob put --file <path|-> [--media-type <type>]
  gw blob get <address> [--output <path|->]
  gw blob stat <address>
  gw ingest submit --kind <kind> --file <path|-> [--project <id>] [--key <key>]
  gw pr open --title <text> [--body <text>] [--body-file <path|->]
             [--base <branch>] [--head <branch>] [--repo <owner/repo>] [--dry-run]
  gw pr merge <number|url|branch> [--strategy squash|merge|rebase]
             [--repo <owner/repo>] [--dry-run]

  --pretty   format the JSON answer for a human; the value is unchanged
  --json     force protocol JSON for list verbs even when stdout is a terminal
  --version  same answer as `build-info`, under the name every CLI is asked by
  --help     this

`attempt list`, `session list`, `term list`, and `cost rollup` print a table
on a terminal and protocol JSON when piped; `--json` forces JSON there, and
every other list verb prints protocol JSON on both. `tui`, `board`,
`event tail`, and `term attach` are interactive. Exits: 0 success, 2 usage or input,
3 refused, 4 not found, 5 unavailable, 6 does not verify, 10 a fault in gw.

`pr` is the one verb that speaks to `gh` instead of the socket — always an
argument array, never a shell line — and its live answer and standard streams
are gh's own. A body comes from exactly one of its two body flags, and
--dry-run prints the argv as JSON instead of running anything.

`theme` resolves the workspace chrome slot from GWK_CHROME_THEME and asks
nothing of the socket. Orchestration stays SIGNAL: the slot themes the
workspace's own furniture, never a lens and never a pane's contents.

`daemon` and `admin` read GWK_DATABASE_URL / GWK_ADMIN_DATABASE_URL and the blob
KEK. Every other verb uses only the socket. `admin blob rotate` also reads
GWK_BLOB_KEK_NEXT, the key it is moving to; it is safe to re-run, and finishes an
interrupted rotation rather than faulting on what it already did.
";

/// Parse the arguments after the program name.
pub fn parse(argv: &[String]) -> Result<Invocation, Failure> {
    let mut rest = Rest::new(argv)?;
    if rest.switch("--help") || rest.switch("-h") {
        return Ok(Invocation {
            verb: Verb::Help,
            pretty: false,
            json: false,
        });
    }
    // An alias, not a second answer. `--version` is the name every CLI gets asked
    // by, and `build-info` already carries the crate version and the public
    // revision; making it print something shorter would mean two version outputs
    // that can disagree. Read before --pretty so the flag still applies.
    if rest.switch("--version") || rest.switch("-V") {
        return Ok(Invocation {
            verb: Verb::BuildInfo,
            pretty: rest.switch("--pretty"),
            json: rest.switch("--json"),
        });
    }
    let pretty = rest.switch("--pretty");
    let json = rest.switch("--json");
    let verb = verb(&mut rest)?;
    rest.done()?;
    if pretty
        && matches!(
            verb,
            Verb::Tui { .. } | Verb::EventTail { .. } | Verb::TermAttach { .. }
        )
    {
        return Err(Failure::usage(
            "--pretty does not apply to interactive views",
        ));
    }
    if json
        && matches!(
            verb,
            Verb::Tui { .. }
                | Verb::EventTail { .. }
                | Verb::TermAttach { .. }
                | Verb::BlobGet { .. }
                | Verb::Pr { .. }
        )
    {
        return Err(Failure::usage(
            "--json does not apply to interactive or pass-through output",
        ));
    }
    Ok(Invocation { verb, pretty, json })
}

fn verb(rest: &mut Rest) -> Result<Verb, Failure> {
    let first = rest.word("gw")?;
    match first.as_str() {
        "help" => Ok(Verb::Help),
        "build-info" => Ok(Verb::BuildInfo),
        "daemon" => Ok(Verb::Daemon),
        "admin" => admin(rest),
        "kernel" => kernel(rest),
        "command" => match rest.word("command")?.as_str() {
            "submit" => Ok(Verb::CommandSubmit {
                source: rest.source()?,
            }),
            other => Err(unknown("command", other)),
        },
        "projection" => projection(rest),
        "event" => event(rest),
        // The Board under its own name. The same interactive loop `event tail`
        // runs — one verb underneath, so the two spellings can never disagree —
        // opening on the estate panel instead of the event tail.
        "board" => Ok(Verb::EventTail {
            cursor: None,
            aggregate_type: None,
            event_type: None,
            view: rest
                .flag("--view")
                .map(|value| board_view(&value))
                .transpose()?
                .unwrap_or(BoardView::Estate),
            motion: rest
                .flag("--motion")
                .map(|value| motion_mode(&value))
                .transpose()?
                .unwrap_or(MotionMode::Full),
        }),
        "estate" => match rest.word("estate")?.as_str() {
            "overview" => Ok(Verb::EstateOverview),
            other => Err(unknown("estate", other)),
        },
        "activity" => match rest.word("activity")?.as_str() {
            "brief" => Ok(Verb::ActivityBrief),
            other => Err(unknown("activity", other)),
        },
        "cost" => match rest.word("cost")?.as_str() {
            "rollup" => Ok(Verb::CostRollup),
            other => Err(unknown("cost", other)),
        },
        "agent" => match rest.word("agent")?.as_str() {
            "fleet" => Ok(Verb::AgentFleet),
            other => Err(unknown("agent", other)),
        },
        "attempt" => attempt(rest),
        "session" => match rest.word("session")?.as_str() {
            "list" => rest.page(ProjectionKind::EngineSession),
            "inspect" => Ok(Verb::ProjectionGet {
                kind: ProjectionKind::EngineSession,
                id: rest.word("session inspect")?,
            }),
            // The noun split moved terminal lifetimes out of `session`; a
            // muscle-memory spelling gets the new address, not a generic
            // unknown-command shrug.
            moved @ ("attach" | "snapshot" | "tail") => Err(Failure::usage(format!(
                "session {moved} moved with the gw term noun split: terminal lifetimes are `gw term list/attach/tail` (engine sessions stay under `gw session`)"
            ))),
            other => Err(unknown("session", other)),
        },
        "term" => match rest.word("term")?.as_str() {
            "list" => rest.page(ProjectionKind::PtySession),
            "tail" => Ok(Verb::TermTail {
                id: rest.word("term tail")?,
            }),
            "attach" => Ok(Verb::TermAttach {
                id: rest.word("term attach")?,
                // Off by default: the hosted session owns the screen.
                motion: rest
                    .flag("--motion")
                    .map(|value| motion_mode(&value))
                    .transpose()?
                    .unwrap_or(MotionMode::Off),
            }),
            other => Err(unknown("term", other)),
        },
        "tui" => Ok(Verb::Tui {
            motion: rest
                .flag("--motion")
                .map(|value| motion_mode(&value))
                .transpose()?
                .unwrap_or(MotionMode::Full),
        }),
        // `attention list` and `authority list` ARE projection pages under
        // another name, so they resolve to the same verb rather than to a second
        // way of asking one question.
        "attention" => match rest.word("attention")?.as_str() {
            "list" => rest.page(ProjectionKind::AttentionItem),
            "ack" => Ok(Verb::AttentionAck {
                id: rest.word("attention ack")?,
            }),
            "mute" => Ok(Verb::AttentionMute {
                id: rest.word("attention mute")?,
                // Required rather than defaulted: the kernel has no clock the
                // caller can borrow, and a mute whose deadline this program
                // invented would be quiet for a length nobody asked for.
                until: rest.required("--until")?,
            }),
            "unmute" => Ok(Verb::AttentionUnmute {
                id: rest.word("attention unmute")?,
            }),
            "resolve" => Ok(Verb::AttentionResolve {
                id: rest.word("attention resolve")?,
                resolution: rest.flag("--resolution"),
            }),
            other => Err(unknown("attention", other)),
        },
        "authority" => match rest.word("authority")?.as_str() {
            "list" => rest.page(ProjectionKind::AuthorityGrant),
            "grant" => Ok(Verb::AuthorityGrant {
                source: rest.source()?,
            }),
            "revoke" => Ok(Verb::AuthorityRevoke {
                id: rest.word("authority revoke")?,
                reason: rest.flag("--reason"),
            }),
            other => Err(unknown("authority", other)),
        },
        // The one verb that asks nothing of the kernel and nothing of the
        // forge: the workspace chrome slot is a local file resolved against a
        // ratified palette, so its twin resolves it locally too.
        "theme" => Ok(Verb::Theme),
        "blob" => blob(rest),
        "pr" => pr(rest),
        "ingest" => match rest.word("ingest")?.as_str() {
            "submit" => Ok(Verb::IngestSubmit {
                kind: ingestion_kind(&rest.required("--kind")?)?,
                source: rest.source()?,
                // Both optional, and both defaulted where the record's identity
                // comes from: `(project_id, idempotency_key)`. The kernel's own
                // project and a digest of the payload make a re-submitted
                // record the SAME record, which is what a retry should mean.
                project: rest.flag("--project"),
                key: rest.flag("--key"),
            }),
            other => Err(unknown("ingest", other)),
        },
        other => Err(unknown("gw", other)),
    }
}

fn admin(rest: &mut Rest) -> Result<Verb, Failure> {
    match rest.word("admin")?.as_str() {
        "init" => Ok(Verb::AdminInit),
        "verify" => Ok(Verb::AdminVerify),
        "rebuild-projections" => Ok(Verb::AdminRebuildProjections {
            scratch: rest.required("--scratch-database")?,
        }),
        // Here and not on the client socket because the figures are SQL
        // aggregations over the log, and the wire grammar has no reason to
        // grow a query it exists to serve exactly once per driving window.
        "receipt" => Ok(Verb::AdminReceipt {
            from: rest.required("--from")?,
            to: rest.flag("--to"),
        }),
        // Retention, which is why it is here and not on the client socket: no
        // wire request removes a blob.
        "blob" => Ok(Verb::AdminBlob {
            what: match rest.word("admin blob")?.as_str() {
                "pin" => Retention::Pin {
                    address: blob_address(&rest.word("a blob address")?)?,
                    evidence: rest.word("an evidence id")?,
                },
                "unpin" => Retention::Unpin {
                    address: blob_address(&rest.word("a blob address")?)?,
                    evidence: rest.word("an evidence id")?,
                },
                "sweep" => Retention::Sweep,
                "shred" => Retention::Shred {
                    address: blob_address(&rest.word("a blob address")?)?,
                },
                // No arguments: the key it moves to arrives in the environment,
                // beside the one it moves off. A KEK on a command line is a KEK
                // in the shell history and in every `ps` on the box.
                "rotate" => Retention::Rotate,
                other => return Err(unknown("admin blob", other)),
            },
        }),
        other => Err(unknown("admin", other)),
    }
}

fn kernel(rest: &mut Rest) -> Result<Verb, Failure> {
    match rest.word("kernel")?.as_str() {
        "health" => Ok(Verb::Health),
        "status" => Ok(Verb::Status),
        "watermark" => Ok(Verb::Watermark),
        "verify-sealed" => Ok(Verb::VerifySealed),
        "activate" => Ok(Verb::Activate {
            cutover_id: rest.required("--cutover-id")?,
            manifest_sha256: rest.required("--archive-manifest-sha256")?,
        }),
        other => Err(unknown("kernel", other)),
    }
}

fn projection(rest: &mut Rest) -> Result<Verb, Failure> {
    let sub = rest.word("projection")?;
    let kind = projection_kind(&rest.word("projection type")?)?;
    match sub.as_str() {
        "get" => Ok(Verb::ProjectionGet {
            kind,
            id: rest.word("projection get")?,
        }),
        "list" => rest.page(kind),
        other => Err(unknown("projection", other)),
    }
}

fn event(rest: &mut Rest) -> Result<Verb, Failure> {
    let cursor = rest.flag("--cursor").map(sequence).transpose()?;
    match rest.word("event")?.as_str() {
        "read" => Ok(Verb::EventRead {
            cursor,
            limit: rest
                .flag("--limit")
                .map(|value| count(&value))
                .transpose()?
                .unwrap_or(DEFAULT_EVENT_LIMIT),
        }),
        "follow" => Ok(Verb::EventFollow { cursor }),
        "tail" => Ok(Verb::EventTail {
            cursor,
            aggregate_type: rest.flag("--aggregate-type"),
            event_type: rest.flag("--event-type"),
            view: rest
                .flag("--view")
                .map(|value| board_view(&value))
                .transpose()?
                .unwrap_or(BoardView::Events),
            motion: rest
                .flag("--motion")
                .map(|value| motion_mode(&value))
                .transpose()?
                .unwrap_or(MotionMode::Full),
        }),
        other => Err(unknown("event", other)),
    }
}

fn attempt(rest: &mut Rest) -> Result<Verb, Failure> {
    match rest.word("attempt")?.as_str() {
        "list" => rest.page(ProjectionKind::Attempt),
        "stop" => Ok(Verb::AttemptStop {
            id: rest.word("attempt stop")?,
        }),
        "budget" => {
            let sub_or_id = rest.word("attempt budget")?;
            match sub_or_id.as_str() {
                "set" => {
                    let id = rest.word("attempt budget set")?;
                    let budget = Budget {
                        max_tokens: optional_u32(rest, "--max-tokens")?,
                        max_tool_calls: optional_u32(rest, "--max-tool-calls")?,
                        max_wall_ms: optional_u32(rest, "--max-wall-ms")?,
                        max_cost_micros: rest
                            .flag("--max-cost-micros")
                            .map(|value| unsigned(&value, "--max-cost-micros").map(CostMicros::new))
                            .transpose()?,
                    };
                    if budget.max_tokens.is_none()
                        && budget.max_tool_calls.is_none()
                        && budget.max_wall_ms.is_none()
                        && budget.max_cost_micros.is_none()
                    {
                        return Err(Failure::usage(
                            "attempt budget set needs at least one cap; use attempt budget clear to uncap every axis",
                        ));
                    }
                    Ok(Verb::AttemptBudgetUpdate {
                        id,
                        expected_version: required_u32(rest, "--expected-version")?,
                        budget,
                    })
                }
                "clear" => Ok(Verb::AttemptBudgetUpdate {
                    id: rest.word("attempt budget clear")?,
                    expected_version: required_u32(rest, "--expected-version")?,
                    budget: Budget {
                        max_tokens: None,
                        max_tool_calls: None,
                        max_wall_ms: None,
                        max_cost_micros: None,
                    },
                }),
                id => Ok(Verb::AttemptBudget { id: id.to_owned() }),
            }
        }
        other => Err(unknown("attempt", other)),
    }
}

fn blob(rest: &mut Rest) -> Result<Verb, Failure> {
    match rest.word("blob")?.as_str() {
        "put" => Ok(Verb::BlobPut {
            source: rest.source()?,
            media_type: rest
                .flag("--media-type")
                .unwrap_or_else(|| "application/octet-stream".to_owned()),
        }),
        "get" => {
            let output = match rest.flag("--output").as_deref() {
                None | Some("-") => Sink::Stdout,
                Some(path) => Sink::File(PathBuf::from(path)),
            };
            Ok(Verb::BlobGet {
                address: blob_address(&rest.word("blob get")?)?,
                output,
            })
        }
        "stat" => Ok(Verb::BlobStat {
            address: blob_address(&rest.word("blob stat")?)?,
        }),
        other => Err(unknown("blob", other)),
    }
}

fn pr(rest: &mut Rest) -> Result<Verb, Failure> {
    use crate::pr::{Body, Gh, Merge, Open, Strategy};
    let what = match rest.word("pr")?.as_str() {
        "open" => Gh::Open(Open {
            title: rest.required("--title")?,
            body: match (rest.flag("--body"), rest.flag("--body-file")) {
                (Some(text), None) => Body::Text(text),
                (None, Some(path)) => Body::File(path),
                (Some(_), Some(_)) => {
                    return Err(Failure::usage(
                        "pr open takes --body or --body-file, not both",
                    ));
                }
                // Refused here rather than left to `gh`, which would fall
                // back to asking interactively — the wrong surprise for a
                // scriptable verb.
                (None, None) => {
                    return Err(Failure::usage(
                        "pr open needs a body: --body <text> or --body-file <path|->",
                    ));
                }
            },
            repo: rest.flag("--repo"),
            base: rest.flag("--base"),
            head: rest.flag("--head"),
        }),
        "merge" => Gh::Merge(Merge {
            subject: rest.word("pr merge")?,
            strategy: rest
                .flag("--strategy")
                .map(|value| Strategy::parse(&value))
                .transpose()?
                .unwrap_or(Strategy::Squash),
            repo: rest.flag("--repo"),
        }),
        other => return Err(unknown("pr", other)),
    };
    Ok(Verb::Pr {
        what,
        dry_run: rest.switch("--dry-run"),
    })
}

fn unknown(under: &str, what: &str) -> Failure {
    Failure::usage(format!("{under}: no such command {what:?}"))
}

fn count(value: &str) -> Result<u32, Failure> {
    value
        .parse()
        .map_err(|_| Failure::usage(format!("a limit is a count, not {value:?}")))
}

fn unsigned(value: &str, name: &str) -> Result<u64, Failure> {
    value
        .parse()
        .map_err(|_| Failure::usage(format!("{name} is an unsigned integer, not {value:?}")))
}

fn optional_u32(rest: &mut Rest, name: &str) -> Result<Option<u32>, Failure> {
    rest.flag(name)
        .map(|value| {
            value
                .parse()
                .map_err(|_| Failure::usage(format!("{name} is a u32, not {value:?}")))
        })
        .transpose()
}

fn required_u32(rest: &mut Rest, name: &str) -> Result<u32, Failure> {
    let value = rest.required(name)?;
    value
        .parse()
        .map_err(|_| Failure::usage(format!("{name} is a u32, not {value:?}")))
}

/// A cursor as a caller writes it: decimal, because that is what the wire
/// carries and what the previous answer printed.
fn sequence(value: String) -> Result<Seq, Failure> {
    value
        .parse::<u64>()
        .map(Seq::new)
        .map_err(|_| Failure::usage(format!("a cursor is a decimal sequence, not {value:?}")))
}

/// A Board view, resolved against the tab strip's own names.
fn board_view(name: &str) -> Result<BoardView, Failure> {
    BoardView::ALL
        .iter()
        .find(|view| view.as_str() == name)
        .copied()
        .ok_or_else(|| {
            let known: Vec<&str> = BoardView::ALL.iter().map(|view| view.as_str()).collect();
            Failure::usage(format!(
                "no board view {name:?}; one of: {}",
                known.join(", ")
            ))
        })
}

/// A projection name, resolved against the closed set the contract defines.
fn projection_kind(name: &str) -> Result<ProjectionKind, Failure> {
    ProjectionKind::ALL
        .iter()
        .find(|kind| kind.as_str() == name)
        .copied()
        .ok_or_else(|| {
            let known: Vec<&str> = ProjectionKind::ALL.iter().map(|k| k.as_str()).collect();
            Failure::usage(format!(
                "no projection {name:?}; one of: {}",
                known.join(", ")
            ))
        })
}

fn ingestion_kind(name: &str) -> Result<IngestionKind, Failure> {
    IngestionKind::ALL
        .iter()
        .find(|kind| kind.as_str() == name)
        .copied()
        .ok_or_else(|| {
            let known: Vec<&str> = IngestionKind::ALL.iter().map(|k| k.as_str()).collect();
            Failure::usage(format!(
                "no ingestion kind {name:?}; one of: {}",
                known.join(", ")
            ))
        })
}

fn motion_mode(name: &str) -> Result<MotionMode, Failure> {
    match name {
        "off" => Ok(MotionMode::Off),
        "reduced" => Ok(MotionMode::Reduced),
        "full" => Ok(MotionMode::Full),
        _ => Err(Failure::usage(format!(
            "motion is {name:?}; one of: off, reduced, full"
        ))),
    }
}

fn blob_address(value: &str) -> Result<BlobAddress, Failure> {
    BlobAddress::parse(value).map_err(|e| Failure::usage(format!("{value:?}: {e}")))
}

/// The arguments, separated into words, flags, and switches.
struct Rest {
    words: Vec<String>,
    flags: Vec<(String, String)>,
    switches: Vec<String>,
}

impl Rest {
    fn new(argv: &[String]) -> Result<Self, Failure> {
        let mut words = Vec::new();
        let mut flags: Vec<(String, String)> = Vec::new();
        let mut switches = Vec::new();
        let mut items = argv.iter();
        while let Some(item) = items.next() {
            // A bare `-` is a document on standard input, not a flag.
            if item == "-" || !item.starts_with('-') {
                words.push(item.clone());
            } else if SWITCHES.contains(&item.as_str()) {
                // The same refusal duplicate value flags get, and for the same
                // reason: a leftover duplicate would otherwise be reported as
                // "does not apply here" by a verb it applies to perfectly well.
                if switches.contains(item) {
                    return Err(Failure::usage(format!("{item} was given twice")));
                }
                switches.push(item.clone());
            } else if let Some((name, value)) = item.split_once('=') {
                if !VALUE_FLAGS.contains(&name) {
                    return Err(Failure::usage(format!("no such flag {name:?}")));
                }
                if value.is_empty() {
                    return Err(Failure::usage(format!("{name} needs a value")));
                }
                if flags.iter().any(|(flag, _)| flag == name) {
                    return Err(Failure::usage(format!("{name} was given twice")));
                }
                flags.push((name.to_owned(), value.to_owned()));
            } else if VALUE_FLAGS.contains(&item.as_str()) {
                let value = items
                    .next()
                    .ok_or_else(|| Failure::usage(format!("{item} needs a value")))?;
                // A flag name where a value belongs is a missing value, not a
                // value that happens to look like a flag.
                if VALUE_FLAGS.contains(&value.as_str()) || SWITCHES.contains(&value.as_str()) {
                    return Err(Failure::usage(format!(
                        "{item} needs a value, and {value} is a flag"
                    )));
                }
                if flags.iter().any(|(name, _)| name == item) {
                    return Err(Failure::usage(format!("{item} was given twice")));
                }
                flags.push((item.clone(), value.clone()));
            } else {
                return Err(Failure::usage(format!("no such flag {item:?}")));
            }
        }
        Ok(Self {
            words,
            flags,
            switches,
        })
    }

    fn switch(&mut self, name: &str) -> bool {
        if let Some(at) = self.switches.iter().position(|item| item == name) {
            self.switches.remove(at);
            return true;
        }
        false
    }

    fn flag(&mut self, name: &str) -> Option<String> {
        let at = self.flags.iter().position(|(flag, _)| flag == name)?;
        Some(self.flags.remove(at).1)
    }

    fn required(&mut self, name: &str) -> Result<String, Failure> {
        self.flag(name)
            .ok_or_else(|| Failure::usage(format!("{name} is required")))
    }

    /// The next positional, named by what wanted it.
    fn word(&mut self, under: &str) -> Result<String, Failure> {
        if self.words.is_empty() {
            return Err(Failure::usage(format!("{under}: expected an argument")));
        }
        Ok(self.words.remove(0))
    }

    /// `--file <path|->`, which every verb taking a document spells the same.
    fn source(&mut self) -> Result<Source, Failure> {
        match self.required("--file")?.as_str() {
            "-" => Ok(Source::Stdin),
            path => Ok(Source::File(PathBuf::from(path))),
        }
    }

    /// The paging pair, spelled the same by every list verb.
    fn page(&mut self, kind: ProjectionKind) -> Result<Verb, Failure> {
        Ok(Verb::ProjectionList {
            kind,
            cursor: self.flag("--cursor"),
            limit: self
                .flag("--limit")
                .map(|value| count(&value))
                .transpose()?,
        })
    }

    fn done(self) -> Result<(), Failure> {
        if let Some(word) = self.words.first() {
            return Err(Failure::usage(format!("unexpected argument {word:?}")));
        }
        // Refused rather than ignored: a flag the verb never reads is a caller
        // who believes something is happening that is not.
        if let Some((flag, _)) = self.flags.first() {
            return Err(Failure::usage(format!("{flag} does not apply here")));
        }
        // Switches too — a `--dry-run` on a verb that has no dry form would
        // otherwise run the real thing while claiming a rehearsal.
        if let Some(switch) = self.switches.first() {
            return Err(Failure::usage(format!("{switch} does not apply here")));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(line: &str) -> Result<Invocation, Failure> {
        let argv: Vec<String> = line.split_whitespace().map(str::to_owned).collect();
        parse(&argv)
    }

    #[test]
    fn the_documented_tree_parses() {
        assert_eq!(parsed("build-info").expect("parse").verb, Verb::BuildInfo);
        assert_eq!(
            parsed("estate overview").expect("parse").verb,
            Verb::EstateOverview
        );
        assert_eq!(
            parsed("activity brief").expect("parse").verb,
            Verb::ActivityBrief
        );
        assert_eq!(parsed("cost rollup").expect("parse").verb, Verb::CostRollup);
        assert_eq!(
            parsed("attempt stop at-1").expect("parse").verb,
            Verb::AttemptStop { id: "at-1".into() }
        );
        assert_eq!(
            parsed("attempt budget at-1").expect("parse").verb,
            Verb::AttemptBudget { id: "at-1".into() }
        );
        assert_eq!(
            parsed(
                "attempt budget set at-1 --expected-version 3 --max-tokens 100 --max-cost-micros 999"
            )
            .expect("parse")
            .verb,
            Verb::AttemptBudgetUpdate {
                id: "at-1".into(),
                expected_version: 3,
                budget: Budget {
                    max_tokens: Some(100),
                    max_tool_calls: None,
                    max_wall_ms: None,
                    max_cost_micros: Some(CostMicros::new(999)),
                },
            }
        );
        assert_eq!(
            parsed("attempt budget clear at-1 --expected-version 3")
                .expect("parse")
                .verb,
            Verb::AttemptBudgetUpdate {
                id: "at-1".into(),
                expected_version: 3,
                budget: Budget {
                    max_tokens: None,
                    max_tool_calls: None,
                    max_wall_ms: None,
                    max_cost_micros: None,
                },
            }
        );
        assert_eq!(parsed("agent fleet").expect("parse").verb, Verb::AgentFleet);
        assert_eq!(
            parsed("attempt list").expect("parse").verb,
            Verb::ProjectionList {
                kind: ProjectionKind::Attempt,
                cursor: None,
                limit: None,
            }
        );
        assert_eq!(
            parsed("session list").expect("parse").verb,
            Verb::ProjectionList {
                kind: ProjectionKind::EngineSession,
                cursor: None,
                limit: None,
            }
        );
        assert_eq!(
            parsed("session inspect es-1").expect("parse").verb,
            Verb::ProjectionGet {
                kind: ProjectionKind::EngineSession,
                id: "es-1".into(),
            }
        );
        assert_eq!(
            parsed("term tail pty-1").expect("parse").verb,
            Verb::TermTail { id: "pty-1".into() }
        );
        assert_eq!(
            parsed("term attach pty-1").expect("parse").verb,
            Verb::TermAttach {
                id: "pty-1".into(),
                motion: MotionMode::Off
            }
        );
        assert_eq!(
            parsed("term list").expect("parse").verb,
            Verb::ProjectionList {
                kind: ProjectionKind::PtySession,
                cursor: None,
                limit: None,
            }
        );
        assert_eq!(
            parsed("tui --motion=reduced").expect("parse").verb,
            Verb::Tui {
                motion: MotionMode::Reduced
            }
        );
        assert_eq!(
            parsed("tui --motion off").expect("parse").verb,
            Verb::Tui {
                motion: MotionMode::Off
            }
        );
        assert_eq!(parsed("kernel health").expect("parse").verb, Verb::Health);
        assert_eq!(
            parsed("kernel verify-sealed").expect("parse").verb,
            Verb::VerifySealed
        );
        assert_eq!(
            parsed("projection get task t-1").expect("parse").verb,
            Verb::ProjectionGet {
                kind: ProjectionKind::Task,
                id: "t-1".to_owned()
            }
        );
        // Two spellings of one question, resolving to one verb.
        assert_eq!(
            parsed("attention list").expect("parse").verb,
            parsed("projection list attention_item")
                .expect("parse")
                .verb
        );
        // The attention lifecycle is four kernel commands, and the command
        // line reaches all four. Two of them had a TUI verb and no twin
        // until this tree grew them, which is exactly the shape a
        // decommission drops without noticing.
        assert_eq!(
            parsed("attention ack a-1").expect("parse").verb,
            Verb::AttentionAck {
                id: "a-1".to_owned()
            }
        );
        assert_eq!(
            parsed("attention mute a-1 --until 2026-08-09T12:00:00Z")
                .expect("parse")
                .verb,
            Verb::AttentionMute {
                id: "a-1".to_owned(),
                until: "2026-08-09T12:00:00Z".to_owned()
            }
        );
        assert_eq!(
            parsed("attention unmute a-1").expect("parse").verb,
            Verb::AttentionUnmute {
                id: "a-1".to_owned()
            }
        );
        assert_eq!(
            parsed("attention resolve a-1").expect("parse").verb,
            Verb::AttentionResolve {
                id: "a-1".to_owned(),
                resolution: None
            }
        );
        // A mute with no deadline is refused rather than defaulted: this
        // program has no clock the kernel would agree with, and quiet for a
        // length nobody asked for is worse than a usage error.
        assert!(parsed("attention mute a-1").is_err());
        assert!(
            parsed("attention snooze a-1").is_err(),
            "no second spelling"
        );

        assert_eq!(
            parsed("admin receipt --from 2026-08-11T03:08:23Z")
                .expect("parse")
                .verb,
            Verb::AdminReceipt {
                from: "2026-08-11T03:08:23Z".to_owned(),
                to: None
            }
        );
        assert_eq!(
            parsed("admin receipt --from 2026-08-11T03:08:23Z --to 2026-09-01T00:00:00Z")
                .expect("parse")
                .verb,
            Verb::AdminReceipt {
                from: "2026-08-11T03:08:23Z".to_owned(),
                to: Some("2026-09-01T00:00:00Z".to_owned())
            }
        );
        // A window with no start is refused rather than defaulted: "since
        // genesis" is a figure nobody asked for, and the receipt's window
        // start is a recorded fact, not a guess.
        assert!(parsed("admin receipt").is_err());

        assert_eq!(
            parsed("event follow --cursor 42").expect("parse").verb,
            Verb::EventFollow {
                cursor: Some(Seq::new(42))
            }
        );
        assert_eq!(
            parsed("event read").expect("parse").verb,
            Verb::EventRead {
                cursor: None,
                limit: DEFAULT_EVENT_LIMIT
            }
        );
        assert_eq!(
            parsed("event tail --cursor 40 --aggregate-type attempt --event-type attempt_started")
                .expect("parse")
                .verb,
            Verb::EventTail {
                cursor: Some(Seq::new(40)),
                aggregate_type: Some("attempt".into()),
                event_type: Some("attempt_started".into()),
                view: BoardView::Events,
                motion: MotionMode::Full,
            }
        );
        assert_eq!(
            parsed("event tail --view fleet").expect("parse").verb,
            Verb::EventTail {
                cursor: None,
                aggregate_type: None,
                event_type: None,
                view: BoardView::Fleet,
                motion: MotionMode::Full,
            }
        );
        assert_eq!(
            parsed("board").expect("parse").verb,
            Verb::EventTail {
                cursor: None,
                aggregate_type: None,
                event_type: None,
                view: BoardView::Estate,
                motion: MotionMode::Full,
            }
        );
        assert_eq!(
            parsed("board --view cost").expect("parse").verb,
            Verb::EventTail {
                cursor: None,
                aggregate_type: None,
                event_type: None,
                view: BoardView::CostHealth,
                motion: MotionMode::Full,
            }
        );
        assert!(parsed("board --view bogus").is_err());
        // The moved spellings answer with the new address, not a generic
        // unknown-command shrug.
        for line in ["session attach pty-1", "session snapshot pty-1"] {
            let refusal = parsed(line).expect_err("the old spelling is refused");
            assert!(
                refusal.message.contains("gw term"),
                "{line}: {}",
                refusal.message
            );
        }
    }

    #[test]
    fn flags_may_come_in_any_order_and_pretty_is_global() {
        let one = parsed("--pretty projection list task --limit 5").expect("parse");
        let two = parsed("projection list --limit 5 task --pretty").expect("parse");
        assert!(one.pretty && two.pretty);
        assert_eq!(one.verb, two.verb);
        assert_eq!(
            one.verb,
            Verb::ProjectionList {
                kind: ProjectionKind::Task,
                cursor: None,
                limit: Some(5)
            }
        );
        assert!(parsed("term list --json").expect("parse").json);
    }

    #[test]
    fn version_is_an_alias_for_build_info() {
        // The point of the alias is that there is exactly one version answer, so
        // assert both spellings land on the same verb `build-info` does — not
        // merely that they parse.
        for line in ["--version", "-V"] {
            let got = parsed(line).expect("parse");
            assert_eq!(got.verb, Verb::BuildInfo, "{line}");
            assert!(!got.pretty, "{line}");
        }
        assert!(parsed("--version --pretty").expect("parse").pretty);
        assert!(parsed("--pretty -V").expect("parse").pretty);
    }

    #[test]
    fn a_mistake_is_refused_rather_than_absorbed() {
        // Every line here would otherwise do something the caller did not ask
        // for: a typo'd flag reads a whole table, a missing value swallows the
        // next flag, an unknown name asks for a table that is not there, and a
        // flag that belongs to another verb quietly does nothing.
        for line in [
            "projection list task --cusor abc",
            "projection list task --limit",
            "projection list task --limit --pretty",
            "projection list task --limit 1 --limit 2",
            "projection list tsak",
            "kernel health --cursor 5",
            "kernel activate --cutover-id c-1",
            "ingest submit --file -",
            "ingest submit --kind nonsense --file - --project p",
            "event read --limit twelve",
            "tui --motion=fast",
            "tui --motion=",
            "tui --pretty",
            "event tail --pretty",
            // The old spelling: refused with a pointer at `gw term attach`,
            // never absorbed (the noun split moved it).
            "session attach pty-1 --pretty",
            "session attach pty-1",
            "session snapshot pty-1",
            "term attach pty-1 --pretty",
            "term attach pty-1 --json",
            "attempt budget set at-1 --expected-version 1",
            "attempt budget set at-1 --max-tokens 1",
            "attempt budget clear at-1",
            "attempt budget at-1 --max-tokens 1",
            "attempt budget set at-1 --expected-version nope --max-tokens 1",
            "attempt budget set at-1 --expected-version 1 --max-cost-micros nope",
            "blob stat not-an-address",
            "pr open",
            "pr open --title t",
            "pr open --title t --body b --body-file -",
            "pr merge",
            "pr merge 61 --strategy fast-forward",
            "pr close 61",
            "kernel health --dry-run",
            "pr merge 61 --dry-run --dry-run",
            "nonsense",
            "kernel",
            "",
        ] {
            let failure = parsed(line).expect_err(line);
            assert_eq!(
                failure.exit,
                crate::exit::USAGE,
                "{line:?} did not exit as a usage error"
            );
        }
    }

    #[test]
    fn the_pr_verb_resolves_to_the_gh_argv_it_advertises() {
        use crate::pr::{Body, Gh, Merge, Open, Strategy};
        assert_eq!(
            parsed("pr open --title t --body-file - --repo o/r --head b --dry-run")
                .expect("parse")
                .verb,
            Verb::Pr {
                what: Gh::Open(Open {
                    title: "t".to_owned(),
                    body: Body::File("-".to_owned()),
                    repo: Some("o/r".to_owned()),
                    base: None,
                    head: Some("b".to_owned()),
                }),
                dry_run: true,
            }
        );
        // The default strategy is the repository's own convention, squash —
        // and without --dry-run the invocation is the live one.
        assert_eq!(
            parsed("pr merge 61").expect("parse").verb,
            Verb::Pr {
                what: Gh::Merge(Merge {
                    subject: "61".to_owned(),
                    strategy: Strategy::Squash,
                    repo: None,
                }),
                dry_run: false,
            }
        );
    }

    #[test]
    fn a_dash_is_a_document_on_standard_input_not_a_flag() {
        assert_eq!(
            parsed("command submit --file -").expect("parse").verb,
            Verb::CommandSubmit {
                source: Source::Stdin
            }
        );
        assert_eq!(
            parsed("blob put --file - --media-type text/plain")
                .expect("parse")
                .verb,
            Verb::BlobPut {
                source: Source::Stdin,
                media_type: "text/plain".to_owned()
            }
        );
    }

    #[test]
    fn every_flag_the_help_advertises_is_a_flag_the_parser_knows() {
        // The help text is the contract a caller reads. A flag documented there
        // and absent from the tables above would be refused as "no such flag" by
        // the very binary that advertised it.
        for line in HELP.lines() {
            for word in line.split_whitespace() {
                let name = word
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .split_once('=')
                    .map_or_else(
                        || word.trim_start_matches('[').trim_end_matches(']'),
                        |pair| pair.0,
                    );
                if !name.starts_with("--") {
                    continue;
                }
                assert!(
                    VALUE_FLAGS.contains(&name) || SWITCHES.contains(&name),
                    "the help advertises {name}, which the parser does not know"
                );
            }
        }
    }
}
