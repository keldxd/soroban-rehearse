use crate::common::{CONTRACT, key_kind, load_txs};
use anyhow::Result;
use soroban_sdk::xdr::{LedgerEntryChange, LedgerKey};
use std::collections::{BTreeMap, BTreeSet};

pub fn inspect() -> Result<()> {
    let (txs, skipped) = load_txs()?;
    let mut calls: BTreeMap<String, usize> = BTreeMap::new();
    let mut key_kinds: BTreeMap<&str, usize> = BTreeMap::new();
    let (mut ro, mut rw, mut auth) = (0usize, 0usize, 0usize);
    let (mut with_before, mut created_n, mut untouched) = (0usize, 0usize, 0usize);

    for t in &txs {
        let target = t.call.contract_address.to_string();
        let func = t.call.function_name.0.to_utf8_string_lossy();
        let label = if target == CONTRACT {
            format!("{func} (direct)")
        } else {
            format!("{func} on {target}")
        };
        *calls.entry(label).or_insert(0) += 1;

        ro += t.read_only.len();
        rw += t.read_write.len();
        auth += t.auth_entries;
        for k in t.read_only.iter().chain(t.read_write.iter()) {
            *key_kinds.entry(key_kind(k)).or_insert(0) += 1;
        }

        let mut before: BTreeSet<LedgerKey> = BTreeSet::new();
        let mut created: BTreeSet<LedgerKey> = BTreeSet::new();
        for c in &t.changes {
            match c {
                LedgerEntryChange::State(e) => {
                    before.insert(e.to_key());
                }
                LedgerEntryChange::Created(e) => {
                    created.insert(e.to_key());
                }
                _ => {}
            }
        }
        for k in &t.read_write {
            if before.contains(k) {
                with_before += 1;
            } else if created.contains(k) {
                created_n += 1;
            } else {
                untouched += 1;
            }
        }
    }

    let n = txs.len().max(1) as f64;
    println!("==== inspect ====");
    println!("decoded: {}, skipped: {skipped:?}", txs.len());
    println!("\ncalls (function and target):");
    for (k, v) in &calls {
        println!("  {v:>3}  {k}");
    }
    println!(
        "\nfootprint per tx (avg): {:.1} read-only, {:.1} read-write",
        ro as f64 / n,
        rw as f64 / n
    );
    println!("footprint entry kinds: {key_kinds:?}");
    println!("auth entries per tx (avg): {:.1}", auth as f64 / n);
    println!(
        "read-write entries: {with_before} have a before-value, {created_n} newly created, {untouched} no recorded change"
    );
    Ok(())
}
