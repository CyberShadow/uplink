// Matrix state: accounts with their rooms, the timelines on show, and the
// messages being sent (SPEC §9, §12.6), mirrored like mail's.

import { signal } from '@preact/signals'
import type {
  ClientRequest,
  LogEntry,
  MatrixAccount,
  MatrixOutboxId,
  MatrixOutboxItem,
  MembershipChange,
  Room,
  RoomEvent,
  RoomEventId,
  RoomId,
} from '../generated/protocol'
import type { Requester } from '../connection'
import type { Ops } from './ops'
import { Loader, query, type Response } from './query'

/** Events loaded at once. */
export const TIMELINE_PAGE = 50

export class MatrixStore {
  /** `null` until loaded. */
  readonly accounts = signal<readonly MatrixAccount[] | null>(null)
  readonly rooms = signal<readonly Room[] | null>(null)
  readonly outbox = signal<readonly MatrixOutboxItem[] | null>(null)
  readonly error = signal<string | null>(null)

  private timelines = new Map<RoomId, Timeline>()
  private roomLoader = new Loader(async () => {
    const response = await this.query({ type: 'list_rooms' }, 'rooms')
    if (!response) return
    this.accounts.value = response.accounts
    this.rooms.value = response.rooms
  })
  private outboxLoader = new Loader(async () => {
    const response = await this.query({ type: 'list_matrix_outbox' }, 'matrix_outbox')
    if (response) this.outbox.value = response.items
  })

  constructor(
    private readonly server: Requester,
    private readonly ops: Ops,
  ) {}

  reset() {
    this.roomLoader.invalidate()
    this.outboxLoader.invalidate()
    for (const timeline of this.timelines.values()) timeline.reset()
  }

  apply(entry: LogEntry) {
    for (const event of entry.events) {
      switch (event.type) {
        case 'rooms_changed':
          this.roomLoader.invalidate()
          break
        case 'timeline_added':
          this.timelines.get(event.room)?.added()
          break
        case 'matrix_outbox_changed':
          this.outboxLoader.invalidate()
          break
        case 'entries_added':
        case 'entries_removed':
        case 'labels_changed':
        case 'label_list_changed':
        case 'draft_edited':
        case 'drafts_changed':
        case 'outbox_changed':
        case 'todos_changed':
        case 'calendar_changed':
          break
      }
    }
  }

  /** The room's timeline, loaded from its latest events on first use. */
  timeline(room: RoomId): Timeline {
    let timeline = this.timelines.get(room)
    if (!timeline) {
      timeline = new Timeline(room, (request) => this.query(request, 'timeline'))
      this.timelines.set(room, timeline)
    }
    return timeline
  }

  send(room: RoomId, body: string) {
    return this.ops.dispatch({ type: 'send_matrix_message', room, body })
  }

  cancelSend(item: MatrixOutboxId) {
    return this.ops.dispatch({ type: 'cancel_matrix_send', item })
  }

  changeMembership(room: RoomId, change: MembershipChange) {
    return this.ops.dispatch({ type: 'change_membership', room, change })
  }

  /** Quietly: reading a room is not news. */
  markRead(room: RoomId, event: RoomEventId) {
    return this.ops.dispatch({ type: 'mark_room_read', room, event }, true)
  }

  private async query<T extends Response<'rooms' | 'timeline' | 'matrix_outbox'>['type']>(
    request: ClientRequest,
    type: T,
  ): Promise<Response<T> | null> {
    try {
      return await query(this.server, request, type)
    } catch (error) {
      this.error.value = (error as Error).message
      return null
    }
  }
}

/** A room's timeline from some point to its end, oldest first. New events
 * are loaded as they are synced; earlier ones on request. */
export class Timeline {
  /** `null` until loaded. */
  readonly events = signal<readonly RoomEvent[] | null>(null)
  readonly moreBefore = signal(false)

  private newer = this.loader()
  private loadingEarlier = false

  constructor(
    readonly room: RoomId,
    private readonly load: (request: ClientRequest) => Promise<Response<'timeline'> | null>,
  ) {
    this.newer.invalidate()
  }

  /** Events were added to the end. */
  added() {
    this.newer.invalidate()
  }

  /** Everything is void: load the latest events afresh. A load in flight
   * finds the events replaced, and drops what it loaded. */
  reset() {
    this.events.value = null
    this.newer = this.loader()
    this.newer.invalidate()
  }

  /** Loads the events after those loaded, or the latest. */
  private loader(): Loader {
    return new Loader(async () => {
      // Page by page, until a page is not full.
      for (;;) {
        const events = this.events.value
        const last = events?.at(-1)
        const response = await this.load({
          type: 'list_timeline',
          room: this.room,
          page: last ? { type: 'after', event: last.id } : { type: 'latest' },
          limit: TIMELINE_PAGE,
        })
        if (!response || this.events.value !== events) return
        if (!last) this.moreBefore.value = response.more_before
        this.events.value = [...(events ?? []), ...response.events]
        if (!last || response.events.length < TIMELINE_PAGE) return
      }
    })
  }

  async loadEarlier() {
    const first = this.events.value?.[0]
    if (!first || this.loadingEarlier) return
    this.loadingEarlier = true
    try {
      const response = await this.load({
        type: 'list_timeline',
        room: this.room,
        page: { type: 'before', event: first.id },
        limit: TIMELINE_PAGE,
      })
      const events = this.events.value
      // Unless loaded afresh meanwhile.
      if (response && events?.[0] === first) {
        this.events.value = [...response.events, ...events]
        this.moreBefore.value = response.more_before
      }
    } finally {
      this.loadingEarlier = false
    }
  }
}
