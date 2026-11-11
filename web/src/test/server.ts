// An in-memory stand-in for uplink's mail, draft and outbox requests, for
// the store and view tests. Like the real server, it delivers an op's log
// entry before the op's response (SPEC §5.4).

import type { Blobs } from '../blobs'
import type { Requester } from '../connection'
import { RequestError } from '../connection'
import type {
  Attachment,
  Account,
  CalendarEvent,
  ClientRequest,
  Conversation,
  Draft,
  OutboxItem,
  AccountOutbox,
  Failing,
  PendingChanges,
  Splice,
  Entry,
  EntryId,
  Label,
  LabelId,
  LabelState,
  ListingProgress,
  MailProvider,
  LogEntry,
  MatrixAccount,
  MatrixSecurity,
  MatrixSend,
  MatrixVerification,
  MemberInfo,
  RoomPowers,
  GapId,
  ReadMarker,
  RoomEventId,
  VerificationPhase,
  Membership,
  Room,
  RoomEvent,
  MemberReceipt,
  RoomMember,
  RoomId,
  Sender,
  ServerEvent,
  TimelinePage,
  Todo,
  TodoEdit,
  ServerResponse,
  UploadedBlob,
  MessageId,
} from '../generated/protocol'
import type { Client } from '../stores/client'
import { apply } from '../compose/splice'
import { sendSummary, text } from '../matrix'

/** The Matrix account's user. */
export const ME = '@me:example.org'
/** The Matrix account's recovery key. */
export const RECOVERY_KEY = 'EsTc 1234 5678'

const SYSTEM = ['INBOX', 'UNREAD', 'STARRED', 'IMPORTANT', 'SENT', 'DRAFT', 'TRASH', 'SPAM']
/** Microsoft 365's well-known folders, by the ids uplink gives them. */
const M365_FOLDERS = ['INBOX', 'SENT', 'DRAFT', 'TRASH', 'SPAM', 'ARCHIVE']
const HIDING = ['TRASH', 'SPAM']

interface Undoable {
  /** Applies the inverse, recording it as an entry; returns the entry. */
  revert: () => number
  undone: boolean
}

const NO_POWERS: RoomPowers = { invite: false, name: false, topic: false }

export class FakeServer implements Requester, Blobs {
  readonly requests: ClientRequest[] = []
  private client!: Client
  private accounts: { id: string; address: string; provider: MailProvider; labels: Label[] }[] = []
  private entries: Entry[] = []
  private log: LogEntry[] = []
  private undoable = new Map<number, Undoable>()
  /** The entry being undone, while its inverse commits. */
  private undoing: number | null = null
  private nextLabel = 1
  private nextEntry = 1

  readonly drafts = new Map<string, Draft>()
  readonly outbox: OutboxItem[] = []
  private nextOutbox = 1
  /** Per account, what its sync task has not pushed yet, as set by
   * `pend`. */
  readonly pending = new Map<string, { failing: Failing | null; changes: PendingChanges[] }>()
  /** Uploaded blobs: their content's names, as the fake's ids. */
  readonly blobs = new Map<string, number>()
  private nextAttachment = 1

  readonly calendar: CalendarEvent[] = []
  readonly todos: Todo[] = []
  private nextTodo = 1
  private matrixAccounts: MatrixAccount[] = []
  readonly rooms: Room[] = []
  /** The op that asked for each room's pending membership. */
  private readonly membershipBy = new Map<RoomId, number>()
  readonly events = new Map<RoomId, RoomEvent[]>()
  /** Of Matrix accounts, their security: unverified until recovered. */
  readonly security = new Map<string, MatrixSecurity>()
  /** Of rooms, their members, and what the account may do there. */
  readonly members = new Map<RoomId, { members: MemberInfo[]; powers: RoomPowers }>()
  /** Of rooms, their gaps, with the events missing there. */
  private readonly gaps = new Map<RoomId, { id: GapId; before: RoomEventId; missing: RoomEvent[] }[]>()
  private nextGap = 1
  /** Of rooms, how far their members have read. */
  readonly receipts = new Map<RoomId, MemberReceipt[]>()
  /** Of events edited, their edits, oldest first. */
  readonly edits = new Map<number, RoomEvent[]>()
  /** Messages being sent, sent and not yet synced back, or sent though
   * cancelled. */
  readonly matrixSends: MatrixSend[] = []
  private nextRoomEvent = 1

  attach(client: Client) {
    this.client = client
  }

  /** Edits a draft as another tab would. */
  editElsewhere(draft: string, splice: Splice) {
    this.editDraft(draft, this.drafts.get(draft)!.revision, splice)
  }

  addAccount(id: string, userLabels: string[] = []) {
    const labels = [
      ...SYSTEM.map((external) => this.label(external, external, true)),
      ...userLabels.map((name, i) => this.label(`Label_${i + 1}`, name, false)),
    ]
    this.accounts.push({ id, address: `${id}@example.com`, provider: 'gmail', labels })
  }

  /** A Microsoft 365 account: folders, of which an entry is in one, and
   * categories. */
  addM365Account(id: string, folders: string[], categories: string[] = []) {
    const labels = [
      ...M365_FOLDERS.map((external) => this.label(external, external, true, true)),
      ...['UNREAD', 'STARRED', 'IMPORTANT'].map((external) => this.label(external, external, true)),
      ...folders.map((name, i) => this.label(`Folder_${i + 1}`, name, false, true)),
      ...categories.map((name) => this.label(`cat:${name}`, name, false)),
    ]
    this.accounts.push({ id, address: `${id}@example.com`, provider: 'm365', labels })
  }

  private label(external_id: string, name: string, system: boolean, folder = false): Label {
    return { id: this.nextLabel++, external_id, name, system, folder, total: 0, unread: 0 }
  }

  labelId(account: string, externalOrName: string): LabelId {
    const label = this.accounts
      .find((a) => a.id === account)!
      .labels.find((l) => l.external_id === externalOrName || l.name === externalOrName)
    if (!label) throw new Error(`no label ${externalOrName} in ${account}`)
    return label.id
  }

  /** Adds a message, as syncing would, in `thread` if given; returns its
   * entry. */
  deliver(account: string, subject: string, labels: string[] = ['INBOX', 'UNREAD'], thread?: string): Entry {
    const id = this.nextEntry++
    const entry: Entry = {
      id,
      account,
      message: id,
      thread_id: thread ?? null,
      received_at: 1_700_000_000_000 + id * 60_000,
      subject,
      from_addr: 'bob@example.org',
      from_name: 'Bob',
      snippet: `About ${subject.toLowerCase()}`,
      has_attachments: false,
      labels: labels.map((l) => this.labelId(account, l)),
    }
    this.entries.push(entry)
    return entry
  }

  /** Records a sync observation and delivers its entry to the store. */
  observe(account: string, events: ServerEvent[]) {
    this.commit({ type: 'account', id: account }, 'Synced', events)
  }

  /** Adds an event to `account`'s calendar, as syncing would, without
   * saying so; {@link calendarSynced} does. Times are ISO strings. */
  addEvent(account: string, subject: string, start: string, end: string, more: Partial<CalendarEvent> = {}): CalendarEvent {
    const event: CalendarEvent = {
      id: this.calendar.length + 1,
      account,
      subject,
      location: '',
      preview: '',
      start: Date.parse(start),
      end: Date.parse(end),
      all_day: false,
      recurring: false,
      exception: false,
      ...more,
    }
    this.calendar.push(event)
    return event
  }

  /** Says that `account`'s calendar changed. */
  calendarSynced(account: string) {
    this.observe(account, [{ type: 'calendar_changed', account }])
  }

  /** Changes labels as the provider would, then observes it. */
  relabel(entry: EntryId, label: LabelId, desired: boolean) {
    const change = this.setLabel(entry, label, desired)!
    const account = this.entries.find((e) => e.id === entry)!.account
    this.observe(account, [{ type: 'labels_changed', changes: [change] }])
  }

  /** Configures the Matrix account `mx`, of user {@link ME}. */
  addMatrixAccount() {
    this.matrixAccounts.push({ id: 'mx', mxid: ME, synced: true })
    this.security.set('mx', {
      account: 'mx',
      device_id: 'UPLINK',
      verified: false,
      cross_signing: false,
      backup: 'unknown',
      recovery: 'incomplete',
      devices: [
        {
          device_id: 'UPLINK',
          name: 'uplink (mx)',
          last_seen_ts: Date.UTC(2026, 10, 10, 12),
          last_seen_ip: '10.0.0.1',
          verified: false,
          current: true,
        },
        {
          device_id: 'PHONE',
          name: 'Element Android',
          last_seen_ts: Date.UTC(2026, 10, 9, 8),
          last_seen_ip: null,
          verified: true,
          current: false,
        },
      ],
      verifications: [],
    })
  }

  /** A verification of the account went on, as the other device says. */
  verificationChanged(account: string, flow_id: string, change: Partial<MatrixVerification>) {
    this.setVerification(account, flow_id, change)
    this.client.notice({ type: 'security_changed', account })
  }

  /** Another device of the account asks to verify. */
  verificationAsked(account: string, flow_id: string, other_device: string) {
    const security = this.security.get(account)!
    this.securityChanged(account, {
      ...security,
      verifications: [
        ...security.verifications,
        { flow_id, other_device, we_started: false, phase: { type: 'requested' } },
      ],
    })
  }

  private setVerification(account: string, flow_id: string, change: Partial<MatrixVerification>) {
    const security = this.security.get(account)!
    this.security.set(account, {
      ...security,
      verifications: security.verifications.map((v) => (v.flow_id === flow_id ? { ...v, ...change } : v)),
    })
  }

  /** What the account's client has changed, as its streams say. */
  securityChanged(account: string, security: MatrixSecurity) {
    this.security.set(account, security)
    this.client.notice({ type: 'security_changed', account })
  }

  /** Adds a joined room of the Matrix account, as syncing would. */
  addRoom(name: string, encrypted = false, membership: Membership = 'joined'): Room {
    const room: Room = {
      id: this.rooms.length + 1,
      account: 'mx',
      room_id: `!${name.toLowerCase()}:example.org`,
      name,
      encrypted,
      membership,
      unread: 0,
      last_event_ts: null,
      read_up_to: null,
      pending: null,
      membership_error: null,
      space: false,
      avatar: null,
      children: [],
      topic: null,
      joined_members: 2,
    }
    this.rooms.push(room)
    this.events.set(room.id, [])
    this.observe('mx', [{ type: 'rooms_changed', rooms: [room.id] }])
    return room
  }

  /** An event arrives in the room, as syncing would deliver it. */
  private event(id: RoomEventId, event: Partial<RoomEvent>): RoomEvent {
    return {
      id,
      pos: id * 1024,
      event_id: `$${id}`,
      sender: '@bob:example.org',
      sender_name: 'Bob',
      ts: 1_700_000_000_000 + id * 60_000,
      kind: 'm.room.message',
      msgtype: 'm.text',
      body: null,
      raw: '{}',
      encryption: 'none',
      redacted: false,
      edit: null,
      reactions: [],
      replied: null,
      html: null,
      thread: null,
      thread_root: null,
      ...event,
    }
  }

  /** A message of the thread whose root is `root` arrives: the root sums
   * the thread up anew, as the server does. */
  receiveInThread(room: RoomId, root: RoomEvent, event: Partial<RoomEvent>): RoomEvent {
    const id = this.nextRoomEvent++
    const full = this.event(id, { ...event, thread_root: root.id })
    const events = this.events.get(room)!
    events.push(full)
    const replies = events.filter((e) => e.thread_root === root.id).length
    const { event_id, sender, sender_name, kind, redacted, msgtype, body } = full
    const latest = { id, event_id, sender, sender_name, kind, redacted, msgtype, body }
    const at = events.findIndex((e) => e.id === root.id)
    events[at] = { ...events[at], thread: { replies, latest } }
    this.observe('mx', [
      { type: 'timeline_added', room, events: [id] },
      { type: 'timeline_changed', room, events: [root.id] },
    ])
    return full
  }

  receive(room: RoomId, event: Partial<RoomEvent>): RoomEvent {
    const id = this.nextRoomEvent++
    const full = this.event(id, event)
    this.events.get(room)!.push(full)
    this.observe('mx', [
      { type: 'timeline_added', room, events: [id] },
      { type: 'rooms_changed', rooms: [room] },
    ])
    return full
  }

  /** Events before `before` are missing, as a limited sync leaves them:
   * `missing`, oldest first, which filling the gap fetches. */
  gap(room: RoomId, before: RoomEvent, missing: Partial<RoomEvent>[]): GapId {
    const id = this.nextGap++
    const events = missing.map((event, i) => {
      const eventId = this.nextRoomEvent++
      return {
        ...this.event(eventId, event),
        pos: before.pos - missing.length + i,
      }
    })
    this.gaps.set(room, [...(this.gaps.get(room) ?? []), { id, before: before.id, missing: events }])
    return id
  }

  /** The read marker at the room's event. */
  marker(room: RoomId, event: RoomEventId): ReadMarker {
    return { event, pos: this.events.get(room)!.find((e) => e.id === event)!.pos }
  }

  /** Sync changes what an event of the room shows (an edit, a reaction). */
  change(room: RoomId, event: RoomEvent, change: Partial<RoomEvent>) {
    Object.assign(event, change)
    this.observe('mx', [{ type: 'timeline_changed', room, events: [event.id] }])
  }

  /** An event decrypted late turned out a relation (a reaction, an
   * edit): it is no longer listed, and what it is of changed. */
  relation(room: RoomId, event: RoomEvent, of: RoomEvent, change: Partial<RoomEvent>) {
    const events = this.events.get(room)!
    events.splice(events.indexOf(event), 1)
    Object.assign(of, change)
    this.observe('mx', [{ type: 'timeline_changed', room, events: [event.id, of.id] }])
  }

  /** The account's sync task reports what it has not pushed yet. */
  pend(account: string, failing: Failing | null, changes: PendingChanges[]) {
    this.pending.set(account, { failing, changes })
    this.observe(account, [{ type: 'outbox_changed', accounts: [account] }])
  }

  /** Mail the provider sent though cancelled meanwhile (SPEC §6.7). */
  sentThoughCancelled(account: string, subject: string) {
    const content = { to: 'bob@example.org', cc: '', bcc: '', subject, body: '' }
    const draft = { id: `sent-${this.nextOutbox}`, account, content, attachments: [], in_reply_to: null, revision: 0, updated_at: 0 }
    this.outbox.push({ id: this.nextOutbox++, account, draft, state: 'sent', wanted: false, error: null, not_before: 0 })
    this.observe(account, [{ type: 'outbox_changed', accounts: [account] }])
  }

  /** The homeserver sends a message, and sync echoes it: it is reconciled. */
  echo(item: number) {
    const index = this.matrixSends.findIndex((i) => i.id === item)
    const [sent] = this.matrixSends.splice(index, 1)
    this.receive(sent.room, { sender: ME, sender_name: 'Me', body: sendSummary(sent.content) })
    this.observe('mx', this.matrixSendsChanged(sent.room))
  }

  /** Members are typing in the room, as sync says. */
  typing(room: RoomId, users: RoomMember[]) {
    this.client.notice({ type: 'typing', room, users })
  }

  /** Members read the room, as their receipts say. */
  read(room: RoomId, receipts: MemberReceipt[]) {
    this.receipts.set(room, receipts)
    this.client.notice({ type: 'receipts_changed', room })
  }

  /** The room's sends went out, and sync brought back what they did. */
  settleSends(room: RoomId) {
    this.matrixSends.splice(0, this.matrixSends.length, ...this.matrixSends.filter((s) => s.room !== room))
    this.observe('mx', this.matrixSendsChanged(room))
  }

  /** The homeserver took a message that was cancelled meanwhile. */
  sentThoughCancelledToRoom(room: RoomId, body: string) {
    this.matrixSends.push({ id: this.nextOutbox++, room, content: text(body), state: 'sent', wanted: false, error: null, queued_at: 0 })
    this.observe('mx', this.matrixSendsChanged(room))
  }

  /** The homeserver refuses the room's pending change. */
  refuseMembership(room: RoomId, reason: string) {
    this.rooms.find((r) => r.id === room)!.membership_error = reason
    this.observe('mx', [
      { type: 'rooms_changed', rooms: [room] },
      { type: 'outbox_changed', accounts: ['mx'] },
    ])
  }

  /** The sync task does as asked: the room's pending change takes effect. */
  settleMembership(room: RoomId) {
    const row = this.rooms.find((r) => r.id === room)!
    row.membership = row.pending === 'join' ? 'joined' : 'left'
    row.pending = null
    this.observe('mx', [{ type: 'rooms_changed', rooms: [room] }])
  }

  private matrixRooms(): Room[] {
    const listed = this.rooms.filter((room) => room.membership !== 'left' || room.pending === 'join')
    return listed.map((room) => {
      const events = this.events.get(room.id)!
      const unread = events.filter(
        (e) => e.pos > (room.read_up_to?.pos ?? -Infinity) && e.kind === 'm.room.message' && e.sender !== ME,
      ).length
      return { ...room, unread, last_event_ts: events.at(-1)?.ts ?? null }
    })
  }

  /** HTML bodies by message, for messages that have one. */
  readonly html = new Map<number, string>()
  readonly text = new Map<number, string>()
  /** Attachments by message, for messages that have some. */
  readonly attachments = new Map<number, Attachment[]>()

  labelsOf(entry: EntryId): LabelId[] {
    return this.entries.find((e) => e.id === entry)!.labels
  }

  async upload(file: Blob): Promise<UploadedBlob> {
    return this.storeBlob(`blob:${await file.text()}`, file.size)
  }

  async copyAttachment(message: MessageId, index: number): Promise<UploadedBlob> {
    const attachment = this.attachments.get(message)![index]
    return this.storeBlob(`attachment:${message}:${index}`, attachment.size)
  }

  async exportRoomKeys(account: string, passphrase: string): Promise<Blob> {
    return new Blob([`-----BEGIN MEGOLM SESSION DATA-----\n${account}:${passphrase}\n`])
  }

  private storeBlob(blob: string, size: number): UploadedBlob {
    this.blobs.set(blob, size)
    return { blob, size }
  }

  /** Accounts whose first sync is under way, with its progress if known. */
  readonly syncing = new Map<string, ListingProgress | null>()

  /** Runs before the next request is handled, as if it happened while
   * the request was on its way. */
  meanwhile: (() => void) | null = null

  async request(body: ClientRequest): Promise<ServerResponse> {
    this.requests.push(body)
    await Promise.resolve()
    const meanwhile = this.meanwhile
    this.meanwhile = null
    meanwhile?.()
    switch (body.type) {
      case 'list_accounts':
        return { type: 'accounts', as_of: this.head(), accounts: this.listAccounts() }
      case 'list_entries': {
        const list = body.list
        const trash = list.type === 'label' && HIDING.includes(this.labelById(list.label).external_id)
        const all = this.conversations(
          (e) => (trash && list.type === 'label' ? e.labels.includes(list.label) : !this.hidden(e)),
          (e) => {
            switch (list.type) {
              case 'label':
                return e.labels.includes(list.label)
              case 'all_mail':
                return e.account === list.account
              case 'inbox':
                return e.labels.some((l) => this.labelById(l).external_id === 'INBOX')
            }
          },
        )
        const start = body.after ? all.findIndex((c) => c.latest.id === body.after!.entry) + 1 : 0
        const page = all.slice(start, start + body.limit)
        const last = page.at(-1)?.latest
        const more = start + body.limit < all.length
        return {
          type: 'conversations',
          as_of: this.head(),
          conversations: page,
          next: more && last ? { received_at: last.received_at, entry: last.id } : null,
        }
      }
      case 'search': {
        const query = body.query.toLowerCase()
        const found = this.conversations(
          (e) => !this.hidden(e),
          (e) => `${e.subject} ${e.snippet}`.toLowerCase().includes(query),
        )
        return { type: 'conversations', as_of: this.head(), conversations: found.slice(0, body.limit), next: null }
      }
      case 'list_conversation': {
        const entry = this.entries.find((e) => e.id === body.entry)!
        const entries = this.sorted()
          .reverse()
          .filter((e) => e.id === entry.id || (entry.thread_id !== null && e.account === entry.account && e.thread_id === entry.thread_id))
        return { type: 'entries', as_of: this.head(), entries: entries.map((e) => ({ ...e })) }
      }
      case 'load_message': {
        const entry = this.entries.find((e) => e.message === body.message)!
        return {
          type: 'message',
          message: {
            id: entry.message,
            message_id: `${entry.message}@example.org`,
            subject: entry.subject,
            from_addr: entry.from_addr,
            from_name: entry.from_name,
            to_addrs: [`${entry.account}@example.com`],
            cc_addrs: [],
            date_sent: entry.received_at / 1000,
            body_text: this.text.get(entry.message) ?? `Body of ${entry.subject}`,
            body_html: this.html.get(entry.message) ?? null,
            attachments: this.attachments.get(entry.message) ?? [],
          },
        }
      }
      case 'modify_labels': {
        const changes: LabelState[] = []
        for (const entry of body.entries) {
          for (const label of body.add) changes.push(...this.maybe(this.setLabel(entry, label, true)))
          for (const label of body.remove) changes.push(...this.maybe(this.setLabel(entry, label, false)))
        }
        const id = this.commit({ type: 'user' }, `Changed ${body.entries.length} messages`, [
          { type: 'labels_changed', changes },
        ])
        this.undoableLabels(id, changes)
        return { type: 'dispatched', tx_log_id: id }
      }
      case 'undo': {
        const op = this.undoable.get(body.tx_log_id)
        if (!op) throw new RequestError({ code: 'not_undoable', message: 'not undoable' })
        if (op.undone) throw new RequestError({ code: 'already_undone', message: 'already undone' })
        op.undone = true
        this.undoing = body.tx_log_id
        try {
          return { type: 'undone', tx_log_id: op.revert() }
        } finally {
          this.undoing = null
        }
      }
      case 'list_history': {
        const user = this.log.filter((e) => e.source.type === 'user' && e.id < (body.before ?? Infinity))
        const page = user.reverse().slice(0, body.limit)
        return {
          type: 'history',
          as_of: this.head(),
          entries: page.map((entry) => ({
            entry,
            undone_by: this.log.find((e) => e.undoes === entry.id)?.id ?? null,
          })),
          more: user.length > body.limit,
        }
      }
      case 'list_drafts':
        return { type: 'drafts', as_of: this.head(), drafts: [...this.drafts.values()] }
      case 'list_outbox': {
        const ids = new Set([...this.pending.keys(), ...this.outbox.map((i) => i.account)])
        const accounts: AccountOutbox[] = [...ids].map((account) => ({
          account,
          failing: this.pending.get(account)?.failing ?? null,
          pending: {
            kind: 'mail',
            changes: this.pending.get(account)?.changes ?? [],
            sends: this.outbox.filter((i) => i.account === account),
          },
        }))
        const memberships = this.rooms
          .filter((r) => r.pending !== null)
          .map((r) => ({
            room: r.id,
            change: r.pending!,
            op: { id: this.membershipBy.get(r.id)!, description: `${r.pending === 'join' ? 'Joined' : 'Left'} '${r.name}'`, ts: 0 },
            refused: r.membership_error,
          }))
        if (this.matrixSends.length > 0 || memberships.length > 0) {
          accounts.push({
            account: 'mx',
            failing: null,
            pending: { kind: 'matrix', sends: [...this.matrixSends], memberships, receipts: [] },
          })
        }
        accounts.sort((a, b) => a.account.localeCompare(b.account))
        return { type: 'outbox', as_of: this.head(), accounts }
      }
      case 'keep_observed': {
        const pending = this.pending.get(body.account)!
        const change = pending.changes.find((c) => (c.op?.id ?? null) === body.op)!
        const kept = change.refused.reduce((n, r) => n + r.entries, 0)
        change.refused = []
        change.entries -= kept
        pending.changes = pending.changes.filter((c) => c.entries > 0)
        return this.dispatched('Kept what the provider has', [{ type: 'outbox_changed', accounts: [body.account] }])
      }
      case 'create_draft': {
        if (this.drafts.has(body.id)) throw new RequestError({ code: 'conflict', message: 'exists' })
        const { id, account, content, in_reply_to } = body
        this.drafts.set(id, { id, account, content, attachments: [], in_reply_to, revision: 0, updated_at: Date.now() })
        return this.dispatched('Saved draft', [{ type: 'drafts_changed', drafts: [id] }])
      }
      case 'edit_draft':
        return { type: 'dispatched', tx_log_id: this.editDraft(body.draft, body.revision, body.splice) }
      case 'attach_to_draft': {
        const draft = this.drafts.get(body.draft)!
        const attachment = {
          id: this.nextAttachment++,
          blob: body.blob,
          name: body.name,
          mime: body.mime,
          size: this.blobs.get(body.blob)!,
        }
        this.drafts.set(draft.id, { ...draft, attachments: [...draft.attachments, attachment] })
        const response = this.dispatched(`Attached '${body.name}'`, [{ type: 'drafts_changed', drafts: [draft.id] }])
        this.undoable.set(this.head(), {
          revert: () => {
            const current = this.drafts.get(draft.id)!
            this.drafts.set(draft.id, {
              ...current,
              attachments: current.attachments.filter((a) => a.id !== attachment.id),
            })
            return this.commit({ type: 'user' }, 'Undid it', [{ type: 'drafts_changed', drafts: [draft.id] }])
          },
          undone: false,
        })
        return response
      }
      case 'detach_from_draft': {
        const draft = this.drafts.get(body.draft)!
        const attachments = draft.attachments.filter((a) => a.id !== body.attachment)
        this.drafts.set(draft.id, { ...draft, attachments })
        return this.dispatched('Removed attachment', [{ type: 'drafts_changed', drafts: [draft.id] }])
      }
      case 'delete_draft':
        this.drafts.delete(body.draft)
        return this.dispatched('Discarded draft', [{ type: 'drafts_changed', drafts: [body.draft] }])
      case 'send_draft': {
        const draft = this.drafts.get(body.draft)!
        this.drafts.delete(body.draft)
        const id = this.nextOutbox++
        this.outbox.push({ id, account: draft.account, draft, state: 'queued', wanted: true, error: null, not_before: Date.now() + 10_000 })
        const response = this.dispatched(`Sent '${draft.content.subject}'`, [
          { type: 'drafts_changed', drafts: [draft.id] },
          { type: 'outbox_changed', accounts: [draft.account] },
        ])
        this.undoable.set(this.head(), { revert: () => this.cancelSend(id, 'Undid it'), undone: false })
        return response
      }
      case 'cancel_send':
        return { type: 'dispatched', tx_log_id: this.cancelSend(body.item, 'Cancelled sending') }
      case 'keep_sent': {
        // Sent and synced, it leaves the outbox once wanted again.
        const index = this.outbox.findIndex((i) => i.id === body.item && i.state === 'sent' && !i.wanted)
        const [kept] = this.outbox.splice(index, 1)
        return this.dispatched('Kept it sent', [{ type: 'outbox_changed', accounts: [kept.account] }])
      }
      case 'list_rooms':
        return {
          type: 'rooms',
          as_of: this.head(),
          accounts: [...this.matrixAccounts],
          rooms: this.matrixRooms(),
        }
      case 'list_timeline':
        return { type: 'timeline', as_of: this.head(), room: body.room, ...this.timeline(body.room, body.page, body.limit) }
      case 'list_room_events':
        return {
          type: 'room_events',
          as_of: this.head(),
          room: body.room,
          events: this.events.get(body.room)!.filter((e) => body.events.includes(e.id)),
        }
      case 'list_edits':
        return { type: 'edits', as_of: this.head(), room: body.room, event: body.event, edits: this.edits.get(body.event) ?? [] }
      case 'list_thread': {
        const events = this.events.get(body.room)!
        const root = events.find((e) => e.id === body.root && e.thread_root === null)
        if (!root) throw new RequestError({ code: 'not_found', message: `no event ${body.root}` })
        const replies = events.filter((e) => e.thread_root === root.id)
        return { type: 'thread', as_of: this.head(), room: body.room, root, replies }
      }
      case 'fill_gap': {
        const gaps = this.gaps.get(body.room) ?? []
        const gap = gaps.find((g) => g.id === body.gap)
        if (!gap) throw new RequestError({ code: 'not_found', message: `no gap ${body.gap}` })
        gaps.splice(gaps.indexOf(gap), 1)
        const events = this.events.get(body.room)!
        events.push(...gap.missing)
        events.sort((a, b) => a.pos - b.pos)
        this.observe('mx', [
          { type: 'timeline_filled', room: body.room, gap: gap.id, events: gap.missing.map((e) => e.id), left: null },
        ])
        return { type: 'done' }
      }
      case 'list_receipts':
        return { type: 'receipts', room: body.room, receipts: this.receipts.get(body.room) ?? [] }
      case 'create_matrix_room': {
        const room = this.addRoom(body.room.name, body.room.encrypted)
        room.topic = body.room.topic
        return { type: 'matrix_room_joined', room_id: room.room_id }
      }
      case 'start_direct_chat': {
        const name = /^@([^:]+)/.exec(body.user)?.[1]
        if (!name) throw new RequestError({ code: 'invalid', message: `not a Matrix user id: "${body.user}"` })
        const room = this.rooms.find((r) => r.name === name) ?? this.addRoom(name, true)
        return { type: 'matrix_room_joined', room_id: room.room_id }
      }
      case 'join_matrix_room': {
        const room = this.rooms.find((r) => r.room_id === body.room || `#${r.name.toLowerCase()}:example.org` === body.room)
        if (!room) throw new RequestError({ code: 'invalid', message: 'the homeserver refused: no such room' })
        room.membership = 'joined'
        this.observe('mx', [{ type: 'rooms_changed', rooms: [room.id] }])
        return { type: 'matrix_room_joined', room_id: room.room_id }
      }
      case 'list_members': {
        const { members, powers } = this.members.get(body.room) ?? { members: [], powers: NO_POWERS }
        return { type: 'members', room: body.room, members, powers }
      }
      case 'set_typing':
        return { type: 'done' }
      case 'get_matrix_security':
        return { type: 'matrix_security', security: this.security.get(body.account)! }
      case 'recover_matrix': {
        if (body.recovery !== RECOVERY_KEY) {
          throw new RequestError({ code: 'invalid', message: 'could not unlock Secret Storage: wrong key' })
        }
        const security = this.security.get(body.account)!
        const recovered: MatrixSecurity = {
          ...security,
          verified: true,
          cross_signing: true,
          backup: 'enabled',
          recovery: 'enabled',
          devices: security.devices.map((d) => (d.current ? { ...d, verified: true } : d)),
        }
        this.security.set(body.account, recovered)
        return { type: 'matrix_security', security: recovered }
      }
      case 'verify_matrix_device': {
        const security = this.security.get(body.account)!
        const verification: MatrixVerification = {
          flow_id: `flow${security.verifications.length + 1}`,
          other_device: body.device,
          we_started: true,
          phase: { type: 'requested' },
        }
        this.security.set(body.account, { ...security, verifications: [...security.verifications, verification] })
        return { type: 'matrix_security', security: this.security.get(body.account)! }
      }
      case 'answer_verification': {
        const phase: VerificationPhase = {
          accept: { type: 'ready' } as const,
          confirm: { type: 'confirmed' } as const,
          mismatch: { type: 'cancelled', reason: 'The short auth string did not match.', by_us: true } as const,
          cancel: { type: 'cancelled', reason: 'The user cancelled the verification.', by_us: true } as const,
        }[body.answer]
        this.setVerification(body.account, body.flow_id, { phase })
        return { type: 'matrix_security', security: this.security.get(body.account)! }
      }
      case 'import_room_keys':
        if (!this.blobs.has(body.blob)) throw new Error(`no blob ${body.blob}`)
        if (body.passphrase !== 'export passphrase') {
          throw new RequestError({ code: 'invalid', message: 'importing the room keys: wrong passphrase' })
        }
        return { type: 'room_keys_imported', imported: 3, total: 5 }
      case 'list_matrix_sends':
        return {
          type: 'matrix_sends',
          as_of: this.head(),
          room: body.room,
          sends: this.matrixSends.filter((s) => s.room === body.room),
        }
      case 'send_matrix_message': {
        const id = this.nextOutbox++
        this.matrixSends.push({
          id,
          room: body.room,
          content: body.content,
          state: 'queued',
          wanted: true,
          error: null,
          queued_at: Date.now(),
        })
        const response = this.dispatched(`Sent '${sendSummary(body.content)}'`, this.matrixSendsChanged(body.room))
        this.undoable.set(this.head(), { revert: () => this.cancelMatrixSend(id, 'Undid it'), undone: false })
        return response
      }
      case 'cancel_matrix_send':
        return { type: 'dispatched', tx_log_id: this.cancelMatrixSend(body.item, 'Cancelled sending') }
      case 'keep_matrix_sent': {
        // Its event synced back, it is reconciled once wanted again.
        const index = this.matrixSends.findIndex((s) => s.id === body.item && s.state === 'sent' && !s.wanted)
        const [kept] = this.matrixSends.splice(index, 1)
        return this.dispatched('Kept it sent', this.matrixSendsChanged(kept.room))
      }
      case 'list_message_entries':
        return {
          type: 'entries',
          as_of: this.head(),
          entries: this.sorted().filter((e) => e.message === body.message).map((e) => ({ ...e })),
        }
      case 'list_events':
        return {
          type: 'events',
          as_of: this.head(),
          events: this.calendar
            .filter((e) => e.start < body.to && (e.end > body.from || e.start >= body.from))
            .sort((a, b) => a.start - b.start),
        }
      case 'list_todos':
        return { type: 'todos', as_of: this.head(), todos: this.sortedTodos() }
      case 'create_todo': {
        const todo: Todo = {
          id: this.nextTodo++,
          title: body.title,
          notes: body.notes,
          due: body.due,
          done_at: null,
          message: body.message,
          created_at: Date.now(),
        }
        return this.putTodo(todo, body.message === null ? `Added '${todo.title}'` : `Flagged '${todo.title}'`)
      }
      case 'edit_todo':
        return { type: 'dispatched', tx_log_id: this.editTodo(body.todo, body.edit, `Edited '${body.todo}'`) }
      case 'delete_todo':
        return { type: 'dispatched', tx_log_id: this.deleteTodo(body.todo) }
      case 'mark_room_read':
        this.rooms.find((r) => r.id === body.room)!.read_up_to = this.marker(body.room, body.event)
        return this.dispatched('Read', [{ type: 'rooms_changed', rooms: [body.room] }])
      case 'change_membership': {
        const room = this.rooms.find((r) => r.id === body.room)!
        room.pending = body.change
        const changed: ServerEvent[] = [
          { type: 'rooms_changed', rooms: [room.id] },
          { type: 'outbox_changed', accounts: ['mx'] },
        ]
        const response = this.dispatched(body.change === 'join' ? 'Joined' : 'Left', changed)
        this.membershipBy.set(room.id, this.head())
        this.undoable.set(this.head(), {
          revert: () => {
            room.pending = null
            room.membership_error = null
            return this.commit({ type: 'user' }, 'Undid it', changed)
          },
          undone: false,
        })
        return response
      }
    }
  }

  private undoableLabels(id: number, changes: LabelState[]) {
    this.undoable.set(id, {
      revert: () => {
        const inverse = changes.map((c) => this.setLabel(c.entry, c.label, !c.desired)!)
        const undo = this.commit({ type: 'user' }, 'Undid it', [{ type: 'labels_changed', changes: inverse }])
        this.undoableLabels(undo, inverse)
        return undo
      },
      undone: false,
    })
  }

  private sortedTodos(): Todo[] {
    const key = (t: Todo) => [t.done_at === null ? 0 : 1, t.due ?? '9999', t.id] as const
    return [...this.todos].sort((a, b) => {
      const [ka, kb] = [key(a), key(b)]
      return ka[0] - kb[0] || ka[1].localeCompare(kb[1]) || ka[2] - kb[2]
    })
  }

  private putTodo(todo: Todo, description: string): ServerResponse {
    this.todos.push(todo)
    const response = this.dispatched(description, [{ type: 'todos_changed', todos: [todo.id] }])
    this.undoable.set(this.head(), { revert: () => this.deleteTodo(todo.id, 'Undid it'), undone: false })
    return response
  }

  private deleteTodo(id: number, description = 'Deleted'): number {
    const [todo] = this.todos.splice(this.todos.findIndex((t) => t.id === id), 1)
    const entry = this.commit({ type: 'user' }, description, [{ type: 'todos_changed', todos: [id] }])
    this.undoable.set(entry, {
      revert: () => {
        this.todos.push(todo)
        return this.commit({ type: 'user' }, 'Undid it', [{ type: 'todos_changed', todos: [id] }])
      },
      undone: false,
    })
    return entry
  }

  private editTodo(id: number, edit: TodoEdit, description: string): number {
    const todo = this.todos.find((t) => t.id === id)!
    const before = { ...todo }
    switch (edit.field) {
      case 'title':
        todo.title = edit.title
        break
      case 'notes':
        todo.notes = edit.notes
        break
      case 'due':
        todo.due = edit.due
        break
      case 'done':
        todo.done_at = edit.done ? Date.now() : null
        break
    }
    const entry = this.commit({ type: 'user' }, description, [{ type: 'todos_changed', todos: [id] }])
    this.undoable.set(entry, {
      revert: () => {
        Object.assign(todo, before)
        return this.commit({ type: 'user' }, 'Undid it', [{ type: 'todos_changed', todos: [id] }])
      },
      undone: false,
    })
    return entry
  }

  private timeline(room: RoomId, page: TimelinePage, limit: number) {
    // The main timeline's: those of threads are the threads'.
    const all = this.events.get(room)!.filter((e) => e.thread_root === null)
    const pos = (id: RoomEventId) => all.find((e) => e.id === id)!.pos
    let events: RoomEvent[]
    switch (page.type) {
      case 'latest':
        events = all.slice(-limit)
        break
      case 'before':
        events = all.filter((e) => e.pos < pos(page.event)).slice(-limit)
        break
      case 'after':
        events = all.filter((e) => e.pos > pos(page.event)).slice(0, limit)
        break
    }
    const first = events[0]
    const ids = new Set(events.map((e) => e.id))
    const gaps = (this.gaps.get(room) ?? [])
      .filter((g) => ids.has(g.before))
      .map((g) => ({ id: g.id, before: g.before }))
    return { events, more_before: first !== undefined && all[0].pos < first.pos, gaps }
  }

  private cancelMatrixSend(item: number, description: string): number {
    const [cancelled] = this.matrixSends.splice(
      this.matrixSends.findIndex((i) => i.id === item),
      1,
    )
    return this.commit({ type: 'user' }, description, this.matrixSendsChanged(cancelled.room))
  }

  private matrixSendsChanged(room: RoomId): ServerEvent[] {
    return [
      { type: 'matrix_sends_changed', rooms: [room] },
      { type: 'outbox_changed', accounts: ['mx'] },
    ]
  }

  private cancelSend(item: number, description: string): number {
    const index = this.outbox.findIndex((i) => i.id === item)
    const [cancelled] = this.outbox.splice(index, 1)
    this.drafts.set(cancelled.draft.id, cancelled.draft)
    return this.commit({ type: 'user' }, description, [
      { type: 'drafts_changed', drafts: [cancelled.draft.id] },
      { type: 'outbox_changed', accounts: [cancelled.account] },
    ])
  }

  private dispatched(description: string, events: ServerEvent[]): ServerResponse {
    return { type: 'dispatched', tx_log_id: this.commit({ type: 'user' }, description, events) }
  }

  private editDraft(id: string, revision: number, splice: Splice): number {
    const draft = this.drafts.get(id)
    if (!draft) throw new RequestError({ code: 'conflict', message: 'the draft no longer exists' })
    if (draft.revision !== revision) {
      throw new RequestError({ code: 'conflict', message: `the draft is at revision ${draft.revision}` })
    }
    const content = { ...draft.content, [splice.field]: apply(draft.content[splice.field], splice) }
    this.drafts.set(id, { ...draft, content, revision: revision + 1 })
    return this.commit({ type: 'user' }, 'Edited draft', [
      { type: 'draft_edited', draft: id, revision: revision + 1, splice },
    ])
  }

  private maybe(change: LabelState | null): LabelState[] {
    return change ? [change] : []
  }

  /** `null` if the entry already is as desired. */
  private setLabel(entry: EntryId, label: LabelId, desired: boolean): LabelState | null {
    const row = this.entries.find((e) => e.id === entry)!
    if (row.labels.includes(label) === desired) return null
    row.labels = desired ? [...row.labels, label] : row.labels.filter((l) => l !== label)
    return { entry, label, desired, observed: false }
  }

  private commit(source: LogEntry['source'], description: string, events: ServerEvent[]): number {
    const entry: LogEntry = {
      id: this.head() + 1,
      ts: Date.now(),
      source,
      description,
      op_type: 'test',
      undoes: this.undoing,
      events,
    }
    this.log.push(entry)
    this.client.apply(entry)
    return entry.id
  }

  lastDescription(): string {
    return this.log.at(-1)!.description
  }

  private head(): number {
    return this.log.at(-1)?.id ?? 0
  }

  private sorted(): Entry[] {
    return [...this.entries].sort((a, b) => b.received_at - a.received_at || b.id - a.id)
  }

  /** The conversations of the entries in scope, of which one matches;
   * newest first, as the server lists them. */
  private conversations(scope: (e: Entry) => boolean, matches: (e: Entry) => boolean): Conversation[] {
    const byKey = new Map<string, Entry[]>()
    for (const entry of this.sorted().reverse().filter(scope)) {
      const key = entry.thread_id === null ? `#${entry.id}` : `${entry.account}/${entry.thread_id}`
      byKey.set(key, [...(byKey.get(key) ?? []), entry])
    }
    return [...byKey.values()]
      .filter((entries) => entries.some(matches))
      .map((entries) => {
        const latest = entries.at(-1)!
        const senders: Sender[] = []
        for (const e of entries) {
          if (!senders.some((s) => s.addr === e.from_addr)) senders.push({ name: e.from_name, addr: e.from_addr })
        }
        return {
          latest: { ...latest },
          earlier: entries.slice(0, -1).map((e) => ({ id: e.id, message: e.message, labels: [...e.labels] })),
          senders,
          has_attachments: entries.some((e) => e.has_attachments),
        }
      })
      .sort((a, b) => b.latest.received_at - a.latest.received_at || b.latest.id - a.latest.id)
  }

  private labelById(id: LabelId): Label {
    return this.accounts.flatMap((a) => a.labels).find((l) => l.id === id)!
  }

  /** In Trash or Spam: listed only there, and not found by search. */
  private hidden(entry: Entry): boolean {
    return entry.labels.some((id) => HIDING.includes(this.labelById(id).external_id))
  }

  private listAccounts(): Account[] {
    return this.accounts.map((account) => {
      const unread = this.labelId(account.id, 'UNREAD')
      return {
        id: account.id,
        address: account.address,
        provider: account.provider,
        synced: !this.syncing.has(account.id),
        listing: this.syncing.get(account.id) ?? null,
        labels: account.labels.map((label) => {
          const showsHidden = HIDING.includes(label.external_id)
          const having = this.entries.filter((e) => e.labels.includes(label.id) && (showsHidden || !this.hidden(e)))
          // Conversations, as the server counts.
          const count = (entries: Entry[]) =>
            new Set(entries.map((e) => (e.thread_id === null ? `#${e.id}` : e.thread_id))).size
          return {
            ...label,
            total: count(having),
            unread: count(having.filter((e) => e.labels.includes(unread))),
          }
        }),
      }
    })
  }
}
