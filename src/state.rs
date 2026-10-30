use crate::common::{DecodedTx, ENTRIES_CACHE, client, decode, encode, rpc};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use soroban_sdk::xdr::{
    LedgerEntry, LedgerEntryChange, LedgerEntryData, LedgerEntryExt, LedgerKey,
};
use std::collections::{BTreeMap, BTreeSet};
use std::{fs, path::Path, thread, time::Duration};

pub struct CurrentEntry {
    pub entry: LedgerEntry,
    pub live_until: Option<u32>,
}

/// Current value of every key any transaction touches, from RPC (cached in cache/entries.json
/// so every run uses identical state; delete that file to refresh).
pub fn current_entries(txs: &[DecodedTx]) -> Result<BTreeMap<LedgerKey, CurrentEntry>> {
    let raw: Vec<Value> = if Path::new(ENTRIES_CACHE).exists() {
        println!("using saved entries from {ENTRIES_CACHE} (delete it to refresh)");
        serde_json::from_str(&fs::read_to_string(ENTRIES_CACHE)?)?
    } else {
        let mut keys: BTreeSet<LedgerKey> = BTreeSet::new();
        for t in txs {
            keys.extend(t.read_only.iter().cloned());
            keys.extend(t.read_write.iter().cloned());
        }
        let encoded: Vec<String> = keys.iter().map(encode).collect::<Result<_>>()?;
        println!(
            "downloading current state of {} ledger entries...",
            encoded.len()
        );
        let client = client()?;
        let mut all = Vec::new();
        for chunk in encoded.chunks(200) {
            let res = rpc(&client, "getLedgerEntries", json!({"keys": chunk}))?;
            all.extend(res["entries"].as_array().cloned().unwrap_or_default());
            thread::sleep(Duration::from_millis(150));
        }
        fs::create_dir_all("cache")?;
        fs::write(ENTRIES_CACHE, serde_json::to_string(&all)?)?;
        println!("saved {} entries to {ENTRIES_CACHE}", all.len());
        all
    };

    let mut out = BTreeMap::new();
    for e in raw {
        let key: LedgerKey = decode(e["key"].as_str().context("entry without key")?)?;
        let data: LedgerEntryData = decode(e["xdr"].as_str().context("entry without xdr")?)?;
        let last_modified = e["lastModifiedLedgerSeq"].as_u64().unwrap_or(0) as u32;
        let live_until = e["liveUntilLedgerSeq"].as_u64().map(|v| v as u32);
        out.insert(
            key,
            CurrentEntry {
                entry: LedgerEntry {
                    last_modified_ledger_seq: last_modified,
                    data,
                    ext: LedgerEntryExt::V0,
                },
                live_until,
            },
        );
    }
    Ok(out)
}

/// Values of entries as recorded by the other transactions:
/// "after" values (what a transaction wrote) and "before" values (what it found).
pub struct History {
    /// key -> (position in time, is it an after-value?, value; None = removed)
    by_key: BTreeMap<LedgerKey, Vec<(u64, bool, Option<LedgerEntry>)>>,
}

impl History {
    pub fn build(txs: &[DecodedTx]) -> History {
        let mut by_key: BTreeMap<LedgerKey, Vec<(u64, bool, Option<LedgerEntry>)>> =
            BTreeMap::new();
        for t in txs {
            let p = t.pos();
            for c in &t.changes {
                let (k, after, v) = match c {
                    LedgerEntryChange::State(e) | LedgerEntryChange::Restored(e) => {
                        (e.to_key(), false, Some(e.clone()))
                    }
                    LedgerEntryChange::Updated(e) | LedgerEntryChange::Created(e) => {
                        (e.to_key(), true, Some(e.clone()))
                    }
                    LedgerEntryChange::Removed(k) => (k.clone(), true, None),
                };
                by_key.entry(k).or_default().push((p, after, v));
            }
        }
        History { by_key }
    }

    /// Best known value of `k` just before position `p`, from the nearest other transaction:
    /// the last one before `p` that wrote it, or the first one after `p` that recorded finding it.
    /// Outer None = no record at all; inner None = the entry did not exist.
    pub fn value_at(&self, k: &LedgerKey, p: u64) -> Option<Option<LedgerEntry>> {
        let recs = self.by_key.get(k)?;
        let before = recs
            .iter()
            .filter(|(q, after, _)| *after && *q < p)
            .max_by_key(|(q, _, _)| *q);
        let later = recs
            .iter()
            .filter(|(q, after, _)| !*after && *q > p)
            .min_by_key(|(q, _, _)| *q);
        match (before, later) {
            (Some(b), Some(a)) => Some(if p - b.0 <= a.0 - p {
                b.2.clone()
            } else {
                a.2.clone()
            }),
            (Some(b), None) => Some(b.2.clone()),
            (None, Some(a)) => Some(a.2.clone()),
            (None, None) => None,
        }
    }
}

/// Where the values in a rebuilt state came from.
#[derive(Default, Clone, Copy)]
pub struct Sources {
    pub own: usize,
    pub history: usize,
    pub current: usize,
    pub absent: usize,
    pub missing: usize,
}

/// Is this a Soroban entry that carries a time-to-live?
fn has_ttl(k: &LedgerKey) -> bool {
    matches!(k, LedgerKey::ContractData(_) | LedgerKey::ContractCode(_))
}

/// The state just before `tx`, from (in order of preference): its own recorded before-values,
/// the other transactions' records, current values from RPC.
pub fn state_before(
    tx: &DecodedTx,
    current: &BTreeMap<LedgerKey, CurrentEntry>,
    history: &History,
) -> (Vec<(LedgerKey, LedgerEntry, Option<u32>)>, Sources) {
    let mut own: BTreeMap<LedgerKey, LedgerEntry> = BTreeMap::new();
    let mut created: BTreeSet<LedgerKey> = BTreeSet::new();
    for c in &tx.changes {
        match c {
            LedgerEntryChange::State(e) | LedgerEntryChange::Restored(e) => {
                own.insert(e.to_key(), e.clone());
            }
            LedgerEntryChange::Created(e) => {
                created.insert(e.to_key());
            }
            _ => {}
        }
    }

    let mut keys: BTreeSet<LedgerKey> = BTreeSet::new();
    keys.extend(tx.read_only.iter().cloned());
    keys.extend(tx.read_write.iter().cloned());

    let mut out = Vec::new();
    let mut src = Sources::default();
    for k in keys {
        let cur = current.get(&k);
        let entry = if let Some(e) = own.get(&k) {
            src.own += 1;
            e.clone()
        } else if created.contains(&k) {
            src.absent += 1; // created by this tx: did not exist before it
            continue;
        } else if let Some(v) = history.value_at(&k, tx.pos()) {
            match v {
                Some(e) => {
                    src.history += 1;
                    e
                }
                None => {
                    src.absent += 1; // removed earlier, according to history
                    continue;
                }
            }
        } else if let Some(c) = cur {
            src.current += 1;
            c.entry.clone()
        } else {
            src.missing += 1;
            continue;
        };
        // Keep every Soroban entry alive for the replay
        let live_until = if has_ttl(&k) {
            Some(
                cur.and_then(|c| c.live_until)
                    .unwrap_or(0)
                    .max(tx.ledger + 1_000_000),
            )
        } else {
            None
        };
        out.push((k, entry, live_until));
    }
    (out, src)
}

/// What mainnet recorded as the final value of each read-write entry (None = removed / never existed).
pub fn recorded_after(
    tx: &DecodedTx,
    input: &[(LedgerKey, LedgerEntry, Option<u32>)],
) -> BTreeMap<LedgerKey, Option<LedgerEntryData>> {
    let mut out: BTreeMap<LedgerKey, Option<LedgerEntryData>> = BTreeMap::new();
    let input_map: BTreeMap<&LedgerKey, &LedgerEntry> =
        input.iter().map(|(k, e, _)| (k, e)).collect();
    for k in &tx.read_write {
        out.insert(k.clone(), input_map.get(k).map(|e| e.data.clone()));
    }
    for c in &tx.changes {
        match c {
            LedgerEntryChange::Updated(e)
            | LedgerEntryChange::Created(e)
            | LedgerEntryChange::Restored(e) => {
                let k = e.to_key();
                if out.contains_key(&k) {
                    out.insert(k, Some(e.data.clone()));
                }
            }
            LedgerEntryChange::Removed(k) => {
                if out.contains_key(k) {
                    out.insert(k.clone(), None);
                }
            }
            LedgerEntryChange::State(_) => {}
        }
    }
    out
}
