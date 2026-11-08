"""Drives uplink's web UI in Chromium, as a user would with the keyboard.

  uplink-ui STEP

Each step starts from the Inbox and ends by printing a JSON summary; the
test script checks the Gmail side between steps. Screenshots go to
/tmp/screenshots.
"""

import json
import os
import sys

from playwright.sync_api import expect, sync_playwright

SHOTS = "/tmp/screenshots"
os.makedirs(SHOTS, exist_ok=True)


def subjects(page):
    rows = page.get_by_role("list", name="Messages").get_by_role("listitem")
    return [row.locator("[class*=subject]").inner_text() for row in rows.all()]


def inbox(page):
    page.goto(os.environ["UPLINK_URL"] + "/")
    expect(page.get_by_role("list", name="Messages").get_by_role("listitem")).not_to_have_count(0, timeout=30000)


def read(page):
    """Opens the second message, then goes back."""
    page.keyboard.press("j")
    page.keyboard.press("Enter")
    expect(page.get_by_role("article")).to_contain_text("quixotic")
    page.screenshot(path=f"{SHOTS}/message.png")
    page.keyboard.press("u")
    expect(page.get_by_role("list", name="Messages")).to_be_visible()
    return {"subjects": subjects(page)}


def archive(page):
    """Archives the second message."""
    before = len(subjects(page))
    page.keyboard.press("j")
    page.keyboard.press("e")
    expect(page.get_by_role("list", name="Messages").get_by_role("listitem")).to_have_count(before - 1)
    expect(page.get_by_role("status")).to_contain_text("Archived 1 message")
    page.screenshot(path=f"{SHOTS}/archived.png")
    return {"subjects": subjects(page), "notice": page.get_by_role("status").inner_text()}


def undo(page):
    """Archives the second message and undoes it from the notice."""
    before = len(subjects(page))
    page.keyboard.press("j")
    page.keyboard.press("e")
    rows = page.get_by_role("list", name="Messages").get_by_role("listitem")
    expect(rows).to_have_count(before - 1)
    page.get_by_role("status").get_by_role("button", name="Undo").click()
    expect(rows).to_have_count(before)
    return {"subjects": subjects(page)}


def label(page):
    """Selects the first two messages and labels them Projects."""
    page.keyboard.press("x")
    page.keyboard.press("j")
    page.keyboard.press("x")
    page.keyboard.press("l")
    page.keyboard.type("proj")
    page.screenshot(path=f"{SHOTS}/picker.png")
    page.keyboard.press("Enter")
    expect(page.get_by_role("list", name="Messages").get_by_text("Projects")).to_have_count(2)
    page.screenshot(path=f"{SHOTS}/labelled.png")
    return {"subjects": subjects(page)}


def search(page):
    """Searches for a word in one message's body, and reloads the result."""
    page.keyboard.press("/")
    page.keyboard.type("quixot")
    page.keyboard.press("Enter")
    rows = page.get_by_role("list", name="Messages").get_by_role("listitem")
    expect(rows).to_have_count(1)
    page.reload()
    expect(rows).to_have_count(1, timeout=10000)
    page.keyboard.press("?")
    expect(page.get_by_role("dialog", name="Keyboard shortcuts")).to_be_visible()
    page.screenshot(path=f"{SHOTS}/search.png")
    return {"path": page.url.split("/", 3)[3], "subjects": subjects(page)}


def reply(page):
    """Finds the (archived) lunch message, replies to it, and sends."""
    page.keyboard.press("/")
    page.keyboard.type("quixot")
    page.keyboard.press("Enter")
    expect(page.get_by_role("list", name="Messages").get_by_role("listitem")).to_have_count(1)
    page.keyboard.press("Enter")
    expect(page.get_by_role("article")).to_contain_text("quixotic")
    page.keyboard.press("r")
    expect(page.get_by_role("textbox", name="Subject")).to_have_value("Re: Lunch on Friday")
    # Writing starts in the body, above the quote.
    page.keyboard.type("Friday works. ")
    page.keyboard.press("Control+b")
    page.keyboard.type("See you there!")
    expect(page.get_by_text("Saved")).to_be_visible()
    page.screenshot(path=f"{SHOTS}/compose.png")
    page.keyboard.press("Control+Enter")
    expect(page.get_by_role("status")).to_contain_text("Sent 'Re: Lunch on Friday'")
    page.goto(os.environ["UPLINK_URL"] + "/outbox")
    expect(page.get_by_text("Loading…")).to_have_count(0)
    page.screenshot(path=f"{SHOTS}/outbox.png")
    return {"path": page.url.split("/", 3)[3]}


def matrix(page):
    """Opens the Planning room, reads Bob's message, and answers."""
    page.get_by_role("navigation").get_by_role("link", name="Planning").click()
    timeline = page.get_by_role("list", name="Timeline")
    expect(timeline).to_contain_text("Hi from Bob")
    expect(timeline).to_contain_text("org.example.custom")
    page.keyboard.press("r")
    page.keyboard.type("Hello from uplink")
    page.keyboard.press("Enter")
    expect(page.get_by_role("status")).to_contain_text("Sent 'Hello from uplink'")
    # Sent, and synced back: no longer pending, but in the timeline.
    expect(timeline.locator("[class*=pending]")).to_have_count(0, timeout=15000)
    expect(timeline.get_by_text("Hello from uplink")).to_have_count(1)
    timeline.locator("summary", has_text="org.example.custom").click()
    expect(timeline).to_contain_text('"answer": 42')
    page.screenshot(path=f"{SHOTS}/room.png")
    return {"path": page.url.split("/", 3)[3]}


def todos(page):
    """Flags the first message, then adds a to-do of its own in To-dos."""
    first = subjects(page)[0]
    page.keyboard.press("Shift+T")
    rows = page.get_by_role("list", name="Messages").get_by_role("listitem")
    expect(rows.first.get_by_title("Flagged", exact=False)).to_be_visible()
    page.keyboard.press("g")
    page.keyboard.press("k")
    items = page.get_by_role("list", name="To-do list").get_by_role("listitem")
    expect(items).to_have_count(1)
    page.keyboard.press("a")
    page.keyboard.type("Book the train")
    page.keyboard.press("Enter")
    page.keyboard.press("Escape")
    expect(items).to_have_count(2)
    page.screenshot(path=f"{SHOTS}/todos.png")
    titles = [item.locator("[class*=title]").inner_text() for item in items.all()]
    # The flagged one opens its message.
    page.keyboard.press("Enter")
    expect(page.get_by_role("heading", name=first)).to_be_visible()
    return {"flagged": first, "todos": titles}


def history(page):
    """Lists what was done, then undoes the latest with z."""
    page.keyboard.press("g")
    page.keyboard.press("h")
    items = page.get_by_role("list", name="History").get_by_role("listitem")
    expect(items).not_to_have_count(0)
    before = items.count()
    latest = items.first.locator("[class*=description]").inner_text()
    page.keyboard.press("z")
    expect(items).to_have_count(before + 1)
    expect(items.nth(1)).to_contain_text("undone")
    page.screenshot(path=f"{SHOTS}/history.png")
    return {
        "undid": latest,
        "descriptions": [item.locator("[class*=description]").inner_text() for item in items.all()],
        "undone": [item.locator("[class*=description]").inner_text() for item in items.filter(has_text="undone").all()],
    }


STEPS = {f.__name__: f for f in [read, archive, undo, label, search, reply, matrix, todos, history]}

step = STEPS[sys.argv[1]]
with sync_playwright() as p:
    browser = p.chromium.launch()
    context = browser.new_context(
        http_credentials={"username": os.environ["UPLINK_USER"], "password": os.environ["UPLINK_PASSWORD"]},
        ignore_https_errors=True,
        viewport={"width": 1200, "height": 600},
    )
    page = context.new_page()
    errors = []
    page.on("console", lambda m: m.type in ("error", "warning") and errors.append(m.text))
    page.on("pageerror", lambda e: errors.append(str(e)))
    inbox(page)
    result = step(page)
    browser.close()
    assert not errors, errors
    print(json.dumps(result))
