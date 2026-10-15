//! Native SOL in tests: the migration runbook (`docs/native-sol-migration.md`) as the admin runs
//! it, the DEX pause around it, and the pool invariant.

use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::instruction::Instruction;
use bordrless_launch::client as launch;
use bordrless_swap::client as swap;
use bordrless_swap::state::Pool;
use solana_keypair::Keypair;
use solana_signer::Signer;

use crate::env::Tx;
use crate::fixture::World;

impl World {
    /// The admin (the deployer: every config's admin and every program's upgrade authority here).
    pub fn admin(&self) -> Keypair {
        self.env.deployer.insecure_clone()
    }

    /// The DEX's `set_config` with `paused`, everything else as it stands.
    pub fn dex_pause_ix(&self, paused: bool) -> Instruction {
        let c: bordrless_swap::state::Config = self.env.read(&swap::config_address());
        swap::set_config(
            c.admin,
            bordrless_swap::instructions::ConfigArgs {
                admin: c.admin,
                protocol_fee_bps: c.protocol_fee_bps,
                fee_collector: c.fee_collector,
                treasury: c.treasury,
                pool_creation_fee_lamports: c.pool_creation_fee_lamports,
                paused,
                launch_protocol_share_bps: c.launch_protocol_share_bps,
            },
        )
    }

    /// Sends `ix` signed and paid by the admin, then a slot passes.
    pub fn as_admin(&mut self, ix: Instruction) -> Tx {
        let a = self.admin();
        let tx = self.env.send(&[ix], &[&a]);
        self.env.warp(1);
        tx
    }

    /// Pauses (or unpauses) the DEX.
    pub fn pause_dex(&mut self, paused: bool) {
        let ix = self.dex_pause_ix(paused);
        self.as_admin(ix).ok();
    }

    /// The pool and the launch of `mint` turned native, in that order, the DEX paused around them
    /// (the runbook's first two steps for one coin). Answers the launch's migration.
    pub fn migrate_launch_native(&mut self, mint: &Pubkey) -> Tx {
        let a = self.admin().pubkey();
        let modules = self.launch(mint).modules;
        let pool = self.launch_pool_key(mint);
        let sol = self.sol;
        self.pause_dex(true);
        self.as_admin(swap::migrate_pool_to_native(a, pool, 0)).ok();
        let tx = self.as_admin(launch::migrate_launch_to_native(a, *mint, sol, modules));
        self.pause_dex(false);
        tx
    }

    /// The whole runbook for one companion coin: pool, launch, companion, the DEX paused around
    /// them. Answers the companion's migration.
    pub fn migrate_companion_native(&mut self, mint: &Pubkey) -> Tx {
        let a = self.admin().pubkey();
        let modules = self.launch(mint).modules;
        let pool = self.launch_pool_key(mint);
        let sol = self.sol;
        self.pause_dex(true);
        self.as_admin(swap::migrate_pool_to_native(a, pool, 0)).ok();
        self.as_admin(launch::migrate_launch_to_native(a, *mint, sol, modules))
            .ok();
        let tx = self.as_admin(bordrless_companion::client::migrate_companion_to_native(
            a, *mint,
        ));
        self.pause_dex(false);
        tx
    }

    /// The way back for one companion coin: companion, launch, pool.
    pub fn revert_companion_native(&mut self, mint: &Pubkey) -> Tx {
        let a = self.admin().pubkey();
        let modules = self.launch(mint).modules;
        let pool = self.launch_pool_key(mint);
        let sol = self.sol;
        self.pause_dex(true);
        let tx = self.as_admin(bordrless_companion::client::revert_companion_to_bridged(
            a, *mint,
        ));
        self.as_admin(launch::revert_launch_to_bridged(a, *mint, sol, modules))
            .ok();
        self.as_admin(swap::revert_launch_pool_to_bridged(a, pool, *mint))
            .ok();
        self.pause_dex(false);
        tx
    }

    /// A pool's invariant: native, lamports = rent + books - buffer and vault = buffer; bridged,
    /// lamports = rent and vault = books.
    #[track_caller]
    pub fn assert_pool_invariant(&self, pool: &Pubkey) {
        let p: Pool = self.env.read(pool);
        let rent = self.env.rent(Pool::LEN);
        let vault = self.env.holding(&self.sol, pool);
        let books = p.quote_reserve + p.protocol_fees_quote;
        if p.quote_native() {
            assert_eq!(self.env.lamports(pool), rent + books - p.buffer_quote());
            assert_eq!(vault, p.buffer_quote());
        } else {
            assert_eq!(self.env.lamports(pool), rent);
            assert_eq!(vault, books);
        }
    }

    /// The SOL a companion's creator address holds: bridged SOL in its holding plus its lamports
    /// above the minimum of an empty account (the two forms; only one is its pot at a time).
    pub fn creator_sol(&self, mint: &Pubkey) -> (u64, u64) {
        let creator = bordrless_companion::client::creator_address(mint);
        (
            self.env.holding(&self.sol, &creator),
            self.env.lamports(&creator) - self.env.rent(0),
        )
    }
}
