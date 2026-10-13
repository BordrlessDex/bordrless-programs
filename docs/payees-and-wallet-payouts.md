# Payees and wallet payouts (companion, 2026-10-13)

Two additions to the companion. Every account, instruction, event and error that existed before
is unchanged, and a companion made before behaves exactly as it did. Both were audited twice
before release; every finding is fixed and pinned by a test (`companion_payees_wallets.rs`,
`audit_pw_adversarial.rs`, `audit_pw_integration.rs`).

## Payees: up to 10 wallets share the beneficiary part

A companion's split pays its beneficiary part to one wallet. With payees, up to 10 more wallets
share that part, each a fixed share of it, and the beneficiary keeps the rest.

### `set_payees(entries)`

- **When and who:** once, before the launch, signed by the launch's mint (the keypair that made the
  companion, as `create` and the pre-launch `withdraw` already require).
- **What it makes:** `PDA(["payees", mint])` (`Payees`: up to 10 `{ wallet, bps, owed, paid }`
  entries and the beneficiary's own `beneficiary_owed`). It also sets the companion's `version` to
  `VERSION_WITH_PAYEES` (2).
- **Rules:**
  - 1 to 10 entries, each 1 to 10,000 basis points of the beneficiary part, 10,000 in all at most.
  - Every wallet must be distinct and none of: the default key, the beneficiary, the creator
    address, the companion (`BadPayees`).
  - Not after the launch (`AlreadyLaunched`), and not twice (the account exists).
- **Send it in its own transaction** (with the mint signing) when the companion's setup is near
  the packet limit (a Loss cover or script setup).

### `withdraw` of such a companion

It takes the payees account (writable) and each payee among its remaining accounts
(`client::withdraw_with_payees`; SDK `companion.withdrawWithPayees`). With something pending, a
`withdraw` without the payees account fails (`PayeesMissing`). That way nobody can pay the
beneficiary around them. It then:

1. computes what the beneficiary part gained since the last `withdraw`: `pending_beneficiary`, less
   what the payees and the beneficiary are owed;
2. splits it: each payee its basis points (rounded down), the beneficiary the rest;
3. pays each payee what it is now due (its share plus anything owed), as SOL, with one
   `unwrap_sol`;
4. pays the beneficiary its rest, anything it was owed, and the creator address's spare SOL.

**Owed, not lost:** a payee whose account wasn't passed, is read-only, can't take SOL
(executable, a sysvar, a reserved key) or would stay below its rent-exempt minimum is owed its
share. The beneficiary likewise: an account that can't take its part leaves it owed, so it never
blocks the payees. What is owed stays in `pending_beneficiary` (bridged SOL in the creator's
holding) and is paid at the next `withdraw` that can pay it. A payee whose account can never take
SOL (a program id) stays owed for good. A `withdraw` that would pay nobody is refused
(`NothingToDo`), so nobody pays fees to resend it.

**Other details:**
- Before the launch (or with nothing pending) there is nothing to split: the refund works without
  the payees account, as before.
- One `PayeePaid` event per payee due anything (self-CPI, like every other event), giving what it
  was paid and what it is still owed.
- `BeneficiaryPaid` and `paid_beneficiary_total` count the beneficiary's own payments only.
  `FeesClaimed.to_beneficiary` is the whole part, payees' shares included.
- Payees share only the fee part. The dev bag (`release`) still goes to the beneficiary.

## Wallet payouts: a script pays any wallet SOL, within its terms

A script companion made with `create_script_v2` has payout terms (`step_bps`, `holding_bps`,
`window_bps`, `window_secs`). Its version-1 answers pay holders of the token, and the hook records
each payout.

**`allow_wallet_payouts`:** once, before the launch, signed by the mint. It lets the script also
answer **wallet answers** (`PayoutTerms.reserved[0]`, `PayoutTerms::wallets_allowed`; event
`WalletPayoutsAllowed`). Without it, a wallet answer fails the step (`WalletPayoutsOff`), so
scripts made before can't start paying wallets. A buyer can read the flag on chain before the
first trade.

**A wallet answer** (`WalletPayoutAnswer`, version `PAYOUT_ANSWER_WALLETS_VERSION` = 2) is
`1 + 4 + 44 * n` bytes. Each entry is `{ to: Pubkey, lamports: u64, tag: u32 }`. It names the
account paid by its key, which must be among the step's remaining accounts, writable. Whoever
sends the step can't put another account in its place.

**The same terms bound it:**
- each entry at most `holding_bps` of the step's budget;
- all entries at most the step's budget (`step_bps` of the pot, and what the window may still
  pay).

Holder and wallet payouts share the window's and the terms' counters.

**Refused before anything moves:**
- zero lamports, or an account named twice (`BadPayoutAnswer`);
- over a bound (`PayoutOverBound`);
- an account not passed, passed read-only, unable to take SOL, or left below its rent-exempt
  minimum for its data length (`WalletPayoutTarget`);
- any account owned by a Bordrless program (the companion, token, bridge, DEX, launchpad) or by
  the upgradeable loader (`WalletPayoutTarget`);
- by key: the launch, its pool, the creator address, the companion, the script program, the
  script's companion record and state, the terms, the launch's custom hook, the mint, the payout
  authority, this program's event authority, the bridge's SOL vault, the default key
  (`WalletPayoutTarget`).

**Payment:** no hook record; there is no holding, so a plain coin can pay wallets. One
`unwrap_sol` of the total and the bounty, a transfer each, one `ScriptWalletPayout` event per
entry.

**The launcher's wallet can be paid.** A wallet payout names its recipient openly, unlike a holder
payout, which refuses the launcher. Label scripts by what they pay.

**Size:** a step that pays wallets must pass each wallet. With the protocol lookup table, about 12
wallets fit one transaction on a plain coin, fewer on a hook coin or a step that also trades.
Answer at most what fits, or put the wallets in a per-step lookup table.
