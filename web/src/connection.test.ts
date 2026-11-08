import { Server, type Client } from 'mock-socket'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { BACKOFF_MS, Connection, ConnectionLost, HEARTBEAT_MS } from './connection'
import type { LogEntry, WsClientMsg, WsServerMsg } from './generated/protocol'

const URL = 'ws://uplink.test/ws'

/** The server side of one connection. */
class Peer {
  readonly received: WsClientMsg[] = []
  private waiters: (() => void)[] = []

  constructor(readonly socket: Client) {
    socket.on('message', (data) => {
      this.received.push(JSON.parse(data as string))
      this.waiters.splice(0).forEach((wake) => wake())
    })
  }

  send(msg: WsServerMsg) {
    this.socket.send(JSON.stringify(msg))
  }

  /** The next message from the client that `matches`. */
  async next<T extends WsClientMsg['type']>(type: T): Promise<Extract<WsClientMsg, { type: T }>> {
    for (let seen = 0; ; ) {
      const found = this.received.slice(seen).find((m) => m.type === type)
      if (found) {
        this.received.splice(this.received.indexOf(found), 1)
        return found as Extract<WsClientMsg, { type: T }>
      }
      seen = this.received.length
      await new Promise<void>((wake) => this.waiters.push(wake))
    }
  }
}

function entry(id: number): LogEntry {
  return {
    id,
    ts: 0,
    source: { type: 'user' },
    description: `entry ${id}`,
    op_type: 'test',
    undoes: null,
    events: [],
  }
}

let server: Server
let peers: Peer[]
let nextPeer: (peer: Peer) => void
let connected: Promise<Peer>
let connection: Connection | undefined

function expectConnection() {
  connected = new Promise((resolve) => (nextPeer = resolve))
}

beforeEach(() => {
  peers = []
  expectConnection()
  server = new Server(URL, { selectProtocol: (protocols) => protocols[0] })
  server.on('connection', (socket) => {
    const peer = new Peer(socket)
    peers.push(peer)
    nextPeer(peer)
  })
})

afterEach(() => {
  connection?.dispose()
  connection = undefined
  server.stop()
  vi.useRealTimers()
})

function connect() {
  const handlers = { reset: vi.fn(), entry: vi.fn() }
  connection = new Connection(URL, handlers)
  return { connection, handlers }
}

/** Accepts a connection: answers its hello, as a fresh start or a resume. */
async function accept(welcome: { cursor: number; resumed: boolean }) {
  const peer = await connected
  expectConnection()
  const hello = await peer.next('hello')
  peer.send({ type: 'welcome', ...welcome })
  return { peer, hello }
}

describe('Connection', () => {
  it('says hello, resets on a fresh start, and answers requests', async () => {
    const { connection, handlers } = connect()
    const { peer, hello } = await accept({ cursor: 5, resumed: false })
    expect(hello.since).toBeNull()

    const response = connection.request({ type: 'list_accounts' })
    const request = await peer.next('request')
    expect(request.body).toEqual({ type: 'list_accounts' })
    peer.send({
      type: 'response',
      request_id: request.request_id,
      body: { type: 'accounts', as_of: 5, accounts: [] },
    })
    await expect(response).resolves.toEqual({ type: 'accounts', as_of: 5, accounts: [] })
    expect(handlers.reset).toHaveBeenCalledOnce()
    expect(connection.status.value).toBe('open')
  })

  it('queues requests until welcomed', async () => {
    const { connection } = connect()
    const response = connection.request({ type: 'list_accounts' })
    const { peer } = await accept({ cursor: 0, resumed: false })
    const request = await peer.next('request')
    peer.send({
      type: 'request_failed',
      request_id: request.request_id,
      error: { code: 'internal', message: 'no' },
    })
    await expect(response).rejects.toThrow('no')
  })

  it('resumes from the last entry it received after a drop', async () => {
    const { connection, handlers } = connect()
    const first = await accept({ cursor: 5, resumed: false })
    first.peer.send({ type: 'entry', entry: entry(6) })
    first.peer.send({ type: 'entry', entry: entry(7) })
    await vi.waitFor(() => expect(handlers.entry).toHaveBeenCalledTimes(2))

    first.peer.socket.close()
    await vi.waitFor(() => expect(connection.status.value).toBe('connecting'))
    const second = await accept({ cursor: 7, resumed: true })
    expect(second.hello.since).toBe(7)
    await vi.waitFor(() => expect(connection.status.value).toBe('open'))
    expect(handlers.reset).toHaveBeenCalledOnce()
    expect(handlers.entry.mock.calls.map(([e]) => e.id)).toEqual([6, 7])
  })

  it('sends queries again on a new connection, but fails ops in flight', async () => {
    const { connection } = connect()
    const first = await accept({ cursor: 1, resumed: false })
    const query = connection.request({ type: 'list_accounts' })
    const op = connection.request({ type: 'undo', tx_log_id: 1 })
    await first.peer.next('request')
    await first.peer.next('request')

    first.peer.socket.close()
    await expect(op).rejects.toBeInstanceOf(ConnectionLost)
    const second = await accept({ cursor: 1, resumed: true })
    const resent = await second.peer.next('request')
    expect(resent.body).toEqual({ type: 'list_accounts' })
    second.peer.send({
      type: 'response',
      request_id: resent.request_id,
      body: { type: 'accounts', as_of: 1, accounts: [] },
    })
    await expect(query).resolves.toMatchObject({ type: 'accounts' })
  })

  it('pings, and reconnects when a ping goes unanswered', async () => {
    vi.useFakeTimers()
    connect()
    const accepting = accept({ cursor: 0, resumed: false })
    await vi.advanceTimersByTimeAsync(10)
    const { peer } = await accepting
    // mock-socket delivers frames on timers too.
    await vi.advanceTimersByTimeAsync(10)

    await vi.advanceTimersByTimeAsync(HEARTBEAT_MS)
    const ping = await peer.next('ping')
    peer.send({ type: 'pong', seq: ping.seq })
    await vi.advanceTimersByTimeAsync(HEARTBEAT_MS)
    await peer.next('ping')
    expect(peers).toHaveLength(1)

    // No pong this time: the next beat gives up on the socket.
    await vi.advanceTimersByTimeAsync(HEARTBEAT_MS + BACKOFF_MS[0] + 10)
    expect(peers).toHaveLength(2)
  })
})
