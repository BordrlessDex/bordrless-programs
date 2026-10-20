# Bordrless programs

The on-chain side of Bordrless, on Solana: a token standard with hooks, a DEX built for them, a
launchpad on top, and a bridge from SPL.

A Token-2022 transfer hook runs only on transfers, after the balances have changed, sees the
transfer read-only and cannot change the amount. Bordrless moves the token into a program designed
for hooks, the way Uniswap v4 designed pools for them: a hook runs before and after every transfer,
mint, burn, swap and liquidity change and answers instead of only approving: up to three cuts from
the amount, a burn on a swap, a swap's fee, and 64 bytes of state inside every holding.

The TypeScript client is [`@bordrless/sdk`](https://github.com/BordrlessDex/bordrless-sdk).

**Integrating Bordrless tokens** into a trading terminal, aggregator, indexer or wallet? Start with
the [integration guides](https://github.com/BordrlessDex/bordrless-sdk/tree/main/docs/integration).

## Program addresses

The same addresses on mainnet-beta, devnet and localnet.

| Program | Address | What |
| --- | --- | --- |
| `bordrless_token` | `2XoEWp8cF3kRXg74eVwPAyTFhVCAztn3V88komxAvr22` | The token standard: mints, holdings, hooks with return deltas |
| `bordrless_swap` | `GyzKSnnEu2uN5bBRecE4XYY2enbfR2D2MtxnbJPGy7hk` | The DEX: constant-product pools, v4-style hooks, virtual reserves, pools quoted in SPL tokens ([quote tokens](docs/token-quotes.md)) |
| `bordrless_bridge` | `CtLkuFVitoXHTa86Hfp8KmfSDfqJaMYFWr6EGmQVsKb7` | SPL / Token-2022 / SOL in and out of the standard, one for one |
| `bordrless_launch` | `1jcBymHxBjniZDhNPy51Vgm5Nz7pLUdxa9UBHc4TavC` | The launchpad: launches, fee schedule and rules as a pool hook, graduation, [partner configs](docs/partner-fee.md), launches quoted in SPL tokens ([quote tokens](docs/token-quotes.md)) |
| `bordrless_kit` | `14RJQXPdJfkehit6ezktjd3xujamf8nVSKw2shKamaEH` | The launch rules as a token hook: holder rewards, max wallet, wallet locks |
| `tax_hook` | `8tjnVSreJGBRQFyDBf1SyyhBgLsdBxa2rHYh9sbxFyX7` | An example token hook: a programmable transfer fee with a wallet cap |
| `half_life` | `53SpmtkdPWQ63mWoDeXk8P9tuwiT4ed2Wx4fwfy5NSF8` | [Half-Life](programs/half_life): an exit fee that halves every 6 hours held, burned |
| `bordrless_companion` | `6ZUM1gWBH9hBBNoJoaVAGwSftyZ6CUda6vUZTW9MsJuo` | [Companions](docs/companions.md): a launch's creator as a program, so its fees buy back, reward holders, vest, or run a [game](docs/games.md), with no keeper |
| `lottery_hook` | `HqFWsCBQ416DAfevJ9TspyT5yXGGoYTCpcreiGkCgWcr` | [Lottery coins](docs/games.md): the token hook that gives holders tickets for a companion's draws |
| `hook_timelock` | `BBUzaamchPWZpKENmn7bopiuQWvRGm2Vg8TqLLgzgGGZ` | [Phase 3a](docs/phase3a.md): a hook's or a strategy's upgrade authority behind a public delay of at least 3 days; its only exit is "immutable" |
| `hook_vault` | `5cojoUStG7WFhHJiSncCUEwTqDuhDLKu4a9BqTbF8jzG` | [Phase 3a](docs/phase3a.md): where a hook's cuts go: slots that burn, sell for SOL to a wallet fixed before launch, or sell and buy-and-burn another token |
| `bordrless_router` | `7M5GTuzhByzBbrJURzLtbBrFjcsC8aHu816qWTXAYD85` | [The router](docs/router.md): an app's integrator fee (at most 1%) on any DEX swap, 20% of it to Bordrless |
| `loss_cover_hook` | `6KMdXC3Fxovp3pnUsdQuUbderPzZtTA4t6R1jacYJWQr` | [Loss cover](programs/loss_cover_hook) (the site's Lossback): the hook that keeps each holding's entry |
| `loss_cover_script` | `9mRXU9gpWY3B13AyuCCtNjHnKt1pUFBQ6vJDLTwgakhm` | Loss cover's companion script: names the holders under water and what each gets |

Deployed to mainnet-beta on 7 Oct 2026 (the DEX, launchpad and kit upgraded and Half-Life and companions added on 8 Oct 2026; companion v2 and the lottery hook on 9 Oct 2026, jackpot and streak games the same day; phase 3a on 10 Oct 2026: hook_timelock, hook_vault, and the launchpad and companion upgraded; script companions, payouts and Loss cover on 12 Oct 2026; on 13 Oct 2026 the launchpad's [SOL cut](docs/quote-cut.md), the companion's [payees and wallet payouts](docs/payees-and-wallet-payouts.md), and the router; that
evening the launchpad's [partner fee](docs/partner-fee.md), with its largest creator fee raised to 10%;
on 14 Oct 2026 the DEX and the launchpad's curve fill, a buy past the end of a launch's curve taking
what is left, `docs/hooks-v2.md` §3.1 and §5.4); on 15 Oct 2026 the [native SOL](docs/native-sol-migration.md)
upgrade of six programs (the DEX, the launchpad, the kit, the companion, `hook_vault` and the router,
built from merge `4591354`, the re-audit fixes of `912fff3`; slots 457324888 to 457325076); that
evening the companion's `set_payout_terms` and the payout window measured against the live pot
(commit `abfc783`, tx `2BjeHPwF…`, hash `72af698e…`), then the companion's script author share taken
from the launcher's part of the creator fee only (commit `b91b673`, slot 457422433). On 16 Oct 2026 the kit gained `claim_sol_for`, a permissionless push of a holder's native rewards to the holding's owner (commit `2d19ec9`, slot 457493758), and new launches went native (`set_native_launches(true)`, no deploy; the coins launched since 15 Oct migrated). On 20 Oct 2026 the DEX and the launchpad gained [quote tokens](docs/token-quotes.md), coins launched against one to six SPL Token or Token-2022 mints (commit `d31557a`, slots 459122832 and 459123096). Each program embeds a `security.txt` that points back to
this repository, and each mainnet binary is a [verified build](#verifying-the-deployments) of this
repository: its hash is the hash of what `solana-verify build` produces here.

| Program | Executable hash (sha256, `solana-verify get-program-hash`) |
| --- | --- |
| `bordrless_token` | `ec9d9f528be0fa7e301e476e6e6a19ec79332fa45dc4e6bccd9f26f5355e5152` |
| `bordrless_swap` | `d94c63331a988bee4d0418da0dee364bb5978edea873b1191f2a49c813929b2a` (commit `d31557a`) |
| `bordrless_bridge` | `7bb0f9447d5c55f89043408b96074ed782bf502b96bdfc535eb95a12e6c7371f` |
| `bordrless_launch` | `0d2494cbccb2340380658048988b639a6efdb5f2ae7c338a17a99f8842b3e7df` (commit `d31557a`) |
| `bordrless_kit` | `44803fa08ce3c3c87363fb9c197c39edff6e51232262238d25923c3afcaed719` (commit `2d19ec9`) |
| `tax_hook` | `a73935a42bf200b6a9a73490d8fa0ea661d1b89963ff2387c5318f6563835bc8` |
| `half_life` | `2978b0b8dae78e46baed63d5c76ad166460fe85bc997fb999cdc7e14b57c9c44` |
| `bordrless_companion` | `f73a6bde3fc4942332bfa3ddbe952f3123688cacfba9a0fd84851f2130c3e249` (commit `b91b673`) |
| `lottery_hook` | `7eb0751aab14688fd5cb438fa815d2d3e6df38a314c979181a0c93955bb7f29c` (not yet a verified build: see below) |
| `hook_timelock` | `30d849b2c54c2ba64668066e77923a6d99483d0cf5eb404cecfc78e50348d60d` |
| `hook_vault` | `0ffa7762f47272c3167931bd915a77caca44d1ef017cd94637e7315f48b4ccea` (commit `4591354`) |
| `bordrless_router` | `73ccf1b27070ad3a0f704b63a1c5c7b93a047c8d39a4513a571765a5da12efd8` (commit `4591354`) |

`lottery_hook` (five-minute rounds, 12 Oct 2026), `loss_cover_hook`
(`af392c58…`) and `loss_cover_script` (`ffdbc50c…`) run builds of this repository's source made
with `cargo build-sbf` before it was public here; verified builds of them are still to be made and
deployed so their hashes match.

`half_life` on mainnet is the verified build of commit `e54cc4a`: later commits changed a struct it
compiles from the launchpad (`Launch.author_share_bps`, taken from reserved bytes; same layout), so
building it from the current tree gives a different hash. It has not been redeployed.

The IDLs are in [`idl/`](idl).

## Half-Life

A token hook Bordrless ships to launch with, and the example of what a hook here can do that a
Token-2022 transfer hook cannot. **Sell or send tokens the moment you get them and 20% of them
burns; the fee halves every six hours you hold and is gone after two days.** Each holding
remembers its tokens' age in its own hook data, the age travels with the tokens, and the fee is
taken from the amount itself into a furnace anyone can burn. Launch with it from the launch config
`ABz5Je9FznnotUQxxaj28vn18t1Wv9SsDzEfDxGLRJY`. The full explainer:
[programs/half_life](programs/half_life).

## Layout

| Path | What |
| --- | --- |
| `programs/bordrless_*`, `programs/tax_hook`, `programs/half_life` | The deployed programs (Anchor 1.2) |
| `programs/hook_tester` | Test-only: a hook with scripted answers and a router, never deployed |
| `programs/tests` | LiteSVM suites that load the built `.so` files |
| `crates/bordrless-hook` | The hook protocol: callback args, return deltas, the extra-accounts registry. Start here to write a hook |
| `crates/bordrless-core` | The pure math and policy |
| `crates/bordrless-strategy` | Phase 3a: the strategy interface (`plan`, `entitle`) a strategy program implements |
| `programs/hook_timelock`, `programs/hook_vault` | Phase 3a: timelocked upgrades; deferred actions for a hook's cuts |
| `docs/hooks-v2.md` | The protocol (v2) and the launch rules. Where it and `architecture.md` differ, it wins |
| `docs/architecture.md` | The original design: programs, hook protocol, curve and graduation |
| `docs/companions.md` | Companions: a launch's creator as a program, and what the audit made it refuse |
| `docs/games.md` | Lottery coins: the game ticket standard, the lottery hook, the companion's draws (ORAO VRF) |
| `docs/phase3a.md` | Phase 3a: labels and timelocks, Studio attestations, strategies, the hook vault (design, owner decisions, as built) |
| `docs/strategies.md` | Strategy games: writing a strategy, the bounds, the limits |
| `docs/quote-cut.md` | A custom hook's SOL cut on every trade of its coin, through the launchpad |
| `docs/partner-fee.md` | Partner configs: a wallet paid a fixed share of every launch's creator fee, Meteora-style |
| `docs/payees-and-wallet-payouts.md`, `docs/router.md` | The companion's payees and wallet payouts; the router's integrator fee |
| `docs/native-sol-migration.md`, `docs/native-sol-audit.md`, `docs/native-sol-reaudit.md` | Native SOL (15 Oct 2026): what holds lamports, `swap_v2`, the old `swap` on native pools, the migration and its audits |
| `idl/` | Anchor IDLs of the deployed programs |
| `scripts/solana` | Toolchain install, build, test and deploy |

## Build and test

Linux x86_64, with Agave 4.3.0, platform-tools v1.57 and Anchor CLI 1.2.0, all pinned and
checksummed by `scripts/solana/toolchain.sh`:

```sh
scripts/solana/toolchain.sh install && scripts/solana/toolchain.sh anchor ~/bin
scripts/solana/programs.sh build            # cargo build-sbf → target/deploy/*.so
scripts/solana/programs.sh test             # crate unit tests and the LiteSVM suites
scripts/solana/programs.sh idl              # anchor idl build → target/idl
```

## Verifying the deployments

The mainnet binaries are built with [`solana-verify`](https://github.com/Ellipsis-Labs/solana-verifiable-build)
in its Agave 4.3.0 Docker image (SBPF v3), one program at a time, so anyone can rebuild them from
this repository and compare hashes (pass an absolute path; the workspace build would also try to
build the host-only test crate):

```sh
solana-verify build --arch v3 -b solanafoundation/solana-verifiable-build:4.3.0 \
  --library-name bordrless_token "$PWD"
solana-verify get-executable-hash target/deploy/bordrless_token.so
solana-verify get-program-hash -u mainnet-beta 2XoEWp8cF3kRXg74eVwPAyTFhVCAztn3V88komxAvr22
```

or check a program against this repository in one step:

```sh
solana-verify verify-from-repo -u mainnet-beta --program-id 2XoEWp8cF3kRXg74eVwPAyTFhVCAztn3V88komxAvr22 \
  --library-name bordrless_token --arch v3 -b solanafoundation/solana-verifiable-build:4.3.0 \
  https://github.com/BordrlessDex/bordrless-programs
```

## Upgrade authority

The programs are upgradeable. The token program's upgrade authority is a Squads multisig vault
(`5wni4K2qdSfTr6JspbLXDStfbq1XSmzMpF75RSpwsjBH`, 7-day time lock). Until each of the others moves to a
multisig behind a timelock (`docs/hooks-v2.md` §4.14), treat them as upgradeable by a single key
(`5xsibKwtiN6ruxsYrEyWVpV3KcwuzSPbQd1n28a7spEd`), and never as immutable.
`solana program show <address>` shows the current authority.

## Security

See [SECURITY.md](SECURITY.md).

## License

[Apache-2.0](LICENSE).
