# AGENTS.md

Guidance for AI coding agents working in this repository.

## Project overview

`fvm-anvil` is an Anvil-compatible local development node that executes Ethereum transactions with `ref-fvm` and Filecoin's builtin EAM, EVM, and EthAccount actors. Its primary purpose is faithful, fast Filecoin gas measurement without running Filecoin consensus or mining infrastructure.

Design decisions and current architecture constraints live in `docs/design.md`. Read that document before changing runtime behavior, RPC compatibility, actor initialization, or gas semantics.

The source trees under `extern/` are read-only upstream references:

- `extern/foundry`: RPC behavior, Anvil controls, transport, and compatibility tests.
- `extern/forest`: canonical Ethereum-to-Filecoin conversion and RPC semantics.
- `extern/ref-fvm`: FVM machine, executor, blockstore, gas accounting, and integration harness.
- `extern/builtin-actors`: EAM, EVM, EthAccount state and execution behavior.

Never edit files under `extern/`. Adapt narrowly scoped upstream code into this workspace with attribution where required.

## Commands

```bash
cargo build --workspace
cargo test --workspace
cargo fmt --all
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo metadata --no-deps --format-version 1
```

Use focused package or test commands during development. Run workspace formatting, Clippy, and tests once after the changed behavior has been exercised directly.

## Architecture

`docs/design.md` records the current direction, not a permanent package or module layout. Let the code structure emerge from complete vertical slices:

- Add a crate or module only when it owns real behavior.
- Keep related behavior together until a concrete boundary makes extraction useful.
- Do not pre-create empty modules, generic utility crates, speculative traits, or abstraction layers.
- Prefer moving proven code over predicting future reuse.

Keep these established system constraints:

- Execute contracts with `ref-fvm`, never `revm`.
- Keep JSON-RPC parsing separate from mutable FVM execution state.
- Ensure one clear owner for the engine pool, blockstore, current root, synthetic chain, pool, and snapshots.
- Do not publish state roots produced by calls or gas estimation.
- Reuse the FVM engine pool and reconstruct machines from the selected root and block context.
- Validate Ethereum signatures and chain IDs from the original transaction envelope.
- Calculate inclusion gas from the exact serialized Filecoin `SignedMessage`.
- Preserve actor-state and IPLD invariants when adding direct state controls.
- Return a clear unsupported error when FEVM cannot provide an Anvil behavior; never fake success.

Use public Foundry and `ref-fvm` crates where they fit. Adapt narrowly scoped Forest and integration-harness behavior rather than taking production dependencies on Forest, `fvm_integration_tests`, `anvil-core`, or the main `anvil` crate.

## Testing

Every test must defend observable behavior and fail on a plausible implementation bug.

- Use focused unit tests for pure protocol and state-transition logic.
- Use integration tests for actor execution and gas accounting with an explicit actor bundle and network version.
- Use black-box endpoint tests for JSON-RPC parameters, aliases, errors, responses, and workflows.
- Adapt applicable Anvil integration cases for behavior compatibility; do not copy tests that only exercise `EthApi` internals.
- Keep actor bundle and network-version inputs explicit in tests that depend on actor state schemas.
- Tests that use a future chain fork must contain `fork` in their name.

For permanent RPC or execution behavior, exercise the real endpoint or runtime path before relying on the test suite. For gas behavior, assert FVM gas outputs or charge names rather than wall-clock time. Keep actor execution fixtures deterministic.

## RPC design

Use Anvil's official documentation and checked-in Foundry request bindings as the wire-format reference. Preserve:

- Exact method names and aliases.
- Empty, omitted, positional, and sequence parameter handling.
- Hex quantity versus fixed-byte encoding.
- Return shapes and stable error codes.
- Mining-mode and snapshot lifecycle semantics.

Keep Filecoin-specific observability under a `filecoin_` namespace. Do not overload standard Ethereum receipt fields with structured diagnostics that clients do not expect.

## CLI output

The CLI follows a strict stdout/stderr contract:

- stdout is for explicitly requested machine-readable output.
- stderr is for warnings, errors, progress, status text, prompts, and the startup banner.
- JSON output changes formatting, not channel cleanliness.
- Quiet mode suppresses diagnostics, not requested results.
- Verbosity changes diagnostics, not stdout content.

Use structured `tracing` for runtime diagnostics. Do not leave unconditional `println!`, `eprintln!`, `dbg!`, or temporary tracing in library code.

## Configuration

When adding or changing configuration:

1. Define one canonical field, explicit default, CLI spelling, environment behavior if any, and serialization behavior.
2. Resolve config-file, environment, and CLI precedence in one place.
3. Validate genesis-time values before starting transports or the worker.
4. Wire the value through every consumer without reading environment variables in library code.
5. Add focused parsing and precedence tests.
6. Update `docs/design.md` when the value changes an architectural default or compatibility guarantee.

Actor bundle, network version, chain ID, genesis accounts, and initial base fee are genesis-time configuration. Runtime RPC changes must not silently rewrite genesis configuration.

## Commit and PR style

Use conventional commits:

```text
type: description
type(scope): description
type(scope)!: breaking description
```

Use `feat`, `fix`, `perf`, `chore`, `docs`, `test`, or `refactor`.

- Use imperative mood.
- Keep the description under 50 characters when practical.
- Do not end the description with a period.
- Include a body for performance changes, bug fixes, and complex changes.
- Include only measured performance claims.

PR descriptions should explain what changed and why in flowing prose. Link related issues and include compatibility or gas evidence when relevant. Do not claim validation that was not run.

## Code style

- Comments end with periods, except URLs.
- Files end with LF and a trailing newline.
- Follow an existing local pattern before introducing another.
- Never expose secrets.
- Prefer boring explicit ownership over abstraction layers.
- Avoid needless allocations, clones, serialization round trips, and actor-state flushes.
- Public APIs describe invariants and failure behavior, not implementation trivia.

### Rust

- Put doc comments before attributes.
- Put module documentation at the top of the module file with inner doc comments (`//!`).
- Keep imports at the top of the file. Function-local imports are only for necessary `#[cfg(...)]` gating.
- Group `use` imports together and keep `pub use` imports in a separate group.
- Prefer `let Some(value) = value else { return ... };` for one early-exit guard.
- Prefer combined `if let` chains over nested `if let` blocks when conditions gate the same behavior.
- Prefer borrowing the expression with `&` or `&mut` over `ref` and `ref mut` patterns.
- Rely on type inference. When a hint is necessary, prefer turbofish at construction over a variable annotation.
- Use newtypes for quantities that are easy to confuse, especially Ethereum gas, FVM gas, epochs, timestamps, message lengths, actor IDs, and snapshot IDs.
- Use checked arithmetic at RPC and transaction boundaries. Document any intentional saturation.
- Preserve original errors with context at subsystem boundaries; map them to JSON-RPC errors only at the RPC boundary.
- Do not panic on user input, remote state, actor state, or RPC requests.
- Avoid trait objects on the execution hot path unless they remove a demonstrated dependency boundary problem.

## Upstream comparison workflow

When reproducing upstream behavior:

1. Locate the exact Foundry or Forest implementation and its tests.
2. Identify which behavior is protocol-required, Anvil compatibility, or merely an upstream internal detail.
3. Reimplement only the required behavior inside the owning crate.
4. Record attribution required by the source license.
5. Add a focused compatibility test using the public boundary.
6. Verify the result against ref-FVM rather than assuming `revm` behavior transfers to FEVM.
