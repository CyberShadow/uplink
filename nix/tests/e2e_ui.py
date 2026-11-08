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


STEPS = {f.__name__: f for f in [read, archive, undo, label, search]}

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
