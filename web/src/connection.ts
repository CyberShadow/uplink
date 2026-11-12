// The WebSocket connection to the server (SPEC §5, §12.3): one per tab,
// reconnecting with backoff, resuming the log from the last entry seen.

import { signal } from '@preact/signals'
import type {
  Badges,
  ClientRequest,
  LogEntry,
  Notice,
  ServerError,
  ServerResponse,
  TxLogId,
  WsClientMsg,
  WsServerMsg,
} from './generated/protocol'
import { PROTOCOL_VERSION } from './generated/protocol'

export const PROTOCOL = 'uplink.v1'

/** The server closes with it a connection whose hello says another
 * protocol version than its own. */
const CLOSE_PROTOCOL_VERSION = 4002
export const HEARTBEAT_MS = 30_000
export const BACKOFF_MS = [250, 500, 1_000, 2_000, 4_000, 8_000, 16_000, 30_000]

export type Status = 'connecting' | 'open'

/** The server refused a request. */
export class RequestError extends Error {
  constructor(readonly error: ServerError) {
    super(error.message)
  }
}

/** The connection dropped while an op was in flight: it may or may not
 * have taken effect, which the log will tell. */
export class ConnectionLost extends Error {
  constructor() {
    super('connection lost')
  }
}

export interface Handlers {
  /** The client's state is void (the first connection, or the server
   * could not resume from the cursor): reload it. */
  reset(): void
  /** The server speaks another version of the protocol: the page is of
   * another build than the server's, and should load anew. The
   * connection is not tried again. */
  outdated(): void
  /** A log entry, replayed or live, in log order. */
  entry(entry: LogEntry): void
  /** A notice of the moment, live only (SPEC §5.5). */
  notice(notice: Notice): void
  /** What the badged buckets hold, on connecting and as it changes
   * (SPEC §12.14). */
  badges(badges: Badges): void
}

export interface Requester {
  request(body: ClientRequest): Promise<ServerResponse>
}

interface Pending {
  body: ClientRequest
  resolve(response: ServerResponse): void
  reject(error: Error): void
}

/** Queries can be sent again on a new connection, and so can ops that
 * refuse to apply twice: creating a draft (its id exists) and editing one
 * (its revision moved on). Other ops can't, since one may have been
 * applied before the connection dropped. */
function isResendable(body: ClientRequest): boolean {
  switch (body.type) {
    case 'list_accounts':
    case 'list_entries':
    case 'count_conversations':
    case 'list_items':
    case 'list_home':
    case 'load_message':
    case 'list_drafts':
    case 'list_outbox':
    case 'list_rooms':
    case 'list_timeline':
    case 'list_room_events':
    case 'list_edits':
    case 'list_thread':
    case 'list_pinned':
    case 'preview_url':
    case 'search_room':
    case 'list_receipts':
    case 'list_members':
    case 'join_matrix_room':
    case 'resolve_room_alias':
    case 'fill_gap':
    case 'set_typing':
    case 'get_matrix_security':
    case 'list_message_entries':
    case 'list_conversation':
    case 'list_todos':
    case 'list_events':
    case 'list_history':
    case 'create_draft':
    case 'edit_draft':
      return true
    case 'undo':
    case 'modify_labels':
    case 'act_on_conversations':
    case 'always_show_images':
    case 'delete_draft':
    case 'attach_to_draft':
    case 'detach_from_draft':
    case 'send_draft':
    case 'cancel_send':
    case 'keep_sent':
    case 'keep_observed':
    case 'send_matrix_message':
    case 'cancel_matrix_send':
    case 'keep_matrix_sent':
    case 'mark_room_read':
    case 'mark_seen':
    case 'recover_matrix':
    case 'verify_matrix_device':
    case 'answer_verification':
    case 'import_room_keys':
    case 'change_membership':
    case 'order_spaces':
    case 'modify_room_tags':
    case 'create_matrix_room':
    case 'start_direct_chat':
    case 'create_todo':
    case 'edit_todo':
    case 'delete_todo':
      return false
  }
}

export class Connection implements Requester {
  readonly status = signal<Status>('connecting')

  private ws!: WebSocket
  /** Welcomed on the current socket: requests can go out. */
  private welcomed = false
  /** The last entry received, to resume from. */
  private cursor: TxLogId | null = null
  private nextRequestId = 1
  /** Requests not yet sent. */
  private queued = new Map<number, Pending>()
  /** Requests sent on the current socket, awaiting their response. */
  private sent = new Map<number, Pending>()
  private attempt = 0
  private heartbeat: ReturnType<typeof setInterval> | undefined
  private awaitingPong = false
  private pingSeq = 0
  private reconnect: ReturnType<typeof setTimeout> | undefined
  private disposed = false

  constructor(
    private readonly url: string,
    private readonly handlers: Handlers,
  ) {
    document.addEventListener('visibilitychange', this.onVisibilityChange)
    this.connect()
  }

  request(body: ClientRequest): Promise<ServerResponse> {
    return new Promise((resolve, reject) => {
      const id = this.nextRequestId++
      const pending = { body, resolve, reject }
      if (this.welcomed) {
        this.send(id, pending)
      } else {
        this.queued.set(id, pending)
      }
    })
  }

  dispose() {
    this.disposed = true
    document.removeEventListener('visibilitychange', this.onVisibilityChange)
    clearTimeout(this.reconnect)
    clearInterval(this.heartbeat)
    this.ws.close()
  }

  private connect() {
    this.ws = new WebSocket(this.url, PROTOCOL)
    this.ws.onopen = () => this.post({ type: 'hello', version: PROTOCOL_VERSION, since: this.cursor })
    this.ws.onmessage = (event) => this.receive(JSON.parse(event.data as string) as WsServerMsg)
    this.ws.onclose = (event) => (event.code === CLOSE_PROTOCOL_VERSION ? this.handlers.outdated() : this.onClose())
  }

  private receive(msg: WsServerMsg) {
    switch (msg.type) {
      case 'welcome': {
        this.welcomed = true
        this.attempt = 0
        this.status.value = 'open'
        this.awaitingPong = false
        this.startHeartbeat()
        if (!msg.resumed) {
          this.cursor = msg.cursor
          this.handlers.reset()
        }
        const queued = [...this.queued]
        this.queued.clear()
        for (const [id, pending] of queued) this.send(id, pending)
        break
      }
      case 'entry':
        this.cursor = msg.entry.id
        this.handlers.entry(msg.entry)
        break
      case 'notice':
        this.handlers.notice(msg.notice)
        break
      case 'badges':
        this.handlers.badges(msg.badges)
        break
      case 'response':
        this.settle(msg.request_id).resolve(msg.body)
        break
      case 'request_failed':
        this.settle(msg.request_id).reject(new RequestError(msg.error))
        break
      case 'pong':
        this.awaitingPong = false
        break
    }
  }

  private settle(requestId: number): Pending {
    const pending = this.sent.get(requestId)
    if (pending === undefined) throw new Error(`response to unknown request ${requestId}`)
    this.sent.delete(requestId)
    return pending
  }

  private send(id: number, pending: Pending) {
    this.sent.set(id, pending)
    this.post({ type: 'request', request_id: id, body: pending.body })
  }

  private post(msg: WsClientMsg) {
    this.ws.send(JSON.stringify(msg))
  }

  private onClose() {
    this.welcomed = false
    clearInterval(this.heartbeat)
    for (const [id, pending] of this.sent) {
      if (isResendable(pending.body)) {
        this.queued.set(id, pending)
      } else {
        pending.reject(new ConnectionLost())
      }
    }
    this.sent.clear()
    if (this.disposed) return
    this.status.value = 'connecting'
    const delay = BACKOFF_MS[Math.min(this.attempt, BACKOFF_MS.length - 1)]
    this.attempt++
    this.reconnect = setTimeout(() => this.connect(), delay)
  }

  private startHeartbeat() {
    clearInterval(this.heartbeat)
    this.heartbeat = setInterval(() => this.beat(), HEARTBEAT_MS)
  }

  /** Pings, or gives up on the socket if the last ping went unanswered. */
  private beat() {
    if (this.awaitingPong) {
      this.ws.close()
      return
    }
    this.awaitingPong = true
    this.post({ type: 'ping', seq: this.pingSeq++ })
  }

  // Timers are throttled in hidden tabs, so check on the connection as
  // soon as the tab is back.
  private onVisibilityChange = () => {
    if (document.visibilityState === 'visible' && this.welcomed) {
      this.beat()
      this.startHeartbeat()
    }
  }
}
