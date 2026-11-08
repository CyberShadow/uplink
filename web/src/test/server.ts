// An in-memory stand-in for uplink's mail, draft and outbox requests, for
// the store and view tests. Like the real server, it delivers an op's log
// entry before the op's response (SPEC §5.4).

import type { Requester } from '../connection'
import { RequestError } from '../connection'
import type {
  Account,
  ClientRequest,
  Draft,
  OutboxItem,
  Splice,
  Entry,
  EntryId,
  Label,
  LabelId,
  LabelState,
  LogEntry,
  MatrixAccount,
  MatrixOutboxItem,
  Room,
  RoomEvent,
  RoomId,
  ServerEvent,
  TimelinePage,
  Todo,
  TodoEdit,
  ServerResponse,
} from '../generated/protocol'
import type { Client } from '../stores/client'
import { apply } from '../compose/splice'

/** The Matrix account's user. */
export const ME = '@me:example.org'

const SYSTEM = ['INBOX', 'UNREAD', 'STARRED', 'SENT', 'DRAFT', 'TRASH', 'SPAM']
const HIDING = ['TRASH', 'SPAM']

interface Undoable {
  /** Applies the inverse, recording it as an entry; returns the entry. */
  revert: () => number
  undone: boolean
}

export class FakeServer implements Requester {
  readonly requests: ClientRequest[] = []
  private client!: Client
  private accounts: { id: string; address: string; labels: Label[] }[] = []
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

  readonly todos: Todo[] = []
  private nextTodo = 1
  private matrixAccounts: MatrixAccount[] = []
  readonly rooms: Room[] = []
  readonly events = new Map<RoomId, RoomEvent[]>()
  readonly matrixOutbox: MatrixOutboxItem[] = []
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
    this.accounts.push({ id, address: `${id}@example.com`, labels })
  }

  private label(external_id: string, name: string, system: boolean): Label {
    return { id: this.nextLabel++, external_id, name, system, total: 0, unread: 0 }
  }

  labelId(account: string, externalOrName: string): LabelId {
    const label = this.accounts
      .find((a) => a.id === account)!
      .labels.find((l) => l.external_id === externalOrName || l.name === externalOrName)
    if (!label) throw new Error(`no label ${externalOrName} in ${account}`)
    return label.id
  }

  /** Adds a message, as syncing would; returns its entry. */
  deliver(account: string, subject: string, labels: string[] = ['INBOX', 'UNREAD']): Entry {
    const id = this.nextEntry++
    const entry: Entry = {
      id,
      account,
      message: id,
      thread_id: null,
      received_at: 1_700_000_000_000 + id * 60_000,
      subject,
      from_addr: 'bob@example.org',
      from_name: 'Bob',
      snippet: `About ${subject.toLowerCase()}`,
      labels: labels.map((l) => this.labelId(account, l)),
    }
    this.entries.push(entry)
    return entry
  }

  /** Records a sync observation and delivers its entry to the store. */
  observe(account: string, events: ServerEvent[]) {
    this.commit({ type: 'account', id: account }, 'Synced', events)
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
  }

  /** Adds a joined room of the Matrix account, as syncing would. */
  addRoom(name: string, encrypted = false): Room {
    const room: Room = {
      id: this.rooms.length + 1,
      account: 'mx',
      room_id: `!${name.toLowerCase()}:example.org`,
      name,
      encrypted,
      membership: 'joined',
      unread: 0,
      last_event_ts: null,
      read_up_to: null,
    }
    this.rooms.push(room)
    this.events.set(room.id, [])
    this.observe('mx', [{ type: 'rooms_changed', rooms: [room.id] }])
    return room
  }

  /** An event arrives in the room, as syncing would deliver it. */
  receive(room: RoomId, event: Partial<RoomEvent>): RoomEvent {
    const id = this.nextRoomEvent++
    const full: RoomEvent = {
      id,
      event_id: `$${id}`,
      sender: '@bob:example.org',
      sender_name: 'Bob',
      ts: 1_700_000_000_000 + id * 60_000,
      kind: 'm.room.message',
      msgtype: 'm.text',
      body: null,
      raw: '{}',
      encryption: 'none',
      ...event,
    }
    this.events.get(room)!.push(full)
    this.observe('mx', [
      { type: 'timeline_added', room, events: [id] },
      { type: 'rooms_changed', rooms: [room] },
    ])
    return full
  }

  /** The homeserver sends an outbox item, and sync echoes it. */
  echo(item: number) {
    const index = this.matrixOutbox.findIndex((i) => i.id === item)
    const [sent] = this.matrixOutbox.splice(index, 1)
    this.receive(sent.room, { sender: ME, sender_name: 'Me', body: sent.body })
    this.observe('mx', [{ type: 'matrix_outbox_changed', items: [item] }])
  }

  private matrixRooms(): Room[] {
    return this.rooms.map((room) => {
      const events = this.events.get(room.id)!
      const unread = events.filter(
        (e) => e.id > (room.read_up_to ?? 0) && e.kind === 'm.room.message' && e.sender !== ME,
      ).length
      return { ...room, unread, last_event_ts: events.at(-1)?.ts ?? null }
    })
  }

  /** HTML bodies by message, for messages that have one. */
  readonly html = new Map<number, string>()

  labelsOf(entry: EntryId): LabelId[] {
    return this.entries.find((e) => e.id === entry)!.labels
  }

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
        const all = this.sorted().filter((e) =>
          list.type === 'label'
            ? e.labels.includes(list.label) &&
              (HIDING.includes(this.labelById(list.label).external_id) || !this.hidden(e))
            : e.account === list.account && !this.hidden(e),
        )
        const start = body.after
          ? all.findIndex((e) => e.id === body.after!.entry) + 1
          : 0
        const page = all.slice(start, start + body.limit)
        const last = page.at(-1)
        const more = start + body.limit < all.length
        return {
          type: 'entries',
          as_of: this.head(),
          entries: page.map((e) => ({ ...e })),
          next: more && last ? { received_at: last.received_at, entry: last.id } : null,
        }
      }
      case 'search': {
        const query = body.query.toLowerCase()
        const found = this.sorted().filter(
          (e) => !this.hidden(e) && `${e.subject} ${e.snippet}`.toLowerCase().includes(query),
        )
        return { type: 'entries', as_of: this.head(), entries: found.slice(0, body.limit), next: null }
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
            body_text: `Body of ${entry.subject}`,
            body_html: this.html.get(entry.message) ?? null,
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
      case 'list_outbox':
        return { type: 'outbox', as_of: this.head(), items: [...this.outbox] }
      case 'create_draft': {
        if (this.drafts.has(body.id)) throw new RequestError({ code: 'conflict', message: 'exists' })
        const { id, account, content, in_reply_to } = body
        this.drafts.set(id, { id, account, content, in_reply_to, revision: 0, updated_at: Date.now() })
        return this.dispatched('Saved draft', [{ type: 'drafts_changed', drafts: [id] }])
      }
      case 'edit_draft':
        return { type: 'dispatched', tx_log_id: this.editDraft(body.draft, body.revision, body.splice) }
      case 'delete_draft':
        this.drafts.delete(body.draft)
        return this.dispatched('Discarded draft', [{ type: 'drafts_changed', drafts: [body.draft] }])
      case 'send_draft': {
        const draft = this.drafts.get(body.draft)!
        this.drafts.delete(body.draft)
        const id = this.nextOutbox++
        this.outbox.push({ id, account: draft.account, draft, state: 'queued', error: null, not_before: Date.now() + 10_000 })
        const response = this.dispatched(`Sent '${draft.content.subject}'`, [
          { type: 'drafts_changed', drafts: [draft.id] },
          { type: 'outbox_changed', items: [id] },
        ])
        this.undoable.set(this.head(), { revert: () => this.cancelSend(id, 'Undid it'), undone: false })
        return response
      }
      case 'cancel_send':
        return { type: 'dispatched', tx_log_id: this.cancelSend(body.item, 'Cancelled sending') }
      case 'list_rooms':
        return {
          type: 'rooms',
          as_of: this.head(),
          accounts: [...this.matrixAccounts],
          rooms: this.matrixRooms(),
        }
      case 'list_timeline':
        return { type: 'timeline', as_of: this.head(), room: body.room, ...this.timeline(body.room, body.page, body.limit) }
      case 'list_matrix_outbox':
        return { type: 'matrix_outbox', as_of: this.head(), items: [...this.matrixOutbox] }
      case 'send_matrix_message': {
        const id = this.nextOutbox++
        this.matrixOutbox.push({ id, room: body.room, body: body.body, state: 'queued', error: null, queued_at: Date.now() })
        const response = this.dispatched(`Sent '${body.body}'`, [{ type: 'matrix_outbox_changed', items: [id] }])
        this.undoable.set(this.head(), { revert: () => this.cancelMatrixSend(id, 'Undid it'), undone: false })
        return response
      }
      case 'cancel_matrix_send':
        return { type: 'dispatched', tx_log_id: this.cancelMatrixSend(body.item, 'Cancelled sending') }
      case 'list_message_entries':
        return {
          type: 'entries',
          as_of: this.head(),
          entries: this.sorted().filter((e) => e.message === body.message).map((e) => ({ ...e })),
          next: null,
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
        this.rooms.find((r) => r.id === body.room)!.read_up_to = body.event
        return this.dispatched('Read', [{ type: 'rooms_changed', rooms: [body.room] }])
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
    const all = this.events.get(room)!
    let events: RoomEvent[]
    switch (page.type) {
      case 'latest':
        events = all.slice(-limit)
        break
      case 'before':
        events = all.filter((e) => e.id < page.event).slice(-limit)
        break
      case 'after':
        events = all.filter((e) => e.id > page.event).slice(0, limit)
        break
    }
    const first = events[0]
    return { events, more_before: first !== undefined && all[0].id < first.id }
  }

  private cancelMatrixSend(item: number, description: string): number {
    this.matrixOutbox.splice(this.matrixOutbox.findIndex((i) => i.id === item), 1)
    return this.commit({ type: 'user' }, description, [{ type: 'matrix_outbox_changed', items: [item] }])
  }

  private cancelSend(item: number, description: string): number {
    const index = this.outbox.findIndex((i) => i.id === item)
    const [cancelled] = this.outbox.splice(index, 1)
    this.drafts.set(cancelled.draft.id, cancelled.draft)
    return this.commit({ type: 'user' }, description, [
      { type: 'drafts_changed', drafts: [cancelled.draft.id] },
      { type: 'outbox_changed', items: [cancelled.id] },
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
        synced: true,
        labels: account.labels.map((label) => {
          const showsHidden = HIDING.includes(label.external_id)
          const having = this.entries.filter((e) => e.labels.includes(label.id) && (showsHidden || !this.hidden(e)))
          return {
            ...label,
            total: having.length,
            unread: having.filter((e) => e.labels.includes(unread)).length,
          }
        }),
      }
    })
  }
}
