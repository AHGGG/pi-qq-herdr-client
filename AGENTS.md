# AGENTS.md

## Start here

Read `README.md`. When available in this checkout, read `docs/design.md` and
`docs/implementation-handoff.md` before implementing, then follow their references
for the affected interface. These local documents are intentionally Git-ignored;
never force-add them. A fresh clone may not have them.

## Working rules

- Preserve user changes and keep edits within the requested scope.
- Use worktrees or agents that change code only when the user explicitly asks.
- Keep the desktop UI small and focused. Run network and filesystem work off the UI thread.
- The desktop client uses Rust and GPUI Kit. The companion extension uses Pi's TypeScript API.
- Reuse the user's Herdr Core and Pi environment. Do not restart or replace shared processes without authorization.
- Keep credentials, account identifiers, runtime state and private message content out of Git and diagnostic output.
- Local design and research are planning inputs, not proof of implemented behavior.

## Layout

- `crates/client-core/`: protocol adapters, persistence and session coordination.
- `crates/desktop/`: native presentation and application lifecycle.
- `integrations/pi/`: the companion extension that runs inside Pi.

## Verification

Choose checks that cover the change. Documentation and initialization changes need
whitespace, link and Git-scope checks. Once a Cargo workspace exists, use its pinned
dependencies for formatting, compilation and relevant static checks. Distinguish
compilation from native UI, real-account and packaged-runtime verification.
