//! Helpers for programs behind `hook_timelock` (`docs/phase3a.md` §3): loader buffers as
//! `solana program write-buffer` leaves them, executable hashes as `solana-verify` computes them,
//! a program registered under a timelock, and transactions with a signer whose key the suite does
//! not hold (sigverify off), for constants such as the Studio attester.

use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::instruction::Instruction;
use bordrless_hook::authority::{
    programdata_address, timelock_address, trimmed_len, BPF_LOADER_UPGRADEABLE_ID,
    PROGRAMDATA_HEADER_LEN,
};
use solana_account::Account;
use solana_keypair::Keypair;
use solana_message::{v0, VersionedMessage};
use solana_signature::Signature;
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;

use crate::env::{compute_unit_limit, wire_size, Env, Tx};

/// `solana-verify get-executable-hash`: sha256 of the code without its trailing zero bytes.
pub fn executable_hash(code: &[u8]) -> [u8; 32] {
    solana_sha256_hasher::hash(&code[..trimmed_len(code)]).to_bytes()
}

/// Lowercase hex.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The executable hash of `program`'s ProgramData as it is now.
pub fn programdata_hash(env: &Env, program: &Pubkey) -> [u8; 32] {
    let pd = env
        .account(&programdata_address(program))
        .expect("programdata");
    executable_hash(&pd.data[PROGRAMDATA_HEADER_LEN..])
}

/// A program's upgrade authority as its ProgramData says (`None`: immutable).
pub fn upgrade_authority(env: &Env, program: &Pubkey) -> Option<Pubkey> {
    let pd = env
        .account(&programdata_address(program))
        .expect("programdata");
    bordrless_hook::authority::parse_programdata(&pd.data)
        .expect("a programdata header")
        .1
}

/// The slot a program's ProgramData was last deployed, upgraded or extended in.
pub fn programdata_slot(env: &Env, program: &Pubkey) -> u64 {
    let pd = env
        .account(&programdata_address(program))
        .expect("programdata");
    bordrless_hook::authority::parse_programdata(&pd.data)
        .expect("a programdata header")
        .0
}

/// Writes a loader buffer at `key` holding `code` (and `extra_zeros` zero bytes after it), its
/// authority `authority`, rent-exempt: what `solana program write-buffer` leaves.
pub fn put_buffer(env: &mut Env, key: Pubkey, authority: Pubkey, code: &[u8], extra_zeros: usize) {
    let mut data = vec![1u8, 0, 0, 0, 1];
    data.extend_from_slice(authority.as_ref());
    data.extend_from_slice(code);
    data.extend(std::iter::repeat_n(0u8, extra_zeros));
    let lamports = env.rent(data.len());
    env.put(
        key,
        Account {
            lamports,
            data,
            owner: BPF_LOADER_UPGRADEABLE_ID,
            executable: false,
            rent_epoch: 0,
        },
    );
}

/// The buffer's authority (`None` for a closed or authority-less buffer).
pub fn buffer_authority(env: &Env, key: &Pubkey) -> Option<Pubkey> {
    let a = env.account(key)?;
    if a.owner != BPF_LOADER_UPGRADEABLE_ID {
        return None;
    }
    bordrless_hook::authority::parse_buffer(&a.data).flatten()
}

/// A program's timelock.
pub fn timelock_of(program: &Pubkey) -> Pubkey {
    timelock_address(program).0
}

/// Registers `program` (upgradeable by `authority`) under a timelock of `delay_secs` with
/// `author`; answers the transaction.
pub fn register(
    env: &mut Env,
    authority: &Keypair,
    program: Pubkey,
    delay_secs: u32,
    author: Pubkey,
) -> Tx {
    let payer = env.payer.insecure_clone();
    let ix = hook_timelock::client::register(
        payer.pubkey(),
        authority.pubkey(),
        program,
        delay_secs,
        author,
    );
    env.send(&[ix], &[authority])
}

/// Sends `ixs` (with a 1.4M compute budget in front) where `fakes` sign without their keys (the
/// SVM's sigverify must be off: [`Env::without_sigverify`]); `signers` sign for real.
pub fn send_as(env: &mut Env, ixs: &[Instruction], signers: &[&Keypair], fakes: &[Pubkey]) -> Tx {
    let mut all = vec![compute_unit_limit(1_400_000)];
    all.extend_from_slice(ixs);
    let payer = env.payer.insecure_clone();
    let message = v0::Message::try_compile(&payer.pubkey(), &all, &[], env.svm.latest_blockhash())
        .expect("compile");
    let n = message.header.num_required_signatures as usize;
    let keys: Vec<Pubkey> = message.account_keys[..n].to_vec();
    let message = VersionedMessage::V0(message);
    let bytes = message.serialize();
    let mut signatures = Vec::with_capacity(n);
    for key in &keys {
        if *key == payer.pubkey() {
            signatures.push(payer.sign_message(&bytes));
        } else if let Some(kp) = signers.iter().find(|k| k.pubkey() == *key) {
            signatures.push(kp.sign_message(&bytes));
        } else {
            assert!(fakes.contains(key), "no key for signer {key}");
            signatures.push(Signature::default());
        }
    }
    let tx = VersionedTransaction {
        signatures,
        message,
    };
    let size = wire_size(&tx);
    let result = env.svm.send_transaction(tx);
    env.svm.expire_blockhash();
    Tx {
        result,
        size,
        keys: vec![],
    }
}

impl Env {
    /// The same SVM with signature verification off (for [`send_as`]).
    pub fn without_sigverify(&mut self) {
        let svm = std::mem::take(&mut self.svm);
        self.svm = svm.with_sigverify(false);
    }
}
