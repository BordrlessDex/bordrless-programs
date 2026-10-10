//! Vectors for the SDK's mirror of `hook_vault` (`docs/phase3a.md` §5), as `phase3a_vectors.rs`
//! renders the timelock's: for fixed keys, every instruction exactly as `hook_vault::client` builds
//! it, a `Vault` as the program serializes it, the addresses and the bounds. When the file differs
//! from what the Rust reference computes, the test rewrites it and fails. The SDK keeps a copy
//! (`packages/sdk/vectors/vault.json`) and holds `vault.ts` to it.

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::AccountSerialize;
use bordrless_launch::client::{CustomHookAccounts, LaunchKeys};
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::json::*;
use hook_vault::client as vc;
use hook_vault::constants::*;
use hook_vault::instructions::{CreateVaultArgs, SlotArgs};
use hook_vault::state::{Slot, Vault};

/// A fixed key: 32 bytes of `n`.
fn fixed(n: u8) -> Pubkey {
    Pubkey::new_from_array([n; 32])
}

const LP_FEE_BPS: u16 = 30;

/// The vault every builder reads: a burn slot, a sale to `wallet`, and a buy of `x_pool` whose
/// token's hook may cut up to 3%; slot 2 holds SOL for later, and the vault is open.
fn sample_vault(mint: Pubkey, hook: Pubkey, wallet: Pubkey, x_pool: Pubkey) -> Vault {
    let slot = |i: u8, policy: u8, target: Pubkey, max_cut_bps: u16| Slot {
        policy,
        target,
        owner_bump: Vault::slot_owner(&mint, i).1,
        max_cut_bps,
        ..Slot::default()
    };
    let mut slots = [
        slot(0, policy::BURN, Pubkey::default(), 0),
        slot(1, policy::SELL_FOR_SOL, wallet, 0),
        slot(2, policy::SELL_BUY_BURN, x_pool, 300),
    ];
    slots[0].burned = 5_000_000_000_000;
    slots[0].last_at = 1_800_000_100;
    slots[1].last_at = 1_800_000_200;
    slots[1].waited_at = 1_800_000_260;
    slots[1].reference_price = 1_234_567_890_123;
    slots[1].reference_at = 1_800_000_200;
    slots[1].sold = 9_000_000_000_000;
    slots[1].sol_out = 250_000_000;
    slots[1].bounties = 1_250_000;
    slots[2].reference_price = 1_111_111_111_111;
    slots[2].reference_at = 1_800_000_300;
    slots[2].last_at = 1_800_000_300;
    slots[2].pending_sol = 123_456_789;
    slots[2].buy_reference = 2_222_222_222;
    slots[2].buy_reference_at = 1_800_000_000;
    slots[2].buy_last_at = 1_800_000_050;
    slots[2].buy_waited_at = 1_800_000_110;
    slots[2].x_burned = 77_000_000;
    Vault {
        version: VERSION,
        bump: Vault::address(&mint).1,
        mint,
        hook,
        creator: fixed(1),
        n_slots: 3,
        slots,
        bounty_bps: 50,
        max_sell_bps: 100,
        interval: 3_600,
        max_hook_cut_bps: 200,
        opened: true,
        opened_at: 1_800_000_000,
        created_at: 1_799_999_000,
        last_activity_at: 1_800_000_300,
        reserved: [0; 64],
    }
}

fn metas(list: &[AccountMeta]) -> J {
    J::Arr(
        list.iter()
            .map(|m| {
                J::Arr(vec![
                    key(&m.pubkey),
                    J::Bool(m.is_signer),
                    J::Bool(m.is_writable),
                ])
            })
            .collect(),
    )
}

fn launch_keys(k: &LaunchKeys) -> J {
    J::Obj(vec![
        ("mint", key(&k.mint)),
        ("quoteMint", key(&k.quote_mint)),
        ("lpFeeBps", num(k.lp_fee_bps)),
        ("modules", num(k.modules)),
        ("burns", J::Bool(k.burns)),
    ])
}

fn vault_vectors() -> Vec<(&'static str, J)> {
    let (payer, mint, hook, cranker, wallet) = (fixed(1), fixed(2), fixed(3), fixed(4), fixed(5));
    let sol = BRIDGED_SOL_MINT;
    // The coin: a launch with a custom hook (its keys; the hook's extras as a client resolved them
    // for the slot's sale and for its burn).
    let coin = LaunchKeys {
        custom_hook: Some(hook),
        ..LaunchKeys::new(mint, sol, LP_FEE_BPS, &LaunchRules::NONE)
    };
    let sale_hook = CustomHookAccounts {
        program: hook,
        extras: vec![
            AccountMeta::new(fixed(6), false),
            AccountMeta::new_readonly(fixed(7), false),
        ],
    };
    let burn_hook = CustomHookAccounts {
        program: hook,
        extras: vec![AccountMeta::new(fixed(6), false)],
    };
    // The token bought: a kit token (max wallet), a plain one, and one with its own hook.
    let kit_rules = LaunchRules {
        max_wallet_bps: 200,
        burn_buy_bps: 100,
        burn_sell_bps: 100,
        ..LaunchRules::NONE
    };
    let x_kit = LaunchKeys::new(fixed(10), sol, LP_FEE_BPS, &kit_rules);
    let x_plain = LaunchKeys::new(fixed(11), sol, LP_FEE_BPS, &LaunchRules::NONE);
    let x_hook = fixed(12);
    let x_custom = LaunchKeys {
        custom_hook: Some(x_hook),
        ..LaunchKeys::new(fixed(13), sol, LP_FEE_BPS, &LaunchRules::NONE)
    };
    let x_transfer = CustomHookAccounts {
        program: x_hook,
        extras: vec![AccountMeta::new(fixed(14), false)],
    };
    let x_burn = CustomHookAccounts {
        program: x_hook,
        extras: vec![AccountMeta::new_readonly(fixed(15), false)],
    };
    let x_pool = x_kit.pool();
    let vault = sample_vault(mint, hook, wallet, x_pool);
    let args = CreateVaultArgs {
        hook,
        slots: vec![
            SlotArgs {
                policy: policy::BURN,
                target: Pubkey::default(),
                max_cut_bps: 0,
            },
            SlotArgs {
                policy: policy::SELL_FOR_SOL,
                target: wallet,
                max_cut_bps: 0,
            },
            SlotArgs {
                policy: policy::SELL_BUY_BURN,
                target: x_pool,
                max_cut_bps: 300,
            },
        ],
        bounty_bps: 50,
        max_sell_bps: 100,
        interval: 3_600,
        max_hook_cut_bps: 200,
    };
    let mut no_pending = sample_vault(mint, hook, wallet, x_pool);
    no_pending.slots[2].pending_sol = 0;
    let instructions = J::Arr(vec![
        ix(
            "createVault",
            &vc::create_vault(payer, mint, args, &[x_kit.mint]),
        ),
        ix("openVault", &vc::open_vault(payer, &vault, &coin)),
        ix(
            "executeBurn",
            &vc::execute(cranker, &vault, 0, &coin, &burn_hook),
        ),
        ix(
            "executeSellForSol",
            &vc::execute(cranker, &vault, 1, &coin, &sale_hook),
        ),
        ix(
            "executeSellToBuy",
            &vc::execute(cranker, &vault, 2, &coin, &sale_hook),
        ),
        ix(
            "executeBuyKit",
            &vc::execute_buy(
                cranker,
                &vault,
                2,
                &x_kit,
                vc::BuyHook::Kit { rewards: false },
            ),
        ),
        ix(
            "executeBuyPlain",
            &vc::execute_buy(
                cranker,
                &vault,
                2,
                &x_plain,
                vc::BuyHook::Kit { rewards: false },
            ),
        ),
        ix(
            "executeBuyCustom",
            &vc::execute_buy(
                cranker,
                &vault,
                2,
                &x_custom,
                vc::BuyHook::Custom {
                    transfer: &x_transfer,
                    burn: &x_burn,
                },
            ),
        ),
        ix(
            "retireSolAndCoin",
            &vc::retire(cranker, &vault, 2, true, Some(&burn_hook)),
        ),
        ix(
            "retireSolOnly",
            &vc::retire(cranker, &vault, 2, false, None),
        ),
        ix(
            "retireCoinOnly",
            &vc::retire(cranker, &no_pending, 1, true, Some(&burn_hook)),
        ),
    ]);
    let mut data = Vec::new();
    vault.try_serialize(&mut data).expect("serialize");
    assert_eq!(data.len(), Vault::LEN);
    vec![
        (
            "keys",
            J::Obj(vec![
                ("payer", key(&payer)),
                ("mint", key(&mint)),
                ("hook", key(&hook)),
                ("cranker", key(&cranker)),
                ("wallet", key(&wallet)),
                ("xPool", key(&x_pool)),
                ("xHook", key(&x_hook)),
            ]),
        ),
        (
            "launches",
            J::Obj(vec![
                ("coin", launch_keys(&coin)),
                ("xKit", launch_keys(&x_kit)),
                ("xPlain", launch_keys(&x_plain)),
                ("xCustom", launch_keys(&x_custom)),
            ]),
        ),
        (
            "hooks",
            J::Obj(vec![
                ("sale", metas(&sale_hook.extras)),
                ("burn", metas(&burn_hook.extras)),
                ("xTransfer", metas(&x_transfer.extras)),
                ("xBurn", metas(&x_burn.extras)),
            ]),
        ),
        (
            "constants",
            J::Obj(vec![
                ("programId", key(&hook_vault::ID)),
                ("eventAuthority", key(&vc::event_authority())),
                ("vaultAddress", key(&vc::vault_address(&mint))),
                (
                    "slotOwners",
                    J::Arr((0..3u8).map(|i| key(&vc::slot_owner(&mint, i))).collect()),
                ),
                (
                    "slotHoldings",
                    J::Arr((0..3u8).map(|i| key(&vc::slot_holding(&mint, i))).collect()),
                ),
                ("vaultLen", num(Vault::LEN as i64)),
                ("maxSlots", num(MAX_SLOTS as i64)),
                ("maxBountyBps", num(MAX_BOUNTY_BPS)),
                ("maxSellBps", num(MAX_SELL_BPS)),
                ("minInterval", num(MIN_INTERVAL)),
                ("maxInterval", num(MAX_INTERVAL)),
                ("maxHookCutBps", num(MAX_HOOK_CUT_BPS)),
                ("retireSecs", num(RETIRE_SECS)),
                ("incinerator", key(&INCINERATOR)),
            ]),
        ),
        ("instructions", instructions),
        (
            "accounts",
            J::Arr(vec![J::Obj(vec![
                ("address", key(&vc::vault_address(&mint))),
                ("data", hex(&data)),
            ])]),
        ),
    ]
}

#[test]
fn the_vault_vectors_are_current() {
    check_vectors("vault.json", &vault_vectors());
}
