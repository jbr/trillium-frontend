#![forbid(unsafe_code)]
use proc_macro::{TokenStream, TokenTree};
use quote::quote;
use std::path::{Path, PathBuf};

/// Internal proc macro invoked by the `frontend!` macro_rules wrapper.
/// Do not call directly; use `trillium_frontend::frontend!` instead.
#[proc_macro]
pub fn frontend_impl(input: TokenStream) -> TokenStream {
    // First token is `debug` or `release`, injected by the macro_rules! wrapper
    // via #[cfg(debug_assertions)] so we don't have to rely on CARGO_CFG_* env vars.
    let mut tokens = input.into_iter().peekable();
    let is_debug = match tokens.next() {
        Some(TokenTree::Ident(id)) if id.to_string() == "debug" => true,
        Some(TokenTree::Ident(id)) if id.to_string() == "release" => false,
        other => {
            panic!("trillium-frontend: expected `debug` or `release` as first token, got {other:?}")
        }
    };
    let rest: TokenStream = tokens.collect();
    let args = parse_args(rest);

    let manifest_dir =
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set by cargo");
    let project_path = resolve_path(&args.path, &manifest_dir);
    let project_path_str = project_path
        .to_str()
        .expect("project path is not valid UTF-8");

    let detection = detect(&project_path);

    let dev_command_tokens = match detection.full_dev_command() {
        Some(cmd) => quote!(Some(#cmd)),
        None => quote!(None),
    };
    if is_debug {
        quote! {
            FrontendHandler::new(
                None,
                None,
                #project_path_str,
                #dev_command_tokens,
            )
        }
    } else {
        // Run the frontend build
        let detected_build = detection.full_build_command();
        let build_command = args
            .build
            .as_deref()
            .or(detected_build.as_deref())
            .expect("trillium-frontend: could not detect a build command; specify build = \"...\" in the macro")
            .to_string();

        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(&build_command)
            .current_dir(&project_path)
            .status()
            .unwrap_or_else(|e| panic!("trillium-frontend: failed to run build `{build_command}`: {e}"));

        if !status.success() {
            panic!("trillium-frontend: build command `{build_command}` failed with {status}");
        }

        let dist_dir = project_path.join(
            args.dist
                .as_deref()
                .or(detection.dist.as_deref())
                .unwrap_or("dist"),
        );

        let dist_str = dist_dir
            .to_str()
            .expect("dist path is not valid UTF-8");

        // Check for a dist/index.html to use as SPA fallback
        let index_html = dist_dir.join("index.html");
        let spa_fallback_tokens = if index_html.exists() {
            let index_str = index_html
                .to_str()
                .expect("index.html path is not valid UTF-8");
            quote! {
                Some(static_compiled!(#index_str))
            }
        } else {
            quote!(None)
        };

        quote! {
            FrontendHandler::new(
                Some(static_compiled!(#dist_str)),
                #spa_fallback_tokens,
                #project_path_str,
                #dev_command_tokens,
            )
        }
    }
    .into()
}

// ── Argument parsing ──────────────────────────────────────────────────────────

#[derive(Default)]
struct MacroArgs {
    path: String,
    build: Option<String>,
    dist: Option<String>,
}

fn parse_args(input: TokenStream) -> MacroArgs {
    let tokens: Vec<_> = input.into_iter().collect();
    match tokens.as_slice() {
        // frontend!("./client")
        [TokenTree::Literal(lit)] => MacroArgs {
            path: unwrap_string_literal(lit),
            ..Default::default()
        },
        // frontend!(path = "...", build = "...", dist = "...")
        _ => parse_key_value_args(&tokens),
    }
}

fn parse_key_value_args(tokens: &[TokenTree]) -> MacroArgs {
    let mut args = MacroArgs::default();
    let mut i = 0;
    while i < tokens.len() {
        let key = match &tokens[i] {
            TokenTree::Ident(id) => id.to_string(),
            TokenTree::Punct(p) if p.as_char() == ',' => {
                i += 1;
                continue;
            }
            other => panic!("trillium-frontend: expected key identifier, got {other}"),
        };
        i += 1;
        // `=`
        match &tokens[i] {
            TokenTree::Punct(p) if p.as_char() == '=' => i += 1,
            other => panic!("trillium-frontend: expected `=` after `{key}`, got {other}"),
        }
        let value = match &tokens[i] {
            TokenTree::Literal(lit) => unwrap_string_literal(lit),
            other => panic!("trillium-frontend: expected string literal for `{key}`, got {other}"),
        };
        i += 1;
        match key.as_str() {
            "path" => args.path = value,
            "build" => args.build = Some(value),
            "dist" => args.dist = Some(value),
            other => {
                panic!("trillium-frontend: unknown key `{other}`; valid keys: path, build, dist")
            }
        }
    }
    if args.path.is_empty() {
        panic!("trillium-frontend: `path` is required");
    }
    args
}

fn unwrap_string_literal(lit: &proc_macro::Literal) -> String {
    let repr = lit.to_string();
    if repr.starts_with('"') && repr.ends_with('"') {
        repr[1..repr.len() - 1].to_string()
    } else {
        panic!("trillium-frontend: expected a string literal, got {repr}")
    }
}

fn resolve_path(raw: &str, manifest_dir: &str) -> PathBuf {
    let p = PathBuf::from(raw);
    let base = if p.is_relative() {
        PathBuf::from(manifest_dir).join(p)
    } else {
        p
    };
    base.canonicalize()
        .unwrap_or_else(|e| panic!("trillium-frontend: could not resolve path `{raw}`: {e}"))
}

// ── Detection ─────────────────────────────────────────────────────────────────

struct Detection {
    pkg_manager: Option<PkgManager>,
    framework: Option<Framework>,
    /// dist dir name (relative to project root)
    pub dist: Option<String>,
}

impl Detection {
    /// `bun run vite`, `npx vite`, etc.
    fn full_dev_command(&self) -> Option<String> {
        let fw = self.framework.as_ref()?;
        let prefix = self
            .pkg_manager
            .as_ref()
            .map(PkgManager::run_prefix)
            .unwrap_or("npx");
        Some(format!("{prefix} {}", fw.dev_command()))
    }

    fn full_build_command(&self) -> Option<String> {
        let fw = self.framework.as_ref()?;
        let prefix = self
            .pkg_manager
            .as_ref()
            .map(PkgManager::run_prefix)
            .unwrap_or("npx");
        Some(format!("{prefix} {}", fw.build_command()))
    }
}

#[derive(Clone, Copy)]
enum PkgManager {
    Bun,
    Pnpm,
    Yarn,
    Npm,
}

impl PkgManager {
    fn run_prefix(&self) -> &'static str {
        match self {
            PkgManager::Bun => "bun run",
            PkgManager::Pnpm => "pnpm run",
            PkgManager::Yarn => "yarn run",
            PkgManager::Npm => "npx",
        }
    }
}

#[derive(Clone, Copy)]
enum Framework {
    Vite,
    Webpack,
    Next,
}

impl Framework {
    fn dev_command(self) -> &'static str {
        match self {
            Framework::Vite => "vite --strictPort --clearScreen false",
            Framework::Webpack => "webpack serve",
            Framework::Next => "next dev",
        }
    }

    fn build_command(self) -> &'static str {
        match self {
            Framework::Vite => "vite build",
            Framework::Webpack => "webpack build",
            Framework::Next => "next build",
        }
    }

    fn dist_dir(self) -> &'static str {
        match self {
            Framework::Vite | Framework::Webpack => "dist",
            Framework::Next => ".next",
        }
    }
}

fn detect(project_path: &Path) -> Detection {
    let pkg_manager = detect_pkg_manager(project_path);
    let framework = detect_framework(project_path);

    Detection {
        dist: framework.map(|f| f.dist_dir().to_string()),
        pkg_manager,
        framework,
    }
}

fn detect_pkg_manager(path: &Path) -> Option<PkgManager> {
    let candidates = [
        ("bun.lockb", PkgManager::Bun),
        ("bun.lock", PkgManager::Bun),
        ("pnpm-lock.yaml", PkgManager::Pnpm),
        ("yarn.lock", PkgManager::Yarn),
        ("package-lock.json", PkgManager::Npm),
    ];
    candidates
        .iter()
        .find(|(f, _)| path.join(f).exists())
        .map(|(_, pm)| *pm)
}

fn detect_framework(path: &Path) -> Option<Framework> {
    // Check exact names and glob-like prefixes
    let vite_configs = ["vite.config.js", "vite.config.ts", "vite.config.mjs"];
    let webpack_configs = [
        "webpack.config.js",
        "webpack.config.ts",
        "webpack.config.mjs",
    ];
    let next_configs = ["next.config.js", "next.config.ts", "next.config.mjs"];

    if vite_configs.iter().any(|f| path.join(f).exists()) {
        return Some(Framework::Vite);
    }
    if webpack_configs.iter().any(|f| path.join(f).exists()) {
        return Some(Framework::Webpack);
    }
    if next_configs.iter().any(|f| path.join(f).exists()) {
        return Some(Framework::Next);
    }
    None
}
