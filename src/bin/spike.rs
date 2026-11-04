use std::collections::VecDeque;
use std::net::TcpListener;
use std::str::FromStr;
use std::time::Duration;

use corepc_node::{Conf, Node as Bitcoind};
use ldk_node::bitcoin::{Address, Amount, Network, Txid};
use ldk_node::config::Config;
use ldk_node::entropy::NodeEntropy;
use ldk_node::lightning::ln::channelmanager::PaymentId;
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::lightning_invoice::{Bolt11InvoiceDescription, Description};
use ldk_node::lightning_types::payment::{PaymentHash, PaymentPreimage};
use ldk_node::{Builder, Event, Node, PendingSweepBalance, UserChannelId};
use rand::RngCore;

// About 12 hours of mainnet blocks: enough to hold a bet through a match.
const CLTV_DELTA: u16 = 72;
// About 8 hours: how long we let the first bet sit pending before resolving it.
const MATCH_BLOCKS: usize = 48;
const EVENT_TIMEOUT: Duration = Duration::from_secs(60);

struct Chain {
    bitcoind: Bitcoind,
    miner: Address,
}

impl Chain {
    fn start() -> Chain {
        let exe = corepc_node::exe_path().expect("put bitcoind on PATH or set BITCOIND_EXE");
        let mut conf = Conf::default();
        conf.network = "regtest";
        let bitcoind = Bitcoind::with_conf(exe, &conf).unwrap();
        let miner = bitcoind.client.new_address().unwrap();
        let chain = Chain { bitcoind, miner };
        chain.mine(101);
        chain
    }

    fn mine(&self, blocks: usize) {
        self.bitcoind
            .client
            .generate_to_address(blocks, &self.miner)
            .unwrap();
    }

    fn height(&self) -> u32 {
        self.bitcoind.client.get_blockchain_info().unwrap().blocks as u32
    }

    fn fund(&self, address: &Address, sats: u64) {
        self.bitcoind
            .client
            .send_to_address(address, Amount::from_sat(sats))
            .unwrap();
    }

    async fn wait_for_mempool(&self, txid: Txid) {
        for _ in 0..100 {
            let mempool = self.bitcoind.client.get_raw_mempool().unwrap().0;
            if mempool.contains(&txid.to_string()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("{txid} never reached the mempool");
    }
}

struct Party {
    name: &'static str,
    node: Node,
    backlog: VecDeque<Event>,
}

impl Party {
    fn start(name: &'static str, chain: &Chain, run_dir: &str) -> Party {
        let mut config = Config::default();
        config.storage_dir_path = format!("{run_dir}/{name}");
        config.network = Network::Regtest;
        config.listening_addresses = Some(vec![free_local_address()]);
        config.manually_handle_unknown_bolt11_payments = true;

        let mut builder = Builder::from_config(config);
        let rpc = chain.bitcoind.params.rpc_socket;
        let cookie = chain.bitcoind.params.get_cookie_values().unwrap().unwrap();
        builder.set_chain_source_bitcoind_rpc(
            rpc.ip().to_string(),
            rpc.port(),
            cookie.user,
            cookie.password,
            None,
        );
        let mut seed = [0u8; 64];
        rand::rng().fill_bytes(&mut seed);
        let node = builder.build(NodeEntropy::from_seed_bytes(seed)).unwrap();
        node.start().unwrap();
        Party {
            name,
            node,
            backlog: VecDeque::new(),
        }
    }

    // Events can arrive in any order, so keep the ones we aren't waiting for yet.
    async fn expect(&mut self, what: &str, wanted: impl Fn(&Event) -> bool) -> Event {
        if let Some(i) = self.backlog.iter().position(&wanted) {
            return self.backlog.remove(i).unwrap();
        }
        loop {
            let event = tokio::time::timeout(EVENT_TIMEOUT, self.node.next_event_async())
                .await
                .unwrap_or_else(|_| panic!("{}: timed out waiting for {what}", self.name));
            self.node.event_handled().unwrap();
            if wanted(&event) {
                return event;
            }
            self.backlog.push_back(event);
        }
    }

    fn lightning_sats(&self) -> u64 {
        self.node.list_balances().total_lightning_balance_sats
    }

    fn onchain_sats(&self) -> u64 {
        self.node.list_balances().total_onchain_balance_sats
    }

    fn has_pending_htlcs(&self) -> bool {
        self.node
            .list_channels()
            .iter()
            .any(|c| !c.pending_inbound_htlcs.is_empty() || !c.pending_outbound_htlcs.is_empty())
    }

    // The sweeper keeps listing confirmed sweeps long after they are final.
    fn is_fully_swept(&self) -> bool {
        let balances = self.node.list_balances();
        balances.lightning_balances.is_empty()
            && balances
                .pending_balances_from_channel_closures
                .iter()
                .all(|b| {
                    matches!(
                        b,
                        PendingSweepBalance::AwaitingThresholdConfirmations { .. }
                    )
                })
    }
}

fn free_local_address() -> SocketAddress {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    SocketAddress::from_str(&format!("127.0.0.1:{port}")).unwrap()
}

fn sync(parties: &[&Party]) {
    for party in parties {
        party.node.sync_wallets().unwrap();
    }
}

struct OracleBet {
    yes: PaymentPreimage,
    no: PaymentPreimage,
}

impl OracleBet {
    fn new() -> OracleBet {
        let mut yes = [0u8; 32];
        let mut no = [0u8; 32];
        rand::rng().fill_bytes(&mut yes);
        rand::rng().fill_bytes(&mut no);
        OracleBet {
            yes: PaymentPreimage(yes),
            no: PaymentPreimage(no),
        }
    }

    fn yes_hash(&self) -> PaymentHash {
        PaymentHash::from(self.yes)
    }

    fn no_hash(&self) -> PaymentHash {
        PaymentHash::from(self.no)
    }
}

// Alice backs yes, Bob backs no.
struct Bet {
    alice_in: PaymentId,
    alice_out: PaymentId,
    bob_in: PaymentId,
    bob_out: PaymentId,
}

async fn place_bet(
    chain: &Chain,
    alice: &mut Party,
    bob: &mut Party,
    oracle: &OracleBet,
    alice_stake: u64,
    bob_stake: u64,
) -> Bet {
    let description = Bolt11InvoiceDescription::Direct(Description::new("bet".into()).unwrap());
    let alice_invoice = alice
        .node
        .bolt11_payment()
        .receive_for_hash_with_min_final_cltv_expiry_delta(
            bob_stake * 1000,
            &description,
            3600,
            oracle.yes_hash(),
            CLTV_DELTA,
        )
        .unwrap();
    let bob_invoice = bob
        .node
        .bolt11_payment()
        .receive_for_hash_with_min_final_cltv_expiry_delta(
            alice_stake * 1000,
            &description,
            3600,
            oracle.no_hash(),
            CLTV_DELTA,
        )
        .unwrap();

    let bob_out = bob
        .node
        .bolt11_payment()
        .send(&alice_invoice, None)
        .unwrap();
    let yes_hash = oracle.yes_hash();
    let Event::PaymentClaimable { payment_id: alice_in, claim_deadline, .. } = alice
		.expect("Bob's stake", |e| matches!(e, Event::PaymentClaimable { payment_hash, .. } if *payment_hash == yes_hash))
		.await
	else {
		unreachable!()
	};

    let alice_out = alice
        .node
        .bolt11_payment()
        .send(&bob_invoice, None)
        .unwrap();
    let no_hash = oracle.no_hash();
    let Event::PaymentClaimable { payment_id: bob_in, .. } = bob
		.expect("Alice's stake", |e| matches!(e, Event::PaymentClaimable { payment_hash, .. } if *payment_hash == no_hash))
		.await
	else {
		unreachable!()
	};

    let height = chain.height();
    let deadline = claim_deadline.expect("held payment has a claim deadline");
    println!(
        "  both stakes held: Alice {alice_stake} sats on yes, Bob {bob_stake} sats on no; \
		 claim deadline {deadline}, {} blocks from now",
        deadline - height
    );
    assert!(
        deadline >= height + CLTV_DELTA as u32 - 3,
        "CLTV delta was not honoured"
    );

    Bet {
        alice_in,
        alice_out,
        bob_in,
        bob_out,
    }
}

async fn open_channel(chain: &Chain, alice: &mut Party, bob: &mut Party) -> UserChannelId {
    let bob_address = bob.node.listening_addresses().unwrap()[0].clone();
    alice
        .node
        .open_channel(
            bob.node.node_id(),
            bob_address,
            1_000_000,
            Some(500_000_000),
            None,
            None,
        )
        .unwrap();
    let Event::ChannelPending { funding_txo, .. } = alice
        .expect("channel pending", |e| {
            matches!(e, Event::ChannelPending { .. })
        })
        .await
    else {
        unreachable!()
    };
    bob.expect("channel pending", |e| {
        matches!(e, Event::ChannelPending { .. })
    })
    .await;
    chain.wait_for_mempool(funding_txo.txid).await;
    chain.mine(6);
    sync(&[alice, bob]);
    alice
        .expect("channel ready", |e| matches!(e, Event::ChannelReady { .. }))
        .await;
    let Event::ChannelReady {
        user_channel_id, ..
    } = bob
        .expect("channel ready", |e| matches!(e, Event::ChannelReady { .. }))
        .await
    else {
        unreachable!()
    };
    for _ in 0..300 {
        if alice.node.list_channels().iter().all(|c| c.is_usable)
            && bob.node.list_channels().iter().all(|c| c.is_usable)
        {
            return user_channel_id;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("channel never became usable");
}

async fn settle_cooperatively(chain: &Chain, alice: &mut Party, bob: &mut Party) {
    println!("\n== Bet 1: 200k each, held for {MATCH_BLOCKS} blocks, yes wins, settled off-chain");
    let alice_before = alice.lightning_sats();
    let bob_before = bob.lightning_sats();
    let oracle = OracleBet::new();
    let bet = place_bet(chain, alice, bob, &oracle, 200_000, 200_000).await;

    chain.mine(MATCH_BLOCKS);
    sync(&[alice, bob]);
    assert!(
        alice.has_pending_htlcs() && bob.has_pending_htlcs(),
        "stakes were released early"
    );
    println!("  {MATCH_BLOCKS} blocks later both stakes are still held");

    println!("  oracle reveals yes");
    alice
        .node
        .bolt11_payment()
        .claim_for_id(bet.alice_in, oracle.yes)
        .unwrap();
    bob.node.bolt11_payment().fail_for_id(bet.bob_in).unwrap();

    let (alice_in, alice_out, bob_out) = (bet.alice_in, bet.alice_out, bet.bob_out);
    alice
        .expect(
            "winnings",
            |e| matches!(e, Event::PaymentReceived { payment_id, .. } if *payment_id == alice_in),
        )
        .await;
    alice
        .expect(
            "own stake refunded",
            |e| matches!(e, Event::PaymentFailed { payment_id, .. } if *payment_id == alice_out),
        )
        .await;
    bob.expect(
        "stake paid out",
        |e| matches!(e, Event::PaymentSuccessful { payment_id, .. } if *payment_id == bob_out),
    )
    .await;
    for _ in 0..100 {
        if !alice.has_pending_htlcs() && !bob.has_pending_htlcs() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!alice.has_pending_htlcs() && !bob.has_pending_htlcs());

    let alice_after = alice.lightning_sats();
    let bob_after = bob.lightning_sats();
    println!("  Alice {alice_before} -> {alice_after}, Bob {bob_before} -> {bob_after}");
    assert_eq!(alice_after, alice_before + 200_000);
    assert_eq!(bob_after, bob_before - 200_000);
}

async fn settle_after_force_close(
    chain: &Chain,
    alice: &mut Party,
    bob: &mut Party,
    bob_channel: UserChannelId,
) {
    println!("\n== Bet 2: Alice 100k on yes vs Bob 200k on no, Bob force-closes, yes wins");
    let alice_expected = alice.onchain_sats() + alice.lightning_sats() + 200_000;
    let bob_expected = bob.onchain_sats() + bob.lightning_sats() - 200_000;
    let oracle = OracleBet::new();
    let bet = place_bet(chain, alice, bob, &oracle, 100_000, 200_000).await;

    bob.node
        .force_close_channel(&bob_channel, alice.node.node_id(), None)
        .unwrap();
    alice
        .expect("channel closed", |e| {
            matches!(e, Event::ChannelClosed { .. })
        })
        .await;
    bob.expect("channel closed", |e| {
        matches!(e, Event::ChannelClosed { .. })
    })
    .await;
    println!("  Bob force-closed with both stakes in flight");

    println!("  oracle reveals yes");
    alice
        .node
        .bolt11_payment()
        .claim_for_id(bet.alice_in, oracle.yes)
        .unwrap();
    println!(
        "  Bob rejecting his held payment: {:?}",
        bob.node.bolt11_payment().fail_for_id(bet.bob_in)
    );

    let start = chain.height();
    for _ in 0..60 {
        chain.mine(6);
        sync(&[alice, bob]);
        if alice.is_fully_swept() && bob.is_fully_swept() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(
        alice.is_fully_swept() && bob.is_fully_swept(),
        "channel funds never fully swept:\nAlice {:#?}\nBob {:#?}",
        alice.node.list_balances(),
        bob.node.list_balances()
    );
    println!(
        "  everything swept on-chain after {} blocks",
        chain.height() - start
    );

    let alice_after = alice.onchain_sats();
    let bob_after = bob.onchain_sats();
    println!(
		"  Alice expected ~{alice_expected}, got {alice_after} (fees {}); Bob expected ~{bob_expected}, got {bob_after} (fees {})",
		alice_expected as i64 - alice_after as i64,
		bob_expected as i64 - bob_after as i64,
	);
    // On-chain fees come out of both sides; the stakes are far larger than any fee.
    assert!(alice_after <= alice_expected && alice_expected - alice_after < 20_000);
    assert!(bob_after <= bob_expected && bob_expected - bob_after < 20_000);
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let chain = Chain::start();
    let mut run_id = [0u8; 4];
    rand::rng().fill_bytes(&mut run_id);
    let run_dir = std::env::temp_dir()
        .join(format!("lightning_bet_{:08x}", u32::from_le_bytes(run_id)))
        .to_string_lossy()
        .into_owned();

    let mut alice = Party::start("alice", &chain, &run_dir);
    let mut bob = Party::start("bob", &chain, &run_dir);
    chain.fund(
        &alice.node.onchain_payment().new_address().unwrap(),
        2_000_000,
    );
    chain.fund(
        &bob.node.onchain_payment().new_address().unwrap(),
        2_000_000,
    );
    chain.mine(6);
    sync(&[&alice, &bob]);

    let bob_channel = open_channel(&chain, &mut alice, &mut bob).await;
    println!(
        "channel open: Alice {} sats, Bob {} sats",
        alice.lightning_sats(),
        bob.lightning_sats()
    );

    settle_cooperatively(&chain, &mut alice, &mut bob).await;
    settle_after_force_close(&chain, &mut alice, &mut bob, bob_channel).await;

    alice.node.stop().unwrap();
    bob.node.stop().unwrap();
    println!("\nall good (node data in {run_dir})");
}
