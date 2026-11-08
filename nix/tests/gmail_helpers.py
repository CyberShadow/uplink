# Shared by the VM tests on gmail-machine.nix, which prepends GOOGLE, the
# emulator's base URL. The test driver provides `machine` and `subtest`.

import base64
import json
import shlex
import time
from urllib.parse import parse_qs, urlparse

STATE = "/var/lib/uplink"
SCOPE = "https://www.googleapis.com/auth/gmail.modify"


def eventually(what, f, timeout=30):
    deadline = time.monotonic() + timeout
    while True:
        result = f()
        if result:
            return result
        if time.monotonic() > deadline:
            raise Exception(f"timed out waiting for {what}")
        time.sleep(0.5)


def sign_in(params):
    """Signs in on the emulator's consent form, as a browser would;
    returns where Google redirects to."""
    form = " ".join(
        f"--data-urlencode {k}={shlex.quote(v)}" for k, v in params.items()
    )
    return machine.succeed(
        f"curl -sf -o /dev/null -w '%{{redirect_url}}' -X POST {GOOGLE}/o/oauth2/v2/auth/callback "
        f"--data-urlencode email=alice@example.com {form}"
    ).strip()


def authorize_uplink():
    """Makes a master key and runs `uplink oauth init`, consenting in the
    emulator."""
    machine.wait_for_unit("emulate.service")
    machine.wait_for_open_port(4002)
    machine.succeed(f"install -d -m 0700 -o uplink -g uplink {STATE}")
    machine.succeed(f"runuser -u uplink -- sh -c 'uplink keygen > {STATE}/master.key'")
    machine.succeed(
        "runuser -u uplink -- sh -c 'uplink --config /etc/uplink/uplink.yaml "
        "oauth init --account alice-gmail </dev/null >/tmp/oauth.log 2>&1 &'"
    )
    url = machine.wait_until_succeeds(f"grep -o '{GOOGLE}/o/oauth2/v2/auth?[^ ]*' /tmp/oauth.log").strip()
    params = {k: v[0] for k, v in parse_qs(urlparse(url).query).items()}
    assert params["scope"] == SCOPE, params
    assert params["code_challenge_method"] == "S256", params
    redirect = sign_in({
        k: params[k]
        for k in ["redirect_uri", "scope", "state", "client_id", "code_challenge", "code_challenge_method"]
    })
    assert redirect.startswith(params["redirect_uri"]), redirect
    machine.succeed(f"curl -sf {shlex.quote(redirect)}")
    machine.wait_until_succeeds("grep -q Authorized /tmp/oauth.log")
    machine.succeed(f"test -s {STATE}/accounts/alice-gmail.token")


def start_uplink():
    machine.systemctl("start uplink.service")
    machine.wait_for_unit("uplink.service")
    machine.wait_for_open_port(8443)


def provider_token():
    """An access token of the test's own, for acting on the Gmail side."""
    location = sign_in({
        "redirect_uri": "http://127.0.0.1/",
        "scope": SCOPE,
        "state": "provider",
        "client_id": "uplink-test",
    })
    code = parse_qs(urlparse(location).query)["code"][0]
    response = machine.succeed(
        f"curl -sf -X POST {GOOGLE}/oauth2/token -d grant_type=authorization_code "
        f"-d code={code} -d client_id=uplink-test -d client_secret=uplink-test-secret "
        f"-d redirect_uri=http://127.0.0.1/"
    )
    return json.loads(response)["access_token"]


def gmail(token, method, path, body=None):
    data = "" if body is None else f"-H 'Content-Type: application/json' -d {shlex.quote(json.dumps(body))}"
    out = machine.succeed(
        f"curl -sf -X {method} -H 'Authorization: Bearer {token}' {data} "
        f"{shlex.quote(GOOGLE + '/gmail/v1/users/me' + path)}"
    )
    return json.loads(out) if out.strip() else None


def gmail_labels(token, message):
    return gmail(token, "GET", f"/messages/{message}?format=minimal").get("labelIds", [])


def deliver(token, subject, body, sender="Bob Sender <bob@example.org>", date="Tue, 3 Nov 2026 10:00:00 +0000"):
    raw = (
        f"Message-ID: <{subject.replace(' ', '.')}@uplink.test>\r\n"
        f"From: {sender}\r\nTo: alice@example.com\r\n"
        f"Date: {date}\r\nSubject: {subject}\r\n\r\n{body}\r\n"
    )
    encoded = base64.urlsafe_b64encode(raw.encode()).decode().rstrip("=")
    return gmail(token, "POST", "/messages/import", {"raw": encoded, "labelIds": ["INBOX", "UNREAD"]})["id"]
