"""Drives uplink's web UI in Chromium, or another browser of Playwright's
(`firefox`), as a user would with the keyboard.

  uplink-ui STEP [BROWSER]

Each step starts from the Inbox and ends by printing a JSON summary; the
test script checks the Gmail side between steps. Screenshots go to
/tmp/screenshots.
"""

import base64
import json
import os
import sys
from urllib.parse import unquote

from playwright.sync_api import expect, sync_playwright

SHOTS = "/tmp/screenshots"
os.makedirs(SHOTS, exist_ok=True)


def subjects(page):
    rows = page.get_by_role("list", name="Messages").get_by_role("listitem")
    return [row.locator("[class*=subject]").inner_text() for row in rows.all()]


def inbox(page):
    page.goto(os.environ["UPLINK_URL"] + "/inbox")
    expect(page.get_by_role("list", name="Messages").get_by_role("listitem")).not_to_have_count(0, timeout=30000)


def read(page):
    """Opens the second message, then goes back."""
    page.keyboard.press("j")
    page.keyboard.press("Enter")
    expect(page.get_by_role("article")).to_contain_text("quixotic")
    page.screenshot(path=f"{SHOTS}/message.png")
    page.emulate_media(color_scheme="dark")
    page.screenshot(path=f"{SHOTS}/message-dark.png")
    page.emulate_media(color_scheme="light")
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
    """Joins the Planning room it is invited to, reads Bob's message, and
    answers."""
    page.get_by_role("navigation").get_by_role("link", name="Planning").click()
    page.get_by_role("button", name="Join").click()
    expect(page.get_by_role("button", name="Leave")).to_be_visible(timeout=15000)
    timeline = page.get_by_role("list", name="Timeline")
    expect(timeline).to_contain_text("Hi from Bob")
    expect(timeline).to_contain_text("org.example.custom")
    # Joined, the room's box has the focus.
    expect(page.get_by_role("textbox", name="Message Planning")).to_be_focused()
    page.keyboard.type("Hello from uplink")
    page.keyboard.press("Enter")
    # Shown in the room, it has no notice: the join's stays.
    expect(timeline.get_by_text("Hello from uplink")).to_have_count(1)
    expect(page.get_by_role("status")).to_contain_text("Joined 'Planning'")
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
    page.keyboard.press("z")
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


def trash(page):
    """Moves the first message to Trash with #, then shows the Trash."""
    first = subjects(page)[0]
    before = len(subjects(page))
    page.keyboard.press("#")
    rows = page.get_by_role("list", name="Messages").get_by_role("listitem")
    expect(rows).to_have_count(before - 1)
    expect(page.get_by_role("status")).to_contain_text("Moved 1 message to Trash")
    page.get_by_role("navigation").get_by_role("link", name="Trash").click()
    expect(rows).to_have_count(1)
    return {"trashed": first, "trash": subjects(page)}


def attachment(page):
    """Opens the message with a file attached, downloads the file, and
    forwards the message with another file attached."""
    rows = page.get_by_role("list", name="Messages").get_by_role("listitem")
    rows.filter(has_text="Quarterly figures").click()
    files = page.get_by_role("list", name="Attachments")
    expect(files).to_contain_text("figures.pdf")
    page.screenshot(path=f"{SHOTS}/attachment.png")
    with page.expect_download() as download:
        files.get_by_role("link", name="figures.pdf").click()
    path = download.value.path()
    with open(path, "rb") as f:
        content = f.read().decode()

    # Forwarded with its file, and one more attached.
    page.keyboard.press("f")
    expect(page.get_by_role("textbox", name="Subject")).to_have_value("Fwd: Quarterly figures")
    attached = page.get_by_role("list", name="Attachments")
    expect(attached).to_contain_text("figures.pdf")
    page.keyboard.type("carol@example.net")
    with open("/tmp/notes.txt", "w") as f:
        f.write("Notes on the figures")
    page.get_by_label("Files to attach").set_input_files("/tmp/notes.txt")
    expect(attached).to_contain_text("notes.txt")
    expect(page.get_by_text("Saved")).to_be_visible()
    page.screenshot(path=f"{SHOTS}/compose-attachments.png")
    page.keyboard.press("Control+Enter")
    expect(page.get_by_role("status")).to_contain_text("Sent 'Fwd: Quarterly figures'")
    return {"name": download.value.suggested_filename, "content": content}


def tabs(page):
    """Writes one draft in two tabs: each sees what the other types, even
    when both type at once."""
    page.keyboard.press("c")
    subject = page.get_by_role("textbox", name="Subject")
    expect(subject).to_be_visible()
    other = page.context.new_page()
    errors = []
    other.on("console", lambda m: m.type in ("error", "warning") and errors.append(f"{m.text} at {m.location['url']}"))
    other.on("pageerror", lambda e: errors.append(str(e)))
    other.goto(page.url)
    other_subject = other.get_by_role("textbox", name="Subject")
    expect(other_subject).to_be_visible()
    body = page.get_by_label("Body")
    other_body = other.get_by_label("Body")

    subject.click()
    page.keyboard.type("Plans")
    expect(other_subject).to_have_value("Plans")
    other_body.locator(".ProseMirror").click()
    other.keyboard.type("Typed in the other tab")
    expect(body).to_contain_text("Typed in the other tab")

    # Both at once, within the moment before either is saved.
    page.keyboard.type(" for Friday")
    other.keyboard.type(", and more")
    for p in [page, other]:
        expect(p.get_by_role("textbox", name="Subject")).to_have_value("Plans for Friday")
        expect(p.get_by_label("Body")).to_have_text("Typed in the other tab, and more")
        expect(p.get_by_text("Saved")).to_be_visible()
    page.screenshot(path=f"{SHOTS}/compose-tabs.png")
    assert not errors, errors
    return {"draft": page.url.rsplit("/", 1)[1]}


def home(page):
    """Goes home: the rows of each bucket."""
    page.get_by_role("link", name="Home").click()
    region = page.get_by_role("region", name="Home")
    expect(region.get_by_role("region", name="Backlog").get_by_role("listitem")).not_to_have_count(0)
    page.screenshot(path=f"{SHOTS}/home.png")
    buckets = region.get_by_role("region")
    return {
        b.get_attribute("aria-label"): [row.inner_text() for row in b.get_by_role("listitem").all()]
        for b in buckets.all()
    }


def hostile(page):
    """Opens the hostile mail, shows its remote images, and follows its
    link: what it runs, where it navigates, and what is requested, as
    the frame's own requests and anyone's beyond uplink."""
    origin = os.environ["UPLINK_URL"]
    dialogs = []
    page.on("dialog", lambda d: (dialogs.append(d.message), d.dismiss()))
    # What reaches the network: not the "request" event, which Chromium
    # fires for requests the CSP then blocks. uplink's fetches of remote
    # images answer with a pixel; the sender's site with a page.
    requests = []
    pixel = base64.b64decode("R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7")

    def sent(route):
        requests.append(route.request)
        url = route.request.url
        if url.startswith(f"{origin}/api/remote-image?"):
            route.fulfill(content_type="image/gif", body=pixel)
        elif url.startswith("http://t.example/"):
            route.fulfill(content_type="text/html", body="<p>sender")
        else:
            route.continue_()

    page.context.route("**/*", sent)
    title = page.title()

    rows = page.get_by_role("list", name="Messages").get_by_role("listitem")
    rows.filter(has_text="Hostile").click()
    body = page.frame_locator("iframe[title='Message body']")
    expect(body.get_by_text("Hostile content")).to_be_visible()
    expect(page.get_by_text("Remote images are not shown.")).to_be_visible()
    page.wait_for_timeout(2000)

    def framed():
        return sorted(r.url for r in requests if r.frame.parent_frame is not None)

    before = framed()
    page.get_by_role("button", name="Show them").click()
    expect(page.get_by_text("Remote images are not shown.")).to_be_hidden()
    page.wait_for_timeout(2000)
    shown = framed()

    with page.expect_popup() as popup:
        body.get_by_role("link", name="A link").click()
    popup.value.wait_for_load_state()
    followed = {
        "url": popup.value.url,
        "opener": popup.value.evaluate("window.opener !== null"),
        "referer": popup.value.evaluate("document.referrer"),
        # The sender's page, of another site, with the browser's
        # credentials for uplink: neither its WebSocket nor its images.
        "reached": popup.value.evaluate(
            """async (origin) => await Promise.all([
                new Promise((done) => {
                    const ws = new WebSocket(origin.replace(/^http/, 'ws') + '/ws', 'uplink.v1')
                    ws.onopen = () => done('websocket')
                    ws.onerror = () => done(null)
                }),
                new Promise((done) => {
                    const img = new Image()
                    img.onload = () => done('image')
                    img.onerror = () => done(null)
                    img.src = origin + '/logo.svg'
                }),
            ])""",
            origin,
        ),
    }
    page.screenshot(path=f"{SHOTS}/hostile-{page.context.browser.browser_type.name}.png")

    # The app's keys, with the focus in the frame, on the link followed:
    # Escape leaves it, and `u` goes back to the list.
    page.bring_to_front()
    body.get_by_role("link", name="A link").focus()
    in_frame = page.evaluate("() => document.activeElement.tagName")
    page.keyboard.press("Escape")
    keys = {"in frame": in_frame, "escaped": page.evaluate("() => document.activeElement.tagName")}
    body.get_by_role("link", name="A link").focus()
    page.keyboard.press("u")
    expect(page.get_by_role("list", name="Messages")).to_be_visible()

    # The frame's CSP, refusing what it was meant to (Firefox's words for
    # a font refused: "content blocked").
    refused = ["Content-Security-Policy", "Content Security Policy", "content blocked"]
    errors[:] = [e for e in errors if not any(r in e for r in refused)]
    return {
        "before": before,
        "shown": [unquote(url.removeprefix(f"{origin}/api/remote-image?url=")) for url in shown],
        "beyond": sorted({r.url for r in requests if not r.url.startswith(origin + "/")}),
        "followed": followed,
        "dialogs": dialogs,
        "title": page.title() == title,
        "pages": len(page.context.pages),
        "keys": keys,
    }


STEPS = {f.__name__: f for f in [read, archive, undo, label, search, reply, matrix, todos, home, history, trash, attachment, tabs, hostile]}

step = STEPS[sys.argv[1]]
with sync_playwright() as p:
    browser = getattr(p, sys.argv[2] if len(sys.argv) > 2 else "chromium").launch()
    context = browser.new_context(
        http_credentials={"username": os.environ["UPLINK_USER"], "password": os.environ["UPLINK_PASSWORD"]},
        ignore_https_errors=True,
        viewport={"width": 1200, "height": 600},
    )
    page = context.new_page()
    errors = []
    page.on("console", lambda m: m.type in ("error", "warning") and errors.append(f"{m.text} at {m.location['url']}"))
    page.on("pageerror", lambda e: errors.append(str(e)))
    inbox(page)
    result = step(page)
    browser.close()
    assert not errors, errors
    print(json.dumps(result))
