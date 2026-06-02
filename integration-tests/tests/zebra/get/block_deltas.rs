//! Integration tests for the `getblockdeltas` RPC method on the State backend.
//!
//! # Regression coverage
//!
//! These tests cover AP-03 / Zellic finding #48500: the State backend used to
//! silently drop every *non-coinbase transparent spend input* from
//! `get_block_deltas`, because the verbosity-2 transaction object from Zebra's
//! stateless `TransactionObject::from_transaction` leaves an input's
//! value/address unset. Clients saw outputs (credits) but never inputs
//! (debits), so computed balances overstated funds.
//!
//! The fix resolves each spend's prevout via Zebra's `ReadStateService`
//! `Transaction` request and reads the spent output's value/address. The tests
//! below build a chain containing a known transparent spend and assert the
//! resulting `InputDelta` is present and correct.

use super::*;

/// Value, in zatoshis, the faucet pays the recipient's transparent address.
/// Chosen so the funding output is uniquely identifiable by its amount.
const FUNDING_AMOUNT: u64 = 250_000;

/// Drives a regtest chain to the point where the recipient has spent a single
/// transparent output it received from the faucet, then returns:
/// - the hash of the block containing the *funding* tx (faucet → recipient),
/// - the funding txid,
/// - the hash of the block containing the *spend* tx (recipient shields the
///   received transparent output).
#[allow(deprecated)] // StateService
async fn setup_transparent_spend(
    test_manager: &mut TestManager<Zebrad, StateService>,
    state_service_subscriber: &StateServiceSubscriber,
) -> (String, String, String) {
    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");
    let recipient_taddr = clients.get_recipient_address("transparent").await;

    // Mine 100 blocks so the faucet's coinbase matures, then shield it (coinbase
    // outputs cannot be spent to a transparent address). Mine 5 more so the
    // shielded funds are confirmed and spendable.
    clients.faucet.sync_and_await().await.unwrap();
    test_manager
        .generate_blocks_and_wait_for_tip(100, state_service_subscriber)
        .await;
    clients.faucet.sync_and_await().await.unwrap();
    clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
    test_manager
        .generate_blocks_and_wait_for_tip(5, state_service_subscriber)
        .await;
    clients.faucet.sync_and_await().await.unwrap();

    // Funding block: faucet creates a non-coinbase transparent output paying the
    // recipient's t-address.
    let funding_txid = from_inputs::quick_send(
        &mut clients.faucet,
        vec![(recipient_taddr.as_str(), FUNDING_AMOUNT, None)],
    )
    .await
    .unwrap()
    .first()
    .to_string();
    test_manager
        .generate_blocks_and_wait_for_tip(1, state_service_subscriber)
        .await;
    let funding_block_hash = state_service_subscriber
        .get_best_blockhash()
        .await
        .unwrap()
        .hash()
        .to_string();

    // Mine 5 more blocks so the recipient's transparent output is confirmed and
    // spendable, then sync the recipient.
    test_manager
        .generate_blocks_and_wait_for_tip(5, state_service_subscriber)
        .await;
    clients.recipient.sync_and_await().await.unwrap();

    // Spend block: recipient shields the transparent output it received. The
    // shielding tx spends that output, producing a non-coinbase transparent
    // input referencing the funding output.
    clients
        .recipient
        .quick_shield(AccountId::ZERO)
        .await
        .unwrap();
    test_manager
        .generate_blocks_and_wait_for_tip(1, state_service_subscriber)
        .await;
    let spend_block_hash = state_service_subscriber
        .get_best_blockhash()
        .await
        .unwrap()
        .hash()
        .to_string();

    test_manager.clients = Some(clients);

    (funding_block_hash, funding_txid, spend_block_hash)
}

/// The spend block's `InputDelta` is present and resolves to the funding
/// output's address and full (negative) value. Pre-fix this input was silently
/// dropped and `inputs` was empty.
#[allow(deprecated)]
#[tokio::test(flavor = "multi_thread")]
async fn resolves_transparent_spend_input() {
    let (
        mut test_manager,
        _fetch_service,
        _fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = super::create_test_manager_and_services::<Zebrad>(
        &ValidatorKind::Zebrad,
        None,
        true,
        true,
        None,
    )
    .await;

    let (funding_block_hash, funding_txid, spend_block_hash) =
        setup_transparent_spend(&mut test_manager, &state_service_subscriber).await;

    // Find the funding output paying the recipient (unique by amount). Its
    // address is derived from the verbosity-2 object's scriptPubKey — a path
    // independent of the new input-resolution code, so comparing the two is a
    // meaningful cross-check.
    let funding_deltas = state_service_subscriber
        .get_block_deltas(funding_block_hash)
        .await
        .unwrap();
    let funding_output = funding_deltas
        .deltas
        .iter()
        .find(|d| d.txid == funding_txid)
        .expect("funding tx should be in its block")
        .outputs
        .iter()
        .find(|o| o.satoshis.zatoshis() == FUNDING_AMOUNT as i64)
        .expect("funding output paying the recipient should be present");
    let funding_vout = funding_output.index;
    let funding_address = funding_output.address.clone();

    // The spend's input must be present and resolved to the funding output's
    // address and full value (negative, because it is a debit).
    let spend_deltas = state_service_subscriber
        .get_block_deltas(spend_block_hash)
        .await
        .unwrap();
    let input = spend_deltas
        .deltas
        .iter()
        .flat_map(|d| d.inputs.iter())
        .find(|i| i.prevtxid == funding_txid && i.prevout == funding_vout)
        .expect("spend input referencing the funding output should be present");

    assert_eq!(
        input.address, funding_address,
        "input must resolve to the prevout's address"
    );
    assert_eq!(
        input.satoshis.zatoshis(),
        -(FUNDING_AMOUNT as i64),
        "input must resolve to the prevout's full value, negated"
    );

    test_manager.close().await;
}

/// A shielded/coinbase-only block is unchanged: the coinbase input is skipped
/// and no transparent input deltas are fabricated.
#[allow(deprecated)]
#[tokio::test(flavor = "multi_thread")]
async fn coinbase_only_block_has_no_input_deltas() {
    let (
        mut test_manager,
        _fetch_service,
        fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = super::create_test_manager_and_services::<Zebrad>(
        &ValidatorKind::Zebrad,
        None,
        true,
        false,
        Some(NetworkKind::Regtest),
    )
    .await;

    // A freshly generated block contains only its coinbase transaction.
    super::generate_blocks_and_poll_all_chain_indexes(
        1,
        &test_manager,
        fetch_service_subscriber.clone(),
        state_service_subscriber.clone(),
    )
    .await;

    let block_hash = state_service_subscriber
        .get_best_blockhash()
        .await
        .unwrap()
        .hash()
        .to_string();

    let deltas = state_service_subscriber
        .get_block_deltas(block_hash)
        .await
        .unwrap();

    assert!(
        deltas.deltas.iter().all(|d| d.inputs.is_empty()),
        "a coinbase-only block must have no input deltas"
    );

    test_manager.close().await;
}
