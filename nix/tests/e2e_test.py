# The web UI in a real browser against the real server and emulated Gmail
# (SPEC §15.10 `e2e`). gmail-machine.nix prepends gmail_helpers.py.


def ui(step):
    return json.loads(machine.succeed(f"uplink-ui {step}"))


def subject_of(id):
    headers = gmail(token, "GET", f"/messages/{id}?format=metadata&metadataHeaders=Subject")["payload"]["headers"]
    return next(h["value"] for h in headers if h["name"] == "Subject")


def inbox_subjects():
    messages = gmail(token, "GET", "/messages?labelIds=INBOX").get("messages", [])
    return {subject_of(m["id"]) for m in messages}


start_all()
authorize_uplink()
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

machine.copy_from_vm("/tmp/screenshots", "")
