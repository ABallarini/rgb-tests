//! Regression test for the insufficient-fee boundary in rgb-lib's `send_begin`.
//!
//! With two allocations of the same asset, 11 and 12 units, the first one on a
//! 350 sat UTXO and the second on a 5000 sat UTXO, a transfer of exactly 11
//! units at 10 sat/vB has to be able to use the colored 5000 sat UTXO to pay the
//! fee: it has free allocation slots, so it can pay; the 12 units it carries
//! move to RGB change.
//!
//! On the current rgb-lib `next` this fails with `InsufficientAllocationSlots`,
//! because the fee top-up in `try_prepare_psbt` only accepts UTXOs without RGB
//! allocations (`get_available_allocations(..., Some(0))`). With that limit
//! removed (`None`) the transfer succeeds and its PSBT spends the 350 sat and
//! the 5000 sat UTXOs.
//!
//! Run with the electrum profile, from the repository root:
//!
//! ```sh
//! INDEXER=electrum cargo test --test insufficient_fee -- --nocapture
//! ```

pub mod utils;

use std::collections::HashMap;

use rgb_lib::bdk_wallet::bitcoin::psbt::Psbt;
use rgb_lib::keys::{WitnessVersion, generate_keys};
use rgb_lib::wallet::*;
use rgb_lib::{AssetSchema, Assignment, BitcoinNetwork, Encryption, Error};

use utils::*;

const FEE_RATE: u64 = 10;

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn new_wallet(name: &str) -> (Wallet, Online) {
    let keys = generate_keys(BitcoinNetwork::Regtest, WitnessVersion::Taproot);
    let data_dir = format!("test-data/insufficient-fee/{name}");
    let _ = std::fs::remove_dir_all(&data_dir);
    std::fs::create_dir_all(&data_dir).unwrap();
    let mut wallet = Wallet::new(
        WalletData {
            data_dir,
            bitcoin_network: BitcoinNetwork::Regtest,
            database_type: DatabaseType::Sqlite,
            max_allocations_per_utxo: 5,
            supported_schemas: vec![AssetSchema::Nia],
            allow_reckless_schemas: false,
        },
        SinglesigKeys::from_keys(&keys, None),
    )
    .unwrap();
    let online = wallet
        .go_online(OnlineOptions {
            indexer_config: IndexerConfig::Electrum {
                url: indexer_url(INSTANCE_1, Network::Regtest),
            },
            skip_consistency_check: true,
            vanilla_sync_lookback: 20,
            failed_witness_receive_grace_secs: 0,
            proxy_auth_keys: HashMap::new(),
            evm_chains: HashMap::new(),
        })
        .unwrap();
    (wallet, online)
}

fn sync(wallet: &mut Wallet, online: Online) {
    wallet
        .sync(
            online,
            SyncOptions {
                keychain: SyncKeychain::Vanilla { lookback: 20 },
                strategy: SyncStrategy::FastSync,
            },
        )
        .unwrap();
    wallet
        .sync(
            online,
            SyncOptions {
                keychain: SyncKeychain::Colored,
                strategy: SyncStrategy::FastSync,
            },
        )
        .unwrap();
}

fn create_utxos(wallet: &mut Wallet, online: Online, size: u32) {
    let psbt = wallet
        .create_utxos_begin(online, false, Some(1), Some(size), 1, false, false)
        .unwrap();
    let signed = wallet.sign_psbt(psbt, None).unwrap();
    wallet.create_utxos_end(online, signed).unwrap();
    mine(false);
}

fn recipient(wallet: &mut Wallet, online: Online, amount: u64) -> Recipient {
    let receive = wallet
        .blind_receive(
            Some(online),
            None,
            Assignment::Fungible(amount),
            now() + 7200,
            vec![],
            1,
            Some(Encryption::Optional),
        )
        .unwrap();
    Recipient {
        recipient_id: receive.recipient_id,
        witness_data: None,
        assignment: Assignment::Fungible(amount),
        transport_endpoints: vec![],
        public_key: None,
        encryption: Encryption::Optional,
        expiration_timestamp: None,
    }
}

struct Pair {
    sender: Wallet,
    sender_online: Online,
    receiver: Wallet,
    receiver_online: Online,
    asset_id: String,
    /// The outpoint of the sender's 5000 sat UTXO, holding `Fungible(12)`.
    utxo_5000: rgb_lib::wallet::Outpoint,
}

/// An independent sender/receiver pair with 11 and 12 units of one asset on a
/// 350 sat and a 5000 sat colored UTXO respectively.
fn setup_pair(name: &str) -> Pair {
    let (mut sender, sender_online) = new_wallet(&format!("{name}-sender"));
    let (mut receiver, receiver_online) = new_wallet(&format!("{name}-receiver"));

    // the receiver needs a colorable UTXO for each blinded invoice
    fund_wallet(receiver.get_address().unwrap(), Some(6_000), INSTANCE_1);
    sync(&mut receiver, receiver_online);
    for _ in 0..3 {
        create_utxos(&mut receiver, receiver_online, 400);
    }

    // the sender funds only the two colorable UTXOs: the small plain change
    // left cannot cover the fee of the transfer
    fund_wallet(sender.get_address().unwrap(), Some(6_000), INSTANCE_1);
    sync(&mut sender, sender_online);
    create_utxos(&mut sender, sender_online, 350);
    create_utxos(&mut sender, sender_online, 5_000);

    let asset = sender
        .issue_asset_nia(
            "FEETEST".to_string(),
            "Fee regression".to_string(),
            0,
            vec![11, 12],
        )
        .unwrap();

    let unspents = sender
        .list_unspents(Some(sender_online), false, false)
        .unwrap();
    let small = unspents
        .iter()
        .find(|u| u.utxo.btc_amount == 350)
        .expect("missing the 350 sat UTXO");
    assert_eq!(small.rgb_allocations.len(), 1);
    assert_eq!(
        small.rgb_allocations[0].assignment,
        Assignment::Fungible(11)
    );
    let large = unspents
        .iter()
        .find(|u| u.utxo.btc_amount == 5_000)
        .expect("missing the 5000 sat UTXO");
    assert_eq!(large.rgb_allocations.len(), 1);
    assert_eq!(
        large.rgb_allocations[0].assignment,
        Assignment::Fungible(12)
    );

    Pair {
        sender,
        sender_online,
        receiver,
        receiver_online,
        asset_id: asset.asset_id,
        utxo_5000: large.utxo.outpoint.clone(),
    }
}

/// Out-of-band transfer of `amount` units: the recipient has no transport
/// endpoint and the sender no proxy.
fn send(pair: &mut Pair, amount: u64) -> Result<SendBeginResult, Error> {
    let recipient = recipient(&mut pair.receiver, pair.receiver_online, amount);
    pair.sender.send_begin(
        pair.sender_online,
        HashMap::from([(pair.asset_id.clone(), vec![recipient])]),
        false,
        FEE_RATE,
        1,
        now() + 3600,
        None,
        false,
    )
}

#[test]
fn send_exact_amount_uses_colored_utxo_for_fee() {
    initialize();
    if !matches!(INDEXER.get(), Some(utils::chain::Indexer::Electrum)) {
        println!("skipping: run with INDEXER=electrum");
        return;
    }

    let mut a = setup_pair("a");
    let mut b = setup_pair("b");

    // pair A: the 11 units sit on the 350 sat UTXO; the fee has to come from
    // the 5000 sat UTXO holding the 12 units
    let res = send(&mut a, 11);
    assert!(res.is_ok(), "11 unit transfer failed on pair A: {res:?}");

    let psbt = Psbt::from_str(&res.unwrap().psbt).expect("invalid PSBT");
    assert_eq!(
        psbt.unsigned_tx.input.len(),
        2,
        "expected the 350 sat colored UTXO and one fee UTXO, got {:?}",
        psbt.unsigned_tx
            .input
            .iter()
            .map(|input| input.previous_output)
            .collect::<Vec<_>>()
    );
    assert!(
        psbt.unsigned_tx.input.iter().any(|input| {
            input.previous_output.txid.to_string() == a.utxo_5000.txid
                && input.previous_output.vout == a.utxo_5000.vout
        }),
        "the transfer does not spend the 5000 sat UTXO {}: {:?}",
        a.utxo_5000,
        psbt.unsigned_tx
            .input
            .iter()
            .map(|input| input.previous_output)
            .collect::<Vec<_>>()
    );

    // pair B: the 12-unit control, matching the single 12-unit allocation
    let res = send(&mut b, 12);
    assert!(res.is_ok(), "12 unit transfer failed on pair B: {res:?}");

    let psbt = Psbt::from_str(&res.unwrap().psbt).expect("invalid PSBT");
    assert!(
        psbt.unsigned_tx.input.iter().any(|input| {
            input.previous_output.txid.to_string() == b.utxo_5000.txid
                && input.previous_output.vout == b.utxo_5000.vout
        }),
        "the transfer does not spend the 5000 sat UTXO {}: {:?}",
        b.utxo_5000,
        psbt.unsigned_tx
            .input
            .iter()
            .map(|input| input.previous_output)
            .collect::<Vec<_>>()
    );
}
