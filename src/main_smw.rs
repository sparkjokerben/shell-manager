//! `smw`: browser-facing bridge to the smd daemon.
//!
//! Serves the embedded web UI, a small JSON API for session management and a
//! WebSocket terminal per session, all on top of [`web::bridge`].

mod web;

use std::sync::Arc;

use shell_manager::protocol;
use web::bridge::{AttachRegistry, ControlClient};
use web::AppState;

const USAGE: &str = "\
usage: smw [options]
options:
  --bind <addr>       address to listen on (default: 127.0.0.1)
  --port <port>       TCP port to listen on (default: 8787)
  --token <token>     API token (default: $SMW_TOKEN; never pass secrets in
                      shell history on a shared machine)
  --instance <name>   instance namespace (overrides $SM_INSTANCE)
  --allow-insecure    allow a non-loopback bind without a token (dangerous)
  -h, --help          show this help
  -V, --version       show the version
environment:
  SMW_TOKEN           API token
  SM_INSTANCE         instance namespace for pipes, mutex and logs
                      (1-32 chars of A-Z a-z 0-9 - _; default: prod, or dev
                      when the executable runs under target/)";

const DEFAULT_BIND: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 8787;

/// Settings resolved from the command line and the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub bind: String,
    pub port: u16,
    pub token: Option<String>,
    pub instance: String,
    /// True when the listener only accepts loopback connections.
    pub loopback_only: bool,
}

/// Raw command-line values, before the instance namespace is applied.
#[derive(Debug, Default, PartialEq, Eq)]
struct Raw {
    bind: String,
    port: u16,
    token: Option<String>,
    instance: Option<String>,
    allow_insecure: bool,
    help: bool,
    version: bool,
}

/// Read the value that belongs to the option at `args[*i]`.
fn next_value(args: &[String], i: &mut usize, option: &str) -> Result<String, String> {
    *i += 1;
    args.get(*i)
        .cloned()
        .ok_or_else(|| format!("{} requires a value", option))
}

/// Parse the command line. Options may appear in any order.
fn parse(args: &[String]) -> Result<Raw, String> {
    let mut raw = Raw {
        bind: DEFAULT_BIND.to_string(),
        port: DEFAULT_PORT,
        ..Raw::default()
    };

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--bind" => raw.bind = next_value(args, &mut i, "--bind")?,
            "--port" => {
                let value = next_value(args, &mut i, "--port")?;
                raw.port = value
                    .parse()
                    .map_err(|_| format!("invalid --port: {}", value))?;
            }
            "--token" => {
                let value = next_value(args, &mut i, "--token")?;
                if value.is_empty() {
                    return Err("--token must not be empty (omit it to disable auth)".to_string());
                }
                raw.token = Some(value);
            }
            "--instance" => raw.instance = Some(next_value(args, &mut i, "--instance")?),
            "--allow-insecure" => raw.allow_insecure = true,
            "-h" | "--help" => raw.help = true,
            "-V" | "--version" => raw.version = true,
            other if other.starts_with('-') => {
                return Err(format!("unknown option: {}", other));
            }
            other => return Err(format!("unexpected argument: {}", other)),
        }
        i += 1;
    }

    if raw.bind.is_empty() {
        return Err("--bind requires a non-empty address".to_string());
    }
    Ok(raw)
}

/// Apply `--instance` before anything reads the process namespace.
///
/// A legal value is written to `SM_INSTANCE` so every later `protocol` call
/// (pipes, mutex, logs) uses the same namespace. An illegal value is reported
/// and ignored, falling through to the environment and executable path rules.
fn apply_instance(raw: Option<&str>) -> String {
    if let Some(value) = raw {
        match protocol::valid_instance(value) {
            Some(name) => std::env::set_var("SM_INSTANCE", &name),
            None => eprintln!(
                "warning: ignoring invalid --instance={:?} (expected 1-32 chars of [A-Za-z0-9_-])",
                value
            ),
        }
    }
    protocol::instance().to_string()
}

/// Token from `--token`, else from `SMW_TOKEN`; an empty value means "off".
///
/// The environment value is passed in so tests never mutate process-wide state.
fn resolve_token(raw: Option<String>, env: Option<String>) -> Option<String> {
    raw.or(env).filter(|token| !token.is_empty())
}

/// Record the startup settings in `%TEMP%\smw-<instance>.log`.
///
/// The token value itself is never written, only whether one is configured.
fn log_startup(config: &Config) {
    let token = if config.token.is_some() { "on" } else { "off" };
    shell_manager::log::write_to(
        &protocol::smw_log_file(),
        &format!(
            "smw start version={} instance={} bind={}:{} token={}",
            env!("CARGO_PKG_VERSION"),
            config.instance,
            config.bind,
            config.port,
            token
        ),
    );
}

/// Whether `bind` only accepts loopback connections.
///
/// A hostname other than `localhost` is treated as non-loopback: it needs a
/// token even if DNS currently resolves it to 127.0.0.1.
fn is_loopback_bind(bind: &str) -> bool {
    if bind.eq_ignore_ascii_case("localhost") {
        return true;
    }
    bind.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

/// Format a host and port, bracketing IPv6 literals.
fn listen_address(bind: &str, port: u16) -> String {
    if bind.contains(':') && !bind.starts_with('[') {
        format!("[{}]:{}", bind, port)
    } else {
        format!("{}:{}", bind, port)
    }
}

/// Bind the listener, print the effective address and serve until the process
/// is interrupted.
///
/// Ctrl-C terminates the process outright; the data-pipe handles close with it
/// and the hosted shell keeps running.
async fn serve(config: Config) -> anyhow::Result<()> {
    let address = listen_address(&config.bind, config.port);
    let listener = tokio::net::TcpListener::bind(&address)
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind {}: {}", address, e))?;
    let local = listener
        .local_addr()
        .map_err(|e| anyhow::anyhow!("failed to read the bound address: {}", e))?;

    let state = AppState {
        control: Arc::new(ControlClient::new()),
        registry: AttachRegistry::new(),
        token: config.token.clone().map(Arc::new),
        instance: config.instance.clone(),
        loopback_only: config.loopback_only,
    };
    let router = web::http::router(state);

    let token = if config.token.is_some() { "on" } else { "off" };
    // `local` is the real address, which matters when `--port 0` was used.
    println!(
        "smw {} instance={} listening on http://{} (token {})",
        env!("CARGO_PKG_VERSION"),
        config.instance,
        local,
        token
    );
    shell_manager::log::write_to(
        &protocol::smw_log_file(),
        &format!(
            "smw listening addr={} instance={} token={}",
            local, config.instance, token
        ),
    );

    axum::serve(listener, router)
        .await
        .map_err(|e| anyhow::anyhow!("server error: {}", e))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let raw = match parse(&args) {
        Ok(raw) => raw,
        Err(msg) => {
            eprintln!("{}", msg);
            std::process::exit(1);
        }
    };

    // Must happen before the first namespace-dependent protocol call.
    let instance = apply_instance(raw.instance.as_deref());

    if raw.help {
        println!("{}", USAGE);
        return;
    }
    if raw.version {
        println!("{} (instance {})", env!("CARGO_PKG_VERSION"), instance);
        return;
    }

    let loopback_only = is_loopback_bind(&raw.bind);
    let config = Config {
        bind: raw.bind,
        port: raw.port,
        token: resolve_token(raw.token, std::env::var("SMW_TOKEN").ok()),
        instance,
        loopback_only,
    };

    if !config.loopback_only && config.token.is_none() && !raw.allow_insecure {
        eprintln!(
            "refusing to listen on non-loopback {} without a token; \
             set --token or SMW_TOKEN, or pass --allow-insecure to override",
            config.bind
        );
        std::process::exit(1);
    }
    if config.token.is_none() {
        eprintln!(
            "warning: no token configured. Any local process or user that can reach \
             http://{} can control sessions; set SMW_TOKEN on shared machines.",
            listen_address(&config.bind, config.port)
        );
    }

    log_startup(&config);

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("failed to start the async runtime: {}", e);
            std::process::exit(1);
        }
    };

    if let Err(e) = runtime.block_on(serve(config)) {
        eprintln!("smw: {:#}", e);
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn defaults_are_local_only() {
        let raw = parse(&[]).unwrap();
        assert_eq!(raw.bind, "127.0.0.1");
        assert_eq!(raw.port, 8787);
        assert_eq!(raw.token, None);
        assert_eq!(raw.instance, None);
    }

    #[test]
    fn options_accept_values_in_any_order() {
        let raw = parse(&args(&[
            "--port",
            "9000",
            "--instance",
            "Staging",
            "--bind",
            "0.0.0.0",
            "--token",
            "secret",
        ]))
        .unwrap();
        assert_eq!(raw.bind, "0.0.0.0");
        assert_eq!(raw.port, 9000);
        assert_eq!(raw.token.as_deref(), Some("secret"));
        assert_eq!(raw.instance.as_deref(), Some("Staging"));
    }

    #[test]
    fn missing_values_are_rejected() {
        for option in ["--bind", "--port", "--token", "--instance"] {
            let err = parse(&args(&[option])).unwrap_err();
            assert!(err.contains("requires a value"), "{}", err);
        }
    }

    #[test]
    fn invalid_port_is_rejected() {
        assert!(parse(&args(&["--port", "nope"])).is_err());
        assert!(parse(&args(&["--port", "70000"])).is_err());
    }

    #[test]
    fn empty_token_is_rejected_instead_of_disabling_auth() {
        assert!(parse(&args(&["--token", ""])).is_err());
    }

    #[test]
    fn allow_insecure_is_parsed() {
        assert!(parse(&args(&["--allow-insecure"])).unwrap().allow_insecure);
    }

    #[test]
    fn loopback_binds_are_recognised() {
        for bind in ["127.0.0.1", "127.1.2.3", "::1", "localhost", "LOCALHOST"] {
            assert!(is_loopback_bind(bind), "{}", bind);
        }
        for bind in ["0.0.0.0", "::", "192.168.1.5", "example.com", ""] {
            assert!(!is_loopback_bind(bind), "{}", bind);
        }
    }

    #[test]
    fn ipv6_listen_addresses_are_bracketed() {
        assert_eq!(listen_address("127.0.0.1", 8787), "127.0.0.1:8787");
        assert_eq!(listen_address("::1", 8787), "[::1]:8787");
        assert_eq!(listen_address("[::1]", 8787), "[::1]:8787");
    }

    #[test]
    fn unknown_options_and_positionals_are_rejected() {
        assert!(parse(&args(&["--bogus"]))
            .unwrap_err()
            .contains("unknown option"));
        assert!(parse(&args(&["session"]))
            .unwrap_err()
            .contains("unexpected argument"));
    }

    #[test]
    fn help_and_version_flags_are_recognised() {
        assert!(parse(&args(&["-h"])).unwrap().help);
        assert!(parse(&args(&["--help"])).unwrap().help);
        assert!(parse(&args(&["-V"])).unwrap().version);
        assert!(parse(&args(&["--version"])).unwrap().version);
    }

    #[test]
    fn empty_token_means_disabled() {
        assert_eq!(resolve_token(None, None), None);
        assert_eq!(resolve_token(Some(String::new()), None), None);
        assert_eq!(resolve_token(Some(String::new()), Some("env".into())), None);
        assert_eq!(resolve_token(Some("abc".into()), None), Some("abc".into()));
    }

    #[test]
    fn token_falls_back_to_the_environment() {
        assert_eq!(
            resolve_token(None, Some("from-env".into())),
            Some("from-env".into())
        );
        assert_eq!(
            resolve_token(Some("explicit".into()), Some("from-env".into())),
            Some("explicit".into())
        );
        assert_eq!(resolve_token(None, Some(String::new())), None);
    }
}
