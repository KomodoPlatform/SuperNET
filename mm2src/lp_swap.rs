//! Atomic swap loops and states
//! 
//! # A note on the terminology used
//! 
//! Alice = Buyer = Liquidity receiver = Taker  
//! ("*The process of an atomic swap begins with the person who makes the initial request — this is the liquidity receiver*" - Komodo Whitepaper).
//! 
//! Bob = Seller = Liquidity provider = Market maker  
//! ("*On the other side of the atomic swap, we have the liquidity provider — we call this person, Bob*" - Komodo Whitepaper).
//! 
//! # Algorithm updates
//! 
//! At the end of 2018 most UTXO coins have BIP65 (https://github.com/bitcoin/bips/blob/master/bip-0065.mediawiki).
//! The previous swap protocol discussions took place at 2015-2016 when there were just a few
//! projects that implemented CLTV opcode support:
//! https://bitcointalk.org/index.php?topic=1340621.msg13828271#msg13828271
//! https://bitcointalk.org/index.php?topic=1364951
//! So the Tier Nolan approach is a bit outdated, the main purpose was to allow swapping of a coin
//! that doesn't have CLTV at least as Alice side (as APayment is 2of2 multisig).
//! Nowadays the protocol can be simplified to the following (UTXO coins, BTC and forks):
//! 
//! 1. AFee: OP_DUP OP_HASH160 FEE_RMD160 OP_EQUALVERIFY OP_CHECKSIG
//!
//! 2. BPayment:
//! OP_IF
//! <now + LOCKTIME*2> OP_CLTV OP_DROP <bob_pub> OP_CHECKSIG
//! OP_ELSE
//! OP_SIZE 32 OP_EQUALVERIFY OP_HASH160 <hash(bob_privN)> OP_EQUALVERIFY <alice_pub> OP_CHECKSIG
//! OP_ENDIF
//! 
//! 3. APayment:
//! OP_IF
//! <now + LOCKTIME> OP_CLTV OP_DROP <alice_pub> OP_CHECKSIG
//! OP_ELSE
//! OP_SIZE 32 OP_EQUALVERIFY OP_HASH160 <hash(bob_privN)> OP_EQUALVERIFY <bob_pub> OP_CHECKSIG
//! OP_ENDIF
//! 

/******************************************************************************
 * Copyright © 2014-2018 The SuperNET Developers.                             *
 *                                                                            *
 * See the AUTHORS, DEVELOPER-AGREEMENT and LICENSE files at                  *
 * the top-level directory of this distribution for the individual copyright  *
 * holder information and the developer policies on copyright and licensing.  *
 *                                                                            *
 * Unless otherwise agreed in a custom licensing agreement, no part of the    *
 * SuperNET software, including this file may be copied, modified, propagated *
 * or distributed except according to the terms contained in the LICENSE file *
 *                                                                            *
 * Removal or modification of this copyright notice is prohibited.            *
 *                                                                            *
 ******************************************************************************/
//
//  lp_swap.rs
//  marketmaker
//
use bigdecimal::BigDecimal;
use futures03::executor::block_on;
use rpc::v1::types::{H160 as H160Json, H256 as H256Json, H264 as H264Json};
use coins::{lp_coinfind, MmCoinEnum, TradeInfo, TransactionDetails};
use common::{bits256, HyRes, rpc_response};
use common::wio::Timeout;
use common::log::{TagParam};
use common::mm_ctx::{from_ctx, MmArc};
use futures::{Future};
use gstuff::{now_ms, slurp};
use hashbrown::{HashSet, HashMap};
use hashbrown::hash_map::Entry;
use http::Response;
use primitives::hash::{H160, H264};
use serde_json::{self as json, Value as Json};
use serialization::{deserialize, serialize};
use std::ffi::OsStr;
use std::fs::{File, DirEntry};
use std::io::prelude::*;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

// NB: Using a macro instead of a function in order to preserve the line numbers in the log.
macro_rules! send {
    ($ctx: expr, $to: expr, $subj: expr, $fallback: expr, $payload: expr) => {{
        // Checksum here helps us visually verify the logistics between the Maker and Taker logs.
        let crc = crc32::checksum_ieee (&$payload);
        log!("Sending '" ($subj) "' (" ($payload.len()) " bytes, crc " (crc) ")");

        peers::send ($ctx, $to, $subj.as_bytes(), $fallback, $payload.into())
    }}
}

macro_rules! recv_ {
    ($swap: expr, $subj: expr, $timeout_sec: expr, $ec: expr, $validator: block) => {{
        let recv_subject = fomat! (($subj) '@' ($swap.uuid));
        let validator = Box::new ($validator) as Box<dyn Fn(&[u8]) -> Result<(), String> + Send>;
        let fallback = ($timeout_sec / 3) .min (30) .max (60) as u8;
        let recv_f = peers::recv (&$swap.ctx, recv_subject.as_bytes(), fallback, Box::new ({
            // NB: `peers::recv` is generic and not responsible for handling errors.
            //     Here, on the other hand, we should know enough to log the errors.
            //     Also through the macros the logging statements will carry informative line numbers on them.
            move |payload: &[u8]| -> bool {
                match validator (payload) {
                    Ok (()) => true,
                    Err (err) => {
                        log! ("Error validating payload '" ($subj) "' (" (payload.len()) " bytes, crc " (crc32::checksum_ieee (payload)) "): " (err) ". Retrying…");
                        false
                    }
                }
            }
        }));
        let recv_f = Timeout::new (recv_f, Duration::from_secs (BASIC_COMM_TIMEOUT + $timeout_sec));
        recv_f.wait().map(|payload| {
            // Checksum here helps us visually verify the logistics between the Maker and Taker logs.
            let crc = crc32::checksum_ieee (&payload);
            log! ("Received '" (recv_subject) "' (" (payload.len()) " bytes, crc " (crc) ")");
            payload
        })
    }}
}

macro_rules! recv {
    ($selff: ident, $subj: expr, $timeout_sec: expr, $ec: expr, $validator: block) => {
        recv_! ($selff, $subj, $timeout_sec, $ec, $validator)
    };
    // Use this form if there's a sending future to terminate upon receiving the answer.
    ($selff: ident, $sending_f: ident, $subj: expr, $timeout_sec: expr, $ec: expr, $validator: block) => {{
        let payload = recv_! ($selff, $subj, $timeout_sec, $ec, $validator);
        drop ($sending_f);
        payload
    }};
}

#[path = "lp_swap/maker_swap.rs"]
mod maker_swap;
#[path = "lp_swap/taker_swap.rs"]
mod taker_swap;

use maker_swap::{MakerSavedSwap, stats_maker_swap_file_path};
use taker_swap::{TakerSavedSwap, stats_taker_swap_file_path};
pub use maker_swap::{MakerSwap, run_maker_swap};
pub use taker_swap::{TakerSwap, run_taker_swap};

/// Includes the grace time we add to the "normal" timeouts
/// in order to give different and/or heavy communication channels a chance.
const BASIC_COMM_TIMEOUT: u64 = 90;

/// Default atomic swap payment locktime, in seconds.
/// Maker sends payment with LOCKTIME * 2
/// Taker sends payment with LOCKTIME
const PAYMENT_LOCKTIME: u64 = 3600 * 2 + 300 * 2;
const _SWAP_DEFAULT_NUM_CONFIRMS: u32 = 1;
const _SWAP_DEFAULT_MAX_CONFIRMS: u32 = 6;

/// Represents the amount of a coin locked by ongoing swap
struct LockedAmount {
    coin: String,
    amount: BigDecimal,
}

struct SwapsContext {
    locked_amounts: Mutex<HashMap<String, LockedAmount>>,
}

impl SwapsContext {
    /// Obtains a reference to this crate context, creating it if necessary.
    fn from_ctx (ctx: &MmArc) -> Result<Arc<SwapsContext>, String> {
        Ok (try_s! (from_ctx (&ctx.swaps_ctx, move || {
            Ok (SwapsContext {
                locked_amounts: Mutex::new(HashMap::new()),
            })
        })))
    }
}

/// Virtually locks the amount of a coin, called when swap is instantiated
fn lock_amount(ctx: &MmArc, uuid: String, coin: String, amount: BigDecimal) {
    let swap_ctx = unwrap!(SwapsContext::from_ctx(&ctx));
    let mut locked = unwrap!(swap_ctx.locked_amounts.lock());
    locked.insert(uuid, LockedAmount {
        coin,
        amount,
    });
}

/// Virtually unlocks the amount of a coin, called when swap transaction is sent so the real balance
/// is updated and virtual lock is not required.
fn unlock_amount(ctx: &MmArc, uuid: &str, amount: &BigDecimal) {
    let swap_ctx = unwrap!(SwapsContext::from_ctx(&ctx));
    let mut locked = unwrap!(swap_ctx.locked_amounts.lock());
    match locked.entry(uuid.into()) {
        Entry::Occupied(mut e) => {
            let entry = e.get_mut();
            if &entry.amount <= amount {
                e.remove();
            } else {
                entry.amount -= amount;
            };
        },
        Entry::Vacant(_) => (),
    };
}

/// Get total amount of selected coin locked by all currently ongoing swaps
pub fn get_locked_amount(ctx: &MmArc, coin: &str) -> BigDecimal {
    let swap_ctx = unwrap!(SwapsContext::from_ctx(&ctx));
    let locked = unwrap!(swap_ctx.locked_amounts.lock());
    locked.iter().fold(
        0.into(),
        |total, (_, locked)| if locked.coin == coin {
            total + &locked.amount
        } else {
            total
        }
    )
}

/// Get total amount of selected coin locked by all currently ongoing swaps except the one with selected uuid
fn get_locked_amount_by_other_swaps(ctx: &MmArc, except_uuid: &str, coin: &str) -> BigDecimal {
    let swap_ctx = unwrap!(SwapsContext::from_ctx(&ctx));
    let locked = unwrap!(swap_ctx.locked_amounts.lock());
    locked.iter().fold(
        0.into(),
        |total, (uuid, locked)| if uuid != except_uuid && locked.coin == coin {
            total + &locked.amount
        } else {
            total
        }
    )
}

/// Some coins are "slow" (block time is high - e.g. BTC average block time is ~10 minutes).
/// https://bitinfocharts.com/comparison/bitcoin-confirmationtime.html
/// We need to increase payment locktime accordingly when at least 1 side of swap uses "slow" coin.
fn lp_atomic_locktime(base: &str, rel: &str) -> u64 {
    if base == "BTC" || rel == "BTC" {
        PAYMENT_LOCKTIME * 10
    } else if base == "BCH" || rel == "BCH" || base == "BTG" || rel == "BTG" || base == "SBTC" || rel == "SBTC" {
        PAYMENT_LOCKTIME * 4
    } else {
        PAYMENT_LOCKTIME
    }
}

fn payment_confirmations(_maker_coin: &MmCoinEnum, _taker_coin: &MmCoinEnum) -> (u32, u32) {
    /*
    let mut maker_confirmations = SWAP_DEFAULT_NUM_CONFIRMS;
    let mut taker_confirmations = SWAP_DEFAULT_NUM_CONFIRMS;
    if maker_coin.ticker() == "BTC" {
        maker_confirmations = 1;
    }

    if taker_coin.ticker() == "BTC" {
        taker_confirmations = 1;
    }

    if maker_coin.is_asset_chain() {
        maker_confirmations = 1;
    }

    if taker_coin.is_asset_chain() {
        taker_confirmations = 1;
    }
    */

    // TODO recognize why the BAY case is special, ask JL777
    /*
        if ( strcmp("BAY",swap->I.req.src) != 0 && strcmp("BAY",swap->I.req.dest) != 0 )
    {
        swap->I.bobconfirms *= !swap->I.bobistrusted;
        swap->I.aliceconfirms *= !swap->I.aliceistrusted;
    }
    */
    (1, 1)
}

fn dex_fee_rate(base: &str, rel: &str) -> BigDecimal {
    if base == "KMD" || rel == "KMD" {
        // 1/777 - 10%
        BigDecimal::from(9) / BigDecimal::from(7770)
    } else {
        BigDecimal::from(1) / BigDecimal::from(777)
    }
}

pub fn dex_fee_amount(base: &str, rel: &str, trade_amount: &BigDecimal) -> BigDecimal {
    let rate = dex_fee_rate(base, rel);
    let min_fee = unwrap!("0.0001".parse());
    let fee_amount = trade_amount * rate;
    if fee_amount < min_fee {
        min_fee
    } else {
        fee_amount
    }
}

// NB: Using a macro instead of a function in order to preserve the line numbers in the log.
macro_rules! send {
    ($ctx: expr, $to: expr, $subj: expr, $fallback: expr, $payload: expr) => {{
        // Checksum here helps us visually verify the logistics between the Maker and Taker logs.
        let crc = crc32::checksum_ieee (&$payload);
        log!("Sending '" ($subj) "' (" ($payload.len()) " bytes, crc " (crc) ")");

        block_on (peers::send ($ctx.clone(), $to, Vec::from ($subj.as_bytes()), $fallback, $payload.into()))
    }}
}

macro_rules! recv_ {
    ($swap: expr, $subj: expr, $timeout_sec: expr, $ec: expr, $validator: block) => {{
        let recv_subject = fomat! (($subj) '@' ($swap.uuid));
        let validator = Box::new ($validator) as Box<dyn Fn(&[u8]) -> Result<(), String> + Send>;
        let fallback = ($timeout_sec / 3) .min (30) .max (60) as u8;
        let recv_f = peers::recv (&$swap.ctx, recv_subject.as_bytes(), fallback, Box::new ({
            // NB: `peers::recv` is generic and not responsible for handling errors.
            //     Here, on the other hand, we should know enough to log the errors.
            //     Also through the macros the logging statements will carry informative line numbers on them.
            move |payload: &[u8]| -> bool {
                match validator (payload) {
                    Ok (()) => true,
                    Err (err) => {
                        log! ("Error validating payload '" ($subj) "' (" (payload.len()) " bytes, crc " (crc32::checksum_ieee (payload)) "): " (err) ". Retrying…");
                        false
                    }
                }
            }
        }));
        let recv_f = Timeout::new (recv_f, Duration::from_secs (BASIC_COMM_TIMEOUT + $timeout_sec));
        recv_f.wait().map(|payload| {
            // Checksum here helps us visually verify the logistics between the Maker and Taker logs.
            let crc = crc32::checksum_ieee (&payload);
            log! ("Received '" (recv_subject) "' (" (payload.len()) " bytes, crc " (crc) ")");
            payload
        })
    }}
}

/// Data to be exchanged and validated on swap start, the replacement of LP_pubkeys_data, LP_choosei_data, etc.
#[derive(Debug, Default, Deserializable, Eq, PartialEq, Serializable)]
struct SwapNegotiationData {
    started_at: u64,
    payment_locktime: u64,
    secret_hash: H160,
    persistent_pubkey: H264,
}

fn my_swaps_dir(ctx: &MmArc) -> PathBuf {
    ctx.dbdir().join("SWAPS").join("MY")
}

fn my_swap_file_path(ctx: &MmArc, uuid: &str) -> PathBuf {
    my_swaps_dir(ctx).join(format!("{}.json", uuid))
}

fn save_stats_swap(ctx: &MmArc, swap: &SavedSwap) -> Result<(), String> {
    let (path, content) = match &swap {
        SavedSwap::Maker(maker_swap) => (stats_maker_swap_file_path(ctx, &maker_swap.uuid), try_s!(json::to_vec(&maker_swap))),
        SavedSwap::Taker(taker_swap) => (stats_taker_swap_file_path(ctx, &taker_swap.uuid), try_s!(json::to_vec(&taker_swap))),
    };
    let mut file = try_s!(File::create(path));
    try_s!(file.write_all(&content));
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
enum SavedSwap {
    Maker(MakerSavedSwap),
    Taker(TakerSavedSwap),
}

/// The helper structure that makes easier to parse the response for GUI devs
/// They won't have to parse the events themselves handling possible errors, index out of bounds etc.
#[derive(Debug, Serialize, Deserialize)]
pub struct MySwapInfo {
    my_coin: String,
    other_coin: String,
    my_amount: BigDecimal,
    other_amount: BigDecimal,
    started_at: u64,
}

impl SavedSwap {
    fn is_finished(&self) -> bool {
        match self {
            SavedSwap::Maker(swap) => swap.is_finished(),
            SavedSwap::Taker(swap) => swap.is_finished(),
        }
    }

    fn uuid(&self) -> &str {
        match self {
            SavedSwap::Maker(swap) => &swap.uuid,
            SavedSwap::Taker(swap) => &swap.uuid,
        }
    }

    fn maker_coin_ticker(&self) -> Result<String, String> {
        match self {
            SavedSwap::Maker(swap) => swap.maker_coin(),
            SavedSwap::Taker(swap) => swap.maker_coin(),
        }
    }

    fn taker_coin_ticker(&self) -> Result<String, String> {
        match self {
            SavedSwap::Maker(swap) => swap.taker_coin(),
            SavedSwap::Taker(swap) => swap.taker_coin(),
        }
    }

    fn get_my_info(&self) -> Option<MySwapInfo> {
        match self {
            SavedSwap::Maker(swap) => swap.get_my_info(),
            SavedSwap::Taker(swap) => swap.get_my_info(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct SwapError {
    error: String,
}

impl Into<SwapError> for String {
    fn into(self) -> SwapError {
        SwapError {
            error: self
        }
    }
}

/// Returns the status of swap performed on `my` node
pub fn my_swap_status(ctx: MmArc, req: Json) -> HyRes {
    let uuid = try_h!(req["params"]["uuid"].as_str().ok_or("uuid parameter is not set or is not string"));
    let path = my_swap_file_path(&ctx, uuid);
    let content = slurp(&path);
    if content.is_empty() {
        return rpc_response(404, json!({
            "error": "swap data is not found"
        }).to_string());
    }
    let status: SavedSwap = try_h!(json::from_slice(&content));
    let my_info = status.get_my_info();
    let mut json = try_h!(json::to_value(status));
    json["my_info"] = try_h!(json::to_value(my_info));

    rpc_response(200, json!({
        "result": json
    }).to_string())
}

/// Returns the status of requested swap, typically performed by other nodes and saved by `save_stats_swap_status`
pub fn stats_swap_status(ctx: MmArc, req: Json) -> HyRes {
    let uuid = try_h!(req["params"]["uuid"].as_str().ok_or("uuid parameter is not set or is not string"));
    let maker_path = stats_maker_swap_file_path(&ctx, uuid);
    let taker_path = stats_taker_swap_file_path(&ctx, uuid);
    let maker_content = slurp(&maker_path);
    let taker_content = slurp(&taker_path);
    let maker_status: Option<MakerSavedSwap> = if maker_content.is_empty() {
        None
    } else {
        Some(try_h!(json::from_slice(&maker_content)))
    };

    let taker_status: Option<TakerSavedSwap> = if taker_content.is_empty() {
        None
    } else {
        Some(try_h!(json::from_slice(&taker_content)))
    };

    if maker_status.is_none() && taker_status.is_none() {
        return rpc_response(404, json!({
            "error": "swap data is not found"
        }).to_string());
    }

    rpc_response(200, json!({
        "result": {
            "maker": maker_status,
            "taker": taker_status,
        }
    }).to_string())
}

/// Broadcasts `my` swap status to P2P network
fn broadcast_my_swap_status(uuid: &str, ctx: &MmArc) -> Result<(), String> {
    let path = my_swap_file_path(ctx, uuid);
    let content = slurp(&path);
    let mut status: SavedSwap = try_s!(json::from_slice(&content));
    match &mut status {
        SavedSwap::Taker(_) => (), // do nothing for taker
        SavedSwap::Maker(ref mut swap) => swap.hide_secret(),
    };
    try_s!(save_stats_swap(ctx, &status));
    let status_string = json!({
        "method": "swapstatus",
        "data": status,
    }).to_string();
    ctx.broadcast_p2p_msg(&status_string);
    Ok(())
}

/// Saves the swap status notification received from P2P network to local DB.
pub fn save_stats_swap_status(ctx: &MmArc, data: Json) -> HyRes {
    let swap: SavedSwap = try_h!(json::from_value(data));
    try_h!(save_stats_swap(ctx, &swap));
    rpc_response(200, json!({
        "result": "success"
    }).to_string())
}

/// Returns the data of recent swaps of `my` node. Returns no more than `limit` records (default: 10).
/// Skips the first `skip` records (default: 0).
pub fn my_recent_swaps(ctx: MmArc, req: Json) -> HyRes {
    let limit = req["limit"].as_u64().unwrap_or(10);
    let from_uuid = req["from_uuid"].as_str();
    let mut entries: Vec<(SystemTime, DirEntry)> = try_h!(my_swaps_dir(&ctx).read_dir()).filter_map(|dir_entry| {
        let entry = match dir_entry {
            Ok(ent) => ent,
            Err(e) => {
                log!("Error " (e) " reading from dir " (my_swaps_dir(&ctx).display()));
                return None;
            }
        };

        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(e) => {
                log!("Error " (e) " getting file " (entry.path().display()) " meta");
                return None;
            }
        };

        let m_time = match metadata.modified() {
            Ok(time) => time,
            Err(e) => {
                log!("Error " (e) " getting file " (entry.path().display()) " m_time");
                return None;
            }
        };

        if entry.path().extension() == Some(OsStr::new("json")) {
            Some((m_time, entry))
        } else {
            None
        }
    }).collect();
    // sort by m_time in descending order
    entries.sort_by(|(a, _), (b, _)| b.cmp(&a));

    let skip = match from_uuid {
        Some(uuid) => try_h!(entries.iter().position(|(_, entry)| entry.path() == my_swap_file_path(&ctx, uuid)).ok_or(format!("from_uuid {} swap is not found", uuid))) + 1,
        None => 0,
    };

    // iterate over file entries trying to parse the file contents and add to result vector
    let swaps: Vec<Json> = entries.iter().skip(skip).take(limit as usize).map(|(_, entry)|
        match json::from_slice::<SavedSwap>(&slurp(&entry.path())) {
            Ok(swap) => {
                let my_info = swap.get_my_info();
                let mut json = unwrap!(json::to_value(swap));
                json["my_info"] = unwrap!(json::to_value(my_info));
                json
            },
            Err(e) => {
                log!("Error " (e) " parsing JSON from " (entry.path().display()));
                Json::Null
            },
        },
    ).collect();

    rpc_response(200, json!({
        "result": {
            "swaps": swaps,
            "from_uuid": from_uuid,
            "skipped": skip,
            "limit": limit,
            "total": entries.len(),
        },
    }).to_string())
}

/// Find out the swaps that need to be kick-started, continue from the point where swap was interrupted
/// Return the tickers of coins that must be enabled for swaps to continue
pub fn swap_kick_starts(ctx: MmArc) -> HashSet<String> {
    let mut coins = HashSet::new();
    let entries: Vec<DirEntry> = unwrap!(my_swaps_dir(&ctx).read_dir()).filter_map(|dir_entry| {
        let entry = match dir_entry {
            Ok(ent) => ent,
            Err(e) => {
                log!("Error " (e) " reading from dir " (my_swaps_dir(&ctx).display()));
                return None;
            }
        };

        if entry.path().extension() == Some(OsStr::new("json")) {
            Some(entry)
        } else {
            None
        }
    }).collect();

    entries.iter().for_each(|entry| {
        match json::from_slice::<SavedSwap>(&slurp(&entry.path())) {
            Ok(swap) => {
                if !swap.is_finished() {
                    log!("Kick starting the swap " [swap.uuid()]);
                    match swap.maker_coin_ticker() {
                        Ok(t) => coins.insert(t),
                        Err(e) => {
                            log!("Error " (e) " getting maker coin of swap " (swap.uuid()));
                            return;
                        }
                    };
                    match swap.taker_coin_ticker() {
                        Ok(t) => coins.insert(t),
                        Err(e) => {
                            log!("Error " (e) " getting taker coin of swap " (swap.uuid()));
                            return;
                        }
                    };
                    thread::spawn({
                        let ctx = ctx.clone();
                        move ||
                            match swap {
                                SavedSwap::Maker(swap) => match MakerSwap::load_from_saved(ctx, swap) {
                                    Ok((maker, command)) => run_maker_swap(maker, command),
                                    Err(e) => log!([e]),
                                },
                                SavedSwap::Taker(swap) => match TakerSwap::load_from_saved(ctx, swap) {
                                    Ok((taker, command)) => run_taker_swap(taker, command),
                                    Err(e) => log!([e]),
                                },
                            }
                    });
                }
            },
            Err(_) => (),
        }
    });
    coins
}

pub async fn coins_needed_for_kick_start(ctx: MmArc) -> Result<Response<Vec<u8>>, String> {
    let res = try_s!(json::to_vec(&json!({
        "result": *(try_s!(ctx.coins_needed_for_kick_start.lock()))
    })));
    Ok(try_s!(Response::builder().body(res)))
}

#[cfg(test)]
mod lp_swap_tests {
    use super::*;

    #[test]
    fn test_dex_fee_amount() {
        let base = "BTC";
        let rel = "ETH";
        let amount = 1.into();
        let actual_fee = dex_fee_amount(base, rel, &amount);
        let expected_fee = amount / 777;
        assert_eq!(expected_fee, actual_fee);

        let base = "KMD";
        let rel = "ETH";
        let amount = 1.into();
        let actual_fee = dex_fee_amount(base, rel, &amount);
        let expected_fee = amount * BigDecimal::from(9) / 7770;
        assert_eq!(expected_fee, actual_fee);

        let base = "BTC";
        let rel = "KMD";
        let amount = 1.into();
        let actual_fee = dex_fee_amount(base, rel, &amount);
        let expected_fee = amount * BigDecimal::from(9) / 7770;
        assert_eq!(expected_fee, actual_fee);

        let base = "BTC";
        let rel = "KMD";
        let amount = unwrap!("0.001".parse());
        let actual_fee = dex_fee_amount(base, rel, &amount);
        let expected_fee: BigDecimal = unwrap!("0.0001".parse());
        assert_eq!(expected_fee, actual_fee);
    }

    #[test]
    fn test_serde_swap_negotiation_data() {
        let data = SwapNegotiationData::default();
        let bytes = serialize(&data);
        let deserialized = unwrap!(deserialize(bytes.as_slice()));
        assert_eq!(data, deserialized);
    }
}
