//! `bordrless_companion`: a launch whose creator is a program (`docs/companions.md`).
//!
//! A companion is made for a mint before it exists (`create`), then creates the launch through the
//! launchpad with its creator address, `PDA(["creator", mint])`, as the launch's creator (`launch`).
//! Every creator fee then lands with the companion, and its code alone decides what happens to it:
//! bought back and burned, streamed to holders through the kit, or paid to the launcher, by the
//! split fixed at `create`. The launcher's own buy (`dev_buy`) is made by the companion and vests
//! to them (`release`). Every step but `dev_buy` is permissionless and pays its sender a bounty:
//! the token runs its rewards with no keeper and no backend to trust.
//!
//! Games (v2): a companion may also run a game (`create_game`, before the launch): a lottery whose
//! tickets the coin's own token hook keeps under the game ticket standard (`bordrless-game`), whose
//! pot is a fourth part of every fee claim, and whose draws are verifiable (ORAO VRF, [`oracle`]).
//! The companion holds the pot and makes every payout; it only reads the hook, never calls it. The
//! protocol can cap or block an unaudited game hook (`set_hook_status`), never take a pot. A pot
//! that pays no prize for long is drawn from a lower floor, then retired to the buyback by anyone
//! (`retire`); a blocked hook's buyback that can't be spent is burned as SOL (`burn_stranded`): no
//! pot is locked for ever, and none is ever paid to anyone but a winner.
//!
//! Scripts (2026-10-12): a builder's own program may decide what a companion's pot does
//! (`create_script`, `script_step`), once Bordrless has approved its code (`approve`, bound to the
//! code's hash). The companion keeps custody and checks every step: the script spends the pot and
//! nothing else set aside, never the dev bag nor the creator address's lamports. Any companion
//! launch (a game's must be its game's) may also take a custom token hook, which every step that
//! moves the token carries.
//!
//! Coins quoted in tokens (2026-10-21, `docs/token-quotes.md` §7, "builder decides"): a script
//! companion may launch a coin quoted in 1 to 6 SPL tokens (`launch_token`). Its whole creator fee
//! is the script's, every leg's in that leg's token, in the creator address's associated token
//! accounts (the launchpad's `claim_creator_fees_token` pays them, anyone may send it); the script
//! trades the legs and pays in tokens as it decides, and the companion keeps those accounts the
//! creator's (`script_step`). A preset companion (a buyback, the launcher's part, a Snowball;
//! §7.4, the owner: "if the quote token is SPY, it should pay in SPY") launches one too: its parts
//! are kept per leg in the leg's token (`CompanionLegs`) and spent there by the leg steps
//! (`claim_fees_leg`, `withdraw_leg`, `buyback_leg`, `snowball_step_leg`); the SOL steps refuse
//! such a coin.
//!
//! Layout: [`state`], [`instructions`] (create and launch, the steps, the game, approvals and
//! scripts), [`invoke`] (how a step calls another program), [`oracle`], [`events`], [`error`],
//! [`client`].

#![allow(unexpected_cfgs)]

use anchor_lang::prelude::*;
use bordrless_launch::instructions::{CreateLaunchArgs, CreateTokenLaunchArgs};

pub mod client;
pub mod constants;
pub mod error;
pub mod events;
pub mod instructions;
pub mod invoke;
pub mod oracle;
pub mod state;

pub use instructions::*;

declare_id!("6ZUM1gWBH9hBBNoJoaVAGwSftyZ6CUda6vUZTW9MsJuo");

#[cfg(not(feature = "no-entrypoint"))]
solana_security_txt::security_txt! {
    name: "Bordrless companion",
    project_url: "https://github.com/BordrlessDex/bordrless-programs",
    contacts: "link:https://github.com/BordrlessDex/bordrless-programs/security/advisories/new",
    policy: "https://github.com/BordrlessDex/bordrless-programs/blob/main/SECURITY.md",
    source_code: "https://github.com/BordrlessDex/bordrless-programs"
}

#[program]
pub mod bordrless_companion {
    use super::*;

    /// Makes a companion for `mint` (not created yet): the split, the bounty, the buyback limits,
    /// the vesting; funds its creator address and creates its bridged-SOL holding.
    pub fn create(ctx: Context<Create>, args: CreateArgs) -> Result<()> {
        instructions::create::process_create(ctx, args)
    }

    /// Creates the launch through the launchpad, the creator address signing as its creator. The
    /// remaining accounts are `create_launch`'s.
    pub fn launch<'info>(
        ctx: Context<'info, LaunchIt<'info>>,
        args: CreateLaunchArgs,
    ) -> Result<()> {
        instructions::create::process_launch(ctx, args)
    }

    /// The beneficiary's buy, held by the companion and vesting to them.
    pub fn dev_buy<'info>(
        ctx: Context<'info, DevBuy<'info>>,
        lamports: u64,
        min_out: u64,
    ) -> Result<()> {
        instructions::steps::process_dev_buy(ctx, lamports, min_out)
    }

    /// Anyone: claims the creator fees and splits them.
    pub fn claim_fees<'info>(ctx: Context<'info, Step<'info>>) -> Result<()> {
        instructions::steps::process_claim_fees(ctx)
    }

    /// Anyone: buys the token back with the pending buyback (capped, spaced) and burns it.
    pub fn buyback<'info>(ctx: Context<'info, Step<'info>>) -> Result<()> {
        instructions::steps::process_buyback(ctx)
    }

    /// Anyone: streams the holders' part to holders through the kit.
    pub fn share<'info>(ctx: Context<'info, Step<'info>>) -> Result<()> {
        instructions::steps::process_share(ctx)
    }

    /// Anyone: pays the beneficiary their part as SOL.
    pub fn withdraw<'info>(ctx: Context<'info, Withdraw<'info>>) -> Result<()> {
        instructions::steps::process_withdraw(ctx)
    }

    /// Anyone: sends the beneficiary the dev bag's vested tokens.
    pub fn release<'info>(ctx: Context<'info, Step<'info>>) -> Result<()> {
        instructions::steps::process_release(ctx)
    }

    // ---- Games (v2) ----

    /// Makes the companion's game for `mint` (not launched yet; the mint signs): the pot's part of
    /// the split, the hook (prepared for the mint), the rounds, the minimum pot, the prize, the
    /// claim windows.
    pub fn create_game(ctx: Context<CreateGame>, args: CreateGameArgs) -> Result<()> {
        instructions::game::process_create_game(ctx, args)
    }

    /// Anyone: once `round` is over, commits its draw's seed, made from `slot` (one of the last
    /// three slots), and asks the oracle for it, the pot paying (or adopts a pending request for
    /// it), in one instruction; or rolls the round over: no tickets, too late, or the pot unable to
    /// pay for the oracle's request.
    pub fn draw<'info>(ctx: Context<'info, GameStep<'info>>, round: u32, slot: u64) -> Result<()> {
        instructions::game::process_draw(ctx, round, slot)
    }

    /// Anyone: stores the oracle's answer; the claim windows start.
    pub fn reveal<'info>(ctx: Context<'info, GameStep<'info>>) -> Result<()> {
        instructions::game::process_reveal(ctx)
    }

    /// Anyone: pays the draw's winner for attempt `attempt`, during its window.
    pub fn claim_prize<'info>(ctx: Context<'info, ClaimPrize<'info>>, attempt: u8) -> Result<()> {
        instructions::game::process_claim_prize(ctx, attempt)
    }

    /// Anyone: rolls over a draw that can't go on (nobody won it, or its claims ended).
    pub fn expire<'info>(ctx: Context<'info, GameStep<'info>>) -> Result<()> {
        instructions::game::process_expire(ctx)
    }

    /// Anyone: sends the pot of a game that has paid no prize for two dormant periods to the
    /// buyback, paying nobody.
    pub fn retire<'info>(ctx: Context<'info, GameStep<'info>>) -> Result<()> {
        instructions::game::process_retire(ctx)
    }

    /// Anyone: burns as SOL a blocked game hook's buyback that no buyback has spent or waited on
    /// for 30 days (the pot with it), paying nobody.
    pub fn burn_stranded<'info>(ctx: Context<'info, BurnStranded<'info>>) -> Result<()> {
        instructions::game::process_burn_stranded(ctx)
    }

    // ---- Phase 2: the jackpot and the streak ----

    /// Makes the companion's game of any kind (`create_game`'s arguments, and the kind's own
    /// settings): a lottery, a last-buyer jackpot or a holding streak.
    pub fn create_game_v2(
        ctx: Context<CreateGame>,
        args: CreateGameArgs,
        kind: GameKindArgs,
    ) -> Result<()> {
        instructions::game::process_create_game_v2(ctx, args, kind)
    }

    /// Anyone: closes the oldest jackpot round that is over, paying its last qualifying buyer if
    /// they still hold what they bought, else forfeiting it.
    pub fn settle<'info>(ctx: Context<'info, ClaimPrize<'info>>) -> Result<()> {
        instructions::kinds::process_settle(ctx)
    }

    /// Anyone: once streak epoch `epoch` is over, fixes its pot and total and opens its claims.
    pub fn close_epoch<'info>(ctx: Context<'info, GameStep<'info>>, epoch: u32) -> Result<()> {
        instructions::kinds::process_close_epoch(ctx, epoch)
    }

    /// Anyone, for any holding: pays its owner its share of streak epoch `epoch`, once.
    pub fn claim_share<'info>(ctx: Context<'info, ClaimShare<'info>>, epoch: u32) -> Result<()> {
        instructions::kinds::process_claim_share(ctx, epoch)
    }

    /// Anyone: once a receipt's epoch's claims have ended, returns its rent to whoever paid it.
    pub fn close_receipt(ctx: Context<CloseReceipt>) -> Result<()> {
        instructions::kinds::process_close_receipt(ctx)
    }

    /// The protocol's upgrade authority: whether a game hook is audited, its pots' cap while not,
    /// and whether it is blocked.
    pub fn set_hook_status(
        ctx: Context<SetHookStatus>,
        hook: Pubkey,
        args: HookStatusArgs,
    ) -> Result<()> {
        instructions::attest::process_set_hook_status(ctx, hook, args)
    }

    // ---- Phase 3a: attestations, audits tied to code, strategies ----

    /// Studio's attester: attests a program's code (rebuilt from its source, the hash matched with
    /// the chain, the checks, the simulator and the review passed).
    pub fn attest(ctx: Context<Attest>, args: AttestArgs) -> Result<()> {
        instructions::attest::process_attest(ctx, args)
    }

    /// The attester or the protocol's upgrade authority: revokes an attestation.
    pub fn revoke(ctx: Context<Revoke>) -> Result<()> {
        instructions::attest::process_revoke(ctx)
    }

    /// The protocol's upgrade authority: `set_hook_status`, with an audit tied to the code's hash
    /// (only of an immutable or Bordrless-managed program; the hash recomputed and kept).
    pub fn set_hook_status_v2(
        ctx: Context<SetHookStatus>,
        hook: Pubkey,
        args: HookStatusArgs,
        audited_hash: [u8; 32],
    ) -> Result<()> {
        instructions::attest::process_set_hook_status_v2(ctx, hook, args, audited_hash)
    }

    /// Makes the companion's strategy game for `mint` (not launched yet; the mint signs): the game's
    /// arguments (kind `Strategy`, a lottery hook's tickets) and the strategy's terms.
    pub fn create_strategy_game<'info>(
        ctx: Context<'info, CreateStrategyGame<'info>>,
        args: CreateGameArgs,
        s: StrategyArgs,
    ) -> Result<()> {
        instructions::strategy::process_create_strategy_game(ctx, args, s)
    }

    /// Anyone: once strategy period `period` is over, asks the strategy for its budget and opens
    /// its payments (or closes it with nothing).
    pub fn plan_period<'info>(ctx: Context<'info, PlanPeriod<'info>>, period: u32) -> Result<()> {
        instructions::strategy::process_plan_period(ctx, period)
    }

    /// Anyone, for up to `max_per_tx` holdings: asks the strategy what each gets of the open
    /// period's budget and pays it, once a holding.
    pub fn pay_strategy<'info>(
        ctx: Context<'info, PayStrategy<'info>>,
        period: u32,
        n: u8,
    ) -> Result<()> {
        instructions::strategy::process_pay_strategy(ctx, period, n)
    }

    /// Snowball (2026-10-12): before the launch, the mint signing, the companion becomes a market
    /// maker: the pot's part of every fee claim, the step, the trade sizes, the interval and the cap.
    pub fn create_snowball(ctx: Context<CreateSnowball>, args: CreateSnowballArgs) -> Result<()> {
        instructions::snowball::process_create_snowball(ctx, args)
    }

    /// Anyone: a Snowball's trade on its pool, a buy on a dip or a sell on a rise of its step from
    /// the reference (or a follow of the price when that side has nothing to trade); pays a bounty.
    pub fn snowball_step<'info>(ctx: Context<'info, SnowballStep<'info>>) -> Result<()> {
        instructions::snowball::process_snowball_step(ctx)
    }

    // ---- Approvals and script companions (2026-10-12) ----

    /// The protocol's upgrade authority or the script approver (`SCRIPT_APPROVER`): approves
    /// `program`'s code (its hash recomputed here) as a
    /// token hook (kind 0) or a companion script (kind 1), `author` its author (a script's earns
    /// half of the launcher's part of the creator fee of the companions made with it). Only a program nobody but Bordrless
    /// can change (immutable, Bordrless's keys, or a hook timelock); an upgrade voids it.
    pub fn approve(
        ctx: Context<Approve>,
        program: Pubkey,
        kind: u8,
        code_hash: [u8; 32],
        author: Pubkey,
    ) -> Result<()> {
        instructions::approval::process_approve(ctx, program, kind, code_hash, author)
    }

    /// The protocol's upgrade authority or the script approver: revokes `program`'s approval.
    pub fn revoke_approval(ctx: Context<RevokeApproval>, program: Pubkey) -> Result<()> {
        instructions::approval::process_revoke_approval(ctx, program)
    }

    /// Before the launch, the mint signing: the companion becomes a script companion, its pot's part
    /// of every fee claim spent by a builder's program (the script: approved by Bordrless, or
    /// immutable), whose
    /// `companion_init` runs here. The remaining accounts are forwarded to it.
    pub fn create_script<'info>(
        ctx: Context<'info, CreateScript<'info>>,
        args: CreateScriptArgs,
    ) -> Result<()> {
        instructions::script::process_create_script(ctx, args)
    }

    /// Anyone, once an interval: the script's `companion_step`, the creator address signing, then
    /// the companion's checks (the pot spent and nothing else, the dev bag and the lamports kept,
    /// custody unchanged); pays a bounty from the pot. The remaining accounts are forwarded to it.
    /// A script may answer payouts in its return data (Loss cover, 2026-10-12): each entry is
    /// checked against the script's payout terms, paid to the holding's owner as SOL and recorded
    /// by the launch's hook (`record_payout`).
    pub fn script_step<'info>(ctx: Context<'info, ScriptStep<'info>>) -> Result<()> {
        instructions::script::process_script_step(ctx)
    }

    /// `create_script` with payout terms (Loss cover, 2026-10-12): the bounds the script's payouts
    /// keep within, kept at `PDA(["payout-terms", mint])`, the payer paying its rent.
    pub fn create_script_v2<'info>(
        ctx: Context<'info, CreateScriptV2<'info>>,
        args: CreateScriptArgs,
        payout: PayoutTermsArgs,
    ) -> Result<()> {
        instructions::script::process_create_script_v2(ctx, args, payout)
    }

    /// Up to 10 wallets that share the beneficiary part, each a fixed share of it, set once before
    /// the launch by whoever holds the mint (2026-10-13): every `withdraw` pays them their parts as
    /// SOL, the beneficiary the rest.
    pub fn set_payees(ctx: Context<SetPayees>, entries: Vec<PayeeArgs>) -> Result<()> {
        instructions::payees::process_set_payees(ctx, entries)
    }

    /// Before the launch, whoever holds the mint lets the script pay any wallet SOL (wallet
    /// answers), within its payout terms (2026-10-13). Without it, a script pays holders only.
    pub fn allow_wallet_payouts(ctx: Context<AllowWalletPayouts>) -> Result<()> {
        instructions::payout::process_allow_wallet_payouts(ctx)
    }

    /// The launch's beneficiary (its launcher) or the script approver, before or after the launch:
    /// replaces a script's payout terms within `create_script_v2`'s bounds; the window restarts
    /// (2026-10-15). The terms name no recipient: they only bound how fast the pot pays holders.
    pub fn set_payout_terms(ctx: Context<SetPayoutTerms>, payout: PayoutTermsArgs) -> Result<()> {
        instructions::payout::process_set_payout_terms(ctx, payout)
    }

    /// The upgrade authority, with the DEX paused, the companion's launch native: turns the
    /// companion native (its SOL to its creator address's lamports). Idempotent.
    pub fn migrate_companion_to_native<'info>(
        ctx: Context<'info, CompanionForm<'info>>,
    ) -> Result<()> {
        native::process_migrate_companion_to_native(ctx)
    }

    /// The upgrade authority, with the DEX paused: turns a native companion back to bridged SOL.
    /// Idempotent.
    pub fn revert_companion_to_bridged<'info>(
        ctx: Context<'info, CompanionForm<'info>>,
    ) -> Result<()> {
        native::process_revert_companion_to_bridged(ctx)
    }

    // ---- Coins quoted in tokens (docs/token-quotes.md §7, 2026-10-21) ----

    /// A companion's launch of a coin quoted in tokens: `create_token_launch` through the
    /// launchpad, the creator address signing as its creator (as `launch` wraps `create_launch`).
    /// A script's whole creator fee is its pot (`pot_bps` 10,000): each leg's fee, in that leg's
    /// token, is claimed into the creator address's associated token account of it by the
    /// launchpad's `claim_creator_fees_token` (anyone), and the script spends it. A preset's
    /// (a buyback, the launcher's part, a Snowball) pots on the legs are made here
    /// (`CompanionLegs`, its rent paid by the creator address), each leg's reference its opening
    /// price. The companion is native from its launch (its SOL, if any reaches it, is its creator
    /// address's lamports). The remaining accounts are `create_token_launch`'s. Since
    /// `docs/token-quotes.md` §7.6 a game (its `Game` appended, writable) and a script with a
    /// launcher's part launch here too, their pots kept per leg (`CompanionLegs`).
    pub fn launch_token<'info>(
        ctx: Context<'info, LaunchToken<'info>>,
        args: CreateTokenLaunchArgs,
    ) -> Result<()> {
        instructions::create::process_launch_token(ctx, args)
    }

    /// Anyone: claims leg `index`'s creator fee into the creator address's token account of the
    /// leg's token (the launchpad's `claim_creator_fees_token`) and splits what arrived by the
    /// companion's split into the leg's parts, in that token; the bounty to the sender's token
    /// account of it, when passed.
    pub fn claim_fees_leg<'info>(ctx: Context<'info, LegStep<'info>>, index: u8) -> Result<()> {
        instructions::legs::process_claim_fees_leg(ctx, index)
    }

    /// Anyone: pays leg `index`'s launcher's part to the launcher's associated token account of the
    /// leg's token (made when missing, the sender paying).
    pub fn withdraw_leg<'info>(ctx: Context<'info, LegWithdraw<'info>>, index: u8) -> Result<()> {
        instructions::legs::process_withdraw_leg(ctx, index)
    }

    /// Anyone: buys the coin back on leg `index` with that leg's buyback part (capped, spaced, at
    /// no less than the leg's own curve gives less its fees and the slippage) and burns it.
    pub fn buyback_leg<'info>(ctx: Context<'info, LegStep<'info>>, index: u8) -> Result<()> {
        instructions::legs::process_buyback_leg(ctx, index)
    }

    /// Anyone: a Snowball's trade on leg `index` with that leg's pot, as `snowball_step` trades the
    /// pool of a SOL coin.
    pub fn snowball_step_leg<'info>(
        ctx: Context<'info, LegSnowballStep<'info>>,
        index: u8,
    ) -> Result<()> {
        instructions::legs::process_snowball_step_leg(ctx, index)
    }

    /// Anyone: shares leg `index`'s holders' part with the coin's holders in the leg's token through
    /// the kit's `share_token` (`docs/token-quotes.md` §7.5.1), the bounty in that token.
    pub fn share_leg<'info>(ctx: Context<'info, LegStep<'info>>, index: u8) -> Result<()> {
        instructions::legs::process_share_leg(ctx, index)
    }

    /// Anyone: with the coin's SOL switch (`docs/token-quotes.md` §7.5.2), sells leg `index`'s
    /// launcher's (`part` 0) or holders' (`part` 1) bucket, or a game's pot (`part` 2, §7.6), to
    /// SOL through the launchpad's `swap_to_sol`, the creator address signing, into the
    /// companion's SOL part of the same name (paid by `withdraw` and `share`; the game's SOL pot).
    pub fn swap_bucket_to_sol<'info>(
        ctx: Context<'info, LegSwapToSol<'info>>,
        index: u8,
        part: u8,
    ) -> Result<()> {
        instructions::legs::process_swap_bucket_to_sol(ctx, index, part)
    }

    /// Before the launch, the mint signing (`docs/token-quotes.md` §7.6): a game's least pot on a
    /// coin quoted in tokens, in basis points of the coin's opening value. `launch_token` requires
    /// it on such a coin whose SOL switch is off and marks the game's pot kept per leg, in the legs'
    /// tokens; a SOL coin's `launch` leaves it unread.
    pub fn set_game_token_terms(ctx: Context<SetGameTokenTerms>, min_pot_bps: u16) -> Result<()> {
        instructions::token_games::process_set_game_token_terms(ctx, min_pot_bps)
    }
}
