# lightning_bet

Spike for betting a friend over Lightning: two held payments locked to an oracle's
hashes, settled when the oracle reveals the winning preimage. See `PLAN.md`
for the design; the spike results are at the end.

`cargo run --bin spike` starts a regtest `bitcoind`, two LDK nodes (Alice and Bob) with a direct
channel, and plays two bets:

1. 200k vs 200k, held for 48 blocks (about 8 hours), then settled off-chain:
   the winner claims, the loser rejects, balances move by exactly the stake.
2. 100k vs 200k, Bob force-closes the channel while both stakes are in flight;
   the oracle still decides who gets the money once everything is swept on-chain.

Needs `bitcoind` on `PATH` (or `BITCOIND_EXE` set). Uses LDK Node from `main`,
because `receive_for_hash_with_min_final_cltv_expiry_delta` isn't released yet.
Held payments only work with `manually_handle_unknown_bolt11_payments` enabled.

## The `bet` CLI

Each player runs their own node. Configure it with environment variables:

```
BET_DATA_DIR=~/.lightning_bet      # node keys and state
BET_NETWORK=regtest                # or signet, bitcoin
BET_CHAIN=bitcoind://user:pass@127.0.0.1:18443   # or an Esplora URL
BET_LISTEN=0.0.0.0:9735
BET_ORACLE=https://douwe.com/projects/lightning_oracle/api
```

Then:

```
bet info                                   # node id, funding address, balances
bet run                                    # stay online while your friend opens a channel
bet open <node_id@host:port> 1000000 --push 500000
bet offer --league uefa.europa --event 401915534 --outcome home --side yes --stake 50000 --their-stake 100000
bet accept <bet_id>                        # your friend, on their machine
bet watch <bet_id>                         # resume after a restart
```

Matches are ESPN league and event ids. `offer` and `accept` keep running until the bet is
settled: the accepting side pays first, the offering side pays once that stake is held, and
after full time the winner claims while the loser returns the other stake. Neither side pays
after kickoff. A freshly started node can take up to a minute to reconnect to its channel peer.
