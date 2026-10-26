use shell_daemon::daemon::Daemon;

fn main() -> anyhow::Result<()> {
    ctrlc::set_handler(move || {
        println!("\nshell-daemon shutting down...");
        std::process::exit(0);
    })
    .expect("error setting Ctrl-C handler");

    let mut daemon = Daemon::new()?;
    println!("shell-daemon started. Press Ctrl-C to stop.");
    daemon.run()?;
    Ok(())
}
