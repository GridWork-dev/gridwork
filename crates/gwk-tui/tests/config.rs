use std::path::Path;
use std::process::Command;

use gwk_domain::command::KernelCommand;
use gwk_domain::entity::Evidence;
use gwk_domain::ids::{AttentionItemId, EvidenceId, Timestamp};
use gwk_theme::marks::GlyphSet;
use gwk_theme::tier::ColorTier;
use gwk_tui::config::{
    ConfigError, ConfigPath, ConfigRepository, ConfigTarget, EditRoute, ValidatedForm, render,
    target_order,
};
use gwk_tui::input::HitMap;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .expect("run git in fixture");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("git fixture output is utf-8")
        .trim()
        .to_owned()
}

fn fixture() -> tempfile::TempDir {
    let temp = tempfile::tempdir().expect("create config fixture");
    let root = temp.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.name", "Config Lens Test"]);
    git(
        root,
        &["config", "user.email", "config-lens@example.invalid"],
    );
    std::fs::create_dir(root.join("identity")).expect("create identity directory");
    for path in ConfigPath::ALL {
        std::fs::write(
            root.join(path.as_str()),
            format!("name = {:?}\n", path.as_str()),
        )
        .expect("seed config file");
    }
    git(root, &["add", "--", "identity"]);
    git(root, &["commit", "-q", "-m", "seed config"]);
    temp
}

fn evidence(revision: &str) -> Evidence {
    Evidence {
        id: EvidenceId::new("ev-config-before"),
        kind: "config_change".into(),
        r#ref: revision.into(),
        digest: None,
        byte_size: None,
        created_at: Timestamp::new("2026-08-06T12:00:00Z"),
    }
}

fn form(repository: &ConfigRepository, path: ConfigPath, serialized: &str) -> ValidatedForm {
    repository
        .form_schema(path)
        .expect("build config form schema")
        .validate(serialized)
        .expect("validate config form")
}

#[test]
fn config_path_off_the_allowlist_is_refused_typed() {
    let error = ConfigPath::parse("identity/security.toml").expect_err("path must be refused");
    assert!(matches!(
        error,
        ConfigError::PathNotEditable { path } if path == "identity/security.toml"
    ));

    let absolute = ConfigPath::parse("/tmp/identity/capabilities.toml")
        .expect_err("absolute path must be refused");
    assert!(matches!(absolute, ConfigError::PathNotEditable { .. }));
}

#[test]
fn config_divergent_git_log_head_raises_attention_on_startup() {
    let temp = fixture();
    let repository = ConfigRepository::open(temp.path()).expect("open config repository");
    let stale = evidence("0000000000000000000000000000000000000000");

    let command = repository
        .reconcile_startup(Some(&stale), AttentionItemId::new("att-config-divergence"))
        .expect("run startup reconciliation")
        .expect("divergence must raise attention");

    assert!(matches!(
        command,
        KernelCommand::RaiseAttention {
            attention_item_id,
            kind,
            subject_ref: Some(subject_ref),
            ..
        } if attention_item_id == AttentionItemId::new("att-config-divergence")
            && kind == "config_divergence"
            && subject_ref == "config/repository"
    ));
}

#[test]
fn config_matching_evidence_is_quiet_on_startup() {
    let temp = fixture();
    let repository = ConfigRepository::open(temp.path()).expect("open config repository");
    let head = git(temp.path(), &["rev-parse", "HEAD"]);

    let command = repository
        .reconcile_startup(
            Some(&evidence(&head)),
            AttentionItemId::new("att-config-divergence"),
        )
        .expect("run startup reconciliation");

    assert_eq!(command, None);
}

#[test]
fn config_sensitive_files_route_through_editor_not_form() {
    let temp = fixture();
    let repository = ConfigRepository::open(temp.path()).expect("open config repository");

    assert_eq!(
        repository
            .edit_route(ConfigPath::AuthorityPolicy)
            .expect("inspect authority policy"),
        EditRoute::Editor
    );
    assert_eq!(
        repository
            .edit_route(ConfigPath::Capabilities)
            .expect("inspect capabilities"),
        EditRoute::Form
    );

    std::fs::write(
        temp.path().join(ConfigPath::Capabilities.as_str()),
        "# warning text may mention ~/.gridwork/env without sourcing it\nname = \"safe\"\n",
    )
    .expect("seed env comment");
    assert_eq!(
        repository
            .edit_route(ConfigPath::Capabilities)
            .expect("inspect comment-only capabilities"),
        EditRoute::Form
    );

    std::fs::write(
        temp.path().join(ConfigPath::Capabilities.as_str()),
        "source = \"~/.gridwork/\\u0065nv\"\n",
    )
    .expect("seed escaped env source");
    assert_eq!(
        repository
            .edit_route(ConfigPath::Capabilities)
            .expect("inspect env-sourcing capabilities"),
        EditRoute::Editor
    );

    let error = repository
        .form_schema(ConfigPath::AuthorityPolicy)
        .expect_err("authority policy must never accept a form edit");
    assert!(matches!(
        error,
        ConfigError::EditorRequired {
            path: ConfigPath::AuthorityPolicy
        }
    ));

    let proposed_env_source = repository
        .commit_form(
            form(
                &repository,
                ConfigPath::NamespaceScopes,
                "name = \"~/.gridwork/./env\"\n",
            ),
            "config: source operator environment",
            EvidenceId::new("ev-env-source"),
        )
        .expect_err("a proposed env source must leave the form route");
    assert!(matches!(
        proposed_env_source,
        ConfigError::EditorRequired {
            path: ConfigPath::NamespaceScopes
        }
    ));
}

#[test]
fn config_form_schema_refuses_unknown_and_wrong_typed_fields() {
    let temp = fixture();
    let repository = ConfigRepository::open(temp.path()).expect("open config repository");
    let schema = repository
        .form_schema(ConfigPath::Capabilities)
        .expect("build form schema");

    let unknown = schema
        .validate("name = \"valid\"\nextra = \"nope\"\n")
        .expect_err("unknown field must be refused");
    assert!(matches!(unknown, ConfigError::SchemaMismatch { .. }));

    let schema = repository
        .form_schema(ConfigPath::Capabilities)
        .expect("rebuild form schema");
    let wrong_type = schema
        .validate("name = true\n")
        .expect_err("wrong field type must be refused");
    assert!(matches!(wrong_type, ConfigError::SchemaMismatch { .. }));
}

#[test]
fn config_empty_array_schema_does_not_admit_an_untyped_first_item() {
    let temp = fixture();
    let path = ConfigPath::Capabilities;
    std::fs::write(temp.path().join(path.as_str()), "items = []\n")
        .expect("seed empty array config");
    git(temp.path(), &["add", "--", path.as_str()]);
    git(
        temp.path(),
        &[
            "commit",
            "-q",
            "-m",
            "seed empty array",
            "--",
            path.as_str(),
        ],
    );
    let repository = ConfigRepository::open(temp.path()).expect("open config repository");
    let schema = repository.form_schema(path).expect("build form schema");

    let error = schema
        .validate("items = [1]\n")
        .expect_err("an empty array carries no element schema");
    assert!(matches!(error, ConfigError::SchemaMismatch { .. }));
}

#[test]
fn config_stale_form_cannot_overwrite_an_intervening_commit() {
    let temp = fixture();
    let path = ConfigPath::Orchestration;
    let repository = ConfigRepository::open(temp.path()).expect("open config repository");
    let pending = form(&repository, path, "name = \"pending\"\n");
    std::fs::write(temp.path().join(path.as_str()), "name = \"newer\"\n")
        .expect("write intervening config");
    git(temp.path(), &["add", "--", path.as_str()]);
    git(
        temp.path(),
        &[
            "commit",
            "-q",
            "-m",
            "intervening config",
            "--",
            path.as_str(),
        ],
    );

    let error = repository
        .commit_form(
            pending,
            "config: stale form",
            EvidenceId::new("ev-stale-form"),
        )
        .expect_err("stale form must not overwrite the newer commit");
    assert!(matches!(error, ConfigError::ConcurrentChange { .. }));
    assert_eq!(
        std::fs::read_to_string(temp.path().join(path.as_str())).expect("read newer config"),
        "name = \"newer\"\n"
    );
}

#[cfg(unix)]
#[test]
fn config_allowlisted_symlink_cannot_edit_a_different_file() {
    use std::os::unix::fs::symlink;

    let temp = fixture();
    let alias = temp.path().join(ConfigPath::Capabilities.as_str());
    std::fs::remove_file(&alias).expect("remove real capabilities file");
    symlink("orchestration.toml", &alias).expect("replace it with an in-tree alias");
    let repository = ConfigRepository::open(temp.path()).expect("open config repository");

    let error = repository
        .edit_route(ConfigPath::Capabilities)
        .expect_err("an allowlisted spelling must not authorize its symlink target");
    assert!(matches!(error, ConfigError::AliasedPath { .. }));
}

#[test]
fn config_form_write_commits_before_returning_evidence() {
    let temp = fixture();
    let repository = ConfigRepository::open(temp.path()).expect("open config repository");
    let before = git(temp.path(), &["rev-parse", "HEAD"]);

    let command = repository
        .commit_form(
            form(&repository, ConfigPath::Orchestration, "name = \"deep\"\n"),
            "config: set default lane",
            EvidenceId::new("ev-config-after"),
        )
        .expect("commit form edit");

    let after = git(temp.path(), &["rev-parse", "HEAD"]);
    assert_ne!(after, before);
    assert_eq!(
        std::fs::read_to_string(temp.path().join(ConfigPath::Orchestration.as_str()))
            .expect("read committed config"),
        "name = \"deep\"\n"
    );
    assert!(matches!(
        command,
        KernelCommand::RecordEvidence {
            evidence_id,
            kind,
            r#ref,
            digest: None,
            byte_size: None,
        } if evidence_id == EvidenceId::new("ev-config-after")
            && kind == "config_change"
            && r#ref == after
    ));
}

#[cfg(unix)]
#[test]
fn config_failed_commit_restores_a_clean_file_and_index() {
    use std::os::unix::fs::PermissionsExt as _;

    let temp = fixture();
    let repository = ConfigRepository::open(temp.path()).expect("open config repository");
    let path = ConfigPath::Orchestration;
    let original =
        std::fs::read_to_string(temp.path().join(path.as_str())).expect("read original config");
    let before = git(temp.path(), &["rev-parse", "HEAD"]);
    let hook = temp.path().join(".git/hooks/pre-commit");
    std::fs::write(&hook, "#!/bin/sh\nexit 1\n").expect("write failing pre-commit hook");
    let mut permissions = std::fs::metadata(&hook)
        .expect("inspect hook")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&hook, permissions).expect("make hook executable");

    let error = repository
        .commit_form(
            form(&repository, path, "name = \"changed\"\n"),
            "config: rejected by hook",
            EvidenceId::new("ev-no-change"),
        )
        .expect_err("git must honor the failing hook");
    assert!(matches!(error, ConfigError::GitFailed { .. }));
    assert_eq!(git(temp.path(), &["rev-parse", "HEAD"]), before);
    assert_eq!(
        std::fs::read_to_string(temp.path().join(path.as_str())).expect("read restored config"),
        original
    );
    assert!(
        git(temp.path(), &["status", "--porcelain", "--", path.as_str()]).is_empty(),
        "failed commit must leave neither worktree nor index state"
    );
}

#[test]
fn config_dirty_path_raises_attention_even_when_history_matches_evidence() {
    let temp = fixture();
    let repository = ConfigRepository::open(temp.path()).expect("open config repository");
    let head = git(temp.path(), &["rev-parse", "HEAD"]);
    let recorded = evidence(&head);
    std::fs::write(
        temp.path().join(ConfigPath::Orchestration.as_str()),
        "default_lane = \"dirty\"\n",
    )
    .expect("seed dirty config");

    let command = repository
        .reconcile_startup(Some(&recorded), AttentionItemId::new("att-config-dirty"))
        .expect("run startup reconciliation")
        .expect("dirty config must raise attention");
    assert!(matches!(
        command,
        KernelCommand::RaiseAttention { kind, summary, .. }
            if kind == "config_divergence" && summary.contains("uncommitted")
    ));
}

#[test]
fn config_render_registers_the_same_targets_the_keyboard_walk_reaches() {
    let temp = fixture();
    let repository = ConfigRepository::open(temp.path()).expect("open config repository");
    let state = repository.state(None).expect("load config state");
    let area = Rect::new(3, 2, 72, 12);
    let mut buffer = Buffer::empty(area);
    let mut hits = HitMap::new();

    render(
        area,
        &mut buffer,
        &state,
        Some(&ConfigTarget::File(ConfigPath::Capabilities)),
        ColorTier::Mono,
        GlyphSet::Ascii,
        &mut hits,
    );

    assert_eq!(
        hits.targets().cloned().collect::<Vec<_>>(),
        target_order(&state)
    );
    let rendered = buffer
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("identity/authority-policy.toml"));
    assert!(rendered.contains("CONFIG"));
}

#[test]
fn config_short_pane_windows_around_the_keyboard_selection() {
    let temp = fixture();
    let repository = ConfigRepository::open(temp.path()).expect("open config repository");
    let state = repository.state(None).expect("load config state");
    let selected = ConfigTarget::File(ConfigPath::Orchestration);
    let area = Rect::new(0, 0, 72, 4);
    let mut buffer = Buffer::empty(area);
    let mut hits = HitMap::new();

    render(
        area,
        &mut buffer,
        &state,
        Some(&selected),
        ColorTier::Mono,
        GlyphSet::Ascii,
        &mut hits,
    );

    assert!(hits.targets().any(|target| target == &selected));
    let rendered = buffer
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("identity/orchestration.toml"));
    assert!(rendered.contains("before"));
}

#[test]
fn config_render_escapes_unstable_cells_and_stays_inside_its_rect() {
    let temp = fixture();
    let repository = ConfigRepository::open(temp.path()).expect("open config repository");
    let mut state = repository.state(None).expect("load config state");
    state.last_evidence_ref = Some("◆hostile-evidence".into());
    let mut hits = HitMap::new();
    let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 7));

    render(
        Rect::new(0, 0, 12, 7),
        &mut buffer,
        &state,
        None,
        ColorTier::Mono,
        GlyphSet::Unicode,
        &mut hits,
    );

    for y in 0..7 {
        for x in 12..40 {
            assert_eq!(
                buffer[(x, y)].symbol(),
                " ",
                "config lens painted outside its area at ({x},{y})"
            );
        }
    }
    assert!(
        buffer.content.iter().all(|cell| cell.symbol() != "◆"),
        "dynamic text must pass through one-cell admission"
    );

    let mut zero_width = Buffer::empty(Rect::new(0, 0, 40, 7));
    render(
        Rect::new(0, 0, 0, 7),
        &mut zero_width,
        &state,
        None,
        ColorTier::Mono,
        GlyphSet::Unicode,
        &mut hits,
    );
    assert_eq!(zero_width, Buffer::empty(Rect::new(0, 0, 40, 7)));
}
