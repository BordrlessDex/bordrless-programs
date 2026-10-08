# Bordrless programs

The on-chain side of Bordrless, on Solana: a token standard with hooks, a DEX built for them, a
launchpad on top, and a bridge from SPL.

A Token-2022 transfer hook runs only on transfers, after the balances have changed, sees the
transfer read-only and cannot change the amount. Bordrless moves the token into a program designed
for hooks, the way Uniswap v4 designed pools for them: a hook runs before and after every transfer,
mint, burn, swap and liquidity change and answers instead of only approving: up to three cuts from
the amount, a burn on a swap, a swap's fee, and 64 bytes of state inside every holding.

The TypeScript client is [`@bordrless/sdk`](https://github.com/BordrlessDex/bordrless-sdk).

## Program addresses

The same addresses on mainnet-beta, devnet and localnet.

| Program | Address | What |
| --- | --- | --- |
| `bordrless_token` | `2XoEWp8cF3kRXg74eVwPAyTFhVCAztn3V88komxAvr22` | The token standard: mints, holdings, hooks with return deltas |
| `bordrless_swap` | `GyzKSnnEu2uN5bBRecE4XYY2enbfR2D2MtxnbJPGy7hk` | The DEX: constant-product pools, v4-style hooks, virtual reserves |
| `bordrless_bridge` | `CtLkuFVitoXHTa86Hfp8KmfSDfqJaMYFWr6EGmQVsKb7` | SPL / Token-2022 / SOL in and out of the standard, one for one |
| `bordrless_launch` | `1jcBymHxBjniZDhNPy51Vgm5Nz7pLUdxa9UBHc4TavC` | The launchpad: launches, fee schedule and rules as a pool hook, graduation |
| `bordrless_kit` | `14RJQXPdJfkehit6ezktjd3xujamf8nVSKw2shKamaEH` | The launch rules as a token hook: holder rewards, max wallet, wallet locks |
| `tax_hook` | `8tjnVSreJGBRQFyDBf1SyyhBgLsdBxa2rHYh9sbxFyX7` | An example token hook: a programmable transfer fee with a wallet cap |

Deployed to mainnet-beta on 7 Oct 2026 (the DEX upgraded on 8 Oct 2026). Each program embeds a `security.txt` that points back to
this repository, and each mainnet binary is a [verified build](#verifying-the-deployments) of this
repository: its hash is the hash of what `solana-verify build` produces here.

| Program | Executable hash (sha256, `solana-verify get-program-hash`) |
| --- | --- |
| `bordrless_token` | `ec9d9f528be0fa7e301e476e6e6a19ec79332fa45dc4e6bccd9f26f5355e5152` |
| `bordrless_swap` | `3cc585c7880262876b95ee7def18e98fedb3e8b49604636f3bb79285f98e4fd6` |
| `bordrless_bridge` | `7bb0f9447d5c55f89043408b96074ed782bf502b96bdfc535eb95a12e6c7371f` |
| `bordrless_launch` | `bf9b521b3f11f88e1da5228a14626a99660a412462534007201670f67aeb318d` |
| `bordrless_kit` | `2bf33657166878c7d4b2634c64b61d6ee898d20ccae99db6398e99d0e69e621c` |
| `tax_hook` | `a73935a42bf200b6a9a73490d8fa0ea661d1b89963ff2387c5318f6563835bc8` |

The IDLs are in [`idl/`](idl).

## Layout

| Path | What |
| --- | --- |
| `programs/bordrless_*`, `programs/tax_hook` | The deployed programs (Anchor 1.2) |
| `programs/hook_tester` | Test-only: a hook with scripted answers and a router, never deployed |
| `programs/tests` | LiteSVM suites that load the built `.so` files |
| `crates/bordrless-hook` | The hook protocol: callback args, return deltas, the extra-accounts registry. Start here to write a hook |
| `crates/bordrless-core` | The pure math and policy |
| `docs/hooks-v2.md` | The protocol (v2) and the launch rules. Where it and `architecture.md` differ, it wins |
| `docs/architecture.md` | The original design: programs, hook protocol, curve and graduation |
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

The programs are upgradeable. Until each upgrade authority moves to a multisig behind a timelock
(`docs/hooks-v2.md` §4.14), treat them as upgradeable by a single key, and never as immutable.
`solana program show <address>` shows the current authority.

## Security

See [SECURITY.md](SECURITY.md).

## License

[Apache-2.0](LICENSE).
