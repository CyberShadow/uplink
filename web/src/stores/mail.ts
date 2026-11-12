// Mail state: accounts with their labels, and the message list on show,
// mirrored from query responses and the log entries that follow them.
//
// The server sends every entry a response reflects before the response
// itself (SPEC §5.4), so a response simply replaces what it covers, and
// each later entry applies on top.

import { batch, computed, signal } from '@preact/signals'
import type {
  Account,
  CalendarEvent,
  ClientRequest,
  Conversation,
  ConversationAction,
  ConversationSet,
  Entry,
  EntryId,
  FoundMessage,
  Label,
  LabelId,
  ListPosition,
  ListScope,
  LogEntry,
  MessageContent,
  MessageId,
  Room,
  Todo,
} from '../generated/protocol'
import type { Requester } from '../connection'
import type { Ops } from './ops'
import { query } from './query'

export const PAGE = 100
/** The server's limit on a page. */
const MAX_PAGE = 500

export type View =
  | { kind: 'label'; account: string; label: string }
  /** The account's mail outside Trash and Spam. */
  | { kind: 'all'; account: string }
  /** Every account's Inbox, as one. */
  | { kind: 'inbox' }
  | { kind: 'search'; query: string }
  /** A message's entries, in every account. */
  | { kind: 'message'; message: MessageId }

export function sameView(a: View, b: View): boolean {
  switch (a.kind) {
    case 'label':
      return b.kind === 'label' && a.account === b.account && a.label === b.label
    case 'all':
      return b.kind === 'all' && a.account === b.account
    case 'inbox':
      return b.kind === 'inbox'
    case 'search':
      return b.kind === 'search' && a.query === b.query
    case 'message':
      return b.kind === 'message' && a.message === b.message
  }
}

export interface List {
  view: View
  /** Whether the first response is in. */
  loaded: boolean
  conversations: Conversation[]
  next: ListPosition | null
  /** The entries its conversations are made of; known once loaded. */
  scope: ListScope | null
  /** What a search found besides mail; `null` of other lists, and until
   * loaded. */
  found: Found | null
}

/** A search's items of other kinds than mail (SPEC §8.7). */
export interface Found {
  rooms: Room[]
  messages: FoundMessage[]
  todos: Todo[]
  events: CalendarEvent[]
}

/** Every entry, as a message's conversation is made of. */
const EVERY: ListScope = { type: 'except', labels: [] }

/** Whether an entry with `labels` is in `scope`. */
export function inScope(scope: ListScope, labels: LabelId[]): boolean {
  return labels.some((l) => scope.labels.includes(l)) === (scope.type === 'within')
}

/** A value of the query language (SPEC §8.7), quoted. */
export function quoted(value: string): string {
  return `"${value.replace(/[\\"]/g, '\\$&')}"`
}

/** An entry as actions take it. */
export type Item = Pick<Entry, 'id' | 'message' | 'labels'>

/** A conversation's entries, as actions take them: oldest first. */
export interface Thread {
  account: string
  /** Its latest entry's, as a to-do flagging it is titled. */
  subject: string | null
  entries: Item[]
}

/** What an action is done to: the threads, as listed, or every
 * conversation the list's query lists. */
export type Targets = Thread[] | 'matching'

export function threadOf(conversation: Conversation): Thread {
  const { latest } = conversation
  return { account: latest.account, subject: latest.subject, entries: [...conversation.earlier, latest] }
}

/** Identifies a conversation while its entries come and go. */
export function conversationKey(conversation: Conversation): string {
  const { latest } = conversation
  return latest.thread_id === null ? `#${latest.id}` : `${latest.account}/${latest.thread_id}`
}

export function hasEntry(conversation: Conversation, entry: EntryId): boolean {
  return conversation.latest.id === entry || conversation.earlier.some((m) => m.id === entry)
}

/** An entry as a conversation by itself, as a message's entries are. */
function alone(entry: Entry): Conversation {
  return {
    latest: entry,
    earlier: [],
    senders: [{ name: entry.from_name, addr: entry.from_addr }],
    has_attachments: entry.has_attachments,
  }
}

/** The conversation open: its entries, all of them, oldest first. */
export interface Open {
  entry: EntryId
  entries: Entry[] | null
}

type Load = 'reload' | 'more'

export class MailStore {
  readonly accounts = signal<Account[] | null>(null)
  readonly list = signal<List | null>(null)
  /** The conversation j/k move between and single actions apply to. */
  readonly focus = signal<string | null>(null)
  readonly selected = signal<ReadonlySet<string>>(new Set())
  /** Whether actions apply to every conversation the list's query lists,
   * rather than the selection; then all those loaded are selected. */
  readonly matching = signal(false)
  readonly open = signal<Open | null>(null)
  readonly error = signal<string | null>(null)
  /** Whether senders' remote images are shown unasked, as changed since
   * their messages loaded, by lowercased address: newer than what those
   * say (`always_show_images`). */
  readonly imageSenders = signal<ReadonlyMap<string, boolean>>(new Map())
  /** Whether the list keeps the conversation open though it may no
   * longer be listed: reading it may take it out of an unread search,
   * which it stays in while open, as in Gmail. Closing it reloads the
   * list, which only the server knows. */
  private kept = false

  readonly labels = computed(() => {
    const byId = new Map<LabelId, Label>()
    for (const account of this.accounts.value ?? []) {
      for (const label of account.labels) byId.set(label.id, label)
    }
    return byId
  })

  /** The conversations actions apply to: the selection, or else the
   * focused one. */
  readonly targets = computed((): Conversation[] => {
    const conversations = this.list.value?.conversations ?? []
    const selected = this.selected.value
    if (selected.size > 0) return conversations.filter((c) => selected.has(conversationKey(c)))
    return conversations.filter((c) => conversationKey(c) === this.focus.value)
  })

  private accountsLoading = false
  private accountsStale = false
  private listLoad: Load | null = null
  private listStale = false
  /** Bumped when the view changes, so responses for an old one are dropped. */
  private generation = 0
  /** Likewise, for the conversation open. */
  private openGeneration = 0

  constructor(
    private readonly server: Requester,
    private readonly ops: Ops,
  ) {}

  // ── state from the server ───────────────────────────────────────────────

  /** Everything is void; load afresh. */
  reset() {
    this.invalidateAccounts()
    this.invalidateList()
    this.loadOpen()
  }

  apply(entry: LogEntry) {
    for (const event of entry.events) {
      switch (event.type) {
        case 'entries_added':
          this.invalidateList()
          // It may be a reply in the conversation open.
          this.loadOpen()
          break
        case 'entries_removed': {
          const gone = new Set(event.entries)
          this.removeEntries(gone)
          break
        }
        case 'labels_changed':
          this.applyLabelChanges(event.changes)
          break
        case 'label_list_changed':
          break
        case 'image_sender_changed':
          this.imageSenders.value = new Map(this.imageSenders.value).set(event.sender, event.always)
          continue
        case 'rooms_changed':
        case 'timeline_added':
        case 'todos_changed':
        case 'calendar_changed':
          // Not mail, but a search lists them too.
          if (this.list.value?.view.kind === 'search') this.invalidateList()
          continue
        case 'draft_edited':
        case 'drafts_changed':
        case 'outbox_changed':
        case 'timeline_filled':
        case 'matrix_sends_changed':
          // Not mail; no counts change.
          continue
      }
      // Every mail event can change the labels' counts.
      this.invalidateAccounts()
    }
  }

  private removeEntries(gone: Set<EntryId>) {
    let stale = false
    this.updateList((conversations) =>
      conversations.flatMap((c) => {
        const earlier = c.earlier.filter((m) => !gone.has(m.id))
        if (!gone.has(c.latest.id)) return [{ ...c, earlier }]
        // Which is its latest now, only the server knows.
        if (earlier.length > 0) stale = true
        return []
      }),
    )
    if (stale) this.invalidateList()
    const open = this.open.value
    if (open?.entries) this.open.value = { ...open, entries: open.entries.filter((e) => !gone.has(e.id)) }
  }

  private applyLabelChanges(changes: { entry: EntryId; label: LabelId; desired: boolean }[]) {
    const byEntry = new Map<EntryId, typeof changes>()
    for (const change of changes) {
      byEntry.set(change.entry, [...(byEntry.get(change.entry) ?? []), change])
    }
    const relabel = <T extends { id: EntryId; labels: LabelId[] }>(item: T): T => {
      const own = byEntry.get(item.id)
      if (!own) return item
      const labels = new Set(item.labels)
      for (const change of own) {
        if (change.desired) labels.add(change.label)
        else labels.delete(change.label)
      }
      return { ...item, labels: [...labels] }
    }
    const open = this.open.value
    if (open?.entries) this.open.value = { ...open, entries: open.entries.map(relabel) }

    const list = this.list.value
    // Until the list is loaded, the response on its way reflects them.
    if (!list?.scope) return
    const { view, scope } = list
    const shown = view.kind === 'label' ? this.labelOf(view) : null
    // The labels that put a conversation on the list, if it is of labels.
    const listing: ((label: LabelId) => boolean) | null =
      view.kind === 'inbox'
        ? (label) => this.labels.value.get(label)!.external_id === 'INBOX'
        : shown
          ? (label) => label === shown.id
          : null
    const present = new Set(list.conversations.flatMap((c) => threadOf(c).entries.map((e) => e.id)))
    // Gaining a label of a scope of them, or losing one of a scope
    // without them, brings an entry into scope.
    const enters = (c: (typeof changes)[number]) =>
      scope.labels.includes(c.label) && c.desired === (scope.type === 'within')
    const joins = (c: (typeof changes)[number]) => (!!listing && listing(c.label) && c.desired) || enters(c)
    // What a search lists by, only the server knows.
    let stale = view.kind === 'search' || changes.some((c) => !present.has(c.entry) && joins(c))
    this.updateList((conversations) =>
      conversations.flatMap((c) => {
        if (![c.latest, ...c.earlier].some((e) => byEntry.has(e.id))) return [c]
        const latest = relabel(c.latest)
        const earlier = c.earlier.map(relabel).filter((m) => inScope(scope, m.labels))
        const latestIn = inScope(scope, latest.labels)
        if (!latestIn && earlier.length === 0) return []
        const entries = latestIn ? [...earlier, latest] : earlier
        if (listing && !entries.some((e) => e.labels.some(listing))) {
          if (!this.isOpen(c)) return []
          this.kept = true
        }
        // Which is its latest now, only the server knows.
        if (!latestIn) stale = true
        return [{ ...c, latest, earlier }]
      }),
    )
    // An entry may have joined the list on show; only the server knows
    // whether, and where.
    if (stale) this.invalidateList()
  }

  /** Replaces the list's conversations, keeping focus and selection on
   * those that remain; focus moves to the next one if its own left. */
  private updateList(update: (conversations: Conversation[]) => Conversation[]) {
    const list = this.list.value
    if (!list) return
    const conversations = update(list.conversations)
    batch(() => {
      this.list.value = { ...list, conversations }
      this.keepFocus(list.conversations, conversations)
    })
  }

  private keepFocus(before: Conversation[], after: Conversation[]) {
    const keys = new Set(after.map(conversationKey))
    const focus = this.focus.value
    if (focus === null) {
      this.focus.value = after[0] ? conversationKey(after[0]) : null
    } else if (!keys.has(focus)) {
      // The first conversation after the old focus that is still listed.
      const index = before.findIndex((c) => conversationKey(c) === focus)
      const next = before.slice(index + 1).find((c) => keys.has(conversationKey(c)))
      const last = after.at(-1)
      this.focus.value = next ? conversationKey(next) : last ? conversationKey(last) : null
    }
    // Every match is selected, as more are loaded too.
    if (this.matching.value) {
      this.selected.value = keys
      this.matching.value = keys.size > 0
      return
    }
    const selected = this.selected.value
    if ([...selected].some((key) => !keys.has(key))) {
      this.selected.value = new Set([...selected].filter((key) => keys.has(key)))
    }
  }

  // ── accounts ────────────────────────────────────────────────────────────

  private invalidateAccounts() {
    // A query in flight reflects every entry received before its response.
    if (this.accountsLoading) return
    this.accountsStale = true
    this.pumpAccounts()
  }

  private async pumpAccounts() {
    if (this.accountsLoading || !this.accountsStale) return
    this.accountsStale = false
    this.accountsLoading = true
    try {
      const response = await query(this.server, { type: 'list_accounts' }, 'accounts')
      this.accounts.value = response.accounts
    } catch (error) {
      this.error.value = (error as Error).message
    } finally {
      this.accountsLoading = false
    }
    this.pumpAccounts()
    // A list of a label may be waiting for the labels' ids.
    this.pumpList()
  }

  labelOf(view: { account: string; label: string }): Label | undefined {
    return this.accounts.value
      ?.find((a) => a.id === view.account)
      ?.labels.find((l) => l.external_id === view.label)
  }

  /** The label with `externalId` in `account`. */
  systemLabel(account: string, externalId: string): Label {
    const label = this.accounts.value
      ?.find((a) => a.id === account)
      ?.labels.find((l) => l.external_id === externalId)
    if (!label) throw new Error(`account ${account} has no label ${externalId}`)
    return label
  }

  // ── the list on show ────────────────────────────────────────────────────

  show(view: View) {
    if (this.list.value && sameView(this.list.value.view, view)) return
    this.generation++
    batch(() => {
      this.list.value = { view, loaded: false, conversations: [], next: null, scope: null, found: null }
      this.focus.value = null
      this.select(new Set())
    })
    this.listLoad = null
    this.listStale = true
    this.kept = false
    this.pumpList()
  }

  loadMore() {
    const list = this.list.value
    if (!list?.next || this.listLoad) return
    this.loadList('more')
  }

  private invalidateList() {
    if (!this.list.value || this.listLoad === 'reload') return
    this.listStale = true
    this.pumpList()
  }

  private pumpList() {
    if (this.listLoad || !this.listStale) return
    const list = this.list.value!
    // Until the accounts are in, a label's id is unknown.
    if (list.view.kind === 'label' && !this.accounts.value) return
    this.listStale = false
    this.loadList('reload')
  }

  private async loadList(load: Load) {
    const list = this.list.value!
    const generation = this.generation
    const request = this.listRequest(list, load)
    if (!request) {
      this.list.value = { ...list, loaded: true }
      return
    }
    this.listLoad = load
    let page:
      | { conversations: Conversation[]; next: ListPosition | null; scope: ListScope; found?: Found }
      | undefined
    try {
      if (request.type === 'list_message_entries') {
        const response = await query(this.server, request, 'entries')
        page = { conversations: response.entries.map(alone), next: null, scope: EVERY }
      } else if (request.type === 'list_items') {
        const { conversations, next, scope, rooms, messages, todos, events } = await query(this.server, request, 'items')
        // A later page is of mail alone.
        const found = request.after === null ? { rooms, messages, todos, events } : undefined
        page = { conversations, next, scope, found }
      } else {
        page = await query(this.server, request, 'conversations')
      }
    } catch (error) {
      this.error.value = (error as Error).message
    }
    if (generation !== this.generation) return
    this.listLoad = null
    if (page) {
      const current = this.list.value!
      const conversations =
        load === 'reload'
          ? this.keepOpen(current.conversations, page.conversations)
          : [...current.conversations, ...page.conversations]
      const { next, scope } = page
      const found = page.found ?? current.found
      batch(() => {
        this.list.value = { ...current, loaded: true, conversations, next, scope, found }
        this.keepFocus(current.conversations, conversations)
      })
    }
    this.pumpList()
  }

  private isOpen(conversation: Conversation): boolean {
    const open = this.open.value
    return open !== null && hasEntry(conversation, open.entry)
  }

  /** A reload's conversations, and the one open, where it would be by
   * its latest entry, if they leave it out. */
  private keepOpen(before: Conversation[], after: Conversation[]): Conversation[] {
    const open = before.find((c) => this.isOpen(c))
    if (!open || after.some((c) => conversationKey(c) === conversationKey(open))) return after
    this.kept = true
    const newer = (c: Conversation) =>
      c.latest.received_at > open.latest.received_at ||
      (c.latest.received_at === open.latest.received_at && c.latest.id > open.latest.id)
    const at = after.findIndex((c) => !newer(c))
    return at === -1 ? [...after, open] : [...after.slice(0, at), open, ...after.slice(at)]
  }

  /** `null` if the view's label does not exist. */
  private listRequest(list: List, load: Load): ClientRequest | null {
    // A reload covers what is already listed, so the user's place stays.
    const limit =
      load === 'reload' ? Math.min(Math.max(list.conversations.length, PAGE), MAX_PAGE) : PAGE
    const after = load === 'reload' ? null : list.next
    const { view } = list
    switch (view.kind) {
      // Of every kind, so names of every kind's.
      case 'search':
        return { type: 'list_items', query: view.query, after, limit }
      case 'message':
        return { type: 'list_message_entries', message: view.message }
      default: {
        const query = this.queryOf(view)
        return query === null ? null : { type: 'list_entries', query, after, limit }
      }
    }
  }

  /** The query a list of a label, an account or the Inboxes is; `null`
   * if the view's label does not exist. */
  private queryOf(view: View & { kind: 'search' | 'all' | 'inbox' | 'label' }): string | null {
    switch (view.kind) {
      case 'search':
        return view.query
      case 'all':
        return `account:${quoted(view.account)}`
      case 'inbox':
        return 'in:inbox'
      case 'label': {
        const label = this.labelOf(view)
        return label ? `account:${quoted(view.account)} label:${quoted(label.name)}` : null
      }
    }
  }

  /** The query of the list on show, which lists more than are loaded. */
  private listQuery(): string {
    const { view } = this.list.value!
    if (view.kind === 'message') throw new Error('a message by itself is of no query')
    const query = this.queryOf(view)
    if (query === null) throw new Error('the list lists nothing')
    return query
  }

  /** How many conversations the list's query lists, loaded or not;
   * `null` if it could not be counted. */
  async count(): Promise<number | null> {
    try {
      const response = await query(this.server, { type: 'count_conversations', query: this.listQuery() }, 'conversation_count')
      return Number(response.count)
    } catch (error) {
      this.error.value = (error as Error).message
      return null
    }
  }

  // ── the conversation open ───────────────────────────────────────────────

  /** Opens the entry's conversation, or with `null` closes it. */
  showConversation(entry: EntryId | null) {
    if ((this.open.value?.entry ?? null) === entry) return
    this.openGeneration++
    this.open.value = entry === null ? null : { entry, entries: null }
    if (this.kept) {
      this.kept = false
      this.invalidateList()
    }
    this.loadOpen()
  }

  private async loadOpen() {
    const open = this.open.value
    if (!open) return
    const generation = this.openGeneration
    let entries: Entry[] | undefined
    try {
      entries = (await query(this.server, { type: 'list_conversation', entry: open.entry }, 'entries')).entries
    } catch (error) {
      this.error.value = (error as Error).message
    }
    if (generation !== this.openGeneration || !entries) return
    this.open.value = { ...open, entries }
  }

  // ── focus and selection ─────────────────────────────────────────────────

  moveFocus(by: number) {
    const conversations = this.list.value?.conversations ?? []
    if (conversations.length === 0) return
    const index = conversations.findIndex((c) => conversationKey(c) === this.focus.value)
    this.focusOn(conversations[Math.min(Math.max(index + by, 0), conversations.length - 1)])
  }

  /** Focuses a conversation of the list, loading more as the end nears,
   * so that moving on, in the list or between conversations, finds more. */
  focusOn(conversation: Conversation) {
    const conversations = this.list.value!.conversations
    this.focus.value = conversationKey(conversation)
    if (conversations.indexOf(conversation) >= conversations.length - 10) this.loadMore()
  }

  toggleSelected(key: string) {
    const selected = new Set(this.selected.value)
    if (!selected.delete(key)) selected.add(key)
    this.select(selected)
  }

  /** Selects the loaded conversations. */
  selectAll() {
    this.select(new Set((this.list.value?.conversations ?? []).map(conversationKey)))
  }

  /** Selects every conversation the list's query lists, loaded or not. */
  selectMatching() {
    batch(() => {
      this.selectAll()
      this.matching.value = true
    })
  }

  selectNone() {
    this.select(new Set())
  }

  /** Selects the loaded conversations with an entry that has the system
   * label `externalId`, or with `having` false, those without. */
  selectHaving(externalId: string, having: boolean) {
    this.select(
      new Set(
        (this.list.value?.conversations ?? [])
          .filter((c) => this.having(threadOf(c), externalId) === having)
          .map(conversationKey),
      ),
    )
  }

  /** Selects the conversations, and so no longer every match. */
  private select(selected: ReadonlySet<string>) {
    batch(() => {
      this.selected.value = selected
      this.matching.value = false
    })
  }

  /** Whether an entry of the thread has the system label `externalId`. */
  having(thread: Thread, externalId: string): boolean {
    const label = this.systemLabel(thread.account, externalId).id
    return thread.entries.some((e) => e.labels.includes(label))
  }

  // ── ops ─────────────────────────────────────────────────────────────────
  // On conversations, as Gmail's are (SPEC §12.8): the server does them,
  // to the threads given, or, `'matching'`, to every one the list's query
  // lists. Which way a toggle goes is the caller's: what it shows.

  archive(targets: Targets) {
    return this.act(targets, { type: 'archive' })
  }

  /** Moves them to Trash, which hides them elsewhere. */
  trash(targets: Targets) {
    return this.act(targets, { type: 'trash' })
  }

  /** Reports them as spam, or, not spam, moves them back to the Inbox. */
  setSpam(targets: Targets, spam: boolean) {
    return this.act(targets, { type: 'spam', spam })
  }

  /** Whether every entry of the threads is spam, which "Not spam" takes
   * back. */
  spam(threads: Thread[]): boolean {
    return all(threads).every((e) => e.labels.includes(this.systemLabel(e.account, 'SPAM').id))
  }

  /** `quiet` leaves the notice alone, as when opening a conversation reads
   * it. */
  markRead(targets: Targets, read: boolean, quiet = false) {
    return this.act(targets, { type: 'read', read }, quiet)
  }

  /** Marks the thread's entries unread from `entry` on, as Gmail's "Mark
   * unread from here" does. */
  markUnreadFrom(thread: Thread, entry: EntryId) {
    const from = thread.entries.findIndex((e) => e.id === entry)
    return this.ops.dispatch({
      type: 'modify_labels',
      entries: thread.entries.slice(from).map((e) => e.id),
      add: [this.systemLabel(thread.account, 'UNREAD').id],
      remove: [],
    })
  }

  setStar(targets: Targets, star: boolean) {
    return this.act(targets, { type: 'star', star })
  }

  markImportant(targets: Targets, important: boolean) {
    return this.act(targets, { type: 'important', important })
  }

  /** Moves them from `from` to `to`, as Gmail's "Move to" does. */
  move(targets: Targets, from: Label, to: Label) {
    return this.act(targets, { type: 'move', from: from.id, to: to.id })
  }

  setLabel(targets: Targets, label: Label, add: boolean) {
    return this.act(targets, { type: 'label', label: label.id, add })
  }

  private act(targets: Targets, action: ConversationAction, quiet = false) {
    const conversations: ConversationSet =
      targets === 'matching'
        ? { type: 'matching', query: this.listQuery() }
        : { type: 'listed', conversations: targets.map((t) => t.entries.map((e) => e.id)) }
    return this.ops.dispatch({ type: 'act_on_conversations', conversations, action }, quiet)
  }

  // ── messages ────────────────────────────────────────────────────────────

  /** Whether the message's remote images are shown unasked. */
  alwaysShowsImages(message: MessageContent): boolean {
    const sender = message.from_addr?.toLowerCase()
    const changed = sender === undefined ? undefined : this.imageSenders.value.get(sender)
    return changed ?? message.always_show_images
  }

  /** Shows the remote images of `sender`'s mail unasked, or no longer. */
  async alwaysShowImages(sender: string, always: boolean) {
    await this.ops.dispatch({ type: 'always_show_images', sender, always })
  }

  /** `null` if it could not be loaded; `error` says why. */
  async loadMessage(message: MessageId): Promise<MessageContent | null> {
    try {
      const response = await query(this.server, { type: 'load_message', message }, 'message')
      return response.message
    } catch (error) {
      this.error.value = (error as Error).message
      return null
    }
  }
}

type AccountItem = Item & { account: string }

function all(threads: Thread[]): AccountItem[] {
  return threads.flatMap((t) => t.entries.map((e) => ({ ...e, account: t.account })))
}

