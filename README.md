# DntDatabase

A desktop database manager for MySQL / MariaDB and PostgreSQL, built with Rust, Tauri 2 and React.
Target platform for P1: Windows 10/11 x64.

## Prerequisites

- Rust stable, MSVC toolchain (`rustup default stable-x86_64-pc-windows-msvc`)
- Visual Studio Build Tools with the "Desktop development with C++" workload
- Node 22 and pnpm 10
- Docker Desktop (integration tests and E2E)
- WebView2 runtime (ships with Windows 11 and current Windows 10)

Install JS dependencies once:

```powershell
pnpm install
pnpm --dir ui install
```

## Run in development

```powershell
pnpm tauri dev
```

Run it from the repository root. It starts the Vite dev server on **port 5173**, which must be free.

## Tests

Unit tests (no database needed; run `gen_fixtures` first, the TLS tests read `testdata/certs`):

```powershell
cargo run -p dnt-testkit --features gen-fixtures --bin gen_fixtures
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Integration tests against real servers (MySQL 5.7/8, MariaDB 10.6/11.4, PostgreSQL 12/17, an SSH bastion):

```powershell
cargo run -p dnt-testkit --features gen-fixtures --bin gen_fixtures   # before compose: the ssh container mounts testdata/ssh
docker compose -f testdata/docker-compose.test.yml up -d --wait
$env:DNT_TEST_DB = "1"
cargo test --workspace
docker compose -f testdata/docker-compose.test.yml down -v
```

Without `DNT_TEST_DB=1` the integration tests print `skipped` and return. Keep `cargo test` (sequential test
binaries); the tunnel tests restart the ssh container.

UI tests:

```powershell
pnpm --dir ui lint
pnpm --dir ui test
```

### End-to-end smoke test

Local only (needs a Windows desktop session and Docker; CI does not run it). Scenario: connect to the MySQL 8
container at `127.0.0.1:33080`, open `dnt_fixture.orders` and check that the DDL tab shows `CREATE TABLE`.

One-time setup:

1. `cargo install tauri-driver --locked`
2. Download the `msedgedriver` that matches your WebView2 runtime exactly. Find the version in
   `HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}` (value `pv`),
   then fetch `https://msedgedriver.microsoft.com/<version>/edgedriver_win64.zip` and extract `msedgedriver.exe`
   to `D:\dev\msedgedriver\` (or anywhere, then set `MSEDGEDRIVER`). Redo this whenever WebView2 updates.
3. `pnpm --dir e2e install`

Each run:

```powershell
docker compose -f testdata/docker-compose.test.yml up -d --wait
pnpm --dir ui build
cargo build -p dntdatabase --features tauri/custom-protocol    # embeds ui/dist; a plain debug build loads the dev server
pnpm --dir e2e test
```

Optional environment: `MSEDGEDRIVER` (path to msedgedriver.exe), `TAURI_DRIVER` (path to tauri-driver.exe,
default `%CARGO_HOME%\bin`). The run uses a fresh temporary `DNT_DATA_DIR` and removes the test password it stored
in Windows Credential Manager when it finishes.

## Fixtures

`cargo run -p dnt-testkit --features gen-fixtures --bin gen_fixtures` generates the TLS certificates
(`testdata/certs`) and the SSH keys (`testdata/ssh`). Both directories are git-ignored.

## TypeScript bindings

The Tauri command bindings are generated from Rust (specta). A debug run of the app refreshes them; to export
without starting the app:

```powershell
cargo run -p dntdatabase --example export_bindings
```

## Data, logs and environment

App data lives in `%APPDATA%\DntDatabase\`: `app.db` (SQLite), `known_hosts` and `logs\`
(`dntdatabase.log.<date>`, daily rotation, 7 days kept). Set `DNT_DATA_DIR` to use another directory
(used by the E2E test). Secrets are kept in Windows Credential Manager under `DntDatabase/<connection uuid>/<kind>`,
never in SQLite or logs.
