"""A minimal uplink WebSocket client for the VM tests, standing in for the UI.

  uplink-ws request BODY_JSON
      Sends one request; prints the response body, or fails with the error.
  uplink-ws await EVENT_TYPE TIMEOUT
      Prints the first live log entry carrying an event of EVENT_TYPE,
      failing after TIMEOUT seconds.

Uses the asyncio client: the threaded one (websockets.sync) reads and
writes one TLS connection from two threads, which OpenSSL does not
support. A TLS 1.3 session ticket arriving while the handshake request is
written then loses the request.
"""

import asyncio
import base64
import json
import os
import ssl
import sys

from websockets.asyncio.client import connect


def session():
    context = ssl.create_default_context(cafile=os.environ["UPLINK_CA"])
    credentials = f"{os.environ['UPLINK_USER']}:{os.environ['UPLINK_PASSWORD']}"
    return connect(
        os.environ["UPLINK_URL"].replace("https://", "wss://", 1) + "/ws",
        ssl=context,
        subprotocols=["uplink.v1"],
        additional_headers={
            "Authorization": "Basic " + base64.b64encode(credentials.encode()).decode()
        },
    )


async def hello(ws):
    await ws.send(json.dumps({"type": "hello", "version": os.environ["UPLINK_PROTOCOL"], "since": None}))
    welcome = json.loads(await ws.recv())
    assert welcome["type"] == "welcome", welcome


async def request(body):
    async with session() as ws:
        await hello(ws)
        await ws.send(json.dumps({"type": "request", "request_id": 1, "body": json.loads(body)}))
        while True:
            message = json.loads(await ws.recv())
            if message.get("request_id") != 1:
                continue
            if message["type"] == "response":
                print(json.dumps(message["body"]))
                return
            sys.exit(f"request failed: {message['error']}")


async def await_event(event_type, timeout):
    async with session() as ws:
        await hello(ws)
        print("connected", file=sys.stderr, flush=True)
        try:
            async with asyncio.timeout(float(timeout)):
                while True:
                    message = json.loads(await ws.recv())
                    if message["type"] != "entry":
                        continue
                    if any(e["type"] == event_type for e in message["entry"]["events"]):
                        print(json.dumps(message["entry"]))
                        return
        except TimeoutError:
            sys.exit(f"no {event_type} event within {timeout} s")


command, *args = sys.argv[1:]
asyncio.run({"request": request, "await": await_event}[command](*args))
