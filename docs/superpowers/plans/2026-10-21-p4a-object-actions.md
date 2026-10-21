# P4a implementation plan

Approved scope: local-first object actions; no CI changes.

1. Add typed SQL action builder and dialect/identifier tests.
2. Add bounded, single-use action plans, dedicated sessions, backend policy/context checks, cancellation and explicit outcomes.
3. Persist object-action history with a backwards-compatible migration; invalidate metadata and edit tokens.
4. Add tree actions and preview/confirmation dialog; block staged edits and refresh affected tabs.
5. Verify builders, plan/operation lifecycle and database actions; run local Rust/UI checks and desktop smoke when the environment supports it. Record actual results and limitations.

Each stage follows existing core/session/IPC/store patterns. Preserve main-worktree changes. P4b is outside this implementation.
