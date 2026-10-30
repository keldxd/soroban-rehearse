use crate::common::key_kind;
use soroban_sdk::testutils::{AuthorizedFunction, AuthorizedInvocation};
use soroban_sdk::xdr::{
    ContractEvent, ContractEventBody, LedgerEntryData, LedgerKey, ScAddress, ScVal,
};
use soroban_sdk::{Env, TryFromVal, Val};

/// Shortens long text so the output stays readable.
pub fn short(s: String, max: usize) -> String {
    if s.len() <= max {
        s
    } else {
        let mut cut = max;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{} ...", &s[..cut])
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// CBAKAZ..QWAO style address: enough to recognise, short enough to read.
pub fn abbrev(addr: &str) -> String {
    if addr.len() > 16 {
        format!("{}..{}", &addr[..6], &addr[addr.len() - 4..])
    } else {
        addr.to_string()
    }
}

/// A Soroban value as short readable text.
pub fn sv(v: &ScVal) -> String {
    match v {
        ScVal::Void => "()".to_string(),
        ScVal::Bool(b) => b.to_string(),
        ScVal::U32(n) => n.to_string(),
        ScVal::I32(n) => n.to_string(),
        ScVal::U64(n) => n.to_string(),
        ScVal::I64(n) => n.to_string(),
        ScVal::U128(p) => (((p.hi as u128) << 64) | p.lo as u128).to_string(),
        ScVal::I128(p) => (((p.hi as i128) << 64) | p.lo as i128).to_string(),
        ScVal::Symbol(s) => s.0.to_utf8_string_lossy(),
        ScVal::String(s) => format!("\"{}\"", s.0.to_utf8_string_lossy()),
        ScVal::Address(a) => abbrev(&a.to_string()),
        ScVal::Bytes(b) => format!("0x{}", hex(b.0.as_slice())),
        ScVal::Vec(Some(items)) => format!(
            "[{}]",
            items.0.iter().map(sv).collect::<Vec<_>>().join(", ")
        ),
        ScVal::Map(Some(m)) => format!(
            "{{{}}}",
            m.0.iter()
                .map(|e| format!("{}: {}", sv(&e.key), sv(&e.val)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        ScVal::LedgerKeyContractInstance => "instance".to_string(),
        ScVal::ContractInstance(i) => match &i.storage {
            Some(m) => format!(
                "instance storage {{{}}}",
                m.0.iter()
                    .map(|e| format!("{}: {}", sv(&e.key), sv(&e.val)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            None => "instance (no storage)".to_string(),
        },
        other => format!("{other:?}"),
    }
}

/// A ledger key as short readable text.
pub fn describe_key(k: &LedgerKey) -> String {
    match k {
        LedgerKey::ContractData(d) => short(
            format!(
                "{} {:?} {}",
                abbrev(&d.contract.to_string()),
                d.durability,
                sv(&d.key)
            ),
            220,
        ),
        LedgerKey::ContractCode(c) => format!("contract code {}", hex(&c.hash.0)),
        other => short(format!("{} {:?}", key_kind(other), other), 220),
    }
}

/// The interesting part of a ledger entry's value.
pub fn value_text(d: &LedgerEntryData) -> String {
    match d {
        LedgerEntryData::ContractData(c) => short(sv(&c.val), 300),
        LedgerEntryData::Account(a) => format!("account balance {}", a.balance),
        LedgerEntryData::Trustline(t) => format!("trustline balance {}", t.balance),
        other => short(format!("{other:?}"), 200),
    }
}

/// A contract event as short readable text.
pub fn event(e: &ContractEvent) -> String {
    let who = e
        .contract_id
        .as_ref()
        .map(|c| abbrev(&ScAddress::Contract(c.clone()).to_string()))
        .unwrap_or_else(|| "?".to_string());
    let ContractEventBody::V0(b) = &e.body;
    let topics: Vec<String> = b.topics.iter().map(sv).collect();
    short(
        format!("{who} [{}] {}", topics.join(", "), sv(&b.data)),
        400,
    )
}

/// One authorization a call required.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Auth {
    /// Who had to authorize
    pub signer: String,
    /// Which contract function they authorized (contract.function)
    pub call: String,
    /// Everything, including arguments and sub-calls
    pub full: String,
}

fn to_sc<T>(env: &Env, v: &T) -> Option<ScVal>
where
    Val: TryFromVal<Env, T>,
{
    let val = Val::try_from_val(env, v).ok()?;
    ScVal::try_from_val(env, &val).ok()
}

fn text<T>(env: &Env, v: &T) -> String
where
    Val: TryFromVal<Env, T>,
{
    to_sc(env, v)
        .map(|s| sv(&s))
        .unwrap_or_else(|| "?".to_string())
}

/// (contract.function, full text) of one authorized invocation and its sub-calls.
fn invocation(env: &Env, inv: &AuthorizedInvocation) -> (String, String) {
    let (call, main) = match &inv.function {
        AuthorizedFunction::Contract((contract, func, args)) => {
            let call = format!("{}.{}", text(env, contract), text(env, func));
            let a = text(env, args);
            let a = a.trim_start_matches('[').trim_end_matches(']').to_string();
            (call.clone(), format!("{call}({a})"))
        }
        other => {
            let t = short(format!("{other:?}"), 120);
            (t.clone(), t)
        }
    };
    if inv.sub_invocations.is_empty() {
        (call, main)
    } else {
        let subs: Vec<String> = inv
            .sub_invocations
            .iter()
            .map(|s| invocation(env, s).1)
            .collect();
        (call, format!("{main} > {{{}}}", subs.join(" + ")))
    }
}

/// Every authorization the last call required.
pub fn auth_list(env: &Env) -> Vec<Auth> {
    env.auths()
        .iter()
        .map(|(who, inv)| {
            let (call, full) = invocation(env, inv);
            let signer = text(env, who);
            Auth {
                full: short(format!("{signer} authorizes {full}"), 400),
                signer,
                call,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::xdr::Int128Parts;

    #[test]
    fn i128_values_print_as_numbers() {
        assert_eq!(
            sv(&ScVal::I128(Int128Parts {
                hi: -1,
                lo: u64::MAX
            })),
            "-1"
        );
        assert_eq!(
            sv(&ScVal::I128(Int128Parts { hi: 1, lo: 0 })),
            "18446744073709551616"
        );
    }

    #[test]
    fn long_addresses_are_shortened() {
        assert_eq!(
            abbrev("CBAKAZUEJBAFA2DCGBRS2WKJT3FNIMJBHTFL76AY7XMAVEINQNNIQWAO"),
            "CBAKAZ..QWAO"
        );
    }

    #[test]
    fn helpers() {
        assert_eq!(short("abcdef".to_string(), 3), "abc ...");
        assert_eq!(hex(&[0, 255, 16]), "00ff10");
    }
}
