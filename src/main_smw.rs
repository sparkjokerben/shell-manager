//! `smw`: browser-facing bridge to the smd daemon.
//!
//! This stage only parses the command line and reports the resolved settings.
//! The HTTP server (and its auth layer) are added in a later stage on top of
//! [`web::bridge`].

mod web;

use shell_manager::protocol;

const USAGE: &str = "\
usage: smw [options]
options:
  --bind <addr>       address to listen on (default: 127.0.0.1)
  --port <port>       TCP port to listen on (default: 8787)
  --token <token>     API token (default: $SMW_TOKEN)
  --instance <name>   instance namespace (overrides $SM_INSTANCE)
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
}

/// Raw command-line values, before the instance namespace is applied.
#[derive(Debug, Default, PartialEq, Eq)]
struct Raw {
    bind: String,
    port: u16,
    token: Option<String>,
    instance: Option<String>,
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
            "--token" => raw.token = Some(next_value(args, &mut i, "--token")?),
            "--instance" => raw.instance = Some(next_value(args, &mut i, "--instance")?),
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
fn resolve_token(raw: Option<String>) -> Option<String> {
    raw.or_else(|| std::env::var("SMW_TOKEN").ok())
        .filter(|token| !token.is_empty())
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

    let config = Config {
        bind: raw.bind,
        port: raw.port,
        token: resolve_token(raw.token),
        instance,
    };

    // The HTTP server is wired up in a later stage; for now report the resolved
    // settings so the CLI can be exercised on its own.
    println!(
        "smw {} instance={} bind={}:{} token={}",
        env!("CARGO_PKG_VERSION"),
        config.instance,
        config.bind,
        config.port,
        if config.token.is_some() { "on" } else { "off" }
    );
    log_startup(&config);
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
        assert_eq!(resolve_token(None), None);
        assert_eq!(resolve_token(Some(String::new())), None);
        assert_eq!(resolve_token(Some("abc".into())), Some("abc".into()));
    }

    #[test]
    fn token_falls_back_to_the_environment() {
        // `std::env::set_var` is process-wide; use a value no other test reads.
        std::env::set_var("SMW_TOKEN", "from-env");
        assert_eq!(resolve_token(None), Some("from-env".into()));
        assert_eq!(
            resolve_token(Some("explicit".into())),
            Some("explicit".into())
        );
        std::env::remove_var("SMW_TOKEN");
    }
}
