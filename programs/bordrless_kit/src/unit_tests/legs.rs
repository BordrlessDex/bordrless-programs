//! The rules of a coin quoted in tokens (`docs/token-quotes.md` §6.1): every leg's pool is the
//! launch's pool, max wallet lifts when the launchpad marks the `Legs`, and the `Legs` read in place.

use super::*;
use crate::legs::LegPools;

/// A coin quoted in tokens with `modules` (no holder rewards) and three legs, leg 0's pool the
/// config's: the launch holds the supply, a third of it then moved into each leg's pool.
struct Legged {
    config: KitConfig,
    key: Pubkey,
    legs: LegPools,
    balances: std::collections::HashMap<Pubkey, (u64, [u8; 64])>,
    now: i64,
}

impl Legged {
    fn new(modules: u8) -> Self {
        let mut kit = setup(modules);
        kit.config.set_token_quoted();
        let mut pools = [Pubkey::default(); LEGS_MAX];
        pools[0] = kit.config.pool;
        pools[1] = pda();
        pools[2] = pda();
        // Every leg opened at the launch: trading opened with it (no delay).
        let legs = LegPools {
            pools,
            count: 3,
            lifted: false,
            all_open: true,
            last_opened_at: NOW,
        };
        let mut balances = std::collections::HashMap::new();
        balances.insert(kit.config.launch, (SUPPLY, [0; 64]));
        let mut l = Self {
            config: kit.config,
            key: kit.key,
            legs,
            balances,
            now: NOW,
        };
        let launch = l.config.launch;
        for i in 0..3 {
            let pool = l.legs.pools[i];
            l.send(launch, pool, SUPPLY / 3).unwrap();
        }
        l
    }

    fn get(&self, owner: &Pubkey) -> (u64, [u8; 64]) {
        self.balances.get(owner).copied().unwrap_or((0, [0; 64]))
    }

    /// A transfer through the rules, with the coin's `Legs` (`legs`) or as a kit without them
    /// would see it (`None`).
    fn send_seen(
        &mut self,
        from: Pubkey,
        to: Pubkey,
        amount: u64,
        legs: Option<LegPools>,
    ) -> Result<HookReturn> {
        let (fb, fd) = self.get(&from);
        let (tb, td) = self.get(&to);
        let args = transfer(&self.config, from, to, amount, fb, tb, fd, td);
        let mut config = self.config.clone();
        let ret = rules::before_transfer_with(
            &mut config,
            &self.key,
            &args,
            None,
            self.now,
            legs.as_ref(),
        )?;
        self.config = config;
        let fd = ret.source_hook_data.unwrap_or(fd);
        let td = ret.destination_hook_data.unwrap_or(td);
        self.balances.insert(from, (fb - amount, fd));
        self.balances.insert(to, (tb + amount, td));
        Ok(ret)
    }

    fn send(&mut self, from: Pubkey, to: Pubkey, amount: u64) -> Result<HookReturn> {
        let legs = self.legs;
        self.send_seen(from, to, amount, Some(legs))
    }

    fn burn(&mut self, from: Pubkey, amount: u64) -> Result<HookReturn> {
        let (fb, fd) = self.get(&from);
        let args = burn(&self.config, from, amount, fb, fd);
        let mut config = self.config.clone();
        let ret = rules::before_burn_with(&mut config, &args, None, self.now, Some(&self.legs))?;
        self.config = config;
        self.balances
            .insert(from, (fb - amount, ret.source_hook_data.unwrap_or(fd)));
        Ok(ret)
    }

    /// The holders' balances: every owner but the launch and the legs' pools.
    fn holders_sum(&self) -> u64 {
        self.balances
            .iter()
            .filter(|(o, _)| !self.config.is_excluded_with(o, Some(&self.legs)))
            .map(|(_, (b, _))| *b)
            .sum()
    }
}

#[test]
fn every_legs_pool_is_the_pool() {
    let mut l = Legged::new(modules::MAX_WALLET | modules::EARLY_BUYER_LOCK);
    let cap = l.config.max_wallet_amount;
    assert_eq!(cap, SUPPLY / 50, "2% of the supply");
    assert!(l.config.is_token_quoted() && !l.config.is_native());
    // The deposits moved a third of the supply into each pool, uncapped, nobody eligible.
    assert_eq!(l.config.eligible, 0);
    let (p0, p1, p2) = (l.legs.pools[0], l.legs.pools[1], l.legs.pools[2]);
    // Seen without the `Legs`, leg 1's pool is a holder: its deposit would be capped.
    let launch = l.config.launch;
    l.balances.insert(launch, (SUPPLY, [0; 64]));
    let other = pda();
    let mut without = l.legs;
    without.pools[1] = other;
    assert_eq!(
        code(l.send_seen(launch, p1, SUPPLY / 3, Some(without))),
        kit(KitError::MaxWalletExceeded)
    );
    assert_eq!(
        code(l.send_seen(launch, p1, SUPPLY / 3, None)),
        kit(KitError::MaxWalletExceeded)
    );
    l.balances.insert(launch, (0, [0; 64]));
    // Buys out of any leg are capped across legs: the cap counts what the wallet holds.
    let buyer = wallet();
    l.send(p1, buyer, cap / 2).unwrap();
    l.send(p2, buyer, cap / 2).unwrap();
    assert_eq!(code(l.send(p0, buyer, 1)), kit(KitError::MaxWalletExceeded));
    let friend = wallet();
    l.send(p0, friend, cap).unwrap();
    // The early window: every buy out of any leg is locked.
    let (_, data) = l.get(&buyer);
    assert_eq!(HolderData::read(&data).early_locked, cap / 2 * 2);
    assert_eq!(code(l.send(buyer, p0, 1)), kit(KitError::EarlyLocked));
    // Unlocked: a wallet-to-wallet transfer above the cap is refused too.
    l.now = NOW + 3_600;
    assert_eq!(
        code(l.send(friend, buyer, 1)),
        kit(KitError::MaxWalletExceeded)
    );
    // Sells into any leg's pool are never capped, whatever the pool holds.
    l.send(buyer, p2, cap).unwrap();
    assert!(l.get(&p2).0 > cap);
    // The eligible count is the holders' balances: pools and the launch are left out.
    assert_eq!(l.config.eligible, l.holders_sum());
    // A buy's burn from any leg's pool is the pool's: nothing the kit keeps changes.
    let before = l.config.clone();
    l.burn(p1, 1_000).unwrap();
    assert_eq!(l.config, before);
    // A holder's burn leaves the eligible supply.
    l.burn(friend, 1_000).unwrap();
    assert_eq!(l.config.eligible, l.holders_sum());
}

#[test]
fn the_early_window_counts_buys_out_of_any_leg_only() {
    let mut l = Legged::new(modules::EARLY_BUYER_LOCK);
    let (p0, p2) = (l.legs.pools[0], l.legs.pools[2]);
    let (a, b) = (wallet(), wallet());
    l.send(p2, a, 5_000).unwrap();
    l.send(p0, a, 7_000).unwrap();
    // A transfer from a holder in the window is no buy.
    l.send(a, b, 0).unwrap();
    assert_eq!(HolderData::read(&l.get(&a).1).early_locked, 12_000);
    assert_eq!(HolderData::read(&l.get(&b).1).early_locked, 0);
    assert_eq!(code(l.send(a, p0, 1)), kit(KitError::EarlyLocked));
    // After the window buys are free; the early ones stay locked until the unlock.
    l.now = NOW + 61;
    l.send(p2, a, 3_000).unwrap();
    l.send(a, p0, 3_000).unwrap();
    assert_eq!(code(l.send(a, p0, 1)), kit(KitError::EarlyLocked));
    l.now = NOW + 3_600;
    l.send(a, p2, 12_000).unwrap();
    assert_eq!(l.get(&a).0, 0);
}

#[test]
fn max_wallet_lifts_when_the_legs_say_so_and_the_config_remembers() {
    let mut l = Legged::new(modules::MAX_WALLET | modules::CREATOR_WALLET_LOCK);
    let cap = l.config.max_wallet_amount;
    let p1 = l.legs.pools[1];
    let whale = wallet();
    l.send(p1, whale, cap).unwrap();
    assert_eq!(code(l.send(p1, whale, 1)), kit(KitError::MaxWalletExceeded));
    assert!(!l.config.graduated);
    l.legs.lifted = true;
    l.send(p1, whale, cap).unwrap();
    assert!(l.config.graduated, "the config remembers the lift");
    l.legs.lifted = false;
    l.send(p1, whale, cap).unwrap();
    // The creator wallet lock stays whatever the legs sold.
    let creator = l.config.creator;
    l.send(p1, creator, 10).unwrap();
    assert_eq!(
        code(l.send(creator, whale, 1)),
        kit(KitError::CreatorLocked)
    );
    assert_eq!(code(l.send(creator, p1, 1)), kit(KitError::CreatorLocked));
    l.now = l.config.creator_unlock_at;
    l.send(creator, p1, 10).unwrap();
}

/// The locks run from when trading opened (the last leg's opening), not from the launch: a launch
/// whose last leg opened 61 s late (the early window's 60 s spent before anyone could trade, were
/// it counted from the launch) still locks the first buy, until an hour after trading opened, and
/// the creator wallet lock ends a day after trading opened.
#[test]
fn the_locks_run_from_when_trading_opened() {
    let mut l = Legged::new(modules::CREATOR_WALLET_LOCK | modules::EARLY_BUYER_LOCK);
    let late = 61;
    l.legs.last_opened_at = NOW + late;
    l.now = NOW + late;
    let times = rules::lock_times(&l.config, Some(&l.legs), l.now);
    assert_eq!(
        (
            times.creator_unlock_at,
            times.early_window_end,
            times.early_unlock_at
        ),
        (
            l.config.creator_unlock_at + late,
            l.config.early_window_end + late,
            l.config.early_unlock_at + late
        )
    );
    assert_eq!(
        rules::lock_times(&l.config, None, l.now).early_window_end,
        NOW + 60
    );
    // The first buy, a second into trading: locked, as on a SOL coin.
    let (p0, p2) = (l.legs.pools[0], l.legs.pools[2]);
    let a = wallet();
    l.send(p2, a, 5_000).unwrap();
    assert_eq!(HolderData::read(&l.get(&a).1).early_locked, 5_000);
    // Locked an hour from trading's opening, not from the launch.
    l.now = NOW + 3_600;
    assert_eq!(code(l.send(a, p0, 1)), kit(KitError::EarlyLocked));
    assert_eq!(code(l.burn(a, 1)), kit(KitError::EarlyLocked));
    l.now = NOW + late + 3_600;
    l.send(a, p0, 5_000).unwrap();
    // The creator wallet lock likewise.
    let creator = l.config.creator;
    l.now = NOW + late;
    l.send(p0, creator, 10).unwrap();
    l.now = l.config.creator_unlock_at;
    assert_eq!(code(l.send(creator, p0, 1)), kit(KitError::CreatorLocked));
    l.now = l.config.creator_unlock_at + late;
    l.send(creator, p0, 10).unwrap();
    // Before every leg is open nothing trades, so no lock has started: the times run from `now`.
    l.legs.all_open = false;
    let times = rules::lock_times(&l.config, Some(&l.legs), NOW + 500);
    assert_eq!(times.early_window_end, l.config.early_window_end + 500);
    // A leg opened "before" the launch (never: `opened_at` follows `created_at`) moves nothing back.
    l.legs.all_open = true;
    l.legs.last_opened_at = NOW - 10;
    assert_eq!(l.legs.delay(NOW, NOW), 0);
}

/// A `Legs` account's bytes as the launchpad lays them out, its pools at their offsets, every leg
/// open (`opened_at` 1,000 + i).
fn legs_data(mint: &Pubkey, pools: &[Pubkey], lifted: bool) -> Vec<u8> {
    let mut d = vec![0u8; LEGS_LEN];
    d[..8].copy_from_slice(&LEGS_DISCRIMINATOR);
    d[8] = 1;
    d[LEGS_MINT_OFFSET..LEGS_MINT_OFFSET + 32].copy_from_slice(mint.as_ref());
    d[LEGS_COUNT_OFFSET] = pools.len() as u8;
    d[LEGS_OPENED_OFFSET] = pools.len() as u8;
    for (i, p) in pools.iter().enumerate() {
        let leg = LEGS_FIRST_LEG_OFFSET + i * LEGS_LEG_LEN;
        let at = leg + LEGS_LEG_POOL_OFFSET;
        d[at..at + 32].copy_from_slice(p.as_ref());
        let at = leg + LEGS_LEG_OPENED_AT_OFFSET;
        d[at..at + 8].copy_from_slice(&(1_000 + i as i64).to_le_bytes());
    }
    d[LEGS_LIFTED_OFFSET] = u8::from(lifted);
    d
}

fn read(key: &Pubkey, owner: &Pubkey, data: &mut [u8], mint: &Pubkey) -> Result<LegPools> {
    let mut lamports = 1u64;
    let info = AccountInfo::new(key, false, false, &mut lamports, data, owner, false);
    LegPools::read(&info, mint)
}

#[test]
fn a_legs_is_read_in_place_and_bound_to_its_mint() {
    let mint = Pubkey::new_unique();
    let pools = [pda(), pda(), pda()];
    let key = Pubkey::new_unique();
    let mut d = legs_data(&mint, &pools, false);
    let got = read(&key, &LAUNCH_ID, &mut d, &mint).unwrap();
    assert_eq!((got.count, got.lifted), (3, false));
    assert!(pools.iter().all(|p| got.has(p)));
    assert!(
        !got.has(&Pubkey::default()),
        "the unused legs' zero keys are no pools"
    );
    // Every leg open: trading opened with the last (the latest `opened_at`).
    assert_eq!((got.all_open, got.last_opened_at), (true, 1_002));
    let mut d = legs_data(&mint, &pools, true);
    assert!(read(&key, &LAUNCH_ID, &mut d, &mint).unwrap().lifted);
    // Two of three open: trading has not opened.
    let mut d = legs_data(&mint, &pools, false);
    d[LEGS_OPENED_OFFSET] = 2;
    let at = LEGS_FIRST_LEG_OFFSET + 2 * LEGS_LEG_LEN + LEGS_LEG_OPENED_AT_OFFSET;
    d[at..at + 8].copy_from_slice(&0i64.to_le_bytes());
    let got = read(&key, &LAUNCH_ID, &mut d, &mint).unwrap();
    assert_eq!((got.all_open, got.last_opened_at), (false, 1_001));
    assert_eq!(
        got.delay(900, 2_000),
        1_100,
        "the locks wait: they run from now"
    );
    // Another program's account, another mint's, another account kind, a short one.
    let mut d = legs_data(&mint, &pools, false);
    assert_eq!(
        code(read(&key, &crate::ID, &mut d, &mint)),
        kit(KitError::WrongLegs)
    );
    assert_eq!(
        code(read(&key, &LAUNCH_ID, &mut d, &Pubkey::new_unique())),
        kit(KitError::WrongLegs)
    );
    let mut d = legs_data(&mint, &pools, false);
    d[0] ^= 1;
    assert_eq!(
        code(read(&key, &LAUNCH_ID, &mut d, &mint)),
        kit(KitError::WrongLegs)
    );
    let mut d = legs_data(&mint, &pools, false);
    d.truncate(LEGS_LEN - 1);
    assert_eq!(
        code(read(&key, &LAUNCH_ID, &mut d, &mint)),
        kit(KitError::WrongLegs)
    );
}

#[test]
fn every_config_made_before_reads_as_a_sol_coins() {
    // `reserved` all zero: neither native nor quoted in tokens.
    let k = setup(modules::ALL);
    assert!(!k.config.is_token_quoted() && !k.config.is_native());
    let mut c = k.config.clone();
    c.set_native(Some(42));
    c.set_token_quoted();
    assert!(c.is_native() && c.is_token_quoted());
    assert_eq!(c.lamports_base(), 42, "the flags keep their own bytes");
    assert_eq!(c.reserved[QUOTE_KIND_OFFSET], QUOTE_KIND_TOKENS);
    assert!(c.reserved[QUOTE_KIND_OFFSET + 1..].iter().all(|b| *b == 0));
    // On the wire: 431 bytes, `graduated` at 12, the native flag at 376, its base at 377..385, the
    // quote kind at 385 (what clients read).
    let mut data = Vec::new();
    anchor_lang::AccountSerialize::try_serialize(&c, &mut data).unwrap();
    assert_eq!((data.len(), KitConfig::LEN), (431, 431));
    assert_eq!((data[376], data[385]), (1, QUOTE_KIND_TOKENS));
    assert_eq!(u64::from_le_bytes(data[377..385].try_into().unwrap()), 42);
    c.graduated = true;
    let mut data = Vec::new();
    anchor_lang::AccountSerialize::try_serialize(&c, &mut data).unwrap();
    assert_eq!(data[12], 1);
}
