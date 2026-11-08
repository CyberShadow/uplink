# The web UI in a real browser against the real server, emulated Gmail and
# Conduit (SPEC §15.10 `e2e`). gmail-machine.nix prepends gmail_helpers.py.


def ui(step):
    return json.loads(machine.succeed(f"uplink-ui {step}"))


def subject_of(id):
    headers = gmail(token, "GET", f"/messages/{id}?format=metadata&metadataHeaders=Subject")["payload"]["headers"]
    return next(h["value"] for h in headers if h["name"] == "Subject")


def inbox_subjects():
    messages = gmail(token, "GET", "/messages?labelIds=INBOX").get("messages", [])
    return {subject_of(m["id"]) for m in messages}


MATRIX = "http://127.0.0.1:6167/_matrix/client/v3"


def matrix(token, method, path, body=None):
    data = f"-d {shlex.quote(json.dumps(body))}" if body is not None else ""
    auth = f"-H 'Authorization: Bearer {token}'" if token else ""
    return json.loads(machine.succeed(f"curl -sf -X {method} {auth} {data} {shlex.quote(MATRIX + path)}"))


def register(user):
    return matrix(None, "POST", "/register", {
        "username": user, "password": "matrix-password", "auth": {"type": "m.login.dummy"},
    })["access_token"]


start_all()
authorize_uplink()

machine.wait_for_unit("conduit.service")
machine.wait_for_open_port(6167)
register("admin")  # Conduit makes its first user admin.
alice = register("alice")
bob = register("bob")
machine.succeed(
    "printf 'matrix-password\n\n' | runuser -u uplink -- "
    "uplink --config /etc/uplink/uplink.yaml matrix pair --account alice-matrix >/tmp/pair.log 2>&1"
)
machine.succeed("grep -q 'Skipped: the device is not verified' /tmp/pair.log")
token = provider_token()
gmail(token, "POST", "/labels", {"name": "Projects"})
deliver(token, "Quarterly report", "Numbers are up this quarter.", date="Mon, 2 Nov 2026 09:00:00 +0000")
deliver(token, "Lunch on Friday", "Shall we try the quixotic place downtown?",
        sender="Carol <carol@example.net>", date="Tue, 3 Nov 2026 12:30:00 +0000")
deliver(token, "Deploy window", "The deploy window moves to Thursday.",
        sender="Dave <dave@example.com>", date="Wed, 4 Nov 2026 16:45:00 +0000")
start_uplink()
machine.wait_until_succeeds(
    f"sqlite3 {STATE}/db/uplink.sqlite \"SELECT sync_state IS NOT NULL FROM account\" | grep -qx 1", timeout=60
)
everything = {"Quarterly report", "Lunch on Friday", "Deploy window"}

with subtest("the Inbox lists the mail newest first, and opening one reads it"):
    assert ui("read")["subjects"] == ["Deploy window", "Lunch on Friday", "Quarterly report"]

with subtest("the notice's Undo puts an archived message back"):
    assert set(ui("undo")["subjects"]) == everything
    eventually("Gmail's Inbox to be whole", lambda: inbox_subjects() == everything, timeout=15)

with subtest("archiving in the UI archives in Gmail"):
    result = ui("archive")
    assert "Lunch on Friday" not in result["subjects"], result
    eventually("the archive to reach Gmail", lambda: inbox_subjects() == everything - {"Lunch on Friday"})

with subtest("labelling in the UI labels in Gmail"):
    ui("label")
    projects = next(l["id"] for l in gmail(token, "GET", "/labels")["labels"] if l["name"] == "Projects")
    eventually("the labels to reach Gmail", lambda: len(gmail(token, "GET", f"/messages?labelIds={projects}").get("messages", [])) == 2)

with subtest("search finds a word in a body, and its URL reloads"):
    result = ui("search")
    assert result == {"path": "search/quixot", "subjects": ["Lunch on Friday"]}, result

with subtest("a reply written in the UI is sent after the delay, in the thread"):
    ui("reply")

    def sent():
        messages = gmail(token, "GET", "/messages?labelIds=SENT").get("messages", [])
        return messages and gmail(token, "GET", f"/messages/{messages[0]['id']}?format=raw")

    message = eventually("the reply to reach Gmail", sent)
    raw = base64.urlsafe_b64decode(message["raw"] + "==").decode()
    assert "Subject: Re: Lunch on Friday" in raw, raw
    assert "carol@example.net" in raw, raw
    assert "<b>See you there!</b>" in raw or "<strong>See you there!</strong>" in raw, raw
    lunch = next(
        m for m in gmail(token, "GET", "/messages")["messages"]
        if subject_of(m["id"]) == "Lunch on Friday"
    )
    assert message["threadId"] == lunch["threadId"], (message["threadId"], lunch)
    eventually(
        "the sent copy to be synced back, emptying the outbox",
        lambda: machine.succeed(f"sqlite3 {STATE}/db/uplink.sqlite 'SELECT count(*) FROM mail_outbox'").strip() == "0",
    )

with subtest("a Matrix room shows messages, raw JSON for other events, and sends"):
    room = matrix(bob, "POST", "/createRoom", {"name": "Planning", "invite": ["@alice:localhost"]})["room_id"]
    # Alice joins from another device; uplink has no invitation UI.
    matrix(alice, "POST", f"/join/{room}", {})
    matrix(bob, "PUT", f"/rooms/{room}/send/m.room.message/1", {"msgtype": "m.text", "body": "Hi from Bob"})
    matrix(bob, "PUT", f"/rooms/{room}/send/org.example.custom/2", {"answer": 42})
    ui("matrix")

    def bodies():
        events = matrix(bob, "GET", f"/rooms/{room}/messages?dir=b&limit=20")["chunk"]
        return [e["content"].get("body") for e in events if e["type"] == "m.room.message"]

    eventually("uplink's message to reach Bob", lambda: "Hello from uplink" in bodies())
    assert machine.succeed(f"sqlite3 {STATE}/db/uplink.sqlite 'SELECT count(*) FROM matrix_outbox'").strip() == "0"

machine.copy_from_vm("/tmp/screenshots", "")
