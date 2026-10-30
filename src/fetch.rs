use crate::common::{client, rpc, rpc_url, CACHE, CONTRACT, LEDGERS_PER_DAY, N};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::{fs, thread, time::Duration};

pub fn fetch() -> Result<()> {
    let client = client()?;

    let health = rpc(&client, "getHealth", Value::Null)?;
    let latest = health["latestLedger"].as_u64().context("no latestLedger in getHealth")?;
    let oldest = health["oldestLedger"].as_u64().context("no oldestLedger in getHealth")?;
    println!("RPC {} remembers ledgers {oldest}..{latest} ({} ledgers)", rpc_url(), latest - oldest);

    let start = latest.saturating_sub(7 * LEDGERS_PER_DAY).max(oldest + 1);
    println!("scanning events of {CONTRACT} from ledger {start}");

    let filters = json!([{"type": "contract", "contractIds": [CONTRACT]}]);
    let mut tx_ledger: BTreeMap<String, u64> = BTreeMap::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let params = match &cursor {
            None => json!({"startLedger": start, "filters": filters, "pagination": {"limit": 100}}),
            Some(c) => json!({"filters": filters, "pagination": {"cursor": c, "limit": 100}}),
        };
        let res = rpc(&client, "getEvents", params)?;
        let events = res["events"].as_array().cloned().unwrap_or_default();
        pages += 1;
        if events.is_empty() {
            break;
        }
        for e in &events {
            if let (Some(h), Some(l)) = (e["txHash"].as_str(), e["ledger"].as_u64()) {
                tx_ledger.insert(h.to_string(), l);
            }
        }
        cursor = res["cursor"].as_str().map(str::to_string);
        if cursor.is_none() || pages > 500 {
            break;
        }
        thread::sleep(Duration::from_millis(150));
    }
    println!("found {} transactions with events from this contract ({pages} pages)", tx_ledger.len());
    if tx_ledger.is_empty() {
        bail!("no transactions found; try another RPC with STELLAR_RPC_URL");
    }

    let mut by_ledger: Vec<(u64, String)> = tx_ledger.into_iter().map(|(h, l)| (l, h)).collect();
    by_ledger.sort();
    let recent: Vec<(u64, String)> = by_ledger.into_iter().rev().take(N).collect();

    let mut saved = Vec::new();
    let mut statuses: BTreeMap<String, usize> = BTreeMap::new();
    for (i, (_, hash)) in recent.iter().enumerate() {
        match rpc(&client, "getTransaction", json!({"hash": hash})) {
            Ok(tx) => {
                let status = tx["status"].as_str().unwrap_or("?").to_string();
                *statuses.entry(status).or_insert(0) += 1;
                let mut tx = tx;
                tx["txHash"] = json!(hash);
                saved.push(tx);
            }
            Err(e) => eprintln!("skipping {hash}: {e}"),
        }
        if (i + 1) % 20 == 0 {
            println!("  downloaded {}/{}", i + 1, recent.len());
        }
        thread::sleep(Duration::from_millis(150));
    }

    fs::create_dir_all("cache")?;
    fs::write(CACHE, serde_json::to_string_pretty(&saved)?)?;
    // New transactions need fresh current entries
    let _ = fs::remove_file(crate::common::ENTRIES_CACHE);

    let ledgers: Vec<u64> = saved.iter().filter_map(|t| t["ledger"].as_u64()).collect();
    println!("saved {} transactions to {CACHE}", saved.len());
    println!("ledger range: {:?}..{:?}", ledgers.iter().min(), ledgers.iter().max());
    println!("statuses: {statuses:?}");
    Ok(())
}
