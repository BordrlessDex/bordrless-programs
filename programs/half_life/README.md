# Half-Life

**An exit fee with a half-life.** Sell or send tokens the moment you get them and 20% of them burns.
Hold them, and the fee halves every six hours until it is gone after two days. The age travels with
the tokens, so moving them to a fresh wallet resets nothing.

| | Mainnet |
| --- | --- |
| Hook program | [`53SpmtkdPWQ63mWoDeXk8P9tuwiT4ed2Wx4fwfy5NSF8`](https://solscan.io/account/53SpmtkdPWQ63mWoDeXk8P9tuwiT4ed2Wx4fwfy5NSF8) |
| Launch config ("Half-Life") | [`ABz5Je9FznnotUQxxaj28vn18t1Wv9SsDzEfDxGLRJY`](https://solscan.io/account/ABz5Je9FznnotUQxxaj28vn18t1Wv9SsDzEfDxGLRJY) |
| Executable hash | `2978b0b8dae78e46baed63d5c76ad166460fe85bc997fb999cdc7e14b57c9c44` |
| IDL | [`idl/half_life.json`](../../idl/half_life.json) |

## The fee

| Tokens held for | Exit fee |
| --- | --- |
| 0 (bought this instant) | 20% |
| 3 hours | 15% |
| 6 hours | 10% |
| 12 hours | 5% |
| 18 hours | 2.5% |
| 24 hours | 1.25% |
| 36 hours | 0.31% |
| 48 hours or more | **0%** |

It halves every six hours and falls in a straight line within each six hours, so there is no cliff
to wait for. It is taken from the tokens themselves on every transfer out of a wallet: a sell into
the pool, or a send to anyone. **Buying never pays it.**

Every token the fee takes goes to the mint's **furnace**, and anyone can **stoke** the furnace to
burn everything in it. People who leave early shrink the supply for everyone who stays.

```
sniper buys at launch ──► sells 30 s later ──► ~20% of the tokens sold go to the furnace ──► burned
holder buys ──► holds 2 days ──► sells ──► 0%
```

## Why a Token-2022 transfer hook can't do this

Each piece needs something a transfer hook doesn't have:

1. **Taking part of the amount.** A transfer hook runs after the balances have moved, sees the
   transfer read-only, and can only let it stand or fail it. It can't take tokens out of the
   transfer. Half-Life answers *before* the transfer with a cut, and the token program delivers
   `amount - fee` and sends the fee to the furnace.
2. **Remembering every holder's age.** A transfer hook would need a separate account per holder,
   created and funded before that holder's first transfer, and carried by every transfer. Half-Life
   keeps the age in the 64 bytes of hook data the Bordrless token program stores *inside each
   holding*, and tells the token program what to write there. No extra accounts.
3. **A different rate for each holder.** Token-2022's own transfer-fee extension charges one rate
   for every transfer of the mint. Here the rate depends on how long those particular tokens were
   held.

## How it works

### The age record

When this hook stamps a holding, its 64 bytes of hook data hold:

| Bytes | Content |
| --- | --- |
| 0–2 | `"HL"`, then layout `1` |
| 3–10 | `since`: when the tokens in the holding arrived, unix seconds, little-endian `i64` |
| 11–63 | zero |

The **age** of a holding's tokens is `now - since`.

On every transfer, `before_transfer` gets both holdings' hook data and answers with the fee and the
new data for both sides. The token program writes the data when it applies the transfer.

- **Buy (out of the launch pool):** no fee. The tokens arrive *now*.
- **Sell or send from a wallet:** the fee is set by the sender's age. The tokens that arrive keep
  the **sender's** `since`: age travels with tokens.
- **Receiving into a wallet that already holds tokens:** `since` becomes the average by weight:
  `(held × since_held + received × since_received) / (held + received)`. Buying more dilutes a
  wallet's age by exactly the share of new tokens. There's no way to lend old age to new tokens
  for free.
- **Selling** never changes the remaining tokens' `since`.
- **Emptying a holding** clears its data, so the holding can be closed (the token program refuses
  to close a holding whose hook data is not zero).
- A holding this hook never stamped holds tokens of unknown age and pays the full fee.

### Exempt accounts

These never pay and are never stamped:

- the launch PDA (`["launch", mint]` under the launchpad), which holds the supply and the
  graduation reserve;
- the launch pool, which the hook reads from the `Launch` account and remembers;
- the furnace.

So the supply deposit and the graduation top-up are free, buys come out of the pool free, and a
sell is charged on the seller's side only.

### The furnace

- `prepare` sets the furnace up for the mint, owned by the PDA `["furnace", mint]` under this
  program.
- `light` creates its holding once the mint exists. Anyone can call it and pays about 0.002 SOL
  of rent.
- **Until the furnace is lit, any transfer that owes a fee fails.** Nobody exits for free in the
  first blocks; buys work from the start.
- `stoke` burns everything in the furnace. Anyone can call it.

### What the DEX does with the fee

On a sell into a Bordrless launch pool, the DEX counts the hook's cut as one of the launch's
"cuts" and takes Bordrless's protocol share, 25% of the cut's value in SOL, from the seller's
proceeds (`docs/hooks-v2.md` §3.1). An instant dump therefore burns 20% of the tokens sold *and*
pays roughly 5% of the trade's value to the protocol, on top of the pool's LP fee and the creator
fee. At 48 hours both are zero.

## Launching with it

A launch front end does this for the creator. By hand:

1. **Make the mint keypair** for the launch. The mint doesn't exist yet.
2. **`prepare(mint)`** creates the hook's state and its extra-accounts registry for that mint:

   | Account | |
   | --- | --- |
   | `payer` | signer, writable, pays the rent |
   | `mint` | the future mint |
   | `state` | `["half-life", mint]` under this program, writable |
   | `registry` | `["bordrless-hook-accounts", mint]` under this program, writable |
   | `system_program` | |

3. **`create_launch`** (a v0 transaction with the protocol lookup table) with launch config `ABz5Je9FznnotUQxxaj28vn18t1Wv9SsDzEfDxGLRJY`. With
   `@bordrless/sdk`: `launch.createLaunch(..., { launchConfig, customHook })`, where the hook's
   accounts are its registry's extras in order: `["half-life", mint]` (writable), the furnace
   holding (writable), `["launch", mint]` under the launchpad (read-only). After `prepare`,
   `fetchCustomHookAccounts(connection, HALF_LIFE, mint)` reads the same list from the chain. The
   rules and creator fee in the arguments must equal the config's: no kit rules, creator fee 1%.
4. **`light(mint)`** right after:

   | Account | |
   | --- | --- |
   | `payer` | signer, writable |
   | `state` | |
   | `mint` | |
   | `furnace_owner` | `["furnace", mint]` |
   | `furnace_holding` | the furnace owner's holding of the mint, writable |
   | `token_program` | `2XoEWp8cF3kRXg74eVwPAyTFhVCAztn3V88komxAvr22` |
   | `token_event_authority` | |
   | `system_program` | |

Send them as three transactions: `prepare`, then the launch, then `light`. A launch with a custom
hook is a large transaction (with the site's metadata about 1,200 of Solana's 1,232 bytes,
measured in the tests), so nothing else fits beside it. Nobody can front-run in between: only the
mint keypair can launch that mint, and until `light` lands, buys work and sells wait.

`stoke(mint)` takes the state and mint (writable), `furnace_owner`, `furnace_holding` (writable),
this program, the token program's signer for it (`FBZPj9PmV9dXL8U4qmRffXdXm11e23KEBnNgEVXxhfhF`),
the token program and its event authority. No signer beyond whoever pays the transaction fee.

## Parameters

Fixed in the program, the same for every mint. `prepare` takes no arguments, so a Half-Life token
is always exactly this:

| Constant | Value |
| --- | --- |
| `MAX_FEE_PPM` | 200,000 (20%) |
| `HALF_LIFE_SECS` | 21,600 (6 h) |
| `ZERO_AFTER_HALVINGS` | 8 (48 h) |
| `FLAGS` | `BEFORE_TRANSFER \| TRANSFER_RETURNS_DELTA \| WRITES_HOOK_DATA` (193) |

The launch config adds a 1% creator fee, and no kit rules (a mint has one token hook).

## Tested

`programs/tests/tests/half_life.rs` runs a real launch from the config in LiteSVM against the built
programs:

- buys are free and stamp the buyer;
- before `light`, a sell fails with `FurnaceNotLit`;
- sells at 0 h, 6 h, 9 h and 48 h pay 20%, 10%, 7.5% and 0%, measured by the DEX as the input's cut;
- a wallet-to-wallet transfer pays by the sender's age and the receiver inherits it;
- a later buy blends the age by weight;
- emptying a holding clears its data;
- `stoke` burns the furnace and lowers the supply;
- the launch graduates through the hook, and sells still pay by age afterwards;
- the site's flow, `prepare`, then `create_launch` with the longest metadata the site sends, then
  `light`, fits mainnet's transaction limits;
- a forged callback is refused.

The unit tests in `src/lib.rs` pin the curve at every half-life, the rounding, the record layout and
the blending.

## Limits

- **Trade on the launch pool.** Only the launch pool is exempt. A pool someone else opens for the
  token on the DEX is a holder like any other, so buying out of it pays by *its* age.
- **Upgradeable.** Like the other Bordrless programs, the hook can be upgraded by its upgrade
  authority (`solana program show 53SpmtkdPWQ63mWoDeXk8P9tuwiT4ed2Wx4fwfy5NSF8`). It is not
  immutable until that authority is revoked.
- **Unverified on the site.** The launchpad doesn't vet custom hooks, so the site labels tokens
  launched with one "Custom hook". This one is Bordrless's own, open source, and a verified build
  of this repository.

## Verify

```sh
solana-verify build --arch v3 -b solanafoundation/solana-verifiable-build:4.3.0 \
  --library-name half_life "$PWD"
solana-verify get-program-hash -u mainnet-beta 53SpmtkdPWQ63mWoDeXk8P9tuwiT4ed2Wx4fwfy5NSF8
```
