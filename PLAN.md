# Bet Your Friend on Tonight's Game, over Lightning

A hackathon plan for a peer-to-peer sports bet between two friends, settled over Lightning with a tiny hash oracle. Noncustodial, but oracle-trusted.

## The idea

You and a friend agree: "Does Ajax win tonight? You back yes, I back no, 20k sats each." Each of you sends the other your stake as a *held* Lightning payment. After full time, a small oracle service reveals which side won. The winner collects, the loser's payment refunds. Nobody holds anyone's money in between, including whoever runs the oracle.

## Why this design

The original proposal used Discreet Log Contracts (DLCs) funded on-chain. That works, but every bet costs an on-chain funding transaction plus a settlement transaction, which at normal fee levels eats 5–20% of a small bet. Routing a long-lived conditional payment over Lightning doesn't work either: routing nodes won't lock capacity for months, and locking to an oracle *signature* needs PTLCs, which Lightning doesn't have.

Short bets change that. A bet that lives a few hours fits inside HTLC timeouts, and if the oracle publishes hash preimages instead of signatures, a bet is just two ordinary hash-locked payments. No DLCs, no on-chain transactions per bet, no new channel per bet, and the BTC price exposure and early-exit problems mostly go away over a 90-minute match. Lightning is the settlement layer itself, not just a way to fund some other mechanism.

## Trust model

- **Noncustodial:** the oracle can't move anyone's funds, and doesn't need to know who the bettors are.
- **Oracle-trusted:** the oracle can reveal the wrong outcome, or both secrets. A DLC oracle that signs both outcomes leaks its private key; a hash oracle has no such penalty. Among friends, with an oracle you run yourself, that's fine. Don't call it trustless.
- **Not atomic:** the two payments are independent HTLCs. They are set up separately and could in principle expire separately. See the state machine below.
- **Nodes must stay watchful:** if a peer force-closes the channel while HTLCs are pending, normal on-chain Lightning rules apply, and each node has to claim or time out its HTLCs on-chain before the deadlines.

## How a bet works

1. Alice and Bob agree on terms: fixture, question, sides, stakes. The app asks the oracle to create a bet, which binds those terms to a new `bet_id` and returns two fresh hashes, `yes_hash` and `no_hash`. Each bet gets its own secrets, so payment hashes are never reused across bets and nobody can link bets on the same match.
2. Alice (backing yes) creates a hold invoice locked to `yes_hash`. Bob (backing no) creates one locked to `no_hash`.
3. They swap invoices and each pays the other's. The bet only becomes active once both payments are pending at their recipients.
4. After full time the oracle publishes the winning preimage, say for `yes_hash`.
5. Alice's app claims her incoming payment (Bob's stake) with that preimage.
6. Bob's app sees the outcome and rejects his incoming payment at once, so Alice's stake returns without waiting for the timeout.

If Bob refuses to reject, nothing is lost: the `no` preimage is never revealed, so he can't claim it, and the payment times out and refunds.

Unequal stakes give you odds: 30k against 70k means 30%.

### State machine

| State | Meaning | Exits |
|---|---|---|
| `PROPOSED` | Terms agreed, `bet_id` and hashes issued | → `FUNDING`, or abandoned |
| `FUNDING` | Payments being sent | → `ACTIVE` when both are pending; → `CANCELLED` if only one arrives in time |
| `ACTIVE` | Both payments pending at their recipients | → `RESOLVED` on oracle reveal; → `REFUNDED` on void or timeout |
| `RESOLVED` | Oracle revealed the outcome | → `SETTLED` |
| `SETTLED` | Winner claimed, loser rejected | final |
| `CANCELLED` | Whoever received a payment rejects it | final |
| `REFUNDED` | Both payments rejected or timed out | final |

`CANCELLED` matters: if Alice's payment reaches Bob but Bob's never reaches Alice, the bet isn't on, and Bob's app has to reject Alice's payment, not hold it as a one-sided wager. Between friends, the remaining risk is that the first payer is briefly exposed to a counterparty who won't cancel. Name it in the pitch as a known gap.

## Components

### 1. Oracle

A small Python web service (it could live on douwe.com as a project).

- `POST /bets` with fixture and terms → `{bet_id, yes_hash, no_hash}`. Terms are stored and can't be changed.
- `GET /bets/{bet_id}` → `{terms, status, winning_preimage}`, where status is `open`, `resolved` or `void`.
- After the match, poll a free scores API (football-data.org has a free tier) and resolve every open bet on that fixture.
- Abandoned or postponed match → `void`; both apps reject their incoming payments.
- It only needs the final score, so a free, slow feed is fine.

Keep questions binary. "Ajax wins" against "Ajax doesn't win" handles draws; alternatively make it draw-no-bet and use the void path.

### 2. Bet app

A CLI or small web UI around LDK Node.

- **Create bet:** pick fixture, side, stakes; call the oracle; produce a shareable link with the `bet_id`.
- **Accept bet:** each app creates its hold invoice for its own winning hash; the apps swap invoices and pay each other.
- **Watch:** track both payments through the state machine, poll the oracle, claim or reject.

LDK Node calls (checked against `main`, November 2026):

- `receive_for_hash_with_min_final_cltv_expiry_delta(amount_msat, description, expiry_secs, payment_hash, min_final_cltv_expiry_delta)` creates the hold invoice with a long enough HTLC time budget. Plain `receive_for_hash` uses the default delta, which is far too short to hold through a match.
- `claim_for_id(payment_id, preimage)` collects when you win.
- `fail_for_id(payment_id)` refunds when you lose, on void, or on cancel.
- Payments surface as `PaymentClaimable` events, which carry the `payment_id` and a `claim_deadline` block height.
- `Config::manually_handle_unknown_bolt11_payments` must be `true`, or payments to `receive_for_hash` invoices are rejected.

**Version caveat:** the CLTV method was added in PR #1137, merged 2026-10-15. The latest release, v0.7.0 (December 2025), doesn't have it, so build LDK Node from `main`. Using the Python bindings then means building them yourself, which is why writing the LDK side in Rust is probably simpler. The oracle can stay in Python.

**Invoice expiry and HTLC expiry are different things.** `expiry_secs` only controls how long the invoice can be paid. How long the payment can stay pending is set by `min_final_cltv_expiry_delta`, in blocks. LDK fails back an HTLC that arrives with fewer blocks left than that, and the invoice asks for three extra blocks of margin.

### 3. Network and liquidity

- **Start on regtest** (or Polar). Mutinynet works too, but with its 30-second blocks a CLTV delta of N blocks lasts only N/2 minutes, so the numbers need adjusting.
- **One direct channel** between the two nodes. No routing nodes, no jamming heuristics, no inbound-liquidity problems, and the same channel works for every bet all season.
- **Both sides need outbound balance** for their stake, plus room for both pending HTLCs after channel reserves. A channel with 100k sats on each side easily covers a 20k/20k bet.

## Things to get right

- **HTLC time budget.** On mainnet, about 50 blocks for 8 hours; leave margin for slow blocks. Place bets close to kickoff to keep hold time short.
- **Reject promptly.** On cancel, void or loss, call `fail_for_id` straight away instead of letting the payment time out.
- **Watch the chain.** The node must be running near the deadlines to handle on-chain resolution if the channel is force-closed.
- **In-flight limits.** A channel holds at most 483 pending HTLCs. Irrelevant for two friends.

## Order of work

The core demo is one sequence. Football results and share links come after it works.

1. Two LDK Nodes on regtest with a funded direct channel, balance on both sides.
2. A mock oracle creates a bet with two fresh payment hashes.
3. Each node creates a hold invoice for its hash with a long CLTV delta, and each pays the other's.
4. Confirm both payments reach `PaymentClaimable`.
5. Reveal one preimage.
6. Confirm one payment is claimed and the other is failed back, with balances as expected.
7. Repeat with a force-close while both HTLCs are pending, mine blocks, and check that the winner still collects on-chain and the loser's stake comes back.

Step 7 is what separates a working system from one that only works while both nodes cooperate. After that:

8. Real oracle: `POST /bets`, `GET /bets/{id}`, score polling, void path.
9. Bet app with share link and the state machine, including cancel.
10. Demo: set up tonight's game, place the bet before kickoff, watch it settle after full time.

## Spike results (steps 1–7)

The spike lives in `~/Dropbox/Douwe/Proj/lightning_bet` (`cargo run`, needs `bitcoind` on `PATH`). Regtest, two LDK Nodes built from `main`, one direct 1M-sat channel. Both scenarios pass:

- **Cooperative settle.** 200k against 200k, with a CLTV delta of 72. Both stakes stayed held through 48 mined blocks (about 8 hours of mainnet time). After the reveal, the winner claimed and the loser rejected, and the channel balances moved by exactly 200k with no fees.
- **Force-close mid-bet.** 100k against 200k. The loser force-closed with both stakes in flight, the oracle then revealed the outcome, and the winner claimed on-chain with the preimage. After 162 blocks everything was swept: the winner ended up 200k up, the loser 200k down, with 482 and 121 sats of on-chain fees.

What we learned:

- **Deadlines are at least what you ask for, often more.** With a requested delta of 72, claim deadlines came out anywhere from 77 to 157 blocks away. The sender adds a random extra CLTV offset for privacy. That's harmless for holding, but it means a stake can be locked longer than planned if the other side disappears.
- **Liquidity bites at the edge.** A 300k stake from a 300k channel balance failed with `PaymentSendingFailed` because of the channel reserve (1%). The app should check `outbound_capacity` before offering a bet.
- **A force-close is slow but safe.** The winner's on-chain claim is immediate, but the loser's own output is time-locked and the winner's refund of their own stake waits for the HTLC timeout, so full on-chain resolution took about 160 blocks. That's more than a day on mainnet, though the money ends up in the right place.
- **`fail_for_id` after a force-close** returns `Ok` and does no harm; the HTLC just resolves on-chain.
- **Pending sweeps hang around.** After a close, `pending_balances_from_channel_closures` keeps listing confirmed sweeps as `AwaitingThresholdConfirmations` long after they are final. Don't treat a non-empty list as unfinished business.

## Pitch

Polymarket-style platforms are good at big, long-running markets with a central order book and dollar collateral. They are bad at small, fast bets between people who know each other and don't want an operator in the middle. This does exactly that: a bet between friends, settled in seconds, nobody holding the money, for a few sats in fees, with Lightning doing the settlement rather than just moving money around it.
