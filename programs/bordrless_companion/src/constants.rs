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
/// Each claim attempt is open from five minutes to a day.
pub const MIN_CLAIM_WINDOW: u32 = 300;
pub const MAX_CLAIM_WINDOW: u32 = 86_400;
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
/// claim and only a sender racing every keeper would draw.
pub const REVEAL_SECS: i64 = 600;
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
        // last claim window, whatever the windows (at most half a round).
        const {
            assert!(
                REVEAL_SECS > 0
                    && REVEAL_SECS < (bordrless_game::MIN_ROUND_SECS / CLAIMS_PER_ROUND) as i64
            )
        };
        // Even the longest rounds give a dormant game a few draws at the lower minimum before
        // its pot may be retired.
        const { assert!(DORMANT_ROUNDS >= 2) };
        // A round of the shortest length takes at least one attempt of the shortest window.
        const { assert!(MIN_CLAIM_WINDOW * CLAIMS_PER_ROUND <= bordrless_game::MIN_ROUND_SECS) };
        assert_eq!(LOTTERY_HOOK_FLAGS, 145);
        // A stranded buyback is burned only well after keepers could have landed it.
        const { assert!(STRANDED_SECS >= MAX_BUYBACK_INTERVAL && STRANDED_INTERVALS >= 2) };
    }
}
