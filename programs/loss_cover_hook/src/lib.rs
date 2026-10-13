//! `loss_cover_hook`: the token hook of a Loss cover coin (`docs/loss-cover.md` in the monorepo,
//! `docs/companions.md` "Loss cover" here). It keeps, in the 64 bytes of hook data the token program
//! stores inside each holding, the holding's average entry price, the tokens that basis covers, the
//! cover it has been paid and the round it was last recorded in. The Loss cover script reads these
//! and names the holders who are under water; the Bordrless companion pays them SOL from its pot and
//! has this program record each payment (`record_payout`). This program only keeps the records: it
//! never sees SOL, never refuses a transfer and takes no cut.
//!
//! - **A buy** (the source's owner is the launch pool, the destination's a wallet) blends the
//!   buyer's basis with the trade's curve price, fees aside, read from the pool as it stands in the
//!   callback: the DEX writes its reserves and exits the pool account before it delivers the output,
//!   so the price is `Q' * 10^12 / (B' + a)`, the reserves after the trade (real plus virtual) and the
//!   amount delivered (`trade_price`). With a reference price set, the basis never takes more than
//!   `ENTRY_CAP_BPS` above it: a buyer's own spike never sets a basis it can be covered for.
//! - **A plain transfer in** (any source owner but the pool) blends at the spot now: tokens that
//!   arrive by transfer carry no loss, so a wallet cannot hand its loss to another.
//! - **A send or a burn** cuts the record to what is left: the cover paid goes with the tokens pro
//!   rata (a partial sell keeps its cover), the entry stays, the `SOLD` flag is set. A holding left
//!   with nothing is cleared to zeros, so it can be closed.
//! - **The reference** moves at most once per `REFERENCE_SECS` by at most `REFERENCE_STEP_BPS`
//!   toward the spot, on any callback, from the first spot seen. It only bounds a buy's entry.
//! - **`record_payout(lamports, tag)`**, signed only by the companion program's
//!   `PDA(["payout-authority"])`: `paid` grows by `lamports`, `round` becomes `tag`, the record is
//!   written through the token program's `write_hook_data`, signed by this program's
//!   `["hook-authority"]`. A holding with no record is refused: the script never asks for one.
//!
//! Who never holds a record: the launch (`["launch", mint]` under the launchpad), its pool (derived
//! at `prepare` from the launchpad's config), the companion's creator address, the default key and
//! every address off the ed25519 curve. The callbacks make no CPI and emit no event: under a
//! companion launch they run at stack height 5, Solana's limit.
//!
//! For a launch: `prepare` for the new mint, signed by the mint's keypair; a `LaunchConfig` naming
//! this program with [`FLAGS`]; then the launch. Registry extras: the state (writable), the launch
//! (read-only), the pool (read-only), so every transfer of the token carries three extra accounts.

#![allow(unexpected_cfgs)]

use anchor_lang::prelude::*;
use bordrless_game::{companion_creator_address, eligible, write_own_hook_data};
use bordrless_hook::{
    hook_accounts_address, token_flags, write_registry, AccountSource, ExtraAccount,
    HookAccountList, HookReturn, Seed, TokenHookArgs, TokenOp, HOOK_DATA_LEN,
};
use bordrless_launch::state::Config as LaunchConfig;
use bordrless_strategy::{pool_reserves, PoolReserves};
use bordrless_token::client as token_client;

declare_id!("6KMdXC3Fxovp3pnUsdQuUbderPzZtTA4t6R1jacYJWQr");

#[cfg(not(feature = "no-entrypoint"))]
solana_security_txt::security_txt! {
    name: "Bordrless Loss cover hook",
    project_url: "https://github.com/BordrlessDex/bordrless-programs",
    contacts: "link:https://github.com/BordrlessDex/bordrless-programs/security/advisories/new",
    policy: "https://github.com/BordrlessDex/bordrless-programs/blob/main/SECURITY.md",
    source_code: "https://github.com/BordrlessDex/bordrless-programs"
}

/// Layout version of [`LossCoverState`].
pub const VERSION: u8 = 1;
/// Layout version of a holding's [`Record`]; 0 is no record.
pub const RECORD_VERSION: u8 = 1;
/// `["state", mint]`: the hook's state for a mint.
pub const STATE_SEED: &[u8] = b"state";
/// The launch program's `["launch", mint]` seed.
pub const LAUNCH_SEED: &[u8] = b"launch";
/// The launch program.
pub const LAUNCH_ID: Pubkey = bordrless_launch::ID;
/// The launchpad's `["config"]`, read at `prepare` for the launch pools' LP fee.
pub const LAUNCH_CONFIG: Pubkey = bordrless_launch::constants::CONFIG_ADDRESS;
/// The DEX (`bordrless_swap`), whose `["pool", mint, quote, lp_fee_bps, hook]` is a launch's pool.
pub const SWAP_ID: Pubkey = Pubkey::from_str_const("GyzKSnnEu2uN5bBRecE4XYY2enbfR2D2MtxnbJPGy7hk");
/// The DEX's pool seed.
pub const POOL_SEED: &[u8] = b"pool";
/// Bridged SOL, every launch's quote.
pub const BRIDGED_SOL_MINT: Pubkey =
    Pubkey::from_str_const("A49oVhX22ExMwTEtFC6Y8nhBdZ4LJDGhdXLDn4c2f59i");
/// The token program's signer of every callback to this hook: `["hook-authority", loss_cover_hook]`
/// under the token program (checked against its derivation in the unit tests).
pub const TOKEN_HOOK_SIGNER: Pubkey =
    Pubkey::from_str_const("A2WKjdKC4PurtabNwhvKJvWdxKXLvt79qrzre7GE8XTe");
/// This program's `["hook-authority"]`, which signs the token program's `write_hook_data` (checked
/// against its derivation in the unit tests).
pub const HOOK_AUTHORITY: Pubkey =
    Pubkey::from_str_const("J7duBVXCMbN1BLp4mt7myzVvRsvXRPU3BAhFyksDSPAm");
/// Its canonical bump.
pub const HOOK_AUTHORITY_BUMP: u8 = 252;
/// The token program's event authority.
pub const TOKEN_EVENT_AUTHORITY: Pubkey = bordrless_token::EVENT_AUTHORITY_AND_BUMP.0;
/// The companion program's `PDA(["payout-authority"])`: the one signer of `record_payout` (the
/// companion's constant; the LiteSVM suite checks the two agree).
pub const PAYOUT_AUTHORITY: Pubkey =
    Pubkey::from_str_const("8XJcBCYmYyggTQBNvK3RausfyRMpmtjJTjKd53W3StkG");
/// The flags a `LaunchConfig` names for this hook: `before_transfer` and `before_burn`, which write
/// hook data. No deltas (the hook takes no cut), no mint callbacks.
pub const FLAGS: u16 =
    token_flags::BEFORE_TRANSFER | token_flags::BEFORE_BURN | token_flags::WRITES_HOOK_DATA;
/// Prices are lamports per base unit, times this (the companion's `PRICE_SCALE`): for a 6-decimal
/// token, SOL per whole token is `price / 10^15`.
pub const PRICE_SCALE: u128 = 1_000_000_000_000;
pub const BPS: u64 = 10_000;
/// A buy's basis takes at most this much above the reference price: 20%.
pub const ENTRY_CAP_BPS: u64 = 2_000;
/// The reference moves at most once per this many seconds, by at most `REFERENCE_STEP_BPS` of
/// itself.
pub const REFERENCE_SECS: i64 = 600;
pub const REFERENCE_STEP_BPS: u64 = 500;
/// `Record.flags`: the holding sold or burned since its last buy.
pub const FLAG_SOLD: u8 = 1;

/// Where each field sits in a holding's 64 bytes of hook data. Integers are little-endian.
pub mod record_offsets {
    /// `version: u8`: 1; 0 means no record (never written, or cleared).
    pub const VERSION: usize = 0;
    /// `flags: u8`: bit 0 (`FLAG_SOLD`), the holding sold or burned since its last buy.
    pub const FLAGS: usize = 1;
    /// `entry: u64`: the average entry price, lamports per base unit times `PRICE_SCALE`.
    pub const ENTRY: usize = 2;
    /// `amount: u64`: the tokens the record covers (the balance as of the last write).
    pub const AMOUNT: usize = 10;
    /// `paid: u64`: lamports of cover paid against this basis.
    pub const PAID: usize = 18;
    /// `updated_slot: u64`: the slot of the last write.
    pub const UPDATED_SLOT: usize = 26;
    /// `paid_at: i64`: when cover was last recorded (0: never).
    pub const PAID_AT: usize = 34;
    /// `basis_at: i64`: when a buy last blended the basis (0: none).
    pub const BASIS_AT: usize = 42;
    /// `round: u32`: the tag of the last `record_payout`: the round the script last measured or
    /// paid it in (0: none).
    pub const ROUND: usize = 50;
    /// 10 reserved bytes, zero.
    pub const RESERVED: usize = 54;
}

/// A holding's record, as the hook data lays it out ([`record_offsets`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Record {
    pub version: u8,
    pub flags: u8,
    pub entry: u64,
    pub amount: u64,
    pub paid: u64,
    pub updated_slot: u64,
    pub paid_at: i64,
    pub basis_at: i64,
    pub round: u32,
}

fn u64_at(data: &[u8], at: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&data[at..at + 8]);
    u64::from_le_bytes(b)
}

impl Record {
    /// Reads a holding's hook data. All zeros (a holding never written, or cleared) is no record.
    pub fn decode(data: &[u8; HOOK_DATA_LEN]) -> Self {
        use record_offsets as o;
        let mut round = [0u8; 4];
        round.copy_from_slice(&data[o::ROUND..o::ROUND + 4]);
        Self {
            version: data[o::VERSION],
            flags: data[o::FLAGS],
            entry: u64_at(data, o::ENTRY),
            amount: u64_at(data, o::AMOUNT),
            paid: u64_at(data, o::PAID),
            updated_slot: u64_at(data, o::UPDATED_SLOT),
            paid_at: u64_at(data, o::PAID_AT) as i64,
            basis_at: u64_at(data, o::BASIS_AT) as i64,
            round: u32::from_le_bytes(round),
        }
    }

    /// The hook data to write (the reserved bytes zero).
    pub fn encode(&self) -> [u8; HOOK_DATA_LEN] {
        use record_offsets as o;
        let mut out = [0u8; HOOK_DATA_LEN];
        out[o::VERSION] = self.version;
        out[o::FLAGS] = self.flags;
        out[o::ENTRY..o::ENTRY + 8].copy_from_slice(&self.entry.to_le_bytes());
        out[o::AMOUNT..o::AMOUNT + 8].copy_from_slice(&self.amount.to_le_bytes());
        out[o::PAID..o::PAID + 8].copy_from_slice(&self.paid.to_le_bytes());
        out[o::UPDATED_SLOT..o::UPDATED_SLOT + 8].copy_from_slice(&self.updated_slot.to_le_bytes());
        out[o::PAID_AT..o::PAID_AT + 8].copy_from_slice(&self.paid_at.to_le_bytes());
        out[o::BASIS_AT..o::BASIS_AT + 8].copy_from_slice(&self.basis_at.to_le_bytes());
        out[o::ROUND..o::ROUND + 4].copy_from_slice(&self.round.to_le_bytes());
        out
    }

    /// Whether the holding has a record (version 1).
    pub fn is_set(&self) -> bool {
        self.version == RECORD_VERSION
    }
}

/// `amount * part / whole`, rounded down, in u128 (0 for a `whole` of 0).
fn mul_div(amount: u64, part: u64, whole: u64) -> u64 {
    if whole == 0 {
        return 0;
    }
    (u128::from(amount) * u128::from(part) / u128::from(whole)) as u64
}

/// A u128 price as the record keeps it: saturating.
fn price_u64(p: u128) -> u64 {
    u64::try_from(p).unwrap_or(u64::MAX)
}

/// The pool's spot: its quote side over its base side, real and virtual, times `PRICE_SCALE`.
/// `None` without a base side.
pub fn spot_of(r: &PoolReserves) -> Option<u64> {
    let quote = u128::from(r.quote_reserve) + u128::from(r.virtual_quote);
    let base = u128::from(r.base_reserve) + u128::from(r.virtual_base);
    (base > 0).then(|| price_u64(quote * PRICE_SCALE / base))
}

/// The curve price of a buy that delivers `amount`, read from the pool after the trade (the DEX
/// writes its reserves before it delivers): `Q' * 10^12 / (B' + a)`, fees aside.
pub fn trade_price(r: &PoolReserves, amount: u64) -> Option<u64> {
    let quote = u128::from(r.quote_reserve) + u128::from(r.virtual_quote);
    let base = u128::from(r.base_reserve) + u128::from(r.virtual_base) + u128::from(amount);
    (base > 0).then(|| price_u64(quote * PRICE_SCALE / base))
}

/// `price`, at most `ENTRY_CAP_BPS` above `reference` when the reference is set.
pub fn capped(price: u64, reference: u64) -> u64 {
    if reference == 0 {
        return price;
    }
    let cap = u128::from(reference) * u128::from(BPS + ENTRY_CAP_BPS) / u128::from(BPS);
    price.min(price_u64(cap))
}

/// `reference` moved toward `spot` by at most `REFERENCE_STEP_BPS` of itself (all the way when
/// unset).
pub fn step_toward(reference: u64, spot: u64) -> u64 {
    if reference == 0 {
        return spot;
    }
    let step = mul_div(reference, REFERENCE_STEP_BPS, BPS);
    if spot > reference {
        spot.min(reference.saturating_add(step))
    } else {
        spot.max(reference.saturating_sub(step))
    }
}

/// A receive of `amount` at price `price` into a holding whose record is `r` (its balance before
/// the receive is `r.amount` when the record is set): the basis blended by weight, the amount
/// grown, the sold flag cleared, the basis time and the slot stamped. A holding with no record
/// takes the price as its basis.
pub fn on_receive(r: &mut Record, price: u64, amount: u64, now: i64, slot: u64) {
    let (entry, held) = if r.is_set() {
        (r.entry, r.amount)
    } else {
        (0, 0)
    };
    let total = u128::from(held) + u128::from(amount);
    let entry = if total == 0 {
        price
    } else {
        // Exact when the two products fit a u128 together (every real case); term by term,
        // within a unit, when they do not: a callback never panics.
        let a = u128::from(entry) * u128::from(held);
        let b = u128::from(price) * u128::from(amount);
        let blended = match a.checked_add(b) {
            Some(sum) => sum / total,
            None => a / total + b / total,
        };
        price_u64(blended)
    };
    *r = Record {
        version: RECORD_VERSION,
        flags: 0,
        entry,
        amount: held.saturating_add(amount),
        paid: if r.is_set() { r.paid } else { 0 },
        updated_slot: slot,
        paid_at: if r.is_set() { r.paid_at } else { 0 },
        basis_at: now,
        round: if r.is_set() { r.round } else { 0 },
    };
}

/// A send or a burn that leaves the holding `left` tokens: the cover paid goes with what left, pro
/// rata (rounded down), the entry stays, the sold flag is set. Nothing left: the record is cleared.
pub fn on_send(r: &mut Record, left: u64, slot: u64) {
    if left == 0 {
        *r = Record::default();
        return;
    }
    if !r.is_set() {
        // Tokens with no record (none can arrive without a write): a record of what is left, no
        // basis, so they are never covered.
        *r = Record {
            version: RECORD_VERSION,
            flags: FLAG_SOLD,
            amount: left,
            updated_slot: slot,
            ..Record::default()
        };
        return;
    }
    r.paid = mul_div(r.paid, left, r.amount);
    r.amount = left;
    r.flags |= FLAG_SOLD;
    r.updated_slot = slot;
}

/// Instructions of the Loss cover hook.
#[program]
pub mod loss_cover_hook {
    use super::*;

    /// Prepares the hook for `mint`, which need not exist yet (a launch's mint: the launchpad
    /// creates it naming this program as its hook): the state and the extra-accounts registry. The
    /// launch pool's address follows from the launchpad's config (its LP fee). The mint's keypair
    /// signs; once per mint.
    pub fn prepare(ctx: Context<Prepare>) -> Result<()> {
        let mint = ctx.accounts.mint.key();
        let (expected, registry_bump) = hook_accounts_address(&crate::ID, &mint);
        require_keys_eq!(
            ctx.accounts.registry.key(),
            expected,
            LossCoverError::WrongAccount
        );
        let now = Clock::get()?.unix_timestamp;
        let launch = Pubkey::find_program_address(&[LAUNCH_SEED, mint.as_ref()], &LAUNCH_ID).0;
        let lp_fee_bps = ctx.accounts.launch_config.lp_fee_bps;
        let fee_seed = lp_fee_bps.to_le_bytes();
        let pool = Pubkey::find_program_address(
            &[
                POOL_SEED,
                mint.as_ref(),
                BRIDGED_SOL_MINT.as_ref(),
                &fee_seed,
                LAUNCH_ID.as_ref(),
            ],
            &SWAP_ID,
        )
        .0;
        let creator = companion_creator_address(&mint);
        let payer = ctx.accounts.payer.key();
        let state_key = ctx.accounts.state.key();
        let state = &mut ctx.accounts.state;
        state.version = VERSION;
        state.bump = ctx.bumps.state;
        state.mint = mint;
        state.launch = launch;
        state.pool = pool;
        state.creator = creator;
        state.reference = 0;
        state.reference_at = 0;
        state.paid_total = 0;
        state.records = 0;
        state.prepared_by = payer;
        state.prepared_at = now;
        state.reserved = [0; 64];
        // The callbacks' extras: the state (written every transfer), the launch (read-only), the
        // pool (read-only: the price), each a PDA of its program with the mint among its seeds.
        let list = HookAccountList::new(vec![
            ExtraAccount {
                writable: true,
                source: AccountSource::Pda {
                    program: crate::ID,
                    seeds: vec![Seed::Literal(STATE_SEED.to_vec()), Seed::Account(1)],
                },
            },
            ExtraAccount {
                writable: false,
                source: AccountSource::Pda {
                    program: LAUNCH_ID,
                    seeds: vec![Seed::Literal(LAUNCH_SEED.to_vec()), Seed::Account(1)],
                },
            },
            ExtraAccount {
                writable: false,
                source: AccountSource::Pda {
                    program: SWAP_ID,
                    seeds: vec![
                        Seed::Literal(POOL_SEED.to_vec()),
                        Seed::Account(1),
                        Seed::Literal(BRIDGED_SOL_MINT.to_bytes().to_vec()),
                        Seed::Literal(fee_seed.to_vec()),
                        Seed::Literal(LAUNCH_ID.to_bytes().to_vec()),
                    ],
                },
            },
        ]);
        write_registry(
            &ctx.accounts.payer.to_account_info(),
            &ctx.accounts.registry.to_account_info(),
            &ctx.accounts.system_program.to_account_info(),
            &crate::ID,
            &mint,
            registry_bump,
            &list,
        )?;
        emit_cpi!(Prepared {
            mint,
            state: state_key,
            launch,
            pool,
            creator,
            lp_fee_bps,
            prepared_by: payer,
        });
        Ok(())
    }

    /// `before_transfer`: the reference moved when due; the sender's record cut to what it keeps
    /// (the cover paid pro rata with it), the receiver's basis blended at the trade's price (a buy
    /// out of the pool) or the spot (any other transfer); each answered as the holding's hook data
    /// when its owner may hold a record. Never refuses a transfer, never takes a cut.
    pub fn before_transfer(ctx: Context<Callback>, args: TokenHookArgs) -> Result<HookReturn> {
        require_keys_eq!(
            args.mint,
            ctx.accounts.mint.key(),
            LossCoverError::WrongAccount
        );
        let clock = Clock::get()?;
        let reserves = read_pool(&ctx.accounts.pool);
        let state = &mut ctx.accounts.state;
        move_reference(state, reserves.as_ref(), clock.unix_timestamp);
        if args.op != TokenOp::Transfer {
            return Ok(HookReturn::default());
        }
        let excluded = state.excluded();
        let mut answer = HookReturn::default();
        if eligible(&args.source_owner, &excluded) {
            let mut r = Record::decode(&args.source_hook_data);
            on_send(
                &mut r,
                args.source_balance.saturating_sub(args.amount),
                clock.slot,
            );
            answer.source_hook_data = Some(r.encode());
        }
        if eligible(&args.destination_owner, &excluded) {
            let mut r = Record::decode(&args.destination_hook_data);
            let price = if args.source_owner == state.pool {
                reserves.as_ref().and_then(|p| trade_price(p, args.amount))
            } else {
                reserves.as_ref().and_then(spot_of)
            };
            // No price to read (never, once the pool exists): the basis stays as it was.
            let price = capped(price.unwrap_or(r.entry), state.reference);
            on_receive(&mut r, price, args.amount, clock.unix_timestamp, clock.slot);
            answer.destination_hook_data = Some(r.encode());
        }
        Ok(answer)
    }

    /// `before_burn`: the reference moved when due, the burner's record cut as for a send.
    pub fn before_burn(ctx: Context<Callback>, args: TokenHookArgs) -> Result<HookReturn> {
        require_keys_eq!(
            args.mint,
            ctx.accounts.mint.key(),
            LossCoverError::WrongAccount
        );
        let clock = Clock::get()?;
        let reserves = read_pool(&ctx.accounts.pool);
        let state = &mut ctx.accounts.state;
        move_reference(state, reserves.as_ref(), clock.unix_timestamp);
        if args.op != TokenOp::Burn {
            return Ok(HookReturn::default());
        }
        let mut answer = HookReturn::default();
        if eligible(&args.source_owner, &state.excluded()) {
            let mut r = Record::decode(&args.source_hook_data);
            on_send(
                &mut r,
                args.source_balance.saturating_sub(args.amount),
                clock.slot,
            );
            answer.source_hook_data = Some(r.encode());
        }
        Ok(answer)
    }

    /// The companion's payout authority alone: records `lamports` of cover paid to `holding` (0: a
    /// touch) in round `tag`. The holding is of this mint, its owner may hold a record and the
    /// record exists; the record is written through the token program.
    pub fn record_payout(ctx: Context<RecordPayout>, lamports: u64, tag: u32) -> Result<()> {
        let mint_key = ctx.accounts.mint.key();
        let holding = token_client::read_holding(&ctx.accounts.holding)?;
        require_keys_eq!(holding.mint, mint_key, LossCoverError::WrongMint);
        let clock = Clock::get()?;
        let state = &mut ctx.accounts.state;
        require!(
            eligible(&holding.owner, &state.excluded()),
            LossCoverError::NotEligible
        );
        let mut r = Record::decode(&holding.hook_data);
        require!(r.is_set(), LossCoverError::NoRecord);
        r.paid = r
            .paid
            .checked_add(lamports)
            .ok_or(LossCoverError::MathOverflow)?;
        r.round = tag;
        r.updated_slot = clock.slot;
        if lamports > 0 {
            r.paid_at = clock.unix_timestamp;
        }
        state.paid_total = state.paid_total.saturating_add(lamports);
        state.records = state.records.saturating_add(1);
        write_own_hook_data(
            &crate::ID,
            &ctx.accounts.hook_authority.to_account_info(),
            &ctx.accounts.mint.to_account_info(),
            &ctx.accounts.holding.to_account_info(),
            &ctx.accounts.token_event_authority.to_account_info(),
            &ctx.accounts.token_program.to_account_info(),
            r.encode(),
        )
    }
}

/// The pool's reserves, when the account is the DEX's and reads.
fn read_pool(pool: &AccountInfo) -> Option<PoolReserves> {
    if *pool.owner != SWAP_ID {
        return None;
    }
    let data = pool.try_borrow_data().ok()?;
    pool_reserves(&data)
}

/// The reference moved toward the spot, at most once per `REFERENCE_SECS` (from the first spot
/// seen).
fn move_reference(state: &mut LossCoverState, reserves: Option<&PoolReserves>, now: i64) {
    let Some(spot) = reserves.and_then(spot_of) else {
        return;
    };
    if spot == 0 {
        return;
    }
    if state.reference == 0 || now >= state.reference_at.saturating_add(REFERENCE_SECS) {
        state.reference = step_toward(state.reference, spot);
        state.reference_at = now;
    }
}

/// The hook's state for one mint, at `["state", mint]`: 274 bytes.
#[account]
#[derive(InitSpace)]
pub struct LossCoverState {
    /// Layout version.
    pub version: u8,
    /// Bump of `["state", mint]`.
    pub bump: u8,
    pub mint: Pubkey,
    /// The launch program's `["launch", mint]`: never holds a record.
    pub launch: Pubkey,
    /// The launch pool, derived at `prepare` from the launchpad's config: never holds a record,
    /// and its sends are buys.
    pub pool: Pubkey,
    /// The companion's `["creator", mint]`: never holds a record.
    pub creator: Pubkey,
    /// A slow reference price (0 until set), and when it last moved.
    pub reference: u64,
    pub reference_at: i64,
    /// Lamports recorded by `record_payout`, and entries recorded (touches included).
    pub paid_total: u64,
    pub records: u64,
    /// Who paid for `prepare`, and when.
    pub prepared_by: Pubkey,
    pub prepared_at: i64,
    /// Reserved.
    pub reserved: [u8; 64],
}

impl LossCoverState {
    /// Account size.
    pub const LEN: usize = 8 + Self::INIT_SPACE;

    /// The owners that never hold a record, besides the default key and every address off the curve.
    pub fn excluded(&self) -> [Pubkey; 3] {
        [self.launch, self.pool, self.creator]
    }
}

/// Accounts of `prepare`.
#[event_cpi]
#[derive(Accounts)]
pub struct Prepare<'info> {
    /// Pays the rent.
    #[account(mut)]
    pub payer: Signer<'info>,
    /// The mint the hook is prepared for (not created yet); its keypair signs.
    pub mint: Signer<'info>,
    #[account(init, payer = payer, space = LossCoverState::LEN, seeds = [STATE_SEED, mint.key().as_ref()], bump)]
    pub state: Box<Account<'info, LossCoverState>>,
    /// CHECK: the registry PDA, created here (address-checked in the handler).
    #[account(mut)]
    pub registry: UncheckedAccount<'info>,
    /// The launchpad's config (address-checked): its LP fee is a seed of the launch's pool.
    #[account(address = LAUNCH_CONFIG @ LossCoverError::WrongAccount)]
    pub launch_config: Box<Account<'info, LaunchConfig>>,
    pub system_program: Program<'info, System>,
}

/// Accounts of the callbacks: the token program's prefix, then the registry's extras.
#[derive(Accounts)]
pub struct Callback<'info> {
    /// CHECK: the token program's signer for this hook (address- and signer-checked).
    #[account(signer, address = TOKEN_HOOK_SIGNER @ LossCoverError::BadHookSigner)]
    pub hook_signer: UncheckedAccount<'info>,
    /// CHECK: the mint (the token program's prefix; equals `args.mint`).
    pub mint: UncheckedAccount<'info>,
    /// CHECK: the source holding (balances, owners and hook data come from the arguments).
    pub source: UncheckedAccount<'info>,
    /// CHECK: the destination holding (the mint for a burn).
    pub destination: UncheckedAccount<'info>,
    /// CHECK: the authority.
    pub authority: UncheckedAccount<'info>,
    #[account(mut, seeds = [STATE_SEED, mint.key().as_ref()], bump = state.bump)]
    pub state: Box<Account<'info, LossCoverState>>,
    /// CHECK: the launch account of the mint (address-checked; not read).
    #[account(address = state.launch @ LossCoverError::WrongAccount)]
    pub launch: UncheckedAccount<'info>,
    /// CHECK: the launch pool (address-checked), read for its reserves.
    #[account(address = state.pool @ LossCoverError::WrongAccount)]
    pub pool: UncheckedAccount<'info>,
}

/// Accounts of `record_payout`, in the order the companion builds them.
#[derive(Accounts)]
pub struct RecordPayout<'info> {
    /// The companion program's `PDA(["payout-authority"])`, signing by CPI.
    #[account(address = PAYOUT_AUTHORITY @ LossCoverError::NotTheCompanion)]
    pub payout_authority: Signer<'info>,
    /// CHECK: the mint (the token program checks its hook is this program).
    pub mint: UncheckedAccount<'info>,
    /// CHECK: the holding, read as the token program's (owner and discriminator checked) and of
    /// `mint`; the token program writes its hook data.
    #[account(mut)]
    pub holding: UncheckedAccount<'info>,
    #[account(mut, seeds = [STATE_SEED, mint.key().as_ref()], bump = state.bump)]
    pub state: Box<Account<'info, LossCoverState>>,
    /// CHECK: this program's `["hook-authority"]` (address-checked), which signs `write_hook_data`.
    #[account(address = HOOK_AUTHORITY @ LossCoverError::WrongAccount)]
    pub hook_authority: UncheckedAccount<'info>,
    /// CHECK: the token program (address-checked).
    #[account(address = bordrless_token::ID @ LossCoverError::WrongAccount)]
    pub token_program: UncheckedAccount<'info>,
    /// CHECK: its event authority (address-checked).
    #[account(address = TOKEN_EVENT_AUTHORITY @ LossCoverError::WrongAccount)]
    pub token_event_authority: UncheckedAccount<'info>,
}

/// The hook was prepared for a mint.
#[event]
pub struct Prepared {
    pub mint: Pubkey,
    pub state: Pubkey,
    pub launch: Pubkey,
    pub pool: Pubkey,
    /// The companion's creator address for the mint.
    pub creator: Pubkey,
    /// The launchpad's LP fee when prepared (a seed of the pool).
    pub lp_fee_bps: u16,
    pub prepared_by: Pubkey,
}

/// Errors.
#[error_code]
pub enum LossCoverError {
    #[msg("an account is not the one this mint's hook expects")]
    WrongAccount,
    #[msg("the hook signer is not the token program's signer for this hook")]
    BadHookSigner,
    #[msg("the holding is of another mint")]
    WrongMint,
    #[msg("this owner holds no record: the launch, its pool, the companion's creator address, or an address off the ed25519 curve")]
    NotEligible,
    #[msg("only the companion program's payout authority records a payout")]
    NotTheCompanion,
    #[msg("this holding has no record to pay against")]
    NoRecord,
    #[msg("arithmetic overflow")]
    MathOverflow,
}

/// Instruction builders and addresses, as the tests, the SDK and the companion build them.
pub mod client {
    use super::*;
    use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
    use anchor_lang::{system_program, InstructionData, ToAccountMetas};

    /// This program's event authority.
    pub fn event_authority() -> Pubkey {
        crate::EVENT_AUTHORITY_AND_BUMP.0
    }

    /// The state of `mint`: `["state", mint]`.
    pub fn state_address(mint: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(&[STATE_SEED, mint.as_ref()], &crate::ID).0
    }

    /// The registry of `mint`: `["bordrless-hook-accounts", mint]`.
    pub fn registry_address(mint: &Pubkey) -> Pubkey {
        hook_accounts_address(&crate::ID, mint).0
    }

    /// The launch of `mint` (the launch program's `["launch", mint]`).
    pub fn launch_address(mint: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(&[LAUNCH_SEED, mint.as_ref()], &LAUNCH_ID).0
    }

    /// The launch pool of `mint` at `lp_fee_bps` (the launchpad's config's).
    pub fn pool_address(mint: &Pubkey, lp_fee_bps: u16) -> Pubkey {
        Pubkey::find_program_address(
            &[
                POOL_SEED,
                mint.as_ref(),
                BRIDGED_SOL_MINT.as_ref(),
                &lp_fee_bps.to_le_bytes(),
                LAUNCH_ID.as_ref(),
            ],
            &SWAP_ID,
        )
        .0
    }

    /// The registry's extras, in its order, for every token instruction on `mint` (after the hook
    /// program and the token program's signer for it): the state, writable; the launch and the
    /// pool, read-only.
    pub fn extras(mint: &Pubkey, lp_fee_bps: u16) -> Vec<AccountMeta> {
        vec![
            AccountMeta::new(state_address(mint), false),
            AccountMeta::new_readonly(launch_address(mint), false),
            AccountMeta::new_readonly(pool_address(mint, lp_fee_bps), false),
        ]
    }

    /// `prepare` for `mint` (whose keypair signs), `payer` paying.
    pub fn prepare(payer: Pubkey, mint: Pubkey) -> Instruction {
        Instruction {
            program_id: crate::ID,
            accounts: crate::accounts::Prepare {
                payer,
                mint,
                state: state_address(&mint),
                registry: registry_address(&mint),
                launch_config: LAUNCH_CONFIG,
                system_program: system_program::ID,
                event_authority: event_authority(),
                program: crate::ID,
            }
            .to_account_metas(None),
            data: crate::instruction::Prepare {}.data(),
        }
    }

    /// `record_payout(lamports, tag)` for `owner`'s holding of `mint`, as the companion signs it.
    pub fn record_payout(mint: Pubkey, owner: Pubkey, lamports: u64, tag: u32) -> Instruction {
        Instruction {
            program_id: crate::ID,
            accounts: crate::accounts::RecordPayout {
                payout_authority: PAYOUT_AUTHORITY,
                mint,
                holding: token_client::holding_address(&mint, &owner),
                state: state_address(&mint),
                hook_authority: HOOK_AUTHORITY,
                token_program: bordrless_token::ID,
                token_event_authority: TOKEN_EVENT_AUTHORITY,
            }
            .to_account_metas(None),
            data: crate::instruction::RecordPayout { lamports, tag }.data(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_token_programs_signer_for_this_hook() {
        assert_eq!(TOKEN_HOOK_SIGNER, token_client::hook_signer(&crate::ID));
    }

    #[test]
    fn this_programs_hook_authority() {
        assert_eq!(
            (HOOK_AUTHORITY, HOOK_AUTHORITY_BUMP),
            bordrless_hook::hook_authority(&crate::ID)
        );
    }

    #[test]
    fn the_seeds_and_programs_are_the_protocols() {
        assert_eq!(LAUNCH_SEED, bordrless_launch::constants::LAUNCH_SEED);
        assert_eq!(bordrless_game::LAUNCH_PROGRAM_ID, LAUNCH_ID);
        assert_eq!(LAUNCH_CONFIG, bordrless_launch::client::config_address());
        assert_eq!(bordrless_game::HOOK_DATA_LEN, HOOK_DATA_LEN);
        assert_eq!(TOKEN_EVENT_AUTHORITY, bordrless_game::TOKEN_EVENT_AUTHORITY);
        let mint = Pubkey::new_unique();
        assert_eq!(
            client::launch_address(&mint),
            bordrless_game::launch_address(&mint)
        );
        assert_eq!(
            client::pool_address(&mint, 30),
            bordrless_launch::client::pool_address(&mint, &BRIDGED_SOL_MINT, 30)
        );
    }

    #[test]
    fn the_state_is_274_bytes_at_its_offsets() {
        let s = LossCoverState {
            version: VERSION,
            bump: 254,
            mint: Pubkey::new_unique(),
            launch: Pubkey::new_unique(),
            pool: Pubkey::new_unique(),
            creator: Pubkey::new_unique(),
            reference: 1,
            reference_at: 2,
            paid_total: 3,
            records: 4,
            prepared_by: Pubkey::new_unique(),
            prepared_at: 5,
            reserved: [0; 64],
        };
        let mut data = Vec::new();
        s.try_serialize(&mut data).unwrap();
        assert_eq!(data.len(), LossCoverState::LEN);
        assert_eq!(LossCoverState::LEN, 274);
        assert_eq!(&data[10..42], s.mint.as_ref());
        assert_eq!(&data[42..74], s.launch.as_ref());
        assert_eq!(&data[74..106], s.pool.as_ref());
        assert_eq!(&data[106..138], s.creator.as_ref());
        assert_eq!(&data[138..146], &1u64.to_le_bytes());
        assert_eq!(&data[146..154], &2i64.to_le_bytes());
        assert_eq!(&data[154..162], &3u64.to_le_bytes());
        assert_eq!(&data[162..170], &4u64.to_le_bytes());
        assert_eq!(&data[170..202], s.prepared_by.as_ref());
        assert_eq!(&data[202..210], &5i64.to_le_bytes());
        assert_eq!(&data[210..], &[0u8; 64][..]);
        // The registry's magic is no state's discriminator.
        assert_ne!(&data[..8], &bordrless_hook::HOOK_ACCOUNTS_MAGIC[..]);
    }

    #[test]
    fn a_record_round_trips_at_its_offsets() {
        let r = Record {
            version: RECORD_VERSION,
            flags: FLAG_SOLD,
            entry: 0x0102_0304_0506_0708,
            amount: 9,
            paid: 10,
            updated_slot: 11,
            paid_at: -12,
            basis_at: 13,
            round: 14,
        };
        let data = r.encode();
        assert_eq!(Record::decode(&data), r);
        assert_eq!(data[0], 1);
        assert_eq!(data[1], 1);
        assert_eq!(&data[2..10], &r.entry.to_le_bytes());
        assert_eq!(&data[10..18], &9u64.to_le_bytes());
        assert_eq!(&data[18..26], &10u64.to_le_bytes());
        assert_eq!(&data[26..34], &11u64.to_le_bytes());
        assert_eq!(&data[34..42], &(-12i64).to_le_bytes());
        assert_eq!(&data[42..50], &13i64.to_le_bytes());
        assert_eq!(&data[50..54], &14u32.to_le_bytes());
        assert_eq!(&data[54..], &[0u8; 10][..]);
        assert_eq!(Record::decode(&[0; 64]), Record::default());
        assert!(!Record::default().is_set());
        assert_eq!(Record::default().encode(), [0; 64]);
    }

    #[test]
    fn a_buy_blends_the_basis_and_a_send_cuts_the_cover_pro_rata() {
        let mut r = Record::default();
        on_receive(&mut r, 100, 1_000, 50, 7);
        assert_eq!(
            r,
            Record {
                version: 1,
                flags: 0,
                entry: 100,
                amount: 1_000,
                paid: 0,
                updated_slot: 7,
                paid_at: 0,
                basis_at: 50,
                round: 0,
            }
        );
        // A second buy at 300 of as much again: the average, 200.
        r.paid = 40;
        r.round = 3;
        r.paid_at = 20;
        r.flags = FLAG_SOLD;
        on_receive(&mut r, 300, 1_000, 60, 8);
        assert_eq!(
            (r.entry, r.amount, r.paid, r.round, r.paid_at),
            (200, 2_000, 40, 3, 20)
        );
        assert_eq!((r.flags, r.basis_at, r.updated_slot), (0, 60, 8));
        // A send of half: the cover goes with it pro rata, the entry stays, the flag is set.
        on_send(&mut r, 1_000, 9);
        assert_eq!(
            (r.entry, r.amount, r.paid, r.flags, r.updated_slot),
            (200, 1_000, 20, FLAG_SOLD, 9)
        );
        assert_eq!((r.basis_at, r.round, r.paid_at), (60, 3, 20));
        // Rounded down: 20 * 333 / 1000 = 6.
        on_send(&mut r, 333, 10);
        assert_eq!((r.amount, r.paid), (333, 6));
        // Everything out: cleared.
        on_send(&mut r, 0, 11);
        assert_eq!(r, Record::default());
        // Tokens without a record that leave: a record of what stays, with no basis.
        let mut none = Record::default();
        on_send(&mut none, 5, 12);
        assert_eq!(
            (none.version, none.entry, none.amount, none.flags),
            (1, 0, 5, FLAG_SOLD)
        );
        // No overflow at the extremes.
        let mut big = Record {
            version: 1,
            entry: u64::MAX,
            amount: u64::MAX,
            paid: u64::MAX,
            ..Record::default()
        };
        on_receive(&mut big, u64::MAX, u64::MAX, 0, 0);
        assert_eq!((big.entry, big.amount), (u64::MAX - 1, u64::MAX));
        on_send(&mut big, u64::MAX / 2, 0);
        assert_eq!(big.paid, u64::MAX / 2);
    }

    #[test]
    fn prices_the_cap_and_the_reference() {
        let r = PoolReserves {
            base_mint: Pubkey::default(),
            quote_mint: Pubkey::default(),
            lp_fee_bps: 30,
            base_reserve: 700_000_000_000_000,
            quote_reserve: 2_000_000_000,
            virtual_base: 50_000_000_000_000,
            virtual_quote: 28_125_000_000,
        };
        // 30.125e9 * 1e12 / 750e12 = 40_166_666.
        assert_eq!(spot_of(&r), Some(40_166_666));
        // A buy delivering 50e12: the price over the base before it left.
        assert_eq!(trade_price(&r, 50_000_000_000_000), Some(37_656_250));
        let empty = PoolReserves {
            base_reserve: 0,
            virtual_base: 0,
            ..r
        };
        assert_eq!(spot_of(&empty), None);
        assert_eq!(trade_price(&empty, 0), None);
        assert_eq!(trade_price(&empty, 1), Some(u64::MAX));
        // The cap: 20% over the reference, none without one.
        assert_eq!(capped(1_000, 0), 1_000);
        assert_eq!(capped(1_000, 900), 1_000);
        assert_eq!(capped(1_300, 1_000), 1_200);
        assert_eq!(capped(1_300, u64::MAX), 1_300);
        // The reference steps 5% at a time, all the way when unset.
        assert_eq!(step_toward(0, 777), 777);
        assert_eq!(step_toward(1_000, 2_000), 1_050);
        assert_eq!(step_toward(1_000, 1_020), 1_020);
        assert_eq!(step_toward(1_000, 10), 950);
        assert_eq!(step_toward(1_000, 960), 960);
        assert_eq!(step_toward(u64::MAX, u64::MAX), u64::MAX);
    }

    #[test]
    fn flags_write_hook_data_and_take_no_cut() {
        assert_eq!(FLAGS, 145);
        assert_eq!(FLAGS & token_flags::TRANSFER_RETURNS_DELTA, 0);
        assert_eq!(
            FLAGS & (token_flags::BEFORE_MINT | token_flags::AFTER_MINT),
            0
        );
    }
}
