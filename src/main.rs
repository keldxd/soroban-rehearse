use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::{fs, thread, time::Duration};

// Blend pool in the backstop's reward zone (Stellar mainnet)
const CONTRACT: &str = "CBAKAZUEJBAFA2DCGBRS2WKJT3FNIMJBHTFL76AY7XMAVEINQNNIQWAO";
// How many of the most recent transactions to keep
const N: usize = 100;
// About one day of ledgers (one ledger every ~5 seconds)
const LEDGERS_PER_DAY: u64 = 17_280;
const CACHE: &str = "cache/txs.json";

fn rpc_url() -> String {
    std::env::var("STELLAR_RPC_URL").unwrap_or_else(|_| "https://mainnet.sorobanrpc.com".to_string())
}

/// One JSON-RPC call. `params` is left out entirely when it is null.
fn rpc(client: &reqwest::blocking::Client, method: &str, params: Value) -> Result<Value> {
    let mut body = json!({"jsonrpc": "2.0", "id": 1, "method": method});
    if !params.is_null() {
        body["params"] = params;
    }
    let resp: Value = client
        .post(rpc_url())
        .json(&body)
        .send()
        .with_context(|| format!("{method}: request failed"))?
        .json()
        .with_context(|| format!("{method}: response was not JSON"))?;
    if let Some(e) = resp.get("error") {
        bail!("{method} returned an error: {e}");
    }
    resp.get("result").cloned().ok_or_else(|| anyhow!("{method}: no result in response"))
}

fn main() -> Result<()> {
    let client = reqwest::blocking::Client::builder().timeout(Duration::from_secs(60)).build()?;

    // What range of ledgers does this RPC still remember?
    let health = rpc(&client, "getHealth", Value::Null)?;
    let latest = health["latestLedger"].as_u64().context("no latestLedger in getHealth")?;
    let oldest = health["oldestLedger"].as_u64().context("no oldestLedger in getHealth")?;
    println!("RPC {} remembers ledgers {oldest}..{latest} ({} ledgers)", rpc_url(), latest - oldest);

    // Look back up to 7 days, but never before the oldest ledger the RPC keeps
    let start = latest.saturating_sub(7 * LEDGERS_PER_DAY).max(oldest + 1);
    println!("scanning events of {CONTRACT} from ledger {start}");

    // Page through the contract's events and collect the transactions that emitted them
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

    // Keep the most recent N
    let mut by_ledger: Vec<(u64, String)> = tx_ledger.into_iter().map(|(h, l)| (l, h)).collect();
    by_ledger.sort();
    let recent: Vec<(u64, String)> = by_ledger.into_iter().rev().take(N).collect();

    // Download each transaction in full (envelope + result meta)
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

    let ledgers: Vec<u64> = saved.iter().filter_map(|t| t["ledger"].as_u64()).collect();
    println!("saved {} transactions to {CACHE}", saved.len());
    println!("ledger range: {:?}..{:?}", ledgers.iter().min(), ledgers.iter().max());
    println!("statuses: {statuses:?}");
    let with_meta = saved.iter().filter(|t| t["resultMetaXdr"].is_string()).count();
    println!("with result meta (needed for before-values): {with_meta}");
    Ok(())
}
