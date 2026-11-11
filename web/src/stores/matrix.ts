// Matrix state: accounts with their rooms, the timelines on show with
// their messages being sent (SPEC §9, §12.6), mirrored like mail's.

import { signal } from '@preact/signals'
import type {
  ClientRequest,
  LogEntry,
  MatrixAccount,
  MatrixContent,
  MatrixSend,
  MatrixSendId,
  MembershipChange,
  Room,
  RoomEvent,
  RoomEventId,
  RoomId,
  TxLogId,
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
  readonly error = signal<string | null>(null)

  private timelines = new Map<RoomId, Timeline>()
  private roomLoader = new Loader(async () => {
    const response = await this.query({ type: 'list_rooms' }, 'rooms')
    if (!response) return
    this.accounts.value = response.accounts
    this.rooms.value = response.rooms
  })

  constructor(
    private readonly server: Requester,
    private readonly ops: Ops,
  ) {}

  reset() {
    this.roomLoader.invalidate()
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
        case 'timeline_changed':
          this.timelines.get(event.room)?.changed(event.events)
          break
        case 'matrix_sends_changed':
          for (const room of event.rooms) this.timelines.get(room)?.sendsChanged()
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
      timeline = new Timeline(
        room,
        (request) => this.query(request, 'timeline'),
        (events) => this.query({ type: 'list_room_events', room, events }, 'room_events'),
        () => this.query({ type: 'list_matrix_sends', room }, 'matrix_sends'),
      )
      this.timelines.set(room, timeline)
    }
    return timeline
  }

  send(room: RoomId, content: MatrixContent) {
    return this.ops.dispatch({ type: 'send_matrix_message', room, content })
  }

  cancelSend(item: MatrixSendId) {
    return this.ops.dispatch({ type: 'cancel_matrix_send', item })
  }

  /** Resolves a message that went out though cancelled by keeping it sent. */
  keepSent(item: MatrixSendId) {
    return this.ops.dispatch({ type: 'keep_matrix_sent', item })
  }

  changeMembership(room: RoomId, change: MembershipChange) {
    return this.ops.dispatch({ type: 'change_membership', room, change })
  }

  /** Quietly: reading a room is not news. */
  markRead(room: RoomId, event: RoomEventId) {
    return this.ops.dispatch({ type: 'mark_room_read', room, event }, true)
  }

  private async query<T extends Response<'rooms' | 'timeline' | 'room_events' | 'matrix_sends'>['type']>(
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

/** A room's timeline from some point to its end, oldest first, and the
 * messages being sent to it, which follow. New events are loaded as they
 * are synced; earlier ones on request. */
export class Timeline {
  /** `null` until loaded. */
  readonly events = signal<readonly RoomEvent[] | null>(null)
  readonly moreBefore = signal(false)
  /** Being sent, sent and not yet synced back, or sent though cancelled;
   * `null` until loaded. */
  readonly sends = signal<readonly MatrixSend[] | null>(null)

  private newer = this.loader()
  /** Of events loaded again as they changed, the log entry their load
   * reflects. */
  private loadedAt = new Map<RoomEventId, TxLogId>()
  private loadingEarlier = false
  private sendLoader = new Loader(async () => {
    const response = await this.loadSends()
    if (response) this.sends.value = response.sends
  })

  constructor(
    readonly room: RoomId,
    private readonly load: (request: ClientRequest) => Promise<Response<'timeline'> | null>,
    private readonly loadEvents: (events: RoomEventId[]) => Promise<Response<'room_events'> | null>,
    private readonly loadSends: () => Promise<Response<'matrix_sends'> | null>,
  ) {
    this.newer.invalidate()
    this.sendLoader.invalidate()
  }

  /** Events were added to the end. */
  added() {
    this.newer.invalidate()
  }

  /** What these events show changed: those loaded are loaded again. */
  async changed(events: readonly RoomEventId[]) {
    const loaded = new Set(this.events.value?.map((e) => e.id))
    const ids = events.filter((id) => loaded.has(id))
    if (ids.length === 0) return
    const response = await this.loadEvents(ids)
    if (!response) return
    const fresh = new Map<RoomEventId, RoomEvent>()
    for (const event of response.events) {
      // Of loads in flight together, the one reflecting more of the log
      // wins, whichever is answered last.
      if ((this.loadedAt.get(event.id) ?? -1) > response.as_of) continue
      this.loadedAt.set(event.id, response.as_of)
      fresh.set(event.id, event)
    }
    const current = this.events.value
    if (current && fresh.size > 0) this.events.value = current.map((e) => fresh.get(e.id) ?? e)
  }

  sendsChanged() {
    this.sendLoader.invalidate()
  }

  /** Everything is void: load the latest events afresh. A load in flight
   * finds the events replaced, and drops what it loaded. */
  reset() {
    this.events.value = null
    this.loadedAt.clear()
    this.newer = this.loader()
    this.newer.invalidate()
    this.sendLoader.invalidate()
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
