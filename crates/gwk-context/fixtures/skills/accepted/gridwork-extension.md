---
name: gridwork-extension
description: Carries a GridWork extension in the one place it can live.
metadata:
  team: infra
  gridwork: '{"routes": ["code_review"], "budget_tokens": 8192}'
---

Upstream types `metadata` as `dict[str, str]`, so the extension is a bounded
JSON string rather than a nested mapping. The key does not also survive as raw
metadata — it is decoded once, into one interpretation.
