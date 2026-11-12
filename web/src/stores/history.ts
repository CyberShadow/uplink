// The user's ops, newest first, for undoing any of them (SPEC §6.3,
// §12.4). Loaded once a connection starts, then kept current by applying
// the entries that follow the load: a user's op goes on top, and an undo
// marks the entry it undid. `z` rewinds it, one op a press, passing over
// what views did by being shown.

import { signal } from '@preact/signals'
import type { HistoryEntry, LogEntry, TxLogId } from '../generated/protocol'
import type { Requester } from '../connection'
import type { Ops } from './ops'
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
  /** Where the presses of `undoLast` are: the newest entry once the last
   * finished, and the entry it undid (or failed to). The next press goes
   * on from there, if nothing was done since. */
  private rewind: { head: TxLogId; at: TxLogId } | null = null
  /** The press being handled; the next waits for it, to go on from it. */
  private undoing = Promise.resolve()

  constructor(
    private readonly server: Requester,
    private readonly ops: Ops,
  ) {}

  reset() {
    this.generation++
    this.entries.value = null
    this.asOf = null
    this.loadingMore = false
    this.rewind = null
    this.load(null)
  }

  loadMore() {
    const entries = this.entries.value
    if (!entries || !this.more.value || this.loadingMore) return
    this.loadingMore = true
    this.load(entries[entries.length - 1].entry.id)
  }

  /** Undoes the last op; pressed again, the one before it, and so on,
   * rather than undoing the undo the last press made. Resolves once
   * this press is handled. */
  undoLast(): Promise<void> {
    return (this.undoing = this.undoing.then(() => this.undoNext()))
  }

  private async undoNext() {
    // Not what views did by being shown (SPEC §12.15), which comes of
    // undoing too (a conversation back in the Inbox is seen): as if it
    // were not listed.
    const entries = (this.entries.value ?? []).filter((h) => !h.entry.shown)
    const rewind = this.rewind
    // Going on, it passes over what is undone, and the undos (each of
    // something older, which it passes over too): what was rewound, or
    // was undone and stays so. Afresh, the last op is never undone, but
    // may be an undo, which it redoes.
    const target =
      rewind !== null && entries[0]?.entry.id === rewind.head
        ? entries.find((h) => h.entry.id < rewind.at && h.undone_by === null && h.entry.undoes === null)
        : entries[0]
    if (!target) {
      this.ops.fail(this.more.value ? 'Older history is still loading' : 'Nothing more to undo')
      this.loadMore()
      return
    }
    const undo = await this.ops.undo(target.entry.id)
    this.rewind = { head: undo?.id ?? entries[0].entry.id, at: target.entry.id }
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
