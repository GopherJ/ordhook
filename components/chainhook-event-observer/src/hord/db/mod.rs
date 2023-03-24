use std::{
    collections::HashMap,
    path::PathBuf,
    sync::mpsc::{channel, Sender},
};

use chainhook_types::{
    BitcoinBlockData, BlockIdentifier, OrdinalInscriptionRevealData, TransactionIdentifier,
};
use hiro_system_kit::slog;

use rusqlite::{Connection, OpenFlags, ToSql};
use threadpool::ThreadPool;

use crate::{
    indexer::bitcoin::{
        retrieve_block_hash_with_retry, retrieve_full_block_breakdown_with_retry,
        standardize_bitcoin_block, BitcoinBlockFullBreakdown,
    },
    observer::BitcoinConfig,
    utils::Context,
};

use super::{ord::height::Height, update_hord_db_and_augment_bitcoin_block};

fn get_default_hord_db_file_path(base_dir: &PathBuf) -> PathBuf {
    let mut destination_path = base_dir.clone();
    destination_path.push("hord.sqlite");
    destination_path
}

pub fn open_readonly_hord_db_conn(base_dir: &PathBuf, ctx: &Context) -> Result<Connection, String> {
    let path = get_default_hord_db_file_path(&base_dir);
    let conn = open_existing_readonly_db(&path, ctx);
    Ok(conn)
}

pub fn open_readwrite_hord_db_conn(
    base_dir: &PathBuf,
    ctx: &Context,
) -> Result<Connection, String> {
    let conn = create_or_open_readwrite_db(&base_dir, ctx);
    Ok(conn)
}

pub fn initialize_hord_db(path: &PathBuf, ctx: &Context) -> Connection {
    let conn = create_or_open_readwrite_db(path, ctx);
    if let Err(e) = conn.execute(
        "CREATE TABLE IF NOT EXISTS blocks (
            id INTEGER NOT NULL PRIMARY KEY,
            compacted_bytes TEXT NOT NULL
        )",
        [],
    ) {
        ctx.try_log(|logger| slog::error!(logger, "{}", e.to_string()));
    }
    if let Err(e) = conn.execute(
        "CREATE TABLE IF NOT EXISTS inscriptions (
            inscription_id TEXT NOT NULL PRIMARY KEY,
            block_height INTEGER NOT NULL,
            block_hash TEXT NOT NULL,
            outpoint_to_watch TEXT NOT NULL,
            ordinal_number INTEGER NOT NULL,
            inscription_number INTEGER NOT NULL,
            offset INTEGER NOT NULL
        )",
        [],
    ) {
        ctx.try_log(|logger| slog::error!(logger, "{}", e.to_string()));
    }
    if let Err(e) = conn.execute(
        "CREATE INDEX IF NOT EXISTS index_inscriptions_on_outpoint_to_watch ON inscriptions(outpoint_to_watch);",
        [],
    ) {
        ctx.try_log(|logger| slog::error!(logger, "{}", e.to_string()));
    }
    if let Err(e) = conn.execute(
        "CREATE INDEX IF NOT EXISTS index_inscriptions_on_ordinal_number ON inscriptions(ordinal_number);",
        [],
    ) {
        ctx.try_log(|logger| slog::error!(logger, "{}", e.to_string()));
    }

    conn
}

fn create_or_open_readwrite_db(cache_path: &PathBuf, ctx: &Context) -> Connection {
    let path = get_default_hord_db_file_path(&cache_path);
    let open_flags = match std::fs::metadata(&path) {
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                // need to create
                if let Some(dirp) = PathBuf::from(&path).parent() {
                    std::fs::create_dir_all(dirp).unwrap_or_else(|e| {
                        ctx.try_log(|logger| slog::error!(logger, "{}", e.to_string()));
                    });
                }
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
            } else {
                panic!("FATAL: could not stat {}", path.display());
            }
        }
        Ok(_md) => {
            // can just open
            OpenFlags::SQLITE_OPEN_READ_WRITE
        }
    };

    let conn = loop {
        match Connection::open_with_flags(&path, open_flags) {
            Ok(conn) => break conn,
            Err(e) => {
                ctx.try_log(|logger| slog::error!(logger, "{}", e.to_string()));
            }
        };
        std::thread::sleep(std::time::Duration::from_secs(1));
    };
    // db.profile(Some(trace_profile));
    // db.busy_handler(Some(tx_busy_handler))?;
    conn.pragma_update(None, "journal_mode", &"WAL").unwrap();
    conn.pragma_update(None, "synchronous", &"NORMAL").unwrap();
    conn
}

fn open_existing_readonly_db(path: &PathBuf, ctx: &Context) -> Connection {
    let open_flags = match std::fs::metadata(path) {
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                panic!("FATAL: could not find {}", path.display());
            } else {
                panic!("FATAL: could not stat {}", path.display());
            }
        }
        Ok(_md) => {
            // can just open
            OpenFlags::SQLITE_OPEN_READ_ONLY
        }
    };

    let conn = loop {
        match Connection::open_with_flags(path, open_flags) {
            Ok(conn) => break conn,
            Err(e) => {
                ctx.try_log(|logger| slog::error!(logger, "{}", e.to_string()));
            }
        };
        std::thread::sleep(std::time::Duration::from_secs(1));
    };
    return conn;
}

#[derive(Debug, Serialize, Deserialize)]
// pub struct CompactedBlock(Vec<(Vec<(u32, u16, u64)>, Vec<u64>)>);
pub struct CompactedBlock(
    (
        ([u8; 4], u64),
        Vec<([u8; 4], Vec<([u8; 4], u32, u16, u64)>, Vec<u64>)>,
    ),
);

impl CompactedBlock {
    pub fn from_full_block(block: &BitcoinBlockFullBreakdown) -> CompactedBlock {
        let mut txs = vec![];
        let mut coinbase_value = 0;
        let coinbase_txid = {
            let txid = hex::decode(block.tx[0].txid.to_string()).unwrap();
            [txid[0], txid[1], txid[2], txid[3]]
        };
        for coinbase_output in block.tx[0].vout.iter() {
            coinbase_value += coinbase_output.value.to_sat();
        }
        for tx in block.tx.iter().skip(1) {
            let mut inputs = vec![];
            for input in tx.vin.iter() {
                let txin = hex::decode(input.txid.unwrap().to_string()).unwrap();

                inputs.push((
                    [txin[0], txin[1], txin[2], txin[3]],
                    input.prevout.as_ref().unwrap().height as u32,
                    input.vout.unwrap() as u16,
                    input.prevout.as_ref().unwrap().value.to_sat(),
                ));
            }
            let mut outputs = vec![];
            for output in tx.vout.iter() {
                outputs.push(output.value.to_sat());
            }
            let txid = hex::decode(tx.txid.to_string()).unwrap();
            txs.push(([txid[0], txid[1], txid[2], txid[3]], inputs, outputs));
        }
        CompactedBlock(((coinbase_txid, coinbase_value), txs))
    }

    pub fn from_standardized_block(block: &BitcoinBlockData) -> CompactedBlock {
        let mut txs = vec![];
        let mut coinbase_value = 0;
        let coinbase_txid = {
            let txid =
                hex::decode(&block.transactions[0].transaction_identifier.hash[2..]).unwrap();
            [txid[0], txid[1], txid[2], txid[3]]
        };
        for coinbase_output in block.transactions[0].metadata.outputs.iter() {
            coinbase_value += coinbase_output.value;
        }
        for tx in block.transactions.iter().skip(1) {
            let mut inputs = vec![];
            for input in tx.metadata.inputs.iter() {
                let txin = hex::decode(&input.previous_output.txid[2..]).unwrap();

                inputs.push((
                    [txin[0], txin[1], txin[2], txin[3]],
                    input.previous_output.block_height as u32,
                    input.previous_output.vout as u16,
                    input.previous_output.value,
                ));
            }
            let mut outputs = vec![];
            for output in tx.metadata.outputs.iter() {
                outputs.push(output.value);
            }
            let txid = hex::decode(&tx.transaction_identifier.hash[2..]).unwrap();
            txs.push(([txid[0], txid[1], txid[2], txid[3]], inputs, outputs));
        }
        CompactedBlock(((coinbase_txid, coinbase_value), txs))
    }

    pub fn from_hex_bytes(bytes: &str) -> CompactedBlock {
        let bytes = hex::decode(&bytes).unwrap();
        let value = ciborium::de::from_reader(&bytes[..]).unwrap();
        value
    }

    pub fn to_hex_bytes(&self) -> String {
        use ciborium::cbor;
        let value = cbor!(self).unwrap();
        let mut bytes = vec![];
        let _ = ciborium::ser::into_writer(&value, &mut bytes);
        let hex_bytes = hex::encode(bytes);
        hex_bytes
    }
}

pub fn find_latest_compacted_block_known(hord_db_conn: &Connection) -> u32 {
    let args: &[&dyn ToSql] = &[];
    let mut stmt = hord_db_conn
        .prepare("SELECT id FROM blocks ORDER BY id DESC LIMIT 1")
        .unwrap();
    let mut rows = stmt.query(args).unwrap();
    while let Ok(Some(row)) = rows.next() {
        let id: u32 = row.get(0).unwrap();
        return id;
    }
    0
}

pub fn find_compacted_block_at_block_height(
    block_height: u32,
    hord_db_conn: &Connection,
) -> Option<CompactedBlock> {
    let args: &[&dyn ToSql] = &[&block_height.to_sql().unwrap()];
    let mut stmt = hord_db_conn
        .prepare("SELECT compacted_bytes FROM blocks WHERE id = ?")
        .unwrap();
    let result_iter = stmt
        .query_map(args, |row| {
            let hex_bytes: String = row.get(0).unwrap();
            Ok(CompactedBlock::from_hex_bytes(&hex_bytes))
        })
        .unwrap();

    for result in result_iter {
        return Some(result.unwrap());
    }
    return None;
}

pub fn store_new_inscription(
    inscription_data: &OrdinalInscriptionRevealData,
    block_identifier: &BlockIdentifier,
    hord_db_conn: &Connection,
    ctx: &Context,
) {
    if let Err(e) = hord_db_conn.execute(
        "INSERT INTO inscriptions (inscription_id, outpoint_to_watch, ordinal_number, inscription_number, offset, block_height, block_hash) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![&inscription_data.inscription_id, &inscription_data.satpoint_post_inscription[0..inscription_data.satpoint_post_inscription.len()-2], &inscription_data.ordinal_number, &inscription_data.inscription_number, 0, &block_identifier.index, &block_identifier.hash],
    ) {
        ctx.try_log(|logger| slog::error!(logger, "{}", e.to_string()));
    }
}

pub fn update_transfered_inscription(
    inscription_id: &str,
    outpoint_post_transfer: &str,
    offset: u64,
    hord_db_conn: &Connection,
    ctx: &Context,
) {
    if let Err(e) = hord_db_conn.execute(
        "UPDATE inscriptions SET outpoint_to_watch = ?, offset = ? WHERE inscription_id = ?",
        rusqlite::params![&outpoint_post_transfer, &offset, &inscription_id],
    ) {
        ctx.try_log(|logger| slog::error!(logger, "{}", e.to_string()));
    }
}

pub fn find_last_inscription_number(
    hord_db_conn: &Connection,
    _ctx: &Context,
) -> Result<u64, String> {
    let args: &[&dyn ToSql] = &[];
    let mut stmt = hord_db_conn
        .prepare(
            "SELECT inscription_number FROM inscriptions ORDER BY inscription_number DESC LIMIT 1",
        )
        .unwrap();
    let mut rows = stmt.query(args).unwrap();
    while let Ok(Some(row)) = rows.next() {
        let inscription_number: u64 = row.get(0).unwrap();
        return Ok(inscription_number);
    }
    Ok(0)
}

pub fn find_inscription_with_ordinal_number(
    ordinal_number: &u64,
    hord_db_conn: &Connection,
    _ctx: &Context,
) -> Option<String> {
    let args: &[&dyn ToSql] = &[&ordinal_number.to_sql().unwrap()];
    let mut stmt = hord_db_conn
        .prepare("SELECT inscription_id FROM inscriptions WHERE ordinal_number = ?")
        .unwrap();
    let mut rows = stmt.query(args).unwrap();
    while let Ok(Some(row)) = rows.next() {
        let inscription_id: String = row.get(0).unwrap();
        return Some(inscription_id);
    }
    return None;
}

pub fn find_all_inscriptions(hord_db_conn: &Connection) -> Vec<(String, u64, u64, u64)> {
    let args: &[&dyn ToSql] = &[];
    let mut stmt = hord_db_conn
        .prepare("SELECT inscription_id, inscription_number, ordinal_number, block_number FROM inscriptions ORDER BY inscription_number ASC")
        .unwrap();
    let mut results = vec![];
    let mut rows = stmt.query(args).unwrap();
    while let Ok(Some(row)) = rows.next() {
        let inscription_id: String = row.get(0).unwrap();
        let inscription_number: u64 = row.get(1).unwrap();
        let ordinal_number: u64 = row.get(2).unwrap();
        let block_number: u64 = row.get(3).unwrap();
        results.push((
            inscription_id,
            inscription_number,
            ordinal_number,
            block_number,
        ));
    }
    return results;
}

pub fn find_inscriptions_at_wached_outpoint(
    outpoint: &str,
    hord_db_conn: &Connection,
) -> Vec<(String, u64, u64, u64)> {
    let args: &[&dyn ToSql] = &[&outpoint.to_sql().unwrap()];
    let mut stmt = hord_db_conn
        .prepare("SELECT inscription_id, inscription_number, ordinal_number, offset FROM inscriptions WHERE outpoint_to_watch = ? ORDER BY offset ASC")
        .unwrap();
    let mut results = vec![];
    let mut rows = stmt.query(args).unwrap();
    while let Ok(Some(row)) = rows.next() {
        let inscription_id: String = row.get(0).unwrap();
        let inscription_number: u64 = row.get(1).unwrap();
        let ordinal_number: u64 = row.get(2).unwrap();
        let offset: u64 = row.get(3).unwrap();
        results.push((inscription_id, inscription_number, ordinal_number, offset));
    }
    return results;
}

pub fn insert_entry_in_blocks(
    block_id: u32,
    compacted_block: &CompactedBlock,
    hord_db_conn: &Connection,
    ctx: &Context,
) {
    let serialized_compacted_block = compacted_block.to_hex_bytes();

    if let Err(e) = hord_db_conn.execute(
        "INSERT INTO blocks (id, compacted_bytes) VALUES (?1, ?2)",
        rusqlite::params![&block_id, &serialized_compacted_block],
    ) {
        ctx.try_log(|logger| slog::error!(logger, "{}", e.to_string()));
    }
}

pub fn remove_entry_from_blocks(block_id: u32, hord_db_conn: &Connection, ctx: &Context) {
    if let Err(e) = hord_db_conn.execute(
        "DELETE FROM blocks WHERE id = ?1",
        rusqlite::params![&block_id],
    ) {
        ctx.try_log(|logger| slog::error!(logger, "{}", e.to_string()));
    }
}

pub fn remove_entry_from_inscriptions(
    inscription_id: &str,
    hord_db_conn: &Connection,
    ctx: &Context,
) {
    if let Err(e) = hord_db_conn.execute(
        "DELETE FROM inscriptions WHERE inscription_id = ?1",
        rusqlite::params![&inscription_id],
    ) {
        ctx.try_log(|logger| slog::error!(logger, "{}", e.to_string()));
    }
}

pub async fn update_hord_db(
    bitcoin_config: &BitcoinConfig,
    hord_db_path: &PathBuf,
    hord_db_conn: &Connection,
    start_block: u64,
    end_block: u64,
    _ctx: &Context,
    network_thread: usize,
) -> Result<(), String> {
    let (block_tx, block_rx) = channel::<BitcoinBlockFullBreakdown>();
    let first_inscription_block_height = 767430;
    let ctx = _ctx.clone();
    let network = bitcoin_config.network.clone();
    let hord_db_path = hord_db_path.clone();
    let handle = hiro_system_kit::thread_named("Inscriptions indexing")
        .spawn(move || {
            let mut cursor = first_inscription_block_height;
            let mut inbox = HashMap::new();

            while let Ok(raw_block) = block_rx.recv() {
                // Early return, only considering blocks after 1st inscription
                if raw_block.height < first_inscription_block_height {
                    continue;
                }
                let block_height = raw_block.height;
                inbox.insert(raw_block.height, raw_block);

                // In the context of ordinals, we're constrained to process blocks sequentially
                // Blocks are processed by a threadpool and could be coming out of order.
                // Inbox block for later if the current block is not the one we should be
                // processing.
                if block_height != cursor {
                    continue;
                }

                // Is the action of processing a block allows us
                // to process more blocks present in the inbox?
                while let Some(next_block) = inbox.remove(&cursor) {
                    let mut new_block = match standardize_bitcoin_block(next_block, &network, &ctx)
                    {
                        Ok(block) => block,
                        Err(e) => {
                            ctx.try_log(|logger| {
                                slog::error!(logger, "Unable to standardize bitcoin block: {e}",)
                            });
                            return;
                        }
                    };

                    if let Err(e) = update_hord_db_and_augment_bitcoin_block(
                        &mut new_block,
                        &hord_db_path,
                        &ctx,
                    ) {
                        ctx.try_log(|logger| {
                            slog::error!(
                                logger,
                                "Unable to augment bitcoin block with hord_db: {e}",
                            )
                        });
                        return;
                    }
                    cursor += 1;
                }
            }
        })
        .expect("unable to detach thread");

    fetch_and_cache_blocks_in_hord_db(
        bitcoin_config,
        hord_db_conn,
        start_block,
        end_block,
        &_ctx,
        network_thread,
        Some(block_tx),
    )
    .await?;

    let _ = handle.join();

    Ok(())
}

pub async fn fetch_and_cache_blocks_in_hord_db(
    bitcoin_config: &BitcoinConfig,
    hord_db_conn: &Connection,
    start_block: u64,
    end_block: u64,
    ctx: &Context,
    network_thread: usize,
    block_tx: Option<Sender<BitcoinBlockFullBreakdown>>,
) -> Result<(), String> {
    let retrieve_block_hash_pool = ThreadPool::new(network_thread);
    let (block_hash_tx, block_hash_rx) = crossbeam_channel::unbounded();
    let retrieve_block_data_pool = ThreadPool::new(network_thread);
    let (block_data_tx, block_data_rx) = crossbeam_channel::unbounded();
    let compress_block_data_pool = ThreadPool::new(8);
    let (block_compressed_tx, block_compressed_rx) = crossbeam_channel::unbounded();

    for block_cursor in start_block..end_block {
        let block_height = block_cursor.clone();
        let block_hash_tx = block_hash_tx.clone();
        let config = bitcoin_config.clone();
        let moved_ctx = ctx.clone();
        retrieve_block_hash_pool.execute(move || {
            let future = retrieve_block_hash_with_retry(&block_height, &config, &moved_ctx);
            let block_hash = hiro_system_kit::nestable_block_on(future).unwrap();
            let _ = block_hash_tx.send(Some((block_height, block_hash)));
        })
    }

    let bitcoin_config = bitcoin_config.clone();
    let moved_ctx = ctx.clone();
    let block_data_tx_moved = block_data_tx.clone();
    let _ = hiro_system_kit::thread_named("Block data retrieval")
        .spawn(move || {
            while let Ok(Some((block_height, block_hash))) = block_hash_rx.recv() {
                let moved_bitcoin_config = bitcoin_config.clone();
                let block_data_tx = block_data_tx_moved.clone();
                let moved_ctx = moved_ctx.clone();
                retrieve_block_data_pool.execute(move || {
                    moved_ctx
                        .try_log(|logger| slog::debug!(logger, "Fetching block #{block_height}"));
                    let future = retrieve_full_block_breakdown_with_retry(
                        &block_hash,
                        &moved_bitcoin_config,
                        &moved_ctx,
                    );
                    let block_data = hiro_system_kit::nestable_block_on(future).unwrap();
                    let _ = block_data_tx.send(Some(block_data));
                });
                let res = retrieve_block_data_pool.join();
                res
            }
        })
        .expect("unable to spawn thread");

    let _ = hiro_system_kit::thread_named("Block data compression")
        .spawn(move || {
            while let Ok(Some(block_data)) = block_data_rx.recv() {
                let block_compressed_tx_moved = block_compressed_tx.clone();
                let block_tx = block_tx.clone();
                compress_block_data_pool.execute(move || {
                    let compressed_block = CompactedBlock::from_full_block(&block_data);
                    let block_index = block_data.height as u32;
                    if let Some(block_tx) = block_tx {
                        let _ = block_tx.send(block_data);
                    }
                    let _ = block_compressed_tx_moved.send(Some((block_index, compressed_block)));
                });

                let res = compress_block_data_pool.join();
                res
            }
        })
        .expect("unable to spawn thread");

    let mut blocks_stored = 0;
    while let Ok(Some((block_height, compacted_block))) = block_compressed_rx.recv() {
        ctx.try_log(|logger| slog::info!(logger, "Storing compacted block #{block_height}"));
        insert_entry_in_blocks(block_height, &compacted_block, &hord_db_conn, &ctx);
        blocks_stored += 1;
        if blocks_stored == end_block - start_block {
            let _ = block_data_tx.send(None);
            let _ = block_hash_tx.send(None);
            ctx.try_log(|logger| {
                slog::info!(
                    logger,
                    "Local ordinals storage successfully seeded with #{blocks_stored} blocks"
                )
            });
            return Ok(());
        }
    }

    retrieve_block_hash_pool.join();

    Ok(())
}

pub fn retrieve_satoshi_point_using_local_storage(
    hord_db_conn: &Connection,
    block_identifier: &BlockIdentifier,
    transaction_identifier: &TransactionIdentifier,
    ctx: &Context,
) -> Result<(u64, u64, u64), String> {
    let mut ordinal_offset = 0;
    let mut ordinal_block_number = block_identifier.index as u32;
    let txid = {
        let bytes = hex::decode(&transaction_identifier.hash[2..]).unwrap();
        [bytes[0], bytes[1], bytes[2], bytes[3]]
    };
    let mut tx_cursor = (txid, 0);

    loop {
        let res = match find_compacted_block_at_block_height(ordinal_block_number, &hord_db_conn) {
            Some(res) => res,
            None => {
                return Err(format!("unable to retrieve block ##{ordinal_block_number}"));
            }
        };

        let coinbase_txid = &res.0 .0 .0;
        let txid = tx_cursor.0;

        // ctx.try_log(|logger| {
        //     slog::debug!(
        //         logger,
        //         "{ordinal_block_number}:{:?}:{:?}",
        //         hex::encode(&coinbase_txid),
        //         hex::encode(&txid)
        //     )
        // });

        // to remove
        //std::thread::sleep(std::time::Duration::from_millis(300));

        // evaluate exit condition: did we reach the **final** coinbase transaction
        if coinbase_txid.eq(&txid) {
            let coinbase_value = &res.0 .0 .1;
            if ordinal_offset.lt(coinbase_value) {
                break;
            }

            // loop over the transaction fees to detect the right range
            let cut_off = ordinal_offset - coinbase_value;
            let mut accumulated_fees = 0;
            for (_, inputs, outputs) in res.0 .1 {
                let mut total_in = 0;
                for (_, _, _, input_value) in inputs.iter() {
                    total_in += input_value;
                }

                let mut total_out = 0;
                for output_value in outputs.iter() {
                    total_out += output_value;
                }

                let fee = total_in - total_out;
                accumulated_fees += fee;
                if accumulated_fees > cut_off {
                    // We are looking at the right transaction
                    // Retraverse the inputs to select the index to be picked
                    let mut sats_in = 0;
                    for (txin, block_height, vout, txin_value) in inputs.into_iter() {
                        sats_in += txin_value;
                        if sats_in >= total_out {
                            ordinal_offset = total_out - (sats_in - txin_value);
                            ordinal_block_number = block_height;
                            // println!("{h}: {blockhash} -> {} [in:{} , out: {}] {}/{vout} (input #{in_index}) {compounded_offset}", transaction.txid, transaction.vin.len(), transaction.vout.len(), txid);
                            tx_cursor = (txin, vout as usize);
                            break;
                        }
                    }
                    break;
                }
            }
        } else {
            // isolate the target transaction
            for (txid_n, inputs, outputs) in res.0 .1 {
                // we iterate over the transactions, looking for the transaction target
                if !txid_n.eq(&txid) {
                    continue;
                }

                // ctx.try_log(|logger| {
                //     slog::debug!(logger, "Evaluating {}: {:?}", hex::encode(&txid_n), outputs)
                // });

                let mut sats_out = 0;
                for (index, output_value) in outputs.iter().enumerate() {
                    if index == tx_cursor.1 {
                        break;
                    }
                    // ctx.try_log(|logger| {
                    //     slog::debug!(logger, "Adding {} from output #{}", output_value, index)
                    // });
                    sats_out += output_value;
                }
                sats_out += ordinal_offset;
                // ctx.try_log(|logger| {
                //     slog::debug!(
                //         logger,
                //         "Adding offset {ordinal_offset} to sats_out {sats_out}"
                //     )
                // });

                let mut sats_in = 0;
                for (txin, block_height, vout, txin_value) in inputs.into_iter() {
                    sats_in += txin_value;
                    // ctx.try_log(|logger| {
                    //     slog::debug!(
                    //         logger,
                    //         "Adding txin_value {txin_value} to sats_in {sats_in} (txin: {})",
                    //         hex::encode(&txin)
                    //     )
                    // });

                    if sats_in >= sats_out {
                        ordinal_offset = sats_out - (sats_in - txin_value);
                        ordinal_block_number = block_height;

                        ctx.try_log(|logger| slog::debug!(logger, "Block {ordinal_block_number} / Tx {} / [in:{sats_in}, out:{sats_out}]: {block_height} -> {ordinal_block_number}:{ordinal_offset} -> {}:{vout}",
                        hex::encode(&txid_n),
                        hex::encode(&txin)));
                        tx_cursor = (txin, vout as usize);
                        break;
                    }
                }
            }
        }
    }

    let height = Height(ordinal_block_number.into());
    let ordinal_number = height.starting_sat().0 + ordinal_offset;

    Ok((ordinal_block_number.into(), ordinal_offset, ordinal_number))
}
