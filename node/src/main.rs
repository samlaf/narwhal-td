// Copyright(C) Facebook, Inc. and its affiliates.
use anyhow::{Context, Result};
use clap::{crate_name, crate_version, App, AppSettings, ArgMatches, SubCommand};
use config::Export;
use config::Import as _;
use config::ThresholdKeyPair;
use config::{Committee, KeyPair, Parameters, WorkerId};
use consensus::Consensus;
use env_logger::Env;
use primary::{Certificate, Primary};
use store::Store;
use tokio::sync::mpsc::{channel, Receiver};
use worker::Worker;

/// The default channel capacity.
pub const CHANNEL_CAPACITY: usize = 1_000;

#[tokio::main]
async fn main() -> Result<()> {
    let matches = App::new(crate_name!())
        .version(crate_version!())
        .about("A research implementation of Narwhal and Tusk.")
        .args_from_usage("-v... 'Sets the level of verbosity'")
        .subcommand(
            SubCommand::with_name("generate_keypair")
                .about("Print a fresh key pair to file")
                .args_from_usage("--filename=<FILE> 'The file where to print the new key pair'"),
        )
        .subcommand(
            SubCommand::with_name("generate_threshold_keypair")
                .about("Print fresh threshold keypair to file")
                .args_from_usage("--filename=<FILE> 'The file where to print the new threshold key shares'")
                .args_from_usage("--threshold=<INT> 'The threshold number st (threshold+1)/num_shares needed to decrypt'")
                .args_from_usage("--node_index=<INT> 'The index of the share to generate'")
                .args_from_usage("--seed=<INT> 'The seed number to generate the threshold keys (use same seed as generate_threshold_publickey)'")
        )
        .subcommand(
            SubCommand::with_name("generate_threshold_publickey")
                .about("Print seeded threshold public key to file")
                .args_from_usage("--filename=<FILE> 'The file where to print the new threshold key shares'")
                .args_from_usage("--threshold=<INT> 'The threshold number st (threshold+1)/num_shares needed to decrypt'")
                .args_from_usage("--seed=<INT> 'The seed number to generate the threshold keys (use same seed as generate_threshold_keypair)'")
        )
        .subcommand(
            SubCommand::with_name("run")
                .about("Run a node")
                .args_from_usage("--keypair=<FILE> 'The file containing the node keypair'")
                .args_from_usage("--committee=<FILE> 'The file containing committee information'")
                .args_from_usage("--parameters=[FILE] 'The file containing the node parameters'")
                .args_from_usage("--store=<PATH> 'The path where to create the data store'")
                .subcommand(SubCommand::with_name("primary").about("Run a single primary"))
                .subcommand(
                    SubCommand::with_name("worker")
                        .about("Run a single worker")
                        .args_from_usage("--id=<INT> 'The worker id'")
                        .args_from_usage("--threshold_keypair=<FILE> 'The file containing the node threshold keypair'")
                )
                .setting(AppSettings::SubcommandRequiredElseHelp),
        )
        .setting(AppSettings::SubcommandRequiredElseHelp)
        .get_matches();

    let log_level = match matches.occurrences_of("v") {
        0 => "error",
        1 => "warn",
        2 => "info",
        3 => "debug",
        _ => "trace",
    };
    let mut logger = env_logger::Builder::from_env(Env::default().default_filter_or(log_level));
    #[cfg(feature = "benchmark")]
    logger.format_timestamp_millis();
    logger.init();

    match matches.subcommand() {
        ("generate_keypair", Some(sub_matches)) => KeyPair::new()
            .export(sub_matches.value_of("filename").unwrap())
            .context("Failed to generate key pair")?,
        ("generate_threshold_keypair", Some(sub_matches)) => {
            let threshold = sub_matches
                .value_of("threshold")
                .unwrap()
                .parse::<usize>()
                .context("threshold must be an integer")?;
            let node_index = sub_matches
                .value_of("node_index")
                .unwrap()
                .parse::<usize>()
                .context("node_index must be an integer")?;
            let seed = sub_matches
                .value_of("seed")
                .unwrap()
                .parse::<u64>()
                .context("seed must be an integer")?;
            ThresholdKeyPair::new(threshold, node_index, seed)
                .export(sub_matches.value_of("filename").unwrap())
                .context("Failed to generate threshold keypair")?;
        }
        ("generate_threshold_publickey", Some(sub_matches)) => {
            let threshold = sub_matches
                .value_of("threshold")
                .unwrap()
                .parse::<usize>()
                .context("threshold must be an integer")?;
            let seed = sub_matches
                .value_of("seed")
                .unwrap()
                .parse::<u64>()
                .context("seed must be an integer")?;
            // reusing this structure just to generate the public key is an ugly hack but this is a hackathon..
            ThresholdKeyPair::new(threshold, 0, seed)
                .pk_set
                .public_key()
                .export(sub_matches.value_of("filename").unwrap())
                .context("Failed to generate threshold public key")?;
        }
        ("run", Some(sub_matches)) => run(sub_matches).await?,
        _ => unreachable!(),
    }
    Ok(())
}

// Runs either a worker or a primary.
async fn run(matches: &ArgMatches<'_>) -> Result<()> {
    let key_file = matches.value_of("keypair").unwrap();
    let committee_file = matches.value_of("committee").unwrap();
    let parameters_file = matches.value_of("parameters");
    let store_path = matches.value_of("store").unwrap();

    // Read the committee and node's keypair and threshold keypair from file.
    let keypair = KeyPair::import(key_file).context("Failed to load the node's keypair")?;
    let committee =
        Committee::import(committee_file).context("Failed to load the committee information")?;

    // Load default parameters if none are specified.
    let parameters = match parameters_file {
        Some(filename) => {
            Parameters::import(filename).context("Failed to load the node's parameters")?
        }
        None => Parameters::default(),
    };

    // Make the data store.
    let store = Store::new(store_path).context("Failed to create a store")?;

    // Channels the sequence of certificates.
    let (tx_output, rx_output) = channel(CHANNEL_CAPACITY);

    // Check whether to run a primary, a worker, or an entire authority.
    match matches.subcommand() {
        // Spawn the primary and consensus core.
        ("primary", _) => {
            let (tx_new_certificates, rx_new_certificates) = channel(CHANNEL_CAPACITY);
            let (tx_feedback, rx_feedback) = channel(CHANNEL_CAPACITY);
            Primary::spawn(
                keypair,
                committee.clone(),
                parameters.clone(),
                store,
                /* tx_consensus */ tx_new_certificates,
                /* rx_consensus */ rx_feedback,
            );
            Consensus::spawn(
                committee,
                parameters.gc_depth,
                /* rx_primary */ rx_new_certificates,
                /* tx_primary */ tx_feedback,
                tx_output,
            );
        }

        // Spawn a single worker.
        ("worker", Some(sub_matches)) => {
            let id = sub_matches
                .value_of("id")
                .unwrap()
                .parse::<WorkerId>()
                .context("The worker id must be a positive integer")?;
            let threshold_key_file = sub_matches.value_of("threshold_keypair").unwrap();
            let threshold_keypair = ThresholdKeyPair::import(threshold_key_file)
                .context("Failed to load the node's threshold keypair")?;
            Worker::spawn(
                keypair.name,
                id,
                threshold_keypair,
                committee,
                parameters,
                store,
            );
        }
        _ => unreachable!(),
    }

    // Analyze the consensus' output.
    analyze(rx_output).await;

    // If this expression is reached, the program ends and all other tasks terminate.
    unreachable!();
}

/// Receives an ordered list of certificates and apply any application-specific logic.
async fn analyze(mut rx_output: Receiver<Certificate>) {
    while let Some(_certificate) = rx_output.recv().await {
        // NOTE: Here goes the application logic.
    }
}
