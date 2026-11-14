# End-to-end test of Microsoft 365 sync over the WebSocket protocol
# (SPEC §14 M5–M6). m365.nix prepends MICROSOFT, the emulator's base URL.
# The test driver provides `machine`, `subtest` and `start_all`.

import base64
import json
import shlex
import time
from datetime import datetime, timedelta, timezone
from urllib.parse import parse_qs, urljoin, urlparse

STATE = "/var/lib/uplink"
CLIENT_ID = "uplink-test"
REDIRECT_URI = "http://127.0.0.1/redirect"
POLL = 30  # uplink's poll interval, seconds


def eventually(what, f, timeout=2 * POLL):
    deadline = time.monotonic() + timeout
    while True:
        result = f()
        if result:
            return result
        if time.monotonic() > deadline:
            raise Exception(f"timed out waiting for {what}")
        time.sleep(0.5)


def sign_in(authorize_url, user="alice@example.com"):
    """Signs in on the emulator's page, as a browser would; returns the
    URL Entra ID redirects to, where nothing listens."""
    page = machine.succeed(f"curl -sf -o /dev/null -w '%{{redirect_url}}' {shlex.quote(authorize_url)}").strip()
    params = {k: v[0] for k, v in parse_qs(urlparse(urljoin(MICROSOFT, page)).query).items()}
    form = " ".join(
        f"--data-urlencode {k}={shlex.quote(params[k])}"
        for k in ["redirect_uri", "scope", "client_id", "response_mode"]
    )
    redirect = machine.succeed(
        f"curl -sf -o /dev/null -w '%{{redirect_url}}' -X POST {MICROSOFT}/oauth2/v2.0/authorize/callback "
        f"--data-urlencode email={user} {form}"
    ).strip()
    assert redirect.startswith(REDIRECT_URI + "?code="), redirect
    return redirect


def authorize_uplink():
    """Runs `uplink oauth init`, pasting what the sign-in redirected to;
    the master key is made on the way."""
    machine.wait_for_unit("emulate.service")
    machine.wait_for_open_port(4003)
    machine.succeed(f"install -d -m 0700 -o uplink -g uplink {STATE}")
    machine.succeed("touch /tmp/pasted")
    machine.succeed(
        "(tail -f /tmp/pasted | runuser -u uplink -- uplink --config /etc/uplink/uplink.yaml "
        "oauth init --account alice-m365 >/tmp/oauth.log 2>&1) >/dev/null 2>&1 &"
    )
    url = machine.wait_until_succeeds(f"grep -o '{MICROSOFT}/common/oauth2/authorize?[^ ]*' /tmp/oauth.log").strip()
    params = {k: v[0] for k, v in parse_qs(urlparse(url).query).items()}
    assert params["resource"] == MICROSOFT, params
    assert params["redirect_uri"] == REDIRECT_URI, params
    redirect = sign_in(url)
    machine.succeed(f"echo {shlex.quote(redirect)} >> /tmp/pasted")
    machine.wait_until_succeeds("grep -q Authorized /tmp/oauth.log")
    machine.succeed(f"test -s {STATE}/accounts/alice-m365.token")


def token_of(user):
    """An access token of the test's own, for acting on the Microsoft side."""
    url = (
        f"{MICROSOFT}/common/oauth2/authorize?client_id={CLIENT_ID}&response_type=code"
        f"&redirect_uri={REDIRECT_URI}&response_mode=query&resource={MICROSOFT}"
    )
    code = parse_qs(urlparse(sign_in(url, user)).query)["code"][0]
    response = machine.succeed(
        f"curl -sf -X POST {MICROSOFT}/common/oauth2/token -d grant_type=authorization_code "
        f"-d code={code} -d client_id={CLIENT_ID} --data-urlencode redirect_uri={REDIRECT_URI}"
    )
    return json.loads(response)["access_token"]


def graph(token, method, path, body=None):
    data = "" if body is None else f"-H 'Content-Type: application/json' -d {shlex.quote(json.dumps(body))}"
    out = machine.succeed(
        f"curl -sf -X {method} -H 'Authorization: Bearer {token}' {data} "
        f"{shlex.quote(MICROSOFT + '/v1.0/me' + path)}"
    )
    return json.loads(out) if out.strip() else None


def deliver(token, subject, body):
    """Puts a message in the Inbox, unread; returns its id."""
    raw = (
        f"Message-ID: <{subject.replace(' ', '.')}@uplink.test>\r\n"
        f"From: Bob Sender <bob@example.org>\r\nTo: alice@example.com\r\n"
        f"Date: Tue, 3 Nov 2026 10:00:00 +0000\r\nSubject: {subject}\r\n\r\n{body}\r\n"
    )
    encoded = base64.b64encode(raw.encode()).decode()
    out = machine.succeed(
        f"curl -sf -X POST -H 'Authorization: Bearer {token}' -H 'Content-Type: text/plain' "
        f"-d {encoded} {MICROSOFT}/v1.0/me/mailFolders/inbox/messages"
    )
    return json.loads(out)["id"]


def ws(body):
    """One request to uplink, as the UI makes it."""
    return json.loads(machine.succeed(f"uplink-ws request {shlex.quote(json.dumps(body))}"))


def account():
    [account] = ws({"type": "list_accounts"})["accounts"]
    return account


def labels():
    return {label["external_id"]: label for label in account()["labels"]}


def entries(label):
    """The latest messages, by subject, of the label's (by id) conversations."""
    [name] = [l["name"] for l in account()["labels"] if l["id"] == label]
    query = f"label:{json.dumps(name, ensure_ascii=False)}"
    page = ws({"type": "list_entries", "query": query, "after": None, "limit": 100})
    return {c["latest"]["subject"]: c["latest"] for c in page["conversations"]}


def in_folder(external_id):
    return entries(labels()[external_id]["id"])


def utc(t):
    return {"dateTime": t.strftime("%Y-%m-%dT%H:%M:%S"), "timeZone": "UTC"}


def events(days=10):
    now = int(time.time() * 1000)
    return ws({"type": "list_events", "from": now - 86_400_000, "to": now + days * 86_400_000})["events"]


def tx_log_head():
    return int(machine.succeed(f"sqlite3 {STATE}/db/uplink.sqlite 'SELECT max(id) FROM tx_log'"))


start_all()

with subtest("uplink oauth init stores a token from the pasted redirect"):
    authorize_uplink()

alice = token_of("alice@example.com")

# A calendar for the first sync to find: a series and a single event.
today = datetime.now(timezone.utc).replace(hour=14, minute=0, second=0, microsecond=0)
graph(alice, "POST", "/events", {
    "subject": "Standup",
    "start": utc(today),
    "end": utc(today + timedelta(minutes=15)),
    "recurrence": {
        "pattern": {"type": "daily", "interval": 1},
        "range": {"type": "numbered", "startDate": today.strftime("%Y-%m-%d"), "numberOfOccurrences": 3},
    },
})
graph(alice, "POST", "/events", {
    "subject": "Dentist",
    "location": {"displayName": "Main Street"},
    "start": utc(today + timedelta(days=5)),
    "end": utc(today + timedelta(days=5, hours=1)),
})

with subtest("the first sync mirrors the mailbox"):
    report = deliver(alice, "Quarterly report", "Numbers are up this quarter")
    machine.systemctl("start uplink.service")
    machine.wait_for_unit("uplink.service")
    machine.wait_for_open_port(8443)
    eventually("the first sync", lambda: account()["synced"])
    assert account()["provider"] == "m365"
    assert "Quarterly report" in in_folder("INBOX")
    assert labels()["ARCHIVE"]["kind"] == "folder"

with subtest("the first sync mirrors the calendar, with its series expanded"):
    listed = eventually("the calendar", lambda: len(events()) == 4 and events())
    standups = [e for e in listed if e["subject"] == "Standup"]
    assert [e["start"] for e in standups] == [int((today + timedelta(days=d)).timestamp() * 1000) for d in range(3)], listed
    assert all(e["recurring"] and not e["exception"] for e in standups), standups
    [dentist] = [e for e in listed if e["subject"] == "Dentist"]
    assert dentist["location"] == "Main Street" and not dentist["recurring"], dentist

with subtest("new mail reaches the client within a poll interval"):
    machine.succeed(f"uplink-ws await entries_added {POLL + 10} >/tmp/await.json 2>/tmp/await.err &")
    machine.wait_until_succeeds("grep -q connected /tmp/await.err")
    lunch = deliver(alice, "Lunch on Friday", "Shall we try the quixotic place downtown")
    machine.wait_until_succeeds("test -s /tmp/await.json", timeout=POLL + 15)
    assert "Lunch on Friday" in in_folder("INBOX")

with subtest("archiving in uplink moves to the Archive, and undo moves back"):
    archive = graph(alice, "GET", "/mailFolders/archive")["id"]
    inbox = graph(alice, "GET", "/mailFolders/inbox")["id"]
    entry = in_folder("INBOX")["Quarterly report"]
    archived = ws({
        "type": "modify_labels",
        "entries": [entry["id"]],
        "add": [],
        "remove": [labels()["INBOX"]["id"]],
    })["tx_log_id"]
    assert "Quarterly report" in in_folder("ARCHIVE")
    eventually("the move to reach Graph", lambda: graph(alice, "GET", f"/messages/{report}")["parentFolderId"] == archive)
    ws({"type": "undo", "tx_log_id": archived})
    eventually("the undo to reach Graph", lambda: graph(alice, "GET", f"/messages/{report}")["parentFolderId"] == inbox)
    assert "Quarterly report" in in_folder("INBOX")

with subtest("a move and a category made in Outlook show up"):
    projects = graph(alice, "POST", "/mailFolders/inbox/childFolders", {"displayName": "Projects"})["id"]
    graph(alice, "POST", f"/messages/{lunch}/move", {"destinationId": projects})
    graph(alice, "PATCH", f"/messages/{lunch}", {"isRead": True, "categories": ["Red"]})
    eventually("the folder to show up", lambda: projects in labels())
    assert labels()[projects]["name"] == "Inbox/Projects"
    lunch_entry = eventually("the move", lambda: in_folder(projects).get("Lunch on Friday"))
    assert "Lunch on Friday" not in in_folder("INBOX")
    assert labels()["UNREAD"]["id"] not in lunch_entry["labels"], lunch_entry
    assert labels()["cat:Red"]["id"] in lunch_entry["labels"], lunch_entry

with subtest("mail sent to Bob reaches him, and its copy in Sent Items shows up"):
    ws({
        "type": "create_draft",
        "id": "to-bob",
        "account": "alice-m365",
        "content": {"from": "", "to": "Bob <bob@example.com>", "cc": "", "bcc": "", "subject": "Plans", "body": "<p>See you there</p>"},
        "in_reply_to": None,
    })
    ws({"type": "send_draft", "draft": "to-bob"})
    bob = token_of("bob@example.com")
    received = eventually(
        "Bob to receive it",
        lambda: graph(bob, "GET", "/mailFolders/inbox/messages/delta")["value"],
    )
    assert [m["subject"] for m in received] == ["Plans"], received
    eventually("the sent copy", lambda: "Plans" in in_folder("SENT"))
    assert all(a["pending"]["sends"] == [] for a in ws({"type": "list_outbox"})["accounts"])

with subtest("a restart loses nothing and resumes from the stored delta links"):
    before = set(in_folder("INBOX"))
    machine.systemctl("restart uplink.service")
    machine.wait_for_open_port(8443)
    assert set(in_folder("INBOX")) == before
    head = tx_log_head()
    time.sleep(POLL + 5)
    assert tx_log_head() == head, "the restarted server re-synced"
    deliver(alice, "After the restart", "still here")
    eventually("mail after the restart", lambda: "After the restart" in in_folder("INBOX"))
    machine.fail("journalctl -u uplink.service | grep -q 'delta link expired'")
