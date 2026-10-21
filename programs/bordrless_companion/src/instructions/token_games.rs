//! Games and scripts with a launcher's part on a coin quoted in tokens (`docs/token-quotes.md`
//! §7.6, 21 Oct 2026, the owner: quote tokens "compatible with every feature as normal SOL is").
//!
//! A game's pot on such a coin is kept per leg, in the leg's token, in the companion's
//! `CompanionLegs` (the presets' `LegPot.pending_pot`): every leg's creator fee is claimed into the
//! creator address's associated token account of its token and split there (`claim_fees_leg`), the
//! pot's part to the leg's pot. The game's steps are its SOL steps, with the pot's arithmetic per
//! leg:
//!
//! - **A pot's size**, for the thresholds only, is `pot_bps = Σ pending_pot_i * 10,000 /
//!   (virtual_quote_i * count)`: each leg's pot over the coin's opening value in that leg's token
//!   (every leg opens at the same value), summed: the pot as a share of what the coin was worth at its
//!   opening. No price is read. A game's least pot is `Game::min_pot_bps` (`set_game_token_terms`),
//!   a dormant game's `TOKEN_MIN_MIN_POT_BPS`, an unaudited hook's cap its lamport cap read as basis
//!   points (`HookTerms::cap_bps`); the part of a leg's pot above its share of the cap
//!   (`cap_bps * virtual_quote_i / 10,000`) moves to the leg's buyback, as on a SOL coin (a
//!   streak's locked epoch pot aside), and a blocked hook's whole pot does.
//! - **A prize is per leg**: a lottery draw and a streak epoch fix `prize_bps` of each leg's pot as
//!   that leg's prize (`LegPot.prize`, what was paid of it `prize_paid`); a jackpot round pays
//!   `prize_bps` of each leg's pot when it settles. Each leg's prize is paid in its token to the
//!   winner's associated token account of it, less the sender's `bounty_bps`, paid to its own
//!   associated account of the leg's token when it passes one. The winner's account of every leg
//!   with something to pay must be passed (`MissingAccount`) and writable (`PayoutTarget`): no
//!   sender can leave a leg out; it is made when missing, the sender paying, and only a leg whose
//!   account can't take its token now (frozen, or asking for a memo: the owner's own doing) keeps
//!   its part in the pot.
//! - **A blocked game's pot is never burned in its tokens**: it moves to each leg's buyback, which
//!   buys the coin back on that leg and burns the coin (`buyback_leg`, the owner's rule, 21 Oct).
//! - **A lottery's oracle is paid in SOL**: its float is `Companion.pending_pot` (lamports, booked
//!   from the launcher's `fund` at `launch_token`); SOL anyone sends the creator address tops it up
//!   (booked at the next `draw`). A float that can't pay rolls the round over (`OracleUnpaid`).
//! - **With the SOL switch on** the game is a SOL coin's: its pot's part of each leg is sold to SOL
//!   (`swap_bucket_to_sol(index, PART_POT)`) into `Companion.pending_pot`, and every step runs its
//!   SOL path (no token terms: `TokenGameTerms` at the launch otherwise).
//!
//! The accounts a step on a game in tokens adds to its remaining accounts, looked up by key: the
//! companion's `CompanionLegs` (writable), the coin's `Legs`; to pay a prize, per leg its quote
//! mint, the creator address's token account of it (writable), the winner's (writable) and,
//! optionally, the sender's (writable, for the bounty), the token programs, the associated token
//! account program, and the winner itself (to make its account).

use anchor_lang::prelude::*;
use anchor_lang::Discriminator;
use bordrless_launch::state::Legs;
use bordrless_swap::spl;

use crate::constants::*;
use crate::error::CompanionError;
use crate::events::*;
use crate::instructions::game::emit_event;
use crate::instructions::legs::receivable;
use crate::instructions::steps::find;
use crate::instructions::CreatorSeeds;
use crate::state::*;

/// A leg's `virtual_quote` within a leg of the launchpad's `Legs` (its opening value, in raw units
/// of its token).
const LEG_VIRTUAL_QUOTE: usize = 35;

/// A game's (or a script's) pots on a coin quoted in tokens: the companion's `CompanionLegs`, read
/// and written back here, and each leg's opening value (the launchpad's `Legs`).
pub(crate) struct TokenPots<'a, 'info> {
    info: &'a AccountInfo<'info>,
    pub legs: Box<CompanionLegs>,
    /// Each leg's `virtual_quote`: the coin's opening value in the leg's token.
    pub vq: [u64; MAX_COMPANION_LEGS],
}

impl<'a, 'info> TokenPots<'a, 'info> {
    /// The companion's legs of `mint` (writable, owned by this program) and the coin's `Legs` (the
    /// launchpad's, of this mint), found among `all` (`LegAccountsMissing`).
    pub(crate) fn load(all: &'a [AccountInfo<'info>], mint: &Pubkey) -> Result<Self> {
        let key = CompanionLegs::address(mint).0;
        let info = all
            .iter()
            .find(|a| *a.key == key)
            .ok_or(CompanionError::LegAccountsMissing)?;
        require!(
            *info.owner == crate::ID && info.is_writable,
            CompanionError::LegAccountsMissing
        );
        let legs = Box::new(CompanionLegs::try_deserialize(
            &mut &info.try_borrow_data()?[..],
        )?);
        require_keys_eq!(legs.mint, *mint, CompanionError::LegAccountsMissing);
        let src = all
            .iter()
            .find(|a| {
                *a.owner == LAUNCH_ID
                    && a.try_borrow_data().is_ok_and(|d| {
                        d.len() >= Legs::LEN
                            && d[..8] == *Legs::DISCRIMINATOR
                            && d[Legs::MINT_OFFSET..Legs::MINT_OFFSET + 32] == mint.to_bytes()
                    })
            })
            .ok_or(CompanionError::LegAccountsMissing)?;
        let data = src.try_borrow_data()?;
        let mut vq = [0u64; MAX_COMPANION_LEGS];
        let count = usize::from(legs.count).min(MAX_COMPANION_LEGS);
        for (i, v) in vq.iter_mut().enumerate().take(count) {
            let at = Legs::LEGS_OFFSET + i * Legs::LEG_LEN + LEG_VIRTUAL_QUOTE;
            *v = u64::from_le_bytes(data[at..at + 8].try_into().expect("8 bytes"));
        }
        Ok(Self { info, legs, vq })
    }

    /// The legs in use.
    pub(crate) fn count(&self) -> usize {
        usize::from(self.legs.count).min(MAX_COMPANION_LEGS)
    }

    /// The pot as a share of the coin's opening value: `Σ pending_pot_i * 10,000 / (virtual_quote_i
    /// * count)`, basis points (rounded down per leg).
    pub(crate) fn pot_bps(&self) -> u64 {
        let mut pots = [0u64; MAX_COMPANION_LEGS];
        for (i, p) in pots.iter_mut().enumerate().take(self.count()) {
            *p = self.legs.legs[i].pending_pot;
        }
        self.bps_of_amounts(&pots)
    }

    /// `amounts` (one per leg, in its token) as a share of the coin's opening value, as
    /// [`Self::pot_bps`] measures the pot.
    pub(crate) fn bps_of_amounts(&self, amounts: &[u64; MAX_COMPANION_LEGS]) -> u64 {
        let n = self.count() as u128;
        let mut sum: u128 = 0;
        for (i, a) in amounts.iter().enumerate().take(self.count()) {
            let vq = u128::from(self.vq[i]);
            if vq > 0 {
                sum = sum.saturating_add(u128::from(*a) * u128::from(BPS) / (vq * n));
            }
        }
        u64::try_from(sum).unwrap_or(u64::MAX)
    }

    /// Leg `i`'s share of a cap of `cap_bps` of the opening value, in its token.
    pub(crate) fn leg_cap(&self, i: usize, cap_bps: u64) -> u64 {
        u64::try_from(u128::from(cap_bps) * u128::from(self.vq[i]) / u128::from(BPS))
            .unwrap_or(u64::MAX)
    }

    /// Whether the pot holds what a draw, an epoch or a jackpot round needs: at least `min_pot_bps`
    /// (the game's least, in basis points of the opening value), or the cap when the cap is lower
    /// (`HookTerms::draw_threshold_bps`).
    pub(crate) fn holds(&self, terms: &HookTerms, min_pot_bps: u64) -> bool {
        let mut pots = [0u64; MAX_COMPANION_LEGS];
        for (i, p) in pots.iter_mut().enumerate().take(self.count()) {
            *p = self.legs.legs[i].pending_pot;
        }
        self.reaches(&pots, terms, min_pot_bps)
    }

    /// Whether `amounts` (one per leg, in its token) hold what [`Self::holds`] asks. When the cap is
    /// the threshold, a pot full at its cap holds it: every leg at least its share of the cap
    /// ([`Self::leg_cap`]), as a SOL coin's pot at its cap is drawn. (Each leg is cut to its share
    /// rounded down and measured rounded down again, so a pot cut to its cap can measure a little
    /// under `cap_bps`.)
    pub(crate) fn reaches(
        &self,
        amounts: &[u64; MAX_COMPANION_LEGS],
        terms: &HookTerms,
        min_pot_bps: u64,
    ) -> bool {
        if self.bps_of_amounts(amounts) >= terms.draw_threshold_bps(min_pot_bps) {
            return true;
        }
        match terms.cap_bps() {
            Some(cap) if cap <= min_pot_bps => {
                let mut any = false;
                for (i, a) in amounts.iter().enumerate().take(self.count()) {
                    let leg_cap = self.leg_cap(i, cap);
                    if *a < leg_cap {
                        return false;
                    }
                    any |= leg_cap > 0;
                }
                any
            }
            _ => false,
        }
    }

    /// Writes the companion's legs back.
    pub(crate) fn save(&self) -> Result<()> {
        let mut data = self.info.try_borrow_mut_data()?;
        let mut w: &mut [u8] = &mut data;
        self.legs.try_serialize(&mut w)?;
        Ok(())
    }
}

/// What `apply_status` does to a game's pot in tokens: under a blocked hook every leg's pot (and
/// any prize it holds) goes to the leg's buyback; else the part of each leg's pot above its share of
/// the cap does, a streak epoch's locked prize aside (`locked`). Answers how many legs moved.
pub(crate) fn enforce_terms_legs(
    event_authority: &AccountInfo,
    companion_key: Pubkey,
    c: &mut Companion,
    terms: &HookTerms,
    locked: bool,
    all: &[AccountInfo],
    now: i64,
) -> Result<u32> {
    let mut pots = TokenPots::load(all, &c.mint)?;
    let cap = terms.cap_bps();
    let mut moved_legs = 0u32;
    let mut changed = false;
    for i in 0..pots.count() {
        let leg_cap = cap.map(|cap| pots.leg_cap(i, cap));
        let p = &mut pots.legs.legs[i];
        let keep = if terms.blocked {
            0
        } else {
            match leg_cap {
                Some(cap) => {
                    let floor = if locked { p.prize_left() } else { 0 };
                    p.pending_pot.min(cap.max(floor))
                }
                None => p.pending_pot,
            }
        };
        if terms.blocked && (p.prize != 0 || p.prize_paid != 0) {
            // No prize is paid under a block.
            p.prize = 0;
            p.prize_paid = 0;
            changed = true;
        }
        let moved = p.pending_pot - keep;
        if moved == 0 {
            continue;
        }
        p.pending_pot = keep;
        p.pending_buyback = p
            .pending_buyback
            .checked_add(moved)
            .ok_or(CompanionError::MathOverflow)?;
        moved_legs += 1;
        changed = true;
        emit_event(
            event_authority,
            &LegPotToBuyback {
                companion: companion_key,
                index: i as u8,
                amount: moved,
                reason: u8::from(terms.blocked),
                pending_pot: keep,
            },
        )?;
    }
    if changed {
        pots.save()?;
    }
    if moved_legs > 0 && terms.blocked {
        c.stranded_burned_at = c.stranded_burned_at.max(now);
    }
    Ok(moved_legs)
}

/// Fixes each leg's prize at `prize_bps` of its pot now (a lottery draw at its request, a streak
/// epoch at its close), nothing of it paid. Answers the prizes and the pot's size then.
pub(crate) fn fix_prizes(pots: &mut TokenPots, prize_bps: u16) -> (Vec<u64>, u64) {
    let pot_bps = pots.pot_bps();
    let mut prizes = Vec::with_capacity(pots.count());
    for i in 0..pots.count() {
        let p = &mut pots.legs.legs[i];
        p.prize = bps_of(p.pending_pot, u64::from(prize_bps));
        p.prize_paid = 0;
        prizes.push(p.prize);
    }
    (prizes, pot_bps)
}

/// A leg's token accounts among a step's: its quote mint, its program and the creator address's
/// associated token account of it.
pub(crate) struct LegToken<'a, 'info> {
    pub quote_mint: &'a AccountInfo<'info>,
    pub program: &'a AccountInfo<'info>,
    pub creator_quote: &'a AccountInfo<'info>,
    pub decimals: u8,
}

impl<'a, 'info> LegToken<'a, 'info> {
    /// Leg `pot`'s accounts among `all` (`MissingAccount`).
    pub(crate) fn of(
        all: &'a [AccountInfo<'info>],
        pot: &LegPot,
        creator: &Pubkey,
    ) -> Result<Self> {
        let program_id = spl::program_id(pot.quote_program);
        Ok(Self {
            quote_mint: find(all, &pot.quote_mint)?,
            program: find(all, &program_id)?,
            creator_quote: find(
                all,
                &spl::associated_token_address(creator, &pot.quote_mint, &program_id),
            )?,
            decimals: pot.decimals,
        })
    }

    /// `owner`'s associated token account of the leg's token.
    pub(crate) fn account_of(&self, owner: &Pubkey) -> Pubkey {
        spl::associated_token_address(owner, self.quote_mint.key, self.program.key)
    }

    /// Pays `amount` from the creator address's account, the creator address signing.
    pub(crate) fn pay(
        &self,
        creator: &AccountInfo<'info>,
        seeds: &CreatorSeeds,
        to: &AccountInfo<'info>,
        amount: u64,
    ) -> Result<()> {
        if amount == 0 {
            return Ok(());
        }
        spl::transfer_checked(
            self.program,
            self.creator_quote,
            self.quote_mint,
            to,
            creator,
            amount,
            self.decimals,
            &[&seeds.seeds()],
        )
    }
}

/// Who pays out a leg's prize, and to whom its bounty goes.
pub(crate) struct LegPayout<'a, 'info> {
    pub all: &'a [AccountInfo<'info>],
    pub creator: &'a AccountInfo<'info>,
    pub seeds: &'a CreatorSeeds,
    /// The sender: pays a missing account's rent, takes the bounty in its own account.
    pub sender: &'a AccountInfo<'info>,
    pub event_authority: &'a AccountInfo<'info>,
    pub companion: Pubkey,
    pub bounty_bps: u16,
}

impl<'a, 'info> LegPayout<'a, 'info> {
    /// `owner`'s account of leg `t`'s token, which must be passed (`MissingAccount`) and writable
    /// (`PayoutTarget`: passed read-only it could take nothing, and the leg would be skipped while
    /// the step closes the draw or the round), made when missing (the owner passed too, the sender
    /// paying): `None` only when it can't take the token now (frozen, or asking for a memo), the
    /// owner's own doing.
    fn destination(
        &self,
        t: &LegToken<'a, 'info>,
        owner: &Pubkey,
    ) -> Result<Option<&'a AccountInfo<'info>>> {
        let key = t.account_of(owner);
        let dest = find(self.all, &key)?;
        require!(dest.is_writable, CompanionError::PayoutTarget);
        if *dest.owner == anchor_lang::system_program::ID && dest.data_is_empty() {
            let wallet = find(self.all, owner)?;
            spl::create_ata_idempotent(
                self.sender,
                dest,
                wallet,
                t.quote_mint,
                find(self.all, &anchor_lang::system_program::ID)?,
                t.program,
                find(self.all, &spl::ATA_PROGRAM_ID)?,
            )?;
        }
        Ok(receivable(dest, t.program.key, t.quote_mint.key, owner).then_some(dest))
    }

    /// Pays `amounts[i]` of each leg's token out of its pot to `owner`, less the sender's bounty
    /// (paid only into the sender's own account of the leg's token, when passed and able to take
    /// it): the leg's `pending_pot` falls by what left, its `bounties_total` grows by the bounty, and
    /// `LegPrizePaid` (`kind`) is emitted. The owner's account of every leg with something to pay
    /// must be passed and writable (`MissingAccount`, `PayoutTarget`); a leg whose account can't
    /// take its token now (frozen, a memo) keeps its part. Answers what left each leg (prize and
    /// bounty).
    pub(crate) fn pay(
        &self,
        pots: &mut TokenPots,
        owner: &Pubkey,
        amounts: &[u64; MAX_COMPANION_LEGS],
        kind: u8,
    ) -> Result<[u64; MAX_COMPANION_LEGS]> {
        let mut paid = [0u64; MAX_COMPANION_LEGS];
        for i in 0..pots.count() {
            let pot = pots.legs.legs[i];
            let amount = amounts[i].min(pot.pending_pot);
            if amount == 0 {
                continue;
            }
            let t = LegToken::of(self.all, &pot, self.creator.key)?;
            let Some(dest) = self.destination(&t, owner)? else {
                continue;
            };
            let bounty_to = {
                let key = t.account_of(self.sender.key);
                self.all.iter().find(|a| {
                    *a.key == key && receivable(a, t.program.key, t.quote_mint.key, self.sender.key)
                })
            };
            let bounty = match bounty_to {
                Some(_) => bps_of(amount, u64::from(self.bounty_bps)),
                None => 0,
            };
            t.pay(self.creator, self.seeds, dest, amount - bounty)?;
            if let Some(to) = bounty_to {
                t.pay(self.creator, self.seeds, to, bounty)?;
            }
            let p = &mut pots.legs.legs[i];
            p.pending_pot -= amount;
            p.bounties_total = p.bounties_total.saturating_add(bounty);
            paid[i] = amount;
            emit_event(
                self.event_authority,
                &LegPrizePaid {
                    companion: self.companion,
                    index: i as u8,
                    quote_mint: pot.quote_mint,
                    kind,
                    owner: *owner,
                    amount: amount - bounty,
                    bounty,
                },
            )?;
        }
        Ok(paid)
    }
}

/// `LegPrizePaid.kind` of a lottery draw's prize.
pub(crate) const PRIZE_DRAW: u8 = 0;
/// `LegPrizePaid.kind` of a jackpot round's prize.
pub(crate) const PRIZE_SETTLE: u8 = 1;
/// `LegPrizePaid.kind` of a streak share.
pub(crate) const PRIZE_SHARE: u8 = 2;

/// Each leg's pot after a `claim_fees_leg` of every leg now: its pot plus the pot's part
/// (`pot_bps`) of what its creator-fee account holds unclaimed (the launchpad's `leg-fees`, read
/// from the coin's `Legs`) and of what the companion address's account of its token holds above
/// the leg's parts (fees claimed with the launchpad's own `claim_creator_fees_token`, which anyone
/// may send: `claim_fees_leg` splits them), counted before any bounty or cap. An upper bound, on a
/// winner's side, as `fundable` is on a SOL coin: a round such a claim would fund is never closed
/// unfunded (`FeesUnclaimed`). Fees no claim can move count for nothing: a fee account its token's
/// issuer froze, or a leg whose companion address's account is frozen (`AccountState` 2), so such a
/// freeze never holds a round open. Each leg's fee account and the companion address's account of
/// each leg's token must be passed.
pub(crate) fn fundable_amounts(
    all: &[AccountInfo],
    pots: &TokenPots,
    c: &Companion,
) -> Result<[u64; MAX_COMPANION_LEGS]> {
    let mint = c.mint;
    let src = all
        .iter()
        .find(|a| {
            *a.owner == LAUNCH_ID
                && a.try_borrow_data().is_ok_and(|d| {
                    d.len() >= Legs::LEN
                        && d[..8] == *Legs::DISCRIMINATOR
                        && d[Legs::MINT_OFFSET..Legs::MINT_OFFSET + 32] == mint.to_bytes()
                })
        })
        .ok_or(CompanionError::LegAccountsMissing)?;
    let fee_keys: Vec<Pubkey> = {
        let data = src.try_borrow_data()?;
        (0..pots.count())
            .map(|i| {
                let at = Legs::LEGS_OFFSET + i * Legs::LEG_LEN + Legs::LEG_FEE_ACCOUNT;
                Pubkey::new_from_array(data[at..at + 32].try_into().expect("32 bytes"))
            })
            .collect()
    };
    let creator = Companion::creator(&mint).0;
    let mut amounts = [0u64; MAX_COMPANION_LEGS];
    for (i, key) in fee_keys.iter().enumerate() {
        let pot = &pots.legs.legs[i];
        let program = spl::program_id(pot.quote_program);
        // What a token account holds, none when it is frozen (no claim moves it).
        let movable = |info: &AccountInfo| {
            let frozen = info
                .try_borrow_data()
                .map_or(true, |d| d.len() > 108 && d[108] == 2);
            if frozen {
                return None;
            }
            Some(spl::read_token_account(info, &program).map_or(0, |t| t.amount))
        };
        let held = movable(find(
            all,
            &spl::associated_token_address(&creator, &pot.quote_mint, &program),
        )?);
        let unclaimed = movable(find(all, key)?);
        let more = match held {
            // The companion address's account frozen: nothing can be claimed into it or split.
            None => 0,
            Some(held) => unclaimed
                .unwrap_or(0)
                .saturating_add(held.saturating_sub(pot.set_aside().unwrap_or(u64::MAX))),
        };
        amounts[i] = pot
            .pending_pot
            .saturating_add(bps_of(more, u64::from(c.pot_bps)));
    }
    Ok(amounts)
}

// ---- set_game_token_terms --------------------------------------------------------------------------

/// Accounts of `set_game_token_terms`.
#[event_cpi]
#[derive(Accounts)]
pub struct SetGameTokenTerms<'info> {
    /// The launch's mint (not created yet): it signs, as for `create_game`.
    pub mint: Signer<'info>,
    #[account(seeds = [COMPANION_SEED, mint.key().as_ref()], bump = companion.bump)]
    pub companion: Box<Account<'info, Companion>>,
    #[account(mut, seeds = [GAME_SEED, mint.key().as_ref()], bump = game.bump)]
    pub game: Box<Account<'info, Game>>,
}

/// `set_game_token_terms(min_pot_bps)`: before the launch, the mint signing, a game's least pot on
/// a coin quoted in tokens whose SOL switch is off, in basis points of the coin's opening value
/// summed over the legs (1 to 10,000; the site sends 100: 1%, about $50 of a $5,000 opening).
/// `launch_token` requires it on such a coin, refuses it on a coin whose SOL switch is on
/// (`TokenGameTerms`), and marks the game's pot kept per leg in the legs' tokens; a SOL coin's
/// `launch` leaves it unread (`Game::in_tokens`). 0 clears it. A strategy game takes none:
/// strategies run on SOL coins only (`StrategyOnTokenCoin`).
pub fn process_set_game_token_terms(
    ctx: Context<SetGameTokenTerms>,
    min_pot_bps: u16,
) -> Result<()> {
    let c = &ctx.accounts.companion;
    require!(!c.launched, CompanionError::AlreadyLaunched);
    require!(u64::from(min_pot_bps) <= BPS, CompanionError::BadTokenTerms);
    let g = &mut ctx.accounts.game;
    require!(
        g.kind != GameKind::Strategy || min_pot_bps == 0,
        CompanionError::StrategyOnTokenCoin
    );
    g.set_min_pot_bps(min_pot_bps);
    emit_cpi!(GameTokenTermsSet {
        game: g.key(),
        mint: g.mint,
        min_pot_bps,
    });
    Ok(())
}
