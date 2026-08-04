//! The command line, parsed by hand.
//!
//! No argument-parsing dependency, and that is a decision rather than an
//! omission. The tree is closed for v1 and flat — a verb, sometimes a subverb, a
//! positional or two, and a handful of named flags — and every answer this
//! binary prints is machine JSON, so the human affordances a parser library is
//! bought for are the part least needed here. What is left is a `match`.
//!
//! Flags and positionals are separated in ONE pass against the closed set of
//! flag names, before any verb is resolved. That is what makes their order free
//! — `projection list --limit 5 task` and `projection list task --limit 5` are
//! the same invocation — and it is what lets a flag nobody claimed be refused
//! rather than ignored: a mistyped `--cursor` becomes exit 2 instead of a
//! silent full-table read.

use std::path::PathBuf;

use gwk_domain::blob::BlobAddress;
use gwk_domain::ids::Seq;
use gwk_domain::ingestion::IngestionKind;
use gwk_domain::protocol::ProjectionKind;

use crate::admin::Retention;
use crate::exit::Failure;

/// Every flag in the tree that takes a value.
const VALUE_FLAGS: &[&str] = &[
    "--archive-manifest-sha256",
    "--cursor",
    "--cutover-id",
    "--file",
    "--key",
    "--kind",
    "--limit",
    "--media-type",
    "--output",
    "--project",
    "--reason",
    "--resolution",
    "--scratch-database",
];

/// Every flag that is its own answer.
const SWITCHES: &[&str] = &["--help", "--pretty", "--version", "-V", "-h"];

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
}

/// The one output a human reads instead of a machine.
pub const HELP: &str = "\
gw — the GridWork kernel's command line

  gw build-info
  gw daemon
  gw admin init
  gw admin verify
  gw admin rebuild-projections --scratch-database <name>
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
  gw attention list [--cursor <key>] [--limit <n>]
  gw attention resolve <id> [--resolution <text>]
  gw authority list [--cursor <key>] [--limit <n>]
  gw authority grant --file <path|->
  gw authority revoke <id> [--reason <text>]
  gw blob put --file <path|-> [--media-type <type>]
  gw blob get <address> [--output <path|->]
  gw blob stat <address>
  gw ingest submit --kind <kind> --file <path|-> [--project <id>] [--key <key>]

  --pretty   format the JSON answer for a human; the value is unchanged
  --version  same answer as `build-info`, under the name every CLI is asked by
  --help     this

Every answer is JSON on standard output. Exits: 0 success, 2 usage or input,
3 refused, 4 not found, 5 unavailable, 6 does not verify, 10 a fault in gw.

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
        });
    }
    let pretty = rest.switch("--pretty");
    let verb = verb(&mut rest)?;
    rest.done()?;
    Ok(Invocation { verb, pretty })
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
        // `attention list` and `authority list` ARE projection pages under
        // another name, so they resolve to the same verb rather than to a second
        // way of asking one question.
        "attention" => match rest.word("attention")?.as_str() {
            "list" => rest.page(ProjectionKind::AttentionItem),
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
        "blob" => blob(rest),
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
        other => Err(unknown("event", other)),
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

fn unknown(under: &str, what: &str) -> Failure {
    Failure::usage(format!("{under}: no such command {what:?}"))
}

fn count(value: &str) -> Result<u32, Failure> {
    value
        .parse()
        .map_err(|_| Failure::usage(format!("a limit is a count, not {value:?}")))
}

/// A cursor as a caller writes it: decimal, because that is what the wire
/// carries and what the previous answer printed.
fn sequence(value: String) -> Result<Seq, Failure> {
    value
        .parse::<u64>()
        .map(Seq::new)
        .map_err(|_| Failure::usage(format!("a cursor is a decimal sequence, not {value:?}")))
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
                switches.push(item.clone());
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
            "blob stat not-an-address",
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
                let name = word.trim_start_matches('[').trim_end_matches(']');
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
