# Bordrless: architecture

Bordrless is a launchpad, a DEX and a token standard on Solana, built so that tokens can carry
programmable behaviour the way Uniswap v4 pools do: hooks that run before and after every transfer,
mint, burn, swap and liquidity change, and that can change the amounts as they run. A Token-2022
transfer hook runs only on transfers, after the balances have changed, sees the transfer read-only
and cannot change the amount: over the transfer it can only let it stand or make it fail (it can
still write its own accounts and charge a separate, pre-approved fee in another token). Bordrless
hooks answer instead: they can take cuts from the amount, burn part of it on a swap, set a swap's
fee, and keep per-holder state inside every holding. `docs/hooks-v2.md` (its Why section) is the
reference for what may be said about the difference.

Tokens launched on Bordrless live on the Bordrless Token Standard (BTS), not on SPL Token or
Token-2022. Wallets and other venues do not see them, so the site is where they are charted,
bought and sold. A bridge wraps any SPL or Token-2022 token (and SOL) into BTS one-for-one and
back, which is how the platform's own token, launched on a Meteora dynamic bonding curve, enters
the ecosystem.

## The four programs

| Program | What it is | PDA seeds of its state |
|---|---|---|
| `bordrless_token` | The token standard: mints, holdings (token accounts), transfers, mints, burns, delegation, freezing, on-chain metadata, and the hook protocol | `["holding", mint, owner]`, `["hook-authority"]` |
| `bordrless_swap` | The DEX: constant-product pools with v4-style hooks, virtual reserves for launch curves, LP tokens on BTS, protocol fees | `["config"]`, `["pool", base, quote, lp_fee_bps, hook]`, `["lp", pool]`, `["hook-authority"]` |
| `bordrless_bridge` | Wraps SPL Token / Token-2022 mints and native SOL into BTS mints and unwraps them | `["config"]`, `["wrapper", underlying]`, `["wrapped", underlying]`, `["sol-vault"]` |
| `bordrless_launch` | The launchpad: creates a BTS token and its curve pool, runs the fee schedule as the pool's hook, graduates the pool, pays creators | `["config"]`, `["launch", mint]`, `["hook-authority"]` |

`tax_hook` is an example token hook (a programmable transfer fee with a max-wallet rule) that the
tests use and that documents how a third party writes one.

All four are Anchor 1.2 programs. Events are emitted by self-CPI (`emit_cpi!`), which the indexer
decodes from inner instructions.

## The hook protocol

A hook is an ordinary program with instructions named after the callbacks. The calling program
(token or swap) invokes them by CPI with an Anchor-style discriminator
(`sha256("global:<name>")[..8]`) and Borsh-encoded arguments, so a hook is written as an Anchor
program whose instructions are `before_transfer`, `after_transfer`, `before_swap`, and so on.

**Authentication.** The calling program signs every hook CPI with its `["hook-authority"]` PDA,
passed as the first account. A hook checks that this account is a signer and is the PDA of the
program it expects to be called by. Nothing else can produce that signature.

**Accounts.** A fixed prefix of accounts is always passed, then the hook's own extra accounts, which
the client resolves from a registry the hook publishes (see below). Signer privileges of the user
are never forwarded to a hook; a hook cannot spend on the user's behalf.

**Return deltas.** A `before_*` callback may answer through return data with a `HookReturn`
(Borsh): an amount to take from the transfer (`delta`), which prefix-or-extra account receives it
(`delta_account`, an index), and for swaps an LP-fee override. The calling program applies the
delta itself: it moves `delta` to the named account (which must be a holding of the right mint) and
delivers the rest. A hook can take a fee; it cannot move funds anywhere a user did not already
allow by sending them.

**No reentrancy.** Solana forbids indirect reentrancy (A → B → A). A hook therefore cannot call
back into the program that invoked it, and a program that is itself a hook (the launchpad) cannot
trigger its own callbacks when it calls the DEX. The DEX skips the initialize callbacks when the
hook program itself is the caller (it signs with its `["hook-authority"]` PDA), and graduation is a
separate, permissionless instruction rather than something an `after_swap` callback does.

### Token hooks (`bordrless_token`)

Set per mint by the mint's hook authority: `hook_program` and `hook_flags`.

| Flag | Callback | Args | May return |
|---|---|---|---|
| `BEFORE_TRANSFER` 1 | `before_transfer` | `TokenHookArgs` | delta (if `TRANSFER_RETURNS_DELTA` 64) |
| `AFTER_TRANSFER` 2 | `after_transfer` | `TokenHookArgs` with post balances | nothing |
| `BEFORE_MINT` 4, `AFTER_MINT` 8 | `before_mint`, `after_mint` | same | nothing |
| `BEFORE_BURN` 16, `AFTER_BURN` 32 | `before_burn`, `after_burn` | same | nothing |

Prefix accounts: `hook_signer`, `mint`, `source`, `destination`, `source_owner`,
`destination_owner`, `authority` (the default pubkey stands in for what a mint or burn lacks).

### Pool hooks (`bordrless_swap`)

Set per pool at creation: `hook_program` and `hook_flags`; part of the pool's address.

| Flag | Callback | May return |
|---|---|---|
| `BEFORE_INITIALIZE` 1, `AFTER_INITIALIZE` 2 | `before_initialize`, `after_initialize` | nothing |
| `BEFORE_ADD_LIQUIDITY` 4, `AFTER_ADD_LIQUIDITY` 8 | … | nothing |
| `BEFORE_REMOVE_LIQUIDITY` 16, `AFTER_REMOVE_LIQUIDITY` 32 | … | nothing |
| `BEFORE_SWAP` 64 | `before_swap` | LP-fee override (if `BEFORE_SWAP_OVERRIDES_FEE` 1024), delta from the input (if `BEFORE_SWAP_RETURNS_DELTA` 256) |
| `AFTER_SWAP` 128 | `after_swap`, called after the input is settled and the output computed, before it is delivered | delta from the output (if `AFTER_SWAP_RETURNS_DELTA` 512) |

Prefix accounts: `hook_signer`, `pool`, `base_mint`, `quote_mint`, `actor` (the trader or the
liquidity provider).

### The extra-accounts registry

A hook that needs accounts of its own publishes them at a PDA it owns:
`["bordrless-hook-accounts", mint]` for a token hook, `["bordrless-hook-accounts", pool]` for a
pool hook. The account holds a Borsh `HookAccountList`: a list of `ExtraAccount { writable, kind }`
where `kind` is a literal key, or a PDA (`program`, `seeds`) whose seeds are literals or the key of
an account already in the list (prefix accounts first, then extras in order). The SDK resolves
them; the calling programs pass `remaining_accounts` through untouched.

A swap carries up to three such lists: the input mint's token hook, the output mint's token hook
and the pool hook. The instruction arguments say how many accounts each takes.

## The token standard (BTS)

- **Mint**: decimals, supply, optional `max_supply`, mint / freeze / hook / metadata authorities
  (each revocable), hook program and flags, and on-chain metadata (name, symbol, uri). A mint
  address is any signer: a keypair, or a PDA of the creating program.
- **Holding**: one per (mint, owner), at `["holding", mint, owner]`. Anyone may create one for
  anyone (the payer pays rent), so sending to a wallet that has none is one extra instruction, as
  with associated token accounts. Holds `amount`, an optional delegate with its allowance, and a
  frozen flag.
- **Transfer** moves `amount` from one holding to another, running the mint's hooks. If the hook
  returns a delta, the destination receives `amount − delta` and the delta goes to the account the
  hook named. The event carries both post-balances, so the indexer keeps exact holder balances.
- `mint_to`, `burn`, `approve`, `revoke`, `freeze`, `thaw`, `set_authority`, `set_hook`,
  `update_metadata`, `close_holding`.

Because the hook sees the owners on both sides, answers with cuts from the amount and keeps state
inside each holding, things a Token-2022 transfer hook can only approximate with pre-approved side
charges, an extra account per holder and guesses about which transfers are trades (a fee taken
from the trade itself, rewards accrued per holder) are ordinary hook code here. Refusing a
transfer (a cap per wallet, a lock) is something both can do.

## The DEX

Constant-product pools (`x · y = k`) between two BTS mints, with:

- **Virtual reserves** `virtual_base`, `virtual_quote`: offsets added to the real reserves when
  pricing, which is how a launch pool is a bonding curve. A swap can never pay out more than the
  real reserve.
- **Fees** on the input amount: an LP fee (`lp_fee_bps`, stays in the pool and raises `k`) and a
  protocol fee (`protocol_fee_bps`, snapshotted from the config at creation, accrued separately and
  collected by the admin; the config holds two rates: 1% for ordinary pools, 0.25% for launch
  pools, the curves a hook program creates). A hook may override the LP fee per swap.
- **LP tokens** are a BTS mint per pool (`["lp", pool]`), 9 decimals. Liquidity is added and
  removed against them. A curve pool mints no LP until the hook finalizes it.
- **`finalize_curve`**: only the pool's hook (signing with its `["hook-authority"]`) may call it.
  It syncs the reserves to the vault balances, drops the virtual offsets and mints the first LP to
  a recipient. This is how a launch graduates.
- Amounts are measured, not assumed: the DEX reads vault balances before and after each transfer,
  so a token whose hook takes a fee still prices correctly, and `min_amount_out` is checked against
  what the trader actually received.

## The bridge

`register(underlying)` is permissionless and idempotent: it creates the wrapper, a vault for the
underlying, and a BTS mint at `["wrapped", underlying]` whose mint authority is the wrapper. `wrap`
moves the underlying into the vault and mints what arrived; `unwrap` burns and pays out.
Native SOL has its own vault; the site wraps SOL into a swap transaction and unwraps on the way
out, so a trader only ever sees SOL.

Token-2022 mints with transfer fees are supported (what arrives is what is minted); mints with
transfer hooks are not, in this version.

## The launchpad

A launch is one transaction: pay the launch fee, create the BTS mint (metadata fixed, mint
authority revoked at the end), mint the supply to the launch PDA, create the curve pool with the
launch program as its hook and `curve_tokens` deposited, register the hook's extra accounts, and
optionally make the creator's first buy. From then on trading is ordinary DEX swaps.

**The curve.** Supply `S`, of which `C` is sold on the curve and `R = S − C` is reserved. The pool
starts with `C` real base tokens, no quote, and virtual offsets chosen so that the price is
continuous when the virtual offsets are dropped at graduation:

```
k        = (C + Vb) · Vq           Vb = Vq · C / T
T        = Vq · (C − R) / R        the quote raised at graduation
mcap_0   = S · Vq · (C − R) / C²   mcap_grad / mcap_0 = (C / R)²
```

Policy: `S` = 1,000,000,000 (6 decimals), `C` = 75%, `R` = 25%, so graduation is at 9× the opening
market cap and the raise is `2 · Vq`. The backend sets `Vq` so the opening market cap is the policy's
dollar figure at the SOL price of the moment the launch is prepared (bounds are enforced on chain).

**Fees on a launch pool.** Protocol fee 0.25% of the SOL side (collected by the admin; the pool
keeps that rate after graduation, while ordinary DEX pools pay 1%). LP fee 0.3%,
staying in the pool for ever. Creator fee 0–2%, chosen at launch, taken by the hook in the quote
token (from the input on buys, from the output on sells) into the launch's own vault, claimable by
the creator at any time. For the first 30 seconds the hook overrides the LP fee from 80% down to
0.3%, linearly: a sniper's premium deepens the pool for everyone. The creator's own first buy in
the launch transaction pays the normal fee.

**Graduation** is permissionless once the pool's real quote reserve reaches `T`. The site appends it
to the buy that crosses the line; the backend cranks it otherwise. It tops the pool up from the
reserve with exactly the base amount that keeps the price continuous, burns the rest of the
reserve, finalizes the curve, and the LP tokens go to the launch PDA, which has no instruction to
move them: liquidity is locked for ever and the hook keeps collecting the creator fee.

## Off chain

The same shape as the user's other launchpads: the browser talks only to the site's own `/api`
routes; the site's server proxies a backend on Railway with a server-only key; the backend holds
Postgres, the indexer and the transaction builders; the chain is read through Helius.

- **Transactions are built by the backend, signed by the wallet, sent by the backend**, stage by
  stage, and followed until they land or expire.
- **Indexer**: a signature cursor per program (all four), events decoded with the IDLs, oldest
  first, keyed by `(signature, ordinal)`. Trades and candles come from `Swap` events (price from the
  reserves after the swap, virtual offsets included); holder balances from the token program's
  post-balances with a slot guard; launches, pools and wrappers from their creation events.
- **Site**: launches board, launch form, token page (chart, trade, holders, trades), bridge, pools,
  portfolio (wallets cannot show BTS balances, so the site must), docs of the standard.

Policy numbers live in `packages/shared/src/policy.ts` and `crates/bordrless-core` (the same
math in Rust, used by the programs and their tests); the shared package's tests pin them to each
other through fixture vectors.

## Decisions taken without asking

1. Launches are quoted in bridged SOL. The platform token is featured on the bridge and gets a
   pool against bridged SOL; pairing launches with it can come later, as a per-launch quote.
2. Devnet first. Mainnet needs the user's authority key and SOL; the deploy scripts take a key
   path and nothing is hard-coded to this machine.
3. The brand is written Bordrless, as in the brief.
4. One holding per (mint, owner), always a PDA: no auxiliary accounts, so there is never a question
   of which account a balance is in.
5. LP tokens are BTS mints rather than position accounts: transferable, lockable by holding them in
   a PDA with no exit, and visible in the site's portfolio like anything else.
