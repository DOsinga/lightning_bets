use std::str::FromStr;
use std::thread::sleep;
use std::time::Duration;

use chrono::{NaiveDateTime, Utc};
use clap::{Parser, Subcommand};
use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::bitcoin::Network;
use ldk_node::config::Config;
use ldk_node::entropy::NodeEntropy;
use ldk_node::lightning::ln::channelmanager::PaymentId;
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescription, Description};
use ldk_node::lightning_types::payment::{PaymentHash, PaymentPreimage};
use ldk_node::payment::{PaymentDetails, PaymentDirection, PaymentKind, PaymentStatus};
use ldk_node::{Builder, Event, Node};
use serde_json::{json, Value};

const POLL: Duration = Duration::from_secs(5);
// How long after kickoff a match can take, including extra time and the oracle noticing.
const MATCH_SECS: i64 = 4 * 3600;

#[derive(Parser)]
struct Cli {
    #[arg(long, env = "BET_DATA_DIR")]
    data_dir: String,
    #[arg(long, env = "BET_NETWORK", default_value = "regtest")]
    network: Network,
    /// `bitcoind://user:pass@host:port` for Bitcoin Core RPC, otherwise an Esplora URL.
    #[arg(long, env = "BET_CHAIN")]
    chain: String,
    #[arg(long, env = "BET_LISTEN", default_value = "0.0.0.0:9735")]
    listen: String,
    #[arg(
        long,
        env = "BET_ORACLE",
        default_value = "https://douwe.com/projects/lightning_oracle/api"
    )]
    oracle: String,
    #[arg(long, env = "BET_BLOCK_SECS", default_value_t = 600)]
    block_secs: i64,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show node id, funding address, balances and channels.
    Info,
    /// Stay online, for instance while a friend opens a channel to you.
    Run,
    /// Open a channel to `node_id@host:port`, optionally pushing some of it to the peer.
    Open {
        peer: String,
        sats: u64,
        #[arg(long, default_value_t = 0)]
        push: u64,
    },
    /// Propose a bet on an ESPN match and wait for it to settle.
    Offer {
        #[arg(long)]
        league: String,
        #[arg(long)]
        event: String,
        /// home, away or draw: what "yes" means.
        #[arg(long)]
        outcome: String,
        /// yes or no: the side you take.
        #[arg(long)]
        side: String,
        #[arg(long)]
        stake: u64,
        #[arg(long)]
        their_stake: u64,
    },
    /// Take the other side of a bet and wait for it to settle.
    Accept { bet_id: String },
    /// Resume following a bet you are part of.
    Watch { bet_id: String },
}

fn start_node(cli: &Cli) -> Node {
    let mut config = Config::default();
    config.storage_dir_path = cli.data_dir.clone();
    config.network = cli.network;
    config.listening_addresses = Some(vec![SocketAddress::from_str(&cli.listen).unwrap()]);
    config.manually_handle_unknown_bolt11_payments = true;
    let mut builder = Builder::from_config(config);
    if let Some(rpc) = cli.chain.strip_prefix("bitcoind://") {
        let (auth, host_port) = rpc.split_once('@').expect("bitcoind://user:pass@host:port");
        let (user, pass) = auth
            .split_once(':')
            .expect("bitcoind://user:pass@host:port");
        let (host, port) = host_port
            .rsplit_once(':')
            .expect("bitcoind://user:pass@host:port");
        builder.set_chain_source_bitcoind_rpc(
            host.into(),
            port.parse().unwrap(),
            user.into(),
            pass.into(),
            None,
        );
    } else {
        builder.set_chain_source_esplora(cli.chain.clone(), None);
    }
    let entropy = NodeEntropy::from_seed_path(format!("{}/seed", cli.data_dir)).unwrap();
    let node = builder.build(entropy).unwrap();
    node.start().unwrap();
    node
}

fn oracle_get(cli: &Cli, path: &str) -> Value {
    ureq::get(&format!("{}/{path}", cli.oracle))
        .call()
        .unwrap()
        .into_json()
        .unwrap()
}

fn oracle_post(cli: &Cli, path: &str, body: Value) -> Value {
    match ureq::post(&format!("{}/{path}", cli.oracle)).send_json(body) {
        Ok(response) => response.into_json().unwrap(),
        Err(ureq::Error::Status(code, response)) => {
            panic!("oracle said {code}: {}", response.into_string().unwrap())
        }
        Err(e) => panic!("{e}"),
    }
}

fn other(side: &str) -> &'static str {
    if side == "yes" {
        "no"
    } else {
        "yes"
    }
}

fn hex32(hex: &str) -> [u8; 32] {
    let mut bytes = [0u8; 32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap();
    }
    bytes
}

fn hash_of(bet: &Value, side: &str) -> PaymentHash {
    PaymentHash(hex32(bet[format!("{side}_hash")].as_str().unwrap()))
}

fn preimage_of(bet: &Value) -> PaymentPreimage {
    PaymentPreimage(hex32(bet["winning_preimage"].as_str().unwrap()))
}

fn invoice_of(bet: &Value, side: &str) -> Option<Bolt11Invoice> {
    bet[format!("{side}_invoice")]
        .as_str()
        .map(|s| Bolt11Invoice::from_str(s).unwrap())
}

fn stake_of(bet: &Value, side: &str) -> u64 {
    bet["terms"]["stakes"][side].as_u64().unwrap()
}

fn my_side(bet: &Value, node: &Node) -> &'static str {
    for side in ["yes", "no"] {
        if invoice_of(bet, side).is_some_and(|i| i.recover_payee_pub_key() == node.node_id()) {
            return side;
        }
    }
    panic!("this node has no invoice in bet {}", bet["bet_id"]);
}

// Our invoice is for what we win: the other side's stake, locked to our side's hash.
fn post_invoice(cli: &Cli, node: &Node, bet: &Value, side: &str) {
    let kickoff =
        NaiveDateTime::parse_from_str(bet["terms"]["kickoff"].as_str().unwrap(), "%Y-%m-%dT%H:%MZ")
            .unwrap()
            .and_utc();
    let secs_to_kickoff = (kickoff - Utc::now()).num_seconds().max(0);
    let cltv_delta = (secs_to_kickoff + MATCH_SECS) / cli.block_secs + 6;
    assert!(
        cltv_delta < 1000,
        "kickoff is too far away to hold a payment until then"
    );
    let description = Bolt11InvoiceDescription::Direct(
        Description::new(bet["terms"]["question"].as_str().unwrap().into()).unwrap(),
    );
    let invoice = node
        .bolt11_payment()
        .receive_for_hash_with_min_final_cltv_expiry_delta(
            stake_of(bet, other(side)) * 1000,
            &description,
            secs_to_kickoff.max(600) as u32,
            hash_of(bet, side),
            cltv_delta as u16,
        )
        .unwrap();
    let bet_id = bet["bet_id"].as_str().unwrap();
    oracle_post(
        cli,
        &format!("bets/{bet_id}/invoice"),
        json!({"side": side, "invoice": invoice.to_string()}),
    );
    println!("posted our {side} invoice, held for up to {cltv_delta} blocks");
}

fn has_inbound_htlc(node: &Node, hash: PaymentHash) -> bool {
    node.list_channels().iter().any(|c| {
        c.pending_inbound_htlcs
            .iter()
            .any(|h| h.payment_hash == hash)
    })
}

fn outbound_status(node: &Node, hash: PaymentHash) -> Option<PaymentStatus> {
    node.payment(&PaymentId(hash.0)).unwrap().map(|p| p.status)
}

// Unlike outbound payments, a held inbound payment isn't stored under its payment hash.
fn inbound(node: &Node, hash: PaymentHash) -> Option<PaymentDetails> {
    let mut token = None;
    loop {
        let page = node.list_payments(token).unwrap();
        let found = page.payments.into_iter().find(|p| {
            p.direction == PaymentDirection::Inbound
                && matches!(p.kind, PaymentKind::Bolt11 { hash: h, .. } if h == hash)
        });
        if found.is_some() || page.next_page_token.is_none() {
            return found;
        }
        token = page.next_page_token;
    }
}

fn drain_events(node: &Node) {
    while let Some(event) = node.next_event() {
        println!("  event: {event:?}");
        node.event_handled().unwrap();
    }
}

// Everything is worked out from the oracle and the node's payment records, so this can be
// stopped and restarted at any point.
fn watch(cli: &Cli, node: &Node, bet_id: &str) {
    let mut last = String::new();
    loop {
        drain_events(node);
        let bet = oracle_get(cli, &format!("bets/{bet_id}"));
        let me = my_side(&bet, node);
        let them = other(me);
        let my_hash = hash_of(&bet, me);
        let their_hash = hash_of(&bet, them);
        let incoming = inbound(node, my_hash);
        let held = has_inbound_htlc(node, my_hash)
            && incoming
                .as_ref()
                .is_some_and(|p| p.status == PaymentStatus::Pending);
        let paid = outbound_status(node, their_hash);
        let bet_status = bet["status"].as_str().unwrap();

        let channel_ready = node.list_channels().iter().any(|c| c.is_usable);
        let summary = format!(
            "bet {bet_status}, channel usable: {channel_ready}, their stake held: {held}, our stake: {paid:?}"
        );
        if summary != last {
            println!("{summary}");
            last = summary;
        }

        if bet_status == "open" {
            let before_kickoff = bet["match_state"] == "pre";
            let can_pay = !matches!(
                paid,
                Some(PaymentStatus::Pending | PaymentStatus::Succeeded)
            );
            if held && can_pay && !before_kickoff {
                println!("match started before we paid; rejecting their stake, the bet is off");
                node.bolt11_payment()
                    .fail_for_id(incoming.as_ref().unwrap().id)
                    .unwrap();
            }
            let we_pay_first = bet["terms"]["offerer"] == them;
            if can_pay && channel_ready && before_kickoff && (we_pay_first || held) {
                if let Some(invoice) = invoice_of(&bet, them) {
                    assert_eq!(
                        invoice.payment_hash().0,
                        their_hash.0,
                        "their invoice isn't locked to the oracle's hash"
                    );
                    assert_eq!(
                        invoice.amount_milli_satoshis(),
                        Some(stake_of(&bet, me) * 1000),
                        "their invoice asks for the wrong stake"
                    );
                    println!("sending our {} sat stake", stake_of(&bet, me));
                    if let Err(e) = node.bolt11_payment().send(&invoice, None) {
                        println!("sending failed ({e:?}), trying again");
                    }
                }
            }
        } else {
            let won = bet["winner"] == me;
            if held {
                if won {
                    println!("we won, claiming their stake");
                    node.bolt11_payment()
                        .claim_for_id(incoming.as_ref().unwrap().id, preimage_of(&bet))
                        .unwrap();
                } else {
                    println!("we didn't win, returning their stake");
                    node.bolt11_payment()
                        .fail_for_id(incoming.as_ref().unwrap().id)
                        .unwrap();
                }
            } else if paid != Some(PaymentStatus::Pending) {
                let succeeded = Some(PaymentStatus::Succeeded);
                let result = if incoming.map(|p| p.status) == succeeded {
                    format!("we won {} sats", stake_of(&bet, them))
                } else if paid == succeeded {
                    format!("we lost {} sats", stake_of(&bet, me))
                } else {
                    format!("bet {bet_status}, no money changed hands")
                };
                println!("done: {result}");
                return;
            }
        }
        sleep(POLL);
    }
}

fn info(node: &Node) {
    let balances = node.list_balances();
    println!("node id:   {}", node.node_id());
    println!(
        "listening: {:?}",
        node.listening_addresses().unwrap_or_default()
    );
    println!(
        "fund at:   {}",
        node.onchain_payment().new_address().unwrap()
    );
    println!(
        "on-chain:  {} sats, lightning: {} sats",
        balances.total_onchain_balance_sats, balances.total_lightning_balance_sats
    );
    for c in node.list_channels() {
        println!(
            "channel with {}: can send {} sats, receive {} sats, usable: {}",
            c.counterparty.node_id,
            c.outbound_capacity_msat / 1000,
            c.inbound_capacity_msat / 1000,
            c.is_usable
        );
    }
}

fn open(node: &Node, peer: &str, sats: u64, push: u64) {
    let (node_id, address) = peer.split_once('@').expect("node_id@host:port");
    node.open_channel(
        PublicKey::from_str(node_id).unwrap(),
        SocketAddress::from_str(address).unwrap(),
        sats,
        Some(push * 1000),
        None,
        None,
    )
    .unwrap();
    loop {
        match node.wait_next_event() {
            Event::ChannelPending { funding_txo, .. } => {
                node.event_handled().unwrap();
                println!(
                    "channel funding in {}; usable after confirmations",
                    funding_txo.txid
                );
                return;
            }
            event => {
                println!("  event: {event:?}");
                node.event_handled().unwrap();
            }
        }
    }
}

fn main() {
    let cli = Cli::parse();
    let node = start_node(&cli);
    node.sync_wallets().unwrap();
    match &cli.command {
        Command::Info => info(&node),
        Command::Run => loop {
            println!("{:?}", node.wait_next_event());
            node.event_handled().unwrap();
        },
        Command::Open { peer, sats, push } => open(&node, peer, *sats, *push),
        Command::Offer {
            league,
            event,
            outcome,
            side,
            stake,
            their_stake,
        } => {
            let stakes = if side == "yes" {
                json!({"yes": stake, "no": their_stake})
            } else {
                json!({"yes": their_stake, "no": stake})
            };
            let bet = oracle_post(
                &cli,
                "bets",
                json!({"league": league, "event_id": event, "outcome": outcome, "offerer": side, "stakes": stakes}),
            );
            println!("{}", bet["terms"]["question"].as_str().unwrap());
            post_invoice(&cli, &node, &bet, side);
            let bet_id = bet["bet_id"].as_str().unwrap();
            println!("bet id {bet_id}: your friend runs `bet accept {bet_id}`");
            watch(&cli, &node, bet_id);
        }
        Command::Accept { bet_id } => {
            let bet = oracle_get(&cli, &format!("bets/{bet_id}"));
            let side = other(bet["terms"]["offerer"].as_str().unwrap());
            println!(
                "{} You take {side}: {} sats against their {}",
                bet["terms"]["question"].as_str().unwrap(),
                stake_of(&bet, side),
                stake_of(&bet, other(side))
            );
            post_invoice(&cli, &node, &bet, side);
            watch(&cli, &node, bet_id);
        }
        Command::Watch { bet_id } => watch(&cli, &node, bet_id),
    }
    node.stop().unwrap();
}
