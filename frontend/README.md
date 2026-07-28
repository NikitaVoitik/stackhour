# Frontend quality baseline

This directory is the future control-panel frontend. The provisional desktop stack is Tauri 2 with
React and TypeScript.

The repository benchmark does not prove that choice. It contains packaged Vanilla Electron and
Svelte Electron experiments. Its only run log ends with DBus errors and has no comparative result.
Tauri must be benchmarked against the browser-only panel before desktop packaging becomes a product
constraint.

Use the repository verification profiles:

- Fast checks formatting and strict TypeScript.
- Standard adds zero-warning ESLint and unit tests.
- Full adds 100% coverage for this initial shell, a production build, dead-code analysis,
  dependency-cycle analysis, bundle-size limits, and a high-severity dependency audit.

When `src-tauri` is added, put its crate in the root Rust workspace or make it inherit the same Rust
lint policy. Tauri capabilities, commands, and permissions are security boundaries and require the
Deep profile.
