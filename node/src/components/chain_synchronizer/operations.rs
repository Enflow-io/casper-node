use std::{
    cmp,
    collections::BTreeMap,
    sync::{
        atomic::{self, AtomicBool},
        Arc,
    },
};

use async_trait::async_trait;
use datasize::DataSize;
use futures::{
    stream::{futures_unordered::FuturesUnordered, StreamExt},
    TryStreamExt,
};
use prometheus::IntGauge;
use quanta::Instant;
use tracing::{info, trace, warn};

use casper_execution_engine::storage::trie::{TrieOrChunk, TrieOrChunkId};
use casper_hashing::Digest;
use casper_types::{bytesrepr::Bytes, EraId, ProtocolVersion, PublicKey, U512};

use super::{metrics::Metrics, Config};
use crate::{
    components::{
        chain_synchronizer::error::{Error, FetchTrieError},
        consensus::{self},
        contract_runtime::ExecutionPreState,
        fetcher::{FetchResult, FetchedData, FetcherError},
    },
    effect::{requests::FetcherRequest, EffectBuilder},
    reactor::joiner::JoinerEvent,
    types::{
        Block, BlockHash, BlockHeader, BlockHeaderWithMetadata, BlockSignatures, BlockWithMetadata,
        Deploy, DeployHash, FinalizedBlock, Item, TimeDiff, Timestamp,
    },
    utils::work_queue::WorkQueue,
};

/// Helper struct that is used to measure a time spent in the scope.
/// At the construction time, a reference to the gauge is provided. When the binding to `ScopeTimer`
/// is dropped, the specified gauge is updated with the duration since the scope was entered.
// In the future, this could be replaced with a mechanism that uses `tracing::instrument` attribute
// macro.
struct ScopeTimer<'a> {
    start: Instant,
    gauge: &'a IntGauge,
}

impl<'a> ScopeTimer<'a> {
    fn new(gauge: &'a IntGauge) -> Self {
        Self {
            start: Instant::now(),
            gauge,
        }
    }
}

impl<'a> Drop for ScopeTimer<'a> {
    fn drop(&mut self) {
        self.gauge
            .set(Instant::now().duration_since(self.start).as_secs() as i64);
    }
}

struct ChainSyncContext<'a> {
    effect_builder: &'a EffectBuilder<JoinerEvent>,
    config: &'a Config,
    trusted_block_header: &'a BlockHeader,
    metrics: &'a Metrics,
}

impl<'a> ChainSyncContext<'a> {
    fn new(
        effect_builder: &'a EffectBuilder<JoinerEvent>,
        config: &'a Config,
        trusted_block_header: &'a BlockHeader,
        metrics: &'a Metrics,
    ) -> Self {
        Self {
            effect_builder,
            config,
            trusted_block_header,
            metrics,
        }
    }

    fn trusted_hash(&self) -> BlockHash {
        self.trusted_block_header
            .hash(self.config.verifiable_chunked_hash_activation())
    }
}

/// Restrict the fan-out for a trie being retrieved by chunks to query at most 10 peers at a time.
const TRIE_CHUNK_FETCH_FAN_OUT: usize = 10;

/// Fetches an item. Keeps retrying to fetch until it is successful. Not suited to fetching a block
/// header or block by height, which require verification with finality signatures.
async fn fetch_retry_forever<T>(
    effect_builder: EffectBuilder<JoinerEvent>,
    config: &Config,
    id: T::Id,
) -> FetchResult<T>
where
    T: Item + 'static,
    JoinerEvent: From<FetcherRequest<T>>,
{
    loop {
        for peer in effect_builder.get_fully_connected_peers().await {
            trace!(
                "attempting to fetch {:?} with id {:?} from {:?}",
                T::TAG,
                id,
                peer
            );
            match effect_builder.fetch::<T>(id, peer).await {
                Ok(fetched_data @ FetchedData::FromStorage { .. }) => {
                    trace!(
                        "did not get {:?} with id {:?} from {:?}, got from storage instead",
                        T::TAG,
                        id,
                        peer
                    );
                    return Ok(fetched_data);
                }
                Ok(fetched_data @ FetchedData::FromPeer { .. }) => {
                    trace!("fetched {:?} with id {:?} from {:?}", T::TAG, id, peer);
                    return Ok(fetched_data);
                }
                Err(FetcherError::Absent { .. }) => {
                    warn!(
                        ?id,
                        tag = ?T::TAG,
                        ?peer,
                        "chain sync could not fetch; trying next peer",
                    )
                }
                Err(FetcherError::TimedOut { .. }) => {
                    warn!(
                        ?id,
                        tag = ?T::TAG,
                        ?peer,
                        "peer timed out",
                    );
                }
                Err(error @ FetcherError::CouldNotConstructGetRequest { .. }) => return Err(error),
            }
        }
        tokio::time::sleep(config.retry_interval()).await
    }
}

enum TrieAlreadyPresentOrDownloaded {
    AlreadyPresent,
    Downloaded(Bytes),
}

async fn fetch_trie_retry_forever(
    id: Digest,
    ctx: &ChainSyncContext<'_>,
) -> Result<TrieAlreadyPresentOrDownloaded, FetchTrieError> {
    let trie_or_chunk = match fetch_retry_forever::<TrieOrChunk>(
        *ctx.effect_builder,
        ctx.config,
        TrieOrChunkId(0, id),
    )
    .await?
    {
        FetchedData::FromStorage { .. } => {
            return Ok(TrieAlreadyPresentOrDownloaded::AlreadyPresent)
        }
        FetchedData::FromPeer {
            item: trie_or_chunk,
            ..
        } => *trie_or_chunk,
    };
    let chunk_with_proof = match trie_or_chunk {
        TrieOrChunk::Trie(trie) => return Ok(TrieAlreadyPresentOrDownloaded::Downloaded(trie)),
        TrieOrChunk::ChunkWithProof(chunk_with_proof) => chunk_with_proof,
    };

    debug_assert!(
        chunk_with_proof.proof().index() == 0,
        "Proof index was not 0.  Proof index: {}",
        chunk_with_proof.proof().index()
    );
    let count = chunk_with_proof.proof().count();
    let first_chunk = chunk_with_proof.into_chunk();
    // Start stream iter to get each chunk.
    // Start from 1 because proof.index() == 0.
    // Build a map of the chunks.
    let chunk_map_result = futures::stream::iter(1..count)
        .map(|index| async move {
            match fetch_retry_forever::<TrieOrChunk>(
                *ctx.effect_builder,
                ctx.config,
                TrieOrChunkId(index, id),
            )
            .await?
            {
                FetchedData::FromStorage { .. } => {
                    Err(FetchTrieError::TrieBeingFetchByChunksSomehowFetchedFromStorage)
                }
                FetchedData::FromPeer { item, .. } => match *item {
                    TrieOrChunk::Trie(_) => Err(
                        FetchTrieError::TrieBeingFetchedByChunksSomehowFetchWholeFromPeer {
                            digest: id,
                        },
                    ),
                    TrieOrChunk::ChunkWithProof(chunk_with_proof) => {
                        let index = chunk_with_proof.proof().index();
                        let chunk = chunk_with_proof.into_chunk();
                        Ok((index, chunk))
                    }
                },
            }
        })
        // Do not try to fetch all of the trie chunks at once; only fetch at most
        // TRIE_CHUNK_FETCH_FAN_OUT at a time.
        // Doing `buffer_unordered` followed by `try_collect` here means if one of the
        // fetches fails, then the outstanding futures are canceled.
        .buffer_unordered(TRIE_CHUNK_FETCH_FAN_OUT)
        .try_collect::<BTreeMap<u64, Bytes>>()
        .await;

    // Handle if a parallel process downloaded all the trie chunks before us.
    let mut chunk_map = match chunk_map_result {
        Ok(chunk_map) => chunk_map,
        Err(FetchTrieError::TrieBeingFetchByChunksSomehowFetchedFromStorage) => {
            // trie must have been downloaded by a parallel process...
            return Ok(TrieAlreadyPresentOrDownloaded::AlreadyPresent);
        }
        Err(error) => {
            return Err(error);
        }
    };
    chunk_map.insert(0, first_chunk);

    // Concatenate all of the chunks into a trie
    let trie_bytes = chunk_map.into_values().flat_map(Vec::<u8>::from).collect();
    Ok(TrieAlreadyPresentOrDownloaded::Downloaded(trie_bytes))
}

/// Fetches and stores a block header from the network.
async fn fetch_and_store_block_header(
    effect_builder: EffectBuilder<JoinerEvent>,
    config: &Config,
    block_hash: BlockHash,
) -> Result<Box<BlockHeader>, Error> {
    // Only genesis should have this as previous hash, so no block should ever have it...
    if block_hash == BlockHash::default() {
        return Err(Error::NoSuchBlockHash {
            bogus_block_hash: block_hash,
        });
    }
    let fetched_block_header =
        fetch_retry_forever::<BlockHeader>(effect_builder, config, block_hash).await?;
    match fetched_block_header {
        FetchedData::FromStorage { item: block_header } => Ok(block_header),
        FetchedData::FromPeer {
            item: block_header, ..
        } => {
            effect_builder
                .put_block_header_to_storage(block_header.clone())
                .await;
            Ok(block_header)
        }
    }
}

/// Fetches and stores a deploy.
async fn fetch_and_store_deploy(
    deploy_or_transfer_hash: DeployHash,
    ctx: &ChainSyncContext<'_>,
) -> Result<Box<Deploy>, FetcherError<Deploy>> {
    let fetched_deploy =
        fetch_retry_forever::<Deploy>(*ctx.effect_builder, ctx.config, deploy_or_transfer_hash)
            .await?;
    match fetched_deploy {
        FetchedData::FromStorage { item: deploy } => Ok(deploy),
        FetchedData::FromPeer { item: deploy, .. } => {
            ctx.effect_builder
                .put_deploy_to_storage(deploy.clone())
                .await;
            Ok(deploy)
        }
    }
}

/// Key block info used for verifying finality signatures.
///
/// Can come from either:
///   - A switch block
///   - The global state under a genesis block
///
/// If the data was scraped from genesis, then `era_id` is 0.
/// Otherwise if it came from a switch block it is that switch block's `era_id + 1`.
#[derive(DataSize, Clone, Debug)]
pub(crate) struct KeyBlockInfo {
    /// The block hash of the key block
    key_block_hash: BlockHash,
    /// The validator weights for checking finality signatures.
    validator_weights: BTreeMap<PublicKey, U512>,
    /// The time the era started.
    era_start: Timestamp,
    /// The height of the switch block that started this era.
    height: u64,
    /// The era in which the validators are operating
    era_id: EraId,
}

impl KeyBlockInfo {
    pub(crate) fn maybe_from_block_header(
        block_header: &BlockHeader,
        verifiable_chunked_hash_activation: EraId,
    ) -> Option<KeyBlockInfo> {
        block_header
            .next_era_validator_weights()
            .and_then(|next_era_validator_weights| {
                Some(KeyBlockInfo {
                    key_block_hash: block_header.hash(verifiable_chunked_hash_activation),
                    validator_weights: next_era_validator_weights.clone(),
                    era_start: block_header.timestamp(),
                    height: block_header.height(),
                    era_id: block_header.era_id().checked_add(1)?,
                })
            })
    }

    /// Returns the era in which the validators are operating
    pub(crate) fn era_id(&self) -> EraId {
        self.era_id
    }

    /// Returns the hash of the key block, i.e. the last block before `era_id`.
    pub(crate) fn block_hash(&self) -> &BlockHash {
        &self.key_block_hash
    }

    /// Returns the validator weights for this era.
    pub(crate) fn validator_weights(&self) -> &BTreeMap<PublicKey, U512> {
        &self.validator_weights
    }
}

#[async_trait]
trait BlockOrHeaderWithMetadata: Item<Id = u64> + 'static {
    fn header(&self) -> &BlockHeader;

    fn finality_signatures(&self) -> &BlockSignatures;

    async fn store_block_or_header(&self, effect_builder: EffectBuilder<JoinerEvent>);
}

#[async_trait]
impl BlockOrHeaderWithMetadata for BlockWithMetadata {
    fn header(&self) -> &BlockHeader {
        self.block.header()
    }

    fn finality_signatures(&self) -> &BlockSignatures {
        &self.finality_signatures
    }

    async fn store_block_or_header(&self, effect_builder: EffectBuilder<JoinerEvent>) {
        let block = Box::new(self.block.clone());
        effect_builder.put_block_to_storage(block).await;
    }
}

#[async_trait]
impl BlockOrHeaderWithMetadata for BlockHeaderWithMetadata {
    fn header(&self) -> &BlockHeader {
        &self.block_header
    }

    fn finality_signatures(&self) -> &BlockSignatures {
        &self.block_signatures
    }

    async fn store_block_or_header(&self, effect_builder: EffectBuilder<JoinerEvent>) {
        let header = Box::new(self.block_header.clone());
        effect_builder.put_block_header_to_storage(header).await;
    }
}

/// Fetches the next block or block header from the network by height.
async fn fetch_and_store_next<I>(
    parent_header: &BlockHeader,
    trusted_key_block_info: &KeyBlockInfo,
    ctx: &ChainSyncContext<'_>,
) -> Result<Option<Box<I>>, Error>
where
    I: BlockOrHeaderWithMetadata,
    JoinerEvent: From<FetcherRequest<I>>,
    Error: From<FetcherError<I>>,
{
    let height = parent_header
        .height()
        .checked_add(1)
        .ok_or_else(|| Error::HeightOverflow {
            parent: Box::new(parent_header.clone()),
        })?;

    // Ensure we have at least one peer to which we're bidirectionally connected before trying to
    // fetch the data.
    let mut peers = vec![];
    for _ in 0..ctx.config.max_retries_while_not_connected() {
        peers = ctx.effect_builder.get_fully_connected_peers().await;
        if !peers.is_empty() {
            break;
        }
        tokio::time::sleep(ctx.config.retry_interval()).await;
    }

    let item = loop {
        let peer = match peers.pop() {
            Some(peer) => peer,
            None => return Ok(None),
        };
        match ctx.effect_builder.fetch::<I>(height, peer).await {
            Ok(FetchedData::FromStorage { item }) => {
                if *item.header().parent_hash()
                    != parent_header.hash(ctx.config.verifiable_chunked_hash_activation())
                {
                    return Err(Error::UnexpectedParentHash {
                        parent: Box::new(parent_header.clone()),
                        child: Box::new(item.header().clone()),
                    });
                }
                break item;
            }
            Ok(FetchedData::FromPeer { item, .. }) => {
                if *item.header().parent_hash()
                    != parent_header.hash(ctx.config.verifiable_chunked_hash_activation())
                {
                    warn!(
                        ?peer,
                        fetched_header = ?item.header(),
                        ?parent_header,
                        "received block with wrong parent from peer",
                    );
                    ctx.effect_builder.announce_disconnect_from_peer(peer).await;
                    continue;
                }

                if let Err(error) = consensus::check_sufficient_finality_signatures(
                    trusted_key_block_info.validator_weights(),
                    ctx.config.finality_threshold_fraction(),
                    item.finality_signatures(),
                ) {
                    warn!(?error, ?peer, "insufficient finality signatures from peer");
                    ctx.effect_builder.announce_disconnect_from_peer(peer).await;
                    continue;
                }

                if let Err(error) = item.finality_signatures().verify() {
                    warn!(
                        ?error,
                        ?peer,
                        "error validating finality signatures from peer"
                    );
                    ctx.effect_builder.announce_disconnect_from_peer(peer).await;
                    continue;
                }

                // Store the block or header itself, and the finality signatures.
                item.store_block_or_header(*ctx.effect_builder).await;
                let sigs = item.finality_signatures().clone();
                ctx.effect_builder.put_signatures_to_storage(sigs).await;

                break item;
            }
            Err(FetcherError::Absent { .. }) => {
                warn!(height, tag = ?I::TAG, ?peer, "block by height absent from peer");
                // If the peer we requested doesn't have the item, continue with the next peer
                continue;
            }
            Err(FetcherError::TimedOut { .. }) => {
                warn!(height, tag = ?I::TAG, ?peer, "peer timed out");
                // Peer timed out fetching the item, continue with the next peer
                continue;
            }
            Err(error) => return Err(error.into()),
        }
    };

    if item.header().protocol_version() < parent_header.protocol_version() {
        return Err(Error::LowerVersionThanParent {
            parent: Box::new(parent_header.clone()),
            child: Box::new(item.header().clone()),
        });
    }

    check_block_version(item.header(), ctx.config.protocol_version())?;

    Ok(Some(item))
}

/// Compares the block's version with the current and parent version and returns an error if it is
/// too new or old.
fn check_block_version(
    header: &BlockHeader,
    current_version: ProtocolVersion,
) -> Result<(), Error> {
    if header.protocol_version() > current_version {
        return Err(Error::RetrievedBlockHeaderFromFutureVersion {
            current_version,
            block_header_with_future_version: Box::new(header.clone()),
        });
    }

    Ok(())
}

/// Queries all of the peers for a trie, puts the trie found from the network in the trie-store, and
/// returns any outstanding descendant tries.
async fn fetch_and_store_trie(
    trie_key: Digest,
    ctx: &ChainSyncContext<'_>,
) -> Result<Vec<Digest>, Error> {
    let fetched_trie = fetch_trie_retry_forever(trie_key, ctx).await?;
    match fetched_trie {
        TrieAlreadyPresentOrDownloaded::AlreadyPresent => Ok(ctx
            .effect_builder
            .find_missing_descendant_trie_keys(trie_key)
            .await?),
        TrieAlreadyPresentOrDownloaded::Downloaded(trie_bytes) => Ok(ctx
            .effect_builder
            .put_trie_and_find_missing_descendant_trie_keys(trie_bytes)
            .await?),
    }
}

/// Downloads and stores a block.
async fn fetch_and_store_block_by_hash(
    block_hash: BlockHash,
    ctx: &ChainSyncContext<'_>,
) -> Result<Box<Block>, FetcherError<Block>> {
    let fetched_block =
        fetch_retry_forever::<Block>(*ctx.effect_builder, ctx.config, block_hash).await?;
    match fetched_block {
        FetchedData::FromStorage { item: block, .. } => Ok(block),
        FetchedData::FromPeer { item: block, .. } => {
            ctx.effect_builder.put_block_to_storage(block.clone()).await;
            Ok(block)
        }
    }
}

/// A worker task that takes trie keys from a queue and downloads the trie.
async fn sync_trie_store_worker(
    worker_id: usize,
    abort: Arc<AtomicBool>,
    queue: Arc<WorkQueue<Digest>>,
    ctx: &ChainSyncContext<'_>,
) -> Result<(), Error> {
    while let Some(job) = queue.next_job().await {
        trace!(worker_id, trie_key = %job.inner(), "worker downloading trie");
        let child_jobs = fetch_and_store_trie(*job.inner(), ctx)
            .await
            .map_err(|err| {
                abort.store(true, atomic::Ordering::Relaxed);
                warn!(?err, trie_key = %job.inner(), "failed to download trie");
                err
            })?;
        trace!(?child_jobs, trie_key = %job.inner(), "downloaded trie node");
        if abort.load(atomic::Ordering::Relaxed) {
            return Ok(()); // Another task failed and sent an error.
        }
        for child_job in child_jobs {
            queue.push_job(child_job);
        }
        drop(job); // Make sure the job gets dropped only when the children are in the queue.
    }
    Ok(())
}

/// Synchronizes the trie store under a given state root hash.
async fn sync_trie_store(state_root_hash: Digest, ctx: &ChainSyncContext<'_>) -> Result<(), Error> {
    info!(?state_root_hash, "syncing trie store",);
    let _metric = ScopeTimer::new(&ctx.metrics.chain_sync_sync_trie_store_duration_seconds);

    // Flag set by a worker when it encounters an error.
    let abort = Arc::new(AtomicBool::new(false));

    let queue = Arc::new(WorkQueue::default());
    queue.push_job(state_root_hash);
    let mut workers: FuturesUnordered<_> = (0..ctx.config.max_parallel_trie_fetches())
        .map(|worker_id| sync_trie_store_worker(worker_id, abort.clone(), queue.clone(), ctx))
        .collect();
    while let Some(result) = workers.next().await {
        result?; // Return the error if a download failed.
    }

    Ok(())
}

/// Fetches the current header and fast-syncs to it.
///
/// Performs the following:
///
///  1. Fetches the most recent block header.
///  2. Fetches deploys for replay detection and historical switch block info.
///  3. The historical switch block info is needed by the [`EraSupervisor`].
///  4. Fetches global state for the current block header.
///
/// Returns the most recent block header along with the last trusted key block information for
/// validation.
async fn fast_sync(ctx: &ChainSyncContext<'_>) -> Result<(KeyBlockInfo, BlockHeader), Error> {
    let _metric = ScopeTimer::new(&ctx.metrics.chain_sync_fast_sync_total_duration_seconds);

    let trusted_key_block_info = get_trusted_key_block_info(ctx).await?;

    let (most_recent_block_header, most_recent_key_block_info) =
        fetch_block_headers_up_to_the_most_recent_one(&trusted_key_block_info, ctx).await?;

    fetch_blocks_for_deploy_replay_protection(
        &most_recent_block_header,
        &most_recent_key_block_info,
        ctx,
    )
    .await?;

    fetch_block_headers_needed_for_era_supervisor_initialization(&most_recent_block_header, ctx)
        .await?;

    // Synchronize the trie store for the most recent block header.
    sync_trie_store(*most_recent_block_header.state_root_hash(), ctx).await?;

    Ok((trusted_key_block_info, most_recent_block_header))
}

/// Gets the trusted key block info for a trusted block header.
async fn get_trusted_key_block_info(ctx: &ChainSyncContext<'_>) -> Result<KeyBlockInfo, Error> {
    let _metric = ScopeTimer::new(
        &ctx.metrics
            .chain_sync_get_trusted_key_block_info_duration_seconds,
    );

    // If the trusted block's version is newer than ours we return an error
    if ctx.trusted_block_header.protocol_version() > ctx.config.protocol_version() {
        return Err(Error::RetrievedBlockHeaderFromFutureVersion {
            current_version: ctx.config.protocol_version(),
            block_header_with_future_version: Box::new(ctx.trusted_block_header.clone()),
        });
    }
    // If the trusted block's version is older than ours we also return an error, except if we are
    // at the current activation point, i.e. at an upgrade.
    if ctx.trusted_block_header.protocol_version() < ctx.config.protocol_version()
        && ctx.trusted_block_header.next_block_era_id() != ctx.config.activation_point()
    {
        return Err(Error::TrustedBlockHasOldVersion {
            current_version: ctx.config.protocol_version(),
            block_header_with_old_version: Box::new(ctx.trusted_block_header.clone()),
        });
    }

    // Fetch each parent hash one by one until we have the switch block info
    // This will crash if we try to get the parent hash of genesis, which is the default [0u8; 32]
    let mut current_header_to_walk_back_from = ctx.trusted_block_header.clone();
    loop {
        // Check that we are not restarting right after an emergency restart, which is too early
        match ctx.config.last_emergency_restart() {
            Some(last_emergency_restart)
                if last_emergency_restart > current_header_to_walk_back_from.era_id() =>
            {
                return Err(Error::TrustedHeaderEraTooEarly {
                    trusted_header: Box::new(ctx.trusted_block_header.clone()),
                    maybe_last_emergency_restart_era_id: ctx.config.last_emergency_restart(),
                })
            }
            _ => {}
        }

        if let Some(key_block_info) = KeyBlockInfo::maybe_from_block_header(
            &current_header_to_walk_back_from,
            ctx.config.verifiable_chunked_hash_activation(),
        ) {
            check_block_version(ctx.trusted_block_header, ctx.config.protocol_version())?;
            break Ok(key_block_info);
        }

        if current_header_to_walk_back_from.height() == 0 {
            break Err(Error::HitGenesisBlockTryingToGetTrustedEraValidators {
                trusted_header: ctx.trusted_block_header.clone(),
            });
        }

        current_header_to_walk_back_from = *fetch_and_store_block_header(
            *ctx.effect_builder,
            ctx.config,
            *current_header_to_walk_back_from.parent_hash(),
        )
        .await?;
    }
}

/// Get the most recent header which has the same version as ours
/// We keep fetching by height until none of our peers have a block at that height and we are in
/// the current era.
async fn fetch_block_headers_up_to_the_most_recent_one(
    trusted_key_block_info: &KeyBlockInfo,
    ctx: &ChainSyncContext<'_>,
) -> Result<(BlockHeader, KeyBlockInfo), Error> {
    let _metric = ScopeTimer::new(&ctx.metrics.chain_sync_fetch_block_headers_duration_seconds);

    let mut most_recent_block_header = ctx.trusted_block_header.clone();
    let mut most_recent_key_block_info = trusted_key_block_info.clone();
    loop {
        let maybe_fetched_block = fetch_and_store_next::<BlockHeaderWithMetadata>(
            &most_recent_block_header,
            &most_recent_key_block_info,
            ctx,
        )
        .await?;

        if let Some(more_recent_block_header_with_metadata) = maybe_fetched_block {
            most_recent_block_header = more_recent_block_header_with_metadata.block_header;

            // If the new block is a switch block, update the validator weights, etc...
            if let Some(key_block_info) = KeyBlockInfo::maybe_from_block_header(
                &most_recent_block_header,
                ctx.config.verifiable_chunked_hash_activation(),
            ) {
                most_recent_key_block_info = key_block_info;
            }
        } else {
            // If we timed out, consider syncing done.
            break;
        }

        // If we synced up to the current era, we can also consider syncing done.
        if is_current_era(
            &most_recent_block_header,
            &most_recent_key_block_info,
            ctx.config,
        ) {
            break;
        }
    }
    Ok((most_recent_block_header, most_recent_key_block_info))
}

/// Fetch and store all blocks that can contain not-yet-expired deploys. These are needed for
/// replay detection.
async fn fetch_blocks_for_deploy_replay_protection(
    most_recent_block_header: &BlockHeader,
    most_recent_key_block_info: &KeyBlockInfo,
    ctx: &ChainSyncContext<'_>,
) -> Result<(), Error> {
    let _metric = ScopeTimer::new(&ctx.metrics.chain_sync_replay_protection_duration_seconds);

    let mut current_header = most_recent_block_header.clone();
    while most_recent_key_block_info
        .era_start
        .saturating_diff(current_header.timestamp())
        < ctx.config.deploy_max_ttl()
        && current_header.height() != 0
    {
        current_header = fetch_and_store_block_by_hash(*current_header.parent_hash(), ctx)
            .await?
            .take_header();
    }
    Ok(())
}

/// The era supervisor requires enough switch blocks to be stored in the database to be able to
/// initialize the most recent eras.
async fn fetch_block_headers_needed_for_era_supervisor_initialization(
    most_recent_block_header: &BlockHeader,
    ctx: &ChainSyncContext<'_>,
) -> Result<(), Error> {
    let _metric = ScopeTimer::new(&ctx.metrics.chain_sync_era_supervisor_init_duration_seconds);

    let earliest_open_era = ctx
        .config
        .earliest_open_era(most_recent_block_header.era_id());
    let earliest_era_needed_by_era_supervisor =
        ctx.config.earliest_switch_block_needed(earliest_open_era);
    let mut current_walk_back_header = most_recent_block_header.clone();
    while current_walk_back_header.era_id() > earliest_era_needed_by_era_supervisor {
        current_walk_back_header = *fetch_and_store_block_header(
            *ctx.effect_builder,
            ctx.config,
            *current_walk_back_header.parent_hash(),
        )
        .await?;
    }
    Ok(())
}

/// Downloads and saves the deploys and transfers for a block.
async fn sync_deploys_and_transfers_and_state(
    block: &Block,
    ctx: &ChainSyncContext<'_>,
) -> Result<(), Error> {
    fetch_and_store_deploys(
        block.deploy_hashes().iter().chain(block.transfer_hashes()),
        ctx,
    )
    .await?;
    sync_trie_store(*block.header().state_root_hash(), ctx).await
}

/// Sync to genesis.
///
/// Performs the following:
///
///  1. Fetches blocks all the way back to genesis.
///  2. Fetches global state for each block starting from genesis onwards.
///  3. Fetches all deploys and finality signatures.
///  4. Stops after fetching a block with the current version and its global state.
///
/// Returns the block header with our current version and the last trusted key block information for
/// validation.
async fn sync_to_genesis(ctx: &ChainSyncContext<'_>) -> Result<(KeyBlockInfo, BlockHeader), Error> {
    let _metric = ScopeTimer::new(&ctx.metrics.chain_sync_to_genesis_total_duration_seconds);

    // Get the trusted block info. This will fail if we are trying to join with a trusted hash in
    // era 0.
    let mut trusted_key_block_info = get_trusted_key_block_info(ctx).await?;

    let trusted_block = *fetch_and_store_block_by_hash(ctx.trusted_hash(), ctx).await?;

    fetch_to_genesis(&trusted_block, ctx).await?;

    // Sync forward until we are at the current version.
    let most_recent_block = fetch_forward(trusted_block, &mut trusted_key_block_info, ctx).await?;

    Ok((trusted_key_block_info, most_recent_block.take_header()))
}

async fn fetch_forward(
    trusted_block: Block,
    trusted_key_block_info: &mut KeyBlockInfo,
    ctx: &ChainSyncContext<'_>,
) -> Result<Block, Error> {
    let _metric = ScopeTimer::new(&ctx.metrics.chain_sync_fetch_forward_duration_seconds);

    let mut most_recent_block = trusted_block;
    while most_recent_block.header().protocol_version() < ctx.config.protocol_version() {
        let maybe_fetched_block_with_metadata = fetch_and_store_next::<BlockWithMetadata>(
            most_recent_block.header(),
            &*trusted_key_block_info,
            ctx,
        )
        .await?;
        most_recent_block = match maybe_fetched_block_with_metadata {
            Some(block_with_metadata) => block_with_metadata.block,
            None => {
                tokio::time::sleep(ctx.config.retry_interval()).await;
                continue;
            }
        };
        sync_deploys_and_transfers_and_state(&most_recent_block, ctx).await?;
        if let Some(key_block_info) = KeyBlockInfo::maybe_from_block_header(
            most_recent_block.header(),
            ctx.config.verifiable_chunked_hash_activation(),
        ) {
            *trusted_key_block_info = key_block_info;
        }
    }
    Ok(most_recent_block)
}

async fn fetch_to_genesis(trusted_block: &Block, ctx: &ChainSyncContext<'_>) -> Result<(), Error> {
    let _metric = ScopeTimer::new(&ctx.metrics.chain_sync_fetch_to_genesis_duration_seconds);

    let mut walkback_block = trusted_block.clone();
    loop {
        sync_deploys_and_transfers_and_state(&walkback_block, ctx).await?;
        if walkback_block.height() == 0 {
            break;
        } else {
            walkback_block =
                *fetch_and_store_block_by_hash(*walkback_block.header().parent_hash(), ctx).await?;
        }
    }
    Ok(())
}

/// Runs the chain synchronization task.
pub(super) async fn run_chain_sync_task(
    effect_builder: EffectBuilder<JoinerEvent>,
    config: Config,
    metrics: Metrics,
    trusted_hash: BlockHash,
) -> Result<BlockHeader, Error> {
    let _metric = ScopeTimer::new(&metrics.chain_sync_total_duration_seconds);

    // Fetch the trusted header
    let trusted_block_header = fetch_and_store_initial_trusted_block_header(
        effect_builder,
        &config,
        &metrics,
        trusted_hash,
    )
    .await?;

    let chain_sync_context =
        ChainSyncContext::new(&effect_builder, &config, &trusted_block_header, &metrics);

    verify_trusted_block_header(&chain_sync_context)?;

    if handle_emergency_restart(&chain_sync_context).await? {
        return Ok(*trusted_block_header);
    }

    if handle_upgrade(&chain_sync_context).await? {
        return Ok(*trusted_block_header);
    }

    let (trusted_key_block_info, most_recent_block_header) = if config.sync_to_genesis() {
        sync_to_genesis(&chain_sync_context).await?
    } else {
        fast_sync(&chain_sync_context).await?
    };

    let most_recent_block_header = execute_blocks(
        &most_recent_block_header,
        &trusted_key_block_info,
        &chain_sync_context,
    )
    .await?;

    info!(
        era_id = ?most_recent_block_header.era_id(),
        height = most_recent_block_header.height(),
        now = %Timestamp::now(),
        block_timestamp = %most_recent_block_header.timestamp(),
        "finished synchronizing",
    );

    Ok(most_recent_block_header)
}

async fn fetch_and_store_initial_trusted_block_header(
    effect_builder: EffectBuilder<JoinerEvent>,
    config: &Config,
    metrics: &Metrics,
    trusted_hash: BlockHash,
) -> Result<Box<BlockHeader>, Error> {
    let _metric = ScopeTimer::new(
        &metrics.chain_sync_fetch_and_store_initial_trusted_block_header_duration_seconds,
    );
    let trusted_block_header =
        fetch_and_store_block_header(effect_builder, config, trusted_hash).await?;
    Ok(trusted_block_header)
}

fn verify_trusted_block_header(ctx: &ChainSyncContext<'_>) -> Result<(), Error> {
    if ctx.trusted_block_header.protocol_version() > ctx.config.protocol_version() {
        return Err(Error::RetrievedBlockHeaderFromFutureVersion {
            current_version: ctx.config.protocol_version(),
            block_header_with_future_version: Box::new(ctx.trusted_block_header.clone()),
        });
    }

    let era_duration: TimeDiff = cmp::max(
        ctx.config.min_round_length() * ctx.config.min_era_height(),
        ctx.config.era_duration(),
    );

    if ctx.trusted_block_header.timestamp()
        + era_duration
            * ctx
                .config
                .unbonding_delay()
                .saturating_sub(ctx.config.auction_delay())
        < Timestamp::now()
    {
        warn!(
            ?ctx.trusted_block_header,
            "timestamp of trusted hash is older than \
             era_duration * (unbonding_delay - auction_delay)"
        );
    };

    Ok(())
}

async fn handle_emergency_restart(ctx: &ChainSyncContext<'_>) -> Result<bool, Error> {
    let _metric = ScopeTimer::new(&ctx.metrics.chain_sync_emergency_restart_duration_seconds);

    let maybe_last_emergency_restart_era_id = ctx.config.last_emergency_restart();
    if let Some(last_emergency_restart_era) = maybe_last_emergency_restart_era_id {
        // After an emergency restart, the old validators cannot be trusted anymore. So the last
        // block before the restart or a later block must be given by the trusted hash. That way we
        // never have to use the untrusted validators' finality signatures.
        if ctx.trusted_block_header.next_block_era_id() < last_emergency_restart_era {
            return Err(Error::TryingToJoinBeforeLastEmergencyRestartEra {
                last_emergency_restart_era,
                trusted_hash: ctx.trusted_hash(),
                trusted_block_header: Box::new(ctx.trusted_block_header.clone()),
            });
        }
        // If the trusted hash specifies the last block before the emergency restart, we have to
        // compute the immediate switch block ourselves, since there's no other way to verify that
        // block. We just sync the trie there and return, so the upgrade can be applied.
        if ctx.trusted_block_header.is_switch_block()
            && ctx.trusted_block_header.next_block_era_id() == last_emergency_restart_era
        {
            sync_trie_store(*ctx.trusted_block_header.state_root_hash(), ctx).await?;
            return Ok(true);
        }
    }

    Ok(false)
}

async fn handle_upgrade(ctx: &ChainSyncContext<'_>) -> Result<bool, Error> {
    let _metric = ScopeTimer::new(&ctx.metrics.chain_sync_upgrade_duration_seconds);

    // If we are at an upgrade:
    // 1. Sync the trie store
    // 2. Get the trusted era validators from the last switch block
    // 3. Try to get the next block by height; if there is `None` then switch to the participating
    //    reactor.
    if ctx.trusted_block_header.is_switch_block()
        && ctx.trusted_block_header.next_block_era_id() == ctx.config.activation_point()
    {
        let trusted_key_block_info = get_trusted_key_block_info(ctx).await?;

        let fetch_and_store_next_result = fetch_and_store_next::<BlockHeaderWithMetadata>(
            ctx.trusted_block_header,
            &trusted_key_block_info,
            ctx,
        )
        .await?;

        if fetch_and_store_next_result.is_none() {
            sync_trie_store(*ctx.trusted_block_header.state_root_hash(), ctx).await?;
            return Ok(true);
        }
    }

    Ok(false)
}

async fn execute_blocks(
    most_recent_block_header: &BlockHeader,
    trusted_key_block_info: &KeyBlockInfo,
    ctx: &ChainSyncContext<'_>,
) -> Result<BlockHeader, Error> {
    let _metric = ScopeTimer::new(&ctx.metrics.chain_sync_execute_blocks_duration_seconds);

    // Execute blocks to get to current.
    let mut execution_pre_state = ExecutionPreState::from_block_header(
        most_recent_block_header,
        ctx.config.verifiable_chunked_hash_activation(),
    );
    info!(
        era_id = ?most_recent_block_header.era_id(),
        height = most_recent_block_header.height(),
        now = %Timestamp::now(),
        block_timestamp = %most_recent_block_header.timestamp(),
        "fetching and executing blocks to synchronize to current",
    );

    let mut most_recent_block_header = most_recent_block_header.clone();
    let mut trusted_key_block_info = trusted_key_block_info.clone();
    loop {
        let result = fetch_and_store_next::<BlockWithMetadata>(
            &most_recent_block_header,
            &trusted_key_block_info,
            ctx,
        )
        .await?;
        let block = match result {
            None => {
                let in_current_era = is_current_era(
                    &most_recent_block_header,
                    &trusted_key_block_info,
                    ctx.config,
                );
                info!(
                    era = most_recent_block_header.era_id().value(),
                    in_current_era,
                    height = most_recent_block_header.height(),
                    timestamp = %most_recent_block_header.timestamp(),
                    "couldn't download a more recent block; finishing syncing",
                );
                break;
            }
            Some(block_with_metadata) => block_with_metadata.block,
        };

        let deploys = fetch_and_store_deploys(block.deploy_hashes().iter(), ctx).await?;

        let transfers = fetch_and_store_deploys(block.transfer_hashes().iter(), ctx).await?;

        info!(
            era_id = ?block.header().era_id(),
            height = block.height(),
            now = %Timestamp::now(),
            block_timestamp = %block.timestamp(),
            "executing block",
        );
        let block_and_execution_effects = ctx
            .effect_builder
            .execute_finalized_block(
                block.protocol_version(),
                execution_pre_state.clone(),
                FinalizedBlock::from(block.clone()),
                deploys,
                transfers,
            )
            .await?;

        if block != *block_and_execution_effects.block() {
            return Err(Error::ExecutedBlockIsNotTheSameAsDownloadedBlock {
                executed_block: Box::new(Block::from(block_and_execution_effects)),
                downloaded_block: Box::new(block.clone()),
            });
        }

        most_recent_block_header = block.take_header();
        execution_pre_state = ExecutionPreState::from_block_header(
            &most_recent_block_header,
            ctx.config.verifiable_chunked_hash_activation(),
        );

        if let Some(key_block_info) = KeyBlockInfo::maybe_from_block_header(
            &most_recent_block_header,
            ctx.config.verifiable_chunked_hash_activation(),
        ) {
            trusted_key_block_info = key_block_info;
        }

        // If we managed to sync up to the current era, stop - we'll have to sync the consensus
        // protocol state, anyway.
        if is_current_era(
            &most_recent_block_header,
            &trusted_key_block_info,
            ctx.config,
        ) {
            info!(
                era = most_recent_block_header.era_id().value(),
                height = most_recent_block_header.height(),
                timestamp = %most_recent_block_header.timestamp(),
                "synchronized up to the current era; finishing syncing",
            );
            break;
        }
    }
    Ok(most_recent_block_header)
}

async fn fetch_and_store_deploys(
    hashes: impl Iterator<Item = &DeployHash>,
    ctx: &ChainSyncContext<'_>,
) -> Result<Vec<Deploy>, Error> {
    let hashes: Vec<_> = hashes.cloned().collect();
    let mut deploys: Vec<Deploy> = Vec::with_capacity(hashes.len());
    let mut stream = futures::stream::iter(hashes)
        .map(|hash| fetch_and_store_deploy(hash, ctx))
        .buffer_unordered(ctx.config.max_parallel_deploy_fetches());
    while let Some(result) = stream.next().await {
        let deploy = result?;
        trace!("fetched {:?}", deploy);
        deploys.push(*deploy);
    }

    Ok(deploys)
}

/// Returns `true` if `most_recent_block` belongs to an era that is still ongoing.
pub(super) fn is_current_era(
    most_recent_block: &BlockHeader,
    trusted_key_block_info: &KeyBlockInfo,
    config: &Config,
) -> bool {
    is_current_era_given_current_timestamp(
        most_recent_block,
        trusted_key_block_info,
        config,
        Timestamp::now(),
    )
}

fn is_current_era_given_current_timestamp(
    most_recent_block: &BlockHeader,
    trusted_key_block_info: &KeyBlockInfo,
    config: &Config,
    current_timestamp: Timestamp,
) -> bool {
    let KeyBlockInfo {
        era_start, height, ..
    } = trusted_key_block_info;

    // If the minimum era duration has not yet run out, the era is still current.
    if current_timestamp.saturating_diff(*era_start) < config.era_duration() {
        return true;
    }

    // Otherwise estimate the earliest possible end of this era based on how many blocks remain.
    let remaining_blocks_in_this_era = config
        .min_era_height()
        .saturating_sub(most_recent_block.height() - *height);
    let time_since_most_recent_block =
        current_timestamp.saturating_diff(most_recent_block.timestamp());
    time_since_most_recent_block < config.min_round_length() * remaining_blocks_in_this_era
}

#[cfg(test)]
mod tests {
    use std::iter;

    use rand::Rng;

    use casper_types::{EraId, ProtocolVersion, PublicKey, SecretKey};

    use super::*;
    use crate::{
        components::consensus::EraReport,
        crypto::AsymmetricKeyExt,
        testing::TestRng,
        types::{Block, BlockPayload, Chainspec, FinalizedBlock, NodeConfig},
        utils::Loadable,
        SmallNetworkConfig,
    };

    /// Creates a block for testing, with the given data, and returns its header.
    ///
    /// The other fields are filled in with defaults, since they are not used in these tests.
    fn create_block(
        timestamp: Timestamp,
        era_id: EraId,
        height: u64,
        switch_block: bool,
        verifiable_chunked_hash_activation: EraId,
    ) -> BlockHeader {
        let secret_key = SecretKey::doc_example();
        let public_key = PublicKey::from(secret_key);

        let maybe_era_report = switch_block.then(|| EraReport {
            equivocators: Default::default(),
            rewards: Default::default(),
            inactive_validators: Default::default(),
        });
        let next_era_validator_weights =
            switch_block.then(|| iter::once((public_key.clone(), 100.into())).collect());

        let block_payload = BlockPayload::new(
            Default::default(), // deploy hashes
            Default::default(), // transfer hashes
            Default::default(), // accusations
            false,              // random bit
        );

        let finalized_block = FinalizedBlock::new(
            block_payload,
            maybe_era_report,
            timestamp,
            era_id,
            height,
            public_key,
        );

        Block::new(
            Default::default(), // parent block hash
            Default::default(), // parent random seed
            Default::default(), // state root hash
            finalized_block,
            next_era_validator_weights,
            Default::default(), // protocol version
            verifiable_chunked_hash_activation,
        )
        .expect("failed to create block for tests")
        .take_header()
    }

    #[test]
    fn test_is_current_era() {
        let mut rng = TestRng::new();
        let mut chainspec = Chainspec::from_resources("local");

        let genesis_time = chainspec
            .protocol_config
            .activation_point
            .genesis_timestamp()
            .expect("test expects genesis timestamp in chainspec");
        let min_round_length = chainspec.highway_config.min_round_length();

        // Configure eras to have at least 10 blocks but to last at least 20 minimum-length rounds.
        let era_duration = min_round_length * 20;
        let minimum_era_height = 10;
        chainspec.core_config.era_duration = era_duration;
        chainspec.core_config.minimum_era_height = minimum_era_height;
        let config = Config::new(
            Arc::new(chainspec),
            NodeConfig::default(),
            SmallNetworkConfig::default(),
        );

        // `verifiable_chunked_hash_activation` can be chosen arbitrarily
        let verifiable_chunked_hash_activation = EraId::from(rng.gen_range(0..=10));

        // We assume era 6 started after six minimum era durations, at block 100.
        let era6_start = genesis_time + era_duration * 6;
        let switch_block5 = create_block(
            era6_start,
            EraId::from(5),
            100,
            true,
            verifiable_chunked_hash_activation,
        );

        let trusted_switch_block_info5 = KeyBlockInfo::maybe_from_block_header(
            &switch_block5,
            verifiable_chunked_hash_activation,
        )
        .expect("no switch block info for switch block");

        // If we are still within the minimum era duration the era is current, even if we have the
        // required number of blocks (115 - 100 > 10).
        let block_time = era6_start + era_duration - 10.into();
        let now = block_time + 5.into();
        let block = create_block(
            block_time,
            EraId::from(6),
            115,
            false,
            verifiable_chunked_hash_activation,
        );
        assert!(is_current_era_given_current_timestamp(
            &block,
            &trusted_switch_block_info5,
            &config,
            now
        ));

        // If the minimum duration has passed but we we know we don't have all blocks yet, it's
        // also still current. There are still five blocks missing but only four rounds have
        // passed.
        let block_time = era6_start + era_duration * 2;
        let now = block_time + min_round_length * 4;
        let block = create_block(
            block_time,
            EraId::from(6),
            105,
            false,
            verifiable_chunked_hash_activation,
        );
        assert!(is_current_era_given_current_timestamp(
            &block,
            &trusted_switch_block_info5,
            &config,
            now
        ));

        // If both criteria are satisfied, the era could have ended.
        let block_time = era6_start + era_duration * 2;
        let now = block_time + min_round_length * 5;
        let block = create_block(
            block_time,
            EraId::from(6),
            105,
            false,
            verifiable_chunked_hash_activation,
        );
        assert!(!is_current_era_given_current_timestamp(
            &block,
            &trusted_switch_block_info5,
            &config,
            now
        ));
    }

    #[test]
    fn test_check_block_version() {
        let mut rng = TestRng::new();
        let v1_2_0 = ProtocolVersion::from_parts(1, 2, 0);
        let v1_3_0 = ProtocolVersion::from_parts(1, 3, 0);

        // `verifiable_chunked_hash_activation` can be chosen arbitrarily
        let verifiable_chunked_hash_activation = EraId::from(rng.gen_range(0..=10));
        let header = Block::random_with_specifics(
            &mut rng,
            EraId::from(6),
            101,
            v1_3_0,
            false,
            verifiable_chunked_hash_activation,
        )
        .take_header();

        // The new block's protocol version is the current one, 1.3.0.
        check_block_version(&header, v1_3_0).expect("versions are valid");

        // If the current version is only 1.2.0 but the block's is 1.3.0, we have to upgrade.
        match check_block_version(&header, v1_2_0) {
            Err(Error::RetrievedBlockHeaderFromFutureVersion {
                current_version,
                block_header_with_future_version,
            }) => {
                assert_eq!(v1_2_0, current_version);
                assert_eq!(header, *block_header_with_future_version);
            }
            result => panic!("expected future block version error, got {:?}", result),
        }
    }
}
