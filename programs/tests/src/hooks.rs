//! Hooks in tests: mints and pools whose hook is the test-only `hook_tester` (answers scripted per
//! callback), mints with `tax_hook`, the extra accounts a client resolves from a hook's registry
//! (a token hook's, a pool hook's), and DEX swaps built from them as a client builds them.

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::InstructionData;
use bordrless_hook::{
    hook_accounts_address, AccountSource, ExtraAccount, HookAccountList, HookReturn,
};
use bordrless_swap::client as swap;
use bordrless_swap::instructions::{CreatePoolArgs, SwapArgs};
use bordrless_swap::state::Pool;
use bordrless_token::client as token;
use bordrless_token::instructions::CreateMintArgs;
use bordrless_token::state::Mint;
use hook_tester::client as tester;
use solana_keypair::Keypair;
use solana_signer::Signer;

use crate::env::{Env, Tx, SYSTEM_PROGRAM_ID};
use crate::fixture::World;

/// A registry entry for a fixed account.
pub fn fixed(key: Pubkey, writable: bool) -> ExtraAccount {
    ExtraAccount {
        writable,
        source: AccountSource::Key(key),
    }
}

/// `tax_hook`'s `install`: the mint's hook authority signs and pays.
pub fn tax_install(
    authority: Pubkey,
    mint: Pubkey,
    collector_holding: Pubkey,
    fee_bps: u16,
    max_wallet_bps: u16,
) -> Instruction {
    let tax = Pubkey::find_program_address(&[tax_hook::TAX_SEED, mint.as_ref()], &tax_hook::ID).0;
    Instruction {
        program_id: tax_hook::ID,
        accounts: vec![
            AccountMeta::new(authority, true),
            AccountMeta::new(mint, false),
            AccountMeta::new(tax, false),
            AccountMeta::new_readonly(collector_holding, false),
            AccountMeta::new(hook_accounts_address(&tax_hook::ID, &mint).0, false),
            AccountMeta::new_readonly(bordrless_token::ID, false),
            AccountMeta::new_readonly(token::event_authority(), false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        ],
        data: tax_hook::instruction::Install {
            fee_bps,
            max_wallet_bps,
        }
        .data(),
    }
}

impl Env {
    /// The extra accounts of a token operation on `mint` whose hook is `hook`, resolved from the
    /// hook's registry as a client does. `source` and `destination` are the prefix's (the mint
    /// stands in for the side a mint or a burn lacks).
    #[allow(clippy::too_many_arguments)]
    pub fn token_hook_extras(
        &self,
        hook: &Pubkey,
        mint: &Pubkey,
        source: &Pubkey,
        destination: &Pubkey,
        authority: &Pubkey,
        source_owner: &Pubkey,
        destination_owner: &Pubkey,
    ) -> Vec<AccountMeta> {
        let registry = hook_accounts_address(hook, mint).0;
        let list = HookAccountList::decode(&self.account(&registry).expect("registry").data)
            .expect("registry decodes");
        list.resolve(
            &[
                token::hook_signer(hook),
                *mint,
                *source,
                *destination,
                *authority,
            ],
            source_owner,
            destination_owner,
        )
        .expect("registry resolves")
    }

    /// The hook data of a holding.
    pub fn hook_data(&self, mint: &Pubkey, owner: &Pubkey) -> [u8; 64] {
        self.read::<bordrless_token::state::Holding>(&token::holding_address(mint, owner))
            .hook_data
    }

    /// A mint's token-hook slice for a DEX instruction: the mint's hook program, the token
    /// program's signer of its callbacks, then the hook's extra accounts resolved for a transfer
    /// from `source` to `destination`; empty for a mint without a hook.
    #[allow(clippy::too_many_arguments)]
    pub fn token_hook_slice(
        &self,
        mint: &Pubkey,
        source: &Pubkey,
        destination: &Pubkey,
        authority: &Pubkey,
        source_owner: &Pubkey,
        destination_owner: &Pubkey,
    ) -> Vec<AccountMeta> {
        match self.read::<Mint>(mint).hook_program {
            Some(hook) => swap::token_hook_slice(
                Some(hook),
                self.token_hook_extras(
                    &hook,
                    mint,
                    source,
                    destination,
                    authority,
                    source_owner,
                    destination_owner,
                ),
            ),
            None => vec![],
        }
    }

    /// The extra accounts of a pool callback, resolved from the pool hook's registry at
    /// `["bordrless-hook-accounts", pool]` as a client does (the prefix: the DEX's signer for the
    /// hook, the pool, its mints and the actor); empty when the hook keeps no registry for the
    /// pool.
    pub fn pool_hook_extras(
        &self,
        hook: &Pubkey,
        pool: &Pubkey,
        base_mint: &Pubkey,
        quote_mint: &Pubkey,
        actor: &Pubkey,
    ) -> Vec<AccountMeta> {
        let registry = hook_accounts_address(hook, pool).0;
        let Some(account) = self.account(&registry) else {
            return vec![];
        };
        HookAccountList::decode(&account.data)
            .expect("registry decodes")
            .resolve(
                &[
                    swap::hook_signer(hook),
                    *pool,
                    *base_mint,
                    *quote_mint,
                    *actor,
                ],
                &Pubkey::default(),
                &Pubkey::default(),
            )
            .expect("registry resolves")
    }

    /// The keys, arguments and remaining accounts of the swap `spec` describes, with each mint's
    /// token-hook slice and the pool hook's extras resolved as a client resolves them.
    pub fn swap_parts(&self, spec: &SwapSpec) -> (swap::SwapKeys, SwapArgs, Vec<AccountMeta>) {
        let pool: Pool = self.read(&spec.pool);
        let (base, quote) = (pool.base_mint, pool.quote_mint);
        let buy = spec.direction == 1;
        let (in_mint, out_mint) = if buy { (quote, base) } else { (base, quote) };
        let trader_in = token::holding_address(&in_mint, &spec.trader);
        let trader_out = token::holding_address(&out_mint, &spec.recipient);
        let in_slice = self.token_hook_slice(
            &in_mint,
            &trader_in,
            &swap::vault_address(&spec.pool, &in_mint),
            &spec.trader,
            &spec.trader,
            &spec.pool,
        );
        let out_slice = self.token_hook_slice(
            &out_mint,
            &swap::vault_address(&spec.pool, &out_mint),
            &trader_out,
            &spec.pool,
            &spec.pool,
            &spec.recipient,
        );
        let args = SwapArgs {
            direction: spec.direction,
            amount_in: spec.amount_in,
            min_amount_out: spec.min_out,
            in_hook_accounts: in_slice.len() as u8,
            out_hook_accounts: out_slice.len() as u8,
            hook_data: spec.hook_data.clone(),
        };
        let mut extras = in_slice;
        extras.extend(out_slice);
        if let Some(hook) = pool.hook_program {
            extras.extend(self.pool_hook_extras(&hook, &spec.pool, &base, &quote, &spec.trader));
        }
        let (trader_base, trader_quote) = if buy {
            (trader_out, trader_in)
        } else {
            (trader_in, trader_out)
        };
        let keys = swap::SwapKeys {
            trader: spec.trader,
            pool: spec.pool,
            base_mint: base,
            quote_mint: quote,
            trader_base,
            trader_quote,
            hook_program: pool.hook_program,
            base_mint_writable: spec.base_mint_writable,
            quote_mint_writable: spec.quote_mint_writable,
        };
        (keys, args, extras)
    }

    /// The swap `spec` describes.
    pub fn swap_ix(&self, spec: &SwapSpec) -> Instruction {
        let (keys, args, extras) = self.swap_parts(spec);
        swap::swap(&keys, args, extras)
    }

    /// The swap `spec` describes, performed by CPI from `hook_tester`'s router.
    pub fn routed_swap_ix(&self, spec: &SwapSpec) -> Instruction {
        let (keys, args, extras) = self.swap_parts(spec);
        tester::route(&keys, args, extras)
    }

    /// `collect_protocol_fees` of `pool` into `collector`'s quote holding, with the quote mint's
    /// token-hook slice.
    pub fn collect_ix(&self, admin: &Pubkey, pool: &Pubkey, collector: &Pubkey) -> Instruction {
        let p: Pool = self.read(pool);
        let slice = self.token_hook_slice(
            &p.quote_mint,
            &p.quote_vault,
            &token::holding_address(&p.quote_mint, collector),
            pool,
            pool,
            collector,
        );
        swap::collect_protocol_fees(*admin, *pool, p.quote_mint, *collector, slice)
    }
}

/// A swap as a test describes it; [`Env::swap_parts`] resolves the rest.
#[derive(Clone, Debug)]
pub struct SwapSpec {
    /// Signs; owns the input holding.
    pub trader: Pubkey,
    /// The pool.
    pub pool: Pubkey,
    /// 0 sells base for quote, 1 buys base with quote.
    pub direction: u8,
    /// Exact input.
    pub amount_in: u64,
    /// The least the recipient's holding must gain.
    pub min_out: u64,
    /// The owner of the holding the output is delivered to.
    pub recipient: Pubkey,
    /// Pass the base mint writable (a hook may burn base).
    pub base_mint_writable: bool,
    /// Pass the quote mint writable (a hook may burn quote).
    pub quote_mint_writable: bool,
    /// Opaque data for the pool hook.
    pub hook_data: Vec<u8>,
}

impl SwapSpec {
    /// `trader` swaps `amount_in` on `pool` (direction 1 buys base), receiving the output
    /// itself, with no minimum and both mints read-only.
    pub fn new(trader: Pubkey, pool: Pubkey, direction: u8, amount_in: u64) -> Self {
        Self {
            trader,
            pool,
            direction,
            amount_in,
            min_out: 0,
            recipient: trader,
            base_mint_writable: false,
            quote_mint_writable: false,
            hook_data: vec![],
        }
    }

    /// Both mints writable, so a hook may burn from either side.
    pub fn burnable(self) -> Self {
        Self {
            base_mint_writable: true,
            quote_mint_writable: true,
            ..self
        }
    }
}

/// A pool a test creates; [`World::new_pool`] resolves the rest.
#[derive(Clone, Debug)]
pub struct NewPool {
    /// Base mint.
    pub base: Pubkey,
    /// Quote mint.
    pub quote: Pubkey,
    /// LP fee (part of the pool's address).
    pub lp_fee_bps: u16,
    /// `hook_tester` as the pool's hook with these flags; `None` for a pool without a hook.
    pub tester_flags: Option<u16>,
    /// The hook_tester registry's accounts after the script (index 6 onwards in a callback).
    pub extras: Vec<ExtraAccount>,
    /// Base deposited.
    pub base_amount: u64,
    /// Quote deposited.
    pub quote_amount: u64,
}

impl World {
    /// Creates `mint` with `hook_tester` as its hook and `flags` (`owner` holds every authority),
    /// the script and registry of the hook (the script, then `extras`) and `owner`'s holding, and
    /// mints `supply` into it with the hook's accounts.
    pub fn tester_mint(
        &mut self,
        mint: &Keypair,
        owner: &Keypair,
        flags: u16,
        supply: u64,
        extras: Vec<ExtraAccount>,
    ) -> Pubkey {
        let key = mint.pubkey();
        let args = CreateMintArgs {
            decimals: 6,
            name: "Hooked".to_string(),
            symbol: "HKT".to_string(),
            uri: String::new(),
            max_supply: 0,
            mint_authority: Some(owner.pubkey()),
            freeze_authority: Some(owner.pubkey()),
            hook_program: Some(hook_tester::ID),
            hook_flags: flags,
            hook_authority: Some(owner.pubkey()),
            metadata_authority: Some(owner.pubkey()),
        };
        let ixs = [
            token::create_mint(owner.pubkey(), key, args),
            tester::init_script(owner.pubkey(), key, extras),
            token::create_holding(owner.pubkey(), key, owner.pubkey()),
        ];
        self.env.send_paid_by(&ixs, owner, &[mint]).ok();
        if supply > 0 {
            self.tester_mint_to(owner, key, &owner.pubkey(), supply)
                .ok();
        }
        key
    }

    /// `mint_to` of a hook_tester mint into `to`'s holding, with the hook's accounts.
    pub fn tester_mint_to(
        &mut self,
        authority: &Keypair,
        mint: Pubkey,
        to: &Pubkey,
        amount: u64,
    ) -> Tx {
        let holding = token::holding_address(&mint, to);
        let extras = self.env.token_hook_extras(
            &hook_tester::ID,
            &mint,
            &mint,
            &holding,
            &authority.pubkey(),
            &Pubkey::default(),
            to,
        );
        self.env.send_paid_by(
            &[token::mint_to(
                authority.pubkey(),
                mint,
                holding,
                Some(hook_tester::ID),
                extras,
                amount,
            )],
            authority,
            &[],
        )
    }

    /// The `transfer` of a hooked mint from `from`'s holding to `to`'s, signed by `authority`, with
    /// the hook's accounts from its registry.
    pub fn hooked_transfer_ix(
        &self,
        hook: Pubkey,
        mint: Pubkey,
        authority: Pubkey,
        from: &Pubkey,
        to: &Pubkey,
        amount: u64,
    ) -> Instruction {
        let (source, destination) = (
            token::holding_address(&mint, from),
            token::holding_address(&mint, to),
        );
        let extras =
            self.env
                .token_hook_extras(&hook, &mint, &source, &destination, &authority, from, to);
        token::transfer(
            authority,
            source,
            destination,
            mint,
            Some(hook),
            extras,
            amount,
        )
    }

    /// Sends [`World::hooked_transfer_ix`] of a hook_tester mint, paid and signed by `authority`.
    pub fn tester_transfer(
        &mut self,
        authority: &Keypair,
        mint: Pubkey,
        from: &Pubkey,
        to: &Pubkey,
        amount: u64,
    ) -> Tx {
        let ix =
            self.hooked_transfer_ix(hook_tester::ID, mint, authority.pubkey(), from, to, amount);
        self.env.send_paid_by(&[ix], authority, &[])
    }

    /// `burn` of a hook_tester mint from `from`'s holding, signed by `authority`.
    pub fn tester_burn(
        &mut self,
        authority: &Keypair,
        mint: Pubkey,
        from: &Pubkey,
        amount: u64,
    ) -> Tx {
        let source = token::holding_address(&mint, from);
        let extras = self.env.token_hook_extras(
            &hook_tester::ID,
            &mint,
            &source,
            &mint,
            &authority.pubkey(),
            from,
            &Pubkey::default(),
        );
        self.env.send_paid_by(
            &[token::burn(
                authority.pubkey(),
                source,
                mint,
                Some(hook_tester::ID),
                extras,
                amount,
            )],
            authority,
            &[],
        )
    }

    /// Scripts callback `which` of `mint`'s hook_tester script to answer `answer`; the script's
    /// authority signs.
    pub fn script_answer(
        &mut self,
        authority: &Keypair,
        mint: Pubkey,
        which: u8,
        answer: &HookReturn,
    ) {
        self.env
            .send_paid_by(
                &[tester::answer(authority.pubkey(), mint, which, answer)],
                authority,
                &[],
            )
            .ok();
    }

    /// Scripts callback `which` of `mint`'s hook_tester script to return raw `data` (or, with
    /// another `action`, to answer nothing or to fail).
    pub fn script_raw(
        &mut self,
        authority: &Keypair,
        mint: Pubkey,
        which: u8,
        action: u8,
        data: Vec<u8>,
    ) {
        self.env
            .send_paid_by(
                &[tester::set_answer(
                    authority.pubkey(),
                    mint,
                    which,
                    action,
                    data,
                )],
                authority,
                &[],
            )
            .ok();
    }

    /// Creates the holdings of `owners` for `mint`, paid by `payer`.
    pub fn holdings(&mut self, payer: &Keypair, mint: Pubkey, owners: &[Pubkey]) {
        let ixs: Vec<Instruction> = owners
            .iter()
            .map(|o| token::create_holding(payer.pubkey(), mint, *o))
            .collect();
        self.env.send_paid_by(&ixs, payer, &[]).ok();
    }

    /// A mint whose hook is `tax_hook` charging `fee_bps` on every transfer (no wallet cap) to
    /// `collector`'s holding; `owner` holds `supply` and every authority.
    pub fn tax_mint(
        &mut self,
        owner: &Keypair,
        decimals: u8,
        supply: u64,
        symbol: &str,
        collector: Pubkey,
        fee_bps: u16,
    ) -> Pubkey {
        let mint = self.mint_to_owner(owner, decimals, supply, symbol);
        let ixs = [
            token::create_holding(owner.pubkey(), mint, collector),
            tax_install(
                owner.pubkey(),
                mint,
                token::holding_address(&mint, &collector),
                fee_bps,
                0,
            ),
        ];
        self.env.send_paid_by(&ixs, owner, &[]).ok();
        mint
    }

    /// `transfer` of `amount` of `mint` (with or without a hook) from `from`'s holding to `to`'s,
    /// signed and paid by `from`.
    pub fn send_tokens(&mut self, from: &Keypair, mint: Pubkey, to: &Pubkey, amount: u64) -> Tx {
        let (source, destination) = (
            token::holding_address(&mint, &from.pubkey()),
            token::holding_address(&mint, to),
        );
        let hook = self.env.read::<Mint>(&mint).hook_program;
        let extras = match hook {
            Some(hook) => self.env.token_hook_extras(
                &hook,
                &mint,
                &source,
                &destination,
                &from.pubkey(),
                &from.pubkey(),
                to,
            ),
            None => vec![],
        };
        let ix = token::transfer(
            from.pubkey(),
            source,
            destination,
            mint,
            hook,
            extras,
            amount,
        );
        self.env.send_paid_by(&[ix], from, &[])
    }

    /// Creates the pool `spec` describes, `authority` depositing and paying. With `hook_tester` as
    /// its hook, the script and registry of the pool (the script, then `spec.extras`) are created
    /// first. Answers the pool and the creation's transaction.
    pub fn new_pool(&mut self, authority: &Keypair, spec: &NewPool) -> (Pubkey, Tx) {
        let hook = spec.tester_flags.map(|_| hook_tester::ID);
        let pool = swap::pool_address(&spec.base, &spec.quote, spec.lp_fee_bps, hook);
        if hook.is_some() {
            self.env
                .send_paid_by(
                    &[tester::init_script(
                        authority.pubkey(),
                        pool,
                        spec.extras.clone(),
                    )],
                    authority,
                    &[],
                )
                .ok();
        }
        let me = authority.pubkey();
        let slice = |env: &Env, mint: &Pubkey| {
            env.token_hook_slice(
                mint,
                &token::holding_address(mint, &me),
                &swap::vault_address(&pool, mint),
                &me,
                &me,
                &pool,
            )
        };
        let base_slice = slice(&self.env, &spec.base);
        let quote_slice = slice(&self.env, &spec.quote);
        let args = CreatePoolArgs {
            lp_fee_bps: spec.lp_fee_bps,
            hook_program: hook.unwrap_or_default(),
            hook_flags: spec.tester_flags.unwrap_or(0),
            virtual_base: 0,
            virtual_quote: 0,
            base_amount: spec.base_amount,
            quote_amount: spec.quote_amount,
            base_hook_accounts: base_slice.len() as u8,
            quote_hook_accounts: quote_slice.len() as u8,
            hook_data: vec![],
        };
        let mut extras = base_slice;
        extras.extend(quote_slice);
        if let Some(hook) = hook {
            extras.extend(
                self.env
                    .pool_hook_extras(&hook, &pool, &spec.base, &spec.quote, &me),
            );
        }
        let keys = swap::CreatePoolKeys {
            payer: me,
            authority: me,
            treasury: self.env.treasury.pubkey(),
            base_mint: spec.base,
            quote_mint: spec.quote,
            hook_caller: None,
        };
        let tx = self
            .env
            .send_paid_by(&[swap::create_pool(&keys, args, extras)], authority, &[]);
        (pool, tx)
    }
}
