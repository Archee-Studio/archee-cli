# Archee CLI

`archee` is the command line for Archee Studio. It has one job: let Archee work in a folder on your computer. You sign in, run `archee connect`, and Archee can then read and write files in that folder and run commands there while you chat in the web app. It is a thin client of the Archee Studio API: the limits, permissions and decisions live on the server.

## Commands

```bash
archee login              # short code in the browser; or set ARCHEE_TOKEN
archee connect [FOLDER]   # serve FOLDER (default: here) until Ctrl-C; --name NAME to choose the name shown
archee whoami
archee logout
archee update             # install the latest release over this one
```

Every command takes `--json`. Plain `archee` shows the sign-in status and the next step. Environment: `ARCHEE_URL` (server, default `https://app.archee.studio`; plain `http` only for localhost) and `ARCHEE_TOKEN` (an access token instead of the stored sign-in).

Exit codes: 0 ok, 1 error, 2 usage, 3 sign-in needed.

## Security

`archee connect` is not a sandbox.

- Commands Archee runs there execute as your operating system user. A script can read any file that user can read, including your stored sign-in, and use the network.
- The file tools stay inside the served folder (no absolute paths, no `..`, no symlink escape). Shell commands are not confined by that.
- `ARCHEE_ON_COMPUTER` is set for those commands and makes `archee` refuse; it guards against accidents, it is not a security boundary.
- `connect` keeps no account sign-in on disk for itself. A saved `archee login` stays readable, so run `archee logout` before you connect if that matters to you.
- The worker dials out only: it registers, sends heartbeats and long-polls for work. Nothing connects in, so no inbound port is needed.
- Every step of an operation is written to a journal under `~/.local/state/archee-cli/` before its network call. A change whose start was recorded but whose result was not is never run again without your say-so on the server.
- Credentials are stored outside the served folder with mode 0600.

Only connect folders you are willing to share with Archee.

## Build

```bash
cargo build --release -p archee-cli   # from the workspace root; the binary is target/release/archee
cargo test -p archee-cli
cargo clippy -p archee-cli --all-targets -- -D warnings
```

The Linux and macOS builds are supported. Windows is **beta** until a person has tested it on a real PC; CI runs the suite on Windows.

## Windows

- Scripts run in Windows PowerShell 5.1 (`powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command`, UTF-8 output), which ships with Windows. Git Bash is not needed. The server tells the agent the computer is Windows, so it writes PowerShell and Windows paths.
- Stop uses `taskkill /T /F /PID`. A process that fully detached from its parent can survive it, so Stop is slightly weaker than the Unix process-group kill.
- The served folder's identity is a digest of its normalized path, so a folder replaced under the same path is not detected, unlike the Unix device and inode check.
- The binary is unsigned: Defender or SmartScreen may warn on first run.
- Ctrl-C once drains, twice kills the running command, three times exits 130; closing the console counts as a Ctrl-C.
