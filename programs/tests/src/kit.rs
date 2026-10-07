//! The kit in tests (`docs/hooks-v2.md` §4).
//!
//! - [`World::direct_kit`] sets a kit token up as `create_launch` will leave it, without the
//!   launch: only the launch program's kit-caller PDA can sign `init`, so the mint is created
//!   through the token program (the kit as hook, no hook authority, the supply minted to a
//!   keypair standing in for the launch, the mint authority revoked), the reward vault through
//!   the token program, and the `KitConfig` and registry `init` would write are written into the
//!   SVM ([`KitConfig::install`], [`bordrless_kit::registry_list`]). Keypairs stand in for the
//!   pool and the launch, so a test moves tokens as those excluded owners: to the kit a buy is a
//!   transfer from the pool owner's holding and a sell a transfer to it.
//! - Holder operations (send, burn, claim, share, donate), the reward mirror (§4.13) read from
//!   account state, and [`check_kit`], the invariants of §4.11.
//! - [`money_walk`], a seeded walk of buys, sells, transfers, burns, claims, shares, donations and
//!   clock warps with the invariants after every step and every claim paying exactly what the
//!   mirror computed. Any [`Market`] drives it: [`DirectMarket`] here; the launch suites drive it
//!   with real swaps on a launch pool and [`KitToken::of`] the launch's mint.

use std::collections::BTreeMap;

use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::AccountSerialize;
use bordrless_hook::token_flags;
use bordrless_kit::client as kit;
use bordrless_kit::constants::{SCALE, SHARE_STREAM_SECS};
use bordrless_kit::error::KitError;
use bordrless_kit::events::RewardsClaimed;
use bordrless_kit::{mint_flags, mirror, modules, KitConfig, KitInitArgs};
use bordrless_token::client as token;
use bordrless_token::instructions::CreateMintArgs;
use bordrless_token::state::{AuthorityKind, Holding};
use solana_account::Account;
use solana_keypair::Keypair;
use solana_signer::Signer;
use solana_transaction::{InstructionError, TransactionError};

use crate::env::{Env, Tx};
use crate::fixture::World;

/// One SOL in lamports.
pub const SOL: u64 = 1_000_000_000;

/// The custom error code of a kit error.
pub fn kit_code(e: KitError) -> u32 {
    u32::from(e)
}

/// The custom error code a failed transaction ended with (`None` when it succeeded, `u32::MAX`
/// for a failure that is not a custom error).
pub fn code_of(tx: &Tx) -> Option<u32> {
    match &tx.result {
        Ok(_) => None,
        Err(f) => match &f.err {
            TransactionError::InstructionError(_, InstructionError::Custom(c)) => Some(*c),
            _ => Some(u32::MAX),
        },
    }
}

/// Whether `tx` failed because the kit itself refused it with `e`: the transaction ended with
/// `e`'s code, the kit program's own failure with that code is in the logs, and Anchor's log names
/// the error. A code alone proves nothing: the token program, the DEX and the launch use the same
/// numbers for other errors (6008 is the token program's `HookProgramMissing`, say).
pub fn refused_by_kit(tx: &Tx, e: KitError) -> bool {
    let code = u32::from(e);
    if code_of(tx) != Some(code) {
        return false;
    }
    let failed = format!(
        "Program {} failed: custom program error: {code:#x}",
        bordrless_kit::ID
    );
    let named = format!("Error Code: {}.", e.name());
    let logs = tx.logs();
    logs.iter().any(|l| l.contains(&failed)) && logs.iter().any(|l| l.contains(&named))
}

/// A kit token: the addresses its rules turn on.
#[derive(Clone, Debug)]
pub struct KitToken {
    /// The token.
    pub mint: Pubkey,
    /// Its modules.
    pub modules: u8,
    /// Its `KitConfig`.
    pub kit_config: Pubkey,
    /// The reward mint (bridged SOL).
    pub reward_mint: Pubkey,
    /// The reward vault, with holder rewards.
    pub reward_vault: Option<Pubkey>,
    /// The pool (excluded).
    pub pool: Pubkey,
    /// The launch (excluded).
    pub launch: Pubkey,
    /// The creator.
    pub creator: Pubkey,
}

impl KitToken {
    /// The kit token of `mint` as its `KitConfig` describes it (a kit a launch installed).
    pub fn of(env: &Env, mint: Pubkey) -> Self {
        let kit_config = kit::kit_config_address(&mint);
        let c: KitConfig = env.read(&kit_config);
        Self {
            mint,
            modules: c.modules,
            kit_config,
            reward_mint: c.reward_mint,
            reward_vault: c.rewards_on().then_some(c.reward_vault),
            pool: c.pool,
            launch: c.launch,
            creator: c.creator,
        }
    }

    /// Whether holder rewards are on.
    pub fn rewards(&self) -> bool {
        self.modules & modules::HOLDER_REWARDS != 0
    }

    /// Whether the kit is told about burns (holder rewards or the early-buyer lock).
    pub fn burns_seen(&self) -> bool {
        mint_flags(self.modules) & token_flags::BEFORE_BURN != 0
    }
}

/// A kit token to set up directly.
#[derive(Clone, Debug)]
pub struct KitSpec {
    /// The modules.
    pub modules: u8,
    /// Supply, base units.
    pub supply: u64,
    /// Decimals.
    pub decimals: u8,
    /// Max wallet (used when the module is on).
    pub max_wallet_bps: u16,
    /// Creator wallet lock, from now (used when the module is on).
    pub creator_lock_secs: i64,
    /// Early window, from now (used when the module is on).
    pub early_window_secs: i64,
    /// Early-buyer unlock, from now (used when the module is on).
    pub early_lock_secs: i64,
    /// Share of the supply the launch moves to the pool, in basis points.
    pub pool_bps: u64,
}

impl KitSpec {
    /// `modules` with the policy's numbers: a billion tokens of 6 decimals, max wallet 2%, the
    /// creator wallet locked 30 days, a 60 s early window locked for an hour, 75% in the pool.
    pub fn new(modules: u8) -> Self {
        Self {
            modules,
            supply: 1_000_000_000_000_000,
            decimals: 6,
            max_wallet_bps: 200,
            creator_lock_secs: 30 * 86_400,
            early_window_secs: 60,
            early_lock_secs: 3_600,
            pool_bps: 7_500,
        }
    }
}

/// A kit token set up directly, with the keypairs standing in for the excluded owners.
pub struct DirectKit {
    /// The token.
    pub token: KitToken,
    /// Stands in for the launch PDA: owns the reserve.
    pub launch: Keypair,
    /// Stands in for the launch pool: owns the pool's holding.
    pub pool: Keypair,
    /// The creator.
    pub creator: Keypair,
    /// What `init` would have been given.
    pub args: KitInitArgs,
}

impl Env {
    /// The `KitConfig` of `mint`.
    pub fn kit_config(&self, mint: &Pubkey) -> KitConfig {
        self.read(&kit::kit_config_address(mint))
    }

    /// Rewrites the `KitConfig` at `key` (to stage a state no instruction of this stage can reach,
    /// such as graduation, which only the launch's kit-caller PDA can sign).
    pub fn put_kit_config(&mut self, key: Pubkey, config: &KitConfig) {
        let mut data = Vec::with_capacity(KitConfig::LEN);
        config.try_serialize(&mut data).expect("serialize");
        data.resize(KitConfig::LEN, 0);
        let lamports = self.rent(KitConfig::LEN);
        self.put(
            key,
            Account {
                lamports,
                data,
                owner: bordrless_kit::ID,
                executable: false,
                rent_epoch: 0,
            },
        );
    }

    /// The reward vault's balance (0 without holder rewards).
    pub fn vault_amount(&self, k: &KitToken) -> u64 {
        k.reward_vault
            .and_then(|v| self.try_read::<Holding>(&v))
            .map_or(0, |h| h.amount)
    }

    /// The balance and hook data of `owner`'s holding of `mint` (zeros when it has none).
    pub fn holding_state(&self, mint: &Pubkey, owner: &Pubkey) -> (u64, [u8; 64]) {
        self.try_read::<Holding>(&token::holding_address(mint, owner))
            .map_or((0, [0; 64]), |h| (h.amount, h.hook_data))
    }

    /// What `owner` could claim now, by the mirror (§4.13).
    pub fn claimable(&self, k: &KitToken, owner: &Pubkey) -> u64 {
        let c = self.kit_config(&k.mint);
        let (balance, data) = self.holding_state(&k.mint, owner);
        mirror::claimable(&c, self.vault_amount(k), owner, balance, &data, self.now)
            .expect("the mirror computes")
    }
}

impl World {
    /// A kit token set up as `create_launch` leaves it, without the launch (see the module docs):
    /// the mint, the reserve, the reward vault, the config and registry `init` would write, and
    /// `pool_bps` of the supply moved from the launch to the pool.
    pub fn direct_kit(&mut self, spec: &KitSpec) -> DirectKit {
        let launch = self.env.funded(100 * SOL);
        let pool = self.env.funded(100 * SOL);
        let creator = self.env.funded(100 * SOL);
        let mint_kp = Keypair::new();
        let mint = mint_kp.pubkey();
        let now = self.env.now;
        let m = spec.modules;
        let on = |module: u8| m & module != 0;
        let (kit_config, bump) = KitConfig::address(&mint);
        let vault =
            on(modules::HOLDER_REWARDS).then(|| kit::reward_vault_address(&mint, &self.sol));
        let args = KitInitArgs {
            launch: launch.pubkey(),
            pool: pool.pubkey(),
            creator: creator.pubkey(),
            reward_mint: self.sol,
            modules: m,
            max_wallet_bps: if on(modules::MAX_WALLET) {
                spec.max_wallet_bps
            } else {
                0
            },
            creator_unlock_at: if on(modules::CREATOR_WALLET_LOCK) {
                now + spec.creator_lock_secs
            } else {
                0
            },
            early_window_end: if on(modules::EARLY_BUYER_LOCK) {
                now + spec.early_window_secs
            } else {
                0
            },
            early_unlock_at: if on(modules::EARLY_BUYER_LOCK) {
                now + spec.early_lock_secs
            } else {
                0
            },
            kit_caller_bump: kit::kit_caller_address(&mint).1,
        };

        // The mint as create_launch makes it: the kit as hook from the first instruction, no hook
        // authority, the whole supply to the launch, then no more minting ever.
        let reserve = token::holding_address(&mint, &launch.pubkey());
        let ixs = [
            token::create_mint(
                launch.pubkey(),
                mint,
                CreateMintArgs {
                    decimals: spec.decimals,
                    name: "Kit Token".to_string(),
                    symbol: "KIT".to_string(),
                    uri: String::new(),
                    max_supply: spec.supply,
                    mint_authority: Some(launch.pubkey()),
                    freeze_authority: None,
                    hook_program: Some(bordrless_kit::ID),
                    hook_flags: mint_flags(m),
                    hook_authority: None,
                    metadata_authority: None,
                },
            ),
            token::create_holding(launch.pubkey(), mint, launch.pubkey()),
            token::mint_to(
                launch.pubkey(),
                mint,
                reserve,
                Some(bordrless_kit::ID),
                vec![],
                spec.supply,
            ),
            token::set_authority(launch.pubkey(), mint, AuthorityKind::Mint, None),
        ];
        self.env.send_paid_by(&ixs, &launch, &[&mint_kp]).ok();
        // The reward vault: the config's holding of bridged SOL, through the token program.
        if vault.is_some() {
            self.env
                .send_paid_by(
                    &[token::create_holding(launch.pubkey(), self.sol, kit_config)],
                    &launch,
                    &[],
                )
                .ok();
        }
        // What init writes.
        let config = KitConfig::install(&args, mint, spec.supply, bump, vault, now)
            .expect("the spec is within the kit's bounds");
        self.env.put_kit_config(kit_config, &config);
        let registry = bordrless_kit::registry_list(kit_config, vault).encode();
        let lamports = self.env.rent(registry.len());
        self.env.put(
            kit::registry_address(&mint),
            Account {
                lamports,
                data: registry,
                owner: bordrless_kit::ID,
                executable: false,
                rent_epoch: 0,
            },
        );
        // The pool's share: launch to pool, excluded to excluded.
        self.holdings(&launch, mint, &[pool.pubkey(), creator.pubkey()]);
        let to_pool = (u128::from(spec.supply) * u128::from(spec.pool_bps) / 10_000) as u64;
        if to_pool > 0 {
            self.send_tokens(&launch, mint, &pool.pubkey(), to_pool)
                .ok();
        }
        DirectKit {
            token: KitToken {
                mint,
                modules: m,
                kit_config,
                reward_mint: self.sol,
                reward_vault: vault,
                pool: pool.pubkey(),
                launch: launch.pubkey(),
                creator: creator.pubkey(),
            },
            launch,
            pool,
            creator,
            args,
        }
    }

    /// A funded wallet with `sol` lamports of bridged SOL and a holding of `k`'s token.
    pub fn kit_holder(&mut self, k: &KitToken, sol: u64) -> Keypair {
        let wallet = self.wallet_with_sol(sol);
        self.env
            .send_paid_by(
                &[token::create_holding(
                    wallet.pubkey(),
                    k.mint,
                    wallet.pubkey(),
                )],
                &wallet,
                &[],
            )
            .ok();
        wallet
    }

    /// The `transfer` of `amount` of a kit token from `from`'s holding to `to`'s, signed by
    /// `authority`, with the extra accounts resolved from the kit's registry.
    pub fn kit_transfer_ix(
        &self,
        mint: Pubkey,
        authority: Pubkey,
        from: &Pubkey,
        to: &Pubkey,
        amount: u64,
    ) -> Instruction {
        self.hooked_transfer_ix(bordrless_kit::ID, mint, authority, from, to, amount)
    }

    /// The `burn` of `amount` from `owner`'s holding of a kit token, signed by `authority`.
    pub fn kit_burn_ix(
        &self,
        mint: Pubkey,
        authority: Pubkey,
        owner: &Pubkey,
        amount: u64,
    ) -> Instruction {
        let source = token::holding_address(&mint, owner);
        let extras = self.env.token_hook_extras(
            &bordrless_kit::ID,
            &mint,
            &source,
            &mint,
            &authority,
            owner,
            &Pubkey::default(),
        );
        token::burn(
            authority,
            source,
            mint,
            Some(bordrless_kit::ID),
            extras,
            amount,
        )
    }

    /// `owner` burns `amount` of its own tokens.
    pub fn kit_burn(&mut self, owner: &Keypair, mint: Pubkey, amount: u64) -> Tx {
        let ix = self.kit_burn_ix(mint, owner.pubkey(), &owner.pubkey(), amount);
        self.env.send_paid_by(&[ix], owner, &[])
    }

    /// `owner` claims its rewards into its own bridged-SOL holding.
    pub fn kit_claim(&mut self, owner: &Keypair, k: &KitToken) -> Tx {
        let ix = kit::claim(owner.pubkey(), k.mint, k.reward_mint);
        self.env.send_paid_by(&[ix], owner, &[])
    }

    /// `sharer` shares `amount` lamports of its bridged SOL with `k`'s holders.
    pub fn kit_share(&mut self, sharer: &Keypair, k: &KitToken, amount: u64) -> Tx {
        let ix = kit::share(
            sharer.pubkey(),
            k.mint,
            token::holding_address(&k.reward_mint, &sharer.pubkey()),
            k.reward_mint,
            amount,
        );
        self.env.send_paid_by(&[ix], sharer, &[])
    }

    /// A bridged-SOL transfer of `lamports` from `from`'s holding to the holding `to`.
    pub fn sol_transfer_ix(&self, from: &Pubkey, to: Pubkey, lamports: u64) -> Instruction {
        token::transfer(
            *from,
            token::holding_address(&self.sol, from),
            to,
            self.sol,
            None,
            vec![],
            lamports,
        )
    }

    /// `from` sends `lamports` of bridged SOL straight to `k`'s reward vault (what a holder fee
    /// or a donation does).
    pub fn donate(&mut self, from: &Keypair, k: &KitToken, lamports: u64) -> Tx {
        let vault = k.reward_vault.expect("holder rewards are on");
        let ix = self.sol_transfer_ix(&from.pubkey(), vault, lamports);
        self.env.send_paid_by(&[ix], from, &[])
    }
}

/// The invariants after any instruction (§4.11), for a token whose holders are all among
/// `owners` (excluded owners among them are skipped): `eligible` is the sum of the holders'
/// balances (at least that sum when the kit is not told about burns), and with holder rewards
/// the vault covers what each holder could claim, what is held and what still streams (the
/// running stream and the shares waiting for it), as a sync now would leave them. Answers the
/// dust: what the vault holds beyond that.
pub fn check_kit(env: &Env, k: &KitToken, owners: &[Pubkey]) -> u64 {
    let c = env.kit_config(&k.mint);
    let mut owners = owners.to_vec();
    owners.sort();
    owners.dedup();
    let vault = env.vault_amount(k);
    let mut sum = 0u64;
    let mut owed = 0u64;
    for owner in owners.iter().filter(|o| !c.is_excluded(o)) {
        let (balance, data) = env.holding_state(&k.mint, owner);
        sum += balance;
        owed += mirror::claimable(&c, vault, owner, balance, &data, env.now).expect("mirror");
    }
    if k.burns_seen() {
        assert_eq!(c.eligible, sum, "eligible is the holders' balances");
    } else {
        assert!(c.eligible >= sum, "eligible counts every holder");
    }
    if !k.rewards() {
        return 0;
    }
    let after = mirror::synced(&c, vault, env.now).expect("mirror");
    let covered = owed + after.held + after.streaming();
    assert!(
        vault >= covered,
        "the vault ({vault}) must cover the claimable {owed}, held {}, streaming {} and waiting {}",
        after.held,
        after.stream_remaining,
        after.stream_next
    );
    vault - covered
}

/// A small deterministic generator (SplitMix64) for the seeded walks.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    /// Seeded.
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// The next 64 bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n` (0 for `n == 0`).
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }

    /// Uniform in `lo..=hi`.
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.below(hi.saturating_sub(lo).saturating_add(1))
    }
}

/// How trades reach the pool in a [`money_walk`].
pub trait Market {
    /// `buyer` buys `k`'s token from the pool; `size` is in the market's own unit (tokens for
    /// [`DirectMarket`], lamports in for a launch pool). Any holder fee reaches the reward vault.
    /// `None` when the market does not trade that (a buy the fees would eat whole, say).
    fn buy(&mut self, w: &mut World, k: &KitToken, buyer: &Keypair, size: u64) -> Option<Tx>;
    /// `seller` sells `amount` of `k`'s token into the pool; `None` when the market does not
    /// trade that.
    fn sell(&mut self, w: &mut World, k: &KitToken, seller: &Keypair, amount: u64) -> Option<Tx>;
}

/// Trades as plain transfers from and to the pool owner's holding, the holder fee paid into the
/// vault by a market maker in the same transaction, as the launch's pool hook would (before the
/// tokens on a buy, after them on a sell, and only while holders hold at least `min_eligible`).
pub struct DirectMarket {
    /// The pool's owner.
    pub pool: Keypair,
    /// Pays the holder fees in bridged SOL.
    pub payer: Keypair,
    /// The holder fee: one lamport per this many base units traded.
    pub units_per_lamport: u64,
}

impl DirectMarket {
    fn fee(&self, w: &World, k: &KitToken, amount: u64) -> u64 {
        if !k.rewards() {
            return 0;
        }
        let c = w.env.kit_config(&k.mint);
        if c.eligible > 0 && c.eligible >= c.min_eligible {
            amount / self.units_per_lamport
        } else {
            0
        }
    }
}

impl Market for DirectMarket {
    fn buy(&mut self, w: &mut World, k: &KitToken, buyer: &Keypair, size: u64) -> Option<Tx> {
        // The pool sells what it has.
        let size = size.min(w.env.holding_state(&k.mint, &k.pool).0).max(1);
        let mut ixs = Vec::new();
        let fee = self.fee(w, k, size);
        if fee > 0 {
            ixs.push(w.sol_transfer_ix(&self.payer.pubkey(), k.reward_vault.unwrap(), fee));
        }
        ixs.push(w.kit_transfer_ix(k.mint, self.pool.pubkey(), &k.pool, &buyer.pubkey(), size));
        let (pool, payer) = (self.pool.insecure_clone(), self.payer.insecure_clone());
        Some(if fee > 0 {
            w.env.send_paid_by(&ixs, buyer, &[&pool, &payer])
        } else {
            w.env.send_paid_by(&ixs, buyer, &[&pool])
        })
    }

    fn sell(&mut self, w: &mut World, k: &KitToken, seller: &Keypair, amount: u64) -> Option<Tx> {
        let mut ixs =
            vec![w.kit_transfer_ix(k.mint, seller.pubkey(), &seller.pubkey(), &k.pool, amount)];
        let fee = self.fee(w, k, amount);
        Some(if fee > 0 {
            ixs.push(w.sol_transfer_ix(&self.payer.pubkey(), k.reward_vault.unwrap(), fee));
            let payer = self.payer.insecure_clone();
            w.env.send_paid_by(&ixs, seller, &[&payer])
        } else {
            w.env.send_paid_by(&ixs, seller, &[])
        })
    }
}

/// A seeded money walk.
#[derive(Clone, Debug)]
pub struct Walk {
    /// The seed.
    pub seed: u64,
    /// Steps.
    pub steps: usize,
    /// Holders (fresh wallets with bridged SOL and a holding of the token).
    pub holders: usize,
    /// Bridged SOL each holder starts with.
    pub holder_sol: u64,
    /// Largest buy in the market's unit while holders are thick.
    pub buy_max: u64,
    /// Largest buy while thin: thin phases also sell whole balances, so `eligible` falls below
    /// `min_eligible` for stretches.
    pub thin_buy_max: u64,
    /// Steps per phase; thick and thin phases alternate.
    pub phase: usize,
    /// Kit errors a buy, sell, transfer or burn may be refused with (the module set's rules: max
    /// wallet, the locks). A refusal counts only when the kit itself refused
    /// ([`refused_by_kit`]); any other failure fails the walk.
    pub allowed: Vec<KitError>,
}

/// What a [`money_walk`] did.
#[derive(Clone, Debug, Default)]
pub struct WalkReport {
    /// Operations attempted, by kind.
    pub ops: BTreeMap<&'static str, usize>,
    /// Refusals by the kit, by error name.
    pub refused: BTreeMap<String, usize>,
    /// Lamports that reached the vault (fees, donations, shares).
    pub inflows: u64,
    /// Lamports claimed, and the claims that paid.
    pub claimed: u64,
    /// Claims that paid.
    pub claims: usize,
    /// Claims refused with `NothingToClaim`, as the mirror predicted.
    pub empty_claims: usize,
    /// The vault's dust at the end, and the most it held during the walk.
    pub dust: u64,
    /// See `dust`.
    pub max_dust: u64,
    /// Steps that ended with `eligible < min_eligible`.
    pub below_min_steps: usize,
    /// Highest compute of each successful operation kind.
    pub max_cu: BTreeMap<&'static str, u64>,
}

/// Walks `walk.steps` seeded operations on `k` through `market`, checking [`check_kit`] after
/// every one and that every claim pays exactly `min(claimable, vault)` by the mirror; then
/// releases every stream, makes holders eligible, claims everything and checks that nothing is
/// left to claim, the dust is rounding and every lamport that arrived is either claimed or in the
/// vault.
pub fn money_walk<M: Market>(
    w: &mut World,
    k: &KitToken,
    market: &mut M,
    walk: &Walk,
) -> WalkReport {
    let mut rng = Rng::new(walk.seed);
    let holders: Vec<Keypair> = (0..walk.holders)
        .map(|_| w.kit_holder(k, walk.holder_sol))
        .collect();
    let donor = w.wallet_with_sol(1_000 * SOL);
    let mut owners: Vec<Pubkey> = holders.iter().map(|h| h.pubkey()).collect();
    owners.push(k.creator);
    let mut report = WalkReport::default();
    let start = w.env.kit_config(&k.mint);
    let start_vault = w.env.vault_amount(k);

    for step in 0..walk.steps {
        let thin = (step / walk.phase.max(1)) % 2 == 1;
        let h = &holders[rng.below(holders.len() as u64) as usize];
        let vault_before = w.env.vault_amount(k);
        let roll = rng.below(100);
        let (kind, tx) = match roll {
            0..=19 => {
                let max = if thin {
                    walk.thin_buy_max
                } else {
                    walk.buy_max
                };
                ("buy", market.buy(w, k, h, rng.range(1, max)))
            }
            20..=34 => {
                let have = w.env.holding_state(&k.mint, &h.pubkey()).0;
                let amount = if thin { have } else { rng.range(1, have) };
                (
                    "sell",
                    if have > 0 {
                        market.sell(w, k, h, amount)
                    } else {
                        None
                    },
                )
            }
            35..=46 => {
                let have = w.env.holding_state(&k.mint, &h.pubkey()).0;
                let to = holders[rng.below(holders.len() as u64) as usize].pubkey();
                let tx = (have > 0 && to != h.pubkey()).then(|| {
                    let amount = rng.range(1, have);
                    w.send_tokens(h, k.mint, &to, amount)
                });
                ("transfer", tx)
            }
            47..=51 => {
                let have = w.env.holding_state(&k.mint, &h.pubkey()).0;
                let tx = (have > 0 && k.burns_seen()).then(|| {
                    let amount = rng.range(1, have / 4 + 1);
                    w.kit_burn(h, k.mint, amount)
                });
                ("burn", tx)
            }
            52..=66 if k.rewards() => {
                claim_as_mirrored(w, k, h, &mut report);
                ("claim", None)
            }
            67..=71 if k.rewards() => {
                let c = w.env.kit_config(&k.mint);
                let amount = rng.range(1_000_000, 50_000_000);
                let tx = w.kit_share(h, k, amount);
                if c.eligible > 0 && c.eligible >= c.min_eligible {
                    tx.ok();
                } else {
                    tx.expect_code(kit_code(KitError::NoEligibleHolders));
                }
                ("share", Some(tx))
            }
            72..=84 if k.rewards() => {
                let amount = if rng.below(3) == 0 {
                    rng.range(1, 3)
                } else {
                    rng.range(1, 10_000_000)
                };
                let tx = w.donate(&donor, k, amount);
                tx.ok();
                ("donate", Some(tx))
            }
            _ => {
                w.env.warp(rng.range(1, 900) as i64);
                ("warp", None)
            }
        };
        *report.ops.entry(kind).or_default() += 1;
        if let Some(tx) = &tx {
            match code_of(tx) {
                None => {
                    let cu = report.max_cu.entry(kind).or_default();
                    *cu = (*cu).max(tx.cu());
                }
                Some(code) if kind != "share" => {
                    let e = walk
                        .allowed
                        .iter()
                        .find(|e| refused_by_kit(tx, **e))
                        .unwrap_or_else(|| {
                            panic!(
                                "step {step}: {kind} failed with {code}, not a refusal of the \
                                 kit's rules\n{}",
                                tx.logs().join("\n")
                            )
                        });
                    *report.refused.entry(e.name()).or_default() += 1;
                }
                Some(_) => {}
            }
        }
        if kind != "claim" {
            let vault_after = w.env.vault_amount(k);
            assert!(
                vault_after >= vault_before,
                "only claims take from the vault"
            );
            report.inflows += vault_after - vault_before;
        }
        let c = w.env.kit_config(&k.mint);
        if c.eligible < c.min_eligible {
            report.below_min_steps += 1;
        }
        report.max_dust = report.max_dust.max(check_kit(&w.env, k, &owners));
    }

    if k.rewards() {
        // Holders eligible (the stream runs only while they are), every stream released (the
        // running stream within the hour, then the hour of the shares waiting for it), everything
        // claimed.
        for i in 0..holders.len() * 4 {
            let c = w.env.kit_config(&k.mint);
            if c.eligible > 0 && c.eligible >= c.min_eligible {
                break;
            }
            let buyer = &holders[i % holders.len()];
            let before = w.env.vault_amount(k);
            if let Some(tx) = market.buy(w, k, buyer, walk.buy_max) {
                if code_of(&tx).is_some() {
                    assert!(
                        walk.allowed.iter().any(|e| refused_by_kit(&tx, *e)),
                        "{}",
                        tx.logs().join("\n")
                    );
                }
            }
            report.inflows += w.env.vault_amount(k) - before;
        }
        w.env.warp(2 * SHARE_STREAM_SECS + 1);
        for h in &holders {
            claim_as_mirrored(w, k, h, &mut report);
        }
        let c = w.env.kit_config(&k.mint);
        if c.eligible > 0 && c.eligible >= c.min_eligible {
            let synced = mirror::synced(&c, w.env.vault_amount(k), w.env.now).expect("mirror");
            assert_eq!(synced.streaming(), 0, "every share is released");
            for h in &holders {
                assert_eq!(w.env.claimable(k, &h.pubkey()), 0, "everything was claimed");
            }
        }
    }
    report.dust = check_kit(&w.env, k, &owners);
    let end = w.env.kit_config(&k.mint);
    assert_eq!(
        end.total_claimed - start.total_claimed,
        report.claimed,
        "the config counts every claim"
    );
    assert_eq!(
        w.env.vault_amount(k) + report.claimed,
        start_vault + report.inflows,
        "every lamport that arrived is claimed or in the vault"
    );
    if k.rewards() {
        // What stays is rounding: under a lamport per settle (two per step at most, a few more
        // at the end), plus the scaled remainder (below the eligible supply over SCALE).
        let bound = 2 * walk.steps as u64
            + 8 * holders.len() as u64
            + (u128::from(end.supply_at_init) / SCALE) as u64
            + 2;
        assert!(report.dust <= bound, "dust {} above {bound}", report.dust);
    }
    report
}

/// `owner` claims, and the claim pays exactly `min(claimable, vault)` as the mirror computes it
/// first, or is refused with `NothingToClaim` when that is 0.
pub fn claim_as_mirrored(w: &mut World, k: &KitToken, owner: &Keypair, report: &mut WalkReport) {
    let vault = w.env.vault_amount(k);
    let expected = w.env.claimable(k, &owner.pubkey()).min(vault);
    let sol_before = w.env.holding(&k.reward_mint, &owner.pubkey());
    let tx = w.kit_claim(owner, k);
    if expected == 0 {
        tx.expect_code(kit_code(KitError::NothingToClaim));
        report.empty_claims += 1;
        return;
    }
    tx.ok();
    let ev: RewardsClaimed = tx.event();
    assert_eq!(ev.amount, expected, "a claim pays what the mirror computed");
    assert_eq!(
        w.env.holding(&k.reward_mint, &owner.pubkey()),
        sol_before + expected
    );
    assert_eq!(w.env.vault_amount(k), vault - expected);
    report.claimed += expected;
    report.claims += 1;
    let cu = report.max_cu.entry("claim").or_default();
    *cu = (*cu).max(tx.cu());
}
