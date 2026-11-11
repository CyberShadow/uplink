// Matrix state: accounts with their rooms, the timelines on show with
// their messages being sent (SPEC §9, §12.6), mirrored like mail's.

import { signal } from '@preact/signals'
import type {
  ClientRequest,
  LogEntry,
  MatrixAccount,
  MatrixSecurity,
  MemberReceipt,
  Notice,
  RoomMember,
  MatrixContent,
  MatrixSend,
  MatrixSendId,
  MembershipChange,
  Room,
  RoomEvent,
  RoomEventId,
  RoomId,
  ServerResponse,
  TxLogId,
  GapId,
  TimelineFilled,
  TimelineGap,
  VerificationAnswer,
} from '../generated/protocol'
import type { Blobs } from '../blobs'
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
  private securities = new Map<string, Security>()
  private memberLists = new Map<RoomId, Members>()
  private roomLoader = new Loader(async () => {
    const response = await this.query({ type: 'list_rooms' }, 'rooms')
    if (!response) return
    this.accounts.value = response.accounts
    this.rooms.value = response.rooms
  })

  constructor(
    private readonly server: Requester & Blobs,
    private readonly ops: Ops,
  ) {}

  reset() {
    this.roomLoader.invalidate()
    for (const timeline of this.timelines.values()) timeline.reset()
    for (const security of this.securities.values()) security.changed()
    for (const members of this.memberLists.values()) members.changed()
  }

  apply(entry: LogEntry) {
    for (const event of entry.events) {
      switch (event.type) {
        case 'rooms_changed':
          this.roomLoader.invalidate()
          break
        case 'timeline_added':
          this.timelines.get(event.room)?.added()
          // Of the events, some may change members, or their power.
          this.memberLists.get(event.room)?.changed()
          break
        case 'timeline_filled':
          this.timelines.get(event.room)?.filled(event)
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

  notice(notice: Extract<Notice, { type: 'typing' | 'receipts_changed' | 'security_changed' }>) {
    if (notice.type === 'security_changed') {
      this.securities.get(notice.account)?.changed()
      return
    }
    const timeline = this.timelines.get(notice.room)
    if (!timeline) return
    switch (notice.type) {
      case 'typing':
        timeline.typing.value = notice.users
        break
      case 'receipts_changed':
        timeline.receiptsChanged()
        break
    }
  }

  /** Tells the room the account is typing, or has stopped. */
  async setTyping(room: RoomId, typing: boolean) {
    try {
      await this.server.request({ type: 'set_typing', room, typing })
    } catch (error) {
      this.error.value = (error as Error).message
    }
  }

  /** The account's device and its encryption, loaded on first use. */
  security(account: string): Security {
    let security = this.securities.get(account)
    if (!security) {
      security = new Security(account, this.server)
      this.securities.set(account, security)
    }
    return security
  }

  /** The room's members, loaded on first use. */
  members(room: RoomId): Members {
    let members = this.memberLists.get(room)
    if (!members) {
      members = new Members(room, this.server)
      this.memberLists.set(room, members)
    }
    return members
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
        () => this.query({ type: 'list_receipts', room }, 'receipts'),
        async (gap) => {
          try {
            await this.server.request({ type: 'fill_gap', room, gap })
          } catch (error) {
            this.error.value = (error as Error).message
          }
        },
      )
      this.timelines.set(room, timeline)
    }
    return timeline
  }

  send(room: RoomId, content: MatrixContent) {
    return this.ops.dispatch({ type: 'send_matrix_message', room, content })
  }

  /** Uploads `files` and sends each, in order; stops at one that fails. */
  async sendFiles(room: RoomId, files: readonly File[], reply_to: string | null) {
    for (const file of files) {
      let uploaded
      try {
        uploaded = await this.server.upload(file)
      } catch (error) {
        this.ops.fail(`Could not send ${file.name}: ${(error as Error).message}`)
        return
      }
      const mime = file.type || 'application/octet-stream'
      const content: MatrixContent = { type: 'file', blob: uploaded.blob, name: file.name, mime, reply_to }
      if (!(await this.send(room, content))) return
      // Only the first answers what it replies to.
      reply_to = null
    }
  }

  /** The edits of the room's message `event`, oldest first. */
  async edits(room: RoomId, event: RoomEventId): Promise<RoomEvent[] | null> {
    const response = await this.query({ type: 'list_edits', room, event }, 'edits')
    return response?.edits ?? null
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

  private async query<T extends Response<'rooms' | 'timeline' | 'room_events' | 'edits' | 'matrix_sends' | 'receipts'>['type']>(
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

/** `events` with `added`, in timeline order, each once. */
function merged(events: readonly RoomEvent[], added: readonly RoomEvent[]): RoomEvent[] {
  const known = new Set(events.map((e) => e.id))
  return [...events, ...added.filter((e) => !known.has(e.id))].sort((a, b) => a.pos - b.pos)
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
  /** Who is typing, as the last notice said. */
  readonly typing = signal<readonly RoomMember[]>([])
  /** How far the room's members have read. */
  readonly receipts = signal<readonly MemberReceipt[]>([])
  /** Events missing among those loaded (SPEC §9.2). */
  readonly gaps = signal<readonly TimelineGap[]>([])
  /** Gaps being filled. */
  readonly filling = signal<ReadonlySet<GapId>>(new Set())

  private newer = this.loader()
  /** Of events loaded again as they changed, the log entry their load
   * reflects. */
  private loadedAt = new Map<RoomEventId, TxLogId>()
  private loadingEarlier = false
  private sendLoader = new Loader(async () => {
    const response = await this.loadSends()
    if (response) this.sends.value = response.sends
  })
  private receiptLoader = new Loader(async () => {
    const response = await this.loadReceipts()
    if (response) this.receipts.value = response.receipts
  }, false)

  constructor(
    readonly room: RoomId,
    private readonly load: (request: ClientRequest) => Promise<Response<'timeline'> | null>,
    private readonly loadEvents: (events: RoomEventId[]) => Promise<Response<'room_events'> | null>,
    private readonly loadSends: () => Promise<Response<'matrix_sends'> | null>,
    private readonly loadReceipts: () => Promise<Response<'receipts'> | null>,
    private readonly fillGap: (gap: GapId) => Promise<void>,
  ) {
    this.newer.invalidate()
    this.sendLoader.invalidate()
    this.receiptLoader.invalidate()
  }

  receiptsChanged() {
    this.receiptLoader.invalidate()
  }

  /** Fetches the events missing in the gap, which then fill it. */
  async fill(gap: GapId) {
    if (this.filling.value.has(gap)) return
    this.filling.value = new Set([...this.filling.value, gap])
    try {
      await this.fillGap(gap)
    } finally {
      this.filling.value = new Set([...this.filling.value].filter((g) => g !== gap))
    }
  }

  /** Events filled a gap: shown among those loaded, if it is. */
  async filled(filled: TimelineFilled) {
    if (!this.gaps.value.some((g) => g.id === filled.gap)) return
    const response = filled.events.length > 0 ? await this.loadEvents(filled.events) : null
    const events = this.events.value
    if (!events) return
    this.events.value = merged(events, response?.events ?? [])
    this.gaps.value = [...this.gaps.value.filter((g) => g.id !== filled.gap), ...(filled.left ? [filled.left] : [])]
  }

  private addGaps(gaps: readonly TimelineGap[]) {
    const known = new Set(this.gaps.value.map((g) => g.id))
    const added = gaps.filter((g) => !known.has(g.id))
    if (added.length > 0) this.gaps.value = [...this.gaps.value, ...added]
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
    // Those not answered are no longer listed (one decrypted late may be
    // a reaction).
    const fresh = new Map<RoomEventId, RoomEvent | null>(ids.map((id) => [id, null]))
    for (const event of response.events) fresh.set(event.id, event)
    for (const id of fresh.keys()) {
      // Of loads in flight together, the one reflecting more of the log
      // wins, whichever is answered last.
      if ((this.loadedAt.get(id) ?? -1) > response.as_of) fresh.delete(id)
      else this.loadedAt.set(id, response.as_of)
    }
    const current = this.events.value
    if (current && fresh.size > 0) {
      this.events.value = current.flatMap((e) => {
        const now = fresh.get(e.id)
        return now === undefined ? [e] : now === null ? [] : [now]
      })
    }
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
    this.gaps.value = []
    // What notices said is void too.
    this.typing.value = []
    this.receiptLoader.invalidate()
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
        this.addGaps(response.gaps)
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
        this.addGaps(response.gaps)
      }
    } finally {
      this.loadingEarlier = false
    }
  }
}

/** A room's members, and what the account may do in it, as its client
 * has them: not logged, so loaded again as events arrive. */
export class Members {
  /** `null` until loaded. */
  readonly state = signal<Response<'members'> | null>(null)
  readonly error = signal<string | null>(null)
  private loader = new Loader(async () => {
    try {
      this.state.value = await query(this.server, { type: 'list_members', room: this.room }, 'members')
      this.error.value = null
    } catch (error) {
      this.error.value = (error as Error).message
    }
  }, false)

  constructor(
    readonly room: RoomId,
    private readonly server: Requester,
  ) {
    this.loader.invalidate()
  }

  changed() {
    this.loader.invalidate()
  }
}

/** An account's device and its encryption (SPEC §9.4), as its client
 * has it: not logged, so loaded again when a notice says it changed. */
export class Security {
  /** `null` until loaded. */
  readonly state = signal<MatrixSecurity | null>(null)
  readonly error = signal<string | null>(null)
  private loader = new Loader(async () => {
    const response = await this.request({ type: 'get_matrix_security', account: this.account }, 'matrix_security')
    if (response) this.state.value = response.security
  }, false)

  constructor(
    readonly account: string,
    private readonly server: Requester & Blobs,
  ) {
    this.loader.invalidate()
  }

  changed() {
    this.loader.invalidate()
  }

  /** Asks to verify uplink's device with `device`, or with whichever of
   * the account's answers first. */
  async verify(device: string | null) {
    const response = await this.request({ type: 'verify_matrix_device', account: this.account, device }, 'matrix_security')
    if (response) this.state.value = response.security
  }

  async answer(flow_id: string, answer: VerificationAnswer) {
    const response = await this.request(
      { type: 'answer_verification', account: this.account, flow_id, answer },
      'matrix_security',
    )
    if (response) this.state.value = response.security
  }

  /** Unlocks Secret Storage with the recovery key or passphrase; returns
   * whether it did. */
  async recover(recovery: string): Promise<boolean> {
    const response = await this.request({ type: 'recover_matrix', account: this.account, recovery }, 'matrix_security')
    if (response) this.state.value = response.security
    return response !== null
  }

  /** Imports the room keys exported to `file`; returns how many were new,
   * of how many, or `null` if it failed. */
  async importKeys(file: File, passphrase: string): Promise<Response<'room_keys_imported'> | null> {
    this.error.value = null
    let blob
    try {
      blob = (await this.server.upload(file)).blob
    } catch (error) {
      this.error.value = (error as Error).message
      return null
    }
    return this.request({ type: 'import_room_keys', account: this.account, blob, passphrase }, 'room_keys_imported')
  }

  /** The account's room keys, exported encrypted with `passphrase`. */
  async exportKeys(passphrase: string): Promise<Blob | null> {
    this.error.value = null
    try {
      return await this.server.exportRoomKeys(this.account, passphrase)
    } catch (error) {
      this.error.value = (error as Error).message
      return null
    }
  }

  private async request<T extends ServerResponse['type']>(request: ClientRequest, type: T): Promise<Response<T> | null> {
    this.error.value = null
    try {
      return await query(this.server, request, type)
    } catch (error) {
      this.error.value = (error as Error).message
      return null
    }
  }
}
