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

/** How far ahead events are reminded of: beyond Outlook's longest
 * reminder but for weeks' (an all-day event's is 18 hours before). */
export const UPCOMING = 2 * 86_400_000

/** Whether the event is the user's own, to edit or delete, as the server
 * has it: organized by the user, or no meeting, and not cancelled. */
export function isOwnEvent(event: CalendarEvent): boolean {
  return (event.response === 'organizer' || event.response === 'none') && !event.cancelled
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

  /** The details of the event of `id` as they are, or `'gone'`; `null`
   * if they could not be had, as {@link error} says. */
  async details(id: EventId): Promise<EventDetails | 'gone' | null> {
    try {
      return (await query(this.server, { type: 'get_event', event: id }, 'event')).details
    } catch (error) {
      if (error instanceof RequestError && error.error.code === 'not_found') return 'gone'
      this.error.value = (error as Error).message
      return null
    }
  }

  private detailsLoader(id: EventId | null): Loader {
    return new Loader(async () => {
      if (id === null) return
      const details = await this.details(id)
      if (details !== null && this.openedId.value === id) this.opened.value = details
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

  /** The events of the coming {@link UPCOMING} (and those on), to be
   * reminded of; `null` while not watched, or until they are in. */
  readonly upcoming = signal<readonly CalendarEvent[] | null>(null)
  private upcomingLoader: Loader | null = null

  /** Keeps {@link upcoming} current, as time passes too, until the
   * returned function is called. */
  watchUpcoming(): () => void {
    const load = () => {
      const from = Date.now()
      this.upcomingLoader = new Loader(async () => {
        try {
          const response = await query(this.server, { type: 'list_events', from, to: from + UPCOMING }, 'events')
          this.upcoming.value = response.events
        } catch (error) {
          this.error.value = (error as Error).message
        }
      })
      this.upcomingLoader.invalidate()
    }
    load()
    // Well within the stretch loaded, so that nothing comes into it
    // unloaded.
    const timer = setInterval(load, UPCOMING / 4)
    return () => {
      clearInterval(timer)
      this.upcomingLoader = null
      this.upcoming.value = null
    }
  }

  reset() {
    this.loader.invalidate()
    this.openedLoader.invalidate()
    this.upcomingLoader?.invalidate()
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
      this.upcomingLoader?.invalidate()
    }
  }
}
