// Matrix state: accounts with their rooms, the timelines on show with
// their messages being sent (SPEC §9, §12.6), mirrored like mail's.

import { batch, signal, type Signal } from '@preact/signals'
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
  SpaceOrderChange,
  Room,
  RoomEvent,
  RoomEventId,
  RoomId,
  ServerResponse,
  TxLogId,
  GapId,
  TimelineFilled,
  TimelineGap,
  UrlPreview,
  VerificationAnswer,
} from '../generated/protocol'
import type { Blobs } from '../blobs'
import type { Requester } from '../connection'
import type { Ops } from './ops'
import { Loader, query, type Response } from './query'

/** Events loaded at once. */
export const TIMELINE_PAGE = 50

/** A notification shows the latest message among these many events. */
const NOTIFIED_AMONG = 10

export class MatrixStore {
  /** `null` until loaded. */
  readonly accounts = signal<readonly MatrixAccount[] | null>(null)
  readonly rooms = signal<readonly Room[] | null>(null)
  readonly error = signal<string | null>(null)

  private timelines = new Map<RoomId, Timeline>()
  private securities = new Map<string, Security>()
  private memberLists = new Map<RoomId, Members>()
  /** By room, then by root. */
  private threads = new Map<RoomId, Map<RoomEventId, Thread>>()
  private pinnedLists = new Map<RoomId, Pinned>()
  /** By event and URL: pages change little, and previews are not logged. */
  private previews = new Map<string, Promise<UrlPreview | null>>()
  /** By account and address: the room ids they resolve to, `null` of
   * those that do not, as the homeserver last said; not logged. */
  private aliases = new Map<string, Signal<string | null | undefined>>()
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
    for (const threads of this.threads.values()) for (const thread of threads.values()) thread.changed()
    for (const pinned of this.pinnedLists.values()) pinned.changed()
  }

  apply(entry: LogEntry) {
    for (const event of entry.events) {
      if (event.type === 'timeline_added' || event.type === 'timeline_filled' || event.type === 'timeline_changed') {
        // Any may be of a thread, or change its root; or be pinned.
        for (const thread of this.threads.get(event.room)?.values() ?? []) thread.changed()
        this.pinnedLists.get(event.room)?.changed()
      }
      // What is pinned is of the room's state.
      if (event.type === 'rooms_changed') for (const room of event.rooms) this.pinnedLists.get(room)?.changed()
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
        case 'image_rule_changed':
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

  /** The room's pinned events, of those stored, loaded on first use. */
  pinned(room: RoomId): Pinned {
    let pinned = this.pinnedLists.get(room)
    if (!pinned) {
      pinned = new Pinned(room, this.server)
      this.pinnedLists.set(room, pinned)
    }
    return pinned
  }

  /** The thread of the room whose root is `root`, loaded on first use. */
  thread(room: RoomId, root: RoomEventId): Thread {
    let threads = this.threads.get(room)
    if (!threads) {
      threads = new Map()
      this.threads.set(room, threads)
    }
    let thread = threads.get(root)
    if (!thread) {
      thread = new Thread(room, root, this.server)
      threads.set(root, thread)
    }
    return thread
  }

  /** The room's timeline, loaded from its latest events on first use. */
  timeline(room: RoomId): Timeline {
    let timeline = this.timelines.get(room)
    if (!timeline) {
      timeline = new Timeline(
        room,
        (request) => this.query(request, 'timeline'),
        (events) => this.query({ type: 'list_room_events', room, events }, 'room_events'),
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

  /** Quietly: what is sent shows in the room at once, which is notice
   * enough. */
  send(room: RoomId, content: MatrixContent) {
    return this.ops.dispatch({ type: 'send_matrix_message', room, content }, true)
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

  /** A preview of the page `url` that the room's message `event` links:
   * `null` if the page says nothing of itself, or could not be previewed,
   * which is shown as no preview. */
  preview(room: RoomId, event: RoomEventId, url: string): Promise<UrlPreview | null> {
    const key = `${event} ${url}`
    let preview = this.previews.get(key)
    if (!preview) {
      preview = query(this.server, { type: 'preview_url', room, event, url }, 'url_preview').then(
        (response) => response.preview,
        () => null,
      )
      this.previews.set(key, preview)
    }
    return preview
  }

  /** The id of the room the address (`#alias:server`) is of, as the
   * account's homeserver resolves it, once asked: `undefined` until it
   * has, `null` if it cannot. */
  roomOfAlias(account: string, alias: string): string | null | undefined {
    const key = `${account} ${alias}`
    let resolved = this.aliases.get(key)
    if (!resolved) {
      const asked = signal<string | null | undefined>(undefined)
      query(this.server, { type: 'resolve_room_alias', account, alias }, 'room_alias_resolved').then(
        (response) => (asked.value = response.room_id),
        () => (asked.value = null),
      )
      this.aliases.set(key, asked)
      resolved = asked
    }
    return resolved.value
  }

  /** The room's latest message from others among its latest events, for
   * a notification; `null` if there is none. */
  async latestMessage(room: RoomId, me: string): Promise<RoomEvent | null> {
    const response = await this.query(
      { type: 'list_timeline', room, page: { type: 'latest' }, limit: NOTIFIED_AMONG },
      'timeline',
    )
    return response?.events.findLast((e) => e.kind === 'm.room.message' && e.sender !== me) ?? null
  }

  /** The room's messages that have every word of `words`, newest first,
   * a page at a time. Throws the server's refusal. */
  search(room: RoomId, words: string, before: RoomEventId | null): Promise<Response<'room_search'>> {
    return query(this.server, { type: 'search_room', room, query: words, before, limit: TIMELINE_PAGE }, 'room_search')
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

  /** Creates a room, starts a direct chat or joins a room: the Matrix id
   * of the room joined, once the homeserver has, which sync then lists.
   * Throws the server's refusal. */
  async join(
    request: Extract<ClientRequest, { type: 'create_matrix_room' | 'start_direct_chat' | 'join_matrix_room' }>,
  ): Promise<string> {
    return (await query(this.server, request, 'matrix_room_joined')).room_id
  }

  changeMembership(room: RoomId, change: MembershipChange) {
    return this.ops.dispatch({ type: 'change_membership', room, change })
  }

  /** Moves top-level spaces (MSC3230). */
  orderSpaces(changes: SpaceOrderChange[]) {
    return this.ops.dispatch({ type: 'order_spaces', changes })
  }

  /** Tags the rooms `tag`, or, if each has it already, untags them. */
  toggleTag(rooms: readonly Room[], tag: string) {
    const each = rooms.every((r) => r.tags.includes(tag))
    const ids = rooms.map((r) => r.id)
    return this.ops.dispatch({ type: 'modify_room_tags', rooms: ids, add: each ? [] : [tag], remove: each ? [tag] : [] })
  }

  /** Quietly: reading a room is not news. */
  markRead(room: RoomId, event: RoomEventId) {
    return this.ops.dispatch({ type: 'mark_room_read', room, event }, true)
  }

  private async query<T extends Response<'rooms' | 'timeline' | 'room_events' | 'edits' | 'receipts'>['type']>(
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
  private receiptLoader = new Loader(async () => {
    const response = await this.loadReceipts()
    if (response) this.receipts.value = response.receipts
  }, false)

  constructor(
    readonly room: RoomId,
    private readonly load: (request: ClientRequest) => Promise<Response<'timeline'> | null>,
    private readonly loadEvents: (events: RoomEventId[]) => Promise<Response<'room_events'> | null>,
    private readonly loadReceipts: () => Promise<Response<'receipts'> | null>,
    private readonly fillGap: (gap: GapId) => Promise<void>,
  ) {
    this.newer.invalidate()
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
    // Those of threads are the threads'.
    this.events.value = merged(events, response?.events.filter((e) => e.thread_root === null) ?? [])
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
    // a reaction), and those of a thread whose root arrived are the
    // thread's.
    const fresh = new Map<RoomEventId, RoomEvent | null>(ids.map((id) => [id, null]))
    for (const event of response.events) if (event.thread_root === null) fresh.set(event.id, event)
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

  /** The sends are loaded with the events at the end, which they
   * follow. */
  sendsChanged() {
    this.newer.invalidate()
  }

  /** Everything is void: load the latest events afresh. A load in flight
   * finds the events replaced, and drops what it loaded. */
  reset() {
    this.events.value = null
    this.loadedAt.clear()
    this.newer = this.loader()
    this.newer.invalidate()
    this.gaps.value = []
    // What notices said is void too.
    this.typing.value = []
    this.receiptLoader.invalidate()
  }

  /** Loads the events after those loaded, or the latest, and the sends,
   * which follow them. */
  private loader(): Loader {
    return new Loader(async () => {
      // Page by page, until one reaches the end, with the sends.
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
        // At once: a send synced back shows as one or the other.
        batch(() => {
          if (!last) this.moreBefore.value = response.more_before
          this.events.value = [...(events ?? []), ...response.events]
          if (response.sends) this.sends.value = response.sends
          this.addGaps(response.gaps)
        })
        if (response.sends) return
      }
    })
  }

  /** Loads earlier pages until the event at `pos` is loaded, if it is
   * listed. */
  async loadUntil(pos: number) {
    for (;;) {
      const first = this.events.value?.[0]
      if (!first || first.pos <= pos || !this.moreBefore.value) return
      await this.loadEarlier()
      // Loaded afresh meanwhile, or another load is under way.
      if (this.events.value?.[0] === first) return
    }
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

/** A room's pinned events, of those stored, in the order pinned, as its
 * client has them: loaded again as the room or its timeline changes. */
export class Pinned {
  /** `null` until loaded. */
  readonly events = signal<readonly RoomEvent[] | null>(null)
  readonly error = signal<string | null>(null)
  private loader = new Loader(async () => {
    try {
      this.events.value = (await query(this.server, { type: 'list_pinned', room: this.room }, 'pinned')).events
      this.error.value = null
    } catch (error) {
      this.error.value = (error as Error).message
    }
  })

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

/** A thread: its root, and its messages, oldest first, as its client
 * has them: loaded again whenever the room's timeline changes. */
export class Thread {
  /** `null` until loaded. */
  readonly state = signal<{ root: RoomEvent; replies: readonly RoomEvent[] } | null>(null)
  readonly error = signal<string | null>(null)
  private loader = new Loader(async () => {
    try {
      const { root, replies } = await query(
        this.server,
        { type: 'list_thread', room: this.room, root: this.root },
        'thread',
      )
      this.state.value = { root, replies }
      this.error.value = null
    } catch (error) {
      this.error.value = (error as Error).message
    }
  })

  constructor(
    readonly room: RoomId,
    readonly root: RoomEventId,
    private readonly server: Requester,
  ) {
    this.loader.invalidate()
  }

  changed() {
    this.loader.invalidate()
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
