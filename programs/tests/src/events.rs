//! The self-CPI events of a transaction: instruction data `EVENT_IX_TAG_LE ‖ discriminator ‖ Borsh`
//! sent by a program to itself with its event authority as the only account.

use anchor_lang::prelude::Pubkey;
use anchor_lang::{AnchorDeserialize, Discriminator};
use litesvm::types::TransactionMetadata;

use crate::env::Tx;

/// One raw event.
#[derive(Clone, Debug)]
pub struct RawEvent {
    /// The emitting program.
    pub program: Pubkey,
    /// The discriminator.
    pub discriminator: [u8; 8],
    /// The Borsh body.
    pub body: Vec<u8>,
}

/// The raw events of `meta`, in emission order.
pub fn raw_events(meta: &TransactionMetadata, keys: &[Pubkey]) -> Vec<RawEvent> {
    let tag = anchor_lang::event::EVENT_IX_TAG_LE;
    let mut out = Vec::new();
    for inner in meta.inner_instructions.iter().flatten() {
        let ix = &inner.instruction;
        if !ix.data.starts_with(tag) || ix.data.len() < tag.len() + 8 {
            continue;
        }
        let program = keys[usize::from(ix.program_id_index)];
        let mut discriminator = [0u8; 8];
        discriminator.copy_from_slice(&ix.data[tag.len()..tag.len() + 8]);
        out.push(RawEvent {
            program,
            discriminator,
            body: ix.data[tag.len() + 8..].to_vec(),
        });
    }
    out
}

impl Tx {
    /// The raw events of this (successful) transaction.
    #[track_caller]
    pub fn raw_events(&self) -> Vec<RawEvent> {
        raw_events(self.ok(), &self.keys)
    }

    /// The events of type `E` in this transaction.
    #[track_caller]
    pub fn events<E: Discriminator + AnchorDeserialize>(&self) -> Vec<E> {
        self.raw_events()
            .into_iter()
            .filter(|e| e.discriminator == E::DISCRIMINATOR)
            .map(|e| E::deserialize(&mut &e.body[..]).expect("event decodes"))
            .collect()
    }

    /// The one event of type `E` in this transaction.
    #[track_caller]
    pub fn event<E: Discriminator + AnchorDeserialize>(&self) -> E {
        let mut events = self.events::<E>();
        assert_eq!(
            events.len(),
            1,
            "expected exactly one event of that type\n{}",
            self.logs().join("\n")
        );
        events.remove(0)
    }
}
