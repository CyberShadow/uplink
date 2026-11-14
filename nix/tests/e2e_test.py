# The web UI in a real browser against the real server, emulated Gmail and
# Conduit (SPEC §15.10 `e2e`). gmail-machine.nix prepends gmail_helpers.py.

import email


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
        lambda: machine.succeed(f"sqlite3 {STATE}/db/uplink.sqlite 'SELECT count(*) FROM mail_send WHERE NOT synced OR NOT wanted'").strip() == "0",
    )

with subtest("a Matrix invitation is joined from the UI; the room shows messages, raw JSON, and sends"):
    room = matrix(bob, "POST", "/createRoom", {"name": "Planning", "invite": ["@alice:localhost"]})["room_id"]
    matrix(bob, "PUT", f"/rooms/{room}/send/m.room.message/1", {"msgtype": "m.text", "body": "Hi from Bob"})
    long = "echo " + "x" * 300
    matrix(bob, "PUT", f"/rooms/{room}/send/m.room.message/code", {
        "msgtype": "m.text",
        "body": f"```\n{long}\n```",
        "format": "org.matrix.custom.html",
        "formatted_body": f"<pre><code>{long}\n</code></pre>",
    })
    matrix(bob, "PUT", f"/rooms/{room}/send/org.example.custom/2", {"answer": 42})
    ui("matrix")

    def bodies():
        events = matrix(bob, "GET", f"/rooms/{room}/messages?dir=b&limit=20")["chunk"]
        return [e["content"].get("body") for e in events if e["type"] == "m.room.message"]

    eventually("uplink's message to reach Bob", lambda: "Hello from uplink" in bodies())
    assert machine.succeed(f"sqlite3 {STATE}/db/uplink.sqlite 'SELECT count(*) FROM matrix_send WHERE NOT synced OR NOT wanted'").strip() == "0"

with subtest("flagging a message adds a to-do, which opens it"):
    result = ui("todos")
    assert sorted(result["todos"]) == sorted([result["flagged"], "Book the train"]), result
    rows = machine.succeed(
        f"sqlite3 {STATE}/db/uplink.sqlite 'SELECT title, message IS NOT NULL FROM todo ORDER BY id'"
    ).splitlines()
    assert rows == [f"{result['flagged']}|1", "Book the train|0"], rows

with subtest("home lists to-dos, rooms and mail in the buckets they first match"):
    result = ui("home")
    assert list(result) == ["Urgent", "Unread", "Starred", "Inbox", "Other"], result
    assert any("Book the train" in row for row in result["Starred"]), result
    # The Inbox's mail is in Urgent if important and unread, else in
    # Unread while unread, else in Inbox.
    listed = "\n".join(result["Urgent"] + result["Unread"] + result["Inbox"])
    assert "Deploy window" in listed and "Quarterly report" in listed, result

with subtest("the history lists the user's ops, and undoes any of them"):
    result = ui("history")
    assert "Added 'Book the train'" in result["descriptions"], result
    # The undo step's archive, and the latest op, just now.
    assert result["undid"] in result["undone"], result
    assert any(d.startswith("Archived") for d in result["undone"]), result

with subtest("# moves a message to Trash in Gmail"):
    result = ui("trash")
    assert result["trash"] == [result["trashed"]], result

    def trashed():
        messages = gmail(token, "GET", "/messages?labelIds=TRASH&includeSpamTrash=true").get("messages", [])
        return [subject_of(m["id"]) for m in messages] == [result["trashed"]]

    eventually("the message to be in Gmail's Trash", trashed)
    assert result["trashed"] not in inbox_subjects()

with subtest("a file attached to a message downloads"):
    raw = (
        "Message-ID: <figures@uplink.test>\r\nFrom: Bob Sender <bob@example.org>\r\n"
        "To: alice@example.com\r\nDate: Wed, 4 Nov 2026 10:00:00 +0000\r\n"
        "Subject: Quarterly figures\r\nMIME-Version: 1.0\r\n"
        "Content-Type: multipart/mixed; boundary=b\r\n\r\n"
        "--b\r\nContent-Type: text/plain\r\n\r\nAttached.\r\n"
        "--b\r\nContent-Type: application/pdf\r\n"
        "Content-Disposition: attachment; filename=figures.pdf\r\n"
        "Content-Transfer-Encoding: base64\r\n\r\n"
        + base64.b64encode(b"%PDF-1.4 figures").decode() + "\r\n--b--\r\n"
    )
    encoded = base64.urlsafe_b64encode(raw.encode()).decode().rstrip("=")
    gmail(token, "POST", "/messages/import", {"raw": encoded, "labelIds": ["INBOX", "UNREAD"]})
    eventually(
        "the message to be synced",
        lambda: machine.succeed(
            f"sqlite3 {STATE}/db/uplink.sqlite \"SELECT count(*) FROM message WHERE subject = 'Quarterly figures'\""
        ).strip() == "1",
    )
    result = ui("attachment")
    assert result == {"name": "figures.pdf", "content": "%PDF-1.4 figures"}, result

    def forwarded():
        for m in gmail(token, "GET", "/messages?labelIds=SENT").get("messages", []):
            if subject_of(m["id"]) == "Fwd: Quarterly figures":
                return gmail(token, "GET", f"/messages/{m['id']}?format=raw")

    forward = email.message_from_bytes(
        base64.urlsafe_b64decode(eventually("the forward to reach Gmail", forwarded)["raw"] + "==")
    )
    files = {part.get_filename(): part.get_payload(decode=True) for part in forward.walk() if part.get_filename()}
    assert files == {"figures.pdf": b"%PDF-1.4 figures", "notes.txt": b"Notes on the figures"}, files
    assert forward["To"] == "<carol@example.net>", forward["To"]

with subtest("a draft written in two tabs at once keeps both tabs' text"):
    draft = ui("tabs")["draft"]
    row = machine.succeed(
        f"sqlite3 {STATE}/db/uplink.sqlite \"SELECT subject, body_html FROM draft WHERE id = '{draft}'\""
    ).strip()
    assert row == "Plans for Friday|<p>Typed in the other tab, and more</p>", row

with subtest("hostile mail runs nothing, loads nothing but through uplink when asked, and stays in its frame"):
    hostile_html = """<p>Hostile content</p>
<script>alert('script')</script>
<img src=x onerror="alert('onerror')"><svg onload="alert('svg')"></svg>
<a href="javascript:alert('javascript')">javascript</a>
<noscript><p title="</noscript><img src=x onerror=alert('mxss')>"></noscript>
<style>i { background: url("data:\\3C/style><img src='/api/messages/1/attachments/0'>") }</style>
<img src="http://t.example/img.gif"><img src="/relative.gif"><img srcset="http://t.example/srcset.gif 1x">
<img width=1 height=1 src="http://t.example/pixel.gif">
<div style="background: url(http://t.example/div.gif)">div</div>
<style>@import url(http://t.example/import.css);
@font-face { font-family: f; src: url(http://t.example/font.woff) }
b { font-family: f; background: url(http://t.example/style.gif) }</style><b>bold</b>
<style>u { background: url(http://t.example/plain.gif) }</style><u>u</u>
<style>s { font-family: f; background: url(http://t.example/fontless.gif) }</style><s>s</s>
<link rel=stylesheet href="http://t.example/link.css"><link rel=prefetch href="http://t.example/prefetch">
<meta http-equiv=refresh content="0;url=http://t.example/refresh">
<form action="http://t.example/form"><input name=q><button>Submit</button></form>
<iframe src="http://t.example/frame"></iframe><object data="http://t.example/object"></object>
<a href="http://t.example/link">A link</a>
"""
    raw = (
        "Message-ID: <hostile@uplink.test>\r\nFrom: Mallory <mallory@example.org>\r\n"
        "To: alice@example.com\r\nDate: Thu, 5 Nov 2026 10:00:00 +0000\r\n"
        "Subject: Hostile\r\nMIME-Version: 1.0\r\nContent-Type: text/html; charset=utf-8\r\n\r\n"
        + hostile_html
    )
    encoded = base64.urlsafe_b64encode(raw.encode()).decode().rstrip("=")
    gmail(token, "POST", "/messages/import", {"raw": encoded, "labelIds": ["INBOX", "UNREAD"]})
    eventually(
        "the message to be synced",
        lambda: machine.succeed(
            f"sqlite3 {STATE}/db/uplink.sqlite \"SELECT count(*) FROM message WHERE subject = 'Hostile'\""
        ).strip() == "1",
    )
    for browser in ["chromium", "firefox"]:
        result = json.loads(machine.succeed(f"uplink-ui hostile {browser}"))
        # Remote images only once shown, and only uplink's fetches of them;
        # never fonts, stylesheets, uplink's own URLs or the pixel.
        assert result["before"] == [], (browser, result)
        expected = sorted(
            f"http://t.example/{name}.gif" for name in ["div", "img", "style", "plain", "fontless"]
        )
        assert sorted(result["shown"]) == expected, (browser, result)
        # Nothing beyond uplink but the link followed, in a tab of its own,
        # without the page as opener or referrer, and reaching nothing of
        # uplink's from there.
        assert result["beyond"] == ["http://t.example/link"], (browser, result)
        assert result["followed"] == {
            "url": "http://t.example/link",
            "opener": False,
            "referer": "",
            "reached": [None, None],
        }, (browser, result)
        assert result["dialogs"] == [] and result["title"] == "Hostile – uplink" and result["pages"] == 2, (
            browser,
            result,
        )
        assert result["keys"] == {"in frame": "IFRAME", "escaped": "BODY"}, (browser, result)

with subtest("a hostile Matrix message runs nothing, and has uplink fetch nothing remote"):
    # Dot-segments, which browsers resolve, out of the event's media to
    # uplink's fetches of mail's remote images.
    climb = "../../../../../remote-image/t.example?url=http%3A%2F%2Ft.example%2F"
    matrix(bob, "PUT", f"/rooms/{room}/send/m.room.message/3", {
        "msgtype": "m.text",
        "body": "Hostile Matrix",
        "format": "org.matrix.custom.html",
        "formatted_body": (
            "<p>Hostile Matrix</p>"
            f'<img src="mxc://localhost/{climb}mx-dots">'
            f'<img src="mxc://localhost/{climb.replace("..", "%2e%2e")}mx-encoded">'
            f'<img src="mxc://../..">'
            '<img src="https://t.example/mx-absolute.png"><img src=x onerror="alert(1)">'
            f'<a href="/api/rooms/1/events/1/media/x/{climb}mx-link">relative</a>'
            '<a href="javascript:alert(1)">script</a>'
        ),
    })
    for browser in ["chromium", "firefox"]:
        result = json.loads(machine.succeed(f"uplink-ui hostile_matrix {browser}"))
        assert result == {
            "remote": [],
            "beyond": [],
            "dialogs": [],
            "images": [None, None, None, None, None],
            "links": [None, None],
        }, (browser, result)

machine.copy_from_vm("/tmp/screenshots", "")
