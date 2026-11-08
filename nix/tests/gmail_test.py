# Prepended by gmail.nix: GOOGLE, the emulator's base URL.

import base64
import json
import shlex
import time
from urllib.parse import parse_qs, urlparse

STATE = "/var/lib/uplink"
SCOPE = "https://www.googleapis.com/auth/gmail.modify"


def eventually(what, f, timeout=30):
    deadline = time.monotonic() + timeout
    while True:
        result = f()
        if result:
            return result
        if time.monotonic() > deadline:
            raise Exception(f"timed out waiting for {what}")
        time.sleep(0.5)


def ws(body):
    """One request to uplink, as the UI makes it."""
    return json.loads(machine.succeed(f"uplink-ws request {shlex.quote(json.dumps(body))}"))


def sign_in(params):
    """Signs in on the emulator's consent form, as a browser would;
    returns where Google redirects to."""
    form = " ".join(
        f"--data-urlencode {k}={shlex.quote(v)}" for k, v in params.items()
    )
    return machine.succeed(
        f"curl -sf -o /dev/null -w '%{{redirect_url}}' -X POST {GOOGLE}/o/oauth2/v2/auth/callback "
        f"--data-urlencode email=alice@example.com {form}"
    ).strip()


def provider_token():
    """An access token of the test's own, for acting on the Gmail side."""
    location = sign_in({
        "redirect_uri": "http://127.0.0.1/",
        "scope": SCOPE,
        "state": "provider",
        "client_id": "uplink-test",
    })
    code = parse_qs(urlparse(location).query)["code"][0]
    response = machine.succeed(
        f"curl -sf -X POST {GOOGLE}/oauth2/token -d grant_type=authorization_code "
        f"-d code={code} -d client_id=uplink-test -d client_secret=uplink-test-secret "
        f"-d redirect_uri=http://127.0.0.1/"
    )
    return json.loads(response)["access_token"]


def gmail(method, path, body=None):
    data = "" if body is None else f"-H 'Content-Type: application/json' -d {shlex.quote(json.dumps(body))}"
    out = machine.succeed(
        f"curl -sf -X {method} -H 'Authorization: Bearer {token}' {data} "
        f"{GOOGLE}/gmail/v1/users/me{path}"
    )
    return json.loads(out) if out.strip() else None


def gmail_labels(message):
    return gmail("GET", f"/messages/{message}?format=minimal").get("labelIds", [])


def deliver(subject, body):
    raw = (
        f"Message-ID: <{subject.replace(' ', '.')}@uplink.test>\r\n"
        "From: Bob Sender <bob@example.org>\r\nTo: alice@example.com\r\n"
        f"Date: Tue, 3 Nov 2026 10:00:00 +0000\r\nSubject: {subject}\r\n\r\n{body}\r\n"
    )
    encoded = base64.urlsafe_b64encode(raw.encode()).decode().rstrip("=")
    return gmail("POST", "/messages/import", {"raw": encoded, "labelIds": ["INBOX", "UNREAD"]})["id"]


def account():
    [account] = ws({"type": "list_accounts"})["accounts"]
    return account


def label_ids():
    return {label["name"]: label["id"] for label in account()["labels"]}


def entries(label):
    page = ws({"type": "list_entries", "label": label, "after": None, "limit": 100})
    return {entry["subject"]: entry for entry in page["entries"]}


def tx_log_head():
    return int(machine.succeed(f"sqlite3 {STATE}/db/uplink.sqlite 'SELECT max(id) FROM tx_log'"))


start_all()
machine.wait_for_unit("emulate.service")
machine.wait_for_open_port(4002)

with subtest("uplink oauth init stores a token"):
    machine.succeed(f"install -d -m 0700 -o uplink -g uplink {STATE}")
    machine.succeed(f"runuser -u uplink -- sh -c 'uplink keygen > {STATE}/master.key'")
    machine.succeed(
        "runuser -u uplink -- sh -c 'uplink --config /etc/uplink/uplink.yaml "
        "oauth init --account alice-gmail </dev/null >/tmp/oauth.log 2>&1 &'"
    )
    url = machine.wait_until_succeeds(f"grep -o '{GOOGLE}/o/oauth2/v2/auth?[^ ]*' /tmp/oauth.log").strip()
    params = {k: v[0] for k, v in parse_qs(urlparse(url).query).items()}
    assert params["scope"] == SCOPE, params
    assert params["code_challenge_method"] == "S256", params
    redirect = sign_in({
        k: params[k]
        for k in ["redirect_uri", "scope", "state", "client_id", "code_challenge", "code_challenge_method"]
    })
    assert redirect.startswith(params["redirect_uri"]), redirect
    machine.succeed(f"curl -sf {shlex.quote(redirect)}")
    machine.wait_until_succeeds("grep -q Authorized /tmp/oauth.log")
    machine.succeed(f"test -s {STATE}/accounts/alice-gmail.token")

token = provider_token()

with subtest("the first sync mirrors the mailbox"):
    report = deliver("Quarterly report", "Numbers are up this quarter")
    machine.systemctl("start uplink.service")
    machine.wait_for_unit("uplink.service")
    machine.wait_for_open_port(8443)
    eventually("the first sync", lambda: account()["synced"])
    labels = label_ids()
    assert "Quarterly report" in entries(labels["INBOX"])

with subtest("new mail reaches the client within a poll interval"):
    machine.succeed("uplink-ws await entries_added 10 >/tmp/await.json 2>/tmp/await.err &")
    machine.wait_until_succeeds("grep -q connected /tmp/await.err")
    lunch = deliver("Lunch on Friday", "Shall we try the quixotic place downtown")
    machine.wait_until_succeeds("test -s /tmp/await.json", timeout=15)
    assert "Lunch on Friday" in entries(labels["INBOX"])

with subtest("archiving in uplink archives in Gmail, and undo brings it back"):
    entry = entries(labels["INBOX"])["Quarterly report"]
    archived = ws({
        "type": "modify_labels",
        "entries": [entry["id"]],
        "add": [],
        "remove": [labels["INBOX"]],
    })["tx_log_id"]
    assert "Quarterly report" not in entries(labels["INBOX"])
    eventually("the archive to reach Gmail", lambda: "INBOX" not in gmail_labels(report))
    ws({"type": "undo", "tx_log_id": archived})
    eventually("the undo to reach Gmail", lambda: "INBOX" in gmail_labels(report))
    assert "Quarterly report" in entries(labels["INBOX"])

with subtest("a label change made in Gmail shows up"):
    projects = gmail("POST", "/labels", {"name": "Projects"})["id"]
    gmail("POST", f"/messages/{lunch}/modify", {"addLabelIds": [projects], "removeLabelIds": ["UNREAD"]})
    projects_id = eventually("the label to show up", lambda: label_ids().get("Projects"))
    eventually("the message to be labelled", lambda: "Lunch on Friday" in entries(projects_id))
    lunch_entry = entries(labels["INBOX"])["Lunch on Friday"]
    assert labels["UNREAD"] not in lunch_entry["labels"], lunch_entry

with subtest("search covers subject, body and sender"):
    for query, expected in [
        ("quarterly", {"Quarterly report"}),
        ("quixot", {"Lunch on Friday"}),
        ("bob@example.org", {"Quarterly report", "Lunch on Friday"}),
    ]:
        found = {e["subject"] for e in ws({"type": "search", "query": query, "limit": 10})["entries"]}
        assert found == expected, (query, found)

with subtest("a restart loses nothing and resumes from the stored historyId"):
    before = set(entries(labels["INBOX"]))
    machine.systemctl("restart uplink.service")
    machine.wait_for_open_port(8443)
    assert set(entries(labels["INBOX"])) == before
    head = tx_log_head()
    time.sleep(12)  # more than two poll intervals
    assert tx_log_head() == head, "the restarted server re-synced"
    deliver("After the restart", "still here")
    eventually("mail after the restart", lambda: "After the restart" in entries(labels["INBOX"]))
    machine.fail("journalctl -u uplink.service | grep -q 'history expired'")
    eml = int(machine.succeed(f"find {STATE}/mail -name '*.eml' | wc -l"))
    assert eml == 3, eml
