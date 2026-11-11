// The home view (SPEC §12.13): the configured buckets' items, and the
// agenda, of now through tomorrow; queried while on show, and again when
// a log entry may change what they list.

import { signal } from '@preact/signals'
import type { CalendarEvent, HomeBucket, LogEntry, ServerEvent } from '../generated/protocol'
import type { Requester } from '../connection'
import { date } from '../format'
import { Loader, query } from './query'

/** Rows a bucket shows at most, and so conversations it asks for; the
 * rest are a search away. */
export const BUCKET_LIMIT = 20

/** What may move an item into a bucket or out of it, or change the agenda. */
const CHANGING: ReadonlySet<ServerEvent['type']> = new Set([
  'entries_added',
  'entries_removed',
  'labels_changed',
  'label_list_changed',
  'rooms_changed',
  'timeline_added',
  'todos_changed',
  'calendar_changed',
])

export interface Home {
  buckets: HomeBucket[]
  /** Of now through tomorrow, by start. */
  agenda: CalendarEvent[]
}

export class HomeStore {
  /** `null` until loaded. */
  readonly home = signal<Home | null>(null)
  readonly error = signal<string | null>(null)
  /** Views showing it: while none is, it goes stale unqueried, and is
   * queried afresh once one is. */
  private shown = 0

  private loader = new Loader(async () => {
    const now = new Date()
    // The day after tomorrow's midnight, local.
    const end = new Date(now.getFullYear(), now.getMonth(), now.getDate() + 2).getTime()
    try {
      const [home, events] = await Promise.all([
        query(this.server, { type: 'list_home', today: date(now), limit: BUCKET_LIMIT }, 'home'),
        query(this.server, { type: 'list_events', from: now.getTime(), to: end }, 'events'),
      ])
      this.home.value = { buckets: home.buckets, agenda: events.events }
    } catch (error) {
      this.error.value = (error as Error).message
    }
  })

  constructor(private readonly server: Requester) {}

  /** Keeps it current until the returned function is called. */
  show(): () => void {
    this.shown++
    this.loader.invalidate()
    return () => {
      this.shown--
    }
  }

  reset() {
    this.invalidate()
  }

  apply(entry: LogEntry) {
    if (entry.events.some((event) => CHANGING.has(event.type))) this.invalidate()
  }

  /** The buckets were configured anew. */
  changed() {
    this.invalidate()
  }

  private invalidate() {
    if (this.shown > 0) this.loader.invalidate()
  }
}
