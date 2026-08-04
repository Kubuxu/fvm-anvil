//! Command-line interface for inspecting Filecoin state snapshots.
use std::fmt::Display;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use fvm_anvil_state::{
    forest_car::ForestCar,
    snapshot::{SnapshotContext, inspect_filecoin_snapshot},
};

#[derive(Debug, Parser)]
#[command(name = "press", version, about = "Inspect Filecoin state snapshots")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print the execution context at a snapshot's head tipset.
    Inspect {
        /// ForestCAR Filecoin snapshot to inspect.
        #[arg(value_name = "SNAPSHOT")]
        snapshot: PathBuf,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Inspect { snapshot } => inspect(&snapshot),
    }
}

fn inspect(snapshot: &Path) -> Result<()> {
    let store = ForestCar::open(snapshot)
        .with_context(|| format!("failed to open snapshot {}", snapshot.display()))?;
    let context = inspect_filecoin_snapshot(&store, store.roots())
        .with_context(|| format!("failed to inspect Filecoin snapshot {}", snapshot.display()))?;

    let output = format_snapshot_context(&context);
    std::io::stdout()
        .lock()
        .write_all(output.as_bytes())
        .context("failed to write snapshot inspection result")?;
    Ok(())
}

fn format_snapshot_context(context: &SnapshotContext) -> String {
    format_snapshot(
        &context.head_tipset,
        &context.state_root,
        context.epoch,
        context.timestamp,
        context.base_fee.atto(),
    )
}

fn format_snapshot(
    head_tipset: &[impl Display],
    state_root: &impl Display,
    epoch: i64,
    timestamp: u64,
    base_fee_atto: &impl Display,
) -> String {
    let mut output = String::from("Head CIDs:\n");
    for cid in head_tipset {
        output.push_str("  ");
        output.push_str(&cid.to_string());
        output.push('\n');
    }
    output.push_str(&format!(
        "Parent state root: {state_root}\nEpoch: {epoch}\nTimestamp: {timestamp}\nBase fee (attoFIL): {base_fee_atto}\n"
    ));
    output
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::{Cli, Command, format_snapshot};

    #[test]
    fn parses_inspect_snapshot() {
        let cli = Cli::try_parse_from(["press", "inspect", "snapshot.car"]).unwrap();

        let Command::Inspect { snapshot } = cli.command;
        assert_eq!(snapshot.to_string_lossy(), "snapshot.car");
    }

    #[test]
    fn formats_snapshot_context_stably() {
        let output = format_snapshot(
            &["bafy-head-one", "bafy-head-two"],
            &"bafy-parent-state",
            12_345,
            1_700_000_000,
            &"1000000000",
        );

        assert_eq!(
            output,
            concat!(
                "Head CIDs:\n",
                "  bafy-head-one\n",
                "  bafy-head-two\n",
                "Parent state root: bafy-parent-state\n",
                "Epoch: 12345\n",
                "Timestamp: 1700000000\n",
                "Base fee (attoFIL): 1000000000\n",
            ),
        );
    }
}
