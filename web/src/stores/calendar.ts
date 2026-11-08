// The calendar (SPEC §10): the events of the range on show, mirrored from
// a query and refreshed when a log entry says a calendar changed.

import { signal } from '@preact/signals'
import type { CalendarEvent, LogEntry } from '../generated/protocol'
import type { Requester } from '../connection'
import { Loader, query } from './query'

export interface Range {
  /** Ms since the epoch. */
  from: number
  to: number
}

export class CalendarStore {
  /** The range on show; `null` before one is. */
  readonly range = signal<Range | null>(null)
  /** Its events, by start; `null` until they are in. */
  readonly events = signal<readonly CalendarEvent[] | null>(null)
  readonly error = signal<string | null>(null)

  private loader = new Loader(async () => {
    const range = this.range.value
    if (!range) return
    try {
      const response = await query(this.server, { type: 'list_events', ...range }, 'events')
      // The range may have changed meanwhile; then another load follows.
      if (this.range.value === range) this.events.value = response.events
    } catch (error) {
      this.error.value = (error as Error).message
    }
  })

  constructor(private readonly server: Requester) {}

  /** Shows the events of `from`–`to`. */
  show(from: number, to: number) {
    const range = this.range.value
    if (range && range.from === from && range.to === to) return
    this.range.value = { from, to }
    this.events.value = null
    this.loader.invalidate()
  }

  reset() {
    this.loader.invalidate()
  }

  apply(entry: LogEntry) {
    if (entry.events.some((event) => event.type === 'calendar_changed')) this.loader.invalidate()
  }
}
