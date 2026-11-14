// The home view (SPEC §12.13): the configured buckets' items, and the
// agenda, of now through tomorrow. The buckets are windows the server
// keeps current from the first connection, so home shows at once, as it
// is; the agenda is queried then, as home shows, and as the calendar
// changes.

import { batch, computed, signal } from '@preact/signals'
import type { Arrival, Badges, CalendarEvent, HomeBucket, LogEntry } from '../generated/protocol'
import type { Requester, Subscriber } from '../connection'
import { rowOf, type Row } from '../items'
import { Loader, query } from './query'
import { LiveWindow } from './window'

/** Rows a bucket shows at most; the rest are a search away. */
export const BUCKET_LIMIT = 20

export interface Bucket {
  bucket: HomeBucket
  /** Its items, as listed, the most recently active first. */
  rows: Row[]
  /** Whether it lists more than these. */
  more: boolean
}

export class HomeStore {
  /** The buckets, once each is listed; `null` until then. */
  readonly buckets = signal<Bucket[] | null>(null)
  /** Of now through tomorrow, by start; `null` until loaded. */
  readonly agenda = signal<CalendarEvent[] | null>(null)
  /** The rows shown, of every bucket, in order. */
  readonly rows = computed(() => (this.buckets.value ?? []).flatMap((b) => b.rows))
  /** The key of the row the keys act on. It stays while home is not on
   * show; a row that leaves gives it to the next one still shown. */
  readonly focus = signal<string | null>(null)
  readonly error = signal<string | null>(null)
  /** What the badged buckets hold, as the server last said (SPEC
   * §12.14); `null` until it first has. */
  readonly badges = signal<Badges | null>(null)
  /** The buckets' windows, in order, as last configured. */
  private windows: { bucket: HomeBucket; window: LiveWindow }[] = []
  /** Those told of what comes into the badged buckets. */
  private listeners = new Set<(arrival: Arrival) => void>()

  /** The buckets as configured, subscribed to anew if they changed. */
  private configuration = new Loader(async () => {
    let buckets: HomeBucket[]
    try {
      buckets = (await query(this.server, { type: 'list_home' }, 'home')).buckets
    } catch (error) {
      this.error.value = (error as Error).message
      return
    }
    // As they were, their windows are current: the connection keeps them.
    if (JSON.stringify(buckets) === JSON.stringify(this.windows.map((w) => w.bucket))) return
    this.windows.forEach(({ window }) => window.close())
    this.windows = buckets.map((bucket) => {
      const window = new LiveWindow(this.server, bucket.query, 'false', BUCKET_LIMIT, (error) => {
        this.error.value = error.message
      })
      window.listener = () => this.showBuckets()
      return { bucket, window }
    })
    this.showBuckets()
    // Of the configuration, not the log: a change to it may come while
    // it is read.
  }, false)

  private agendaLoader = new Loader(async () => {
    const now = new Date()
    // The day after tomorrow's midnight, local.
    const end = new Date(now.getFullYear(), now.getMonth(), now.getDate() + 2).getTime()
    try {
      this.agenda.value = (await query(this.server, { type: 'list_events', from: now.getTime(), to: end }, 'events')).events
    } catch (error) {
      this.error.value = (error as Error).message
    }
  })

  constructor(private readonly server: Requester & Subscriber) {}

  /** As home shows: the agenda, of now. */
  show() {
    this.agendaLoader.invalidate()
  }

  /** Everything is void, as at first: the buckets may be configured
   * otherwise. */
  reset() {
    this.configuration.invalidate()
    this.agendaLoader.invalidate()
  }

  apply(entry: LogEntry) {
    if (entry.events.some((event) => event.type === 'calendar_changed')) this.agendaLoader.invalidate()
  }

  /** The buckets were configured anew. */
  changed() {
    this.configuration.invalidate()
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

  /** Shows the buckets once each is listed. */
  private showBuckets() {
    if (this.windows.some(({ window }) => window.shown === null)) return
    const before = this.rows.value
    batch(() => {
      this.buckets.value = this.windows.map(({ bucket, window }) => ({
        bucket,
        rows: window.listed().map(({ item }) => rowOf(item, bucket.query)),
        more: window.shown!.more,
      }))
      this.keepFocus(before, this.rows.value)
    })
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
}
