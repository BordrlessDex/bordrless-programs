//! The reference venues of the SOL switch in LiteSVM (`docs/token-quotes.md` §7.5.2): the mainnet
//! programs of Raydium AMM v4, CPMM and CLMM and of Pump AMM (and Pump's fee program, which its
//! `sell` calls), dumped from mainnet into `fixtures/venues/*.so` (`scripts/solana/venue-fixtures.sh`;
//! not in the text patch), and live pools read from mainnet on 21 Oct 2026 into
//! `fixtures/venues/*.accounts` (one account a line: key, owner, lamports, executable, base64 data;
//! each pool's accounts read in one `getMultipleAccounts`, so they share a slot). Real swaps run on
//! them: the tests check the launchpad's reads against what the venue then does.

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use bordrless_launch::client as launch;
use bordrless_launch::venues::{self, pump};
use bordrless_swap::spl;
use solana_account::Account;

use crate::env::Env;

/// The venue programs' files and ids.
pub const PROGRAMS: [(&str, Pubkey); 5] = [
    ("raydium_amm_v4", venues::RAYDIUM_AMM_V4),
    ("raydium_cpmm", venues::RAYDIUM_CPMM),
    ("raydium_clmm", venues::RAYDIUM_CLMM),
    ("pump_amm", venues::PUMP_AMM),
    ("pump_fees", pump::FEE_PROGRAM),
];

/// A time after every fixture's own (their pools' observations, open times and the fee program's
/// tiers all read as of then): the tests' clock when they trade on the venues.
pub const VENUE_NOW: i64 = 1_792_567_700;

fn dir() -> String {
    format!("{}/fixtures/venues", env!("CARGO_MANIFEST_DIR"))
}

/// Whether the venue programs' dumps are present.
pub fn present() -> bool {
    PROGRAMS
        .iter()
        .all(|(name, _)| std::path::Path::new(&format!("{}/{name}.so", dir())).exists())
}

/// Loads every venue program into `env` (the dumps must be there: `present`).
pub fn load_programs(env: &mut Env) {
    for (name, id) in PROGRAMS {
        let path = format!("{}/{name}.so", dir());
        let so = std::fs::read(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        env.svm
            .add_program(id, &so)
            .unwrap_or_else(|e| panic!("load {name}: {e:?}"));
    }
}

/// Standard base64 (with padding), decoded.
pub fn base64(text: &str) -> Vec<u8> {
    let value = |c: u8| -> u32 {
        match c {
            b'A'..=b'Z' => u32::from(c - b'A'),
            b'a'..=b'z' => u32::from(c - b'a') + 26,
            b'0'..=b'9' => u32::from(c - b'0') + 52,
            b'+' => 62,
            b'/' => 63,
            _ => panic!("not base64: {c}"),
        }
    };
    let bytes: Vec<u8> = text.bytes().filter(|c| *c != b'=').collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut acc = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            acc |= value(*c) << (18 - 6 * i);
        }
        let n = chunk.len() * 6 / 8;
        for i in 0..n {
            out.push((acc >> (16 - 8 * i)) as u8);
        }
    }
    out
}

/// A pool's accounts as read from mainnet.
pub struct Fixture {
    pub name: String,
    pub slot: u64,
    pub accounts: Vec<(Pubkey, Account)>,
}

impl Fixture {
    /// `fixtures/venues/<name>.accounts`.
    pub fn load(name: &str) -> Self {
        let path = format!("{}/{name}.accounts", dir());
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        let mut slot = 0;
        let mut accounts = vec![];
        for line in text.lines() {
            if let Some(header) = line.strip_prefix('#') {
                if let Some(at) = header.find("slot ") {
                    slot = header[at + 5..]
                        .split(|c: char| !c.is_ascii_digit())
                        .next()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0);
                }
                continue;
            }
            let f: Vec<&str> = line.split(' ').collect();
            if f.len() < 5 {
                continue;
            }
            let data = if f[4] == "-" { vec![] } else { base64(f[4]) };
            accounts.push((
                f[0].parse().expect("key"),
                Account {
                    lamports: f[2].parse().expect("lamports"),
                    data,
                    owner: f[1].parse().expect("owner"),
                    executable: f[3] == "1",
                    rent_epoch: 0,
                },
            ));
        }
        Self {
            name: name.to_string(),
            slot,
            accounts,
        }
    }

    /// The account at `key`.
    pub fn account(&self, key: &Pubkey) -> &Account {
        &self
            .accounts
            .iter()
            .find(|(k, _)| k == key)
            .unwrap_or_else(|| panic!("{}: no account {key}", self.name))
            .1
    }

    /// The first account owned by `program` with `len` bytes (a pool, by its size).
    pub fn find(&self, program: &Pubkey, len: usize) -> Pubkey {
        self.accounts
            .iter()
            .find(|(_, a)| a.owner == *program && a.data.len() == len)
            .unwrap_or_else(|| panic!("{}: no account of {program} with {len} bytes", self.name))
            .0
    }

    /// Writes every account into `env` (the venue's programs are loaded apart). Mainnet's rent is
    /// lower than LiteSVM's (5,080 lamports a byte-epoch since its 2026 change, against 6,960): an
    /// account made at mainnet's minimum is topped up to LiteSVM's, so a venue's own rent-exempt
    /// constraints read it as mainnet does.
    pub fn put(&self, env: &mut Env) {
        for (key, account) in &self.accounts {
            if account.executable {
                continue;
            }
            let mut account = account.clone();
            if !account.data.is_empty() {
                account.lamports = account.lamports.max(env.rent(account.data.len()));
            }
            env.put(*key, account);
        }
    }
}

fn key_at(data: &[u8], at: usize) -> Pubkey {
    Pubkey::new_from_array(data[at..at + 32].try_into().expect("32"))
}

/// A reference pool as the tests use it: the venue accounts of a sale of `token` on it, the other
/// side's mint, and the token's program.
pub struct Venue {
    pub pool: Pubkey,
    pub token: Pubkey,
    pub other: Pubkey,
    pub token_program: Pubkey,
    /// Whether the token is the pool's first side.
    pub token_is_0: bool,
    pub accounts: Vec<AccountMeta>,
}

impl Venue {
    /// A Raydium AMM v4 pool of `f`, selling `token`.
    pub fn amm_v4(f: &Fixture, token: Pubkey) -> Self {
        let pool = f.find(&venues::RAYDIUM_AMM_V4, venues::AMM_V4_LEN);
        let d = &f.account(&pool).data;
        let (coin, pc) = (key_at(d, 400), key_at(d, 432));
        let other = if coin == token { pc } else { coin };
        Self {
            pool,
            token,
            other,
            token_program: spl::TOKEN_PROGRAM_ID,
            token_is_0: coin == token,
            accounts: launch::amm_v4_accounts(pool, d[8], key_at(d, 336), key_at(d, 368)),
        }
    }

    /// A Raydium CPMM pool of `f`, selling `token`.
    pub fn cpmm(f: &Fixture, token: Pubkey) -> Self {
        let pool = f.find(&venues::RAYDIUM_CPMM, venues::CPMM_POOL_LEN);
        let d = &f.account(&pool).data;
        let (m0, m1) = (key_at(d, 168), key_at(d, 200));
        let (other, program) = if m0 == token {
            (m1, key_at(d, 232))
        } else {
            (m0, key_at(d, 264))
        };
        Self {
            pool,
            token,
            other,
            token_program: program,
            token_is_0: m0 == token,
            accounts: launch::cpmm_accounts(
                key_at(d, 8),
                pool,
                key_at(d, 72),
                key_at(d, 104),
                key_at(d, 296),
            ),
        }
    }

    /// A Raydium CLMM pool of `f`, selling `token`: the bitmap extension, then the fixture's tick
    /// arrays in the sale's direction (from the one holding the current tick).
    pub fn clmm(f: &Fixture, token: Pubkey) -> Self {
        let pool = f.find(&venues::RAYDIUM_CLMM, venues::CLMM_POOL_LEN);
        let d = &f.account(&pool).data;
        let (m0, m1) = (key_at(d, 73), key_at(d, 105));
        let zero_for_one = m0 == token;
        let other = if zero_for_one { m1 } else { m0 };
        let spacing = i32::from(u16::from_le_bytes([d[235], d[236]]));
        let tick = i32::from_le_bytes(d[269..273].try_into().unwrap());
        let span = 60 * spacing;
        let current = tick.div_euclid(span) * span;
        let mut arrays: Vec<(i32, Pubkey)> = f
            .accounts
            .iter()
            .filter(|(_, a)| {
                a.owner == venues::RAYDIUM_CLMM
                    && a.data.len() == venues::CLMM_TICK_ARRAY_LEN
                    && key_at(&a.data, 8) == pool
            })
            .map(|(k, a)| (i32::from_le_bytes(a.data[40..44].try_into().unwrap()), *k))
            .filter(|(start, _)| {
                if zero_for_one {
                    *start <= current
                } else {
                    *start >= current
                }
            })
            .collect();
        arrays.sort_by_key(|(start, _)| if zero_for_one { -*start } else { *start });
        let mut ticks: Vec<Pubkey> = f
            .accounts
            .iter()
            .filter(|(_, a)| {
                a.owner == venues::RAYDIUM_CLMM
                    && a.data.len() == venues::CLMM_BITMAP_EXTENSION_LEN
                    && key_at(&a.data, 8) == pool
            })
            .map(|(k, _)| *k)
            .collect();
        ticks.extend(arrays.iter().map(|(_, k)| *k));
        let token_program = f.account(&token).owner;
        Self {
            pool,
            token,
            other,
            token_program,
            token_is_0: zero_for_one,
            accounts: launch::clmm_accounts(
                key_at(d, 9),
                pool,
                key_at(d, 137),
                key_at(d, 169),
                key_at(d, 201),
                &ticks,
            ),
        }
    }

    /// A Pump AMM pool of `f`, selling its base: the global config's first protocol fee recipient
    /// and first buyback wallet, with their accounts of the quote, and `pool_v2` when the fixture
    /// has it.
    pub fn pump(f: &Fixture) -> Self {
        let pool = f
            .accounts
            .iter()
            .find(|(_, a)| a.owner == pump::PROGRAM && a.data[..8] == pump::POOL_DISCRIMINATOR)
            .expect("a Pump AMM pool")
            .0;
        let d = &f.account(&pool).data;
        let (base, quote) = (key_at(d, 43), key_at(d, 75));
        let coin_creator = key_at(d, 211);
        let g = &f.account(&pump::GLOBAL_CONFIG).data;
        let recipient = key_at(g, 57);
        let buyback = key_at(g, 643);
        let quote_program = spl::TOKEN_PROGRAM_ID;
        let ata = |owner: &Pubkey| spl::associated_token_address(owner, &quote, &quote_program);
        let creator_vault = Pubkey::find_program_address(
            &[b"creator_vault", coin_creator.as_ref()],
            &pump::PROGRAM,
        )
        .0;
        let pool_v2 = Pubkey::find_program_address(&[b"pool-v2", base.as_ref()], &pump::PROGRAM).0;
        // `pool_v2` goes along exactly when the sale pays a creator fee (the program's own rule).
        let has_v2 = pump_pays_creator(f, d, &base, &quote);
        Self {
            pool,
            token: base,
            other: quote,
            token_program: f.account(&base).owner,
            token_is_0: true,
            accounts: launch::pump_accounts(
                pool,
                key_at(d, 139),
                key_at(d, 171),
                recipient,
                ata(&recipient),
                ata(&creator_vault),
                creator_vault,
                has_v2.then_some(pool_v2),
                buyback,
                ata(&buyback),
            ),
        }
    }
}

impl Venue {
    fn k(&self, i: usize) -> Pubkey {
        self.accounts[i].pubkey
    }

    /// The venue's own exact-in sale of `amount` of the token by `owner` from `source` into
    /// `destination` (an account of the other side), for at least `min_out`: what a trader sends
    /// the venue itself (the push tests' attacker), built as the launchpad's `swap_ix` builds it.
    pub fn sell_ix(
        &self,
        owner: Pubkey,
        source: Pubkey,
        destination: Pubkey,
        amount: u64,
        min_out: u64,
    ) -> Instruction {
        let ro = |k: Pubkey| AccountMeta::new_readonly(k, false);
        let rw = |k: Pubkey| AccountMeta::new(k, false);
        let program = self.k(0);
        let (data, accounts) = if program == venues::RAYDIUM_AMM_V4 {
            let mut data = vec![16];
            data.extend_from_slice(&amount.to_le_bytes());
            data.extend_from_slice(&min_out.to_le_bytes());
            let accounts = vec![
                ro(spl::TOKEN_PROGRAM_ID),
                rw(self.k(1)),
                ro(self.k(2)),
                rw(self.k(3)),
                rw(self.k(4)),
                rw(source),
                rw(destination),
                AccountMeta::new_readonly(owner, true),
            ];
            (data, accounts)
        } else if program == venues::RAYDIUM_CPMM {
            let mut data = vec![143, 190, 90, 218, 196, 30, 51, 222];
            data.extend_from_slice(&amount.to_le_bytes());
            data.extend_from_slice(&min_out.to_le_bytes());
            let (iv, ov) = if self.token_is_0 {
                (self.k(4), self.k(5))
            } else {
                (self.k(5), self.k(4))
            };
            let accounts = vec![
                AccountMeta::new_readonly(owner, true),
                ro(self.k(1)),
                ro(self.k(2)),
                rw(self.k(3)),
                rw(source),
                rw(destination),
                rw(iv),
                rw(ov),
                ro(self.token_program),
                ro(spl::TOKEN_PROGRAM_ID),
                ro(self.token),
                ro(self.other),
                rw(self.k(6)),
            ];
            (data, accounts)
        } else if program == venues::RAYDIUM_CLMM {
            let mut data = vec![43, 4, 237, 11, 26, 201, 30, 98];
            data.extend_from_slice(&amount.to_le_bytes());
            data.extend_from_slice(&min_out.to_le_bytes());
            data.extend_from_slice(&0u128.to_le_bytes());
            data.push(1);
            let (iv, ov) = if self.token_is_0 {
                (self.k(3), self.k(4))
            } else {
                (self.k(4), self.k(3))
            };
            let mut a = vec![
                AccountMeta::new_readonly(owner, true),
                ro(self.k(1)),
                rw(self.k(2)),
                rw(source),
                rw(destination),
                rw(iv),
                rw(ov),
                rw(self.k(5)),
                ro(spl::TOKEN_PROGRAM_ID),
                ro(spl::TOKEN_2022_PROGRAM_ID),
                ro(venues::MEMO_PROGRAM),
                ro(self.token),
                ro(self.other),
            ];
            a.extend(self.accounts[8..].iter().map(|m| rw(m.pubkey)));
            (data, a)
        } else {
            let mut data = pump::SELL_DISCRIMINATOR.to_vec();
            data.extend_from_slice(&amount.to_le_bytes());
            data.extend_from_slice(&min_out.to_le_bytes());
            let n = self.accounts.len();
            let mut a = vec![
                rw(self.k(1)),
                AccountMeta::new(owner, true),
                ro(self.k(2)),
                ro(self.token),
                ro(self.other),
                rw(source),
                rw(destination),
                rw(self.k(3)),
                rw(self.k(4)),
                ro(self.k(5)),
                rw(self.k(6)),
                ro(self.token_program),
                ro(spl::TOKEN_PROGRAM_ID),
                ro(anchor_lang::system_program::ID),
                ro(spl::ATA_PROGRAM_ID),
                ro(self.k(7)),
                ro(pump::PROGRAM),
                rw(self.k(8)),
                ro(self.k(9)),
                ro(self.k(10)),
                ro(self.k(11)),
            ];
            if n == 15 {
                a.push(ro(self.k(12)));
            }
            a.push(ro(self.k(n - 2)));
            a.push(rw(self.k(n - 1)));
            (data, a)
        };
        Instruction {
            program_id: program,
            accounts,
            data,
        }
    }
}

/// Whether a sale on the Pump AMM pool `d` (of `f`) pays a creator fee: its coin creator set, and
/// the pool's own creator fee (when the global config lets pools set one) or its fee schedule's
/// creator part above zero, as `bordrless_launch::venues::pump::read` works it out.
fn pump_pays_creator(f: &Fixture, d: &[u8], base: &Pubkey, quote: &Pubkey) -> bool {
    if key_at(d, 211) == Pubkey::default() {
        return false;
    }
    let g = &f.account(&pump::GLOBAL_CONFIG).data;
    let own = d
        .get(261..269)
        .map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap()));
    if g.get(940).copied().unwrap_or(0) != 0 && own != 0 {
        return true;
    }
    let base_vault = f.account(&key_at(d, 139));
    let quote_vault = f.account(&key_at(d, 171));
    let amount = |a: &Account| u64::from_le_bytes(a.data[64..72].try_into().unwrap());
    let virtual_quote = d
        .get(245..261)
        .map_or(0, |b| i128::from_le_bytes(b.try_into().unwrap()));
    let effective = (i128::from(amount(quote_vault)) + virtual_quote) as u128;
    let supply = if d.get(243).copied().unwrap_or(0) != 0 {
        pump::MAYHEM_SUPPLY
    } else {
        u128::from(u64::from_le_bytes(
            f.account(base).data[36..44].try_into().unwrap(),
        ))
    };
    let mcap = effective * supply / u128::from(amount(base_vault));
    let canonical = key_at(d, 11)
        == Pubkey::find_program_address(&[b"pool-authority", base.as_ref()], &pump::PUMP_BONDING).0;
    pump::schedule_fees(&f.account(&pump::FEE_CONFIG).data, canonical, quote, mcap)
        .is_some_and(|fees| fees.creator > 0)
}
