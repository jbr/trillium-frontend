//! A [Trillium](https://trillium.rs) handler for serving JS frontend projects that works
//! transparently in both development and production:
//!
//! - **Debug mode**: auto-detects your framework and package manager, spawns the dev server on a
//!   free port, and proxies all requests to it — including WebSocket upgrades for HMR
//! - **Release mode**: runs the frontend build at **compile time** and embeds all dist assets
//!   directly in the binary via [`trillium_static_compiled`]
//!
//! # Usage
//!
//! ```toml
//! [dependencies]
//! trillium-frontend = "0.1"
//! ```
//!
//! ```rust,ignore
//! use trillium_client::Client;
//! use trillium_frontend::frontend;
//! use trillium_smol::ClientConfig;
//!
//! fn main() {
//!     trillium_smol::run((
//!         frontend!("./client")
//!             .with_client(Client::new(ClientConfig::default()))
//!             .with_index_file("index.html"),
//!     ));
//! }
//! ```
//!
//! The [`frontend!`] macro expands differently based on `cfg(debug_assertions)`:
//!
//! - **Debug**: returns a [`FrontendHandler`] that will spawn your dev server and proxy to it on
//!   [`Handler::init`](trillium::Handler::init)
//! - **Release**: runs your build command at compile time and returns a [`FrontendHandler`] with
//!   the compiled assets embedded
//!
//! # Macro syntax
//!
//! ```rust,ignore
//! // Simple: path is relative to the Cargo.toml of the calling crate
//! frontend!("./client")
//!
//! // With explicit overrides
//! frontend!(
//!     path = "./client",
//!     build = "bun run build",
//!     dist = "dist",
//! )
//! ```
//!
//! | Argument | Description |
//! |----------|-------------|
//! | `path`   | Path to the frontend project directory (required) |
//! | `build`  | Build command override (default: auto-detected from framework) |
//! | `dist`   | Dist directory name relative to `path` (default: auto-detected, usually `"dist"`) |
//!
//! # Auto-detection
//!
//! In debug mode (and for release builds without explicit `build`/`dist` arguments),
//! `trillium-frontend` inspects the project directory for known config and lock files.
//!
//! **Package manager** (detected by lock file, in priority order):
//!
//! | Lock file | Package manager | Run prefix |
//! |-----------|----------------|------------|
//! | `bun.lockb` / `bun.lock` | Bun | `bun run` |
//! | `pnpm-lock.yaml` | pnpm | `pnpm exec` |
//! | `yarn.lock` | Yarn | `yarn exec` |
//! | `package-lock.json` | npm | `npx` |
//!
//! **Framework** (detected by config file):
//!
//! | Config file | Framework | Dev command | Build command | Dist dir |
//! |-------------|-----------|-------------|---------------|----------|
//! | `vite.config.{js,ts,mjs}` | Vite | `vite --strictPort --clearScreen false` | `vite build` | `dist` |
//! | `webpack.config.{js,ts,mjs}` | Webpack | `webpack serve` | `webpack build` | `dist` |
//! | `next.config.{js,ts,mjs}` | Next.js | `next dev` | `next build` | `.next` |
//!
//! # Builder API
//!
//! [`FrontendHandler`] is returned by the [`frontend!`] macro and offers these builder methods:
//!
//! | Method | Description |
//! |--------|-------------|
//! | `.with_client(Client)` | **Required in dev mode.** Provide your runtime's HTTP connector for the proxy. |
//! | `.with_index_file("index.html")` | Enable SPA fallback: serve this file for any unmatched path. |
//! | `.with_dev_command("npm run dev -- --port $PORT")` | Override the auto-detected dev command. When set, you are responsible for port handling — `$PORT` is exported as an env var. |
//! | `.with_dev_port(3000)` | Pin the dev server to a specific port instead of picking a free one automatically. |

#![forbid(unsafe_code)]

use fieldwork::Fieldwork;
use std::{
    borrow::Cow,
    io::ErrorKind,
    process::{Child, Command},
    sync::Mutex,
    time::Duration,
};
use trillium::{Conn, Error, Handler, Info, Method, Upgrade, async_trait};
use trillium_client::Client;
use trillium_proxy::{Proxy, Url};
use trillium_static_compiled::StaticCompiledHandler;

#[derive(Fieldwork)]
#[fieldwork(opt_in, with, into, option_set_some)]
pub struct FrontendHandler {
    assets: Option<StaticCompiledHandler>,

    /// A `StaticCompiledHandler` pointing at the dist index file, used for SPA
    /// fallback (serves index for any path not matched by `assets`).
    spa_fallback: Option<StaticCompiledHandler>,

    project_path: Cow<'static, str>,

    detected_dev_command: Option<Cow<'static, str>>,

    /// trillium HTTP client for dev-mode proxying (provide with your runtime connector)
    #[field]
    client: Option<Client>,

    /// override auto-detected dev command
    #[field]
    dev_command: Option<Cow<'static, str>>,

    /// override auto-detected dev port
    #[field(into = false)]
    dev_port: Option<u16>,

    /// opt-in SPA index file fallback (e.g. `"index.html"`)
    #[field]
    index_file: Option<&'static str>,

    // Runtime state (set in init)
    dev_process: Option<Mutex<Child>>,

    proxy: Option<Proxy<Url>>,
}

impl FrontendHandler {
    #[doc(hidden)]
    pub fn new(
        assets: Option<StaticCompiledHandler>,
        spa_fallback: Option<StaticCompiledHandler>,
        project_path: &'static str,
        detected_dev_command: Option<&'static str>,
    ) -> Self {
        FrontendHandler {
            assets,
            spa_fallback,
            project_path: Cow::Borrowed(project_path),
            detected_dev_command: detected_dev_command.map(Cow::Borrowed),
            client: None,
            dev_command: None,
            dev_port: None,
            index_file: None,
            dev_process: None,
            proxy: None,
        }
    }
}

#[async_trait]
impl Handler for FrontendHandler {
    async fn run(&self, conn: Conn) -> Conn {
        if let Some(assets) = self.assets {
            let conn = assets.run(conn).await;
            if !conn.is_halted()
                && let Some(spa) = self.spa_fallback.as_ref()
            {
                return spa.run(conn).await;
            }
            return conn;
        }

        if let Some(proxy) = &self.proxy {
            return proxy.run(conn).await;
        }

        conn
    }

    async fn init(&mut self, info: &mut Info) {
        if self.assets.is_some() {
            if let Some(index) = self.index_file {
                self.assets = self.assets.map(|h| h.with_index_file(index));
            }
            return;
        }

        let client = self
            .client
            .take()
            .expect("trillium-frontend: in dev mode, provide a Client via .with_client()")
            .with_default_pool();

        let port = self.dev_port.unwrap_or_else(|| {
            portpicker::pick_unused_port()
                .expect("trillium-frontend: could not find a free port for the dev server")
        });

        // If the user provided an explicit dev command, use it verbatim and let
        // them handle the port (we still export PORT so they can reference $PORT
        // in their script if they want). If we auto-detected the command from a
        // known framework, we know --port is supported and append it ourselves.
        let dev_command = if let Some(cmd) = self.dev_command.as_deref() {
            cmd.to_string()
        } else {
            let detected = self.detected_dev_command.as_deref().expect(
                "trillium-frontend: no dev command detected; configure with .with_dev_command()",
            );
            format!("{detected} --port {port}")
        };

        let child = Command::new("sh")
            .arg("-c")
            .arg(&dev_command)
            .env("PORT", port.to_string())
            .current_dir(self.project_path.as_ref())
            .spawn()
            .expect("trillium-frontend: failed to spawn dev server");
        self.dev_process = Some(Mutex::new(child));

        let upstream = format!("http://localhost:{port}").parse().unwrap();
        wait_for_port(&upstream, &client).await;

        let mut proxy = Proxy::new(client, upstream).with_websocket_upgrades();
        proxy.init(info).await;
        self.proxy = Some(proxy);
    }

    fn has_upgrade(&self, upgrade: &Upgrade) -> bool {
        self.proxy.as_ref().is_some_and(|p| p.has_upgrade(upgrade))
    }

    async fn upgrade(&self, upgrade: Upgrade) {
        if let Some(proxy) = &self.proxy {
            proxy.upgrade(upgrade).await;
        }
    }
}

impl Drop for FrontendHandler {
    fn drop(&mut self) {
        if let Some(mutex) = self.dev_process.take()
            && let Ok(mut child) = mutex.into_inner()
        {
            let _ = child.kill();
        }
    }
}

async fn wait_for_port(upstream: &Url, client: &Client) {
    for _ in 0..100 {
        match client.build_conn(Method::Head, upstream.clone()).await {
            Ok(_) => {
                log::debug!("Successfully connected to {upstream}");
                // we don't care what the response was as long as it was valid http
                return;
            }

            Err(Error::Io(e)) if e.kind() == ErrorKind::ConnectionRefused => {
                // note(jbr): this is bad and represents a flaw in the current Connector api in that there's no
                // way to agnostically and asynchronously sleep
                std::thread::sleep(Duration::from_millis(10));
                log::debug!("Could not connect to {upstream} yet, sleeping 10ms");
            }

            Err(other) => {
                panic!("trillium-frontend was unable to connect to the dev server: {other}");
            }
        }
    }
}

#[doc(hidden)]
pub mod __macro_internals {
    pub use crate::FrontendHandler;
    pub use trillium_frontend_macros::frontend_impl;
    pub use trillium_static_compiled::static_compiled;
}

/// Build a [`FrontendHandler`] for your frontend project.
///
/// In debug builds: detects your framework and package manager, spawns the dev
/// server, and proxies all requests to it (including WebSocket HMR).
///
/// In release builds: runs the frontend build at compile time and embeds all
/// dist files directly in the binary.
///
/// # Usage
///
/// ```rust,ignore
/// // Simple: just the project path
/// trillium_frontend::frontend!("./client")
///     .with_client(Client::new(SmolConnector::default()))
///     .with_index_file("index.html")
///
/// // With explicit overrides
/// trillium_frontend::frontend!(
///     path = "./client",
///     build = "vite build",
///     dist = "dist",
/// )
/// ```
#[macro_export]
macro_rules! frontend {
    ($($tt:tt)*) => {{
        use $crate::__macro_internals::{FrontendHandler, static_compiled};
        #[cfg(debug_assertions)]
        { $crate::__macro_internals::frontend_impl!(debug $($tt)*) }
        #[cfg(not(debug_assertions))]
        { $crate::__macro_internals::frontend_impl!(release $($tt)*) }
    }};
}
