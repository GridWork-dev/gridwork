//! Engine-name → spawn-factory routing: the seam where "adapters live
//! server-side" is code. Each adapter owns how its engine's TUI is put on a
//! PTY (`spawn_tui`); this module only routes a name to that function, so a
//! fourth engine is one arm here and zero changes anywhere else in the host.
//!
//! Derivation: none — a three-arm match over the adapter crates' own public
//! constants and spawn functions; no engine protocol fact originates here.

use crate::session::SpawnFn;

/// The spawn factory for a named engine, or `None` for a name no adapter
/// claims. The names are the adapters' own `ENGINE` constants — never
/// re-spelled here, so a renamed engine cannot silently strand this match.
pub fn spawn_fn(engine: &str) -> Option<SpawnFn> {
    match engine {
        gwk_adapter_claude::ENGINE => Some(Box::new(gwk_adapter_claude::spawn_tui)),
        gwk_adapter_codex::ENGINE => Some(Box::new(gwk_adapter_codex::spawn_tui)),
        gwk_adapter_opencode::ENGINE => Some(Box::new(gwk_adapter_opencode::spawn_tui)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_adapter_engine_routes_and_an_unknown_name_does_not() {
        // The three adapters' own names route; the check is intentionally
        // through their constants so an adapter rename moves this test with
        // it instead of leaving a stale literal green.
        for engine in [
            gwk_adapter_claude::ENGINE,
            gwk_adapter_codex::ENGINE,
            gwk_adapter_opencode::ENGINE,
        ] {
            assert!(spawn_fn(engine).is_some(), "{engine} should route");
        }
        assert!(spawn_fn("not-an-engine").is_none());
    }
}
