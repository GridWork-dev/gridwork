import Link from "next/link";

import { CopyCommand } from "@/components/copy-command";

const sourceUrl = "https://github.com/GridWork-dev/gridwork";
const sourceFilesUrl = `${sourceUrl}/blob/main`;

const architecture = [
  [
    "One log",
    "Every platform truth is a projection of one append-only event log.",
    "/docs/architecture",
  ],
  [
    "A kernel, not a wrapper",
    "The daemon owns storage, attention, authority, workflows, and worktrees.",
    "/docs/architecture",
  ],
  [
    "Terminal-native",
    "The intended human surface is a TUI, with no web console.",
    "/docs/architecture",
  ],
  [
    "Engine-agnostic",
    "Adapters use ACP, engine hooks, and PTY; control never rides keystrokes.",
    "/docs/protocol",
  ],
] as const;

const stages = [
  ["01", "Contract", "shipped", "The shared language and conformance surface."],
  ["02", "Kernel", "shipped", "The sole writer, projections, blobs, attention, and authority."],
  ["03", "Engines", "shipped", "PTY and agent-control adapters, certified by the parity matrix."],
  ["04", "Console", "shipped", "Five lenses over one estate: hall, work, fleet, flow, term."],
  ["05", "Workspace", "current", "A daily-driver terminal multiplexer."],
  [
    "06",
    "Context runtime",
    "planned",
    "Skills, memory, and knowledge as measured, receipted surfaces.",
  ],
] as const;

const crates = [
  [
    "gwk-domain",
    "https://docs.rs/gwk-domain",
    "Shared types, events, state machines — the contract",
    "0.0.3",
  ],
  [
    "gwk-cert",
    "https://docs.rs/gwk-cert",
    "Stream checker, plus the storage suite a backend runs against its own event store",
    "0.0.3",
  ],
  [
    "gwk-theme",
    "https://docs.rs/gwk-theme",
    "The 15 SIGNAL design tokens — one source for the site, the TUI, and the generated TypeScript",
    "0.0.3",
  ],
  [
    "gwk-kernel",
    "https://docs.rs/gwk-kernel",
    "Daemon: event store, projections, blobs, attention, authority, the wire",
    "0.0.3",
  ],
  [
    "gwk-tui",
    "https://docs.rs/gwk-tui",
    "The client: modes, five lenses, palette — the console the installed binary renders",
    "0.0.3",
  ],
  [
    "gridwork",
    "https://docs.rs/gridwork",
    "Ships the gw binary — the CLI that speaks the kernel's protocol",
    "0.0.3",
  ],
  ["gwk", "https://docs.rs/gwk", "Namespace root for the gwk-* crates. No API", "0.0.3, name only"],
  ["xtask", null, "Codegen and release glue. Not published", "in-tree"],
  [
    "gwk-pty",
    null,
    "PTY engine: server-side VT, render-state deltas, reattach",
    "in tree, unpublished",
  ],
  [
    "gwk-pty-host",
    null,
    "Resident PTY engine host: session registry, spawn, detach/reattach routing",
    "in tree, unpublished — not in cargo install gridwork until its own release",
  ],
  ["gwk-adapter-*", null, "Per-engine ACP + hooks adapters", "in tree, unpublished"],
  [
    "gwk-parity",
    null,
    "The engine parity matrix harness — runs locally, never in CI",
    "in tree, unpublished",
  ],
  [
    "gwk-text",
    null,
    "Pure column arithmetic and extended grapheme boundaries",
    "in tree, unpublished",
  ],
] as const;

export default function HomePage() {
  return (
    <>
      <a className="landing-skip-link" href="#main-content">
        Skip to main content
      </a>

      <header className="landing-status-bar">
        <div className="landing-status-inner">
          <Link className="landing-brand" href="/" aria-label="GridWork home">
            <span aria-hidden="true">▌</span> gridwork
          </Link>

          <span className="landing-status-segment landing-status-optional">apache-2.0</span>
          <Link className="landing-stage" href="/docs/roadmap">
            stage 5/6 · workspace
          </Link>
          <span className="landing-status-segment landing-status-optional">pre-1.0</span>

          <nav className="landing-nav" aria-label="Primary navigation">
            <Link href="/docs">Docs</Link>
            <Link className="landing-nav-optional" href="/docs/roadmap">
              Roadmap
            </Link>
            <a className="landing-nav-optional" href={sourceUrl} rel="noreferrer">
              GitHub
            </a>
          </nav>
        </div>
      </header>

      <main id="main-content">
        <section className="landing-hero" aria-labelledby="landing-title">
          <div className="landing-shell landing-hero-grid">
            <div className="landing-hero-copy">
              <p className="landing-kicker">Built in the open</p>
              <h1 id="landing-title">An agent operating system for the terminal.</h1>
              <p className="landing-lede">
                GridWork is the operating layer for a fleet of coding agents: one append-only event
                log, one kernel, and one place that says what needs a human.
              </p>

              <div className="landing-actions">
                <Link className="landing-button landing-button--primary" href="/docs">
                  Read the docs
                </Link>
                <a className="landing-button" href={sourceUrl} rel="noreferrer">
                  View source
                </a>
              </div>

              <dl className="landing-spec-ledger">
                <div>
                  <dt>binary</dt>
                  <dd>
                    <code>gw</code>
                    <span>a single Rust binary</span>
                  </dd>
                </div>
                <div>
                  <dt>truth</dt>
                  <dd>
                    append-only event log
                    <span>every view is a projection</span>
                  </dd>
                </div>
                <div>
                  <dt>surface</dt>
                  <dd>
                    terminal-native
                    <span>no web console</span>
                  </dd>
                </div>
              </dl>
            </div>

            <div
              className="landing-terminal"
              aria-label="Certified stream and kernel health command transcript"
            >
              {/* Same reason as the crates table: the transcript scrolls, so it has to
                  be reachable by keyboard. */}
              {/* oxlint-disable-next-line jsx-a11y/no-noninteractive-tabindex */}
              <pre tabIndex={0}>
                <code>{`$ cargo run -p gwk-cert -- crates/gwk-cert/fixtures/valid-stream.json
[]
gwk-cert: certified — 16 events, 0 findings
$ gw kernel health
{"type":"health","ready":true,"sealed":true}`}</code>
              </pre>
            </div>
          </div>
        </section>

        <section className="landing-install landing-shell" aria-label="Install GridWork">
          <h2 className="landing-sr-only">Install GridWork</h2>
          <div className="landing-install-block">
            <div className="landing-install-line landing-install-line--copyable">
              <code>cargo install gridwork</code>
              <CopyCommand />
            </div>
            <div className="landing-install-line">
              <code>cargo build --workspace # stable Rust · msrv 1.94</code>
            </div>
            <p className="landing-install-warning">
              <strong>Pre-1.0: expect breakage.</strong> Schemas, protocols, and the binary change
              without notice until 1.0. The headless CLI needs PostgreSQL 16 and separate admin and
              runtime roles. <Link href="/docs/quickstart">Review the prerequisites</Link>.
            </p>
          </div>
        </section>

        <section className="landing-truth landing-shell" aria-labelledby="truth-title">
          <div className="landing-section-heading">
            <h2 id="truth-title">Where it actually is</h2>
            <p>
              Pre-alpha, at <strong>stage 5 of 6</strong>. The contract, kernel, engines, and
              console are done; the workspace multiplexer is the work now.
            </p>
          </div>

          <div className="landing-truth-ledger">
            <section aria-labelledby="working-title">
              <h3 id="working-title">
                <span aria-hidden="true">◆</span> Working
              </h3>
              <ul>
                <li>The contract and kernel are published.</li>
                <li>Certification runs against a real PostgreSQL 16.</li>
                <li>The performance envelope is measured, not asserted.</li>
                <li>
                  The headless <code>gw</code> CLI speaks the kernel protocol.
                </li>
                <li>
                  <code>gw tui</code> opens five live lenses over the estate.
                </li>
              </ul>
            </section>

            <section aria-labelledby="not-built-title">
              <h3 id="not-built-title">
                <span aria-hidden="true">◇</span> Not built yet
              </h3>
              <ul>
                <li>
                  The exportable-as-evidence half of replay — recordings replay in the console, but
                  not yet ledger-synced.
                </li>
                <li>The workspace: the real multiplexer.</li>
                <li>Engine host packaging — a separate, unpublished process until 1.0.</li>
              </ul>
            </section>
          </div>

          <p className="landing-section-route">
            <Link href="/docs/roadmap">Read the six-stage roadmap</Link>
          </p>
        </section>

        <section
          className="landing-architecture landing-shell"
          aria-labelledby="architecture-title"
        >
          <div className="landing-section-heading">
            <h2 id="architecture-title">The architecture stays legible</h2>
            <p>
              Four boundaries define what owns truth, how clients behave, and where engine control
              belongs.
            </p>
          </div>

          <ul className="landing-architecture-ledger">
            {architecture.map(([title, description, href]) => (
              <li key={title}>
                <Link href={href}>
                  <h3>{title}</h3>
                  <p>{description}</p>
                  <span>Read the boundary</span>
                </Link>
              </li>
            ))}
          </ul>

          <p className="landing-section-route">
            <Link href="/docs/architecture">Read the architecture</Link>
          </p>
        </section>

        <section className="landing-roadmap landing-shell" aria-labelledby="roadmap-title">
          <div className="landing-section-heading">
            <h2 id="roadmap-title">Six stages, in order</h2>
            <p>
              Stages land when their gates are green. Contract, kernel, engines, and console have
              shipped; the workspace is current.
            </p>
          </div>

          <ol className="landing-stage-list">
            {stages.map(([number, name, status, description]) => (
              <li key={number}>
                <span className="landing-stage-number">{number}</span>
                <div>
                  <div className="landing-stage-heading">
                    <h3>{name}</h3>
                    <span data-status={status}>{status}</span>
                  </div>
                  <p>{description}</p>
                </div>
              </li>
            ))}
          </ol>

          <div className="landing-principles">
            <h3>Principles that won&apos;t move</h3>
            <ul>
              <li>One append-only log owns every truth; user interfaces are projections of it.</li>
              <li>The kernel is the sole writer; clients are thin.</li>
              <li>Control never rides keystrokes.</li>
              <li>Terminal only. No web console.</li>
            </ul>
          </div>

          <p className="landing-section-route">
            <Link href="/docs/roadmap">Full roadmap</Link>
          </p>
        </section>

        <section className="landing-provenance landing-shell" aria-labelledby="provenance-title">
          <div className="landing-section-heading">
            <h2 id="provenance-title">Built by the thing it builds</h2>
            <p>
              The public repository begins after months of agent-operated software work, with
              authorship stated plainly.
            </p>
          </div>

          <dl className="landing-provenance-facts">
            <div>
              <dt>7,300+</dt>
              <dd>contributions in the measured window</dd>
            </div>
            <div>
              <dt>5 mo</dt>
              <dd>through July 2026</dd>
            </div>
            <div>
              <dt>1 public repo</dt>
              <dd>the first from this operating system</dd>
            </div>
          </dl>

          <p className="landing-provenance-disclosure">
            This profile&apos;s contribution graph is the receipt — 7,300+ contributions in the five
            months to July 2026, nearly all agent-authored, and until this repo, all of it in
            private repos. This is the first public one, and the agents that produced that graph are
            writing this codebase too: most commits here are agent-authored under human direction
            and review. That&apos;s disclosed as a fact, not a caveat — the same gates apply
            regardless of who typed the code.
          </p>

          <p className="landing-section-route">
            <a href={sourceUrl} rel="noreferrer">
              Inspect the public repository
            </a>
          </p>
        </section>

        <section className="landing-cleanroom landing-shell" aria-labelledby="cleanroom-title">
          <div className="landing-section-heading">
            <h2 id="cleanroom-title">Clean-room means a traceable boundary</h2>
            <p>
              Apache-2.0 engine work is derived from permitted specifications and observations,
              never incompatible source.
            </p>
          </div>

          <div className="landing-cleanroom-panel">
            <div className="landing-verdict landing-verdict--permitted">
              <h3>Permitted</h3>
              <ul>
                <li>Public specifications cited by stable derivation ID.</li>
                <li>Observed wire behavior captured in the public registry.</li>
              </ul>
            </div>
            <div className="landing-verdict landing-verdict--forbidden">
              <h3>Forbidden</h3>
              <ul>
                <li>
                  Code copied, ported, or mechanically translated from an incompatibly licensed
                  project.
                </li>
                <li>
                  Copyleft terminal-multiplexer source in a gated engine author&apos;s context.
                </li>
              </ul>
            </div>
            <p className="landing-second-reader">
              <strong>Independent second reader.</strong> Every clean-room change gets an additional
              fresh-context review with no exposure to the implementing session. The reader is not a
              second human, and the status check does not claim reviewer independence.
            </p>
          </div>

          <p className="landing-section-route">
            <Link href="/docs/derivation">Read the derivation trail</Link>
          </p>
        </section>

        <section className="landing-crates landing-shell" aria-labelledby="crates-title">
          <div className="landing-section-heading">
            <h2 id="crates-title">Crates, without padding the surface</h2>
            <p>
              Published contract and kernel crates sit beside the engine and TUI work that remains
              planned.
            </p>
          </div>

          {/* A horizontally scrollable region has to be focusable or a keyboard user
              cannot reach the overflow. The rule cannot see that; this is the pattern
              it is meant to allow. The directive sits on the attribute, not on the
              opening tag: `disable-next-line` matches the line the attribute is on,
              and this tag spans several. */}
          <section
            className="landing-crates-scroll"
            aria-labelledby="crates-title"
            // oxlint-disable-next-line jsx-a11y/no-noninteractive-tabindex
            tabIndex={0}
          >
            <table>
              <caption>Published and planned crates · stable Rust, MSRV 1.94</caption>
              <thead>
                <tr>
                  <th scope="col">Crate</th>
                  <th scope="col">What</th>
                  <th scope="col">Status</th>
                </tr>
              </thead>
              <tbody>
                {crates.map(([name, href, description, status]) => (
                  <tr key={name}>
                    <th scope="row">{href ? <a href={href}>{name}</a> : <code>{name}</code>}</th>
                    <td>{description}</td>
                    <td>{status}</td>
                  </tr>
                ))}
              </tbody>
              <tfoot>
                <tr>
                  <td colSpan={3}>
                    Before contributing, review the prerequisites and enforced gates in{" "}
                    <a href={`${sourceFilesUrl}/CONTRIBUTING.md`} rel="noreferrer">
                      CONTRIBUTING.md
                    </a>
                    .
                  </td>
                </tr>
              </tfoot>
            </table>
          </section>
        </section>
      </main>

      <footer className="landing-footer">
        <div className="landing-footer-texture" aria-hidden="true">
          ⠁⠃⠇⠏⠟⠿⠾⠶⠦⠤⠠ ⠁⠉⠙⠹⠸⠰⠠ ⠂⠆⠇⠧⠷⠿⠻⠹⠸⠰
        </div>
        <div className="landing-shell landing-footer-inner">
          <p>
            <strong>GridWork</strong>
            <span>One log. One kernel. Terminal only.</span>
          </p>

          <div className="landing-footer-columns">
            <nav aria-labelledby="footer-docs-title">
              <h2 id="footer-docs-title">Docs</h2>
              <ul>
                <li>
                  <Link href="/docs/architecture">Architecture</Link>
                </li>
                <li>
                  <Link href="/docs/protocol">Protocol</Link>
                </li>
                <li>
                  <Link href="/docs/parity">Parity</Link>
                </li>
                <li>
                  <Link href="/docs/contract">Contract</Link>
                </li>
                <li>
                  <Link href="/docs/security">Threat model</Link>
                </li>
                <li>
                  <Link href="/docs/derivation">Derivation</Link>
                </li>
              </ul>
            </nav>

            <nav aria-labelledby="footer-project-title">
              <h2 id="footer-project-title">Project</h2>
              <ul>
                <li>
                  <a href={sourceUrl} rel="noreferrer">
                    GitHub
                  </a>
                </li>
                <li>
                  <Link href="/docs/roadmap">Roadmap</Link>
                </li>
                <li>
                  <a href={`${sourceFilesUrl}/CONTRIBUTING.md`} rel="noreferrer">
                    Contributing
                  </a>
                </li>
                <li>
                  <a href={`${sourceFilesUrl}/CLEANROOM.md`} rel="noreferrer">
                    Clean-room policy
                  </a>
                </li>
                <li>
                  <a href={`${sourceFilesUrl}/LICENSE`} rel="noreferrer">
                    Apache-2.0
                  </a>
                </li>
                <li>
                  <a href={`${sourceFilesUrl}/SECURITY.md`} rel="noreferrer">
                    SECURITY.md
                  </a>
                </li>
                <li>
                  <Link href="/privacy">Privacy</Link>
                </li>
              </ul>
            </nav>
          </div>
        </div>
      </footer>
    </>
  );
}
