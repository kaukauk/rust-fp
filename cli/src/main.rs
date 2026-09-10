use async_std::{io::stdout, main};
use std::error::Error;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use clap::{Parser, Subcommand};
use postcard::from_bytes;
use rust_fp_common::{enroll_step_dbus_result::EnrollStepDbusOutput, rust_fp_dbus::RustFpProxy};
use zbus::export::futures_util::AsyncWriteExt;
use zbus::Connection;

use rust_fp::fingerprint_driver::{EnrollStepOutput, MatchOutput, MatchedOutput};
use rust_fp_common::get_templates::get_templates;
use rust_fp_common::set_templates::set_templates;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Get the maximum number of templates that the fingerprint sensor can have stored
    GetMaxTemplates,
    /// Print sensor and host-command details. Include this in bug reports.
    Info,
    Add {
        label: String,
    },
    List,
    /// Remove a fingerprint template
    Remove {
        label: String,
    },
    /// Remove all stored fingerprints for a user
    Clear,
    /// Test out the fingerprint sensor by matching a finger, and save the updated template if it was updated
    Match,
    /// Prints a template in binary to stdout
    DownloadTemplate {
        label: String,
    },
}

#[main]
async fn main() -> Result<(), Box<dyn Error>> {
    match Cli::parse().command {
        Commands::GetMaxTemplates => {
            let connection = Connection::system().await?;
            let proxy = RustFpProxy::new(&connection).await?;
            let max_templates = proxy.get_max_templates().await?;
            println!("Max templates: {max_templates}");
        }
        Commands::Info => {
            let connection = Connection::system().await?;
            let proxy = RustFpProxy::new(&connection).await?;
            print!("{}", proxy.get_info().await?);
        }
        Commands::Add { label } => {
            let mut templates = get_templates().await?;
            if templates.contains_key(&label) {
                Err("A fingerprint with that label already exists")?;
            }
            let connection = Connection::system().await?;
            let proxy = RustFpProxy::new(&connection).await?;

            // Enrolling owns the sensor, and nothing else — matching included —
            // can use it until the enrolment ends. Dying without a word leaves
            // the daemon to time the session out, so hand it back on the way
            // out instead.
            let cancelled = Arc::new(AtomicBool::new(false));
            {
                let cancelled = cancelled.clone();
                // A handler already being set is not worth failing over.
                let _ = ctrlc::set_handler(move || cancelled.store(true, Ordering::SeqCst));
            }

            let mut id = None;
            let mut last_percentage = None;
            println!("Touch the FP sensor");
            let template = loop {
                if cancelled.load(Ordering::SeqCst) {
                    if let Some(id) = id {
                        proxy.cancel_enroll(id).await?;
                    }
                    println!("Enroll cancelled");
                    return Ok(());
                }
                let output: EnrollStepDbusOutput =
                    from_bytes(&proxy.enroll_step(id.unwrap_or_default()).await?)?;
                id = Some(output.id);
                match output.result {
                    Err(error) => {
                        println!("{error:?}. Try again.");
                    }
                    // Nobody has touched the sensor yet. Steps are deliberately
                    // bounded so the daemon stays usable; just ask again.
                    Ok(EnrollStepOutput::TimedOut) => {}
                    Ok(EnrollStepOutput::InProgress(percentage)) => {
                        if last_percentage != Some(percentage) {
                            last_percentage = Some(percentage);
                            println!("Enroll progress: {percentage}%");
                            println!("Touch the FP sensor");
                        }
                    }
                    Ok(EnrollStepOutput::Complete(template)) => break template,
                }
            };
            println!("Enroll complete");
            templates.insert(label, template);
            set_templates(&templates).await?;
            println!("Saved template to file");
        }
        Commands::List => {
            let templates = get_templates().await?;
            println!("Fingerprints saved for this user:  {:#?}", templates.keys());
        }
        Commands::Remove { label } => {
            let mut templates = get_templates().await?;
            match templates.remove(&label) {
                Some(_removed_template) => {
                    set_templates(&templates).await?;
                    println!("Removed template {:#?}", label);
                    Ok(())
                }
                None => Err(format!(
                    "Template {:#?} doesn't exist. Existing templates: {:#?}",
                    label,
                    templates.keys().collect::<Vec<_>>()
                )),
            }?;
        }
        Commands::Clear => {
            set_templates(&Default::default()).await?;
            println!("Cleared templates");
        }
        Commands::Match => {
            let mut templates = get_templates().await?;
            if !templates.is_empty() {
                let connection = Connection::system().await?;
                let proxy = RustFpProxy::new(&connection).await?;
                let templates_vec = templates.iter().collect::<Vec<_>>();
                let payload = templates_vec
                    .iter()
                    .map::<Vec<u8>, _>(|(_k, v)| v.to_vec())
                    .collect::<Vec<_>>();
                println!("Ready to match...");
                let output: MatchOutput = loop {
                    let output: MatchOutput =
                        from_bytes(&proxy.match_templates(payload.clone()).await?)?;
                    // Matching is bounded for the same reason enrolling is.
                    if !matches!(output, MatchOutput::TimedOut) {
                        break output;
                    }
                };
                println!("Output: {output:?}");
                match output {
                    MatchOutput::Match(MatchedOutput {
                        index,
                        updated_template,
                    }) => {
                        let matched_label = templates_vec[index].0;
                        println!("Matched: {matched_label}.");
                        if let Some(updated_template) = updated_template {
                            println!("Template was updated. Saving updated template...");
                            let matched_label = matched_label.to_owned();
                            templates.insert(matched_label, updated_template);
                            set_templates(&templates).await?;
                            println!("Saved updated template");
                        }
                    }
                    MatchOutput::NoMatch(error) => {
                        println!("No match");
                        if let Some(error) = error {
                            println!("Error matching: {error:?}");
                        }
                    }
                    MatchOutput::TimedOut => unreachable!("looped until a decision"),
                }
            } else {
                println!("No templates saved. Not matching.");
            }
        }
        Commands::DownloadTemplate { label } => {
            let templates = get_templates().await?;
            match templates.get(&label) {
                Some(template) => {
                    stdout().write_all(template).await?;
                }
                None => {
                    println!("Template does not exist");
                }
            }
        }
    }
    Ok(())
}
