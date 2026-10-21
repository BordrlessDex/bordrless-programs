//! Quote tokens in tests (`docs/token-quotes.md`): SPL Token and Token-2022 mints written by hand
//! (their extensions laid out as Token-2022 lays them out: the base mint, zero padding to 165
//! bytes, the account type, then TLV entries), token accounts made by the associated token account
//! program (so Token-2022 sizes them for the mint's extensions), launches quoted in tokens and
//! their legs, and a kind-2 pool's invariant.
//!
//! The SPL programs are LiteSVM 0.17.0's own (`LiteSVM::new` loads SPL Token, Token-2022
//! `spl_token_2022-11.0.0.so` and the ATA program `spl_associated_token_account-1.1.1.so`).

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use bordrless_launch::client as launch;
use bordrless_launch::instructions::CreateTokenLaunchArgs;
use bordrless_launch::state::{Launch, LaunchRules, Legs};
use bordrless_swap::spl;
use bordrless_swap::state::Pool;
use bordrless_token::client as token;
use solana_account::Account;
use solana_keypair::Keypair;
use solana_signer::Signer;

use crate::env::{Env, Tx, SYSTEM_PROGRAM_ID};
use crate::fixture::World;

/// SPL Token.
pub const TOKEN: Pubkey = spl::TOKEN_PROGRAM_ID;
/// Token-2022.
pub const TOKEN_2022: Pubkey = spl::TOKEN_2022_PROGRAM_ID;
/// A trader's quote tokens in the suites, raw units.
pub const QUOTE_FUNDS: u64 = 1_000_000_000_000_000;

/// Token-2022 mint extensions as `(type, value)`, each value as Token-2022 lays it out
/// (spl-token-2022-interface 3.1.2; `bordrless_swap::spl::ext` names the numbers).
pub mod ext {
    use super::Pubkey;
    use bordrless_swap::spl::ext as n;

    /// One extension.
    pub type Ext = (u16, Vec<u8>);

    fn key(k: Option<Pubkey>) -> Vec<u8> {
        k.map_or(vec![0; 32], |k| k.to_bytes().to_vec())
    }

    fn borsh_str(out: &mut Vec<u8>, s: &str) {
        out.extend_from_slice(&(s.len() as u32).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
    }

    /// `TransferFeeConfig` (refused): both fees at `bps`.
    pub fn transfer_fee(bps: u16) -> Ext {
        let mut v = vec![0u8; 108];
        v[88..90].copy_from_slice(&bps.to_le_bytes());
        v[106..108].copy_from_slice(&bps.to_le_bytes());
        (n::TRANSFER_FEE_CONFIG, v)
    }
    /// `MintCloseAuthority`.
    pub fn mint_close(authority: Pubkey) -> Ext {
        (n::MINT_CLOSE_AUTHORITY, key(Some(authority)))
    }
    /// `ConfidentialTransferMint` (authority, auto-approve, no auditor).
    pub fn confidential_mint() -> Ext {
        (n::CONFIDENTIAL_TRANSFER_MINT, vec![0; 65])
    }
    /// `DefaultAccountState` (1 initialized, 2 frozen).
    pub fn default_state(state: u8) -> Ext {
        (n::DEFAULT_ACCOUNT_STATE, vec![state])
    }
    /// `NonTransferable` (refused).
    pub fn non_transferable() -> Ext {
        (n::NON_TRANSFERABLE, vec![])
    }
    /// `InterestBearingConfig` (rate 0).
    pub fn interest_bearing(authority: Pubkey) -> Ext {
        let mut v = key(Some(authority));
        v.extend_from_slice(&[0; 20]);
        (n::INTEREST_BEARING_CONFIG, v)
    }
    /// `PermanentDelegate`.
    pub fn permanent_delegate(delegate: Pubkey) -> Ext {
        (n::PERMANENT_DELEGATE, key(Some(delegate)))
    }
    /// `TransferHook`: its authority and its program (none: allowed).
    pub fn transfer_hook(authority: Option<Pubkey>, program: Option<Pubkey>) -> Ext {
        let mut v = key(authority);
        v.extend(key(program));
        (n::TRANSFER_HOOK, v)
    }
    /// `ConfidentialTransferFeeConfig` (refused).
    pub fn confidential_fee() -> Ext {
        (n::CONFIDENTIAL_TRANSFER_FEE_CONFIG, vec![0; 129])
    }
    /// `MetadataPointer` to `address`.
    pub fn metadata_pointer(authority: Pubkey, address: Pubkey) -> Ext {
        let mut v = key(Some(authority));
        v.extend(key(Some(address)));
        (n::METADATA_POINTER, v)
    }
    /// `TokenMetadata` (variable length).
    pub fn token_metadata(authority: Pubkey, mint: Pubkey, name: &str, symbol: &str) -> Ext {
        let mut v = key(Some(authority));
        v.extend(key(Some(mint)));
        borsh_str(&mut v, name);
        borsh_str(&mut v, symbol);
        borsh_str(&mut v, "https://example.com/token.json");
        v.extend_from_slice(&0u32.to_le_bytes());
        (n::TOKEN_METADATA, v)
    }
    /// `GroupPointer`.
    pub fn group_pointer(authority: Pubkey, address: Pubkey) -> Ext {
        let mut v = key(Some(authority));
        v.extend(key(Some(address)));
        (n::GROUP_POINTER, v)
    }
    /// `TokenGroup` (update authority, mint, size, max size).
    pub fn token_group(authority: Pubkey, mint: Pubkey) -> Ext {
        let mut v = key(Some(authority));
        v.extend(key(Some(mint)));
        v.extend_from_slice(&0u64.to_le_bytes());
        v.extend_from_slice(&10u64.to_le_bytes());
        (n::TOKEN_GROUP, v)
    }
    /// `GroupMemberPointer`.
    pub fn group_member_pointer(authority: Pubkey, address: Pubkey) -> Ext {
        let mut v = key(Some(authority));
        v.extend(key(Some(address)));
        (n::GROUP_MEMBER_POINTER, v)
    }
    /// `TokenGroupMember` (mint, group, member number).
    pub fn token_group_member(mint: Pubkey, group: Pubkey) -> Ext {
        let mut v = key(Some(mint));
        v.extend(key(Some(group)));
        v.extend_from_slice(&1u64.to_le_bytes());
        (n::TOKEN_GROUP_MEMBER, v)
    }
    /// `ConfidentialMintBurn` (refused).
    pub fn confidential_mint_burn() -> Ext {
        (n::CONFIDENTIAL_MINT_BURN, vec![0; 196])
    }
    /// `ScaledUiAmount` at a multiplier of 1.
    pub fn scaled_ui_amount(authority: Pubkey) -> Ext {
        let mut v = key(Some(authority));
        v.extend_from_slice(&1f64.to_le_bytes());
        v.extend_from_slice(&0i64.to_le_bytes());
        v.extend_from_slice(&1f64.to_le_bytes());
        (n::SCALED_UI_AMOUNT, v)
    }
    /// `Pausable` (paused or not).
    pub fn pausable(authority: Pubkey, paused: bool) -> Ext {
        let mut v = key(Some(authority));
        v.push(u8::from(paused));
        (n::PAUSABLE, v)
    }
    /// Any number, for the unknown-extension refusal.
    pub fn raw(kind: u16, len: usize) -> Ext {
        (kind, vec![0; len])
    }
}

/// A mint's data: the base mint (its authorities, supply, decimals, initialized), then, with any
/// extension, zero padding to 165 bytes, the account type (1, a mint) and the extensions.
pub fn pack_mint(
    mint_authority: Option<Pubkey>,
    freeze_authority: Option<Pubkey>,
    decimals: u8,
    supply: u64,
    extensions: &[ext::Ext],
) -> Vec<u8> {
    let mut d = vec![0u8; spl::MINT_LEN];
    if let Some(a) = mint_authority {
        d[..4].copy_from_slice(&1u32.to_le_bytes());
        d[4..36].copy_from_slice(a.as_ref());
    }
    d[36..44].copy_from_slice(&supply.to_le_bytes());
    d[44] = decimals;
    d[45] = 1;
    if let Some(f) = freeze_authority {
        d[46..50].copy_from_slice(&1u32.to_le_bytes());
        d[50..82].copy_from_slice(f.as_ref());
    }
    if extensions.is_empty() {
        return d;
    }
    d.resize(spl::ACCOUNT_LEN, 0);
    d.push(1);
    for (kind, value) in extensions {
        d.extend_from_slice(&kind.to_le_bytes());
        d.extend_from_slice(&(value.len() as u16).to_le_bytes());
        d.extend_from_slice(value);
    }
    // Token-2022 never leaves an account at a multisig's length (355): it pads two bytes (an
    // `Uninitialized` type, which ends the entries).
    if d.len() == 355 {
        d.extend_from_slice(&[0, 0]);
    }
    d
}

/// The xStocks-like extensions (`docs/token-quotes.md` §2.3): metadata, a
/// permanent delegate, a transfer hook with no program, pausable (not paused) and scaled amounts.
pub fn xstock_extensions(mint: Pubkey, issuer: Pubkey) -> Vec<ext::Ext> {
    vec![
        ext::metadata_pointer(issuer, mint),
        ext::permanent_delegate(issuer),
        ext::transfer_hook(Some(issuer), None),
        ext::pausable(issuer, false),
        ext::scaled_ui_amount(issuer),
        ext::token_metadata(issuer, mint, "Tesla xStock", "TSLAx"),
    ]
}

/// ATA `CreateIdempotent` of `owner`'s account of `mint` under `program`.
pub fn create_ata_ix(payer: Pubkey, owner: Pubkey, mint: Pubkey, program: Pubkey) -> Instruction {
    Instruction {
        program_id: spl::ATA_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(payer, true),
            AccountMeta::new(
                spl::associated_token_address(&owner, &mint, &program),
                false,
            ),
            AccountMeta::new_readonly(owner, false),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            AccountMeta::new_readonly(program, false),
        ],
        data: vec![1],
    }
}

impl Env {
    /// Writes a mint of `program` at `key`.
    pub fn put_mint(&mut self, key: Pubkey, program: Pubkey, data: Vec<u8>) {
        let lamports = self.rent(data.len());
        self.put(
            key,
            Account {
                lamports,
                data,
                owner: program,
                executable: false,
                rent_epoch: 0,
            },
        );
    }

    /// A fresh mint of `program` with `decimals` and `extensions` (the deployer its mint
    /// authority).
    pub fn new_quote_mint(
        &mut self,
        program: Pubkey,
        decimals: u8,
        extensions: &[ext::Ext],
    ) -> Pubkey {
        let key = Pubkey::new_unique();
        let data = pack_mint(
            Some(self.deployer.pubkey()),
            None,
            decimals,
            u64::MAX / 2,
            extensions,
        );
        self.put_mint(key, program, data);
        key
    }

    /// `owner`'s associated token account of `mint` (made by the ATA program if it does not
    /// exist), its balance set to `amount`.
    pub fn token_account(
        &mut self,
        owner: &Pubkey,
        mint: &Pubkey,
        program: &Pubkey,
        amount: u64,
    ) -> Pubkey {
        let key = spl::associated_token_address(owner, mint, program);
        if self.account(&key).is_none() {
            let payer = self.payer.pubkey();
            self.send(&[create_ata_ix(payer, *owner, *mint, *program)], &[])
                .ok();
        }
        self.set_token_amount(&key, amount);
        key
    }

    /// Sets a token account's balance.
    pub fn set_token_amount(&mut self, key: &Pubkey, amount: u64) {
        let mut account = self.account(key).expect("token account");
        account.data[64..72].copy_from_slice(&amount.to_le_bytes());
        self.put(*key, account);
    }

    /// A token account's balance (0 if it does not exist).
    pub fn token_amount(&self, key: &Pubkey) -> u64 {
        self.account(key).map_or(0, |a| {
            u64::from_le_bytes(a.data[64..72].try_into().expect("8 bytes"))
        })
    }

    /// A kind-2 pool's invariant, exact: its quote vault holds its books.
    #[track_caller]
    pub fn assert_spl_invariant(&self, pool: &Pubkey) {
        let p: Pool = self.read(pool);
        assert!(p.quote_spl(), "a pool quoted in an SPL token");
        assert_eq!(
            self.token_amount(&p.quote_vault),
            p.quote_reserve + p.protocol_fees_quote,
            "vault = quote_reserve + protocol_fees_quote"
        );
    }
}

/// One quote of a launch quoted in tokens: its mint and its leg's virtual quote.
pub type Quote = (Pubkey, u64);

impl World {
    /// The arguments of a launch quoted in `quotes`, without rules.
    pub fn token_launch_args(
        symbol: &str,
        creator_fee_bps: u16,
        quotes: &[Quote],
    ) -> CreateTokenLaunchArgs {
        Self::token_launch_args_with(symbol, creator_fee_bps, quotes, LaunchRules::NONE)
    }

    /// The arguments of a launch quoted in `quotes` with `rules` (`docs/token-quotes.md` §6.1).
    pub fn token_launch_args_with(
        symbol: &str,
        creator_fee_bps: u16,
        quotes: &[Quote],
        rules: LaunchRules,
    ) -> CreateTokenLaunchArgs {
        CreateTokenLaunchArgs {
            name: format!("Token {symbol}"),
            symbol: symbol.to_string(),
            uri: "ipfs://bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi".to_string(),
            creator_fee_bps,
            virtual_quotes: quotes.iter().map(|q| q.1).collect(),
            rules,
            payouts: Default::default(),
        }
    }

    /// The `create_token_launch` of `creator` for `mint`, without rules.
    pub fn token_launch_ix(
        &self,
        creator: &Pubkey,
        mint: &Pubkey,
        creator_fee_bps: u16,
        quotes: &[Quote],
    ) -> Instruction {
        self.token_launch_ix_with(creator, mint, creator_fee_bps, quotes, LaunchRules::NONE)
    }

    /// The `create_token_launch` of `creator` for `mint` with `rules` (the kit's accounts after the
    /// quote mints when they install it).
    pub fn token_launch_ix_with(
        &self,
        creator: &Pubkey,
        mint: &Pubkey,
        creator_fee_bps: u16,
        quotes: &[Quote],
        rules: LaunchRules,
    ) -> Instruction {
        let mints: Vec<Pubkey> = quotes.iter().map(|q| q.0).collect();
        launch::create_token_launch(
            *creator,
            *mint,
            self.env.treasury.pubkey(),
            Self::token_launch_args_with("TQ", creator_fee_bps, quotes, rules),
            &mints,
        )
    }

    /// A launch quoted in `quotes` by `creator`, without rules.
    pub fn create_token_launch(
        &mut self,
        creator: &Keypair,
        creator_fee_bps: u16,
        quotes: &[Quote],
    ) -> (Pubkey, Tx) {
        self.create_token_launch_with(creator, creator_fee_bps, quotes, LaunchRules::NONE)
    }

    /// A launch quoted in `quotes` by `creator` with `rules`.
    pub fn create_token_launch_with(
        &mut self,
        creator: &Keypair,
        creator_fee_bps: u16,
        quotes: &[Quote],
        rules: LaunchRules,
    ) -> (Pubkey, Tx) {
        let mint = Keypair::new();
        let ix = self.token_launch_ix_with(
            &creator.pubkey(),
            &mint.pubkey(),
            creator_fee_bps,
            quotes,
            rules,
        );
        let tx = self.env.send_paid_by(&[ix], creator, &[&mint]);
        (mint.pubkey(), tx)
    }

    /// The `Legs` of the launch of `mint`.
    pub fn legs(&self, mint: &Pubkey) -> Legs {
        self.env.read(&launch::legs_address(mint))
    }

    /// The keys of leg `index` of the launch of `mint`.
    pub fn leg_keys(&self, mint: &Pubkey, index: usize) -> launch::LegKeys {
        let legs = self.legs(mint);
        let l: Launch = self.env.read(&launch::launch_address(mint));
        let leg = legs.legs[index];
        launch::LegKeys {
            mint: *mint,
            quote_mint: leg.quote_mint,
            quote_program: spl::program_id(leg.quote_program),
            lp_fee_bps: l.lp_fee_bps,
        }
    }

    /// `open_leg(index)` of the launch of `mint`, `payer` paying (with the kit's accounts when the
    /// launch's rules installed it).
    pub fn open_leg_ix(&self, payer: &Pubkey, mint: &Pubkey, index: usize) -> Instruction {
        let l: Launch = self.env.read(&launch::launch_address(mint));
        launch::open_leg_with(
            *payer,
            self.env.treasury.pubkey(),
            &self.leg_keys(mint, index),
            index as u8,
            l.modules,
        )
    }

    /// A swap on leg `index` of the launch of `mint` by `trader`, delivered to `recipient`, as a
    /// client builds it from the launch's rules (the kit's slice with the `Legs`, the coin's mint
    /// writable when it burns).
    pub fn leg_swap_ix(
        &self,
        trader: &Pubkey,
        recipient: &Pubkey,
        mint: &Pubkey,
        index: usize,
        direction: u8,
        amount_in: u64,
    ) -> Instruction {
        let l: Launch = self.env.read(&launch::launch_address(mint));
        launch::leg_swap_for(
            &l,
            self.legs(mint).count,
            &self.leg_keys(mint, index),
            *trader,
            *recipient,
            direction,
            amount_in,
            0,
        )
    }

    /// Opens leg `index`, paid by the environment's payer.
    pub fn open_leg(&mut self, mint: &Pubkey, index: usize) -> Tx {
        let payer = self.env.payer.pubkey();
        let ix = self.open_leg_ix(&payer, mint, index);
        self.env.send(&[ix], &[])
    }

    /// Opens every leg, checking each lands.
    pub fn open_all_legs(&mut self, mint: &Pubkey) {
        for i in 0..usize::from(self.legs(mint).count) {
            self.open_leg(mint, i).ok();
        }
    }

    /// A funded wallet holding `amount` of `quote` (its associated token account of it).
    pub fn quote_wallet(&mut self, quote: &Pubkey, program: &Pubkey, amount: u64) -> Keypair {
        let kp = self.env.funded(10_000_000_000);
        self.env.token_account(&kp.pubkey(), quote, program, amount);
        kp
    }

    /// A buy of `amount` of leg `index`'s quote, delivered to the trader (its holding of the coin
    /// created first).
    pub fn leg_buy(&mut self, trader: &Keypair, mint: &Pubkey, index: usize, amount: u64) -> Tx {
        let who = trader.pubkey();
        let ixs = [
            token::create_holding(who, *mint, who),
            self.leg_swap_ix(&who, &who, mint, index, 1, amount),
        ];
        self.env.send_paid_by(&ixs, trader, &[])
    }

    /// A sell of `amount` of the coin on leg `index`, delivered to the trader's associated token
    /// account of the quote.
    pub fn leg_sell(&mut self, trader: &Keypair, mint: &Pubkey, index: usize, amount: u64) -> Tx {
        let who = trader.pubkey();
        let ix = self.leg_swap_ix(&who, &who, mint, index, 0, amount);
        self.env.send_paid_by(&[ix], trader, &[])
    }
}
