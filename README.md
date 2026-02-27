# trillium-frontend

[![ci][ci-badge]][ci]
[![crates.io version badge][version-badge]][crate]

[ci]: https://github.com/jbr/trillium-frontend/actions?query=workflow%3ACI
[ci-badge]: https://github.com/jbr/trillium-frontend/workflows/CI/badge.svg
[version-badge]: https://img.shields.io/crates/v/trillium-frontend.svg?style=flat-square
[crate]: https://crates.io/crates/trillium-frontend

A [Trillium](https://trillium.rs) handler for serving JS frontend projects that works transparently in both development and production:

- **Debug mode**: auto-detects your framework and package manager, spawns the dev server on a free port, and proxies all requests to it — including WebSocket upgrades for HMR
- **Release mode**: runs the frontend build at **compile time** and embeds all dist assets directly in the binary via [`trillium-static-compiled`](https://docs.rs/trillium-static-compiled)

## Usage

```toml
[dependencies]
trillium-frontend = "0.1"
```

```rust
use trillium_client::Client;
use trillium_frontend::frontend;
use trillium_smol::ClientConfig;

fn main() {
    trillium_smol::run((
        frontend!("./client")
            .with_client(Client::new(ClientConfig::default()))
            .with_index_file("index.html"),
    ));
}
```

The `frontend!` macro expands differently based on `cfg(debug_assertions)`:

- **Debug**: returns a `FrontendHandler` that will spawn your dev server and proxy to it on `Handler::init`
- **Release**: runs your build command at compile time and embeds the output as a `FrontendHandler` with static assets

## Macro syntax

```rust
// Simple: path is relative to the Cargo.toml of the calling crate
frontend!("./client")

// With explicit overrides
frontend!(
    path = "./client",
    build = "bun run build",
    dist = "dist",
)
```

| Argument | Description |
|----------|-------------|
| `path` | Path to the frontend project directory (required) |
| `build` | Build command override (default: auto-detected from framework) |
| `dist` | Dist directory name relative to `path` (default: auto-detected, usually `"dist"`) |

## Auto-detection

In debug mode (and for release builds without explicit `build`/`dist` arguments), `trillium-frontend` inspects the project directory for known config and lock files.

**Package manager** (detected by lock file, in priority order):

| Lock file | Package manager | Run prefix |
|-----------|----------------|------------|
| `bun.lockb` / `bun.lock` | Bun | `bun run` |
| `pnpm-lock.yaml` | pnpm | `pnpm run` |
| `yarn.lock` | Yarn | `yarn run` |
| `package-lock.json` | npm | `npx` |

**Framework** (detected by config file):

| Config file | Framework | Dev command | Build command | Dist dir |
|-------------|-----------|-------------|---------------|----------|
| `vite.config.{js,ts,mjs}` | Vite | `vite --strictPort --clearScreen false` | `vite build` | `dist` |
| `webpack.config.{js,ts,mjs}` | Webpack | `webpack serve` | `webpack build` | `dist` |
| `next.config.{js,ts,mjs}` | Next.js | `next dev` | `next build` | `.next` |

The full dev command is assembled as `{run_prefix} {framework_command} --port {port}`. The `PORT` environment variable is also set, so you can reference it in a custom `dev_command`.

## Builder API

`FrontendHandler` is returned by the `frontend!` macro and offers these builder methods:

| Method | Description |
|--------|-------------|
| `.with_client(Client)` | **Required in dev mode.** Provide your runtime's HTTP connector for the proxy. |
| `.with_index_file("index.html")` | Enable SPA fallback: serve this file for any unmatched path. In release mode this also applies to the embedded static handler. |
| `.with_dev_command("npm run dev -- --port $PORT")` | Override the auto-detected dev command. When set, you are responsible for port handling — `$PORT` is exported as an env var. |
| `.with_dev_port(3000)` | Pin the dev server to a specific port instead of picking a free one automatically. |

## How it works

### Debug builds

1. The proc macro (`trillium-frontend-macros`) detects the framework and package manager at **compile time** and records the dev command in the `FrontendHandler`.
2. When Trillium calls `Handler::init`, `FrontendHandler`:
   - Picks a free port (or uses `.with_dev_port`)
   - Spawns `sh -c "<dev_command> --port <port>"` in the project directory
   - Polls the port until the dev server responds (up to ~1 second with 10 ms retries)
   - Builds a `trillium-proxy` pointing at `http://localhost:<port>` with WebSocket upgrade support
3. All subsequent requests (HTTP and WS) are forwarded to the dev server.
4. On `Drop`, the dev process is killed.

### Release builds

1. At **compile time**, the proc macro runs the build command (`sh -c "<build_command>"`) in the project directory.
2. The compiled `dist/` directory is embedded using `trillium-static-compiled::static_compiled!`.
3. If `dist/index.html` exists, it is also embedded as a separate SPA fallback handler.
4. No external process is spawned at runtime.

## Safety

This crate uses `#![forbid(unsafe_code)]` (via its dependencies; the crate itself contains no unsafe code).

## License

<sup>
Licensed under either of <a href="LICENSE-APACHE">Apache License, Version
2.0</a> or <a href="LICENSE-MIT">MIT license</a> at your option.
</sup>

---

<sub>
Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this crate by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.
</sub>
