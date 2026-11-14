// The home view (SPEC §12.13): the configured buckets' items, and the
// agenda, of now through tomorrow; queried while on show, and again when
// a log entry may change what they list.

import { batch, computed, signal } from '@preact/signals'
import type { Arrival, Badges, CalendarEvent, HomeBucket, LogEntry, ServerEvent } from '../generated/protocol'
import type { Requester } from '../connection'
import { rowOf, type Row } from '../items'
import { Loader, query } from './query'

/** Rows a bucket shows at most; the rest are a search away. */
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

export interface Bucket {
  bucket: HomeBucket
  /** Its items, as listed, the most recently active first. */
  rows: Row[]
}

function bucketsOf(home: Home | null): Bucket[] {
  return (home?.buckets ?? []).map((bucket) => ({
    bucket,
    rows: bucket.items.map((item) => rowOf(item, bucket.query)),
  }))
}

export class HomeStore {
  /** `null` until loaded. */
  readonly home = signal<Home | null>(null)
  readonly buckets = computed(() => bucketsOf(this.home.value))
  /** The rows shown, of every bucket, in order. */
  readonly rows = computed(() => this.buckets.value.flatMap((b) => b.rows))
  /** The key of the row the keys act on. It stays while home is not on
   * show; a row that leaves gives it to the next one still shown. */
  readonly focus = signal<string | null>(null)
  readonly error = signal<string | null>(null)
  /** What the badged buckets hold, as the server last said (SPEC
   * §12.14); `null` until it first has. */
  readonly badges = signal<Badges | null>(null)
  /** Views showing it: while none is, it goes stale unqueried, and is
   * queried afresh once one is. */
  private shown = 0
  /** Those told of what comes into the badged buckets. */
  private listeners = new Set<(arrival: Arrival) => void>()

  private loader = new Loader(async () => {
    const now = new Date()
    // The day after tomorrow's midnight, local.
    const end = new Date(now.getFullYear(), now.getMonth(), now.getDate() + 2).getTime()
    try {
      const [home, events] = await Promise.all([
        query(this.server, { type: 'list_home', limit: BUCKET_LIMIT }, 'home'),
        query(this.server, { type: 'list_events', from: now.getTime(), to: end }, 'events'),
      ])
      const before = this.rows.value
      batch(() => {
        this.home.value = { buckets: home.buckets, agenda: events.events }
        this.keepFocus(before, this.rows.value)
      })
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

  /** Tells `listener` of what comes from others into the badged buckets
   * (SPEC §12.16), until the returned function is called. */
  onArrival(listener: (arrival: Arrival) => void): () => void {
    this.listeners.add(listener)
    return () => this.listeners.delete(listener)
  }

  arrived(arrival: Arrival) {
    this.listeners.forEach((listener) => listener(arrival))
  }

  /** Moves the focus `by` rows. */
  moveFocus(by: number) {
    const rows = this.rows.value
    if (rows.length === 0) return
    const index = rows.findIndex((r) => r.key === this.focus.value)
    this.focus.value = rows[Math.min(Math.max(index + by, 0), rows.length - 1)].key
  }

  private keepFocus(before: Row[], after: Row[]) {
    const keys = new Set(after.map((r) => r.key))
    const focus = this.focus.value
    if (focus === null) {
      this.focus.value = after[0]?.key ?? null
    } else if (!keys.has(focus)) {
      // The first row after the old focus that is still shown.
      const index = before.findIndex((r) => r.key === focus)
      const next = before.slice(index + 1).find((r) => keys.has(r.key))
      this.focus.value = (next ?? after.at(-1))?.key ?? null
    }
  }

  private invalidate() {
    if (this.shown > 0) this.loader.invalidate()
  }
}
