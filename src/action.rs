//! Running the actions configured in the YAML file.

use crate::config::Command as CmdSpec;
use anyhow::{Context, Result};
use std::process::{Command, Stdio};

/// Start a command without waiting for it to finish — the deck stays
/// responsive even while the launched program runs on.
pub fn run(spec: &CmdSpec) -> Result<()> {
    let mut cmd = match spec {
        CmdSpec::Shell(line) => {
            let mut c = Command::new("sh");
            c.arg("-c").arg(line);
            c
        }
        CmdSpec::Argv(argv) => {
            let Some((program, args)) = argv.split_first() else {
                anyhow::bail!("empty command list");
            };
            let mut c = Command::new(program);
            c.args(args);
            c
        }
    };

    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("could not start {spec:?}"))?;

    // Reap the child so no zombies pile up.
    let pid = child.id();
    std::thread::spawn(move || {
        let mut child = child;
        match child.wait() {
            Ok(status) if !status.success() => {
                log::warn!("command (pid {pid}) exited with {status}");
            }
            Err(e) => log::warn!("wait on pid {pid} failed: {e}"),
            _ => {}
        }
    });
    Ok(())
}
