//! Command-line interface for preparing and inspecting Filecoin state snapshots.
mod recipe;

use std::fmt::{Display, Write as _};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use fvm_anvil_state::{
    archive::{
        ArchiveBuildOptions, ArchiveBuildProgress, ArchiveBuildStage, BuildSummary,
        build_prepared_archive_with_progress,
    },
    forest_car::ForestCar,
    prepared::{PreparedStateMetadata, load_prepared_metadata},
    snapshot::{SnapshotContext, inspect_filecoin_snapshot},
};

use crate::recipe::Recipe;

#[derive(Debug, Parser)]
#[command(name = "press", version, about = "Prepare and inspect Filecoin state snapshots")]
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
    /// Copy selected actors into a standalone prepared-state archive.
    Build {
        /// Source ForestCAR Filecoin snapshot.
        #[arg(long, value_name = "SNAPSHOT")]
        snapshot: PathBuf,
        /// Versioned actor-selection recipe.
        #[arg(long, value_name = "RECIPE")]
        recipe: PathBuf,
        /// Standalone ForestCAR archive to create.
        #[arg(long, value_name = "OUTPUT")]
        output: PathBuf,
        /// Filecoin network version for the imported execution context.
        #[arg(long, default_value_t = 28, value_parser = parse_network_version)]
        network_version: u32,
        /// Replace an existing output archive.
        #[arg(long)]
        force: bool,
        /// Read every reachable block back through the completed archive's index.
        #[arg(long)]
        verify: bool,
    },
}

fn parse_network_version(value: &str) -> Result<u32, String> {
    let version = value.parse::<u32>().map_err(|error| error.to_string())?;
    if !(21..=29).contains(&version) {
        return Err("network version must be between 21 and 29".into());
    }
    Ok(version)
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Inspect { snapshot } => inspect(&snapshot),
        Command::Build { snapshot, recipe, output, network_version, force, verify } => {
            build(&snapshot, &recipe, &output, network_version, force, verify)
        }
    }
}

fn inspect(snapshot: &Path) -> Result<()> {
    let store = ForestCar::open(snapshot)
        .with_context(|| format!("failed to open snapshot {}", snapshot.display()))?;
    let output = match load_prepared_metadata(&store, store.roots())
        .with_context(|| format!("failed to inspect prepared archive {}", snapshot.display()))?
    {
        Some((root, metadata)) => format_prepared_metadata(&root, &metadata),
        None => {
            let context = inspect_filecoin_snapshot(&store, store.roots()).with_context(|| {
                format!("failed to inspect Filecoin snapshot {}", snapshot.display())
            })?;
            format_snapshot_context(&context)
        }
    };
    std::io::stdout()
        .lock()
        .write_all(output.as_bytes())
        .context("failed to write snapshot inspection result")?;
    Ok(())
}

fn build(
    snapshot: &Path,
    recipe_path: &Path,
    output: &Path,
    network_version: u32,
    force: bool,
    verify: bool,
) -> Result<()> {
    if paths_refer_to_same_file(snapshot, output)? {
        bail!("source snapshot and output archive must be different files");
    }
    let source = ForestCar::open(snapshot)
        .with_context(|| format!("failed to open source snapshot {}", snapshot.display()))?;
    let context = inspect_filecoin_snapshot(&source, source.roots())
        .with_context(|| format!("failed to inspect source snapshot {}", snapshot.display()))?;
    let recipe = Recipe::load(recipe_path)?;
    let patches = recipe.patches()?;
    let started = Instant::now();
    let summary = build_prepared_archive_with_progress(
        &source,
        &context,
        &patches,
        output,
        ArchiveBuildOptions { network_version, overwrite: force, verify_output: verify },
        |progress| report_build_progress(progress, started.elapsed()),
    )
    .with_context(|| format!("failed to build prepared archive {}", output.display()))?;
    let result = format_build_summary(output, &summary);
    std::io::stdout().lock().write_all(result.as_bytes()).context("failed to write build result")
}

fn report_build_progress(progress: ArchiveBuildProgress, elapsed: Duration) {
    let status = match progress.stage {
        ArchiveBuildStage::Constructing => "constructing selected state".to_owned(),
        ArchiveBuildStage::Writing if progress.blocks == 0 => {
            "writing selected actor state".to_owned()
        }
        ArchiveBuildStage::Writing => format!(
            "wrote {} blocks ({:.1} MiB uncompressed)",
            progress.blocks,
            progress.bytes as f64 / (1024.0 * 1024.0)
        ),
        ArchiveBuildStage::Indexing => {
            format!("indexing {} written blocks", progress.blocks)
        }
        ArchiveBuildStage::Validating => "validating completed archive structure".to_owned(),
        ArchiveBuildStage::Verifying if progress.blocks == 0 => {
            "verifying completed archive".to_owned()
        }
        ArchiveBuildStage::Verifying => {
            format!("verified {} reachable blocks", progress.blocks)
        }
        ArchiveBuildStage::Publishing => "publishing completed archive".to_owned(),
    };
    let _ = writeln!(std::io::stderr().lock(), "press: [{:.1}s] {status}", elapsed.as_secs_f64());
}

fn paths_refer_to_same_file(source: &Path, output: &Path) -> Result<bool> {
    let source = source
        .canonicalize()
        .with_context(|| format!("failed to resolve source path {}", source.display()))?;
    let output = if output.exists() {
        output
            .canonicalize()
            .with_context(|| format!("failed to resolve output path {}", output.display()))?
    } else {
        let parent = output
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let parent = parent
            .canonicalize()
            .with_context(|| format!("failed to resolve output directory {}", parent.display()))?;
        let file_name = output.file_name().context("output archive path must name a file")?;
        parent.join(file_name)
    };
    if source == output {
        return Ok(true);
    }
    if !output.exists() {
        return Ok(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let source_metadata = source.metadata()?;
        let output_metadata = output.metadata()?;
        Ok(source_metadata.dev() == output_metadata.dev()
            && source_metadata.ino() == output_metadata.ino())
    }
    #[cfg(not(unix))]
    Ok(false)
}

fn format_build_summary(output: &Path, summary: &BuildSummary) -> String {
    format!(
        "Archive: {}\nMetadata root: {}\nPrepared state root: {}\nActors: {}\nBlocks: {}\nReachable DAG verified: {}\n",
        output.display(),
        summary.metadata_root,
        summary.state_root,
        summary.actors,
        summary.blocks,
        if summary.reachable_dag_verified { "yes" } else { "no" },
    )
}

fn format_prepared_metadata(root: &impl Display, metadata: &PreparedStateMetadata) -> String {
    let mut output = format!(
        "Press state metadata: {root}\nPrepared state root: {}\nSource head CIDs:\n",
        metadata.state_root,
    );
    for cid in &metadata.source_head_tipset {
        writeln!(output, "  {cid}").expect("writing to a String cannot fail");
    }
    writeln!(
        output,
        "Source parent state root: {}\nEpoch: {}\nTimestamp: {}\nBase fee (attoFIL): {}\nNetwork version: {}\nActor manifest data: {}",
        metadata.source_parent_state_root,
        metadata.epoch,
        metadata.timestamp,
        metadata.base_fee.atto(),
        metadata.network_version,
        metadata.actor_manifest_data,
    )
    .expect("writing to a String cannot fail");
    if !metadata.external_actor_codes.is_empty() {
        output.push_str("External actor code CIDs:\n");
        for cid in &metadata.external_actor_codes {
            writeln!(output, "  {cid}").expect("writing to a String cannot fail");
        }
    }
    output.push_str("Actors:\n");
    for actor in &metadata.actors {
        let requested = if actor.requested_addresses.is_empty() {
            String::new()
        } else {
            format!(" ({})", actor.requested_addresses.join(", "))
        };
        let delegated_address =
            actor.delegated_address.map_or_else(|| "-".into(), |address| address.to_string());
        writeln!(
            output,
            "  f0{}{} code={} state={} source_state={} sequence={} balance_atto={} delegated_address={}",
            actor.id,
            requested,
            actor.code,
            actor.state,
            actor.source_state,
            actor.sequence,
            actor.balance.atto(),
            delegated_address,
        )
        .expect("writing to a String cannot fail");
        if let Some(patch) = &actor.patch {
            if let Some(code) = &patch.runtime_code {
                writeln!(
                    output,
                    "    runtime_code: {} (0x{}) -> {} (0x{})",
                    code.source,
                    hex::encode(&code.source_hash),
                    code.replacement,
                    hex::encode(&code.replacement_hash),
                )
                .expect("writing to a String cannot fail");
            }
            for storage in &patch.storage {
                writeln!(
                    output,
                    "    storage 0x{}: 0x{} -> 0x{}",
                    hex::encode(&storage.key),
                    hex::encode(&storage.source),
                    hex::encode(&storage.replacement),
                )
                .expect("writing to a String cannot fail");
            }
            if let Some(balance) = &patch.balance {
                writeln!(
                    output,
                    "    balance_atto: {} -> {}",
                    balance.source.atto(),
                    balance.replacement.atto(),
                )
                .expect("writing to a String cannot fail");
            }
            if let Some(sequence) = patch.sequence {
                writeln!(output, "    sequence: {} -> {}", sequence.source, sequence.replacement,)
                    .expect("writing to a String cannot fail");
            }
        }
    }
    output
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

        match cli.command {
            Command::Inspect { snapshot } => {
                assert_eq!(snapshot.to_string_lossy(), "snapshot.car");
            }
            Command::Build { .. } => panic!("expected inspect command"),
        }
    }

    #[test]
    fn parses_standalone_build_options() {
        let cli = Cli::try_parse_from([
            "press",
            "build",
            "--snapshot",
            "source.car",
            "--recipe",
            "recipe.json",
            "--output",
            "prepared.car",
            "--network-version",
            "28",
            "--force",
            "--verify",
        ])
        .unwrap();
        let Command::Build { snapshot, recipe, output, network_version, force, verify } =
            cli.command
        else {
            panic!("expected build command");
        };
        assert_eq!(snapshot.to_string_lossy(), "source.car");
        assert_eq!(recipe.to_string_lossy(), "recipe.json");
        assert_eq!(output.to_string_lossy(), "prepared.car");
        assert_eq!(network_version, 28);
        assert!(force);
        assert!(verify);

        let cli = Cli::try_parse_from([
            "press",
            "build",
            "--snapshot",
            "source.car",
            "--recipe",
            "recipe.json",
            "--output",
            "prepared.car",
        ])
        .unwrap();
        let Command::Build { verify, .. } = cli.command else {
            panic!("expected build command");
        };
        assert!(!verify);
    }

    #[test]
    fn validates_archive_network_version_boundaries() {
        for version in ["21", "28", "29"] {
            assert_eq!(super::parse_network_version(version).unwrap().to_string(), version);
        }
        for version in ["20", "30", "-1", "4294967296"] {
            assert!(super::parse_network_version(version).is_err());
        }
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
