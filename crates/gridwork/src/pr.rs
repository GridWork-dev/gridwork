//! `gw pr` — the SHIP act's PR and merge, shelled to `gh`.
//!
//! The demonstration gate's operative clause is verbatim: a `gw` verb
//! shelling `gh` for PR/merge SATISFIES "without leaving `gw`" — the twin
//! framing under which every console verb is also a scriptable subcommand.
//! The forge conversation belongs to `gh`; this verb's whole job is to reach
//! it safely and say exactly what it is about to run.
//!
//! Two rules, both load-bearing:
//!
//! * **`gh` receives an argument array, never a shell line.** The argv is
//!   built as a `Vec<String>` and handed to `Command::args`; no string is
//!   ever concatenated into a command, so a title with spaces — or quotes,
//!   or `$(…)` — is one argument and nothing more. `--dry-run` prints that
//!   argv as JSON instead of running it, which makes the claim inspectable
//!   rather than asserted.
//! * **The live answer is `gh`'s own.** Standard streams pass through: the
//!   URL `gh pr create` prints, the prompts it may ask, the refusal it may
//!   give all reach the caller unreshaped, and a nonzero `gh` exit becomes
//!   this program's error shape naming that exit. Wrapping `gh`'s
//!   conversation in a JSON envelope would re-say what was already said,
//!   one screen later.

use crate::exit::Failure;

/// Where a PR body comes from. Exactly one, refused at parse otherwise: a
/// bodyless `gh pr create` falls back to asking interactively, which is not
/// something a scriptable verb should do by accident.
#[derive(Debug, PartialEq, Eq)]
pub enum Body {
    /// `--body <text>`, passed through as one argument.
    Text(String),
    /// `--body-file <path|->`, passed through for `gh` to read — including
    /// `-`, which `gh` takes as standard input.
    File(String),
}

/// How `gw pr merge` asks `gh` to merge. The default is `squash`, this
/// repository's own convention.
#[derive(Debug, PartialEq, Eq)]
pub enum Strategy {
    Squash,
    Merge,
    Rebase,
}

impl Strategy {
    pub fn parse(value: &str) -> Result<Self, Failure> {
        match value {
            "squash" => Ok(Self::Squash),
            "merge" => Ok(Self::Merge),
            "rebase" => Ok(Self::Rebase),
            other => Err(Failure::usage(format!(
                "no merge strategy {other:?}; one of: squash, merge, rebase"
            ))),
        }
    }

    const fn flag(&self) -> &'static str {
        match self {
            Self::Squash => "--squash",
            Self::Merge => "--merge",
            Self::Rebase => "--rebase",
        }
    }
}

/// `gw pr open` — `gh pr create`, with the arguments a scripted caller
/// needs and none it does not.
#[derive(Debug, PartialEq, Eq)]
pub struct Open {
    pub title: String,
    pub body: Body,
    /// `--repo <owner/repo>`, for a checkout whose remote does not answer
    /// the question. Optional pass-throughs stay `gh`'s to default.
    pub repo: Option<String>,
    pub base: Option<String>,
    pub head: Option<String>,
}

/// `gw pr merge` — `gh pr merge`. The subject is whatever `gh` accepts
/// there: a number, a URL, or a branch.
#[derive(Debug, PartialEq, Eq)]
pub struct Merge {
    pub subject: String,
    pub strategy: Strategy,
    pub repo: Option<String>,
}

/// One `gh` invocation, resolved. What runs is exactly what
/// [`Gh::argv`] returns — the dry run and the live run share it.
#[derive(Debug, PartialEq, Eq)]
pub enum Gh {
    Open(Open),
    Merge(Merge),
}

impl Gh {
    /// The argument array `gh` receives, in a stable order a test can pin.
    pub fn argv(&self) -> Vec<String> {
        let mut argv: Vec<String> = Vec::new();
        let repo = match self {
            Self::Open(open) => {
                argv.extend(["pr", "create", "--title"].map(str::to_owned));
                argv.push(open.title.clone());
                match &open.body {
                    Body::Text(text) => argv.extend(["--body".to_owned(), text.clone()]),
                    Body::File(path) => argv.extend(["--body-file".to_owned(), path.clone()]),
                }
                for (flag, value) in [("--base", &open.base), ("--head", &open.head)] {
                    if let Some(value) = value {
                        argv.extend([flag.to_owned(), value.clone()]);
                    }
                }
                &open.repo
            }
            Self::Merge(merge) => {
                argv.extend(["pr", "merge"].map(str::to_owned));
                argv.push(merge.subject.clone());
                argv.push(merge.strategy.flag().to_owned());
                &merge.repo
            }
        };
        if let Some(repo) = repo {
            argv.extend(["--repo".to_owned(), repo.clone()]);
        }
        argv
    }
}

/// Run it, or say what would run.
pub fn run(gh: &Gh, dry_run: bool, pretty: bool) -> Result<(), Failure> {
    let argv = gh.argv();
    if dry_run {
        crate::emit(
            &serde_json::json!({ "type": "gh_argv", "argv": argv }),
            pretty,
        );
        return Ok(());
    }
    let status = std::process::Command::new("gh")
        .args(&argv)
        .status()
        .map_err(|e| Failure::unreachable(format!("run gh: {e}")))?;
    if status.success() {
        return Ok(());
    }
    // `gh` said no and said why on its own standard error. This program's
    // contribution is the machine-readable fact of it.
    Err(match status.code() {
        Some(code) => Failure::external(format!("gh exited {code}")),
        None => Failure::external("gh was stopped by a signal"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_argv_is_an_array_with_a_pinned_order() {
        let open = Gh::Open(Open {
            title: "feat(tui): two words".to_owned(),
            body: Body::File("-".to_owned()),
            repo: Some("GridWork-dev/gridwork".to_owned()),
            base: None,
            head: Some("feature/walk".to_owned()),
        });
        // The title stays ONE element. That single fact is the difference
        // between an argument array and a shell line.
        assert_eq!(
            open.argv(),
            [
                "pr",
                "create",
                "--title",
                "feat(tui): two words",
                "--body-file",
                "-",
                "--head",
                "feature/walk",
                "--repo",
                "GridWork-dev/gridwork",
            ]
        );

        let merge = Gh::Merge(Merge {
            subject: "61".to_owned(),
            strategy: Strategy::Squash,
            repo: None,
        });
        assert_eq!(merge.argv(), ["pr", "merge", "61", "--squash"]);
    }

    #[test]
    fn a_strategy_is_one_of_exactly_three_words() {
        assert_eq!(Strategy::parse("rebase").expect("parse"), Strategy::Rebase);
        let refusal = Strategy::parse("fast-forward").expect_err("refused");
        assert_eq!(refusal.exit, crate::exit::USAGE);
        assert!(refusal.message.contains("squash"), "{}", refusal.message);
    }
}
