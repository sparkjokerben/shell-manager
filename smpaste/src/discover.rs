//! Discover interactive `ssh` sessions owned by this user.
//!
//! `smpaste` uses this module when it is started without `--ssh-host`: it
//! scans the process list for SSH clients attached to a terminal, parses
//! their destination and the connection options worth carrying over, and
//! rebuilds a matching command line for the clipboard stream.

// The parser is cross-platform and exercised by tests, but the release build
// only uses it on Unix, where `scan` is implemented.
#![cfg_attr(not(unix), allow(dead_code))]

use std::path::Path;

use anyhow::{bail, Result};

#[cfg(unix)]
use anyhow::Context;
#[cfg(unix)]
use std::process::{Command, Stdio};

/// One host to forward clipboard images to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SshTarget {
    /// The destination as written on the original command line: `user@host`
    /// or an alias from `~/.ssh/config`.
    pub destination: String,
    /// Connection options inherited from the original command line, already
    /// expanded to one argv element per option and value.
    pub carried: Vec<String>,
}

impl SshTarget {
    /// Unique key for this destination plus its inherited options.
    pub fn key(&self) -> String {
        let mut key = self.destination.clone();
        for arg in &self.carried {
            key.push('\u{0}');
            key.push_str(arg);
        }
        key
    }

    /// Human-readable label used in log lines.
    pub fn label(&self) -> &str {
        &self.destination
    }
}

/// Options that take a value (OpenSSH client short options).
const VALUE_OPTIONS: &[char] = &[
    'B', 'b', 'c', 'D', 'E', 'e', 'F', 'I', 'i', 'J', 'L', 'l', 'm', 'O', 'o', 'p', 'Q', 'R',
    'S', 'W', 'w',
];

/// Options that make a session unusable for forwarding: no TTY, no remote
/// command, backgrounding, port forwarding, or control/master operations.
const REJECTED_OPTIONS: &[char] = &['T', 'N', 'W', 'D', 'n', 'f', 'G', 'O', 'Q', 'V', 's'];

/// Options that are safe to carry over to the clipboard stream. `-o` is
/// handled separately because it needs filtering.
const CARRIED_OPTIONS: &[char] = &['F', 'i', 'J', 'l', 'p', 'S'];

/// `-o` keywords that must not be inherited: they would either conflict with
/// the forwarding stream or change its behaviour.
const SKIPPED_O_KEYWORDS: &[&str] = &[
    "LocalForward",
    "RemoteForward",
    "DynamicForward",
    "RemoteCommand",
    "BatchMode",
    "ServerAliveInterval",
    "ServerAliveCountMax",
    "Compression",
    // A tty would corrupt the binary frame stream.
    "RequestTTY",
    // Forwarding configured through -o would either bind busy ports or need a
    // display that a background process does not have.
    "ForwardAgent",
    "ForwardX11",
    "ForwardX11Trusted",
    // LocalCommand runs on every new connection.
    "LocalCommand",
    "PermitLocalCommand",
];

/// Parse one process command line as an interactive `ssh` invocation.
///
/// Returns `None` for anything that is not a plain interactive SSH client:
/// other programs, sessions started with a remote command (git, scp, VS Code
/// Remote, smpaste itself), and non-interactive or forwarding options.
pub(crate) fn parse_ssh_argv(argv: &[String]) -> Option<SshTarget> {
    let name = Path::new(argv.first()?).file_name()?.to_str()?;
    if name != "ssh" {
        return None;
    }

    let mut carried: Vec<String> = Vec::new();
    let mut destination: Option<String> = None;
    let mut index = 1;

    while index < argv.len() {
        let arg = &argv[index];

        if arg == "--" {
            index += 1;
            destination = Some(argv.get(index)?.clone());
            index += 1;
            if index != argv.len() {
                // Anything after the destination is a remote command.
                return None;
            }
            break;
        }

        let Some(rest) = arg.strip_prefix('-') else {
            destination = Some(arg.clone());
            index += 1;
            if index != argv.len() {
                return None;
            }
            break;
        };

        if rest.is_empty() {
            // A lone `-` would be the destination, which is rejected below.
            return None;
        }

        let mut position = 0;
        while position < rest.len() {
            let option = rest[position..].chars().next()?;
            position += option.len_utf8();

            if REJECTED_OPTIONS.contains(&option) {
                return None;
            }

            if VALUE_OPTIONS.contains(&option) {
                let value = if position < rest.len() {
                    // Glued form, e.g. `-p2222` or `-oProxyJump=bastion`.
                    rest[position..].to_string()
                } else {
                    index += 1;
                    argv.get(index)?.clone()
                };
                if option == 'o' {
                    // Without `=` the option cannot be classified reliably.
                    if !value.contains('=') {
                        return None;
                    }
                    let keyword = value.split_once('=')?.0;
                    if !SKIPPED_O_KEYWORDS
                        .iter()
                        .any(|skip| keyword.trim().eq_ignore_ascii_case(skip))
                    {
                        carried.push("-o".to_string());
                        carried.push(value);
                    }
                } else if CARRIED_OPTIONS.contains(&option) {
                    carried.push(format!("-{option}"));
                    carried.push(value);
                }
                break;
            }
        }
        index += 1;
    }

    let destination = destination?;
    if destination.is_empty() || destination.starts_with('_') || destination.starts_with('-') {
        return None;
    }

    Some(SshTarget {
        destination,
        carried,
    })
}

/// Parse `ps -ww -eo pid=,command=` output.
pub(crate) fn parse_ps_output(output: &str) -> Vec<SshTarget> {
    let mut targets: Vec<SshTarget> = Vec::new();
    for line in output.lines() {
        let line = line.trim_start();
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split_whitespace();
        if fields.next().is_none() {
            continue;
        }
        let argv: Vec<String> = fields.map(|field| field.to_string()).collect();
        let Some(target) = parse_ssh_argv(&argv) else {
            continue;
        };
        if !targets.iter().any(|known| known.key() == target.key()) {
            targets.push(target);
        }
    }
    targets.sort_by(|a, b| a.key().cmp(&b.key()));
    targets
}

/// List interactive SSH sessions owned by this user.
#[cfg(unix)]
pub(crate) fn scan() -> Result<Vec<SshTarget>> {
    // `-x` selects this user's processes on both macOS and Linux, so another
    // account's ssh sessions are never considered.
    let output = Command::new("ps")
        .arg("-ww")
        .arg("-x")
        .arg("-o")
        .arg("pid=,command=")
        .stdin(Stdio::null())
        .output()
        .context("failed to run ps to discover SSH sessions")?;
    if !output.status.success() {
        bail!("ps exited with {}", output.status);
    }
    Ok(parse_ps_output(&String::from_utf8_lossy(&output.stdout)))
}

/// Host discovery is not implemented outside Unix.
#[cfg(not(unix))]
pub(crate) fn scan() -> Result<Vec<SshTarget>> {
    bail!("automatic SSH host discovery is not available on this platform")
}

/// Build the argv for the persistent `ssh` process of one target.
pub(crate) fn ssh_command_args(target: &SshTarget, remote_sm: &str) -> Vec<String> {
    let mut args = vec![
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "ServerAliveInterval=15".to_string(),
        "-o".to_string(),
        "ServerAliveCountMax=3".to_string(),
        "-C".to_string(),
        "-T".to_string(),
    ];
    args.extend(target.carried.iter().cloned());
    args.push(target.destination.clone());
    args.push(remote_sm.to_string());
    args.push("clipboard-image".to_string());
    args.push("--stream".to_string());
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    fn parsed(items: &[&str]) -> Option<SshTarget> {
        parse_ssh_argv(&argv(items))
    }

    #[test]
    fn parses_bare_host() {
        let target = parsed(&["ssh", "host"]).expect("interactive ssh");
        assert_eq!(target.destination, "host");
        assert!(target.carried.is_empty());
        assert_eq!(target.label(), "host");
    }

    #[test]
    fn carries_connection_options_in_order() {
        let target = parsed(&[
            "ssh", "-p", "2222", "-i", "key", "-F", "cfg", "-J", "bastion", "-l", "user", "-S",
            "sock", "user@host",
        ])
        .expect("interactive ssh");
        assert_eq!(target.destination, "user@host");
        assert_eq!(
            target.carried,
            argv(&[
                "-p", "2222", "-i", "key", "-F", "cfg", "-J", "bastion", "-l", "user", "-S",
                "sock",
            ])
        );
    }

    #[test]
    fn parses_glued_option_values() {
        let target = parsed(&["ssh", "-4p2222", "host"]).expect("interactive ssh");
        assert_eq!(target.destination, "host");
        assert_eq!(target.carried, argv(&["-p", "2222"]));

        let target = parsed(&["ssh", "-oProxyJump=bastion", "host"]).expect("interactive ssh");
        assert_eq!(target.destination, "host");
        assert_eq!(target.carried, argv(&["-o", "ProxyJump=bastion"]));
    }

    #[test]
    fn rejects_remote_commands() {
        for command in [
            &["ssh", "host", "tmux", "attach"][..],
            &["ssh", "host", "sm", "clipboard-image", "--stream"][..],
            &["ssh", "git@github.com", "git-upload-pack", "/x"][..],
        ] {
            assert_eq!(parsed(command), None, "{command:?}");
        }
    }

    #[test]
    fn rejects_non_interactive_options() {
        for command in [
            &["ssh", "-T", "host"][..],
            &["ssh", "-N", "host"][..],
            &["ssh", "-W", "host:22", "host"][..],
            &["ssh", "-D", "1080", "host"][..],
            &["ssh", "-n", "host"][..],
        ] {
            assert_eq!(parsed(command), None, "{command:?}");
        }
    }

    #[test]
    fn allows_local_forwards_without_carrying_them() {
        let target = parsed(&["ssh", "-L", "8080:localhost:80", "host"]).expect("interactive ssh");
        assert_eq!(target.destination, "host");
        assert!(target.carried.is_empty());
    }

    #[test]
    fn filters_conflicting_o_options() {
        for option in [
            "LocalForward=8080:localhost:80",
            "RemoteForward=9090:localhost:90",
            "DynamicForward=1080",
            "RemoteCommand=tmux attach",
            "BatchMode=no",
            "ServerAliveInterval=60",
            "ServerAliveCountMax=9",
            "Compression=no",
            "batchmode=no",
            "RequestTTY=force",
            "ForwardAgent=yes",
            "ForwardX11=yes",
            "LocalCommand=echo hi",
        ] {
            let target = parsed(&["ssh", "-o", option, "host"]).expect("interactive ssh");
            assert!(target.carried.is_empty(), "{option}");
        }

        let target = parsed(&["ssh", "-o", "ProxyJump=bastion", "host"]).expect("interactive ssh");
        assert_eq!(target.carried, argv(&["-o", "ProxyJump=bastion"]));
    }

    #[test]
    fn rejects_o_option_without_equals() {
        assert_eq!(parsed(&["ssh", "-o", "BatchMode", "host"]), None);
        assert_eq!(parsed(&["ssh", "-oBatchMode", "host"]), None);
    }

    #[test]
    fn supports_double_dash_terminator() {
        let target = parsed(&["ssh", "--", "user@host"]).expect("interactive ssh");
        assert_eq!(target.destination, "user@host");
        assert!(target.carried.is_empty());

        assert_eq!(parsed(&["ssh", "--", "host", "echo"]), None);
        assert_eq!(parsed(&["ssh", "--"]), None);
    }

    #[test]
    fn rejects_missing_option_values() {
        assert_eq!(parsed(&["ssh", "-p"]), None);
        assert_eq!(parsed(&["ssh", "-o"]), None);
    }

    #[test]
    fn rejects_other_programs() {
        for command in [
            &["sshd"][..],
            &["ssh-agent", "-s"][..],
            &["mosh", "server"][..],
            &["/usr/sbin/sshd", "-D"][..],
            &["/usr/bin/sm", "start"][..],
        ] {
            assert_eq!(parsed(command), None, "{command:?}");
        }
        assert!(parsed(&["/usr/bin/ssh", "host"]).is_some());
    }

    #[test]
    fn rejects_bad_destinations() {
        assert_eq!(parsed(&["ssh", ""]), None);
        assert_eq!(parsed(&["ssh", "_hidden"]), None);
        assert_eq!(parsed(&["ssh", "-"]), None);
    }

    #[test]
    fn parses_ps_output_and_dedupes() {
        let output = "\
  101 /usr/bin/ssh host
  102 ssh -p 2222 user@host
  103 ssh host
  104 ssh git@github.com git-upload-pack '/x'
  105 ssh -T -D 1080 vscode-remote
  106 ssh -T host sm clipboard-image --stream
  107 /usr/sbin/sshd -D
  108 /usr/bin/ssh-agent -s
  109 mosh server
";
        let targets = parse_ps_output(output);
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].destination, "host");
        assert!(targets[0].carried.is_empty());
        assert_eq!(targets[1].destination, "user@host");
        assert_eq!(targets[1].carried, argv(&["-p", "2222"]));
    }

    #[test]
    fn ignores_empty_ps_lines() {
        assert!(parse_ps_output("\n   \n 9\n").is_empty());
    }

    #[test]
    fn builds_ssh_arguments() {
        let target = SshTarget {
            destination: "user@host".to_string(),
            carried: argv(&["-p", "2222", "-o", "ProxyJump=bastion"]),
        };
        let args = ssh_command_args(&target, "sm");
        assert_eq!(
            args,
            argv(&[
                "-o",
                "BatchMode=yes",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                "-C",
                "-T",
                "-p",
                "2222",
                "-o",
                "ProxyJump=bastion",
                "user@host",
                "sm",
                "clipboard-image",
                "--stream",
            ])
        );

        let batch_mode = args
            .iter()
            .position(|arg| arg == "BatchMode=yes")
            .expect("BatchMode option");
        let carried = args
            .iter()
            .position(|arg| arg == "2222")
            .expect("carried option");
        assert!(batch_mode < carried);
        assert_eq!(
            args[args.len() - 4..],
            argv(&["user@host", "sm", "clipboard-image", "--stream"])
        );
        for forbidden in ["-L", "-R", "-D", "-t", "-v"] {
            assert!(!args.iter().any(|arg| arg == forbidden), "{forbidden}");
        }
    }

    #[test]
    fn forwarded_options_are_not_inherited() {
        let target = parsed(&["ssh", "-L", "8080:localhost:80", "-t", "-v", "host"])
            .expect("interactive ssh");
        assert!(target.carried.is_empty());
        let args = ssh_command_args(&target, "sm");
        for forbidden in ["-L", "-R", "-D", "-t", "-v"] {
            assert!(!args.iter().any(|arg| arg == forbidden), "{forbidden}");
        }
    }

    /// The `ps` invocation itself must work on every Unix platform. The result
    /// depends on the machine, so only the shape is checked.
    #[cfg(unix)]
    #[test]
    fn scan_reads_the_real_process_list() {
        let targets = scan().expect("ps should succeed");
        assert!(targets.iter().all(|target| !target.destination.is_empty()));
    }
}
