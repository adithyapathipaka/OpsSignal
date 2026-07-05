---
name: Feature request
about: Suggest a provider, sink, adapter, or capability
labels: enhancement
---

**What problem does this solve?**

**Is this a new provider, sink, or framework adapter?**
If so, see CONTRIBUTING.md — these should fit the existing trait
boundaries (`Provider` / `SignalSink`) without touching routing/retry
logic. PRs are very welcome for these.

**If this is a request for Java or Go bindings:**
These are intentionally deferred until there's demonstrated real-world
demand (see README roadmap). If you need this for production use, please
describe your use case and language/runtime constraints in detail — that
context is exactly what would justify prioritizing it.

**Proposed approach (optional):**
