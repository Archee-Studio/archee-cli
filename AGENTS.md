# archee-cli

`archee` connects a folder on the user's computer to Archee Studio. It is a thin client: limits, permissions and decisions live on the server.

## Commands

```sh
cargo build --release          # target/release/archee
cargo test
cargo clippy --all-targets -- -D warnings
```

Every command takes `--json`. Exit codes: 0 ok, 1 error, 2 usage, 3 sign-in needed. Environment: `ARCHEE_URL` (default `https://app.archee.studio`; plain `http` only for localhost), `ARCHEE_TOKEN` (instead of the stored sign-in).

## How `connect` behaves

- Commands run as the user's operating system user; the file tools stay inside the served folder (no absolute paths, `..` or symlink escape), shell commands do not.
- `ARCHEE_ON_COMPUTER` is set for those commands and makes `archee` refuse; an accident guard, not a boundary.
- The worker only dials out: register, heartbeat, long-poll. No inbound port.
- Every step is journalled under `~/.local/state/archee-cli/` before its network call; a started step with no recorded result is never rerun without the user's say-so.
- Credentials live outside the served folder, mode 0600.

## Windows (beta)

- Scripts run in Windows PowerShell 5.1 (`-NoProfile -NonInteractive -ExecutionPolicy Bypass`, UTF-8); no Git Bash.
- Stop is `taskkill /T /F /PID`; a fully detached child can survive it.
- The folder identity is a digest of its normalised path, so a folder replaced under the same path is not detected.
- The binary is unsigned; SmartScreen may warn. Ctrl-C once drains, twice kills the command, three times exits 130.

## Rules

This repository is published from private source; pull requests are reapplied upstream. Keep output compact and non-interactive.
