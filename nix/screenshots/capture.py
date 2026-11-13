"""Takes the README's screenshots of uplink's web UI, light and dark, into
/tmp/screenshots; first does what a user would, so that the to-dos, the
history and the outbox have something in them.

  uplink-shots
"""

import os
from datetime import datetime, time as dtime, timedelta
from urllib.parse import quote

from playwright.sync_api import expect, sync_playwright

URL = os.environ["UPLINK_URL"]
SHOTS = "/tmp/screenshots"
THEMES = ["light", "dark"]
os.makedirs(SHOTS, exist_ok=True)


def rows(page):
    return page.get_by_role("list", name="Messages").get_by_role("listitem")


def subjects(page):
    return [row.locator("[class*=subject]").inner_text() for row in rows(page).all()]


def inbox(page):
    page.goto(URL + "/inbox")
    expect(rows(page)).not_to_have_count(0, timeout=30000)


def focus(page, subject):
    """Moves the focus to the conversation whose subject has `subject`,
    from the first."""
    shown = subjects(page)
    [index] = [i for i, s in enumerate(shown) if subject in s] or [shown]
    for _ in range(index):
        page.keyboard.press("j")


def reply(page):
    """Alice's reply to Maya, in its HTML body's frame."""
    return page.frame_locator("iframe[title='Message body']").get_by_text("Wouldn't miss it")


def shoot(page, name, theme):
    # No hover toolbar left on what was clicked or tapped last.
    page.mouse.move(0, 0)
    page.wait_for_timeout(500)
    page.screenshot(path=f"{SHOTS}/{name}-{theme}.png")


def act(page):
    """Archives the newsletter, flags the roadmap, adds to-dos, and writes
    a reply to Maya, shot as it is written, then sent."""
    inbox(page)
    newsletter = "Issue 212: Shipping small, shipping often"
    focus(page, newsletter)
    page.keyboard.press("e")
    expect(rows(page).filter(has_text=newsletter)).to_have_count(0)

    inbox(page)
    focus(page, "Q4 roadmap review: slides attached")
    page.keyboard.press("Shift+T")
    page.keyboard.press("g")
    page.keyboard.press("k")
    items = page.get_by_role("list", name="To-do list").get_by_role("listitem")
    expect(items).to_have_count(1)
    for title in ["Renew passport before Lisbon", "Buy trail snacks"]:
        page.keyboard.press("a")
        page.keyboard.type(title)
        page.keyboard.press("Enter")
        page.keyboard.press("Escape")
    expect(items).to_have_count(3)

    inbox(page)
    focus(page, "Ridge trail on Saturday?")
    page.keyboard.press("Enter")
    expect(page.get_by_role("article").last).to_contain_text("Biscuit")
    page.keyboard.press("r")
    expect(page.get_by_role("textbox", name="Subject")).to_have_value("Re: Ridge trail on Saturday?")
    page.keyboard.type("Wouldn't miss it. ")
    page.keyboard.press("Control+b")
    page.keyboard.type("See you at 8!")
    expect(page.get_by_text("Saved")).to_be_visible()
    for theme in THEMES:
        page.emulate_media(color_scheme=theme)
        shoot(page, "compose", theme)
    page.emulate_media(color_scheme="light")
    page.keyboard.press("Control+Enter")
    expect(page.get_by_role("status")).to_contain_text("Sent")
    # Once sent and synced back, the outbox is empty again.
    inbox(page)
    focus(page, "Ridge trail on Saturday?")
    page.keyboard.press("Enter")
    expect(reply(page)).to_be_visible(timeout=60000)


def views(page, theme):
    inbox(page)
    shoot(page, "inbox", theme)

    focus(page, "Ridge trail on Saturday?")
    page.keyboard.press("Enter")
    expect(reply(page)).to_be_visible()
    shoot(page, "conversation", theme)

    page.goto(URL + "/search/" + quote("has:attachment OR label:travel", safe=""))
    expect(rows(page)).to_have_count(3, timeout=10000)
    shoot(page, "search", theme)

    inbox(page)
    page.get_by_role("navigation").get_by_role("link", name="Family chat").click()
    expect(page.get_by_role("list", name="Timeline")).to_contain_text("You're the best", timeout=15000)
    shoot(page, "room", theme)

    page.goto(URL + "/history")
    expect(page.get_by_role("list", name="History").get_by_role("listitem")).not_to_have_count(0)
    shoot(page, "history", theme)


def calendar(page, theme):
    # Tuesday of the week seeded, mid-morning: the week opens an hour
    # before, at the working day, its time marked.
    today = datetime.now()
    page.clock.install(time=datetime.combine(today - timedelta(days=today.weekday() - 1), dtime(10, 40)))
    page.goto(URL + "/calendar")
    page.get_by_role("link", name="Roadmap review").click()
    expect(page.get_by_role("link", name="Join online meeting")).to_be_visible(timeout=10000)
    shoot(page, "calendar", theme)


def phone(page, theme):
    inbox(page)
    shoot(page, "mobile-inbox", theme)
    page.goto(URL + "/inbox")
    page.get_by_role("button", name="Menu").click()
    page.get_by_role("navigation").get_by_role("link", name="Hiking club").click()
    expect(page.get_by_role("list", name="Timeline")).to_contain_text("Biscuit", timeout=15000)
    shoot(page, "mobile-room", theme)


with sync_playwright() as p:
    browser = p.chromium.launch()

    def context(theme, **viewport):
        return browser.new_context(
            http_credentials={"username": os.environ["UPLINK_USER"], "password": os.environ["UPLINK_PASSWORD"]},
            ignore_https_errors=True,
            color_scheme=theme,
            **viewport,
        )

    # Desktop shots are of a small window, so that what they show is large
    # on the page, at twice its resolution, so that it is sharp. The
    # calendar's week needs a wider one, for its seven days; it is taken at
    # the same size.
    desktop = {"viewport": {"width": 1024, "height": 640}, "device_scale_factor": 2}
    act(context("light", **desktop).new_page())
    for theme in THEMES:
        views(context(theme, **desktop).new_page(), theme)
        calendar(context(theme, viewport={"width": 1280, "height": 800}, device_scale_factor=1.6).new_page(), theme)
        phone(context(theme, viewport={"width": 390, "height": 844}, device_scale_factor=2, is_mobile=True,
                      has_touch=True).new_page(), theme)
    browser.close()
