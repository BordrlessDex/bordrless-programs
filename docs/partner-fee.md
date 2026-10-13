# The partner fee (launchpad, 2026-10-13)

A launch config can name a **partner**: a wallet paid a fixed share of the creator fee of every
coin launched from the config, in SOL, whoever launches it. It works like Meteora's DBC configs:
the config fixes the fee and how it splits between the creator and the partner. A partner (an app
that launches coins through the SDK, a service like an AI-compute budget, a launch platform)
gets paid without writing a hook, without holding or selling anyone's coin, and without being the
config's author, so it combines with any hook a config names.

Nothing else changes. The DEX, the token program and every hook are untouched; a config or launch
made before has no partner and claims exactly as it did.

## The config: `create_partner_config(args, author_share_bps, cut, partner, partner_share_bps)`

- **Accounts:** those of `create_config`, with the partner's account
  `PartnerFee = PDA(["partner", launch_config])` after the config (it is made here, in the same
  instruction). A custom hook's program data, and a timelocked hook's `Timelock`, follow as the
  remaining accounts, as for `create_config`.
- **What it fixes, for ever:**
  - everything `create_config` fixes (rules, creator fee within the launchpad's maximum, custom
    hook, label);
  - `partner`, any key but the default one;
  - `partner_share_bps`: 1 to 10,000 basis points of the creator fee (`InvalidPartnerShare`);
  - optionally an author's share, `author_share_bps` 0 to 5,000, listed when above 0 and paid as a
    listed config's is (not on launches its author makes) (`InvalidAuthorShare`);
  - the author's and the partner's shares together at most 10,000 (`InvalidPartnerShare`);
  - optionally a custom hook's SOL cut, `cut` (zeros for none), with `create_quote_cut_config`'s
    rules (`InvalidQuoteCut`).
- **Where it is kept:** the partner's share in `LaunchConfig.reserved[5..7]` and its account's bump
  in `reserved[7]` (`LaunchConfig::partner_share_bps`, `partner_bump`); the partner's key, share and
  running total in `PartnerFee { version, bump, config, partner, share_bps, paid_total }`.
- **Events:** `LaunchConfigCreated`, then `QuoteCutConfigured` and `ConfigListed` when it has those,
  then `PartnerConfigured { config, partner, partner_share_bps }`.

## The launch

`create_launch` from such a config takes no new account. It copies the partner's share and bump
into `Launch.reserved[14..17]` (`Launch::partner_share_bps`, `partner_bump`), whoever launches,
and logs `LaunchPartner { launch, mint, config, partner_share_bps }` after `LaunchCreated`.

**Not a companion coin** (`PartnerOnCompanion`): a launch whose creator is a companion's creator
address (`PDA(["creator", mint], companion)`) is refused from a partner config. A companion coin's
creator fees are the companion's to split (buybacks, pots, a script's author, payees); a partner
would take its part before the companion saw it, and the companion's own claim can't pass the
partner's accounts. A companion that wants to share its launcher's part uses payees.

## The claims: every claim pays every party

The fees are what they always were: the creator fee (and any custom hook's SOL cut, which is part of
it), less Bordrless's share, taken by the DEX on each trade (25% today), accrue as bridged SOL in
the launch's quote holding. A claim takes all of it and splits it (`fee_split`):

| Who | Gets |
|---|---|
| the partner | `partner_share_bps` of the claim, rounded down |
| the config's author (a listed config, launched by someone else) | `author_share_bps` of the claim, rounded down |
| the creator | the rest |

Each part goes to that party's own holding of bridged SOL; a part of zero is not sent. Every way to
claim pays all of them at once:

- **`claim_creator_fees`** (the creator): with an author, the config and the author's holding as the
  first remaining accounts, as before; with a partner, then the `PartnerFee` (writable) and the
  partner's holding (writable). Missing or wrong: `AuthorAccountsMissing`,
  `PartnerAccountsMissing`, `WrongHolding`.
- **`claim_author_fees`** (the author): with a partner, its two accounts as the remaining accounts.
- **`distribute_fees`** (new, **anyone**, no signer but the fee payer): only for a launch that pays a
  partner (otherwise its config has no `PartnerFee`, refused as Anchor's `AccountNotInitialized`,
  3012; `NoPartnerShare` stays as a second check). Accounts: the launch, the
  `PartnerFee`, the quote mint, the launch's quote holding, the creator's, the partner's, the token
  program and its event authority, the event accounts; with an author, the config and the author's
  holding as the remaining accounts. This is how a partner claims whenever it likes. Nobody can redirect
  anything: every destination is the fixed holding of its party.

The partner's account is checked by address (`PDA(["partner", launch.config])` with the bump the
launch recorded), by owner and discriminator, and must name the launch's config; a copy elsewhere is
refused (`PartnerAccountsMissing`). The partner's holding must be its holding of the launch's quote.

**Events:** `CreatorFeesClaimed` (the creator's part, which may be 0), `AuthorFeesPaid` when the launch
pays an author, `PartnerFeesPaid { launch, mint, config, partner, amount, paid_total }` when it pays a
partner. `paid_total` counts every launch made from the config (`PartnerFee.paid_total`); a
per-coin total is the sum of its `PartnerFeesPaid`.

**Counters:** `Launch.creator_fees_claimed` and `author_fees_paid` as before; the partner's in its
`PartnerFee`.

### Things to know

- **The partner's holding must exist** (bridged SOL, `token.createHolding(payer, BRIDGED_SOL_MINT,
  partner)`). Until it does, claims of its coins fail and the fees wait in the launch; anyone can
  make it (it is idempotent), in the same transaction as the claim.
- **A partner of 10,000** takes all of the creator fee; the creator's part is 0 (and is not sent).
- **The partner is paid whoever launches**, the config's author included. The author's own share is
  not paid on its own launches, as before.
- **The partner and the creator may be the same wallet**: both parts land in its holding.
- **What it is not:** an extra fee on top of the creator fee. The creator fee is the total fee; the
  partner fee is a share of it, decided by the config's maker. The launchpad's maximum creator fee
  (`Config.max_creator_fee_bps`, at most 1,000) bounds it.
