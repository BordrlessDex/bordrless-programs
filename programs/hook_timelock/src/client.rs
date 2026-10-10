//! Builders for `hook_timelock`'s instructions, as the tests and the SDK build them.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::{system_program, InstructionData, ToAccountMetas};
use bordrless_hook::authority::{programdata_address, BPF_LOADER_UPGRADEABLE_ID};

use crate::loader::{CLOCK_SYSVAR, RENT_SYSVAR};
use crate::Timelock;

/// This program's event authority.
pub fn event_authority() -> Pubkey {
    Pubkey::find_program_address(&[b"__event_authority"], &crate::ID).0
}

/// The timelock (and upgrade authority, once registered) of `program`.
pub fn timelock_address(program: &Pubkey) -> Pubkey {
    Timelock::address(program).0
}

/// `register(delay_secs, author)`: `authority` (the program's current upgrade authority) hands
/// `program` to its timelock; `payer` pays the timelock's rent.
pub fn register(
    payer: Pubkey,
    authority: Pubkey,
    program: Pubkey,
    delay_secs: u32,
    author: Pubkey,
) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: crate::accounts::Register {
            payer,
            authority,
            target: program,
            programdata: programdata_address(&program),
            timelock: timelock_address(&program),
            loader: BPF_LOADER_UPGRADEABLE_ID,
            system_program: system_program::ID,
            event_authority: event_authority(),
            program: crate::ID,
        }
        .to_account_metas(None),
        data: crate::instruction::Register { delay_secs, author }.data(),
    }
}

/// `propose(len)`: `buffer` (its authority already the timelock) proposed for `program`.
pub fn propose(author: Pubkey, program: Pubkey, buffer: Pubkey, len: u32) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: crate::accounts::Propose {
            author,
            timelock: timelock_address(&program),
            buffer,
            event_authority: event_authority(),
            program: crate::ID,
        }
        .to_account_metas(None),
        data: crate::instruction::Propose { len }.data(),
    }
}

fn close_proposal(sender: Pubkey, program: Pubkey, buffer: Pubkey, author: Pubkey) -> Vec<AccountMeta> {
    crate::accounts::CloseProposal {
        sender,
        timelock: timelock_address(&program),
        buffer,
        author,
        loader: BPF_LOADER_UPGRADEABLE_ID,
        event_authority: event_authority(),
        program: crate::ID,
    }
    .to_account_metas(None)
}

/// `cancel`: the author closes the proposal's `buffer` to themselves.
pub fn cancel(author: Pubkey, program: Pubkey, buffer: Pubkey) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: close_proposal(author, program, buffer, author),
        data: crate::instruction::Cancel {}.data(),
    }
}

/// `expire`: anyone, after the window; the proposal's `buffer` is closed to `author`.
pub fn expire(sender: Pubkey, program: Pubkey, buffer: Pubkey, author: Pubkey) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: close_proposal(sender, program, buffer, author),
        data: crate::instruction::Expire {}.data(),
    }
}

/// `execute`: anyone, from the eta; `buffer` the proposal's, `author` the timelock's (the spill).
pub fn execute(sender: Pubkey, program: Pubkey, buffer: Pubkey, author: Pubkey) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: crate::accounts::Execute {
            sender,
            timelock: timelock_address(&program),
            target: program,
            programdata: programdata_address(&program),
            buffer,
            author,
            rent: RENT_SYSVAR,
            clock: CLOCK_SYSVAR,
            loader: BPF_LOADER_UPGRADEABLE_ID,
            event_authority: event_authority(),
            program: crate::ID,
        }
        .to_account_metas(None),
        data: crate::instruction::Execute {}.data(),
    }
}

/// `reclaim_buffer`: the author closes a stray `buffer` (authority the timelock) to themselves.
pub fn reclaim_buffer(author: Pubkey, program: Pubkey, buffer: Pubkey) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: crate::accounts::ReclaimBuffer {
            author,
            timelock: timelock_address(&program),
            buffer,
            loader: BPF_LOADER_UPGRADEABLE_ID,
            event_authority: event_authority(),
            program: crate::ID,
        }
        .to_account_metas(None),
        data: crate::instruction::ReclaimBuffer {}.data(),
    }
}

fn author_only(author: Pubkey, program: Pubkey) -> Vec<AccountMeta> {
    crate::accounts::AuthorOnly {
        author,
        timelock: timelock_address(&program),
        event_authority: event_authority(),
        program: crate::ID,
    }
    .to_account_metas(None)
}

/// `lengthen(delay_secs)`.
pub fn lengthen(author: Pubkey, program: Pubkey, delay_secs: u32) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: author_only(author, program),
        data: crate::instruction::Lengthen { delay_secs }.data(),
    }
}

/// `propose_author(new_author)`.
pub fn propose_author(author: Pubkey, program: Pubkey, new_author: Pubkey) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: author_only(author, program),
        data: crate::instruction::ProposeAuthor { new_author }.data(),
    }
}

/// `accept_author`, signed by the author named.
pub fn accept_author(new_author: Pubkey, program: Pubkey) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: crate::accounts::AcceptAuthor {
            new_author,
            timelock: timelock_address(&program),
            event_authority: event_authority(),
            program: crate::ID,
        }
        .to_account_metas(None),
        data: crate::instruction::AcceptAuthor {}.data(),
    }
}

/// `finalize`: the program made immutable.
pub fn finalize(author: Pubkey, program: Pubkey) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: crate::accounts::Finalize {
            author,
            timelock: timelock_address(&program),
            programdata: programdata_address(&program),
            loader: BPF_LOADER_UPGRADEABLE_ID,
            event_authority: event_authority(),
            program: crate::ID,
        }
        .to_account_metas(None),
        data: crate::instruction::Finalize {}.data(),
    }
}
