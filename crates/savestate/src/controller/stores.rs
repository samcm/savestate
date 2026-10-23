//! Garbage collection of the object stores: every minute, per configured store,
//! the records of checkpoints that are gone and the files no remaining checkpoint
//! uses (see "Object stores" in `docs/design.md`).

use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
    time::Duration,
};

use kube::{Api, ResourceExt, api::ListParams};
use savestate_api::Checkpoint;
use savestate_objstore::Collected;

use super::Context;

const INTERVAL: Duration = Duration::from_secs(60);

pub(super) async fn run(ctx: Arc<Context>) {
    // Per store, the files each record names (records never change).
    let mut caches: HashMap<String, HashMap<String, Vec<String>>> = HashMap::new();
    loop {
        for store in ctx.config.stores.keys() {
            let cache = caches.entry(store.clone()).or_default();
            match collect(&ctx, store, cache).await {
                Ok(collected) => {
                    if collected != Collected::default() {
                        tracing::info!(store, ?collected, "store garbage collected");
                    }
                    metrics::counter!("savestate_store_gc_records_deleted_total", "store" => store.clone())
                        .increment(collected.records as u64);
                    metrics::counter!("savestate_store_gc_files_deleted_total", "store" => store.clone())
                        .increment(collected.files as u64);
                }
                Err(error) => {
                    tracing::warn!(
                        store,
                        error = format!("{error:#}"),
                        "store garbage collection failed"
                    );
                    metrics::counter!("savestate_store_gc_errors_total", "store" => store.clone())
                        .increment(1);
                }
            }
        }
        tokio::time::sleep(INTERVAL).await;
    }
}

async fn collect(
    ctx: &Context,
    store: &str,
    cache: &mut HashMap<String, Vec<String>>,
) -> anyhow::Result<Collected> {
    let remote = crate::stores::open(&ctx.client, &ctx.config, store).await?;
    let (client, namespaces) = (ctx.client.clone(), ctx.config.namespaces.clone());
    // Read fresh from the API server, after the store's records are listed.
    let live = move || async move {
        let mut uids = BTreeSet::new();
        for namespace in &namespaces {
            let checkpoints = Api::<Checkpoint>::namespaced(client.clone(), namespace)
                .list(&ListParams::default())
                .await?;
            uids.extend(checkpoints.iter().filter_map(ResourceExt::uid));
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(uids)
    };
    Ok(remote.collect(live, cache).await?)
}
