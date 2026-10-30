# soroban-rehearse

Rehearse a Soroban contract upgrade on **real mainnet traffic** before you ship it.

soroban-rehearse takes recent mainnet calls that touched your contract, rebuilds the ledger state each one saw, and runs every call twice: once on the deployed code and once on your new WASM. It then reports, call by call, what the upgrade would change:

- **authorizations** the new code no longer asks for (`DANGER`), or newly requires
- calls that start **failing** (or start succeeding)
- different **return values**
- different **events**
- different **storage writes**, including entries the old code never wrote

```
$ soroban-rehearse replay pool_bug1_auth.wasm

==== upgrade rehearsal ====
real calls replayed:            100
skipped (needs protocol 29):    17
same behaviour:                 80
different behaviour:            3  (3 of them on calls whose replay matches mainnet exactly)

findings (calls affected):
  DANGER auth-removed      3
  REVIEW auth-added        2
  REVIEW storage-removed   1

DANGER: authorizations the new code no longer asks for:
  ea5973bc6bcbca92  submit (Blend pool, direct)  no longer required: GD6XSM..5QS5 authorizes CBAKAZ..QWAO.submit(...)
  ...
```

Status: **v0.1, early.** Tested on one contract (a Blend lending pool on Stellar mainnet). Read [Limits](#limits) before relying on it.

## Why

Unit tests check the cases someone thought of. An upgrade breaks the cases nobody thought of: the bot that calls a function in an unusual order, the position at an edge of the interest curve, the indexer that filters on an event topic. Those cases already exist on mainnet. Replaying them against the new code shows what will actually change for real users, before the upgrade transaction is signed.

## Results on Blend (Stellar mainnet, 30 Oct 2026)

Target: Blend pool `CBAKAZUEJBAFA2DCGBRS2WKJT3FNIMJBHTFL76AY7XMAVEINQNNIQWAO`. Sample: the 300 most recent transactions that emitted events from it (ledgers 65,204,243 to 65,214,527); the newest 100 are replayed, all 300 are used to rebuild state.

**1. Does the replay match mainnet?** Each call is run on the deployed code and compared with what mainnet recorded (result, return value, events, storage writes):

| | Calls |
|---|---|
| Replayed | 100 |
| Skipped: they call a contract built for protocol 29 (see Limits) | 17 |
| Runnable | 83 |
| **Identical to mainnet on all four checks** | **77 of 83 (93%)** |
| Different: the call read short-lived data that has since expired | 6 |

**2. Does it raise false alarms?** Rehearsing an "upgrade" to the deployed code itself, and to Blend's pool built from [source](https://github.com/blend-capital/blend-contracts-v2) with no changes: **0 of 83 calls differ** in both cases.

**3. Does it catch real mistakes?** Three one-line bugs planted in Blend's pool source, each built and rehearsed separately:

| Planted bug | What the rehearsal reports |
|---|---|
| `submit` no longer calls `spender.require_auth()` | **DANGER `auth-removed` on 3 of 3** direct `submit` calls; the other 80 calls unchanged |
| Interest rate below target utilization drops the `r_base` term | **51 of 83** calls change: lower `b_rate`, `d_rate` and backstop credit, different flash loan amounts; no auth findings |
| `flash_loan` event drops the `contract` topic | **79 of 83** calls change, **events only**: every call that uses a flash loan |

Full outputs are in [`demo/`](demo/). [`scripts/blend-demo.sh`](scripts/blend-demo.sh) rebuilds all four pools from a pinned Blend commit and reruns the rehearsals. A rerun uses that day's mainnet traffic, so the counts will differ.

These are planted bugs in a copy of Blend's code, **not vulnerabilities in Blend**.

## How it works

1. `fetch` finds recent transactions through the contract's events (Stellar RPC `getEvents`, about 7 days of history) and downloads each one with `getTransaction`.
2. For each call, the state it saw is rebuilt from, in order: the before-values recorded in its own transaction, values recorded by the other downloaded transactions closest in time, and the current value from `getLedgerEntries`.
3. The call is run in a local Soroban host (`soroban-sdk` test utilities) on that state. Signatures cannot be re-checked offline, so every authorization is allowed but recorded.
4. For an upgrade, the contract instance is pointed at the new WASM, keeping all its storage, exactly as `update_current_contract_wasm` would, and the call is run again on identical state.
5. The two runs are compared. Each call is also labelled by whether the replay of the deployed code matched mainnet exactly, so lower-confidence results are visible.

## Usage

Needs a recent stable Rust toolchain.

```
git clone https://github.com/keldxd/soroban-rehearse
cd soroban-rehearse
cargo build --release

cargo run --release -- fetch                  # download recent transactions (cache/)
cargo run --release -- inspect                # what the sample contains
cargo run --release -- replay                 # fidelity check; saves the deployed code to cache/deployed.wasm
cargo run --release -- replay new.wasm        # rehearse an upgrade to new.wasm
cargo run --release -- diagnose <tx hash>     # details for one call
```

`STELLAR_RPC_URL` selects the RPC endpoint (default: `https://mainnet.sorobanrpc.com`). Downloaded data is cached in `cache/` so every run uses identical state; delete it to refresh.

In v0.1 the target contract is set by `CONTRACT` in `src/common.rs`.

## Limits

- **No findings does not mean safe.** Only the code paths used by the sampled calls are exercised.
- **Recent, successful calls only.** The sample comes from the public RPC's 7-day window and is found through events, so failed transactions are not included.
- **Protocol 28 host.** Mainnet runs protocol 29, but the released `soroban-sdk` host supports up to 28. The ledger is replayed as protocol 28; calls that reach a contract built for protocol 29 cannot run and are reported as skipped. The new WASM must also be built for protocol 28 or older.
- **Rebuilt state is approximate.** An entry the call read but no nearby transaction recorded takes its current value. Short-lived (temporary) entries that have expired cannot be recovered; this causes the 6 mismatches above.
- **Contract-to-contract authorization is not recorded.** When a contract calls the target on its own behalf, the host approves that automatically and the test host does not report it, so a removed `require_auth` is only flagged on calls signed by accounts.
- **Fees and resource use are not compared.**

## Roadmap

- Rebuild state from Stellar's full history archive instead of the RPC window: longer samples, failed transactions, and exact values for expired entries.
- Protocol 29 host support, so no calls are skipped.
- Record every authorization check, including contract-to-contract ones.
- Any contract from the command line, JSON output, and a GitHub Action that fails a pull request on `DANGER` findings.
- A storage compatibility check between the old and new contract types.

## License

MIT
