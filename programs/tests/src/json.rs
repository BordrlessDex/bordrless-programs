//! The deterministic JSON the vector files are written in (as `vectors.rs` and `game_vectors.rs`
//! render theirs), for the phase-3a vectors: one top-level field a line, each section's entries one
//! a line, integers beyond 2^53 as strings.

use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};

/// A JSON value, rendered deterministically. Integers that may exceed 2^53 are strings.
pub enum J {
    /// null
    Null,
    /// a boolean
    Bool(bool),
    /// a safe integer
    Num(i64),
    /// a string
    Str(String),
    /// an array
    Arr(Vec<J>),
    /// an object, in field order
    Obj(Vec<(&'static str, J)>),
}

pub fn num(n: impl Into<i64>) -> J {
    J::Num(n.into())
}

pub fn big(n: impl Into<i128>) -> J {
    J::Str(n.into().to_string())
}

pub fn s(text: impl Into<String>) -> J {
    J::Str(text.into())
}

pub fn key(k: &Pubkey) -> J {
    s(k.to_string())
}

pub fn hex(bytes: &[u8]) -> J {
    s(bytes.iter().map(|b| format!("{b:02x}")).collect::<String>())
}

pub fn opt(v: Option<J>) -> J {
    v.unwrap_or(J::Null)
}

pub fn inline(j: &J, out: &mut String) {
    match j {
        J::Null => out.push_str("null"),
        J::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        J::Num(n) => out.push_str(&n.to_string()),
        J::Str(text) => {
            out.push('"');
            out.push_str(text);
            out.push('"');
        }
        J::Arr(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                inline(item, out);
            }
            out.push(']');
        }
        J::Obj(fields) => {
            out.push('{');
            for (i, (k, v)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push('"');
                out.push_str(k);
                out.push_str("\": ");
                inline(v, out);
            }
            out.push('}');
        }
    }
}

/// The file: the top-level fields one per line, each section's entries one per line.
pub fn render(fields: &[(&'static str, J)]) -> String {
    let mut out = String::from("{\n");
    for (i, (k, v)) in fields.iter().enumerate() {
        out.push_str(&format!("  \"{k}\": "));
        match v {
            J::Arr(items) if !items.is_empty() => {
                out.push_str("[\n");
                for (n, item) in items.iter().enumerate() {
                    out.push_str("    ");
                    inline(item, &mut out);
                    if n + 1 < items.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                out.push_str("  ]");
            }
            J::Obj(entries) if !entries.is_empty() => {
                out.push_str("{\n");
                for (n, (ek, ev)) in entries.iter().enumerate() {
                    out.push_str(&format!("    \"{ek}\": "));
                    inline(ev, &mut out);
                    if n + 1 < entries.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                out.push_str("  }");
            }
            other => inline(other, &mut out),
        }
        if i + 1 < fields.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str("}\n");
    out
}


/// An instruction as the SDK's vectors hold it: its program, its accounts (key, signer, writable)
/// and its data.
pub fn ix(name: &str, i: &Instruction) -> J {
    J::Obj(vec![
        ("name", s(name)),
        ("program", key(&i.program_id)),
        (
            "accounts",
            J::Arr(
                i.accounts
                    .iter()
                    .map(|m: &AccountMeta| {
                        J::Arr(vec![
                            key(&m.pubkey),
                            J::Bool(m.is_signer),
                            J::Bool(m.is_writable),
                        ])
                    })
                    .collect(),
            ),
        ),
        ("data", hex(&i.data)),
    ])
}

/// Writes `fields` to `vectors/<file>` when it differs, and fails then: a stale file never passes.
#[track_caller]
pub fn check_vectors(file: &str, fields: &[(&'static str, J)]) {
    let json = render(fields);
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("vectors")
        .join(file);
    let on_disk = std::fs::read_to_string(&path).ok();
    if on_disk.as_deref() != Some(json.as_str()) {
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("vectors directory");
        std::fs::write(&path, &json).expect("write the vectors");
        panic!(
            "{} was stale (or missing) and has been rewritten from the Rust reference: review the \
             change, then run the tests again",
            path.display()
        );
    }
}
