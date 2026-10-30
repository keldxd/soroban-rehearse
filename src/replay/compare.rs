use super::render::{Auth, describe_key, event, hex, short, sv, value_text};
use super::{
    Outcome, Upgrade, call_label, is_target_instance, matches_mainnet, prepare, run_safely,
};
use crate::common::{CONTRACT, REPLAY_N};
use crate::state::state_before;
use anyhow::Result;
use soroban_sdk::xdr::{ContractExecutable, Hash, LedgerEntryData, LedgerKey, ScVal};
use std::collections::{BTreeMap, BTreeSet};

/// One difference between the old and the new code on a real call.
pub struct Finding {
    pub level: &'static str,
    pub kind: &'static str,
    pub text: String,
}

fn finding(level: &'static str, kind: &'static str, text: String) -> Finding {
    Finding { level, kind, text }
}

/// The instance entry with the code reference blanked, so only its storage is compared.
fn normalized(k: &LedgerKey, d: &LedgerEntryData) -> LedgerEntryData {
    let mut d = d.clone();
    if is_target_instance(k) {
        if let LedgerEntryData::ContractData(c) = &mut d {
            if let ScVal::ContractInstance(i) = &mut c.val {
                i.executable = ContractExecutable::Wasm(Hash([0; 32]));
            }
        }
    }
    d
}

/// Everything that differs between the old and the new code's run of one call.
pub fn differences(old: &Outcome, new: &Outcome) -> Vec<Finding> {
    let mut out = Vec::new();

    // 1. Result
    match (old.ok, new.ok) {
        (true, false) => out.push(finding(
            "REVIEW",
            "call-now-fails",
            format!(
                "worked on the old code, fails on the new: {}",
                new.error.clone().unwrap_or_default()
            ),
        )),
        (false, true) => out.push(finding(
            "REVIEW",
            "call-now-succeeds",
            format!(
                "failed on the old code ({}), succeeds on the new",
                old.error.clone().unwrap_or_default()
            ),
        )),
        (false, false) => {
            if old.error != new.error {
                out.push(finding(
                    "REVIEW",
                    "error-changed",
                    format!(
                        "fails on both, with a different error: {} -> {}",
                        old.error.clone().unwrap_or_default(),
                        new.error.clone().unwrap_or_default()
                    ),
                ));
            }
            return out;
        }
        (true, true) => {}
    }
    if !(old.ok && new.ok) {
        // One side failed: the rest of the comparison would only be noise
        return out;
    }

    // 2. Authorizations
    let old_set: BTreeSet<&Auth> = old.auths.iter().collect();
    let new_set: BTreeSet<&Auth> = new.auths.iter().collect();
    let removed: Vec<&Auth> = old_set.difference(&new_set).copied().collect();
    let added: Vec<&Auth> = new_set.difference(&old_set).copied().collect();
    let same_place = |a: &Auth, b: &Auth| a.signer == b.signer && a.call == b.call;
    for r in &removed {
        if added.iter().any(|a| same_place(*a, *r)) {
            out.push(finding(
                "REVIEW",
                "auth-changed",
                format!(
                    "{} still authorizes {}, but with different arguments or sub-calls",
                    r.signer, r.call
                ),
            ));
        } else {
            out.push(finding(
                "DANGER",
                "auth-removed",
                format!("no longer required: {}", r.full),
            ));
        }
    }
    for a in &added {
        if !removed.iter().any(|r| same_place(*a, *r)) {
            out.push(finding(
                "REVIEW",
                "auth-added",
                format!("now required: {}", a.full),
            ));
        }
    }

    // 3. Return value
    if old.return_value != new.return_value {
        let show = |v: &Option<ScVal>| v.as_ref().map(sv).unwrap_or_else(|| "none".to_string());
        out.push(finding(
            "REVIEW",
            "return-changed",
            short(
                format!("{} -> {}", show(&old.return_value), show(&new.return_value)),
                400,
            ),
        ));
    }

    // 4. Events
    if old.events != new.events {
        let i = (0..old.events.len().max(new.events.len()))
            .find(|i| old.events.get(*i) != new.events.get(*i))
            .unwrap_or(0);
        let show = |e: Option<&soroban_sdk::xdr::ContractEvent>| {
            e.map(event).unwrap_or_else(|| "none".to_string())
        };
        out.push(finding(
            "REVIEW",
            "events-changed",
            format!(
                "{} events -> {}; first difference at #{i}:\n             old: {}\n             new: {}",
                old.events.len(),
                new.events.len(),
                show(old.events.get(i)),
                show(new.events.get(i))
            ),
        ));
    }

    // 5. Storage (every entry after the call, including keys the old code never touched)
    let keys: BTreeSet<&LedgerKey> = old
        .after
        .keys()
        .chain(new.after.keys())
        .filter(|k| !matches!(k, LedgerKey::ContractCode(_) | LedgerKey::Ttl(_)))
        .collect();
    for k in keys {
        let a = old.after.get(k).map(|d| normalized(k, d));
        let b = new.after.get(k).map(|d| normalized(k, d));
        match (a, b) {
            (Some(a), Some(b)) if a != b => out.push(finding(
                "REVIEW",
                "storage-changed",
                format!(
                    "{}: {} -> {}",
                    describe_key(k),
                    value_text(&a),
                    value_text(&b)
                ),
            )),
            (None, Some(b)) => out.push(finding(
                "REVIEW",
                "storage-added",
                format!(
                    "new code writes an entry the old code did not: {} = {}",
                    describe_key(k),
                    value_text(&b)
                ),
            )),
            (Some(_), None) => out.push(finding(
                "REVIEW",
                "storage-removed",
                format!("entry missing after the new code runs: {}", describe_key(k)),
            )),
            _ => {}
        }
    }
    out
}

/// `replay <new.wasm>`: every real call, on the deployed code and on the new code, on identical state.
pub fn compare_all(path: &str) -> Result<()> {
    let upgrade = Upgrade::load(path)?;
    let (txs, current, history) = prepare()?;
    let replay_set = &txs[txs.len().saturating_sub(REPLAY_N)..];
    println!("rehearsing an upgrade of {CONTRACT}");
    println!(
        "new code: {path} ({} bytes, hash {})",
        upgrade.wasm.len(),
        hex(&upgrade.hash)
    );
    println!(
        "replaying the newest {} real calls on the deployed code and on the new code\n",
        replay_set.len()
    );

    // Panics are caught and reported per transaction
    std::panic::set_hook(Box::new(|_| {}));

    let (mut skipped, mut crashed, mut same, mut changed, mut changed_verified) = (0, 0, 0, 0, 0);
    // per call: [replayed, skipped, changed]
    let mut by_call: BTreeMap<String, [usize; 3]> = BTreeMap::new();
    // (level, kind) -> number of calls with that finding
    let mut by_kind: BTreeMap<(&'static str, &'static str), usize> = BTreeMap::new();
    let mut details: Vec<String> = Vec::new();
    let mut dangers: Vec<String> = Vec::new();

    for tx in replay_set {
        let (entries, _) = state_before(tx, &current, &history);
        let label = call_label(tx);
        let slot = by_call.entry(label.clone()).or_insert([0, 0, 0]);
        slot[0] += 1;

        let old = match run_safely(tx, &entries, None) {
            Ok(o) => o,
            Err(_) => {
                crashed += 1;
                continue;
            }
        };
        if !old.ok && old.needs_newer_protocol() {
            skipped += 1;
            slot[1] += 1;
            continue;
        }
        let verified = matches_mainnet(tx, &entries, &old);

        let findings = match run_safely(tx, &entries, Some(&upgrade)) {
            Ok(new) => differences(&old, &new),
            Err(e) => vec![finding(
                "REVIEW",
                "host-crash",
                format!("the new code crashed the host: {e}"),
            )],
        };
        if findings.is_empty() {
            same += 1;
            continue;
        }
        changed += 1;
        changed_verified += verified as usize;
        slot[2] += 1;

        let kinds: BTreeSet<(&'static str, &'static str)> =
            findings.iter().map(|f| (f.level, f.kind)).collect();
        for k in kinds {
            *by_kind.entry(k).or_insert(0) += 1;
        }
        let baseline = if verified {
            "replay of the deployed code matches mainnet exactly"
        } else {
            "replay of the deployed code differs from mainnet, lower confidence"
        };
        let mut block = format!("{}  {label}\n  ({baseline})", &tx.hash[..16]);
        for f in findings.iter().take(8) {
            block.push_str(&format!("\n  {:<6} {:<17} {}", f.level, f.kind, f.text));
            if f.level == "DANGER" {
                dangers.push(format!("{}  {}  {}", &tx.hash[..16], label, f.text));
            }
        }
        if findings.len() > 8 {
            block.push_str(&format!("\n  ... and {} more", findings.len() - 8));
        }
        details.push(block);
    }

    let n = replay_set.len();
    println!("==== upgrade rehearsal ====");
    println!("real calls replayed:            {n}");
    println!("skipped (needs protocol 29):    {skipped}");
    println!("host crashed on deployed code:  {crashed}");
    println!("same behaviour:                 {same}");
    println!(
        "different behaviour:            {changed}  ({changed_verified} of them on calls whose replay matches mainnet exactly)"
    );

    if !by_kind.is_empty() {
        println!("\nfindings (calls affected):");
        for ((level, kind), count) in &by_kind {
            println!("  {level:<6} {kind:<17} {count}");
        }
    }
    println!("\nby call (changed / replayed, skipped):");
    for (k, [t, s, c]) in &by_call {
        println!("  {c:>3} / {:>3}   skipped {s:>3}   {k}", t - s);
    }
    if !dangers.is_empty() {
        println!("\nDANGER: authorizations the new code no longer asks for:");
        for d in &dangers {
            println!("  {d}");
        }
    }
    if !details.is_empty() {
        println!("\nfirst calls that behave differently:");
        for d in details.iter().take(10) {
            println!("\n{d}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(auths: Vec<Auth>) -> Outcome {
        Outcome {
            ok: true,
            error: None,
            return_value: None,
            events: vec![],
            after: BTreeMap::new(),
            auths,
            diagnostics: vec![],
        }
    }

    fn auth(signer: &str, call: &str, args: &str) -> Auth {
        Auth {
            signer: signer.into(),
            call: call.into(),
            full: format!("{signer} authorizes {call}({args})"),
        }
    }

    #[test]
    fn identical_runs_have_no_findings() {
        let a = outcome(vec![auth("G1", "P.submit", "1")]);
        let b = outcome(vec![auth("G1", "P.submit", "1")]);
        assert!(differences(&a, &b).is_empty());
    }

    #[test]
    fn removed_auth_is_danger() {
        let f = differences(
            &outcome(vec![auth("G1", "P.submit", "1")]),
            &outcome(vec![]),
        );
        assert_eq!(f.len(), 1);
        assert_eq!((f[0].level, f[0].kind), ("DANGER", "auth-removed"));
    }

    #[test]
    fn changed_auth_arguments_are_review_not_danger() {
        let f = differences(
            &outcome(vec![auth("G1", "P.submit", "1")]),
            &outcome(vec![auth("G1", "P.submit", "2")]),
        );
        assert_eq!(f.len(), 1);
        assert_eq!((f[0].level, f[0].kind), ("REVIEW", "auth-changed"));
    }

    #[test]
    fn added_auth_is_review() {
        let f = differences(
            &outcome(vec![]),
            &outcome(vec![auth("G1", "P.submit", "1")]),
        );
        assert_eq!(f.len(), 1);
        assert_eq!((f[0].level, f[0].kind), ("REVIEW", "auth-added"));
    }

    #[test]
    fn a_new_failure_is_reported_alone() {
        let old = outcome(vec![auth("G1", "P.submit", "1")]);
        let mut new = outcome(vec![]);
        new.ok = false;
        new.error = Some("Error(Contract, #1205)".into());
        let f = differences(&old, &new);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].kind, "call-now-fails");
    }

    #[test]
    fn return_value_change_is_reported() {
        let mut old = outcome(vec![]);
        let mut new = outcome(vec![]);
        old.return_value = Some(ScVal::U32(1));
        new.return_value = Some(ScVal::U32(2));
        let f = differences(&old, &new);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].kind, "return-changed");
        assert!(f[0].text.contains("1 -> 2"));
    }
}
