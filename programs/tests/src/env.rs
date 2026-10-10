//! The LiteSVM environment: the programs (the six deployed ones and the test-only `hook_tester`),
//! their upgrade authority, a clock, transactions and account reads.

use std::sync::OnceLock;

use anchor_lang::prelude::{Clock, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::AccountDeserialize;
use litesvm::{
    types::{FailedTransactionMetadata, TransactionMetadata},
    LiteSVM,
};
use solana_account::Account;
use solana_keypair::Keypair;
use solana_message::{v0, AddressLookupTableAccount, VersionedMessage};
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;
use solana_transaction::{InstructionError, TransactionError};

use crate::program_bytes;

/// Unix time of the first block.
pub const T0: i64 = 1_800_000_000;
/// Slot of the first block.
pub const SLOT0: u64 = 400_000_000;
/// The upgradeable loader.
pub const LOADER_V3: Pubkey = Pubkey::from_str_const("BPFLoaderUpgradeab1e11111111111111111111111");
/// The system program.
pub const SYSTEM_PROGRAM_ID: Pubkey = Pubkey::from_str_const("11111111111111111111111111111111");
/// The compute budget program.
pub const COMPUTE_BUDGET_ID: Pubkey =
    Pubkey::from_str_const("ComputeBudget111111111111111111111111111111");
/// The address lookup table program.
pub const ADDRESS_LOOKUP_TABLE_ID: Pubkey =
    Pubkey::from_str_const("AddressLookupTab1e1111111111111111111111111");
/// The associated token account program.
pub const ASSOCIATED_TOKEN_ID: Pubkey =
    Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
/// The serialized size of a lookup table's metadata, before its addresses.
pub const LOOKUP_TABLE_META_SIZE: usize = 56;

/// `SetComputeUnitLimit(units)`.
pub fn compute_unit_limit(units: u32) -> Instruction {
    let mut data = vec![2u8];
    data.extend_from_slice(&units.to_le_bytes());
    Instruction {
        program_id: COMPUTE_BUDGET_ID,
        accounts: vec![],
        data,
    }
}

/// `SetComputeUnitPrice(micro_lamports)`.
pub fn compute_unit_price(micro_lamports: u64) -> Instruction {
    let mut data = vec![3u8];
    data.extend_from_slice(&micro_lamports.to_le_bytes());
    Instruction {
        program_id: COMPUTE_BUDGET_ID,
        accounts: vec![],
        data,
    }
}

fn bytes_of(name: &'static str) -> &'static [u8] {
    static CACHE: OnceLock<
        std::sync::Mutex<std::collections::HashMap<&'static str, &'static [u8]>>,
    > = OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let mut map = cache.lock().expect("cache");
    if let Some(bytes) = map.get(name) {
        return bytes;
    }
    let leaked: &'static [u8] = Box::leak(program_bytes(name).into_boxed_slice());
    map.insert(name, leaked);
    leaked
}

/// The result of one transaction.
#[derive(Debug)]
pub struct Tx {
    /// LiteSVM's metadata.
    pub result: Result<TransactionMetadata, FailedTransactionMetadata>,
    /// Serialized size on the wire.
    pub size: usize,
    /// The keys the message's instructions index into: the static keys, then the addresses
    /// loaded from lookup tables (writable, then read-only).
    pub keys: Vec<Pubkey>,
}

impl Tx {
    /// Compute units consumed.
    pub fn cu(&self) -> u64 {
        match &self.result {
            Ok(m) => m.compute_units_consumed,
            Err(f) => f.meta.compute_units_consumed,
        }
    }

    /// Logs.
    pub fn logs(&self) -> &[String] {
        match &self.result {
            Ok(m) => &m.logs,
            Err(f) => &f.meta.logs,
        }
    }

    /// The metadata of a successful transaction.
    #[track_caller]
    pub fn ok(&self) -> &TransactionMetadata {
        match &self.result {
            Ok(m) => m,
            Err(f) => panic!(
                "transaction failed: {:?}\n{}",
                f.err,
                f.meta.logs.join("\n")
            ),
        }
    }

    /// The error of a failed transaction.
    #[track_caller]
    pub fn err(&self) -> &TransactionError {
        match &self.result {
            Ok(m) => panic!("transaction succeeded: {}", m.logs.join("\n")),
            Err(f) => &f.err,
        }
    }

    /// The custom error code of a failed transaction.
    #[track_caller]
    pub fn custom(&self) -> u32 {
        match self.err() {
            TransactionError::InstructionError(_, InstructionError::Custom(c)) => *c,
            other => panic!("not a custom error: {other:?}\n{}", self.logs().join("\n")),
        }
    }

    /// Asserts a custom error code.
    #[track_caller]
    pub fn expect_code(&self, code: u32) {
        let got = self.custom();
        assert_eq!(
            got,
            code,
            "expected error {code}, got {got}\n{}",
            self.logs().join("\n")
        );
    }

    /// Asserts any failure.
    #[track_caller]
    pub fn expect_fail(&self) {
        let _ = self.err();
    }

    /// Return data of a successful transaction.
    pub fn return_data(&self) -> Vec<u8> {
        self.ok().return_data.data.clone()
    }

    /// The deepest invocation of a successful transaction: 1 for its own instructions, 2 for a
    /// CPI they make, and so on (the runtime allows 5).
    #[track_caller]
    pub fn max_height(&self) -> u8 {
        self.ok()
            .inner_instructions
            .iter()
            .flatten()
            .map(|inner| inner.stack_height)
            .max()
            .unwrap_or(1)
    }

    /// The instruction trace of a successful transaction: its own instructions and every CPI
    /// they make (the runtime allows 64).
    #[track_caller]
    pub fn trace_len(&self) -> usize {
        let inner = &self.ok().inner_instructions;
        inner.len() + inner.iter().map(Vec::len).sum::<usize>()
    }

    /// The inner instructions (every CPI) of a successful transaction.
    #[track_caller]
    pub fn inner_len(&self) -> usize {
        self.ok().inner_instructions.iter().map(Vec::len).sum()
    }
}

/// The environment.
pub struct Env {
    /// The SVM.
    pub svm: LiteSVM,
    /// Fee payer of every transaction.
    pub payer: Keypair,
    /// Current unix time.
    pub now: i64,
    /// Current slot.
    pub slot: u64,
    /// Upgrade authority of every program; the admin of every config.
    pub deployer: Keypair,
    /// Receives fees.
    pub treasury: Keypair,
}

impl Default for Env {
    fn default() -> Self {
        Self::new()
    }
}

impl Env {
    /// A fresh SVM with the programs loaded (upgrade authority [`Env::deployer`]) and the clock at
    /// [`T0`]: the deployed ones and the test-only `hook_tester`.
    pub fn new() -> Self {
        let mut svm = LiteSVM::new();
        let programs = [
            ("bordrless_token", bordrless_token::ID),
            ("bordrless_swap", bordrless_swap::ID),
            ("bordrless_bridge", bordrless_bridge::ID),
            ("bordrless_launch", bordrless_launch::ID),
            ("bordrless_kit", bordrless_kit::ID),
            ("tax_hook", tax_hook::ID),
            ("half_life", half_life::ID),
            ("lottery_hook", lottery_hook::ID),
            ("hook_tester", hook_tester::ID),
            ("bordrless_companion", bordrless_companion::ID),
            ("hook_timelock", hook_timelock::ID),
            ("hook_vault", hook_vault::ID),
        ];
        for (name, id) in programs {
            svm.add_program(id, bytes_of(name))
                .unwrap_or_else(|e| panic!("load {name}: {e:?}"));
        }
        let payer = Keypair::new();
        svm.airdrop(&payer.pubkey(), 1_000_000_000_000)
            .expect("airdrop");
        let mut env = Self {
            svm,
            payer,
            now: T0,
            slot: SLOT0,
            deployer: Keypair::new(),
            treasury: Keypair::new(),
        };
        env.fund(env.deployer.pubkey(), 100_000_000_000);
        env.fund(env.treasury.pubkey(), 10_000_000);
        // The protocol's programs are upgradeable by the deployer; the hooks as on mainnet: by the
        // protocol's own upgrade authority, one of those `create_config` allows for a custom hook.
        let hooks = [
            tax_hook::ID,
            half_life::ID,
            lottery_hook::ID,
            hook_tester::ID,
        ];
        for (_, id) in programs {
            let authority = if hooks.contains(&id) {
                bordrless_launch::constants::HOOK_UPGRADE_AUTHORITIES[1]
            } else {
                env.deployer.pubkey()
            };
            env.set_upgrade_authority(id, Some(authority));
        }
        env.sync_clock();
        env
    }

    // ------------------------------------------------------------------------------ clock

    /// Writes the clock.
    pub fn sync_clock(&mut self) {
        let mut clock: Clock = self.svm.get_sysvar();
        clock.unix_timestamp = self.now;
        clock.slot = self.slot;
        self.svm.set_sysvar(&clock);
    }

    /// Moves the clock forward by `secs` (and the slot by one).
    pub fn warp(&mut self, secs: i64) {
        self.now += secs;
        self.slot += 1;
        self.sync_clock();
    }

    // ------------------------------------------------------------------------------ transactions

    /// Sends `ixs` in one v0 transaction paid by [`Env::payer`] and signed by `signers`, with a
    /// 1.4M compute budget in front.
    pub fn send(&mut self, ixs: &[Instruction], signers: &[&Keypair]) -> Tx {
        let payer = self.payer.insecure_clone();
        self.send_paid_by(ixs, &payer, signers)
    }

    /// Sends `ixs` exactly as given (no compute budget instruction), for CU measurement.
    pub fn send_bare(&mut self, ixs: &[Instruction], signers: &[&Keypair]) -> Tx {
        let payer = self.payer.insecure_clone();
        self.send_full(ixs, &payer, signers, false, &[])
    }

    /// Sends `ixs` paid by `fee_payer`.
    pub fn send_paid_by(
        &mut self,
        ixs: &[Instruction],
        fee_payer: &Keypair,
        signers: &[&Keypair],
    ) -> Tx {
        self.send_full(ixs, fee_payer, signers, true, &[])
    }

    /// Sends `ixs` exactly as given (no compute budget instruction added) as a v0 transaction that
    /// loads from `tables` every account it can (an account a top-level instruction invokes, or
    /// that signs, stays a static key), paid by `fee_payer`.
    pub fn send_v0(
        &mut self,
        ixs: &[Instruction],
        fee_payer: &Keypair,
        signers: &[&Keypair],
        tables: &[AddressLookupTableAccount],
    ) -> Tx {
        self.send_full(ixs, fee_payer, signers, false, tables)
    }

    /// The serialized size of a v0 transaction of `ixs` with `tables`, signed, without sending it.
    pub fn v0_size(
        &self,
        ixs: &[Instruction],
        fee_payer: &Keypair,
        signers: &[&Keypair],
        tables: &[AddressLookupTableAccount],
    ) -> usize {
        let message = v0::Message::try_compile(
            &fee_payer.pubkey(),
            ixs,
            tables,
            self.svm.latest_blockhash(),
        )
        .expect("compile v0 message");
        let mut keypairs: Vec<&Keypair> = vec![fee_payer];
        for s in signers {
            if !keypairs.iter().any(|k| k.pubkey() == s.pubkey()) {
                keypairs.push(s);
            }
        }
        let tx =
            VersionedTransaction::try_new(VersionedMessage::V0(message), &keypairs).expect("sign");
        wire_size(&tx)
    }

    fn send_full(
        &mut self,
        ixs: &[Instruction],
        fee_payer: &Keypair,
        signers: &[&Keypair],
        budget: bool,
        tables: &[AddressLookupTableAccount],
    ) -> Tx {
        let mut all = Vec::with_capacity(ixs.len() + 1);
        if budget {
            all.push(compute_unit_limit(1_400_000));
        }
        all.extend_from_slice(ixs);
        let blockhash = self.svm.latest_blockhash();
        let message = v0::Message::try_compile(&fee_payer.pubkey(), &all, tables, blockhash)
            .expect("compile v0 message");
        // The keys instructions index into: the static keys, then every table's writable loads,
        // then every table's read-only loads.
        let mut keys = message.account_keys.clone();
        let mut readonly = Vec::new();
        for lookup in &message.address_table_lookups {
            let table = tables
                .iter()
                .find(|t| t.key == lookup.account_key)
                .expect("a table the message names");
            keys.extend(
                lookup
                    .writable_indexes
                    .iter()
                    .map(|i| table.addresses[usize::from(*i)]),
            );
            readonly.extend(
                lookup
                    .readonly_indexes
                    .iter()
                    .map(|i| table.addresses[usize::from(*i)]),
            );
        }
        keys.extend(readonly);
        let message = VersionedMessage::V0(message);
        let mut keypairs: Vec<&Keypair> = vec![fee_payer];
        for s in signers {
            if !keypairs.iter().any(|k| k.pubkey() == s.pubkey()) {
                keypairs.push(s);
            }
        }
        let tx = VersionedTransaction::try_new(message, &keypairs).expect("sign");
        let size = wire_size(&tx);
        let result = self.svm.send_transaction(tx);
        self.svm.expire_blockhash();
        Tx { result, size, keys }
    }

    /// Writes an active address lookup table at `key` holding `addresses`, as the lookup-table
    /// program leaves one extended in an earlier slot, and answers it for compiling messages.
    pub fn put_lookup_table(
        &mut self,
        key: Pubkey,
        addresses: &[Pubkey],
    ) -> AddressLookupTableAccount {
        assert!(addresses.len() <= 256, "a table holds 256 addresses");
        // ProgramState::LookupTable(LookupTableMeta { deactivation_slot: u64::MAX (active),
        // last_extended_slot: 0, last_extended_slot_start_index: 0, authority: None, padding }),
        // zero-padded to 56 bytes, then the addresses.
        let mut data = vec![0u8; LOOKUP_TABLE_META_SIZE];
        data[0..4].copy_from_slice(&1u32.to_le_bytes());
        data[4..12].copy_from_slice(&u64::MAX.to_le_bytes());
        for address in addresses {
            data.extend_from_slice(address.as_ref());
        }
        let lamports = self.rent(data.len());
        self.put(
            key,
            Account {
                lamports,
                data,
                owner: ADDRESS_LOOKUP_TABLE_ID,
                executable: false,
                rent_epoch: 0,
            },
        );
        AddressLookupTableAccount {
            key,
            addresses: addresses.to_vec(),
        }
    }

    // ------------------------------------------------------------------------------ accounts

    /// An account, if it exists.
    pub fn account(&self, key: &Pubkey) -> Option<Account> {
        self.svm.get_account(key)
    }

    /// Lamports of an account (0 if it does not exist).
    pub fn lamports(&self, key: &Pubkey) -> u64 {
        self.account(key).map_or(0, |a| a.lamports)
    }

    /// Writes an account.
    pub fn put(&mut self, key: Pubkey, account: Account) {
        self.svm.set_account(key, account).expect("set account");
    }

    /// A system account holding `lamports`.
    pub fn fund(&mut self, key: Pubkey, lamports: u64) {
        let mut account = self.account(&key).unwrap_or(Account {
            lamports: 0,
            data: vec![],
            owner: SYSTEM_PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        });
        account.lamports = lamports;
        self.put(key, account);
    }

    /// A funded keypair.
    pub fn funded(&mut self, lamports: u64) -> Keypair {
        let kp = Keypair::new();
        self.fund(kp.pubkey(), lamports);
        kp
    }

    /// Rent-exempt minimum for `len` bytes.
    pub fn rent(&self, len: usize) -> u64 {
        self.svm.minimum_balance_for_rent_exemption(len)
    }

    /// Reads an Anchor account.
    #[track_caller]
    pub fn read<T: AccountDeserialize>(&self, key: &Pubkey) -> T {
        let account = self
            .account(key)
            .unwrap_or_else(|| panic!("no account {key}"));
        T::try_deserialize(&mut &account.data[..])
            .unwrap_or_else(|e| panic!("deserialize {key}: {e}"))
    }

    /// Reads an Anchor account if it exists and deserializes.
    pub fn try_read<T: AccountDeserialize>(&self, key: &Pubkey) -> Option<T> {
        let account = self.account(key)?;
        T::try_deserialize(&mut &account.data[..]).ok()
    }

    /// Balance of a holding (0 if it does not exist).
    pub fn holding(&self, mint: &Pubkey, owner: &Pubkey) -> u64 {
        let key = bordrless_token::client::holding_address(mint, owner);
        self.try_read::<bordrless_token::state::Holding>(&key)
            .map_or(0, |h| h.amount)
    }

    /// Sets the upgrade authority of `program` by rewriting its ProgramData header.
    pub fn set_upgrade_authority(&mut self, program: Pubkey, authority: Option<Pubkey>) {
        let (pd, _) = Pubkey::find_program_address(&[program.as_ref()], &LOADER_V3);
        let mut account = self
            .account(&pd)
            .unwrap_or_else(|| panic!("no programdata for {program}"));
        write_programdata_header(&mut account.data, self.slot, authority);
        self.put(pd, account);
    }
}

/// Writes a loader-v3 ProgramData header (tag 3, slot, optional authority).
pub fn write_programdata_header(data: &mut [u8], slot: u64, authority: Option<Pubkey>) {
    data[..4].copy_from_slice(&3u32.to_le_bytes());
    data[4..12].copy_from_slice(&slot.to_le_bytes());
    match authority {
        Some(key) => {
            data[12] = 1;
            data[13..45].copy_from_slice(key.as_ref());
        }
        None => {
            data[12] = 0;
            data[13..45].fill(0);
        }
    }
}

/// Serialized size of a signed transaction.
pub fn wire_size(tx: &VersionedTransaction) -> usize {
    let signatures = tx.signatures.len();
    let n = match signatures {
        0..=0x7f => 1,
        0x80..=0x3fff => 2,
        _ => 3,
    };
    n + 64 * signatures + tx.message.serialize().len()
}
