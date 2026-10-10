//! `hook_vault` in tests (`docs/phase3a.md` §5): a launchpad coin whose custom token hook is
//! `tax_hook`, its collector slot 0's owner, so every transfer's 1% lands in slot 0 once the vault is
//! open; the vault's instructions built from the vault's state as a client builds them, with the
//! coin's hook extras resolved from its registry for each operation.

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::InstructionData;
use bordrless_hook::hook_accounts_address;
use bordrless_launch::client::{self as launch, CustomHookAccounts};
use bordrless_launch::instructions::CreateConfigArgs;
use bordrless_launch::state::LaunchRules;
use bordrless_swap::client as swap;
use bordrless_token::client as token;
use hook_vault::client as vault_client;
use hook_vault::constants::policy;
use hook_vault::instructions::{CreateVaultArgs, SlotArgs};
use hook_vault::state::Vault;
use solana_keypair::Keypair;
use solana_signer::Signer;

use crate::env::{Tx, SYSTEM_PROGRAM_ID};
use crate::fixture::World;
use crate::launch::{SOL, VQ};

/// `tax_hook`'s `prepare` for a mint not created yet: `fee_bps` of every transfer to `collector`'s
/// holding (once it exists), and a wallet cap of `max_wallet_bps` (0: none).
pub fn tax_prepare(
    payer: Pubkey,
    mint: Pubkey,
    collector: Pubkey,
    fee_bps: u16,
    max_wallet_bps: u16,
) -> Instruction {
    Instruction {
        program_id: tax_hook::ID,
        accounts: vec![
            AccountMeta::new(payer, true),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new_readonly(collector, false),
            AccountMeta::new(tax_config(&mint), false),
            AccountMeta::new(hook_accounts_address(&tax_hook::ID, &mint).0, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        ],
        data: tax_hook::instruction::Prepare {
            fee_bps,
            max_wallet_bps,
        }
        .data(),
    }
}

/// `tax_hook`'s config of `mint`.
pub fn tax_config(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[tax_hook::TAX_SEED, mint.as_ref()], &tax_hook::ID).0
}

/// A burn slot.
pub fn burn() -> SlotArgs {
    SlotArgs {
        policy: policy::BURN,
        target: Pubkey::default(),
        max_cut_bps: 0,
    }
}

/// A slot selling for SOL to `to`.
pub fn sell_for_sol(to: Pubkey) -> SlotArgs {
    SlotArgs {
        policy: policy::SELL_FOR_SOL,
        target: to,
        max_cut_bps: 0,
    }
}

/// A slot selling, then buying and burning the token of `pool`.
pub fn sell_buy_burn(pool: Pubkey) -> SlotArgs {
    sell_buy_burn_cut(pool, 0)
}

/// A slot selling, then buying and burning the token of `pool`, whose own hook may cut up to
/// `max_cut_bps` from the buy (declared).
pub fn sell_buy_burn_cut(pool: Pubkey, max_cut_bps: u16) -> SlotArgs {
    SlotArgs {
        policy: policy::SELL_BUY_BURN,
        target: pool,
        max_cut_bps,
    }
}

/// The vault's arguments with `slots`: tax_hook, a 0.5% bounty, the most a sell may take (1%), a
/// minute apart, no declared hook cut (slot 0 is tax_hook's collector, which it never taxes).
pub fn vault_args(slots: Vec<SlotArgs>) -> CreateVaultArgs {
    CreateVaultArgs {
        hook: tax_hook::ID,
        slots,
        bounty_bps: 50,
        max_sell_bps: 100,
        interval: 60,
        max_hook_cut_bps: 0,
    }
}

/// How a vault coin is launched.
#[derive(Clone, Copy, Debug)]
pub struct CoinSpec {
    /// The creator fee of its config.
    pub creator_fee_bps: u16,
    /// The pool hook rules of its config (burn only: a custom hook takes no kit rule).
    pub rules: LaunchRules,
    /// tax_hook's fee, to slot 0.
    pub tax_bps: u16,
    /// tax_hook's wallet cap (0: none).
    pub max_wallet_bps: u16,
    /// Open the vault after the launch.
    pub open: bool,
}

impl Default for CoinSpec {
    fn default() -> Self {
        Self {
            creator_fee_bps: 100,
            rules: LaunchRules::NONE,
            tax_bps: 100,
            max_wallet_bps: 0,
            open: true,
        }
    }
}

/// A coin launched with a vault.
pub struct VaultCoin {
    pub mint: Pubkey,
    pub creator: Keypair,
    pub config: Pubkey,
}

impl World {
    /// A `LaunchConfig` by `creator` naming `tax_hook`, with `creator_fee_bps` and `rules` (pool
    /// hook rules only: burn and the creator fee).
    pub fn tax_hook_config(
        &mut self,
        creator: &Keypair,
        creator_fee_bps: u16,
        rules: LaunchRules,
    ) -> Pubkey {
        let (config, tx) = self.create_config(
            creator,
            CreateConfigArgs {
                rules,
                creator_fee_bps,
                custom_hook: Some(tax_hook::ID),
                custom_hook_flags: tax_hook::FLAGS,
                label: "Vaulted".to_string(),
            },
        );
        tx.ok();
        config
    }

    /// The instructions that make a vault coin before its launch: tax_hook prepared for `mint`
    /// (`tax_bps` of every transfer to slot 0's owner, `max_wallet_bps`), then the vault.
    pub fn vault_setup_ixs(
        &self,
        payer: &Pubkey,
        mint: &Pubkey,
        args: CreateVaultArgs,
        buy_mints: &[Pubkey],
        spec: &CoinSpec,
    ) -> Vec<Instruction> {
        vec![
            tax_prepare(
                *payer,
                *mint,
                vault_client::slot_owner(mint, 0),
                spec.tax_bps,
                spec.max_wallet_bps,
            ),
            vault_client::create_vault(*payer, *mint, args, buy_mints),
        ]
    }

    /// A coin with a vault of `args`, launched from a fresh tax_hook config (creator fee 1%, no
    /// rules, a 1% tax), and opened when `open`. Every step is checked to land.
    pub fn vault_coin(
        &mut self,
        args: CreateVaultArgs,
        buy_mints: &[Pubkey],
        open: bool,
    ) -> VaultCoin {
        self.vault_coin_spec(
            args,
            buy_mints,
            &CoinSpec {
                open,
                ..CoinSpec::default()
            },
        )
    }

    /// [`World::vault_coin`] as `spec` says.
    pub fn vault_coin_spec(
        &mut self,
        args: CreateVaultArgs,
        buy_mints: &[Pubkey],
        spec: &CoinSpec,
    ) -> VaultCoin {
        let creator = self.wallet_with_sol(50 * SOL);
        let config = self.tax_hook_config(&creator, spec.creator_fee_bps, spec.rules);
        let mint = Keypair::new();
        let setup = self.vault_setup_ixs(&creator.pubkey(), &mint.pubkey(), args, buy_mints, spec);
        self.env.send_paid_by(&setup, &creator, &[&mint]).ok();
        let ix = self.create_launch_from_config_ix(
            &creator.pubkey(),
            &mint.pubkey(),
            "VLT",
            VQ,
            &config,
        );
        self.env.send_paid_by(&[ix], &creator, &[&mint]).ok();
        if spec.open {
            self.open_vault(&creator, &mint.pubkey()).ok();
        }
        VaultCoin {
            mint: mint.pubkey(),
            creator,
            config,
        }
    }

    /// Rewrites tax_hook's settings for `mint` (as an upgrade of the hook, or a hook with other
    /// settings, would leave them): its fee and its wallet cap.
    pub fn set_tax(&mut self, mint: &Pubkey, fee_bps: u16, max_wallet_bps: u16) {
        let key = tax_config(mint);
        let mut c: tax_hook::TaxConfig = self.env.read(&key);
        c.fee_bps = fee_bps;
        c.max_wallet_bps = max_wallet_bps;
        let mut account = self.env.account(&key).expect("tax config");
        let mut data = Vec::new();
        anchor_lang::AccountSerialize::try_serialize(&c, &mut data).expect("serialize");
        account.data[..data.len()].copy_from_slice(&data);
        self.env.put(key, account);
    }

    /// The vault of `mint`.
    pub fn vault(&self, mint: &Pubkey) -> Vault {
        self.env.read(&vault_client::vault_address(mint))
    }

    /// `open_vault` of `mint`'s vault, sent by `sender`.
    pub fn open_vault_ix(&self, sender: &Pubkey, mint: &Pubkey) -> Instruction {
        vault_client::open_vault(*sender, &self.vault(mint), &self.launch_keys(mint))
    }

    pub fn open_vault(&mut self, sender: &Keypair, mint: &Pubkey) -> Tx {
        let ix = self.open_vault_ix(&sender.pubkey(), mint);
        self.env.send_paid_by(&[ix], sender, &[])
    }

    /// The coin's hook with its extras resolved for slot `i`'s sell (its holding to the pool's coin
    /// vault, the slot's owner signing).
    pub fn slot_sell_hook(&self, mint: &Pubkey, i: u8) -> CustomHookAccounts {
        let owner = vault_client::slot_owner(mint, i);
        let pool = self.launch_pool_key(mint);
        let hook = self.vault(mint).hook;
        CustomHookAccounts {
            program: hook,
            extras: self.env.token_hook_extras(
                &hook,
                mint,
                &token::holding_address(mint, &owner),
                &swap::vault_address(&pool, mint),
                &owner,
                &owner,
                &pool,
            ),
        }
    }

    /// `hook` (a custom hook of `mint`) with its extras resolved for `owner`'s burn of its holding.
    pub fn burn_hook(&self, hook: &Pubkey, mint: &Pubkey, owner: &Pubkey) -> CustomHookAccounts {
        CustomHookAccounts {
            program: *hook,
            extras: self.env.token_hook_extras(
                hook,
                mint,
                &token::holding_address(mint, owner),
                mint,
                owner,
                owner,
                &Pubkey::default(),
            ),
        }
    }

    /// The coin's hook resolved for slot `i`'s burn.
    pub fn slot_burn_hook(&self, mint: &Pubkey, i: u8) -> CustomHookAccounts {
        let hook = self.vault(mint).hook;
        self.burn_hook(&hook, mint, &vault_client::slot_owner(mint, i))
    }

    /// `execute(i)` of `mint`'s vault, sent by `cranker`.
    pub fn execute_ix(&self, cranker: &Pubkey, mint: &Pubkey, i: u8) -> Instruction {
        let v = self.vault(mint);
        let hook = if v.slots[usize::from(i)].policy == policy::BURN {
            self.slot_burn_hook(mint, i)
        } else {
            self.slot_sell_hook(mint, i)
        };
        vault_client::execute(*cranker, &v, i, &self.launch_keys(mint), &hook)
    }

    pub fn execute(&mut self, cranker: &Keypair, mint: &Pubkey, i: u8) -> Tx {
        let ix = self.execute_ix(&cranker.pubkey(), mint, i);
        self.env.send_paid_by(&[ix], cranker, &[])
    }

    /// `execute_buy(i)` of `mint`'s vault, sent by `cranker`: the slot's token, its hook resolved.
    pub fn execute_buy_ix(&self, cranker: &Pubkey, mint: &Pubkey, i: u8) -> Instruction {
        let v = self.vault(mint);
        let pool: bordrless_swap::state::Pool = self.env.read(&v.slots[usize::from(i)].target);
        let x = pool.base_mint;
        let xl = self.launch(&x);
        let keys = self.launch_keys(&x);
        let owner = vault_client::slot_owner(mint, i);
        match xl.custom_hook {
            None => vault_client::execute_buy(
                *cranker,
                &v,
                i,
                &keys,
                vault_client::BuyHook::Kit {
                    rewards: xl.rules.rewards_on(),
                },
            ),
            Some(hook) => {
                let transfer = CustomHookAccounts {
                    program: hook,
                    extras: self.env.token_hook_extras(
                        &hook,
                        &x,
                        &swap::vault_address(&xl.pool, &x),
                        &token::holding_address(&x, &owner),
                        &xl.pool,
                        &xl.pool,
                        &owner,
                    ),
                };
                let burn = self.burn_hook(&hook, &x, &owner);
                vault_client::execute_buy(
                    *cranker,
                    &v,
                    i,
                    &keys,
                    vault_client::BuyHook::Custom {
                        transfer: &transfer,
                        burn: &burn,
                    },
                )
            }
        }
    }

    pub fn execute_buy(&mut self, cranker: &Keypair, mint: &Pubkey, i: u8) -> Tx {
        let ix = self.execute_buy_ix(&cranker.pubkey(), mint, i);
        self.env.send_paid_by(&[ix], cranker, &[])
    }

    /// `retire(i, burn_coin)` of `mint`'s vault, sent by `cranker`.
    pub fn retire_ix(
        &self,
        cranker: &Pubkey,
        mint: &Pubkey,
        i: u8,
        burn_coin: bool,
    ) -> Instruction {
        let v = self.vault(mint);
        let burn = self.slot_burn_hook(mint, i);
        vault_client::retire(*cranker, &v, i, burn_coin, Some(&burn))
    }

    pub fn retire(&mut self, cranker: &Keypair, mint: &Pubkey, i: u8, burn_coin: bool) -> Tx {
        let ix = self.retire_ix(&cranker.pubkey(), mint, i, burn_coin);
        self.env.send_paid_by(&[ix], cranker, &[])
    }

    /// Slot `i`'s coin balance.
    pub fn slot_coin(&self, mint: &Pubkey, i: u8) -> u64 {
        self.env.holding(mint, &vault_client::slot_owner(mint, i))
    }

    /// Slot `i`'s bridged SOL.
    pub fn slot_sol(&self, mint: &Pubkey, i: u8) -> u64 {
        self.env
            .holding(&self.sol, &vault_client::slot_owner(mint, i))
    }

    /// Trades on `mint`'s pool by fresh wallets (a buy of `lamports`, then a sell of half of it),
    /// `n` times: trade that pays the coin's hook its cut both ways. Each wallet keeps half, so the
    /// pool always holds SOL for more than the slots' coin (a buy's cut is valued and Bordrless's
    /// share of it taken from the quote reserve, so the last coins out could not all be sold back).
    pub fn churn(&mut self, mint: &Pubkey, n: usize, lamports: u64) {
        for _ in 0..n {
            let t = self.wallet_with_sol(lamports + SOL);
            self.buy(&t, mint, lamports).ok();
            let got = self.env.holding(mint, &t.pubkey());
            self.sell(&t, mint, got / 2).ok();
        }
    }

    /// The spot price of `mint`'s launch pool, as the vault computes it.
    /// The coin's opening price: its launch's curve start (`virtual_quote` over `curve_tokens +
    /// virtual_base`), the most a vault's sell reference opens at.
    pub fn opening_price(&self, mint: &Pubkey) -> u128 {
        let l = self.launch(mint);
        hook_vault::state::spot_price(0, l.virtual_quote, l.curve_tokens, l.virtual_base)
            .expect("a price")
    }

    /// Sets slot `i`'s sell reference to the coin's price now, as sales at that price would leave
    /// it (a vault opens at most at the opening price, and each sale raises the reference by at most
    /// 5%): the state the guard tests start from.
    pub fn set_sell_reference_to_price(&mut self, mint: &Pubkey, i: usize) {
        let key = vault_client::vault_address(mint);
        let mut v = self.vault(mint);
        v.slots[i].reference_price = self.coin_price(mint);
        let mut account = self.env.account(&key).expect("the vault");
        let mut data = Vec::new();
        anchor_lang::AccountSerialize::try_serialize(&v, &mut data).expect("serialize");
        account.data[..data.len()].copy_from_slice(&data);
        self.env.put(key, account);
    }

    pub fn coin_price(&self, mint: &Pubkey) -> u128 {
        let p = self.launch_pool(mint);
        hook_vault::state::spot_price(
            p.quote_reserve,
            p.virtual_quote,
            p.base_reserve,
            p.virtual_base,
        )
        .expect("a price")
    }
}

/// The holder of slot `i` of `mint`, and the pool's coin vault: what a delta to the slot is.
pub fn slot_owner(mint: &Pubkey, i: u8) -> Pubkey {
    vault_client::slot_owner(mint, i)
}

/// The launch key of a coin (re-exported for the suite).
pub fn launch_of(mint: &Pubkey) -> Pubkey {
    launch::launch_address(mint)
}
