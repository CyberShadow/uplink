// Ops dispatched from this tab, and the notice of the last one, with its
// Undo (SPEC §12.6).

import { signal } from '@preact/signals'
import type { ClientRequest, LogEntry, ServerResponse, TxLogId } from '../generated/protocol'
import type { Requester } from '../connection'

/** The outcome of the last op dispatched from this tab. */
export type Notice =
  | { kind: 'done'; id: TxLogId; description: string }
  | { kind: 'failed'; message: string }

/** How many recent entries to remember, to describe dispatched ops. */
const RECENT = 64

export class Ops {
  readonly notice = signal<Notice | null>(null)
  private recent = new Map<TxLogId, LogEntry>()

  constructor(private readonly server: Requester) {}

  /** Every log entry passes here first. */
  record(entry: LogEntry) {
    this.recent.set(entry.id, entry)
    if (this.recent.size > RECENT) this.recent.delete(this.recent.keys().next().value!)
  }

  /** Notes a failure to do what the user asked, before any op. */
  fail(message: string) {
    this.notice.value = { kind: 'failed', message }
  }

  undo(id: TxLogId) {
    return this.dispatch({ type: 'undo', tx_log_id: id })
  }

  /** Sends an op and notes its outcome; `quiet` leaves the notice alone.
   * Resolves to its log entry, or `null` if it failed. */
  async dispatch(request: ClientRequest, quiet = false): Promise<LogEntry | null> {
    let response: ServerResponse
    try {
      response = await this.server.request(request)
    } catch (error) {
      this.fail((error as Error).message)
      return null
    }
    if (response.type !== 'dispatched' && response.type !== 'undone') {
      throw new Error(`unexpected response to ${request.type}: ${response.type}`)
    }
    // The entry came before the response.
    const entry = this.recent.get(response.tx_log_id)
    if (!entry) throw new Error(`entry ${response.tx_log_id} not received`)
    if (!quiet) this.notice.value = { kind: 'done', id: entry.id, description: entry.description }
    return entry
  }
}
