// Copyright (C) 2023, Ava Labs, Inc. All rights reserved.
// See the file LICENSE.md for licensing terms.

// The idea behind this benchmark:
// Phase 1: (setup) Generate known keys from the SHA256 of the row number, starting at 0, for 1B keys
// Phase 2: (steady-state) Continuously insert, delete, and update keys in the database

// Phase 2 consists of:
// 1. 25% of batch size is inserting more rows like phase 1
// 2. 25% of batch size is deleting rows from the beginning
// 3. 50% of batch size is updating rows in the middle, but setting the value to the hash of the first row inserted
//

use clap::Parser;
use firewood::logger::{debug, trace};
use metrics_exporter_prometheus::PrometheusBuilder;
use metrics_util::MetricKindMask;
use pretty_duration::pretty_duration;
use sha2::{Digest, Sha256};
use std::error::Error;
use std::net::{Ipv4Addr, SocketAddr};
use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

use firewood::db::{BatchOp, Db, DbConfig};
use firewood::manager::RevisionManagerConfig;
use firewood::v2::api::{Db as _, Proposal as _};

#[derive(Parser, Debug)]
struct Args {
    #[arg(short, long, default_value_t = 10000)]
    batch_size: u64,
    #[arg(short, long, default_value_t = 100000)]
    number_of_batches: u64,
    #[arg(short = 'p', long, default_value_t = 0, value_parser = clap::value_parser!(u16).range(0..=100))]
    read_verify_percent: u16,
    #[arg(
        short,
        long,
        help = "Only initialize the database, do not do the insert/delete/update loop"
    )]
    initialize_only: bool,
    #[arg(short, long, default_value_t = NonZeroUsize::new(1500000).expect("is non-zero"))]
    cache_size: NonZeroUsize,
    #[arg(short, long)]
    assume_preloaded_rows: Option<u64>,
    #[arg(short, long, default_value_t = 128)]
    revisions: usize,
    #[arg(short = 'l', long, default_value_t = 3000)]
    prometheus_port: u16,
    #[arg(short, long)]
    test_name: Option<String>,
}

#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();

    env_logger::init();

    let builder = PrometheusBuilder::new();
    builder
        .with_http_listener(SocketAddr::new(
            Ipv4Addr::UNSPECIFIED.into(),
            args.prometheus_port,
        ))
        .idle_timeout(
            MetricKindMask::COUNTER | MetricKindMask::HISTOGRAM,
            Some(Duration::from_secs(10)),
        )
        .install()
        .expect("unable in run prometheusbuilder");

    let mgrcfg = RevisionManagerConfig::builder()
        .node_cache_size(args.cache_size)
        .free_list_cache_size(
            NonZeroUsize::new(2 * args.batch_size as usize).expect("batch size > 0"),
        )
        .max_revisions(args.revisions)
        .build();
    let cfg = DbConfig::builder()
        .truncate(args.assume_preloaded_rows.is_none())
        .manager(mgrcfg)
        .build();

    let db = Db::new("rev_db", cfg)
        .await
        .expect("db initiation should succeed");

    let keys = args.batch_size;
    let start = Instant::now();

    if args.assume_preloaded_rows.is_none() {
        for key in 0..args.number_of_batches {
            let batch = generate_inserts(key * keys, args.batch_size).collect();

            let proposal = db.propose(batch).await.expect("proposal should succeed");
            proposal.commit().await?;
        }

        let duration = start.elapsed();
        println!(
            "Generated and inserted {} batches of size {keys} in {}",
            args.number_of_batches,
            pretty_duration(&duration, None)
        );
    }

    let current_hash = db.root_hash().await?.expect("root hash should exist");

    if args.initialize_only {
        println!("Completed initialization with hash of {:?}", current_hash);

        return Ok(());
    }

    let all_hashes = db.all_hashes().await?;
    println!(
        "Database has {} hashes (oldest {:?})",
        all_hashes.len(),
        all_hashes.first().expect("one hash must exist")
    );

    // batches consist of:
    // 1. 25% deletes from low
    // 2. 25% new insertions from high
    // 3. 50% updates from the middle

    println!(
        "Starting inner loop with database hash of {:?}",
        current_hash
    );

    let mut low = 0;
    let mut high = args
        .assume_preloaded_rows
        .unwrap_or(args.number_of_batches * args.batch_size);
    let twenty_five_pct = args.batch_size / 4;

    loop {
        let batch: Vec<BatchOp<_, _>> = generate_inserts(high, twenty_five_pct)
            .chain(generate_deletes(low, twenty_five_pct))
            .chain(generate_updates(low + high / 2, twenty_five_pct * 2, low))
            .collect();
        let proposal = db.propose(batch).await.expect("proposal should succeed");
        proposal.commit().await?;
        low += twenty_five_pct;
        high += twenty_five_pct;
    }
}

fn generate_inserts(start: u64, count: u64) -> impl Iterator<Item = BatchOp<Vec<u8>, Vec<u8>>> {
    (start..start + count)
        .map(|inner_key| {
            let digest = Sha256::digest(inner_key.to_ne_bytes()).to_vec();
            trace!(
                "inserting {:?} with digest {}",
                inner_key,
                hex::encode(&digest),
            );
            (digest.clone(), digest)
        })
        .map(|(key, value)| BatchOp::Put { key, value })
        .collect::<Vec<_>>()
        .into_iter()
}

fn generate_deletes(start: u64, count: u64) -> impl Iterator<Item = BatchOp<Vec<u8>, Vec<u8>>> {
    (start..start + count)
        .map(|key| {
            let digest = Sha256::digest(key.to_ne_bytes()).to_vec();
            debug!("deleting {:?} with digest {}", key, hex::encode(&digest));
            #[allow(clippy::let_and_return)]
            digest
        })
        .map(|key| BatchOp::Delete { key })
        .collect::<Vec<_>>()
        .into_iter()
}

fn generate_updates(
    start: u64,
    count: u64,
    low: u64,
) -> impl Iterator<Item = BatchOp<Vec<u8>, Vec<u8>>> {
    let hash_of_low = Sha256::digest(low.to_ne_bytes()).to_vec();
    (start..start + count)
        .map(|inner_key| {
            let digest = Sha256::digest(inner_key.to_ne_bytes()).to_vec();
            debug!(
                "updating {:?} with digest {} to {}",
                inner_key,
                hex::encode(&digest),
                hex::encode(&hash_of_low)
            );
            (digest, hash_of_low.clone())
        })
        .map(|(key, value)| BatchOp::Put { key, value })
        .collect::<Vec<_>>()
        .into_iter()
}
