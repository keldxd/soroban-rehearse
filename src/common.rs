use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use serde_json::{Value, json};
use soroban_sdk::xdr::{
    ContractEvent, FeeBumpTransactionInnerTx, HostFunction, InvokeContractArgs, LedgerEntryChange,
    LedgerKey, Limits, OperationBody, ReadXdr, ScVal, Transaction, TransactionEnvelope,
    TransactionExt, TransactionMeta, WriteXdr,
};
use std::collections::BTreeMap;
use std::fs;

// Blend pool in the backstop's reward zone (Stellar mainnet)
pub const CONTRACT: &str = "CBAKAZUEJBAFA2DCGBRS2WKJT3FNIMJBHTFL76AY7XMAVEINQNNIQWAO";
// How many of the most recent transactions `fetch` keeps (all are used as history)
pub const N: usize = 300;
// How many of the newest transactions `replay` checks
pub const REPLAY_N: usize = 100;
// About one day of ledgers (one ledger every ~5 seconds)
pub const LEDGERS_PER_DAY: u64 = 17_280;
pub const CACHE: &str = "cache/txs.json";
pub const ENTRIES_CACHE: &str = "cache/entries.json";
// SHA-256 of "Public Global Stellar Network ; September 2015" (shown by `stellar snapshot create`)
pub const NETWORK_ID_HEX: &str = "7ac33997544e3175d266bd022439b22cdb16508c01163f26e5cb2a3e1045a979";

// ---------------------------------------------------------------- RPC

pub fn rpc_url() -> String {
    std::env::var("STELLAR_RPC_URL")
        .unwrap_or_else(|_| "https://mainnet.sorobanrpc.com".to_string())
}

pub fn client() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?)
}

/// One JSON-RPC call. `params` is left out entirely when it is null.
pub fn rpc(client: &reqwest::blocking::Client, method: &str, params: Value) -> Result<Value> {
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
    resp.get("result")
        .cloned()
        .ok_or_else(|| anyhow!("{method}: no result in response"))
}

// ---------------------------------------------------------------- XDR helpers

pub fn decode<T: ReadXdr>(b64: &str) -> Result<T> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64)?;
    T::from_xdr(bytes, Limits::none()).map_err(|e| anyhow!("XDR decode failed: {e:?}"))
}

pub fn encode<T: WriteXdr>(v: &T) -> Result<String> {
    let bytes = v
        .to_xdr(Limits::none())
        .map_err(|e| anyhow!("XDR encode failed: {e:?}"))?;
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}

pub fn network_id() -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&NETWORK_ID_HEX[i * 2..i * 2 + 2], 16).unwrap();
    }
    out
}

/// The transaction inside an envelope (unwrapping fee bumps).
fn inner_tx(env: &TransactionEnvelope) -> Option<&Transaction> {
    match env {
        TransactionEnvelope::Tx(e) => Some(&e.tx),
        TransactionEnvelope::TxFeeBump(fb) => match &fb.tx.inner_tx {
            FeeBumpTransactionInnerTx::Tx(e) => Some(&e.tx),
        },
        TransactionEnvelope::TxV0(_) => None,
    }
}

pub fn key_kind(k: &LedgerKey) -> &'static str {
    match k {
        LedgerKey::Account(_) => "account",
        LedgerKey::Trustline(_) => "trustline",
        LedgerKey::ContractData(_) => "contract_data",
        LedgerKey::ContractCode(_) => "contract_code",
        LedgerKey::Ttl(_) => "ttl",
        _ => "other",
    }
}

// ---------------------------------------------------------------- decoded transactions

/// Everything the replay needs from one real mainnet transaction.
pub struct DecodedTx {
    pub hash: String,
    pub ledger: u32,
    /// Position of the transaction inside its ledger (1-based)
    pub order: u32,
    pub timestamp: u64,
    pub call: InvokeContractArgs,
    pub read_only: Vec<LedgerKey>,
    pub read_write: Vec<LedgerKey>,
    pub auth_entries: usize,
    /// Ledger entry changes of the operation (State = value before the tx)
    pub changes: Vec<LedgerEntryChange>,
    pub return_value: Option<ScVal>,
    pub events: Vec<ContractEvent>,
}

impl DecodedTx {
    /// A single number that orders transactions in time.
    pub fn pos(&self) -> u64 {
        self.ledger as u64 * 10_000 + self.order as u64
    }
}

fn decode_one(t: &Value) -> Result<DecodedTx> {
    let hash = t["txHash"].as_str().unwrap_or("?").to_string();
    let ledger = t["ledger"].as_u64().context("no ledger")? as u32;
    let order = t["applicationOrder"].as_u64().unwrap_or(0) as u32;
    let timestamp = t["createdAt"]
        .as_u64()
        .or_else(|| t["createdAt"].as_str().and_then(|s| s.parse().ok()))
        .context("no createdAt")?;
    let envelope: TransactionEnvelope =
        decode(t["envelopeXdr"].as_str().context("no envelopeXdr")?)?;
    let meta: TransactionMeta = decode(t["resultMetaXdr"].as_str().context("no resultMetaXdr")?)?;

    let tx = inner_tx(&envelope).context("old v0 envelope")?;
    if tx.operations.len() != 1 {
        bail!("{} operations", tx.operations.len());
    }
    let OperationBody::InvokeHostFunction(op) = &tx.operations[0].body else {
        bail!("not a contract call");
    };
    let HostFunction::InvokeContract(call) = &op.host_function else {
        bail!("host function is not InvokeContract");
    };
    let TransactionExt::V1(data) = &tx.ext else {
        bail!("no Soroban footprint");
    };
    let TransactionMeta::V4(m) = &meta else {
        bail!("transaction meta is not v4");
    };

    Ok(DecodedTx {
        hash,
        ledger,
        order,
        timestamp,
        call: call.clone(),
        read_only: data.resources.footprint.read_only.to_vec(),
        read_write: data.resources.footprint.read_write.to_vec(),
        auth_entries: op.auth.len(),
        changes: m
            .operations
            .iter()
            .flat_map(|o| o.changes.iter().cloned())
            .collect(),
        return_value: m.soroban_meta.as_ref().and_then(|s| s.return_value.clone()),
        events: m
            .operations
            .iter()
            .flat_map(|o| o.events.iter().cloned())
            .collect(),
    })
}

/// Reads cache/txs.json; returns the decoded transactions, oldest first, and a count of anything skipped.
pub fn load_txs() -> Result<(Vec<DecodedTx>, BTreeMap<String, usize>)> {
    let raw = fs::read_to_string(CACHE).context("cache/txs.json not found, run `fetch` first")?;
    let items: Vec<Value> = serde_json::from_str(&raw)?;
    let mut out = Vec::new();
    let mut skipped: BTreeMap<String, usize> = BTreeMap::new();
    for t in &items {
        match decode_one(t) {
            Ok(d) => out.push(d),
            Err(e) => *skipped.entry(e.to_string()).or_insert(0) += 1,
        }
    }
    out.sort_by_key(|t| t.pos());
    Ok((out, skipped))
}
