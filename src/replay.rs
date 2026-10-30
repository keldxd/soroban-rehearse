pub mod compare;
pub mod render;

use crate::common::{CONTRACT, DecodedTx, REPLAY_N, load_txs, network_id};
use crate::state::{CurrentEntry, History, Sources, current_entries, recorded_after, state_before};
use anyhow::{Context, Result, anyhow, bail};
use render::{Auth, auth_list, hex};
use soroban_ledger_snapshot::LedgerSnapshot;
use soroban_sdk::testutils::{EnvTestConfig, Events};
use soroban_sdk::xdr::{
    ContractEvent, ContractEventBody, ContractExecutable, Hash, LedgerEntry, LedgerEntryData,
    LedgerKey, LedgerKeyContractCode, ScVal,
};
use soroban_sdk::{
    Address, Env, Error as SorobanError, Symbol, TryFromVal, Val, Vec as SorobanVec,
};
use std::collections::BTreeMap;
use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};

pub const DEPLOYED_WASM: &str = "cache/deployed.wasm";

pub struct Outcome {
    pub ok: bool,
    pub error: Option<String>,
    pub return_value: Option<ScVal>,
    pub events: Vec<ContractEvent>,
    /// Every ledger entry after the call (key -> value)
    pub after: BTreeMap<LedgerKey, LedgerEntryData>,
    /// Authorizations the call required
    pub auths: Vec<Auth>,
    /// The host's error diagnostics (the real cause behind a generic error)
    pub diagnostics: Vec<String>,
}

impl Outcome {
    /// The call touched a contract built for a newer protocol than our host supports.
    pub fn needs_newer_protocol(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|d| d.contains("newer than host"))
    }
}

/// New code for the target contract.
pub struct Upgrade {
    pub wasm: Vec<u8>,
    pub hash: [u8; 32],
}

impl Upgrade {
    pub fn load(path: &str) -> Result<Upgrade> {
        let wasm = fs::read(path).with_context(|| format!("cannot read {path}"))?;
        let w = wasm.clone();
        let hash = catch_unwind(move || {
            let mut e = Env::default();
            e.set_config(EnvTestConfig { capture_snapshot_at_drop: false });
            e.deployer().upload_contract_wasm(w.as_slice()).to_array()
        })
        .map_err(|_| anyhow!("{path} could not be loaded as contract code (it must be built for protocol 28 or older)"))?;
        Ok(Upgrade { wasm, hash })
    }
}

/// Is this the ledger key of the target contract's instance (which says which code it runs)?
pub fn is_target_instance(k: &LedgerKey) -> bool {
    matches!(k, LedgerKey::ContractData(d) if d.key == ScVal::LedgerKeyContractInstance && d.contract.to_string() == CONTRACT)
}

/// Points the target contract at different code, keeping all of its storage.
fn swap_code(entries: &mut [(LedgerKey, LedgerEntry, Option<u32>)], hash: [u8; 32]) {
    for (k, e, _) in entries.iter_mut() {
        if is_target_instance(k) {
            if let LedgerEntryData::ContractData(d) = &mut e.data {
                if let ScVal::ContractInstance(inst) = &mut d.val {
                    inst.executable = ContractExecutable::Wasm(Hash(hash));
                }
            }
        }
    }
}

/// Runs one real transaction's call on the given state in a fresh local Soroban host,
/// optionally with new code for the target contract.
pub fn run(
    tx: &DecodedTx,
    entries: &[(LedgerKey, LedgerEntry, Option<u32>)],
    upgrade: Option<&Upgrade>,
) -> Result<Outcome> {
    let mut entries = entries.to_vec();
    if let Some(u) = upgrade {
        swap_code(&mut entries, u.hash);
    }
    let snapshot = LedgerSnapshot {
        // Mainnet is on protocol 29; the released SDK host only runs up to 28 (see README)
        protocol_version: 28,
        sequence_number: tx.ledger,
        timestamp: tx.timestamp,
        network_id: network_id(),
        base_reserve: 5_000_000,
        // Approximate network TTL settings; entries are kept alive explicitly in state.rs
        min_persistent_entry_ttl: 120_960,
        min_temp_entry_ttl: 17_280,
        max_entry_ttl: 6_312_000,
        ledger_entries: entries
            .iter()
            .map(|(k, e, l)| (Box::new(k.clone()), (Box::new(e.clone()), *l)))
            .collect(),
    };
    let mut env = Env::from_ledger_snapshot(snapshot);
    env.set_config(EnvTestConfig {
        capture_snapshot_at_drop: false,
    });
    // We compare behaviour, not fees: no CPU/memory budget limit during replay
    env.cost_estimate().budget().reset_unlimited();
    if let Some(u) = upgrade {
        env.deployer().upload_contract_wasm(u.wasm.as_slice());
    }
    // Signatures cannot be re-checked offline: allow every authorization, but record which were required
    env.mock_all_auths_allowing_non_root_auth();

    let addr = Address::try_from_val(&env, &ScVal::Address(tx.call.contract_address.clone()))
        .map_err(|_| anyhow!("bad contract address"))?;
    let func = Symbol::try_from_val(&env, &ScVal::Symbol(tx.call.function_name.clone()))
        .map_err(|_| anyhow!("bad function name"))?;
    let mut args = SorobanVec::<Val>::new(&env);
    for a in tx.call.args.iter() {
        args.push_back(Val::try_from_val(&env, a).map_err(|e| anyhow!("bad argument: {e:?}"))?);
    }

    let res = env.try_invoke_contract::<Val, SorobanError>(&addr, &func, args);
    let (ok, error, return_value) = match res {
        Ok(Ok(v)) => (true, None, ScVal::try_from_val(&env, &v).ok()),
        Ok(Err(e)) => (
            true,
            Some(format!("return value conversion failed: {e:?}")),
            None,
        ),
        Err(Ok(e)) => (false, Some(format!("{e:?}")), None),
        Err(Err(e)) => (false, Some(format!("{e:?}")), None),
    };

    let events = env.events().all().events().to_vec();
    let auths = auth_list(&env);
    let diagnostics: Vec<String> = env
        .host()
        .get_diagnostic_events()
        .map(|evs| {
            evs.0
                .iter()
                .filter_map(|he| {
                    let ContractEventBody::V0(b) = &he.event.body;
                    let first = b
                        .topics
                        .first()
                        .map(|t| format!("{t:?}"))
                        .unwrap_or_default();
                    if first.contains("error") {
                        Some(format!("{:?} {:?}", b.topics, b.data))
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    let snap = env.to_ledger_snapshot();
    let after = snap
        .ledger_entries
        .iter()
        .map(|(k, (e, _))| (k.as_ref().clone(), e.data.clone()))
        .collect();

    Ok(Outcome {
        ok,
        error,
        return_value,
        events,
        after,
        auths,
        diagnostics,
    })
}

/// Like `run`, but a panic inside the host becomes an error instead of stopping everything.
pub fn run_safely(
    tx: &DecodedTx,
    entries: &[(LedgerKey, LedgerEntry, Option<u32>)],
    upgrade: Option<&Upgrade>,
) -> Result<Outcome> {
    match catch_unwind(AssertUnwindSafe(|| run(tx, entries, upgrade))) {
        Ok(r) => r,
        Err(payload) => {
            let msg = payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "unknown panic".to_string());
            Err(anyhow!(
                "host panicked: {}",
                msg.lines().next().unwrap_or("")
            ))
        }
    }
}

/// Loads the transactions, the current state and the history of values.
pub fn prepare() -> Result<(Vec<DecodedTx>, BTreeMap<LedgerKey, CurrentEntry>, History)> {
    let (txs, skipped) = load_txs()?;
    println!(
        "loaded {} transactions as history (skipped: {skipped:?})",
        txs.len()
    );
    let current = current_entries(&txs)?;
    let history = History::build(&txs);
    Ok((txs, current, history))
}

/// Short name of the call a transaction makes.
pub fn call_label(tx: &DecodedTx) -> String {
    let func = tx.call.function_name.0.to_utf8_string_lossy();
    let target = tx.call.contract_address.to_string();
    if target == CONTRACT {
        format!("{func} (Blend pool, direct)")
    } else {
        format!("{func} on {target}")
    }
}

/// Did the replay on the deployed code reproduce exactly what mainnet recorded?
pub fn matches_mainnet(
    tx: &DecodedTx,
    entries: &[(LedgerKey, LedgerEntry, Option<u32>)],
    out: &Outcome,
) -> bool {
    let expected = recorded_after(tx, entries);
    out.ok
        && out.return_value == tx.return_value
        && out.events == tx.events
        && expected.iter().all(|(k, v)| out.after.get(k) == v.as_ref())
}

/// Saves the target contract's deployed code to cache/deployed.wasm.
fn save_deployed_wasm(current: &BTreeMap<LedgerKey, CurrentEntry>) -> Result<()> {
    let hash = current
        .iter()
        .find_map(|(k, c)| {
            if !is_target_instance(k) {
                return None;
            }
            match &c.entry.data {
                LedgerEntryData::ContractData(d) => match &d.val {
                    ScVal::ContractInstance(i) => match &i.executable {
                        ContractExecutable::Wasm(h) => Some(h.clone()),
                        _ => None,
                    },
                    _ => None,
                },
                _ => None,
            }
        })
        .context("the contract's instance was not found in the saved state")?;
    let key = LedgerKey::ContractCode(LedgerKeyContractCode { hash: hash.clone() });
    let Some(LedgerEntryData::ContractCode(code)) = current.get(&key).map(|c| &c.entry.data) else {
        bail!("the contract's code was not found in the saved state");
    };
    fs::write(DEPLOYED_WASM, code.code.to_vec())?;
    println!(
        "saved the deployed code ({} bytes, hash {}) to {DEPLOYED_WASM}",
        code.code.len(),
        hex(&hash.0)
    );
    Ok(())
}

/// `replay` checks fidelity; `replay <new.wasm>` rehearses an upgrade.
pub fn replay_all() -> Result<()> {
    if let Some(path) = std::env::args().nth(2) {
        return compare::compare_all(&path);
    }

    let (txs, current, history) = prepare()?;
    save_deployed_wasm(&current)?;
    let replay_set = &txs[txs.len().saturating_sub(REPLAY_N)..];
    println!("replaying the newest {}", replay_set.len());

    // Keep the output readable: panics are caught and reported per transaction below
    std::panic::set_hook(Box::new(|_| {}));

    let (mut ran_ok, mut ret_match, mut ev_match, mut wr_match, mut all_match) = (0, 0, 0, 0, 0);
    let (mut crashed, mut skipped) = (0, 0);
    let mut sources = Sources::default();
    let mut mismatches: Vec<String> = Vec::new();
    // per call: [transactions, skipped, identical]
    let mut by_call: BTreeMap<String, [usize; 3]> = BTreeMap::new();

    for tx in replay_set {
        let (entries, src) = state_before(tx, &current, &history);
        sources.own += src.own;
        sources.history += src.history;
        sources.current += src.current;
        sources.absent += src.absent;
        sources.missing += src.missing;

        let func = tx.call.function_name.0.to_utf8_string_lossy();
        let slot = by_call.entry(call_label(tx)).or_insert([0, 0, 0]);
        slot[0] += 1;

        let out = match run_safely(tx, &entries, None) {
            Ok(o) => o,
            Err(e) => {
                crashed += 1;
                mismatches.push(format!("{} {func}: could not run: {e}", tx.hash));
                continue;
            }
        };
        if !out.ok && out.needs_newer_protocol() {
            skipped += 1;
            slot[1] += 1;
            continue;
        }

        let expected_writes = recorded_after(tx, &entries);
        let r = out.return_value == tx.return_value;
        let ev = out.events == tx.events;
        let wr_diff = expected_writes
            .iter()
            .filter(|(k, v)| out.after.get(*k) != v.as_ref())
            .count();
        let wr = wr_diff == 0;

        ran_ok += out.ok as usize;
        ret_match += r as usize;
        ev_match += ev as usize;
        wr_match += wr as usize;
        if out.ok && r && ev && wr {
            all_match += 1;
            slot[2] += 1;
        } else {
            mismatches.push(format!(
                "{} {func}: replay {}, return {}, events {} ({} vs {} recorded), writes {} ({} of {} differ){}",
                tx.hash,
                if out.ok { "ok" } else { "FAILED" },
                if r { "same" } else { "different" },
                if ev { "same" } else { "different" },
                out.events.len(),
                tx.events.len(),
                if wr { "same" } else { "different" },
                wr_diff,
                expected_writes.len(),
                out.error.map(|e| format!(", error: {e}")).unwrap_or_default()
            ));
        }
    }

    let n = replay_set.len();
    let runnable = n - skipped - crashed;
    println!("\n==== replay fidelity (deployed code, rebuilt state vs what mainnet recorded) ====");
    println!("replayed:                      {n}");
    println!("skipped (needs protocol 29):   {skipped}");
    println!("host crashed:                  {crashed}");
    println!("runnable:                      {runnable}");
    println!("  ran successfully:            {ran_ok}");
    println!("  same return value:           {ret_match}");
    println!("  same events:                 {ev_match}");
    println!("  same storage writes:         {wr_match}");
    println!("  identical on all four:       {all_match} / {runnable}");
    println!(
        "\nstate entries came from: own before-values {}, other transactions {}, current RPC {}, absent {}, missing {}",
        sources.own, sources.history, sources.current, sources.absent, sources.missing
    );
    println!("\nby call (identical / runnable, skipped):");
    for (k, [t, s, ok]) in &by_call {
        println!("  {ok:>3} / {:>3}   skipped {s:>3}   {k}", t - s);
    }
    if !mismatches.is_empty() {
        println!("\nfirst mismatches:");
        for m in mismatches.iter().take(10) {
            println!("  {m}");
        }
    }
    Ok(())
}
