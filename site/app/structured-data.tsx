import { SITE_ORIGIN, absolute } from "@/lib/site-url";

// schema.org JSON-LD. The entity graph roots at the GridWork Digital hub, which
// owns the authoritative Organization and Brand nodes; this site emits
// REFERENCE nodes carrying only @id plus what is true from here, so the two
// surfaces reconcile on the IRI instead of racing to restate each other. The
// hub's origin is not live yet — an @id is a name, not a fetch, so the edge is
// correct today and resolves the day it ships. The visible footer backlink is
// deliberately NOT here: that one waits for a page a reader can actually open.
//
// Data blocks are inert, not executable script, so the CSP's script-src does
// not apply to them.
//
// Every claim below is a repo fact: the license, the languages, the published
// version, and the first-publish date, which is the crates.io timestamp for
// gwk-domain 0.0.1. No install counts, no users, no adoption.

const HUB_ORIGIN = "https://gridworkdigital.com";
const organizationId = `${HUB_ORIGIN}/#organization`;
const brandId = `${HUB_ORIGIN}/#gridwork`;
const repository = "https://github.com/GridWork-dev/gridwork";

export function StructuredData() {
  const organization = {
    "@type": "Organization",
    "@id": organizationId,
    name: "GridWork",
    legalName: "GridWork Digital LLC",
    url: HUB_ORIGIN,
  };

  const brand = {
    "@type": "Brand",
    "@id": brandId,
    name: "gridwork",
    url: SITE_ORIGIN,
  };

  // SoftwareApplication over SoftwareSourceCode: the thing this site is about
  // is an installable binary (`cargo install gridwork`), not a codebase to read.
  // codeRepository carries the source half rather than a second node claiming
  // to be a different entity.
  const application = {
    "@type": "SoftwareApplication",
    "@id": `${SITE_ORIGIN}/#gridwork`,
    name: "GridWork",
    alternateName: "gw",
    applicationCategory: "DeveloperApplication",
    operatingSystem: "Linux, macOS",
    url: SITE_ORIGIN,
    description:
      "An agent operating system for the terminal: one Rust binary, one append-only event log as the source of truth, and a TUI as the only surface.",
    codeRepository: repository,
    programmingLanguage: "Rust",
    license: "https://www.apache.org/licenses/LICENSE-2.0",
    isAccessibleForFree: true,
    // Pinned by tools/check-claims.sh C10 against crates/gridwork/Cargo.toml.
    // This is the third surface to state the published version and the last one
    // to be found: it sat at 0.0.2 through the 0.0.3 release because it is
    // machine-readable metadata nobody reads by eye. It is also the surface
    // where being wrong costs most — search engines and crawlers ingest it as
    // fact, not as prose to be weighed.
    softwareVersion: "0.0.3",
    // datePublished is first publication, not this release — schema.org means
    // the date the thing first existed, and it stays at gwk-domain 0.0.1's
    // crates.io timestamp while softwareVersion moves. They look like a pair
    // and are not one.
    datePublished: "2026-07-28",
    downloadUrl: "https://crates.io/crates/gridwork",
    documentation: absolute("/docs"),
    brand: { "@id": brandId },
    publisher: { "@id": organizationId },
    author: { "@id": organizationId },
    sameAs: [repository, "https://crates.io/crates/gridwork"],
  };

  const graph = {
    "@context": "https://schema.org",
    "@graph": [organization, brand, application],
  };

  return (
    <script
      type="application/ld+json"
      // inert JSON-LD built from literals in this file — no user input reaches this sink
      dangerouslySetInnerHTML={{ __html: JSON.stringify(graph) }}
    />
  );
}
