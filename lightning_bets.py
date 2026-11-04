import hashlib
import json
import os
import re
import secrets
import sqlite3
import time
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
from datetime import datetime, timedelta, timezone
from urllib.request import urlopen

from projects.common import IS_MACOS, JsonResponse, Project

SUFFIX = "-mac" if IS_MACOS else ""
DB_PATH = os.path.join(os.path.dirname(__file__), f"bets{SUFFIX}.sqlite3")
ESPN_SUMMARY = "https://site.api.espn.com/apis/site/v2/sports/soccer/{league}/summary?event={event_id}"
ESPN_SCOREBOARD = "https://site.api.espn.com/apis/site/v2/sports/soccer/{league}/scoreboard?dates={dates}"
LEAGUES = {
    "uefa.champions": "Champions League",
    "uefa.europa": "Europa League",
    "uefa.europa.conf": "Conference League",
    "eng.1": "Premier League",
    "esp.1": "La Liga",
    "ger.1": "Bundesliga",
    "ita.1": "Serie A",
    "fra.1": "Ligue 1",
    "ned.1": "Eredivisie",
    "usa.1": "MLS",
    "mex.1": "Liga MX",
}
# On Mutinynet the CLI can hold a payment for about 15 hours, so a bet has to be placed
# within this long before kickoff to last until the final whistle.
BETTING_WINDOW = timedelta(hours=11)
LISTING_WINDOW = timedelta(hours=36)
CACHE_SECONDS = 600
DEFAULT_ORACLE = "https://douwe.com/projects/lightning_bets/api"
_matches_cache = (0.0, [])
VOID_STATUSES = ("STATUS_POSTPONED", "STATUS_CANCELED", "STATUS_ABANDONED")
OUTCOMES = ("home", "away", "draw")
RULES = (
    "Yes if the outcome matches the final score as reported by ESPN once the match is over, "
    "extra time included, penalty shootouts not. Void if the match is postponed, cancelled "
    "or abandoned."
)


def db():
    conn = sqlite3.connect(DB_PATH)
    conn.row_factory = sqlite3.Row
    conn.execute(
        "CREATE TABLE IF NOT EXISTS bets ("
        " id TEXT PRIMARY KEY, terms TEXT NOT NULL, created REAL NOT NULL,"
        " yes_preimage TEXT NOT NULL, no_preimage TEXT NOT NULL,"
        " yes_invoice TEXT, no_invoice TEXT,"
        " status TEXT NOT NULL DEFAULT 'open', winner TEXT)"
    )
    return conn


def day_matches(league, day, now):
    with urlopen(ESPN_SCOREBOARD.format(league=league, dates=f"{day:%Y%m%d}"), timeout=10) as r:
        events = json.load(r)["events"]
    end = now + LISTING_WINDOW
    matches = []
    for event in events:
        competition = event["competitions"][0]
        kickoff = datetime.strptime(competition["date"], "%Y-%m-%dT%H:%MZ").replace(tzinfo=timezone.utc)
        if competition["status"]["type"]["state"] != "pre" or not now < kickoff < end:
            continue
        teams = {c["homeAway"]: c["team"]["displayName"] for c in competition["competitors"]}
        matches.append(
            {
                "league": league,
                "league_name": LEAGUES[league],
                "event_id": event["id"],
                "home": teams["home"],
                "away": teams["away"],
                "kickoff": kickoff,
                "open": kickoff - now < BETTING_WINDOW,
            }
        )
    return matches


def upcoming_matches():
    global _matches_cache
    fetched, matches = _matches_cache
    if time.time() - fetched > CACHE_SECONDS:
        now = datetime.now(timezone.utc)
        days = [now.date() + timedelta(days=i) for i in range(LISTING_WINDOW.days + 2)]
        jobs = [(league, day) for league in LEAGUES for day in days]
        with ThreadPoolExecutor(16) as pool:
            per_day = pool.map(lambda job: day_matches(*job, now), jobs)
        matches = sorted((m for ms in per_day for m in ms), key=lambda m: m["kickoff"])
        _matches_cache = (time.time(), matches)
    return matches


def payment_hash(preimage_hex):
    return hashlib.sha256(bytes.fromhex(preimage_hex)).hexdigest()


def fetch_match(league, event_id):
    with urlopen(ESPN_SUMMARY.format(league=league, event_id=event_id), timeout=10) as r:
        competition = json.load(r)["header"]["competitions"][0]
    teams = {c["homeAway"]: c for c in competition["competitors"]}
    return {
        "kickoff": competition["date"],
        "state": competition["status"]["type"]["state"],
        "status": competition["status"]["type"]["name"],
        "home": teams["home"]["team"]["displayName"],
        "away": teams["away"]["team"]["displayName"],
        "home_score": int(teams["home"].get("score") or 0),
        "away_score": int(teams["away"].get("score") or 0),
    }


def question(terms):
    home, away = terms["home"], terms["away"]
    return {
        "home": f"Does {home} beat {away}?",
        "away": f"Does {away} win at {home}?",
        "draw": f"Do {home} and {away} draw?",
    }[terms["outcome"]]


def bet_json(row):
    terms = json.loads(row["terms"])
    res = {
        "bet_id": row["id"],
        "terms": terms,
        "yes_hash": payment_hash(row["yes_preimage"]),
        "no_hash": payment_hash(row["no_preimage"]),
        "yes_invoice": row["yes_invoice"],
        "no_invoice": row["no_invoice"],
        "status": row["status"],
        "winner": row["winner"],
        "winning_preimage": None,
    }
    if row["winner"]:
        res["winning_preimage"] = row[f"{row['winner']}_preimage"]
    return res


def resolve(conn, row, status, winner=None):
    conn.execute("UPDATE bets SET status = ?, winner = ? WHERE id = ?", (status, winner, row["id"]))
    conn.commit()
    return conn.execute("SELECT * FROM bets WHERE id = ?", (row["id"],)).fetchone()


def refresh(conn, row):
    if row["status"] != "open":
        return bet_json(row)
    terms = json.loads(row["terms"])
    match = fetch_match(terms["league"], terms["event_id"])
    if match["status"] in VOID_STATUSES:
        row = resolve(conn, row, "void")
    elif match["state"] == "post":
        diff = match["home_score"] - match["away_score"]
        outcome = "home" if diff > 0 else "away" if diff < 0 else "draw"
        row = resolve(conn, row, "resolved", "yes" if outcome == terms["outcome"] else "no")
    return {**bet_json(row), "match_state": match["state"]}


def error(message, status=400):
    return JsonResponse({"error": message}, status=status)


class LightningBets(Project):
    def fill_dict(self, request, d):
        d["matches"] = upcoming_matches()
        d["download_url"] = request.build_absolute_uri(f"/static/projects/{self.id}/bet-macos-arm64")
        oracle = request.build_absolute_uri(f"/projects/{self.id}/api")
        d["oracle_url"] = None if oracle == DEFAULT_ORACLE else oracle

    def handle_request(self, handler, request):
        if not handler or not handler.startswith("api/"):
            return None
        path = handler[len("api/") :].rstrip("/")
        if path == "bets" and request.method == "POST":
            return self.create_bet(request)
        m = re.fullmatch(r"bets/([0-9a-f]{16})(/invoice|/resolve)?", path)
        if not m:
            return error("not found", 404)
        bet_id, action = m.groups()
        with closing(db()) as conn:
            row = conn.execute("SELECT * FROM bets WHERE id = ?", (bet_id,)).fetchone()
            if not row:
                return error("no such bet", 404)
            if action is None and request.method == "GET":
                return JsonResponse(refresh(conn, row))
            if action == "/invoice" and request.method == "POST":
                return self.add_invoice(conn, row, request)
            if action == "/resolve" and request.method == "POST":
                return self.force_resolve(conn, row, request)
        return error("method not allowed", 405)

    def create_bet(self, request):
        try:
            data = json.loads(request.body)
            league, event_id, outcome = data["league"], str(data["event_id"]), data["outcome"]
            offerer = data["offerer"]
            stakes = {"yes": int(data["stakes"]["yes"]), "no": int(data["stakes"]["no"])}
        except (KeyError, TypeError, ValueError, json.JSONDecodeError):
            return error("need league, event_id, outcome, offerer and stakes")
        if outcome not in OUTCOMES or not re.fullmatch(r"[a-z0-9._]+", league) or not event_id.isdigit():
            return error("bad league, event_id or outcome")
        if offerer not in ("yes", "no") or min(stakes.values()) <= 0:
            return error("bad offerer or stakes")
        match = fetch_match(league, event_id)
        if match["state"] != "pre":
            return error("match has already started")
        terms = {
            "league": league,
            "event_id": event_id,
            "outcome": outcome,
            "home": match["home"],
            "away": match["away"],
            "kickoff": match["kickoff"],
            "offerer": offerer,
            "stakes": stakes,
            "rules": RULES,
        }
        terms["question"] = question(terms)
        bet_id = secrets.token_hex(8)
        with closing(db()) as conn:
            conn.execute(
                "INSERT INTO bets (id, terms, created, yes_preimage, no_preimage) VALUES (?, ?, ?, ?, ?)",
                (bet_id, json.dumps(terms), time.time(), secrets.token_hex(32), secrets.token_hex(32)),
            )
            conn.commit()
            row = conn.execute("SELECT * FROM bets WHERE id = ?", (bet_id,)).fetchone()
        return JsonResponse(bet_json(row), status=201)

    def add_invoice(self, conn, row, request):
        try:
            data = json.loads(request.body)
            side, invoice = data["side"], data["invoice"]
        except (KeyError, TypeError, json.JSONDecodeError):
            return error("need side and invoice")
        if side not in ("yes", "no") or not re.fullmatch(r"ln[a-z0-9]{50,2000}", invoice):
            return error("bad side or invoice")
        if row["status"] != "open":
            return error("bet is no longer open", 409)
        updated = conn.execute(
            f"UPDATE bets SET {side}_invoice = ? WHERE id = ? AND {side}_invoice IS NULL",
            (invoice, row["id"]),
        ).rowcount
        conn.commit()
        if not updated:
            return error(f"{side} invoice already set", 409)
        return JsonResponse(bet_json(conn.execute("SELECT * FROM bets WHERE id = ?", (row["id"],)).fetchone()))

    # Lets us test the whole flow without waiting for a real match to finish.
    def force_resolve(self, conn, row, request):
        token = os.environ.get("LIGHTNING_ORACLE_ADMIN_TOKEN")
        try:
            data = json.loads(request.body)
        except json.JSONDecodeError:
            return error("bad json")
        if not token or not secrets.compare_digest(str(data.get("token", "")), token):
            return error("forbidden", 403)
        if row["status"] != "open":
            return error("bet is no longer open", 409)
        if data.get("result") == "void":
            return JsonResponse(bet_json(resolve(conn, row, "void")))
        if data.get("result") not in ("yes", "no"):
            return error("result must be yes, no or void")
        return JsonResponse(bet_json(resolve(conn, row, "resolved", data["result"])))
