# Lightning Bets

Bet a friend on a football match over Lightning without anyone holding the money. Each bet
gets two secrets from an oracle, one for yes and one for no. Both players send their stake as a
held payment locked to the other side's secret. After full time the oracle reveals the winning
secret: the winner collects and the loser hands the other stake back.

Try it at https://douwe.com/projects/lightning_bets, which lists upcoming matches and has a
download of the `bet` command line tool for Apple Silicon Macs. `PLAN.md` explains the design and
what the regtest spike found.

## What's here

- `src/bin/bet.rs`: the `bet` CLI, an LDK Node wallet that talks to the oracle.
- `lightning_bets.py`, `lightning_bets.html`: the oracle and the project page, as served by
  [douwe.com](https://douwe.com). The oracle stores bets in sqlite and resolves them from ESPN's
  public scoreboard.
- `src/bin/spike.rs`: the first experiment. `cargo run --bin spike` starts a regtest `bitcoind`
  (needs it on `PATH`), two nodes with a channel, and plays a bet that settles off-chain and one
  that settles on-chain after a force-close.
- `build_mac.sh`: builds `static/bet-macos-arm64`, the binary the page offers for download.

LDK Node is pinned to a `main` commit, because
`receive_for_hash_with_min_final_cltv_expiry_delta` isn't in a release yet.

## Using `bet`

By default `bet` runs on Mutinynet (a signet with 30 second blocks and a faucet), keeps its node in
`~/.lightning_bets` and uses the oracle on douwe.com. Every default can be changed with a flag or
environment variable; see `bet --help`.

```
bet info                                   # node id, funding address, balances
bet run                                    # stay online while your friend opens a channel
bet open <node_id@host:port> 200000 --push 100000
bet offer --league eng.1 --event <espn event id> --outcome home --side yes --stake 10000 --their-stake 10000
bet accept <bet_id>                        # your friend, on their machine
bet watch <bet_id>                         # resume after a restart
```

`offer` and `accept` keep running until the bet is settled: the accepting side pays first, the
offering side pays once that stake is held, nobody pays after kickoff, and after full time the
winner claims while the loser returns the other stake. A freshly started node can take up to a
minute to reconnect to its channel peer. On Mutinynet a payment can be held for about 15 hours, so
bets can be placed at most about 11 hours before kickoff.
