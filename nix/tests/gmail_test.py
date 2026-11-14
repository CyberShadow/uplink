# End-to-end test of Gmail sync over the WebSocket protocol (SPEC §14 M4).
# gmail-machine.nix prepends gmail_helpers.py, with its imports.


def ws(body):
    """One request to uplink, as the UI makes it."""
    return json.loads(machine.succeed(f"uplink-ws request {shlex.quote(json.dumps(body))}"))


def account():
    [account] = ws({"type": "list_accounts"})["accounts"]
    return account


def label_ids():
    return {label["name"]: label["id"] for label in account()["labels"]}


def entries(label):
    """The latest messages, by subject, of the label's (by id) conversations."""
    [name] = [l["name"] for l in account()["labels"] if l["id"] == label]
    query = f"label:{json.dumps(name, ensure_ascii=False)}"
    page = ws({"type": "list_entries", "query": query, "after": None, "limit": 100})
    return {c["latest"]["subject"]: c["latest"] for c in page["conversations"]}


def tx_log_head():
    return int(machine.succeed(f"sqlite3 {STATE}/db/uplink.sqlite 'SELECT max(id) FROM tx_log'"))


start_all()

with subtest("uplink oauth init stores a token"):
    authorize_uplink()

token = provider_token()

with subtest("the first sync mirrors the mailbox"):
    report = deliver(token, "Quarterly report", "Numbers are up this quarter")
    start_uplink()
    eventually("the first sync", lambda: account()["synced"])
    labels = label_ids()
    assert "Quarterly report" in entries(labels["INBOX"])

with subtest("new mail reaches the client within a poll interval"):
    machine.succeed("uplink-ws await entries_added 10 >/tmp/await.json 2>/tmp/await.err &")
    machine.wait_until_succeeds("grep -q connected /tmp/await.err")
    lunch = deliver(token, "Lunch on Friday", "Shall we try the quixotic place downtown")
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
    eventually("the archive to reach Gmail", lambda: "INBOX" not in gmail_labels(token, report))
    ws({"type": "undo", "tx_log_id": archived})
    eventually("the undo to reach Gmail", lambda: "INBOX" in gmail_labels(token, report))
    assert "Quarterly report" in entries(labels["INBOX"])

with subtest("a label change made in Gmail shows up"):
    projects = gmail(token, "POST", "/labels", {"name": "Projects"})["id"]
    gmail(token, "POST", f"/messages/{lunch}/modify", {"addLabelIds": [projects], "removeLabelIds": ["UNREAD"]})
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
        found = {c["latest"]["subject"] for c in ws({"type": "list_entries", "query": query, "after": None, "limit": 10})["conversations"]}
        assert found == expected, (query, found)

with subtest("a restart loses nothing and resumes from the stored historyId"):
    before = set(entries(labels["INBOX"]))
    machine.systemctl("restart uplink.service")
    machine.wait_for_open_port(8443)
    assert set(entries(labels["INBOX"])) == before
    head = tx_log_head()
    time.sleep(12)  # time for the watch to begin, and the sync it starts
    assert tx_log_head() == head, "the restarted server re-synced"
    deliver(token, "After the restart", "still here")
    eventually("mail after the restart", lambda: "After the restart" in entries(labels["INBOX"]))
    machine.fail("journalctl -u uplink.service | grep -q 'history expired'")
    # Gmail's push notifications came through all along.
    machine.fail("journalctl -u uplink.service | grep -q 'watching for changes'")
    eml = int(machine.succeed(f"find {STATE}/mail -name '*.eml' | wc -l"))
    assert eml == 3, eml
