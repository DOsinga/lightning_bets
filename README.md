# lightning_bet

Spike for betting a friend over Lightning: two held payments locked to an oracle's
hashes, settled when the oracle reveals the winning preimage. See `PLAN.md`
for the design; the spike results are at the end.

`cargo run` starts a regtest `bitcoind`, two LDK nodes (Alice and Bob) with a direct
channel, and plays two bets:

1. 200k vs 200k, held for 48 blocks (about 8 hours), then settled off-chain:
   the winner claims, the loser rejects, balances move by exactly the stake.
2. 100k vs 200k, Bob force-closes the channel while both stakes are in flight;
   the oracle still decides who gets the money once everything is swept on-chain.

Needs `bitcoind` on `PATH` (or `BITCOIND_EXE` set). Uses LDK Node from `main`,
because `receive_for_hash_with_min_final_cltv_expiry_delta` isn't released yet.
Held payments only work with `manually_handle_unknown_bolt11_payments` enabled.
