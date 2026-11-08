// The user's ops, newest first, for undoing any of them (SPEC §6.3,
// §12.4). Loaded once a connection starts, then kept current by applying
// the entries that follow the load: a user's op goes on top, and an undo
// marks the entry it undid.

import { signal } from '@preact/signals'
import type { HistoryEntry, LogEntry, TxLogId } from '../generated/protocol'
import type { Requester } from '../connection'
import { query } from './query'

export const HISTORY_PAGE = 100

export class HistoryStore {
  /** `null` until loaded. */
  readonly entries = signal<readonly HistoryEntry[] | null>(null)
  /** Whether there are earlier ones than those listed. */
  readonly more = signal(false)
  readonly error = signal<string | null>(null)
  /** What the listed entries reflect: later entries are applied. */
  private asOf: TxLogId | null = null
  private generation = 0
  private loadingMore = false

  constructor(private readonly server: Requester) {}

  reset() {
    this.generation++
    this.entries.value = null
    this.asOf = null
    this.loadingMore = false
    this.load(null)
  }

  loadMore() {
    const entries = this.entries.value
    if (!entries || !this.more.value || this.loadingMore) return
    this.loadingMore = true
    this.load(entries[entries.length - 1].entry.id)
  }

  apply(entry: LogEntry) {
    if (this.asOf === null || entry.id <= this.asOf || entry.source.type !== 'user') return
    const listed = this.entries.value!.map((h) =>
      h.entry.id === entry.undoes ? { ...h, undone_by: entry.id } : h,
    )
    this.entries.value = [{ entry, undone_by: null }, ...listed]
  }

  private async load(before: TxLogId | null) {
    const generation = this.generation
    let response
    try {
      response = await query(this.server, { type: 'list_history', before, limit: HISTORY_PAGE }, 'history')
    } catch (error) {
      this.error.value = (error as Error).message
      return
    }
    if (generation !== this.generation) return
    // The first page sets the position; a later one, as of a later entry,
    // is older entries as they are now, which those applied agree with.
    if (before === null) this.asOf = response.as_of
    this.entries.value = [...(this.entries.value ?? []), ...response.entries]
    this.more.value = response.more
    this.loadingMore = false
  }
}
