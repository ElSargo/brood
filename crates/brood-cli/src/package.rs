//! Public CLI adapter for the package installation contract.
use anyhow::Result;
use brood_package_manifest::installation::{self, Plan};
use clap::{Parser, Subcommand};
use std::{fs, path::PathBuf};

#[derive(Debug, Parser)]
pub struct PackageArgs {
    /// Explicit local installation store; this does not grant a remote endpoint authority.
    #[arg(long, default_value_os_t = crate::package_runtime::default_store())]
    pub store: PathBuf,
    #[command(subcommand)]
    pub command: PackageCommand,
}

#[derive(Debug, Subcommand)]
pub enum PackageCommand {
    /// Resume an interrupted, previously authorized graph publication.
    Recover,
    /// Query the common introspection envelope using a versioned JSON request.
    Query {
        request: String,
        #[arg(long = "dbus-destination")]
        destination: Option<String>,
        #[arg(long = "dbus-address")]
        address: Option<String>,
    },
    /// Inspect durable state compatibility independently of plugin code.
    MigrationStatus { package: String },
    /// Attest that manual migration completed after quiescing affected instances.
    MigrationComplete {
        package: String,
        #[arg(long)]
        installation: String,
        #[arg(long)]
        previous_state: String,
        #[arg(long)]
        evidence: String,
    },
    /// Export verified artifacts and knowledge for the authorized artifact exchange.
    Export {
        package: String,
        #[arg(long)]
        installation: Option<String>,
        #[arg(long)]
        output: PathBuf,
    },
    /// Request teardown while retaining the installation and mutable state.
    Disable { package: String },
    /// Restore activation declarations for a disabled installation.
    Enable { package: String },
    /// Deselect the installation, retaining immutable artifacts and mutable state.
    Remove { package: String },
    /// Query host-owned live package facts without executing plugin diagnostics.
    Instances {
        #[arg(long = "dbus-destination")]
        destination: Option<String>,
        #[arg(long = "dbus-address")]
        address: Option<String>,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u32).range(1..=100))]
        limit: u32,
    },
    /// Resolve a manifest into a portable plan containing exact input bytes.
    Plan {
        manifest: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    /// Build and install the exact inputs recorded in a plan.
    Apply { plan: PathBuf },
    /// Verify and describe the selected local installation.
    Inspect {
        package: String,
        #[arg(long)]
        installation: Option<String>,
    },
    /// Retrieve verified installed documentation and interface schemas without running code.
    Knowledge {
        package: String,
        #[arg(long)]
        installation: Option<String>,
    },
}

pub fn run(args: PackageArgs) -> Result<()> {
    match args.command {
        PackageCommand::Recover => println!(
            "{}",
            serde_json::to_string_pretty(&installation::recover(&args.store)?)?
        ),
        PackageCommand::Query {
            request,
            destination,
            address,
        } => {
            if destination.is_some() || address.is_some() {
                let target = crate::control::resolve_control_target(destination, address);
                let output = crate::control::request_internal_command(
                    &target,
                    &["package-query".into(), request],
                    false,
                )?;
                println!("{output}");
            } else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&crate::introspection::offline_query(
                        &args.store,
                        request.as_bytes()
                    ))?
                );
            }
        }

        PackageCommand::MigrationStatus { package } => println!(
            "{}",
            serde_json::to_string_pretty(&installation::migration_status(&args.store, &package)?)?
        ),
        PackageCommand::MigrationComplete {
            package,
            installation: revision,
            previous_state,
            evidence,
        } => println!(
            "{}",
            serde_json::to_string_pretty(&installation::complete_migration(
                &args.store,
                &package,
                &revision,
                &previous_state,
                &evidence
            )?)?
        ),
        PackageCommand::Export {
            package,
            installation: revision,
            output,
        } => {
            installation::export_bundle(&args.store, &package, revision.as_deref(), &output)?;
            println!(
                "{}",
                serde_json::json!({"exported": output, "activation": "not-requested"})
            );
        }
        PackageCommand::Disable { package } => println!(
            "{}",
            serde_json::to_string_pretty(&installation::change_selection(
                &args.store,
                &package,
                installation::SelectionAction::Disable
            )?)?
        ),
        PackageCommand::Enable { package } => println!(
            "{}",
            serde_json::to_string_pretty(&installation::change_selection(
                &args.store,
                &package,
                installation::SelectionAction::Enable
            )?)?
        ),
        PackageCommand::Remove { package } => println!(
            "{}",
            serde_json::to_string_pretty(&installation::change_selection(
                &args.store,
                &package,
                installation::SelectionAction::Remove
            )?)?
        ),
        PackageCommand::Instances {
            destination,
            address,
            offset,
            limit,
        } => {
            let target = crate::control::resolve_control_target(destination, address);
            let output = crate::control::request_internal_command(
                &target,
                &[
                    "package-instances".into(),
                    offset.to_string(),
                    limit.to_string(),
                ],
                false,
            )?;
            println!("{output}");
        }
        PackageCommand::Plan { manifest, output } => {
            use std::io::Write;
            let plan = installation::plan(&manifest, &args.store)?;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(output)?;
            file.write_all(&serde_json::to_vec_pretty(&plan)?)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&installation::describe_plan(&plan)?)?
            );
        }
        PackageCommand::Apply { plan } => {
            let plan: Plan = serde_json::from_slice(&fs::read(plan)?)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&installation::apply(&plan, &args.store)?)?
            );
        }
        PackageCommand::Knowledge {
            package,
            installation: revision,
        } => {
            let knowledge = match revision {
                Some(revision) => {
                    installation::knowledge_revision(&args.store, &package, &revision)?
                }
                None => installation::knowledge(&args.store, &package)?,
            };
            println!("{}", serde_json::to_string_pretty(&knowledge)?);
        }
        PackageCommand::Inspect {
            package,
            installation: revision,
        } => {
            let installed = match revision {
                Some(revision) => installation::inspect_revision(&args.store, &package, &revision)?,
                None => installation::inspect(&args.store, &package)?,
            };
            let status =
                installation::revision_status(&args.store, &package, &installed.plan_digest)?;
            let mut description = serde_json::to_value(installed)?;
            if let Some(object) = description.as_object_mut() {
                object.insert("selection_status".into(), serde_json::json!(status));
            }
            println!("{}", serde_json::to_string_pretty(&description)?);
        }
    }
    Ok(())
}
