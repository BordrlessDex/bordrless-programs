//! Seeds, limits and the programs a companion calls (and nothing else).

use anchor_lang::prelude::Pubkey;

/// `PDA(["companion", mint])`: a launch's companion.
pub const COMPANION_SEED: &[u8] = b"companion";
/// `PDA(["creator", mint])`: the launch's creator, a system-owned address with no data that only
/// this program can sign for. Creator fees land in its bridged-SOL holding; the dev bag in its
/// token holding.
pub const CREATOR_SEED: &[u8] = b"creator";

pub const VERSION: u8 = 1;
pub const BPS: u64 = 10_000;
/// The most a step pays whoever sends it: 1% of what it moves.
pub const MAX_BOUNTY_BPS: u16 = 100;
/// Buybacks are at least a minute apart, and at most 30 days.
pub const MIN_BUYBACK_INTERVAL: i64 = 60;
pub const MAX_BUYBACK_INTERVAL: i64 = 30 * 86_400;
/// One buyback spends at least 0.01 SOL of cap (a smaller cap would leave the pending buyback stuck).
pub const MIN_MAX_BUYBACK: u64 = 10_000_000;
/// One buyback spends at most 1% of the pool's quote side (real and virtual), and less on a pool
/// whose fees are low (`steps::pool_share_bps`): a front-runner can only ever move one small slice,
/// smaller than the fees a sandwich around it pays.
pub const BUYBACK_POOL_SHARE_BPS: u64 = 100;
/// A buyback waits while the pool's price is more than 3% above the reference price. A wait raises
/// the reference toward the price by 5% for each interval since it last moved; a buy only lowers it.
pub const MAX_PREMIUM_BPS: u64 = 300;
pub const REFERENCE_STEP_BPS: u64 = 500;
/// The most reference steps one wait catches up (1.05^64, about 23x).
pub const MAX_REFERENCE_STEPS: i64 = 64;
/// Prices are quote per base unit, times this.
pub const PRICE_SCALE: u128 = 1_000_000_000_000;
/// The dev's buy is a launch-time thing: within ten minutes of the launch.
pub const DEV_BUY_WINDOW: i64 = 600;
/// A dev bag vests over a year at most.
pub const MAX_VEST_SECS: i64 = 365 * 86_400;
/// No buyback in the launch's first minute (the sniper fee's window, 30 s, with room).
pub const BUYBACK_AFTER_LAUNCH: i64 = 60;
/// A buyback takes no less than the pool's own quote at that moment, less this (and the fees).
pub const BUYBACK_SLIPPAGE_BPS: u64 = 200;
/// The kit's id, as the kit names the companion (they must agree).
pub const KIT_COMPANION_ID: Pubkey = bordrless_kit::constants::COMPANION_ID;

/// The programs a companion invokes.
pub const LAUNCH_ID: Pubkey = bordrless_launch::ID;
pub const SWAP_ID: Pubkey = bordrless_swap::ID;
pub const TOKEN_ID: Pubkey = bordrless_token::ID;
pub const KIT_ID: Pubkey = bordrless_kit::ID;
pub const BRIDGE_ID: Pubkey = bordrless_bridge::ID;
/// The randomness oracle a game's draw calls (ORAO VRF, classic; everything about it is in
/// `oracle.rs`).
pub const ORAO_VRF_ID: Pubkey = crate::oracle::ORAO_VRF_ID;
/// Bridged SOL, every launch's quote.
pub const BRIDGED_SOL_MINT: Pubkey = bordrless_swap::constants::BRIDGED_SOL_MINT;
/// The upgradeable loader: a program's ProgramData is `PDA([program], loader)`.
pub const BPF_LOADER_UPGRADEABLE_ID: Pubkey =
    Pubkey::from_str_const("BPFLoaderUpgradeab1e11111111111111111111111");

// ---- Games (v2, `docs/companions.md`) -----------------------------------------------------------

/// `PDA(["game", mint])`: a companion's game.
pub const GAME_SEED: &[u8] = b"game";
/// `PDA(["hook-status", hook])`: what the protocol says of a game hook (audited, pot cap, blocked).
pub const HOOK_STATUS_SEED: &[u8] = b"hook-status";
/// `PDA(["oracle", mint])`: a system-owned address with no data that pays a game's oracle
/// requests and receives the oracle's refunds (never the creator address, whose spare lamports
/// `withdraw` pays the beneficiary).
pub const ORACLE_SEED: &[u8] = b"oracle";
pub const GAME_VERSION: u8 = 1;
pub const HOOK_STATUS_VERSION: u8 = 1;
/// The most a pot holds while its hook is not audited: 10 SOL, unless the protocol sets a lower
/// cap (never a higher one: only an audit lifts it). The pot's share above it goes to the buyback.
pub const DEFAULT_POT_CAP: u64 = 10_000_000_000;
/// The lowest cap the protocol may set for a hook that is not audited: the lowest minimum pot, so a
/// pot full at its cap can always be drawn.
pub const MIN_POT_CAP: u64 = MIN_MIN_POT;
/// A game's minimum pot: at least 0.1 SOL (ten times the most one draw's oracle request can cost),
/// at most 1,000 SOL.
pub const MIN_MIN_POT: u64 = 100_000_000;
pub const MAX_MIN_POT: u64 = 1_000_000_000_000;
/// One draw pays at least 10% of the pot (so a prize is never dust), at most all of it.
pub const MIN_PRIZE_BPS: u16 = 1_000;
/// Each claim attempt of a lottery is open from a minute to a day (a five-minute round takes two
/// attempts of a minute, or one of up to two and a half).
pub const MIN_CLAIM_WINDOW: u32 = 60;
pub const MAX_CLAIM_WINDOW: u32 = 86_400;
/// A streak's epochs and a strategy's periods last an hour or more, and the least time they leave
/// for claims (their claim window) is five minutes or more, as before short lottery rounds: a
/// strategy pays at most one budget a period (`docs/phase3a.md`, owner decision 4), and a streak's
/// holders claim each epoch's shares themselves. Only a lottery's rounds go down to
/// `bordrless_game::MIN_ROUND_SECS` and its windows to `MIN_CLAIM_WINDOW`.
pub const MIN_EPOCH_SECS: u32 = 3_600;
pub const MIN_EPOCH_CLAIM_WINDOW: u32 = 300;
/// A draw tries at most this many tickets before the round rolls over.
pub const MAX_ATTEMPTS: u8 = 16;
/// A draw's attempts take at most this share of a round (`max_attempts * claim_window_secs <=
/// round_secs / CLAIMS_PER_ROUND`): a draw of round `r` made early in `r + 1` keeps every attempt
/// before its claims end with `r + 1` (`Game::claims_end`), with room for a late draw or a slow
/// oracle.
pub const CLAIMS_PER_ROUND: u32 = 2;
/// What a draw leaves for ORAO's answer and its reveal (five keeper passes of two minutes; ORAO's
/// mainnet answer came 7 to 319 slots, about 3 s to over 2 minutes, after the request in the 8
/// requests measured on 2026-10-09, so this is over four times the slowest seen) before its last
/// claim window (`Game::last_draw`): a draw of round `r` lands
/// at least this long and a whole claim window before its claims end with `r + 1`, or the round
/// rolls over (`Late`). So a draw is never made in its last moments, where its winner could hardly
/// claim and only a sender racing every keeper would draw. A round shorter than 40 minutes leaves
/// a quarter of itself instead ([`reveal_secs`]).
pub const REVEAL_SECS: i64 = 600;

/// What a draw of a round of `round_secs` leaves for ORAO's answer and its reveal before its last
/// claim window (`Game::last_draw`): `REVEAL_SECS`, or a quarter of the round when that is less.
/// Every round of 40 minutes or more (so every round of an hour or more, all a game could have
/// before five-minute rounds) keeps exactly `REVEAL_SECS`; a five-minute round leaves 75 s. A
/// quarter is under half a round, the most a draw's attempts take (`CLAIMS_PER_ROUND`), so a draw
/// made early in the round after its own is never late, whatever its windows.
pub const fn reveal_secs(round_secs: u32) -> i64 {
    let quarter = (round_secs / 4) as i64;
    if quarter < REVEAL_SECS {
        quarter
    } else {
        REVEAL_SECS
    }
}
/// A game whose pot has paid no prize for this long is dormant (`Game::dormant_secs`: 30 days, or
/// `DORMANT_ROUNDS` rounds when longer, counted from the launch or the pot's last prize or
/// retirement). A dormant pot is drawn from `MIN_MIN_POT` instead of the game's `min_pot`, so a pot
/// the coin's fees no longer grow to its minimum (a coin nobody trades, or the remainder a prize
/// below 100% leaves) is still paid to a holder.
pub const DORMANT_SECS: i64 = 30 * 86_400;
pub const DORMANT_ROUNDS: i64 = 4;
/// After this many dormant periods with no prize (`Game::retirable_at`: 60 days, or 8 rounds when
/// longer), anyone may send the pot to the buyback (`retire`), which pays nobody: a pot nobody can
/// win (below `MIN_MIN_POT`, an oracle that can't be paid or has stopped answering, nobody to
/// claim) is never locked for ever, audited hook or not.
pub const RETIRE_DORMANT_PERIODS: i64 = 2;
/// The token hook callbacks a lottery's hook runs, exactly (`lottery_hook::FLAGS`): transfers and
/// burns, writing hook data (the tickets). No other: a hook taking transfer deltas could skim the
/// companion's buybacks, and a callback the hook lacks (`after_burn`) would fail every burn.
pub const LOTTERY_HOOK_FLAGS: u16 = bordrless_hook::token_flags::BEFORE_TRANSFER
    | bordrless_hook::token_flags::BEFORE_BURN
    | bordrless_hook::token_flags::WRITES_HOOK_DATA;
/// The most extra accounts a game hook's registry may list besides the launch (which a launch
/// transaction carries anyway): with its state among them, the companion's launch through the
/// protocol's lookup table still fits a packet at the site's metadata URI.
pub const MAX_GAME_HOOK_EXTRAS: usize = 3;
/// The most extra accounts a game hook's registry may list besides the launch for a game made by
/// `create_game_v2` (any kind), checked again at the launch of a jackpot or a streak: with the
/// state among them, the companion's launch through the protocol's lookup table fits a packet at
/// the site's longest name (32 characters) and metadata URI, which 3 extras do not. `create_game`
/// (phase 1's lottery, as deployed) keeps `MAX_GAME_HOOK_EXTRAS`.
pub const MAX_GAME_HOOK_EXTRAS_V2: usize = 2;
/// A hook's registry the companion reads (a hook-owned account, decoded onto the 32 KiB heap) may
/// list at most this many extra accounts, each PDA at most `MAX_REGISTRY_SEEDS` seeds, each literal
/// seed at most `MAX_REGISTRY_SEED_LEN` bytes, in at most `MAX_REGISTRY_LEN` bytes; checked before it
/// is decoded, so no registry can make a step abort out of memory (a game hook lists 2 to 4).
pub const MAX_REGISTRY_ACCOUNTS: usize = 8;
pub const MAX_REGISTRY_SEEDS: usize = 16;
pub const MAX_REGISTRY_SEED_LEN: usize = 32;
pub const MAX_REGISTRY_LEN: usize = 1_024;
/// Bordrless's lottery hook (`programs/lottery_hook`, upgradeable only by the protocol, audited with
/// this program): the one game hook `create_game` takes without a status. Any other needs the
/// protocol to have written its `HookStatus` first (phase 1: a hook nobody vetted could refuse the
/// companion's own token moves, its buyback and burn, and strand the buyback).
pub const LOTTERY_HOOK_ID: Pubkey =
    Pubkey::from_str_const("HqFWsCBQ416DAfevJ9TspyT5yXGGoYTCpcreiGkCgWcr");
/// The incinerator: lamports sent to it are burned when the block ends (the runtime removes them
/// from the supply). Where `burn_stranded` sends a blocked game's buyback that can't be spent.
pub const INCINERATOR: Pubkey =
    Pubkey::from_str_const("1nc1nerator11111111111111111111111111111111");
/// Under a blocked hook, a buyback that has neither bought nor waited on its reference price for
/// this long (`STRANDED_SECS`, or `STRANDED_INTERVALS` buyback intervals when longer), counted from
/// the latest of the launch, the last buyback, the reference price's last move, the status's last
/// write, the last burn or move of the pot into the buyback, and the last fee claim that credited
/// it at least what it held, is stranded: anyone may burn it as SOL (`burn_stranded`). Long enough
/// for keepers to land the buybacks a working hook allows; a buyback waiting for its reference to
/// catch up with the price moves the reference once an interval, which restarts the wait.
pub const STRANDED_SECS: i64 = 30 * 86_400;
pub const STRANDED_INTERVALS: i64 = 4;

// ---- Phase 2: the jackpot and the streak (`docs/games.md`) ---------------------------------------

/// A jackpot round left unsettled this long after its timer ran out is forfeited by the next
/// `settle`, whatever its buyer: no round, however odd its buyer, can hold the rounds after it (or
/// `retire`) for ever. Keepers settle within minutes; a winner can settle its own round any time
/// before.
pub const SETTLE_GRACE_SECS: i64 = 30 * 86_400;
/// The sysvar program, owner of every sysvar account.
pub const SYSVAR_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("Sysvar1111111111111111111111111111111111111");
/// The runtime's reserved keys (agave's `ReservedAccountKeys`): the builtin programs and the
/// sysvars. Every transaction demotes them to read-only, so none can be paid; several are on the
/// ed25519 curve, which the game standard takes for a wallet, and some have no account. A jackpot
/// buyer among them forfeits its round.
pub const RESERVED_KEYS: [Pubkey; 31] = [
    Pubkey::from_str_const("AddressLookupTab1e1111111111111111111111111"),
    Pubkey::from_str_const("BPFLoader2111111111111111111111111111111111"),
    Pubkey::from_str_const("BPFLoader1111111111111111111111111111111111"),
    Pubkey::from_str_const("BPFLoaderUpgradeab1e11111111111111111111111"),
    Pubkey::from_str_const("ComputeBudget111111111111111111111111111111"),
    Pubkey::from_str_const("Config1111111111111111111111111111111111111"),
    Pubkey::from_str_const("Ed25519SigVerify111111111111111111111111111"),
    Pubkey::from_str_const("Feature111111111111111111111111111111111111"),
    Pubkey::from_str_const("LoaderV411111111111111111111111111111111111"),
    Pubkey::from_str_const("KeccakSecp256k11111111111111111111111111111"),
    Pubkey::from_str_const("Secp256r1SigVerify1111111111111111111111111"),
    Pubkey::from_str_const("StakeConfig11111111111111111111111111111111"),
    Pubkey::from_str_const("Stake11111111111111111111111111111111111111"),
    Pubkey::from_str_const("11111111111111111111111111111111"),
    Pubkey::from_str_const("Vote111111111111111111111111111111111111111"),
    Pubkey::from_str_const("ZkE1Gama1Proof11111111111111111111111111111"),
    Pubkey::from_str_const("ZkTokenProof1111111111111111111111111111111"),
    Pubkey::from_str_const("SysvarC1ock11111111111111111111111111111111"),
    Pubkey::from_str_const("SysvarEpochRewards1111111111111111111111111"),
    Pubkey::from_str_const("SysvarEpochSchedu1e111111111111111111111111"),
    Pubkey::from_str_const("SysvarFees111111111111111111111111111111111"),
    Pubkey::from_str_const("Sysvar1nstructions1111111111111111111111111"),
    Pubkey::from_str_const("SysvarLastRestartS1ot1111111111111111111111"),
    Pubkey::from_str_const("SysvarRecentB1ockHashes11111111111111111111"),
    Pubkey::from_str_const("SysvarRent111111111111111111111111111111111"),
    Pubkey::from_str_const("SysvarRewards111111111111111111111111111111"),
    Pubkey::from_str_const("SysvarS1otHashes111111111111111111111111111"),
    Pubkey::from_str_const("SysvarS1otHistory11111111111111111111111111"),
    Pubkey::from_str_const("SysvarStakeHistory1111111111111111111111111"),
    Pubkey::from_str_const("NativeLoader1111111111111111111111111111111"),
    SYSVAR_PROGRAM_ID,
];

/// `PDA(["claimed", game, epoch_le, owner])`: a streak share claimed (`ShareReceipt`).
pub const CLAIMED_SEED: &[u8] = b"claimed";
pub const RECEIPT_VERSION: u8 = 1;
/// The protocol's upgrade keys a Studio game hook may be upgradeable by, for `create_game` to take
/// it without a status (the launchpad's own rule for custom hooks: Studio's key and the
/// protocol's). Such a hook is not audited: its pots are capped at `DEFAULT_POT_CAP`.
pub const HOOK_UPGRADE_AUTHORITIES: [Pubkey; 2] =
    bordrless_launch::constants::HOOK_UPGRADE_AUTHORITIES;

// ---- Phase 3a: attestations, timelocks, strategies (`docs/phase3a.md`) -----------------------------

/// `PDA(["attest", program])`: Bordrless Studio's attestation of a program's code
/// (`HookAttestation`).
pub const ATTEST_SEED: &[u8] = b"attest";
pub const ATTESTATION_VERSION: u8 = 1;
/// Studio's attester: a hot key used only by Studio's build worker, which writes an attestation
/// once it has rebuilt a program's source, matched the hash on chain and passed the static checks,
/// the simulator and the review (owner decision 8). It is not Studio's upgrade key.
pub const STUDIO_ATTESTER: Pubkey =
    Pubkey::from_str_const("3uGLsTJNse7vE3pgRkAf6aKKBoKmCyPUcNNu1bw3KvP2");
/// The script approver: a hot key used only by Bordrless's backend (the Workshop), which writes
/// and revokes approvals (`approve`, `revoke_approval`) once a submission has passed the checks,
/// and can do nothing else here (not attest, not set a hook's status, nothing of a game or a
/// companion). It is not Bordrless's upgrade key, which approves and revokes as before.
pub const SCRIPT_APPROVER: Pubkey =
    Pubkey::from_str_const("3onVUMKmJYmEqLpbUTv1bdLixowHJyiRPF4ApEW8ipJW");
/// `hook_timelock`: a program whose upgrade authority is its timelock is "timelocked".
pub const HOOK_TIMELOCK_ID: Pubkey = bordrless_hook::authority::HOOK_TIMELOCK_ID;
/// An attestation's review verdicts.
pub const REVIEW_PASS: u8 = 0;
pub const REVIEW_WARN: u8 = 1;
/// An attestation's kinds (`HookAttestation.kind`): a game hook is taken only with a kind-1 one.
pub const ATTEST_KIND_TOKEN_HOOK: u8 = 0;
pub const ATTEST_KIND_GAME_HOOK: u8 = 1;
pub const ATTEST_KIND_STRATEGY: u8 = 2;
/// `HookAttestation.reserved[0]` once the protocol's upgrade authority revoked it: the program is
/// never attested again.
pub const REVOKED_BY_PROTOCOL: u8 = 1;

/// `PDA(["strategy", mint])`: a strategy game's terms (`StrategyTerms`).
pub const STRATEGY_SEED: &[u8] = b"strategy";
pub const STRATEGY_VERSION: u8 = 1;
/// The most of the unlocked pot one period may pay: 50% (owner decision 4).
pub const MAX_STRATEGY_BUDGET_BPS: u16 = 5_000;
/// The most one holder gets of a period's budget: 25% (owner decision 4).
pub const MAX_STRATEGY_SHARE_BPS: u16 = 2_500;
/// The most candidates one `pay_strategy` takes (transaction size, measured in
/// `companion_strategy.rs`).
pub const MAX_STRATEGY_PER_TX: u8 = 4;
/// The least share of the pot (bps) a strategy period must have paid for a payment to count as
/// activity: below it, payments don't hold off `retire` (a pot paid out as dust every few weeks
/// would otherwise never retire).
pub const STRATEGY_ACTIVE_BPS: u16 = 100;
/// The most compute a strategy's `plan` and `entitle` may declare: what keepers budget for each call
/// (the companion can't meter a CPI on mainnet, `strategy::ask`; a call over the transaction's limit
/// fails it). Studio's simulator holds a strategy to 70% of its declared caps.
pub const MAX_PLAN_CU: u32 = 150_000;
pub const MAX_ENTITLE_CU: u32 = 60_000;
/// The most extra accounts a strategy's registry may list, each owned by the strategy.
pub const MAX_STRATEGY_EXTRAS: usize = bordrless_strategy::MAX_EXTRAS;

// ---- Snowball (the market-maker companion, 2026-10-12) ---------------------------------------------

/// `PDA(["snowball", mint])`: a Snowball companion's market maker.
pub const SNOWBALL_SEED: &[u8] = b"snowball";
pub const SNOWBALL_VERSION: u8 = 1;
/// `Companion.mode`: the classic companion (buyback, holders, launcher, or a game).
pub const MODE_CLASSIC: u8 = 0;
/// `Companion.mode`: a Snowball: its pot (`pending_pot`, `pot_bps` of every fee claim) trades the
/// token on its own pool, buying on a dip and selling on a rise.
pub const MODE_SNOWBALL: u8 = 1;
/// The price move that triggers a buy (a drop) or a sell (a rise), each its own: 0.5% to 20%.
pub const MIN_SNOWBALL_STEP_BPS: u16 = 50;
pub const MAX_SNOWBALL_STEP_BPS: u16 = 2_000;
/// The share of the pot a buy spends, and of the holding a sell sells: 1% to all of it.
pub const MIN_SNOWBALL_SIZE_BPS: u16 = 100;
pub const MAX_SNOWBALL_SIZE_BPS: u16 = 10_000;
/// The launcher's cap on one trade, as a share of the pool's SOL side (real and virtual): 0.25% to
/// 10%. Above about a quarter of a buy's and a sell's fees together (1% at the usual fees), a bot can
/// profitably sandwich a trade; a Snowball's goal is volume, and the launcher chooses.
pub const MIN_SNOWBALL_POOL_BPS: u16 = 25;
pub const MAX_SNOWBALL_POOL_BPS: u16 = 1_000;
/// Trades are at least 10 seconds apart, and at most a day.
pub const MIN_SNOWBALL_INTERVAL: i64 = 10;
pub const MAX_SNOWBALL_INTERVAL: i64 = 86_400;
/// A trade is worth at least 0.001 SOL; a smaller one is not made ([`crate::instructions::snowball`]).
pub const MIN_SNOWBALL_TRADE: u64 = 1_000_000;
/// The most one trade moves, lamports (or the SOL value of what a sell sells): at least 0.01 SOL.
pub const MIN_SNOWBALL_MAX_TRADE: u64 = 10_000_000;

// ---- Approvals and script companions (2026-10-12) ---------------------------------------------------

/// `PDA(["approval", program])`: Bordrless's approval of a program's code (`Approval`).
pub const APPROVAL_SEED: &[u8] = b"approval";
pub const APPROVAL_VERSION: u8 = 1;
/// `Approval.kind`: a token hook the site offers beside a companion (read off chain only: a launch
/// takes a custom hook without an approval on chain).
pub const APPROVAL_KIND_TOKEN_HOOK: u8 = 0;
/// `Approval.kind`: a companion script (`create_script`, `script_step`).
pub const APPROVAL_KIND_SCRIPT: u8 = 1;
/// `PDA(["script", mint])`: a script companion's script (`Script`).
pub const SCRIPT_SEED: &[u8] = b"script";
pub const SCRIPT_VERSION: u8 = 1;
/// `Companion.mode`: a script companion: a builder's program (the script, approved by Bordrless)
/// decides what its pot (`pending_pot`, `pot_bps` of every fee claim) does, the companion keeping
/// custody and checking what each step spent.
pub const MODE_SCRIPT: u8 = 2;
/// Script steps are at least 10 seconds apart, and at most a day.
pub const MIN_SCRIPT_INTERVAL: i64 = 10;
pub const MAX_SCRIPT_INTERVAL: i64 = 86_400;
/// The most bytes `create_script` hands the script's `companion_init`.
pub const MAX_SCRIPT_INIT_LEN: usize = 512;
/// `PDA(["script-state", mint])`: a script's state, made by `create_script` (zeroed, owned by the
/// script, the payer paying its rent) for the script to keep its data in: a script never gets a
/// payer's signature, so it can't make accounts of its own.
pub const SCRIPT_STATE_SEED: &[u8] = b"script-state";
/// The most bytes a script's state holds (what one instruction may allocate).
pub const MAX_SCRIPT_STATE_LEN: u32 = 10_240;
/// What a script step pays whoever sends it, from the pot, when the pot holds it (lamports).
pub const SCRIPT_BOUNTY: u64 = 20_000;
/// No script step in the launch's first minute (the sniper fee's window, 30 s, with room).
pub const SCRIPT_AFTER_LAUNCH: i64 = 60;
/// A script's author earns this share of the creator fees every claim of a script companion takes
/// from its launch (half, as a listed config's author can), paid as SOL before the split; nothing
/// when the author is the launch's own launcher (its beneficiary).
pub const AUTHOR_SHARE_BPS: u16 = 5_000;
/// The script's two instructions, as Anchor names them: the first 8 bytes of
/// `sha256("global:companion_init")` and of `sha256("global:companion_step")`.
pub const COMPANION_INIT_DISCRIMINATOR: [u8; 8] = [0x76, 0xa1, 0xbb, 0xb4, 0x57, 0x56, 0xdc, 0x88];
pub const COMPANION_STEP_DISCRIMINATOR: [u8; 8] = [0x5e, 0x15, 0xaa, 0xa1, 0xf6, 0x6e, 0x15, 0x28];

// ---- Payouts: a script pays holders through the companion (Loss cover, 2026-10-12) --------------

/// `PDA(["payout-terms", mint])`: the bounds a script companion's payouts keep within
/// (`PayoutTerms`, made by `create_script_v2`).
pub const PAYOUT_TERMS_SEED: &[u8] = b"payout-terms";
pub const PAYOUT_TERMS_VERSION: u8 = 1;
/// `PDA(["payout-authority"])`: the one signer of a hook's `record_payout`, a program-wide PDA that
/// signs nothing else. The address and its bump, so a step pays no search.
pub const PAYOUT_AUTHORITY_SEED: &[u8] = b"payout-authority";
pub const PAYOUT_AUTHORITY: Pubkey =
    Pubkey::from_str_const("8XJcBCYmYyggTQBNvK3RausfyRMpmtjJTjKd53W3StkG");
pub const PAYOUT_AUTHORITY_BUMP: u8 = 251;
/// The layout of a `PayoutAnswer` a script returns.
pub const PAYOUT_ANSWER_VERSION: u8 = 1;
/// A `PayoutAnswer` whose entries name wallets to pay, not holdings (2026-10-13): the same layout,
/// each `account` an index of the account paid itself, within the same terms; no hook record.
pub const PAYOUT_ANSWER_WALLETS_VERSION: u8 = 2;
/// `PDA(["payees", mint])`: the wallets that share a companion's beneficiary part (2026-10-13).
pub const PAYEES_SEED: &[u8] = b"payees";
/// The most wallets that share the beneficiary part.
pub const MAX_PAYEES: usize = 10;
/// `Companion.version` of a companion whose beneficiary part is shared (`set_payees`): its
/// `withdraw` takes its `Payees` and pays them. 1 for every other companion.
pub const VERSION_WITH_PAYEES: u8 = 2;
/// `Payees.version`.
pub const PAYEES_VERSION: u8 = 1;
/// `PayoutTerms.reserved[0]` of a script allowed wallet payouts (`allow_wallet_payouts`).
pub const WALLET_PAYOUTS_ALLOWED: u8 = 1;
/// The most entries one answer carries (213 bytes of return data at most).
pub const MAX_PAYOUTS_PER_STEP: usize = 16;
/// An answer of this many entries (or more) earns the whole `SCRIPT_BOUNTY`; a thinner one earns
/// its part of it, an empty one nothing.
pub const PAYOUT_BOUNTY_FULL: u64 = 8;
/// A payout window (the share of the pot a window may pay is counted over it): 10 seconds to a day.
pub const MIN_PAYOUT_WINDOW_SECS: u32 = 10;
pub const MAX_PAYOUT_WINDOW_SECS: u32 = 86_400;
/// The hook's `record_payout(lamports, tag)`, as Anchor names it: the first 8 bytes of
/// `sha256("global:record_payout")`.
pub const RECORD_PAYOUT_DISCRIMINATOR: [u8; 8] = [0xe3, 0xa4, 0xd5, 0x36, 0x45, 0xac, 0x1f, 0xd8];

/// Workshop scripts that are not native-ready: they trade the classic swap with the creator's
/// bridged SOL and check the pool's quote vault holds its books, so on a native coin every step
/// fails. A companion running one can't launch while native launches are on
/// (`ScriptNotNativeReady`, audit H-1/I-5); the 13 coins already launched with one stay bridged
/// but for their pool. Taken off this list (a program upgrade) when the author ships a native build.
pub const NOT_NATIVE_READY_SCRIPTS: [Pubkey; 2] = [
    Pubkey::from_str_const("HxsFA5ox5pYVBDN1QSUtnUzvAuJtAQZRG8YYMuJzR2Mp"),
    Pubkey::from_str_const("DMQE7DHM2sjLXAYmqmnFvxCdkmEFctB2fZQY21ToZQuZ"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_kit_names_this_program() {
        assert_eq!(KIT_COMPANION_ID, crate::ID);
        assert_eq!(
            bordrless_kit::constants::COMPANION_CREATOR_SEED,
            CREATOR_SEED
        );
    }

    #[test]
    fn the_game_standard_names_this_program_and_the_launchpad() {
        assert_eq!(bordrless_game::COMPANION_PROGRAM_ID, crate::ID);
        assert_eq!(bordrless_game::COMPANION_CREATOR_SEED, CREATOR_SEED);
        assert_eq!(bordrless_game::LAUNCH_PROGRAM_ID, LAUNCH_ID);
        let mint = Pubkey::new_unique();
        assert_eq!(
            bordrless_game::companion_creator_address(&mint),
            crate::state::Companion::creator(&mint).0
        );
        assert_eq!(
            bordrless_game::launch_address(&mint),
            bordrless_launch::client::launch_address(&mint)
        );
    }

    #[test]
    fn the_game_standard_names_the_token_program_and_the_launch_layout() {
        use anchor_lang::Discriminator;
        assert_eq!(bordrless_game::TOKEN_PROGRAM_ID, TOKEN_ID);
        assert_eq!(
            bordrless_game::TOKEN_EVENT_AUTHORITY,
            bordrless_token::EVENT_AUTHORITY_AND_BUMP.0
        );
        assert_eq!(
            bordrless_game::launch::LAUNCH_DISCRIMINATOR[..],
            bordrless_launch::state::Launch::DISCRIMINATOR[..]
        );
        assert_eq!(
            bordrless_game::launch::LAUNCH_STATUS_CURVE,
            bordrless_launch::constants::STATUS_CURVE
        );
        assert_eq!(
            bordrless_game::cpi::HOOK_AUTHORITY_SEED,
            bordrless_hook::HOOK_AUTHORITY_SEED
        );
        // `write_own_hook_data` builds exactly the token program's own `write_hook_data`.
        let hook = Pubkey::new_unique();
        let (authority, _) = bordrless_game::cpi::hook_authority_address(&hook);
        assert_eq!(authority, bordrless_hook::hook_authority(&hook).0);
        let (mint, holding) = (Pubkey::new_unique(), Pubkey::new_unique());
        let data = [7u8; 64];
        assert_eq!(
            bordrless_game::cpi::write_hook_data_ix(authority, mint, holding, data),
            bordrless_token::client::write_hook_data(authority, mint, holding, data)
        );
        // A holding reads as the token program writes it, with or without a delegate.
        for delegate in [None, Some(Pubkey::new_unique())] {
            let h = bordrless_token::state::Holding {
                version: 1,
                bump: 254,
                mint,
                owner: Pubkey::new_unique(),
                amount: 123_456_789,
                delegate,
                delegated_amount: 5,
                frozen: false,
                hook_data: [9; 64],
                reserved: [0; 16],
            };
            let mut bytes = Vec::new();
            anchor_lang::AccountSerialize::try_serialize(&h, &mut bytes).unwrap();
            bytes.resize(bordrless_token::state::Holding::LEN, 0);
            let v = bordrless_game::parse_holding(&bytes).expect("a holding");
            assert_eq!(
                (v.mint, v.owner, v.amount, v.delegate, v.hook_data),
                (h.mint, h.owner, h.amount, h.delegate, h.hook_data)
            );
            bytes[0] ^= 1;
            assert!(bordrless_game::parse_holding(&bytes).is_none());
        }
        // The launch's fields a game hook reads sit where `launch_offsets` says.
        use bordrless_game::launch::launch_offsets as o;
        assert_eq!(o::MINT, 8 + 1 + 1);
        assert_eq!(o::POOL, o::MINT + 32 + 32);
        assert_eq!(o::STATUS, o::POOL + 32 + 32);
        // Every kind runs the lottery's callbacks, exactly.
        use crate::state::GameKind;
        for kind in [
            GameKind::Lottery,
            GameKind::Jackpot,
            GameKind::Streak,
            GameKind::Strategy,
        ] {
            assert_eq!(kind.hook_flags(), LOTTERY_HOOK_FLAGS);
        }
        assert_eq!(HOOK_UPGRADE_AUTHORITIES.len(), 2);
    }

    #[test]
    fn the_strategy_interface_is_the_crates() {
        // The pool reader of the strategy crate reads the DEX's layout.
        let pool = bordrless_swap::state::Pool {
            version: 1,
            bump: 2,
            lp_mint_bump: 3,
            base_mint: Pubkey::new_unique(),
            quote_mint: Pubkey::new_unique(),
            lp_mint: Pubkey::new_unique(),
            base_vault: Pubkey::new_unique(),
            quote_vault: Pubkey::new_unique(),
            hook_program: Some(LAUNCH_ID),
            hook_flags: 7,
            lp_fee_bps: 125,
            protocol_fee_bps: 0,
            base_reserve: 11,
            quote_reserve: 12,
            virtual_base: 13,
            virtual_quote: 14,
            lp_supply: 15,
            protocol_fees_quote: 16,
            curve: true,
            creator: Pubkey::new_unique(),
            created_at: 17,
            last_swap_at: 18,
            swap_count: 19,
            base_volume: 20,
            quote_volume: 21,
            hook_signer_bump: 22,
            fee_model: 1,
            protocol_share_bps: 2_500,
            reserved: [0; 60],
        };
        for hook in [Some(LAUNCH_ID), None] {
            let mut p = pool.clone();
            p.hook_program = hook;
            let mut data = Vec::new();
            anchor_lang::AccountSerialize::try_serialize(&p, &mut data).unwrap();
            let r = bordrless_strategy::pool_reserves(&data).expect("reads");
            assert_eq!(
                (r.base_mint, r.quote_mint, r.lp_fee_bps),
                (p.base_mint, p.quote_mint, 125)
            );
            assert_eq!(
                (
                    r.base_reserve,
                    r.quote_reserve,
                    r.virtual_base,
                    r.virtual_quote
                ),
                (11, 12, 13, 14)
            );
        }
        const { assert!(MAX_STRATEGY_EXTRAS == 2 && MAX_STRATEGY_PER_TX == 4) };
    }

    #[test]
    fn game_bounds_hold_together() {
        // The smallest pot dwarfs the most one request can cost (the fee cap plus the pending
        // account's rent, at well above today's rent per byte).
        let rent_749_ceiling = (128 + crate::oracle::PENDING_LEN as u64) * 10_000;
        assert!(MIN_MIN_POT >= 5 * (crate::oracle::MAX_REQUEST_FEE + rent_749_ceiling));
        // A prize from the smallest pot is never below a wallet's rent-exempt minimum.
        assert!(
            crate::state::bps_of(MIN_MIN_POT, u64::from(MIN_PRIZE_BPS)) > 128 * 10_000,
            "a prize can open a wallet"
        );
        const { assert!(DEFAULT_POT_CAP >= MIN_MIN_POT) };
        const { assert!(MIN_POT_CAP >= MIN_MIN_POT && MIN_POT_CAP <= DEFAULT_POT_CAP) };
        // A game is retirable only after a second dormant period.
        const { assert!(RETIRE_DORMANT_PERIODS >= 2) };
        // A draw made early in the round after its own leaves the reveal its margin before the
        // last claim window, whatever the windows (at most half a round): the reveal is under half
        // of the shortest round (and of every other: `every_round_leaves_room_to_draw`).
        const {
            assert!(
                reveal_secs(bordrless_game::MIN_ROUND_SECS) > 0
                    && reveal_secs(bordrless_game::MIN_ROUND_SECS)
                        < (bordrless_game::MIN_ROUND_SECS / CLAIMS_PER_ROUND) as i64
            )
        };
        // At the shortest round and window, the reveal and a window take less than half the
        // round: a draw has over half of the round after its own to land.
        const {
            assert!(
                reveal_secs(bordrless_game::MIN_ROUND_SECS) + (MIN_CLAIM_WINDOW as i64)
                    < (bordrless_game::MIN_ROUND_SECS / CLAIMS_PER_ROUND) as i64
            )
        };
        // Every round of 40 minutes or more (every round of an hour or more included) keeps the
        // whole `REVEAL_SECS`; a five-minute round leaves 75 s.
        const {
            assert!(
                REVEAL_SECS > 0
                    && reveal_secs(2_400) == REVEAL_SECS
                    && reveal_secs(2_399) < REVEAL_SECS
                    && reveal_secs(3_600) == REVEAL_SECS
                    && reveal_secs(bordrless_game::MAX_ROUND_SECS) == REVEAL_SECS
                    && reveal_secs(u32::MAX) == REVEAL_SECS
                    && reveal_secs(300) == 75
            )
        };
        // Even the longest rounds give a dormant game a few draws at the lower minimum before
        // its pot may be retired.
        const { assert!(DORMANT_ROUNDS >= 2) };
        // A round of the shortest length takes at least one attempt of the shortest window; a
        // streak's epoch or a strategy's period, one of its own shortest window.
        const { assert!(MIN_CLAIM_WINDOW * CLAIMS_PER_ROUND <= bordrless_game::MIN_ROUND_SECS) };
        const {
            assert!(
                MIN_EPOCH_CLAIM_WINDOW * CLAIMS_PER_ROUND <= MIN_EPOCH_SECS
                    && MIN_EPOCH_SECS >= bordrless_game::MIN_ROUND_SECS
                    && MIN_EPOCH_SECS <= bordrless_game::MAX_ROUND_SECS
                    && MIN_EPOCH_CLAIM_WINDOW >= MIN_CLAIM_WINDOW
            )
        };
        assert_eq!(LOTTERY_HOOK_FLAGS, 145);
        // A stranded buyback is burned only well after keepers could have landed it.
        const { assert!(STRANDED_SECS >= MAX_BUYBACK_INTERVAL && STRANDED_INTERVALS >= 2) };
    }

    #[test]
    fn every_round_leaves_room_to_draw() {
        // For every round length a game can have: the reveal is positive and under half the round,
        // so a draw made as the round after its own begins is in time with the longest window the
        // round allows (half of it), and with the shortest window it has over half that round to
        // land. Rounds of 40 minutes or more keep exactly `REVEAL_SECS` (as every round did before
        // five-minute rounds); shorter ones a quarter of themselves.
        for secs in bordrless_game::MIN_ROUND_SECS..=bordrless_game::MAX_ROUND_SECS {
            let reveal = reveal_secs(secs);
            let half = i64::from(secs / CLAIMS_PER_ROUND);
            let round = i64::from(secs);
            assert!(reveal > 0 && reveal < half, "{secs}");
            assert!(
                round - half - reveal > 0,
                "{secs}: the longest window leaves no draw"
            );
            assert!(
                round - i64::from(MIN_CLAIM_WINDOW) - reveal > half,
                "{secs}: the shortest window leaves under half a round to draw"
            );
            let expected = if secs >= 2_400 {
                REVEAL_SECS
            } else {
                i64::from(secs / 4)
            };
            assert_eq!(reveal, expected, "{secs}");
        }
        assert_eq!(reveal_secs(0), 0);
    }

    #[test]
    fn a_scripts_instructions_are_named_as_anchor_names_them() {
        for (name, disc) in [
            ("global:companion_init", COMPANION_INIT_DISCRIMINATOR),
            ("global:companion_step", COMPANION_STEP_DISCRIMINATOR),
        ] {
            let hash = solana_sha256_hasher::hash(name.as_bytes()).to_bytes();
            assert_eq!(hash[..8], disc, "{name}");
        }
        const {
            assert!(
                MODE_CLASSIC != MODE_SNOWBALL
                    && MODE_SNOWBALL != MODE_SCRIPT
                    && MODE_SCRIPT != MODE_CLASSIC
            )
        };
        const { assert!(MIN_SCRIPT_INTERVAL > 0 && MIN_SCRIPT_INTERVAL <= MAX_SCRIPT_INTERVAL) };
        // The bounty is never above what a step's fee is worth many times over.
        const { assert!(SCRIPT_BOUNTY <= 100_000) };
    }
}
