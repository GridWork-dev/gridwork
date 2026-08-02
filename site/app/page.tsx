import { CopyCommand } from "@/components/copy-command";

const sourceUrl = "https://github.com/GridWork-dev/gridwork";

export default function HomePage() {
  return (
    <>
      <a className="landing-skip-link" href="#main-content">
        Skip to main content
      </a>

      <header className="landing-status-bar">
        <div className="landing-status-inner">
          <a className="landing-brand" href="/" aria-label="GridWork home">
            <span aria-hidden="true">▌</span> gridwork
          </a>

          <span className="landing-status-segment landing-status-optional">
            apache-2.0
          </span>
          <a className="landing-stage" href="/docs/roadmap">
            stage 3/5 · engines
          </a>
          <span className="landing-status-segment landing-status-optional">
            pre-1.0
          </span>

          <nav className="landing-nav" aria-label="Primary navigation">
            <a href="/docs">Docs</a>
            <a className="landing-nav-optional" href="/docs/roadmap">
              Roadmap
            </a>
            <a
              className="landing-nav-optional"
              href={sourceUrl}
              rel="noreferrer"
            >
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
              <h1 id="landing-title">
                An agent operating system for the terminal.
              </h1>
              <p className="landing-lede">
                GridWork is the operating layer for a fleet of coding agents: one
                append-only event log, one kernel, and one place that says what
                needs a human.
              </p>

              <div className="landing-actions">
                <a className="landing-button landing-button--primary" href="/docs">
                  Read the docs
                </a>
                <a
                  className="landing-button"
                  href={sourceUrl}
                  rel="noreferrer"
                >
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
              <code>cargo build --workspace   # stable Rust · msrv 1.94</code>
            </div>
            <p className="landing-install-warning">
              <strong>Pre-1.0: expect breakage.</strong> Schemas, protocols, and
              the binary change without notice until 1.0. The headless CLI needs
              PostgreSQL 16 and separate admin and runtime roles. {" "}
              <a href="/docs/quickstart">Review the prerequisites</a>.
            </p>
          </div>
        </section>

        <section
          className="landing-truth landing-shell"
          aria-labelledby="truth-title"
        >
          <div className="landing-section-heading">
            <h2 id="truth-title">Where it actually is</h2>
            <p>
              Pre-alpha, at <strong>stage 3 of 5</strong>. The contract and kernel
              are in the tree; engines and the human interface are the work now. {" "}
              <a href="/docs/roadmap">Read the five-stage roadmap</a>.
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
                <li>The headless <code>gw</code> CLI speaks the kernel protocol.</li>
              </ul>
            </section>

            <section aria-labelledby="not-built-title">
              <h3 id="not-built-title">
                <span aria-hidden="true">◇</span> Not built yet
              </h3>
              <ul>
                <li>The complete PTY engine.</li>
                <li>Complete engine adapters.</li>
                <li>The TUI.</li>
                <li>The daemon&apos;s current human face is JSON command output.</li>
              </ul>
            </section>
          </div>
        </section>
      </main>
    </>
  );
}
