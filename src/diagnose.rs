use crate::common::load_txs;
use crate::replay::render::{describe_key, event, short, sv, value_text};
use crate::replay::run_safely;
use crate::state::{History, current_entries, recorded_after, state_before};
use anyhow::{Result, bail};
use soroban_sdk::xdr::{LedgerEntryChange, LedgerKey};
use std::collections::BTreeSet;

pub fn diagnose(hash: &str) -> Result<()> {
    let (txs, _) = load_txs()?;
    let Some(tx) = txs.iter().find(|t| t.hash.starts_with(hash)) else {
        bail!("no cached transaction starts with {hash}");
    };
    let current = current_entries(&txs)?;
    let history = History::build(&txs);
    let (entries, src) = state_before(tx, &current, &history);

    println!("tx {}", tx.hash);
    println!(
        "ledger {}, call {} on {}, {} args, footprint {} read-only + {} read-write",
        tx.ledger,
        tx.call.function_name.0.to_utf8_string_lossy(),
        tx.call.contract_address,
        tx.call.args.len(),
        tx.read_only.len(),
        tx.read_write.len()
    );
    println!(
        "state: own before-values {}, other transactions {}, current RPC {}, absent {}, missing {}",
        src.own, src.history, src.current, src.absent, src.missing
    );

    // Footprint entries that got no value in the rebuilt state
    let have: BTreeSet<&LedgerKey> = entries.iter().map(|(k, _, _)| k).collect();
    let created: BTreeSet<LedgerKey> = tx
        .changes
        .iter()
        .filter_map(|c| match c {
            LedgerEntryChange::Created(e) => Some(e.to_key()),
            _ => None,
        })
        .collect();
    println!("\nfootprint entries absent from the rebuilt state:");
    for k in tx.read_only.iter().chain(tx.read_write.iter()) {
        if !have.contains(k) {
            let rw = if tx.read_write.contains(k) {
                "read-write"
            } else {
                "read-only"
            };
            let why = if created.contains(k) {
                "created by this tx"
            } else {
                "no value found"
            };
            println!("  [{rw}, {why}] {}", describe_key(k));
        }
    }

    // Disable the default panic message; run_safely reports it
    std::panic::set_hook(Box::new(|_| {}));
    let out = match run_safely(tx, &entries, None) {
        Ok(o) => o,
        Err(e) => {
            println!("\nreplay could not run: {e}");
            return Ok(());
        }
    };

    println!("\nreplay: {}", if out.ok { "ok" } else { "FAILED" });
    if let Some(e) = &out.error {
        println!("error: {e}");
    }
    let show = |v: &Option<soroban_sdk::xdr::ScVal>| {
        v.as_ref().map(sv).unwrap_or_else(|| "none".to_string())
    };
    println!(
        "return value  replay:   {}",
        short(show(&out.return_value), 300)
    );
    println!(
        "return value  recorded: {}",
        short(show(&tx.return_value), 300)
    );

    println!(
        "\nhost error diagnostics ({}), first ones:",
        out.diagnostics.len()
    );
    for d in out.diagnostics.iter().take(8) {
        println!("  {}", short(d.clone(), 400));
    }

    println!("\nauthorizations required ({}):", out.auths.len());
    for a in out.auths.iter().take(8) {
        println!("  {}", a.full);
    }

    println!(
        "\nevents: {} in replay, {} recorded",
        out.events.len(),
        tx.events.len()
    );
    let first_diff = (0..out.events.len().max(tx.events.len()))
        .find(|i| out.events.get(*i) != tx.events.get(*i));
    if let Some(i) = first_diff {
        println!("first different event is #{i}");
        println!(
            "  replay:   {}",
            out.events
                .get(i)
                .map(event)
                .unwrap_or_else(|| "none".to_string())
        );
        println!(
            "  recorded: {}",
            tx.events
                .get(i)
                .map(event)
                .unwrap_or_else(|| "none".to_string())
        );
    }

    let expected = recorded_after(tx, &entries);
    println!("\nstorage writes that differ:");
    for (k, v) in &expected {
        let got = out.after.get(k);
        if got != v.as_ref() {
            println!("  {}", describe_key(k));
            println!(
                "    replay:   {}",
                got.map(value_text).unwrap_or_else(|| "absent".to_string())
            );
            println!(
                "    recorded: {}",
                v.as_ref()
                    .map(value_text)
                    .unwrap_or_else(|| "absent".to_string())
            );
        }
    }
    Ok(())
}
