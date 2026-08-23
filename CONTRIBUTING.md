# Contributing to Vela

Welcome. Read this entire document before writing a single line of code
or making any change — including documentation changes.
These rules apply equally to human contributors and AI coding agents.

---

## The 5 mandatory pre-change iterations

Before implementing ANY change, you must complete all five of these steps
and confirm them in your pull request description:

1. **Read the full codebase structure.** Run `find src/ -name "*.rs"` and
   read every file listed. Know what exists before adding anything.

2. **Search for existing implementations.** Before writing a function,
   run `grep -r "fn <your_function_name>" src/`. If something similar exists,
   extend it rather than duplicating it.

3. **Read `src/models.rs` completely.** Every data type in this project
   lives in `models.rs`. If the type you need already exists, use it.
   If it does not, add it to `models.rs` before using it elsewhere.

4. **Read `src/error.rs` completely.** Every error variant lives in
   `VelaError`. If the error you need already exists, use it.
   If it does not, add it to `VelaError` before propagating it.

5. **Read `src/state.rs` completely.** Understand how state is read and
   written before touching any engine. Never bypass the state store.

Your PR description must include a section titled "Pre-change iterations"
stating which existing functions you reviewed and why you chose to
reuse, extend, or not use them.

---

## Non-negotiable rules

These rules have no exceptions. A PR that violates any of them will not
be merged regardless of how good the rest of the code is.

### No `unwrap()` in engine code
Use the `?` operator and propagate errors through `VelaError`.
`unwrap()` is only acceptable inside `#[test]` functions.

### No `panic!()` outside tests
If a situation is truly unrecoverable, log the error and call
`std::process::exit(1)` from `main.rs`. Never panic in an engine.

### No engine-to-engine direct calls
Engines must not call each other's functions directly.
Communication happens through:
- The shared `VelaState` store (for persistent state)
- `tokio::sync::broadcast` channels (for events like status changes)

This rule enforces separation of concerns and prevents circular dependencies.

### All public functions must have doc comments
Every `pub fn` must have a `///` doc comment explaining:
- What the function does
- What it returns
- Any important preconditions

### All errors must use `VelaError`
Never define a local error type in an engine file.
Never use `Box<dyn Error>` as a return type in engine code.
Always add to `VelaError` if a new error category is needed.

### No new dependencies without justification
Before adding a crate to `Cargo.toml`:
1. Confirm the standard library cannot solve the problem.
2. Confirm an existing project dependency cannot solve the problem.
3. Add an inline comment in `Cargo.toml` explaining the justification.

A PR that adds an unjustified dependency will not be merged.

### Never hold a write lock across an `await` point
In async Rust, holding a `RwLock` or `Mutex` across `.await` causes
deadlocks. Always:
1. Acquire the lock
2. Perform the write
3. Drop the lock (or let it go out of scope)
4. Then await

### Security rules
- The API engine must check the `Authorization: Bearer <key>` header
  on every request. No endpoint is exempt.
- Never log the API key, even at debug level.
- Never accept a config file with an empty `api_key`.

---

## Code style

- Format all code with `cargo fmt` before committing.
- All warnings must be resolved. No PR with `cargo build` warnings
  will be merged.
- Run `cargo clippy` and address all suggestions before submitting.
- Run `cargo audit` and address any high-severity findings.

## Testing requirements

- Every new public function must have at least one unit test.
- Every engine must have an integration test covering its happy path.
- Tests live in:
  - `src/<engine>.rs` → `#[cfg(test)]` block at the bottom of the file
  - `tests/integration/` → full-stack integration tests

## Commit message format

```
<type>(<scope>): <short description>

<body — what changed and why>

<footer — references issues, breaking changes>
```

Types: `feat`, `fix`, `refactor`, `test`, `docs`, `chore`
Scopes: `config`, `health`, `recovery`, `alert`, `proxy`, `api`, `state`, `models`

Examples:
```
feat(health): add HTTP health check support

Extends the health engine to support GET requests in addition
to TCP connection checks. The check kind is controlled by
HealthCheckConfig.kind in the service config.

Closes #12
```

---

## Pull request checklist

Before opening a PR, confirm all of these:

- [ ] I completed all 5 mandatory pre-change iterations
- [ ] I ran `cargo fmt`
- [ ] I ran `cargo clippy` and resolved all warnings
- [ ] I ran `cargo audit` and found no high-severity issues
- [ ] I ran `cargo test` and all tests pass
- [ ] I added doc comments to all new public functions
- [ ] I added at least one test for every new public function
- [ ] I did not add `unwrap()` in engine code
- [ ] I did not add `panic!()` outside tests
- [ ] I did not define new types outside `models.rs`
- [ ] I did not define new error variants outside `error.rs`
- [ ] I did not add a dependency without inline justification
- [ ] My PR description includes the "Pre-change iterations" section
