// The calendar (SPEC §10): the events of the range on show, mirrored from
// a query and refreshed when a log entry says a calendar changed.

import { signal } from '@preact/signals'
import type { CalendarEvent, EventDetails, EventId, LogEntry, TimeZone } from '../generated/protocol'
import { RequestError, type Requester } from '../connection'
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
  /** The time zones configured to tick the hours in too. */
  readonly timeZones = signal<readonly TimeZone[]>([])

  /** The event opened; `null` while none is. */
  readonly openedId = signal<EventId | null>(null)
  /** Its details: `null` until they are in, `'gone'` if it is no longer
   * on the calendar. */
  readonly opened = signal<EventDetails | 'gone' | null>(null)

  // A loader for each query: one in flight for another range or event
  // has its answer dropped, and does not hold back the new query, as a
  // log entry would (as of the old query, it reflects the entry).
  private loader = this.eventsLoader(null)
  private openedLoader = this.detailsLoader(null)

  constructor(private readonly server: Requester) {}

  private eventsLoader(range: Range | null): Loader {
    return new Loader(async () => {
      if (!range) return
      try {
        const response = await query(this.server, { type: 'list_events', ...range }, 'events')
        if (this.range.value !== range) return
        this.events.value = response.events
        this.timeZones.value = response.time_zones
      } catch (error) {
        this.error.value = (error as Error).message
      }
    })
  }

  private detailsLoader(id: EventId | null): Loader {
    return new Loader(async () => {
      if (id === null) return
      let details: EventDetails | 'gone'
      try {
        details = (await query(this.server, { type: 'get_event', event: id }, 'event')).details
      } catch (error) {
        if (!(error instanceof RequestError && error.error.code === 'not_found')) {
          this.error.value = (error as Error).message
          return
        }
        details = 'gone'
      }
      if (this.openedId.value === id) this.opened.value = details
    })
  }

  /** Opens the event of `id`, or none. */
  open(id: EventId | null) {
    if (this.openedId.value === id) return
    this.openedId.value = id
    this.opened.value = null
    this.openedLoader = this.detailsLoader(id)
    this.openedLoader.invalidate()
  }

  /** Shows the events of `from`–`to`. */
  show(from: number, to: number) {
    const range = this.range.value
    if (range && range.from === from && range.to === to) return
    this.range.value = { from, to }
    this.events.value = null
    this.loader = this.eventsLoader(this.range.value)
    this.loader.invalidate()
  }

  reset() {
    this.loader.invalidate()
    this.openedLoader.invalidate()
  }

  /** The calendar was configured anew: not logged, so a query in flight
   * may not reflect it. */
  configured() {
    this.loader = this.eventsLoader(this.range.value)
    this.loader.invalidate()
  }

  apply(entry: LogEntry) {
    if (entry.events.some((event) => event.type === 'calendar_changed')) {
      this.loader.invalidate()
      this.openedLoader.invalidate()
    }
  }
}
