# The router: an integrator fee on any swap (2026-10-13)

`bordrless_router` (`7M5GTuzhByzBbrJURzLtbBrFjcsC8aHu816qWTXAYD85`) lets an app that routes swaps
(a terminal, a wallet, a bot) charge its own fee on them, as Jupiter's integrator fee does. It
works on every pool of the DEX, launch pools and ordinary pools alike, and changes nothing in the
DEX, the launchpad or any hook: it wraps the DEX's own `swap`.

## `swap(args)`

- **Accounts:**
  1. the trader (signer);
  2. the integrator's holding of the pool's quote;
  3. the DEX config's treasury's holding of the quote;
  4. the DEX program;
  5. the token program and its event authority;
  6. the router's event accounts;
  7. then the DEX swap's own accounts, exactly as `bordrless_swap::client::swap` (or the SDK's
     `swap` and `launch.swap`) builds them, the trader signing.
- **`args`:**
  - `swap`: the DEX's `SwapArgs`. A buy's `amount_in` is the trader's whole input, fee included.
  - `fee_bps`: 1 to 100 (1%).
  - `min_quote_after_fee`: a sell's floor on what the trader keeps.
- **A buy** (quote in): the fee is `amount_in * fee_bps / 10,000`, rounded down. It is taken from
  the trader's quote holding first. The DEX then swaps `amount_in - fee`; its `min_amount_out`
  bounds the tokens out as usual.
- **A sell** (quote out): the DEX swaps first (its `min_amount_out` bounds what arrives). The
  router measures what reached the trader's quote holding, takes the fee from that (rounded down),
  and refuses (`Slippage`) if what is left is under `min_quote_after_fee`.
- **The split:** 20% of every fee (`BORDRLESS_SHARE_BPS`, rounded down) goes to the DEX config's
  treasury, the rest to the integrator. One `IntegratorFeePaid` event per swap: pool, trader,
  integrator, direction, what the fee was taken of, the fee, Bordrless's share.
- **What it refuses:**
  - a fee out of range (`BadFee`);
  - accounts that aren't the DEX swap's own with the router's trader signing first
    (`SwapAccounts`);
  - a quote mint with a token hook (`QuoteHasHook`; bridged SOL has none);
  - a swap whose quote holding isn't the trader's (`NotTradersHolding`), so a routed sell delivers
    to the trader, whose signature moves the fee;
  - another treasury holding (`WrongTreasury`);
  - an integrator holding of another mint, or the trader's own (`WrongIntegratorHolding`);
  - another DEX or token program (`WrongProgram`).
- **The fees stack:** the DEX's fees and every hook's (the launchpad's creator fee, a custom hook's
  SOL cut) apply to what the router passes on, exactly as for a direct swap. CPI height: router
  (1), DEX (2), launchpad (3), a custom hook's `quote_cut` (4).

## `swap_v2(args)`: native pools (15 Oct 2026)

A native pool (`Pool.reserved[0] == 1`: every launch pool but two since 15 and 16 Oct 2026,
`docs/native-sol-migration.md`) trades with the DEX's `swap_v2`, and the router wraps it the same
way, with the same discriminator as the DEX's: the trader (writable, signer), the integrator's
**wallet** (writable), the DEX treasury's **wallet** (writable), the DEX program, the System
Program, the router's event accounts, then the DEX `swap_v2` accounts (its trader quote is the
trader's own wallet on both sides). The fee and the split are as above, paid in SOL to the two
wallets: no holding, no unwrap. The old `swap` above wraps only the DEX's old `swap`, which on a
native pool sells by CPI and so fails with `BufferShort` whenever the pool's buffer is short (the
usual case): route native pools through `swap_v2`.

## For apps

Build the swap as you would without the router, then wrap it:
`client::swap(trader, integrator_quote, treasury_quote, &dex_swap, fee_bps, min_quote_after_fee)`
in Rust, or the SDK's `router.swap(...)`. Create your integrator holding of bridged SOL once
(`token.createHolding(payer, BRIDGED_SOL_MINT, you)`). Your fees accrue there as bridged SOL;
unwrap it with the bridge. On a native pool use `client::swap_v2` or the SDK's
`router.swapV2(trader, integratorWallet, treasuryWallet, launch.swap(...), feeBps)` instead: your fee
arrives in your wallet as SOL.
