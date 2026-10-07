//! The Bordrless hook protocol (v2), shared by the token standard (`bordrless_token`), the DEX
//! (`bordrless_swap`) and every program that implements a hook.
//!
//! A hook is an ordinary program whose instructions are named after the callbacks
//! (`before_transfer`, `after_swap`, …). The calling program invokes them by CPI with an
//! Anchor-style discriminator ([`discriminators`]) and Borsh arguments ([`TokenHookArgs`],
//! [`PoolHookArgs`]), signing with its `["hook-authority", hook_program]` PDA ([`hook_signer`]),
//! which is always the first account. The signer is one per hook program: a callback receives it
//! as a signer and could pass it on in a CPI of its own, so a hook accepts only the signer made
//! for it, and a signer passed on by another hook vouches for nothing (`docs/hooks-v2.md`,
//! implementation notes, review fixes).
//! A `before_*` callback (and a pool's `after_swap`) may answer with a [`HookReturn`] in its return
//! data: up to [`MAX_DELTAS`] cuts from the amount, each to an account the hook names, a burn and
//! an LP fee (pool swaps), and the 64 bytes of state a token hook keeps in each holding
//! ([`HOOK_DATA_LEN`]). The calling program checks the answer ([`read_answer`], [`Allowed`]) and
//! applies it; a hook never moves funds itself and never gets the user's signature.
//!
//! A hook that needs accounts of its own publishes a [`HookAccountList`] at
//! `["bordrless-hook-accounts", mint-or-pool]` under its own program id; clients resolve the list
//! and append the accounts to the instruction, after the fixed prefix.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::{get_return_data, set_return_data};

/// Seed of the PDA a calling program signs hook CPIs with (`["hook-authority", hook_program]`,
/// [`hook_signer`]), and of a hook program's own authority (`["hook-authority"]`,
/// [`hook_authority`]), with which it signs the token program's `write_hook_data` and proves it
/// creates its own pool.
pub const HOOK_AUTHORITY_SEED: &[u8] = b"hook-authority";
/// Seed of a hook's extra-accounts registry (with the mint or pool as the second seed).
pub const HOOK_ACCOUNTS_SEED: &[u8] = b"bordrless-hook-accounts";
/// First eight bytes of a registry account (`sha256("bordrless:hook-accounts")[..8]`).
pub const HOOK_ACCOUNTS_MAGIC: [u8; 8] = [0x02, 0xda, 0xf6, 0x66, 0x2c, 0x38, 0xc0, 0xcd];
/// Accounts always passed to a token hook before its extras: `hook_signer`, `mint`, `source`,
/// `destination`, `authority` (the mint stands in for the side a mint or burn lacks).
pub const TOKEN_PREFIX_ACCOUNTS: usize = 5;
/// Accounts always passed to a pool hook before its extras: `hook_signer`, `pool`, `base_mint`,
/// `quote_mint`, `actor`.
pub const POOL_PREFIX_ACCOUNTS: usize = 5;
/// Largest hook arguments the protocol forwards (`hook_data` of a swap).
pub const MAX_HOOK_DATA: usize = 256;
/// Most deltas one answer may carry.
pub const MAX_DELTAS: usize = 3;
/// Bytes of state a token hook keeps in every holding (`Holding.hook_data`).
pub const HOOK_DATA_LEN: usize = 64;

/// Token hook flags (`Mint.hook_flags`).
pub mod token_flags {
    /// `before_transfer` runs.
    pub const BEFORE_TRANSFER: u16 = 1 << 0;
    /// `after_transfer` runs.
    pub const AFTER_TRANSFER: u16 = 1 << 1;
    /// `before_mint` runs.
    pub const BEFORE_MINT: u16 = 1 << 2;
    /// `after_mint` runs.
    pub const AFTER_MINT: u16 = 1 << 3;
    /// `before_burn` runs.
    pub const BEFORE_BURN: u16 = 1 << 4;
    /// `after_burn` runs.
    pub const AFTER_BURN: u16 = 1 << 5;
    /// `before_transfer` may answer deltas taken from the transfer (nothing else).
    pub const TRANSFER_RETURNS_DELTA: u16 = 1 << 6;
    /// The `before_*` callbacks may answer the holdings' hook data, and the hook program may write
    /// it with the token program's `write_hook_data`.
    pub const WRITES_HOOK_DATA: u16 = 1 << 7;
    /// Every valid bit.
    pub const ALL: u16 = (1 << 8) - 1;
}

/// Pool hook flags (`Pool.hook_flags`).
pub mod pool_flags {
    /// `before_initialize` runs (unless the hook program itself creates the pool).
    pub const BEFORE_INITIALIZE: u16 = 1 << 0;
    /// `after_initialize` runs.
    pub const AFTER_INITIALIZE: u16 = 1 << 1;
    /// `before_add_liquidity` runs.
    pub const BEFORE_ADD_LIQUIDITY: u16 = 1 << 2;
    /// `after_add_liquidity` runs.
    pub const AFTER_ADD_LIQUIDITY: u16 = 1 << 3;
    /// `before_remove_liquidity` runs.
    pub const BEFORE_REMOVE_LIQUIDITY: u16 = 1 << 4;
    /// `after_remove_liquidity` runs.
    pub const AFTER_REMOVE_LIQUIDITY: u16 = 1 << 5;
    /// `before_swap` runs.
    pub const BEFORE_SWAP: u16 = 1 << 6;
    /// `after_swap` runs: after the input is settled and the output computed, before delivery.
    pub const AFTER_SWAP: u16 = 1 << 7;
    /// `before_swap` may answer deltas and a burn taken from the input.
    pub const BEFORE_SWAP_RETURNS_DELTA: u16 = 1 << 8;
    /// `after_swap` may answer deltas and a burn taken from the output.
    pub const AFTER_SWAP_RETURNS_DELTA: u16 = 1 << 9;
    /// `before_swap` may override the LP fee of the swap.
    pub const BEFORE_SWAP_OVERRIDES_FEE: u16 = 1 << 10;
    /// Every valid bit.
    pub const ALL: u16 = (1 << 11) - 1;
}

/// Instruction discriminators of the callbacks: `sha256("global:<name>")[..8]`, what Anchor gives an
/// instruction of that name.
pub mod discriminators {
    /// `before_transfer`.
    pub const BEFORE_TRANSFER: [u8; 8] = [0x17, 0x76, 0xbd, 0x17, 0xb3, 0xde, 0x5e, 0x92];
    /// `after_transfer`.
    pub const AFTER_TRANSFER: [u8; 8] = [0x99, 0x80, 0xdc, 0x05, 0xd1, 0x5a, 0x27, 0x59];
    /// `before_mint`.
    pub const BEFORE_MINT: [u8; 8] = [0x43, 0x1b, 0x39, 0x07, 0x1c, 0xa8, 0x6d, 0x99];
    /// `after_mint`.
    pub const AFTER_MINT: [u8; 8] = [0x86, 0x72, 0xb6, 0xa1, 0x52, 0x45, 0x0e, 0x9a];
    /// `before_burn`.
    pub const BEFORE_BURN: [u8; 8] = [0x07, 0xb1, 0x13, 0xa0, 0x1c, 0xe5, 0x39, 0x49];
    /// `after_burn`.
    pub const AFTER_BURN: [u8; 8] = [0x06, 0x99, 0x45, 0x78, 0x01, 0xf8, 0x3f, 0x00];
    /// `before_initialize`.
    pub const BEFORE_INITIALIZE: [u8; 8] = [0x22, 0x5c, 0xa3, 0x2c, 0xbb, 0xd5, 0x83, 0x0f];
    /// `after_initialize`.
    pub const AFTER_INITIALIZE: [u8; 8] = [0x82, 0xc2, 0xbc, 0x09, 0x22, 0x06, 0x47, 0x8d];
    /// `before_add_liquidity`.
    pub const BEFORE_ADD_LIQUIDITY: [u8; 8] = [0x72, 0x1b, 0x7b, 0xb5, 0xc8, 0xfc, 0xb2, 0xf0];
    /// `after_add_liquidity`.
    pub const AFTER_ADD_LIQUIDITY: [u8; 8] = [0x7c, 0x18, 0xa8, 0x6e, 0x8f, 0x38, 0x23, 0x0e];
    /// `before_remove_liquidity`.
    pub const BEFORE_REMOVE_LIQUIDITY: [u8; 8] = [0x42, 0x81, 0x8b, 0x3f, 0xc4, 0x54, 0xe7, 0xdd];
    /// `after_remove_liquidity`.
    pub const AFTER_REMOVE_LIQUIDITY: [u8; 8] = [0xee, 0x8a, 0x1c, 0xdc, 0x23, 0xa7, 0x08, 0x2a];
    /// `before_swap`.
    pub const BEFORE_SWAP: [u8; 8] = [0xe3, 0x3b, 0xf0, 0x44, 0xa4, 0x09, 0x1d, 0xfe];
    /// `after_swap`.
    pub const AFTER_SWAP: [u8; 8] = [0xeb, 0xd7, 0xe8, 0xb7, 0x98, 0x6d, 0x05, 0x23];
}

/// What a token hook is being told about.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenOp {
    /// A transfer between holdings.
    Transfer,
    /// A mint into a holding.
    Mint,
    /// A burn from a holding.
    Burn,
}

/// What a pool hook is being told about.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoolOp {
    /// Pool creation.
    Initialize,
    /// Liquidity added.
    AddLiquidity,
    /// Liquidity removed.
    RemoveLiquidity,
    /// A swap.
    Swap,
}

/// Before or after the operation.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Nothing has moved yet; balances are the pre-state.
    Before,
    /// The operation is applied; balances are the post-state.
    After,
}

/// Arguments of a token hook callback. Balances and hook data are those of the holdings before
/// (`Before`) or after (`After`) the operation; `delta` is what the deltas took, known only after.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct TokenHookArgs {
    /// The operation.
    pub op: TokenOp,
    /// Before or after it.
    pub phase: Phase,
    /// The mint.
    pub mint: Pubkey,
    /// Source holding (the mint for a mint).
    pub source: Pubkey,
    /// Destination holding (the mint for a burn).
    pub destination: Pubkey,
    /// Owner of the source holding (default for a mint).
    pub source_owner: Pubkey,
    /// Owner of the destination holding (default for a burn).
    pub destination_owner: Pubkey,
    /// Who signed.
    pub authority: Pubkey,
    /// Whether the signer acted as a delegate rather than the owner.
    pub authority_is_delegate: bool,
    /// The amount of the operation.
    pub amount: u64,
    /// The sum of the deltas a `before_transfer` answer took (zero before).
    pub delta: u64,
    /// Source balance.
    pub source_balance: u64,
    /// Destination balance.
    pub destination_balance: u64,
    /// Decimals of the mint.
    pub decimals: u8,
    /// Supply of the mint.
    pub supply: u64,
    /// The source holding's hook data (zeros for a mint).
    pub source_hook_data: [u8; 64],
    /// The destination holding's hook data (zeros for a burn).
    pub destination_hook_data: [u8; 64],
}

/// Arguments of a pool hook callback.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct PoolHookArgs {
    /// The operation.
    pub op: PoolOp,
    /// Before or after it.
    pub phase: Phase,
    /// The pool.
    pub pool: Pubkey,
    /// Base mint.
    pub base_mint: Pubkey,
    /// Quote mint.
    pub quote_mint: Pubkey,
    /// The trader or liquidity provider.
    pub actor: Pubkey,
    /// Who receives: for a swap the owner of the holding the output is delivered to; for liquidity
    /// the owner of the receiving holding (the LP holding on add, the base holding on remove); the
    /// default key on initialize.
    pub recipient: Pubkey,
    /// Swap direction: 0 base → quote (a sell), 1 quote → base (a buy).
    pub direction: u8,
    /// Swap input (after any before-delta, before fees); liquidity base amount.
    pub amount_in: u64,
    /// Swap output (known after); liquidity quote amount.
    pub amount_out: u64,
    /// Real base reserve.
    pub base_reserve: u64,
    /// Real quote reserve.
    pub quote_reserve: u64,
    /// Virtual base offset.
    pub virtual_base: u64,
    /// Virtual quote offset.
    pub virtual_quote: u64,
    /// The pool's LP fee (after a swap: the fee actually applied).
    pub lp_fee_bps: u16,
    /// The pool's protocol fee.
    pub protocol_fee_bps: u16,
    /// Swaps so far (before this one).
    pub swap_count: u64,
    /// Pool creation time.
    pub created_at: i64,
    /// LP amount of a liquidity operation.
    pub lp_amount: u64,
    /// Opaque data from the caller (v4's `hookData`).
    pub hook_data: Vec<u8>,
}

/// One cut an answer takes from the amount.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Delta {
    /// How much (above zero).
    pub amount: u64,
    /// Index in the callback's account list (prefix first, then extras) of the holding that
    /// receives it. Must be an extra.
    pub account: u8,
}

/// What a callback may answer in its return data. Which fields a callback may fill depends on the
/// callback and the flags ([`Allowed`]); the calling program refuses an answer that fills any
/// other field.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct HookReturn {
    /// Cuts from the amount (the transfer; a swap's input before, its output after), at most
    /// [`MAX_DELTAS`].
    pub deltas: Vec<Delta>,
    /// Pool swap callbacks only: an amount to burn from the same side.
    pub burn: u64,
    /// `before_swap` only: the LP fee to apply instead of the pool's.
    pub lp_fee_bps: Option<u16>,
    /// Token callbacks only (`before_transfer`, `before_burn`): the source holding's new hook data
    /// ([`HOOK_DATA_LEN`] bytes).
    pub source_hook_data: Option<[u8; 64]>,
    /// Token callbacks only (`before_transfer`, `before_mint`): the destination holding's new hook
    /// data.
    pub destination_hook_data: Option<[u8; 64]>,
}

/// Which fields of a [`HookReturn`] one callback may fill. An answer is read only when at least one
/// is allowed; a filled field that is not allowed refuses the whole answer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Allowed {
    /// `deltas`.
    pub deltas: bool,
    /// `burn`.
    pub burn: bool,
    /// `lp_fee_bps`.
    pub lp_fee: bool,
    /// `source_hook_data`.
    pub source_hook_data: bool,
    /// `destination_hook_data`.
    pub destination_hook_data: bool,
}

impl Allowed {
    /// Nothing: the callback's return data is never read.
    pub const NONE: Allowed = Allowed {
        deltas: false,
        burn: false,
        lp_fee: false,
        source_hook_data: false,
        destination_hook_data: false,
    };

    /// Whether the callback may answer anything at all.
    pub fn any(&self) -> bool {
        self.deltas
            || self.burn
            || self.lp_fee
            || self.source_hook_data
            || self.destination_hook_data
    }

    /// A token callback of a mint with `flags`: `before_transfer` may answer deltas
    /// (`TRANSFER_RETURNS_DELTA`) and both sides' hook data (`WRITES_HOOK_DATA`); `before_mint` the
    /// destination's hook data and `before_burn` the source's (`WRITES_HOOK_DATA`); no token
    /// callback answers a burn or an LP fee, and the `After` phase answers nothing.
    pub fn token(op: TokenOp, phase: Phase, flags: u16) -> Self {
        if phase == Phase::After {
            return Self::NONE;
        }
        let data = flags & token_flags::WRITES_HOOK_DATA != 0;
        match op {
            TokenOp::Transfer => Self {
                deltas: flags & token_flags::TRANSFER_RETURNS_DELTA != 0,
                source_hook_data: data,
                destination_hook_data: data,
                ..Self::NONE
            },
            TokenOp::Mint => Self {
                destination_hook_data: data,
                ..Self::NONE
            },
            TokenOp::Burn => Self {
                source_hook_data: data,
                ..Self::NONE
            },
        }
    }

    /// A pool callback of a pool with `flags`: `before_swap` may answer deltas and a burn
    /// (`BEFORE_SWAP_RETURNS_DELTA`) and an LP fee (`BEFORE_SWAP_OVERRIDES_FEE`); `after_swap` deltas
    /// and a burn (`AFTER_SWAP_RETURNS_DELTA`); initialize and liquidity callbacks answer nothing.
    pub fn pool(op: PoolOp, phase: Phase, flags: u16) -> Self {
        match (op, phase) {
            (PoolOp::Swap, Phase::Before) => {
                let delta = flags & pool_flags::BEFORE_SWAP_RETURNS_DELTA != 0;
                Self {
                    deltas: delta,
                    burn: delta,
                    lp_fee: flags & pool_flags::BEFORE_SWAP_OVERRIDES_FEE != 0,
                    ..Self::NONE
                }
            }
            (PoolOp::Swap, Phase::After) => {
                let delta = flags & pool_flags::AFTER_SWAP_RETURNS_DELTA != 0;
                Self {
                    deltas: delta,
                    burn: delta,
                    ..Self::NONE
                }
            }
            _ => Self::NONE,
        }
    }
}

/// Why an answer is refused. Each calling program maps these to its own error codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnswerError {
    /// The return data does not decode as a [`HookReturn`] (or has bytes left over).
    Malformed,
    /// More than [`MAX_DELTAS`] deltas.
    TooManyDeltas,
    /// A delta of zero.
    ZeroDelta,
    /// The same account index named by two deltas.
    DuplicateDeltaAccount,
    /// The deltas and the burn overflow a u64.
    DeltaTooLarge,
    /// A field the callback or the flags do not allow.
    Unsupported,
}

impl HookReturn {
    /// The callers' rules for an answer: no field outside `allowed`, at most [`MAX_DELTAS`]
    /// deltas, each above zero, no account index twice. Answers the sum of the deltas and the
    /// burn, in checked arithmetic (an overflow is [`AnswerError::DeltaTooLarge`]). Whether that sum
    /// fits the amount, and whether each named account is acceptable, is the caller's to check.
    pub fn check(&self, allowed: Allowed) -> core::result::Result<u64, AnswerError> {
        if (!self.deltas.is_empty() && !allowed.deltas)
            || (self.burn != 0 && !allowed.burn)
            || (self.lp_fee_bps.is_some() && !allowed.lp_fee)
            || (self.source_hook_data.is_some() && !allowed.source_hook_data)
            || (self.destination_hook_data.is_some() && !allowed.destination_hook_data)
        {
            return Err(AnswerError::Unsupported);
        }
        if self.deltas.len() > MAX_DELTAS {
            return Err(AnswerError::TooManyDeltas);
        }
        let mut taken = self.burn;
        for (i, delta) in self.deltas.iter().enumerate() {
            if delta.amount == 0 {
                return Err(AnswerError::ZeroDelta);
            }
            if self.deltas[..i].iter().any(|d| d.account == delta.account) {
                return Err(AnswerError::DuplicateDeltaAccount);
            }
            taken = taken
                .checked_add(delta.amount)
                .ok_or(AnswerError::DeltaTooLarge)?;
        }
        Ok(taken)
    }

    /// The sum of the deltas alone (checked).
    pub fn delta_sum(&self) -> core::result::Result<u64, AnswerError> {
        self.deltas.iter().try_fold(0u64, |sum, d| {
            sum.checked_add(d.amount).ok_or(AnswerError::DeltaTooLarge)
        })
    }
}

/// A checked answer and what it takes in total (the deltas plus the burn).
pub type Answer = (HookReturn, u64);

/// Reads the answer of the callback that just returned, when `allowed` lets it answer anything:
/// `Ok(None)` when it may not (its return data is then never read) or when it left no return data
/// of its own; the checked answer otherwise ([`HookReturn::check`]).
pub fn read_answer(
    hook_program: &Pubkey,
    allowed: Allowed,
) -> core::result::Result<Option<Answer>, AnswerError> {
    if !allowed.any() {
        return Ok(None);
    }
    answer_from(get_return_data(), hook_program, allowed)
}

/// [`read_answer`] on given return data (the program that set it, and the bytes).
pub fn answer_from(
    return_data: Option<(Pubkey, Vec<u8>)>,
    hook_program: &Pubkey,
    allowed: Allowed,
) -> core::result::Result<Option<Answer>, AnswerError> {
    if !allowed.any() {
        return Ok(None);
    }
    match return_data {
        Some((program, data)) if program == *hook_program && !data.is_empty() => {
            let answer = HookReturn::try_from_slice(&data).map_err(|_| AnswerError::Malformed)?;
            let taken = answer.check(allowed)?;
            Ok(Some((answer, taken)))
        }
        _ => Ok(None),
    }
}

/// Clears the return data before a hook CPI, so stale data can never be mistaken for an answer.
pub fn clear_return_data() {
    set_return_data(&[]);
}

/// The `["hook-authority"]` PDA of `program_id`: a hook program's own authority.
pub fn hook_authority(program_id: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[HOOK_AUTHORITY_SEED], program_id)
}

/// The PDA `caller` (the token program, the DEX) signs every callback to `hook_program` with:
/// `["hook-authority", hook_program]` under `caller`, and its bump. A hook compares the signer
/// of a callback with this (a constant for it), so a signer another hook received and passed on
/// is refused.
pub fn hook_signer(caller: &Pubkey, hook_program: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[HOOK_AUTHORITY_SEED, hook_program.as_ref()], caller)
}

/// [`hook_signer`] at a known `bump` (no search): `None` when the seeds give no PDA.
pub fn hook_signer_at(caller: &Pubkey, hook_program: &Pubkey, bump: u8) -> Option<Pubkey> {
    Pubkey::create_program_address(
        &[HOOK_AUTHORITY_SEED, hook_program.as_ref(), &[bump]],
        caller,
    )
    .ok()
}

/// The registry address of `hook_program` for `key` (a mint or a pool).
pub fn hook_accounts_address(hook_program: &Pubkey, key: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[HOOK_ACCOUNTS_SEED, key.as_ref()], hook_program)
}

/// A seed of a PDA in the registry.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, PartialEq, Eq)]
pub enum Seed {
    /// Literal bytes.
    Literal(Vec<u8>),
    /// The key of an account already in the callback's list: the prefix accounts first, then the
    /// extras in registry order.
    Account(u8),
    /// The source owner of a token operation.
    SourceOwner,
    /// The destination owner of a token operation.
    DestinationOwner,
}

/// Where an extra account comes from.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, PartialEq, Eq)]
pub enum AccountSource {
    /// A fixed key.
    Key(Pubkey),
    /// A PDA of `program` with `seeds`.
    Pda {
        /// The program the PDA belongs to.
        program: Pubkey,
        /// Its seeds.
        seeds: Vec<Seed>,
    },
}

/// One extra account.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct ExtraAccount {
    /// Passed writable.
    pub writable: bool,
    /// How to find it.
    pub source: AccountSource,
}

/// The extra accounts a hook needs, in order.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct HookAccountList {
    /// Layout version (1).
    pub version: u8,
    /// The accounts.
    pub accounts: Vec<ExtraAccount>,
}

impl HookAccountList {
    /// Current layout version.
    pub const VERSION: u8 = 1;

    /// A list of `accounts`.
    pub fn new(accounts: Vec<ExtraAccount>) -> Self {
        Self {
            version: Self::VERSION,
            accounts,
        }
    }

    /// The registry account's bytes: the magic, then Borsh.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + 64);
        out.extend_from_slice(&HOOK_ACCOUNTS_MAGIC);
        self.serialize(&mut out).expect("vec write");
        out
    }

    /// Decodes a registry account's bytes.
    pub fn decode(data: &[u8]) -> Option<Self> {
        if data.len() < 8 || data[..8] != HOOK_ACCOUNTS_MAGIC {
            return None;
        }
        Self::try_from_slice(&data[8..]).ok()
    }

    /// Resolves the list against the prefix keys of a callback (and, for token hooks, the owners),
    /// in the order and with the writability the calling program must pass them. `None` when a seed
    /// refers to an account that is not yet resolved.
    pub fn resolve(
        &self,
        prefix: &[Pubkey],
        source_owner: &Pubkey,
        destination_owner: &Pubkey,
    ) -> Option<Vec<AccountMeta>> {
        let mut keys: Vec<Pubkey> = prefix.to_vec();
        let mut metas = Vec::with_capacity(self.accounts.len());
        for extra in &self.accounts {
            let key = match &extra.source {
                AccountSource::Key(key) => *key,
                AccountSource::Pda { program, seeds } => {
                    let mut bytes: Vec<Vec<u8>> = Vec::with_capacity(seeds.len());
                    for seed in seeds {
                        bytes.push(match seed {
                            Seed::Literal(b) => b.clone(),
                            Seed::Account(i) => keys.get(usize::from(*i))?.to_bytes().to_vec(),
                            Seed::SourceOwner => source_owner.to_bytes().to_vec(),
                            Seed::DestinationOwner => destination_owner.to_bytes().to_vec(),
                        });
                    }
                    let refs: Vec<&[u8]> = bytes.iter().map(Vec::as_slice).collect();
                    Pubkey::find_program_address(&refs, program).0
                }
            };
            keys.push(key);
            metas.push(if extra.writable {
                AccountMeta::new(key, false)
            } else {
                AccountMeta::new_readonly(key, false)
            });
        }
        Some(metas)
    }
}

/// Creates the registry account of `hook_program` for `key` (a PDA the hook signs for) and writes
/// `list` into it; `payer` (a signer) funds the rent. An address someone has already funded is
/// taken as Anchor's `init` takes it: topped up to the rent-exempt minimum, allocated and assigned
/// (a plain create would fail on it). Idempotent: an existing registry is rewritten when it has
/// room, else it fails.
pub fn write_registry<'info>(
    payer: &AccountInfo<'info>,
    registry: &AccountInfo<'info>,
    system_program: &AccountInfo<'info>,
    hook_program: &Pubkey,
    key: &Pubkey,
    bump: u8,
    list: &HookAccountList,
) -> Result<()> {
    use anchor_lang::system_program as system;

    let bytes = list.encode();
    let bump_seed = [bump];
    let seeds: &[&[u8]] = &[HOOK_ACCOUNTS_SEED, key.as_ref(), &bump_seed];
    let expected = Pubkey::create_program_address(seeds, hook_program)
        .map_err(|_| ProgramError::InvalidSeeds)?;
    require_keys_eq!(
        *registry.key,
        expected,
        anchor_lang::error::ErrorCode::ConstraintSeeds
    );
    require_keys_eq!(
        *system_program.key,
        system::ID,
        anchor_lang::error::ErrorCode::InvalidProgramId
    );
    if *registry.owner == system::ID {
        // Not created yet: a fresh address, or one someone already sent lamports to.
        let space = bytes.len();
        let rent = Rent::get()?.minimum_balance(space);
        let current = registry.lamports();
        if current == 0 {
            system::create_account(
                CpiContext::new_with_signer(
                    system_program.key(),
                    system::CreateAccount {
                        from: payer.clone(),
                        to: registry.clone(),
                    },
                    &[seeds],
                ),
                rent,
                space as u64,
                hook_program,
            )?;
        } else {
            let top_up = rent.max(1).saturating_sub(current);
            if top_up > 0 {
                system::transfer(
                    CpiContext::new(
                        system_program.key(),
                        system::Transfer {
                            from: payer.clone(),
                            to: registry.clone(),
                        },
                    ),
                    top_up,
                )?;
            }
            system::allocate(
                CpiContext::new_with_signer(
                    system_program.key(),
                    system::Allocate {
                        account_to_allocate: registry.clone(),
                    },
                    &[seeds],
                ),
                space as u64,
            )?;
            system::assign(
                CpiContext::new_with_signer(
                    system_program.key(),
                    system::Assign {
                        account_to_assign: registry.clone(),
                    },
                    &[seeds],
                ),
                hook_program,
            )?;
        }
    } else {
        require_keys_eq!(
            *registry.owner,
            *hook_program,
            anchor_lang::error::ErrorCode::ConstraintOwner
        );
        require!(
            registry.data_len() >= bytes.len(),
            anchor_lang::error::ErrorCode::ConstraintSpace
        );
    }
    let mut data = registry.try_borrow_mut_data()?;
    data[..bytes.len()].copy_from_slice(&bytes);
    for b in data[bytes.len()..].iter_mut() {
        *b = 0;
    }
    Ok(())
}

// The types above (and `Holding.hook_data`) spell the length as the literal 64, which is what an
// IDL can carry; this keeps the constant honest.
const _: () = assert!(HOOK_DATA_LEN == 64);

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(amount: u64, account: u8) -> Delta {
        Delta { amount, account }
    }

    fn all() -> Allowed {
        Allowed {
            deltas: true,
            burn: true,
            lp_fee: true,
            source_hook_data: true,
            destination_hook_data: true,
        }
    }

    #[test]
    fn registry_round_trips_and_resolves() {
        let hook = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let list = HookAccountList::new(vec![
            ExtraAccount {
                writable: true,
                source: AccountSource::Pda {
                    program: hook,
                    seeds: vec![Seed::Literal(b"tax".to_vec()), Seed::Account(1)],
                },
            },
            ExtraAccount {
                writable: false,
                source: AccountSource::Pda {
                    program: hook,
                    seeds: vec![
                        Seed::Literal(b"wallet".to_vec()),
                        Seed::DestinationOwner,
                        Seed::Account(5),
                    ],
                },
            },
            ExtraAccount {
                writable: true,
                source: AccountSource::Key(Pubkey::new_unique()),
            },
        ]);
        let bytes = list.encode();
        assert_eq!(HookAccountList::decode(&bytes), Some(list.clone()));
        assert_eq!(HookAccountList::decode(&bytes[1..]), None);
        let prefix = [
            Pubkey::new_unique(),
            mint,
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        ];
        let owner = Pubkey::new_unique();
        let metas = list
            .resolve(&prefix, &Pubkey::new_unique(), &owner)
            .unwrap();
        assert_eq!(metas.len(), 3);
        let first = Pubkey::find_program_address(&[b"tax", mint.as_ref()], &hook).0;
        assert_eq!(metas[0], AccountMeta::new(first, false));
        let second =
            Pubkey::find_program_address(&[b"wallet", owner.as_ref(), first.as_ref()], &hook).0;
        assert_eq!(metas[1], AccountMeta::new_readonly(second, false));
        assert!(metas[2].is_writable);
        // A seed that points past the resolved accounts fails rather than resolving to nonsense.
        let bad = HookAccountList::new(vec![ExtraAccount {
            writable: false,
            source: AccountSource::Pda {
                program: hook,
                seeds: vec![Seed::Account(9)],
            },
        }]);
        assert!(bad.resolve(&prefix, &owner, &owner).is_none());
    }

    #[test]
    fn flags_are_stable() {
        assert_eq!(token_flags::WRITES_HOOK_DATA, 128);
        assert_eq!(token_flags::ALL, 255);
        let every = token_flags::BEFORE_TRANSFER
            | token_flags::AFTER_TRANSFER
            | token_flags::BEFORE_MINT
            | token_flags::AFTER_MINT
            | token_flags::BEFORE_BURN
            | token_flags::AFTER_BURN
            | token_flags::TRANSFER_RETURNS_DELTA
            | token_flags::WRITES_HOOK_DATA;
        assert_eq!(every, token_flags::ALL);
        assert_eq!(pool_flags::ALL, 0x7ff);
    }

    #[test]
    fn answers_round_trip_and_stay_small() {
        let full = HookReturn {
            deltas: vec![delta(u64::MAX, 5), delta(2, 6), delta(3, 7)],
            burn: u64::MAX,
            lp_fee_bps: Some(9_000),
            source_hook_data: Some([0xab; HOOK_DATA_LEN]),
            destination_hook_data: Some([0xcd; HOOK_DATA_LEN]),
        };
        let mut bytes = Vec::new();
        full.serialize(&mut bytes).unwrap();
        // 4 + 3 * 9 + 8 + 3 + 2 * 65: well under the 1,024-byte return-data limit.
        assert_eq!(bytes.len(), 172);
        assert!(bytes.len() <= 200);
        assert_eq!(HookReturn::try_from_slice(&bytes).unwrap(), full);
        let mut empty = Vec::new();
        HookReturn::default().serialize(&mut empty).unwrap();
        assert_eq!(empty, vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn token_callbacks_allow_by_flags() {
        use token_flags::*;
        let none = Allowed::NONE;
        assert_eq!(
            Allowed::token(TokenOp::Transfer, Phase::Before, BEFORE_TRANSFER),
            none
        );
        assert_eq!(
            Allowed::token(TokenOp::Transfer, Phase::Before, TRANSFER_RETURNS_DELTA),
            Allowed {
                deltas: true,
                ..none
            }
        );
        assert_eq!(
            Allowed::token(TokenOp::Transfer, Phase::Before, WRITES_HOOK_DATA),
            Allowed {
                source_hook_data: true,
                destination_hook_data: true,
                ..none
            }
        );
        assert_eq!(
            Allowed::token(TokenOp::Mint, Phase::Before, ALL),
            Allowed {
                destination_hook_data: true,
                ..none
            }
        );
        assert_eq!(
            Allowed::token(TokenOp::Burn, Phase::Before, ALL),
            Allowed {
                source_hook_data: true,
                ..none
            }
        );
        assert_eq!(
            Allowed::token(TokenOp::Mint, Phase::Before, ALL & !WRITES_HOOK_DATA),
            none
        );
        for op in [TokenOp::Transfer, TokenOp::Mint, TokenOp::Burn] {
            assert_eq!(Allowed::token(op, Phase::After, ALL), none);
            // No token callback ever answers a burn or an LP fee.
            let a = Allowed::token(op, Phase::Before, ALL);
            assert!(!a.burn && !a.lp_fee);
        }
        assert!(!none.any());
    }

    #[test]
    fn pool_callbacks_allow_by_flags() {
        use pool_flags::*;
        let none = Allowed::NONE;
        assert_eq!(
            Allowed::pool(PoolOp::Swap, Phase::Before, BEFORE_SWAP),
            none
        );
        assert_eq!(
            Allowed::pool(PoolOp::Swap, Phase::Before, BEFORE_SWAP_RETURNS_DELTA),
            Allowed {
                deltas: true,
                burn: true,
                ..none
            }
        );
        assert_eq!(
            Allowed::pool(PoolOp::Swap, Phase::Before, BEFORE_SWAP_OVERRIDES_FEE),
            Allowed {
                lp_fee: true,
                ..none
            }
        );
        assert_eq!(
            Allowed::pool(PoolOp::Swap, Phase::After, ALL),
            Allowed {
                deltas: true,
                burn: true,
                ..none
            }
        );
        assert_eq!(
            Allowed::pool(PoolOp::Swap, Phase::After, ALL & !AFTER_SWAP_RETURNS_DELTA),
            none
        );
        for op in [
            PoolOp::Initialize,
            PoolOp::AddLiquidity,
            PoolOp::RemoveLiquidity,
        ] {
            assert_eq!(Allowed::pool(op, Phase::Before, ALL), none);
            assert_eq!(Allowed::pool(op, Phase::After, ALL), none);
        }
        // No pool callback ever answers hook data.
        let a = Allowed::pool(PoolOp::Swap, Phase::Before, ALL);
        assert!(!a.source_hook_data && !a.destination_hook_data);
    }

    #[test]
    fn checks_every_rule() {
        let ok = HookReturn {
            deltas: vec![delta(1, 5), delta(2, 6), delta(3, 7)],
            burn: 4,
            ..Default::default()
        };
        assert_eq!(ok.check(all()), Ok(10));
        assert_eq!(ok.delta_sum(), Ok(6));
        assert_eq!(HookReturn::default().check(Allowed::NONE), Ok(0));
        // At most three deltas.
        let four = HookReturn {
            deltas: vec![delta(1, 5), delta(1, 6), delta(1, 7), delta(1, 8)],
            ..Default::default()
        };
        assert_eq!(four.check(all()), Err(AnswerError::TooManyDeltas));
        // Each above zero.
        let zero = HookReturn {
            deltas: vec![delta(1, 5), delta(0, 6)],
            ..Default::default()
        };
        assert_eq!(zero.check(all()), Err(AnswerError::ZeroDelta));
        // No account twice.
        let twice = HookReturn {
            deltas: vec![delta(1, 5), delta(2, 6), delta(3, 5)],
            ..Default::default()
        };
        assert_eq!(twice.check(all()), Err(AnswerError::DuplicateDeltaAccount));
        // Sums are checked: an overflow is DeltaTooLarge, with or without the burn.
        let over = HookReturn {
            deltas: vec![delta(u64::MAX, 5), delta(1, 6)],
            ..Default::default()
        };
        assert_eq!(over.check(all()), Err(AnswerError::DeltaTooLarge));
        assert_eq!(over.delta_sum(), Err(AnswerError::DeltaTooLarge));
        let over_burn = HookReturn {
            deltas: vec![delta(u64::MAX, 5)],
            burn: 1,
            ..Default::default()
        };
        assert_eq!(over_burn.check(all()), Err(AnswerError::DeltaTooLarge));
        // Each field outside what the callback allows refuses the answer.
        let fields = [
            (
                HookReturn {
                    deltas: vec![delta(1, 5)],
                    ..Default::default()
                },
                Allowed {
                    deltas: false,
                    ..all()
                },
            ),
            (
                HookReturn {
                    burn: 1,
                    ..Default::default()
                },
                Allowed {
                    burn: false,
                    ..all()
                },
            ),
            (
                HookReturn {
                    lp_fee_bps: Some(0),
                    ..Default::default()
                },
                Allowed {
                    lp_fee: false,
                    ..all()
                },
            ),
            (
                HookReturn {
                    source_hook_data: Some([0; HOOK_DATA_LEN]),
                    ..Default::default()
                },
                Allowed {
                    source_hook_data: false,
                    ..all()
                },
            ),
            (
                HookReturn {
                    destination_hook_data: Some([0; HOOK_DATA_LEN]),
                    ..Default::default()
                },
                Allowed {
                    destination_hook_data: false,
                    ..all()
                },
            ),
        ];
        for (answer, allowed) in fields {
            assert_eq!(
                answer.check(allowed),
                Err(AnswerError::Unsupported),
                "{answer:?}"
            );
            assert!(answer.check(all()).is_ok());
        }
        // A token transfer callback with every flag: a burn is still refused.
        let burn = HookReturn {
            burn: 1,
            ..Default::default()
        };
        assert_eq!(
            burn.check(Allowed::token(
                TokenOp::Transfer,
                Phase::Before,
                token_flags::ALL
            )),
            Err(AnswerError::Unsupported)
        );
    }

    #[test]
    fn reads_only_the_hooks_own_answer_when_allowed() {
        let hook = Pubkey::new_unique();
        let answer = HookReturn {
            deltas: vec![delta(7, 5)],
            ..Default::default()
        };
        let mut bytes = Vec::new();
        answer.serialize(&mut bytes).unwrap();
        let allowed = Allowed {
            deltas: true,
            ..Allowed::NONE
        };
        assert_eq!(
            answer_from(Some((hook, bytes.clone())), &hook, allowed),
            Ok(Some((answer.clone(), 7)))
        );
        // Never read when nothing is allowed, even if malformed.
        assert_eq!(
            answer_from(Some((hook, vec![1, 2, 3])), &hook, Allowed::NONE),
            Ok(None)
        );
        // Another program's return data, or none, is no answer.
        assert_eq!(
            answer_from(Some((Pubkey::new_unique(), bytes.clone())), &hook, allowed),
            Ok(None)
        );
        assert_eq!(answer_from(Some((hook, vec![])), &hook, allowed), Ok(None));
        assert_eq!(answer_from(None, &hook, allowed), Ok(None));
        // Garbage, a truncated answer or bytes left over are malformed.
        assert_eq!(
            answer_from(Some((hook, vec![9; 3])), &hook, allowed),
            Err(AnswerError::Malformed)
        );
        assert_eq!(
            answer_from(
                Some((hook, bytes[..bytes.len() - 1].to_vec())),
                &hook,
                allowed
            ),
            Err(AnswerError::Malformed)
        );
        let mut longer = bytes.clone();
        longer.push(0);
        assert_eq!(
            answer_from(Some((hook, longer)), &hook, allowed),
            Err(AnswerError::Malformed)
        );
        // A read answer is checked.
        assert_eq!(
            answer_from(
                Some((hook, bytes)),
                &hook,
                Allowed {
                    source_hook_data: true,
                    ..Allowed::NONE
                }
            ),
            Err(AnswerError::Unsupported)
        );
    }

    #[test]
    fn hook_signers_are_one_per_hook_program() {
        let caller = Pubkey::new_unique();
        let (a, b) = (Pubkey::new_unique(), Pubkey::new_unique());
        let (signer_a, bump_a) = hook_signer(&caller, &a);
        assert_eq!(hook_signer_at(&caller, &a, bump_a), Some(signer_a));
        // Another hook program, another signer; and neither is the caller's own authority nor
        // the hook's.
        assert_ne!(hook_signer(&caller, &b).0, signer_a);
        assert_ne!(hook_authority(&caller).0, signer_a);
        assert_ne!(hook_authority(&a).0, signer_a);
        // Under another caller, another signer.
        assert_ne!(hook_signer(&Pubkey::new_unique(), &a).0, signer_a);
    }

    #[test]
    fn token_args_carry_hook_data() {
        let args = TokenHookArgs {
            op: TokenOp::Transfer,
            phase: Phase::Before,
            mint: Pubkey::new_unique(),
            source: Pubkey::new_unique(),
            destination: Pubkey::new_unique(),
            source_owner: Pubkey::new_unique(),
            destination_owner: Pubkey::new_unique(),
            authority: Pubkey::new_unique(),
            authority_is_delegate: false,
            amount: 1,
            delta: 0,
            source_balance: 2,
            destination_balance: 3,
            decimals: 6,
            supply: 4,
            source_hook_data: [1; HOOK_DATA_LEN],
            destination_hook_data: [2; HOOK_DATA_LEN],
        };
        let mut bytes = Vec::new();
        args.serialize(&mut bytes).unwrap();
        assert_eq!(bytes.len(), 364);
        assert_eq!(TokenHookArgs::try_from_slice(&bytes).unwrap(), args);
    }
}
