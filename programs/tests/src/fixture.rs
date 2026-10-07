//! Ready-made setups: configured programs, bridged SOL, holdings, a launch.

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use bordrless_bridge::client as bridge;
use bordrless_core::policy;
use bordrless_launch::client as launch;
use bordrless_launch::instructions::CreateConfigArgs;
use bordrless_launch::state::{LaunchConfig, LaunchRules, RuleBounds};
use bordrless_swap::client as swap;
use bordrless_token::client as token;
use bordrless_token::state::Mint;
use solana_keypair::Keypair;
use solana_signer::Signer;

use crate::env::{Env, Tx};

/// The token-rule bounds of the policy (§5.2).
pub fn policy_rule_bounds() -> RuleBounds {
    RuleBounds {
        max_holder_fee_bps: policy::MAX_HOLDER_FEE_BPS,
        max_burn_bps: policy::MAX_BURN_BPS,
        max_rules_fee_bps: policy::MAX_RULES_FEE_BPS,
        min_max_wallet_bps: policy::MIN_MAX_WALLET_BPS,
        max_max_wallet_bps: policy::MAX_MAX_WALLET_BPS,
        max_creator_lock_secs: policy::MAX_CREATOR_LOCK_SECS,
        max_early_window_secs: policy::MAX_EARLY_WINDOW_SECS,
        max_early_lock_secs: policy::MAX_EARLY_LOCK_SECS,
    }
}

/// The launch config's arguments of the policy (as `World::new` sets them), with `admin` and
/// `treasury`.
pub fn policy_launch_config(
    admin: Pubkey,
    treasury: Pubkey,
    quote_mint: Pubkey,
) -> bordrless_launch::instructions::ConfigArgs {
    bordrless_launch::instructions::ConfigArgs {
        admin,
        treasury,
        quote_mint,
        launch_fee_lamports: policy::LAUNCH_FEE_LAMPORTS,
        lp_fee_bps: policy::LP_FEE_BPS,
        max_creator_fee_bps: policy::MAX_CREATOR_FEE_BPS,
        sniper_window_secs: policy::SNIPER_WINDOW_SECS,
        sniper_start_bps: policy::SNIPER_START_BPS,
        curve_bps: policy::CURVE_BPS as u16,
        supply: policy::TOKEN_SUPPLY,
        decimals: policy::TOKEN_DECIMALS,
        min_virtual_quote: policy::MIN_VIRTUAL_QUOTE,
        max_virtual_quote: policy::MAX_VIRTUAL_QUOTE,
        paused: false,
        rule_bounds: policy_rule_bounds(),
    }
}

/// The configured world: every config created, SOL registered on the bridge.
pub struct World {
    /// The environment.
    pub env: Env,
    /// Bridged SOL's mint.
    pub sol: Pubkey,
}

impl World {
    /// Configures everything with the policy defaults.
    pub fn new() -> Self {
        let mut env = Env::new();
        let deployer = env.deployer.insecure_clone();
        let admin = deployer.pubkey();
        let treasury = env.treasury.pubkey();
        let sol = bridge::wrapped_mint_address(&bordrless_bridge::constants::NATIVE_MINT);
        let ixs = [
            swap::init_config(
                admin,
                bordrless_swap::instructions::ConfigArgs {
                    admin,
                    protocol_fee_bps: policy::PROTOCOL_FEE_BPS,
                    launch_protocol_share_bps: policy::LAUNCH_PROTOCOL_SHARE_BPS,
                    fee_collector: admin,
                    treasury,
                    pool_creation_fee_lamports: 0,
                    paused: false,
                },
            ),
            bridge::init_config(
                admin,
                bordrless_bridge::instructions::ConfigArgs {
                    admin,
                    paused: false,
                },
            ),
            launch::init_config(admin, policy_launch_config(admin, treasury, sol)),
            bridge::register_sol(admin),
        ];
        env.send(&ixs, &[&deployer]).ok();
        Self { env, sol }
    }

    /// A funded wallet holding `lamports` of bridged SOL (and as much SOL again for fees).
    pub fn wallet_with_sol(&mut self, lamports: u64) -> Keypair {
        let kp = self.env.funded(lamports * 2 + 1_000_000_000);
        self.wrap_sol(&kp, lamports).ok();
        kp
    }

    /// Wraps `lamports` of a wallet's SOL.
    pub fn wrap_sol(&mut self, wallet: &Keypair, lamports: u64) -> Tx {
        let ixs = [
            token::create_holding(wallet.pubkey(), self.sol, wallet.pubkey()),
            bridge::wrap_sol(wallet.pubkey(), lamports),
        ];
        self.env.send_paid_by(&ixs, wallet, &[])
    }

    /// Creates a hook-less BTS mint with `supply` minted to `owner`.
    pub fn mint_to_owner(
        &mut self,
        owner: &Keypair,
        decimals: u8,
        supply: u64,
        symbol: &str,
    ) -> Pubkey {
        let mint = Keypair::new();
        let args = bordrless_token::instructions::CreateMintArgs {
            decimals,
            name: format!("Test {symbol}"),
            symbol: symbol.to_string(),
            uri: String::new(),
            max_supply: 0,
            mint_authority: Some(owner.pubkey()),
            freeze_authority: Some(owner.pubkey()),
            hook_program: None,
            hook_flags: 0,
            hook_authority: Some(owner.pubkey()),
            metadata_authority: Some(owner.pubkey()),
        };
        let ixs = [
            token::create_mint(owner.pubkey(), mint.pubkey(), args),
            token::create_holding(owner.pubkey(), mint.pubkey(), owner.pubkey()),
            token::mint_to(
                owner.pubkey(),
                mint.pubkey(),
                token::holding_address(&mint.pubkey(), &owner.pubkey()),
                None,
                vec![],
                supply,
            ),
        ];
        self.env.send_paid_by(&ixs, owner, &[&mint]).ok();
        mint.pubkey()
    }

    /// A launch by `creator` with the policy curve at `virtual_quote` and no token rules.
    pub fn create_launch(
        &mut self,
        creator: &Keypair,
        symbol: &str,
        creator_fee_bps: u16,
        virtual_quote: u64,
    ) -> (Pubkey, Tx) {
        self.create_launch_with(
            creator,
            symbol,
            creator_fee_bps,
            virtual_quote,
            LaunchRules::NONE,
        )
    }

    /// A launch by `creator` with the policy curve at `virtual_quote` and `rules`.
    pub fn create_launch_with(
        &mut self,
        creator: &Keypair,
        symbol: &str,
        creator_fee_bps: u16,
        virtual_quote: u64,
        rules: LaunchRules,
    ) -> (Pubkey, Tx) {
        let mint = Keypair::new();
        let ix = self.create_launch_ix(
            &creator.pubkey(),
            &mint.pubkey(),
            symbol,
            creator_fee_bps,
            virtual_quote,
            rules,
        );
        let tx = self.env.send_paid_by(&[ix], creator, &[&mint]);
        (mint.pubkey(), tx)
    }

    /// The arguments of a launch of `symbol` with the fixed metadata every test launch has.
    pub fn launch_args(
        symbol: &str,
        creator_fee_bps: u16,
        virtual_quote: u64,
        rules: LaunchRules,
    ) -> bordrless_launch::instructions::CreateLaunchArgs {
        bordrless_launch::instructions::CreateLaunchArgs {
            name: format!("Launch {symbol}"),
            symbol: symbol.to_string(),
            uri: "ipfs://bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi".to_string(),
            creator_fee_bps,
            virtual_quote,
            rules,
        }
    }

    /// The `create_launch` instruction of `creator` for `mint`, with inline rules.
    pub fn create_launch_ix(
        &self,
        creator: &Pubkey,
        mint: &Pubkey,
        symbol: &str,
        creator_fee_bps: u16,
        virtual_quote: u64,
        rules: LaunchRules,
    ) -> Instruction {
        launch::create_launch(
            *creator,
            *mint,
            self.env.treasury.pubkey(),
            self.sol,
            policy::LP_FEE_BPS,
            Self::launch_args(symbol, creator_fee_bps, virtual_quote, rules),
        )
    }

    /// Makes a `LaunchConfig` by `creator` (a fresh keypair account that signs) with `args`.
    /// Answers its key and the transaction.
    pub fn create_config(&mut self, creator: &Keypair, args: CreateConfigArgs) -> (Pubkey, Tx) {
        let config = Keypair::new();
        let ix = launch::create_config(creator.pubkey(), config.pubkey(), args);
        let tx = self.env.send_paid_by(&[ix], creator, &[&config]);
        (config.pubkey(), tx)
    }

    /// A `LaunchConfig`, read.
    pub fn launch_config(&self, config: &Pubkey) -> LaunchConfig {
        self.env.read(config)
    }

    /// The custom hook `hook`'s accounts for the launch of `mint`, its extras resolved from the
    /// hook's registry as a client resolves them (with the launch's deposit as the prefix: the
    /// reserve to the pool's base vault, the launch signing).
    pub fn custom_hook_accounts(&self, hook: &Pubkey, mint: &Pubkey) -> launch::CustomHookAccounts {
        let launch_key = launch::launch_address(mint);
        let pool = self.launch_pool_key(mint);
        let extras = self.env.token_hook_extras(
            hook,
            mint,
            &token::holding_address(mint, &launch_key),
            &swap::vault_address(&pool, mint),
            &launch_key,
            &launch_key,
            &pool,
        );
        launch::CustomHookAccounts {
            program: *hook,
            extras,
        }
    }

    /// The `create_launch` instruction of `creator` for `mint` from the `LaunchConfig` at
    /// `config` (read from the SVM): the arguments repeat the config's rules and creator fee, and
    /// a custom hook's accounts are resolved from its registry.
    pub fn create_launch_from_config_ix(
        &self,
        creator: &Pubkey,
        mint: &Pubkey,
        symbol: &str,
        virtual_quote: u64,
        config: &Pubkey,
    ) -> Instruction {
        let c = self.launch_config(config);
        let custom = c.custom_hook.map(|h| self.custom_hook_accounts(&h, mint));
        launch::create_launch_with(
            *creator,
            *mint,
            self.env.treasury.pubkey(),
            self.sol,
            policy::LP_FEE_BPS,
            Self::launch_args(symbol, c.creator_fee_bps, virtual_quote, c.rules),
            Some(*config),
            custom.as_ref(),
        )
    }

    /// A launch by `creator` from the `LaunchConfig` at `config`.
    pub fn create_launch_from_config(
        &mut self,
        creator: &Keypair,
        symbol: &str,
        virtual_quote: u64,
        config: &Pubkey,
    ) -> (Pubkey, Tx) {
        let mint = Keypair::new();
        let ix = self.create_launch_from_config_ix(
            &creator.pubkey(),
            &mint.pubkey(),
            symbol,
            virtual_quote,
            config,
        );
        let tx = self.env.send_paid_by(&[ix], creator, &[&mint]);
        (mint.pubkey(), tx)
    }

    /// What a client needs of the launch of `mint` to build its swaps and graduation.
    pub fn launch_keys(&self, mint: &Pubkey) -> launch::LaunchKeys {
        launch::LaunchKeys::of(&self.launch(mint))
    }

    /// The token-hook slice of a launch's mint for a swap by `trader` delivered to `recipient`,
    /// resolved from the mint's hook registry as a client resolves it (the kit's two fixed
    /// extras, a custom hook's own, none without a hook).
    pub fn launch_base_slice(
        &self,
        mint: &Pubkey,
        trader: &Pubkey,
        recipient: &Pubkey,
        buy: bool,
    ) -> Vec<AccountMeta> {
        if self.env.read::<Mint>(mint).hook_program.is_none() {
            return vec![];
        }
        let pool = self.launch_pool_key(mint);
        let vault = swap::vault_address(&pool, mint);
        if buy {
            self.env.token_hook_slice(
                mint,
                &vault,
                &token::holding_address(mint, recipient),
                &pool,
                &pool,
                recipient,
            )
        } else {
            self.env.token_hook_slice(
                mint,
                &token::holding_address(mint, trader),
                &vault,
                trader,
                trader,
                &pool,
            )
        }
    }

    /// The swap instruction of `trader` on a launch pool, delivered to itself. `direction` 1 buys
    /// with bridged SOL.
    pub fn launch_swap_ix(
        &self,
        trader: &Pubkey,
        mint: &Pubkey,
        direction: u8,
        amount_in: u64,
        min_out: u64,
    ) -> Instruction {
        let slice = self.launch_base_slice(mint, trader, trader, direction == 1);
        launch::swap_with_base_slice(
            &self.launch_keys(mint),
            *trader,
            *trader,
            direction,
            amount_in,
            min_out,
            slice,
        )
    }

    /// The `graduate` instruction of the launch of `mint`, cranked by `cranker` (with a custom
    /// hook's slice when the token has one).
    pub fn graduate_ix(&self, cranker: &Pubkey, mint: &Pubkey) -> Instruction {
        let keys = self.launch_keys(mint);
        let custom = keys
            .custom_hook
            .map(|h| self.custom_hook_accounts(&h, mint));
        launch::graduate_with(
            *cranker,
            keys.mint,
            keys.quote_mint,
            keys.lp_fee_bps,
            keys.modules,
            custom.as_ref(),
        )
    }

    /// Buys `lamports` of a launch with bridged SOL (creating the trader's holding first).
    pub fn buy(&mut self, trader: &Keypair, mint: &Pubkey, lamports: u64) -> Tx {
        let ixs = [
            token::create_holding(trader.pubkey(), *mint, trader.pubkey()),
            self.launch_swap_ix(&trader.pubkey(), mint, 1, lamports, 0),
        ];
        self.env.send_paid_by(&ixs, trader, &[])
    }

    /// Sells `amount` tokens of a launch for bridged SOL.
    pub fn sell(&mut self, trader: &Keypair, mint: &Pubkey, amount: u64) -> Tx {
        let ix = self.launch_swap_ix(&trader.pubkey(), mint, 0, amount, 0);
        self.env.send_paid_by(&[ix], trader, &[])
    }

    /// The pool of a launch.
    pub fn launch_pool(&self, mint: &Pubkey) -> bordrless_swap::state::Pool {
        self.env.read(&self.launch_pool_key(mint))
    }

    /// The launch account.
    pub fn launch(&self, mint: &Pubkey) -> bordrless_launch::state::Launch {
        self.env.read(&launch::launch_address(mint))
    }
}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}
