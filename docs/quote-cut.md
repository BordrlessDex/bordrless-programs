# A custom hook's SOL cut

A token hook only ever sees token transfers, so any cut it takes is in the token. A hook that wants
SOL had to take tokens and sell them, and the sells showed on the chart. With this launchpad upgrade
a custom hook can also take SOL on every trade of its coin, through the launchpad, from the same SOL
as the creator fee. The token program and the DEX don't change.

## How it works

1. **The config.** `create_quote_cut_config(args, author_share_bps, cut)` makes a `LaunchConfig`
   exactly as `create_config` (or, with `author_share_bps` above 0, as `create_listed_config`)
   does, and also records `cut`: `max_buy_bps` and `max_sell_bps`, each at most 1,000 (10%), the
   buy cap above 0. It needs the config's custom hook (`InvalidQuoteCut`). The hook is always asked
   on buys (it may answer 0 there): a hook that can't answer then fails every buy, so a coin whose
   hook would fail its sells can't be bought in the first place. A sell-only cap on a hook without
   `quote_cut` (Bordrless's own `tax_hook`, say) would let anyone buy and only the creator sell. The caps live in
   `LaunchConfig.reserved[1..5]`, zero for every config made before. Event: `QuoteCutConfigured`.
2. **The launch.** `create_launch` from such a config copies the caps into `Launch.reserved[0..4]`,
   with the bumps of the hook's state (`reserved[12]`) and of the launchpad's signer for the hook
   (`reserved[13]`). It then writes the pool's registry with four more extras after the usual four
   (`registry_list_with_quote_cut`), at callback indices 9 to 12:

   | Index | Account | Writable |
   |---|---|---|
   | 9 | the custom hook program | no |
   | 10 | the trader's holding of the token: `holding(base mint, actor)` | no |
   | 11 | the hook's state: `PDA(["state", base mint], hook)` (need not exist) | yes |
   | 12 | the launchpad's signer for the hook: `PDA(["quote-cut", hook], launchpad)` | no |

   Event: `LaunchQuoteCut`. A launch made without such a config has zeros there and a four-extra
   registry, as before.
3. **Each trade.** A buy's `before_swap` and a sell's `after_swap` ask the hook when the launch
   has a cap above 0 on that side. The launchpad calls the hook's `quote_cut` instruction (Anchor
   discriminator `sha256("global:quote_cut")[..8]`). The data is the `PoolHookArgs` the DEX gave
   the callback: a buy's `phase` is `Before` with `amount_in` the SOL in; a sell's is `After` with
   `amount_out` the SOL out. The accounts, in order:

   ```
   signer (the launchpad's ["quote-cut", hook] PDA, signing), launch, pool, base mint, quote mint,
   actor (the trader), the trader's holding of the token, the hook's ["state", mint] (writable)
   ```

   The hook answers with its return data: a `u16` little-endian rate in basis points. No return
   data means 0. Two bytes from any other program also count as 0. Any other length from the hook
   is refused (`QuoteCutBadAnswer`). The launchpad takes `min(answer, cap)` of the amount, rounded
   down. It drops the cut when the cut plus the creator and holder fees would not stay below the
   amount. The cut is added to the creator fee's delta: one delta, to the launch's quote holding.
   The DEX then takes Bordrless's share of it as it does of the creator fee (25% today). It counts
   in `creator_fees_accrued`, with a running total in `Launch.reserved[4..12]`
   (`Launch::quote_cut_accrued`). `QuoteCutTaken` is logged with `emit!`, not by self-CPI, because
   the DEX's callback has no event authority.
4. **Payout.** It is part of the creator fees: `claim_creator_fees` pays it to the creator. For a
   companion coin the creator is the companion, so the SOL lands in the companion's pot through
   the claims the keeper already makes. A listed config's author share applies to it like the rest
   of the creator fee.

## Who pays, and who can refuse

- **Exempt:** the launch's creator trading to itself (`actor` and `recipient` both the creator; for
  a companion coin, every companion trade: dev buy, buyback, Snowball, scripts), because the cut
  would pay the creator's side from the creator's side. A creator buying for someone else pays like
  anyone, so the exemption can't relay cut-free buys. Also exempt: the token's three `hook_vault`
  slots (`PDA(["slot", mint, [i]], hook_vault)`), which sell and buy the hook's token cut. Both
  build their swaps with the pool's four extras only, so their trades need no extra accounts.
- **Everyone else** must pass the four accounts, or the trade fails (`QuoteCutAccountsMissing`).
  If any of them is wrong (another wallet's holding, another hook, another state, the state
  read-only, another signer), the trade fails too (`QuoteCutWrongAccount`). So a trader can't skip
  the cut by leaving the accounts out, or by passing another wallet's holding as its own.
- **What the launchpad can't see:** whose tokens a sell moves, or who a buy really serves. The
  holding passed is the *signer's* (`actor`). A delegate can sell a whale's tokens from its own
  empty holding, and a fresh wallet can buy with `recipient` set to the whale. A rate based on the
  trader's balance is therefore advisory unless the hook checks more itself (below).
- **Traders pay about 1.25 times the cut.** Bordrless's share is charged on the cut as on the
  creator fee: a 10% cap costs a trader up to about 12.5%. Interfaces should show that.
- **The hook can refuse:** if `quote_cut` fails, the trade fails. A custom hook could already
  refuse any transfer, so the risk label covers this. It can also burn compute, or write its own
  state. It gets no signer of the DEX's or the launch's and no writable account but its state, and
  it can't re-enter the DEX or the launchpad (both are on the stack).
- **The signer** is one PDA per hook program. A hook that passes the signer it was given to another
  hook can only vouch for itself: the other hook checks its own `["quote-cut", own id]`.

## For a hook author

```rust
// In your hook program (Anchor). The launchpad calls this on every trade of your coin.
pub fn quote_cut(ctx: Context<QuoteCut>, args: PoolHookArgs) -> Result<()> {
    // Trust the call only when the launchpad signed it for this program.
    let (signer, _) = Pubkey::find_program_address(&[b"quote-cut", crate::ID.as_ref()], &LAUNCHPAD);
    require!(ctx.accounts.signer.is_signer && ctx.accounts.signer.key() == signer, MyError::NotLaunchpad);
    // The trader's balance (a buy: before it; a sell: after the tokens left), from accounts[6].
    let bps: u16 = if balance >= WHALE_LINE { 700 } else { 0 };
    anchor_lang::solana_program::program::set_return_data(&bps.to_le_bytes());
    Ok(())
}
```

Keep the cap as the hard limit: the launchpad never takes more than the config's cap, whatever
the hook answers.

- **Implement `quote_cut` before making the config.** The launchpad doesn't check that the hook
  has it. On a coin whose hook lacks it, every trade but the creator's fails (Anchor's
  `InstructionFallbackNotFound`), buys first, since every config asks on buys: nobody can buy, so
  no buyer is trapped, but the coin is dead. Simulate a buy first; the site and SDK should.
- **Answer every buy.** A hook that answers buys but fails sells is a honeypot, as a token hook
  that refuses sells is; the risk label of a SOL-cut coin says the hook can refuse its trades.
- **A balance rule must close its own gaps.** `args.recipient` is who a buy delivers to: charge the
  full rate when it isn't `args.actor`. A sell's tokens may come from a holding other than the
  signer's (a delegate). Your token hook's `before_transfer` in the same transaction sees the real
  source holding and its balance, so record it in your state there and read it back in
  `quote_cut`.

## For clients and terminals

A trade on such a coin passes the launch pool's four extras and then `quote_cut_extras(hook,
mint, actor)`. Read them from the pool's registry (`hook_accounts_address(launchpad, pool)`): its
entries resolve against the callback prefix, with `Seed::Account(4)` as the trader. A client that
hardcodes four extras gets `QuoteCutAccountsMissing` on these coins only. Quotes can't include the
cut ahead of time, because the hook decides it at trade time; simulate for the exact figure.
`Launch.reserved[0..4]` non-zero means the coin has a SOL cut (`Launch::quote_cut`).

## Limits

- At most 10% a side. The caps are fixed in the config. A coin can't get a cut after launch, and
  coins launched before this upgrade can't get one.
- The launch's CPI depth: DEX (1) → launchpad (2) → hook (3); from a companion (1) → DEX (2) →
  launchpad (3) → hook (4). The creator is exempt, so a companion's trades never reach the hook.
- `hook_vault`'s `SellBuyBurn` slot can't buy another coin that has a SOL cut: its swap passes four
  extras and its slot is not that coin's. `hook_vault` still accepts such a target at
  `create_vault`, and its sells' SOL then waits in `pending_sol` until the slot retires, so
  clients must refuse it as a target (a later `hook_vault` upgrade can refuse it on chain).
- A game companion's hook has no `quote_cut`: a game coin must not launch from a SOL-cut config.
