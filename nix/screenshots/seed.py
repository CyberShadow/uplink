# Seeds the demo the README's screenshots show, then takes them
# (uplink-shots). default.nix prepends gmail_helpers.py and MICROSOFT, the
# Microsoft emulator's base URL.

import email.utils
from datetime import datetime, timedelta, timezone

now = datetime.now(timezone.utc).replace(second=0, microsecond=0)


def ago(**delta):
    return now - timedelta(**delta)


def mime(subject, body, sender, to, date, message_id, in_reply_to=None, attachment=None):
    """An RFC 5322 message; `attachment` is (filename, content type, bytes)."""
    headers = (
        f"Message-ID: <{message_id}>\r\nFrom: {sender}\r\nTo: {to}\r\n"
        f"Date: {email.utils.format_datetime(date)}\r\nSubject: {subject}\r\nMIME-Version: 1.0\r\n"
    )
    if in_reply_to:
        headers += f"In-Reply-To: <{in_reply_to}>\r\nReferences: <{in_reply_to}>\r\n"
    if attachment is None:
        return headers + f"Content-Type: text/plain; charset=utf-8\r\n\r\n{body}\r\n"
    name, content_type, content = attachment
    return headers + (
        "Content-Type: multipart/mixed; boundary=b\r\n\r\n"
        f"--b\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{body}\r\n"
        f"--b\r\nContent-Type: {content_type}\r\nContent-Disposition: attachment; filename={name}\r\n"
        "Content-Transfer-Encoding: base64\r\n\r\n"
        f"{base64.b64encode(content).decode()}\r\n--b--\r\n"
    )


# ── Gmail: Alice's personal mail ─────────────────────────────────────────

PERSONAL = "Alice <alice@example.com>"


def personal(labels, subject, body, sender, date, message_id, **kwargs):
    raw = mime(subject, body, sender, PERSONAL, date, message_id, **kwargs)
    encoded = base64.urlsafe_b64encode(raw.encode()).decode().rstrip("=")
    gmail(google, "POST", "/messages/import", {"raw": encoded, "labelIds": labels})


# ── Microsoft 365: Alice's work mail and calendar ────────────────────────


def ms_sign_in(authorize_url):
    """Signs in on the emulator's page; returns where it redirects to."""
    page = machine.succeed(f"curl -sf -o /dev/null -w '%{{redirect_url}}' {shlex.quote(authorize_url)}").strip()
    params = {k: v[0] for k, v in parse_qs(urlparse(page).query).items()}
    form = " ".join(
        f"--data-urlencode {k}={shlex.quote(params[k])}" for k in ["redirect_uri", "scope", "client_id", "response_mode"]
    )
    return machine.succeed(
        f"curl -sf -o /dev/null -w '%{{redirect_url}}' -X POST {MICROSOFT}/oauth2/v2.0/authorize/callback "
        f"--data-urlencode email=alice@example.org {form}"
    ).strip()


def authorize_work():
    machine.wait_for_unit("emulate-microsoft.service")
    machine.wait_for_open_port(4003)
    machine.succeed("touch /tmp/pasted")
    machine.succeed(
        "(tail -f /tmp/pasted | runuser -u uplink -- uplink --config /etc/uplink/uplink.yaml "
        "oauth init --account alice-work >/tmp/oauth-work.log 2>&1) >/dev/null 2>&1 &"
    )
    url = machine.wait_until_succeeds(f"grep -o '{MICROSOFT}/common/oauth2/authorize?[^ ]*' /tmp/oauth-work.log").strip()
    machine.succeed(f"echo {shlex.quote(ms_sign_in(url))} >> /tmp/pasted")
    machine.wait_until_succeeds("grep -q Authorized /tmp/oauth-work.log")


def ms_token():
    redirect = "http://127.0.0.1/redirect"
    url = (
        f"{MICROSOFT}/common/oauth2/authorize?client_id=uplink-demo&response_type=code"
        f"&redirect_uri={redirect}&response_mode=query&resource={MICROSOFT}"
    )
    code = parse_qs(urlparse(ms_sign_in(url)).query)["code"][0]
    return json.loads(machine.succeed(
        f"curl -sf -X POST {MICROSOFT}/common/oauth2/token -d grant_type=authorization_code "
        f"-d code={code} -d client_id=uplink-demo --data-urlencode redirect_uri={redirect}"
    ))["access_token"]


def graph(method, path, body):
    machine.succeed(
        f"curl -sf -X {method} -H 'Authorization: Bearer {ms}' -H 'Content-Type: application/json' "
        f"-d {shlex.quote(json.dumps(body))} {shlex.quote(MICROSOFT + '/v1.0/me' + path)}"
    )


def work(subject, body, sender, date, message_id, **kwargs):
    raw = mime(subject, body, sender, "Alice <alice@example.org>", date, message_id, **kwargs)
    machine.succeed(
        f"curl -sf -X POST -H 'Authorization: Bearer {ms}' -H 'Content-Type: text/plain' "
        f"-d {base64.b64encode(raw.encode()).decode()} {MICROSOFT}/v1.0/me/mailFolders/inbox/messages"
    )


def utc(t):
    return {"dateTime": t.strftime("%Y-%m-%dT%H:%M:%S"), "timeZone": "UTC"}


def event(subject, start, minutes, location=None, recurrence=None):
    body = {"subject": subject, "start": utc(start), "end": utc(start + timedelta(minutes=minutes))}
    if location:
        body["location"] = {"displayName": location}
    if recurrence:
        body["recurrence"] = recurrence
    graph("POST", "/events", body)


# ── Matrix ───────────────────────────────────────────────────────────────

MATRIX = "http://127.0.0.1:6167/_matrix/client/v3"
txn = iter(range(1, 1_000_000))


def matrix(token, method, path, body=None):
    data = f"-d {shlex.quote(json.dumps(body))}" if body is not None else ""
    auth = f"-H 'Authorization: Bearer {token}'" if token else ""
    return json.loads(machine.succeed(f"curl -sf -X {method} {auth} {data} {shlex.quote(MATRIX + path)}"))


def register(user, name):
    token = matrix(None, "POST", "/register", {
        "username": user, "password": "matrix-password", "auth": {"type": "m.login.dummy"},
    })["access_token"]
    matrix(token, "PUT", f"/profile/@{user}:example.org/displayname", {"displayname": name})
    return token


def room(creator, name, members, topic=None, space=False, direct=False):
    body = {"name": name, "invite": [f"@{m}:example.org" for m in members], "preset": "private_chat"}
    if topic:
        body["topic"] = topic
    if space:
        body["creation_content"] = {"type": "m.space"}
    if direct:
        body["is_direct"] = True
        del body["name"]
    room_id = matrix(users[creator], "POST", "/createRoom", body)["room_id"]
    for m in members:
        matrix(users[m], "POST", f"/join/{room_id}", {})
    return room_id


def send(user, room_id, content, kind="m.room.message"):
    return matrix(users[user], "PUT", f"/rooms/{room_id}/send/{kind}/{next(txn)}", content)["event_id"]


def say(user, room_id, body, html=None, **content):
    message = {"msgtype": "m.text", "body": body, **content}
    if html:
        message.update(format="org.matrix.custom.html", formatted_body=html)
    return send(user, room_id, message)


# ── The demo ─────────────────────────────────────────────────────────────

start_all()
authorize_uplink()
authorize_work()
google = provider_token()
ms = ms_token()

labels = {
    name: gmail(google, "POST", "/labels", {"name": name})["id"]
    for name in ["Travel", "Receipts", "Family"]
}
personal(["INBOX"], "Ridge trail on Saturday?",
         "The forecast looks perfect for the ridge trail. Meet at the north trailhead at 8?\n\nMaya",
         "Maya Chen <maya@example.net>", ago(days=2, hours=3), "hike-1@example.net")
personal(["SENT"], "Re: Ridge trail on Saturday?",
         "Count me in! I'll bring snacks and the good thermos.",
         PERSONAL, ago(days=1, hours=5), "hike-2@example.com", in_reply_to="hike-1@example.net")
personal(["INBOX", "UNREAD"], "Re: Ridge trail on Saturday?",
         "Perfect. Tom is coming too, and he says he's bringing Biscuit.\n\nSee you at 8!",
         "Maya Chen <maya@example.net>", ago(hours=3), "hike-3@example.net", in_reply_to="hike-2@example.com")
personal(["INBOX", "UNREAD", labels["Travel"], "IMPORTANT"], "Your trip to Lisbon is confirmed",
         "Booking reference K7Q2XP\n\nLisbon (LIS), Friday 4 December, 09:40\nReturn Monday 7 December, 18:15\n\n"
         "Check-in opens 24 hours before departure.",
         "Skyline Air <bookings@skyline-air.example>", ago(hours=5), "k7q2xp@skyline-air.example")
personal(["INBOX", "STARRED"], "Book club: November pick",
         "We voted, and it's The Dispossessed by Ursula K. Le Guin. We meet on the 28th at my place; "
         "I'll make soup.\n\nPriya",
         "Priya Raman <priya@example.net>", ago(days=1, hours=2), "bookclub@example.net")
personal(["INBOX", labels["Receipts"]], "Invoice #2041",
         "Hi Alice,\n\nPlease find attached the invoice for the October photo session.\n\nNorthwind Studio",
         "Northwind Studio <billing@northwind.example>", ago(days=2, hours=1), "invoice-2041@northwind.example",
         attachment=("invoice-2041.pdf", "application/pdf", b"%PDF-1.4 invoice 2041"))
personal(["INBOX", labels["Family"]], "Sunday dinner",
         "Your aunt is visiting, so we're doing a roast. Come by at 6?\n\nLove, Mom",
         "Mom <mom@example.com>", ago(hours=6), "dinner@example.com")
personal(["INBOX", "UNREAD"], "Issue 212: Shipping small, shipping often",
         "This week: why small pull requests ship faster, a tour of SQLite's query planner, "
         "and a keyboard you can build in an afternoon.",
         "The Weekly Byte <hello@weeklybyte.example>", ago(hours=1), "issue-212@weeklybyte.example")

work("Q4 roadmap review: slides attached",
     "Hi all,\n\nThe slides for Tuesday's review are attached. Please add your team's items to slide 6 "
     "before Monday.\n\nDaniel",
     "Daniel Okafor <daniel@example.org>", ago(minutes=30), "roadmap@example.org",
     attachment=("q4-roadmap.pdf", "application/pdf", b"%PDF-1.4 roadmap"))
work("Design review: new onboarding flow",
     "The prototype is ready for review. The main change: sign-up now takes three steps instead of seven.\n\nSofia",
     "Sofia Marquez <sofia@example.org>", ago(hours=2), "onboarding@example.org")
work("Postmortem published: checkout latency (INC-4721)",
     "The postmortem for INC-4721 is published. Root cause: connection pool exhaustion under peak load. "
     "Action items are assigned.",
     "Incident Bot <incidents@example.org>", ago(hours=4), "inc-4721@example.org")
work("Open enrollment closes Friday",
     "A reminder that benefits open enrollment closes this Friday at 17:00.",
     "People Team <people@example.org>", ago(days=1, hours=4), "enrollment@example.org")

monday = (now - timedelta(days=now.weekday())).replace(hour=0, minute=0)
event("Standup", monday.replace(hour=9, minute=30), 15, recurrence={
    "pattern": {"type": "daily", "interval": 1},
    "range": {"type": "numbered", "startDate": monday.strftime("%Y-%m-%d"), "numberOfOccurrences": 5},
})
event("Focus time", monday.replace(hour=13), 120)
event("Roadmap review", monday + timedelta(days=1, hours=14), 60, location="Room 4B")
event("1:1 with Daniel", monday + timedelta(days=2, hours=11), 30)
event("Design critique", monday + timedelta(days=3, hours=15), 60, location="Studio")
event("Team lunch", monday + timedelta(days=4, hours=12, minutes=30), 60, location="Café Lumen")
event("Incident review", monday + timedelta(days=2, hours=16), 45)

machine.wait_for_unit("conduit.service")
machine.wait_for_open_port(6167)
register("admin", "Admin")  # Conduit makes its first user admin.
# Alice's colleagues are on Matrix itself; everyone else is on another
# network, bridged in as bridges do (mautrix's): each bridge's bot makes a
# space of its chats, whose members are puppets of the other network's.
users = {
    user: register(user, name)
    for user, name in [
        ("alice", "Alice"), ("daniel", "Daniel Okafor"), ("sofia", "Sofia Marquez"),
        ("whatsappbot", "WhatsApp bridge bot"), ("whatsapp_mom", "Mom"), ("whatsapp_dad", "Dad"),
        ("whatsapp_lena", "Lena"), ("whatsapp_maya", "Maya Chen"),
        ("telegrambot", "Telegram bridge bot"), ("telegram_maya", "Maya Chen"), ("telegram_tom", "Tom Becker"),
        ("signalbot", "Signal bridge bot"), ("signal_priya", "Priya Raman"), ("signal_noor", "Noor Haddad"),
        ("slackbot", "Slack bridge bot"), ("slack_ines", "Inês Costa"), ("slack_jonas", "Jonas Weber"),
    ]
}


def upload(user, path):
    return json.loads(machine.succeed(
        f"curl -sf -X POST -H 'Authorization: Bearer {users[user]}' -H 'Content-Type: image/png' "
        f"--data-binary @{path} http://127.0.0.1:6167/_matrix/media/v3/upload"))["content_uri"]


def space(creator, name, members, children, topic=None, avatar=None):
    space_id = room(creator, name, members, topic=topic, space=True)
    if avatar:
        matrix(users[creator], "PUT", f"/rooms/{space_id}/state/m.room.avatar", {"url": avatar})
    for child in children:
        matrix(users[creator], "PUT", f"/rooms/{space_id}/state/m.space.child/{child}", {"via": ["example.org"]})


icons = {}
for bridge in ["whatsapp", "telegram", "signal", "slack"]:
    icons[bridge] = upload(f"{bridge}bot", f"{BRIDGE_ICONS}/{bridge}.png")
    matrix(users[f"{bridge}bot"], "PUT", f"/profile/@{bridge}bot:example.org/avatar_url", {"avatar_url": icons[bridge]})

platform = room("alice", "Platform", ["daniel", "sofia"], topic="Deploys, incidents and infrastructure")
design = room("alice", "Design", ["sofia"], topic="Critiques and prototypes")
space("alice", "Acme", ["daniel", "sofia"], [platform, design])
family = room("whatsappbot", "Family chat", ["alice", "whatsapp_mom", "whatsapp_dad", "whatsapp_lena"])
maya = room("whatsapp_maya", "", ["alice"], direct=True)
space("whatsappbot", "WhatsApp", ["alice"], [family, maya], topic="Your WhatsApp bridged chats",
      avatar=icons["whatsapp"])
hiking = room("telegrambot", "Hiking club", ["alice", "telegram_maya", "telegram_tom"], topic="Weekend trails")
space("telegrambot", "Telegram", ["alice"], [hiking], topic="Your Telegram bridged chats",
      avatar=icons["telegram"])
books = room("signalbot", "Book club", ["alice", "signal_priya", "signal_noor"])
space("signalbot", "Signal", ["alice"], [books], topic="Your Signal bridged chats",
      avatar=icons["signal"])
meetup = room("slackbot", "rust-meetup", ["alice", "slack_ines", "slack_jonas"])
space("slackbot", "Slack", ["alice"], [meetup], topic="Your Slack bridged chats",
      avatar=icons["slack"])
matrix(users["alice"], "PUT", "/user/@alice:example.org/account_data/m.direct", {"@whatsapp_maya:example.org": [maya]})

for user, body in [
    ("daniel", "Release 4.2 is tagged, the changelog is in the usual place"),
    ("sofia", "I'll keep an eye on the checkout dashboards while it rolls out"),
]:
    say(user, platform, body)
release = say("daniel", platform, "Release 4.2 is rolling out to production now 🚀")
send("sofia", platform, {"m.relates_to": {"rel_type": "m.annotation", "event_id": release, "key": "🎉"}}, "m.reaction")
say("daniel", platform, "If anything looks off, roll back with: deploy rollback --to 4.1.3",
    html="If anything looks off, roll back with:<pre><code>deploy rollback --to 4.1.3</code></pre>")
say("sofia", design, "New onboarding prototype is up, three steps instead of seven. Feedback welcome!")

for user, body in [
    ("whatsapp_mom", "Is everyone coming on Sunday?"),
    ("whatsapp_lena", "Yes! I'm bringing dessert 🍰"),
    ("whatsapp_dad", "I'll pick your aunt up from the station at 4"),
    ("alice", "Wouldn't miss it. Should I bring anything?"),
]:
    say(user, family, body)
wine = say("whatsapp_mom", family, "Just yourselves. And maybe some wine 🍷")
send("whatsapp_dad", family, {"m.relates_to": {"rel_type": "m.annotation", "event_id": wine, "key": "😂"}}, "m.reaction")
say("whatsapp_dad", family, "I'm doing the roast this time, so expect it to be *excellent*",
    html="I'm doing the roast this time, so expect it to be <em>excellent</em>",
    **{"m.relates_to": {"m.in_reply_to": {"event_id": wine}}})
train = say("whatsapp_lena", family, "My train gets in at 17:40, can someone pick me up?")
lift = say("alice", family, "I can, I'll be driving past anyway",
           **{"m.relates_to": {"m.in_reply_to": {"event_id": train}}})
for user in ["whatsapp_lena", "whatsapp_mom"]:
    send(user, family, {"m.relates_to": {"rel_type": "m.annotation", "event_id": lift, "key": "❤️"}}, "m.reaction")
say("whatsapp_lena", family, "You're the best 🙏")

for user, body in [
    ("telegram_maya", "Who's in for the ridge trail this weekend?"),
    ("telegram_tom", "Me! Saturday works better than Sunday"),
    ("alice", "Saturday's good for me too"),
    ("telegram_maya", "Saturday it is. It's about 14 km, with the climb in the first half"),
    ("telegram_tom", "Is the hut at the top open this time of year?"),
    ("telegram_maya", "Only on Sundays, so bring lunch"),
    ("alice", "I'll bring the good thermos ☕"),
    ("telegram_maya", "Trailhead at 8 on Saturday. Bring layers, it's cold up top ⛰️"),
    ("telegram_tom", "I'm bringing Biscuit 🐕"),
    ("telegram_maya", "Of course you are 😄"),
]:
    say(user, hiking, body)
say("signal_priya", books, "Halfway through The Dispossessed and I already want to argue about Anarres")
say("signal_noor", books, "No spoilers, I'm only on chapter 3!")
say("slack_ines", meetup, "Next meetup is on the 19th, who wants to give a talk?")
say("slack_jonas", meetup, "I could do 20 minutes on the async pitfalls we hit at work")
say("whatsapp_maya", maya, "Did you see the forecast? Clear skies all weekend ☀️")

machine.succeed(
    "printf 'matrix-password\n\n' | runuser -u uplink -- "
    "uplink --config /etc/uplink/uplink.yaml matrix pair --account alice-matrix >/tmp/pair.log 2>&1"
)
start_uplink()


def count(query):
    return int(machine.succeed(f"sqlite3 {STATE}/db/uplink.sqlite {shlex.quote(query)}").strip())


eventually("the mail to be synced", lambda: count("SELECT count(*) FROM message") >= 12, timeout=120)
eventually("the calendar to be synced", lambda: count("SELECT count(*) FROM calendar_event") >= 6, timeout=120)
eventually("the rooms to be synced", lambda: count("SELECT count(*) FROM matrix_room") >= 12, timeout=120)

machine.succeed("uplink-shots >&2")
machine.copy_from_vm("/tmp/screenshots", "docs")
