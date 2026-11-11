// The home view (SPEC §12.13): the configured buckets' items, and the
// agenda, of now through tomorrow; queried while on show, and again when
// a log entry may change what they list.

import { batch, computed, signal } from '@preact/signals'
import type { CalendarEvent, Conversation, HomeBucket, LogEntry, Room, ServerEvent, Todo } from '../generated/protocol'
import type { Requester } from '../connection'
import { date } from '../format'
import { conversationKey } from './mail'
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

export type Row =
  | { kind: 'todo'; key: string; todo: Todo }
  | { kind: 'event'; key: string; event: CalendarEvent }
  | { kind: 'mail'; key: string; conversation: Conversation; query: string }
  | { kind: 'room'; key: string; room: Room }

/** A bucket's rows: to-dos (as listed, by due date), events, then mail
 * and rooms, the most recently active first. */
export function rowsOf(bucket: HomeBucket): Row[] {
  const active = (row: Row) =>
    row.kind === 'mail' ? row.conversation.latest.received_at : row.kind === 'room' ? (row.room.last_event_ts ?? 0) : 0
  const mixed: Row[] = [
    ...bucket.conversations.map((conversation): Row => ({
      kind: 'mail',
      key: `mail:${conversationKey(conversation)}`,
      conversation,
      query: bucket.query,
    })),
    ...bucket.rooms.map((room): Row => ({ kind: 'room', key: `room:${room.id}`, room })),
  ].sort((a, b) => active(b) - active(a))
  return [
    ...bucket.todos.map((todo): Row => ({ kind: 'todo', key: `todo:${todo.id}`, todo })),
    ...bucket.events.map((event): Row => ({ kind: 'event', key: `event:${event.id}`, event })),
    ...mixed,
  ]
}

export interface Bucket {
  bucket: HomeBucket
  rows: Row[]
  /** Those shown: up to `BUCKET_LIMIT`. */
  visible: Row[]
}

function bucketsOf(home: Home | null): Bucket[] {
  return (home?.buckets ?? []).map((bucket) => {
    const rows = rowsOf(bucket)
    return { bucket, rows, visible: rows.slice(0, BUCKET_LIMIT) }
  })
}

export class HomeStore {
  /** `null` until loaded. */
  readonly home = signal<Home | null>(null)
  readonly buckets = computed(() => bucketsOf(this.home.value))
  /** The rows shown, of every bucket, in order. */
  readonly rows = computed(() => this.buckets.value.flatMap((b) => b.visible))
  /** The key of the row the keys act on. It stays while home is not on
   * show; a row that leaves gives it to the next one still shown. */
  readonly focus = signal<string | null>(null)
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
