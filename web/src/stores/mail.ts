// Mail state: accounts with their labels, the list on show, and the
// conversation open. Accounts and the conversation open are mirrored from
// query responses and the log entries that follow them: the server sends
// every entry a response reflects before the response itself (SPEC §5.4),
// so a response simply replaces what it covers, and each later entry
// applies on top. A list of a query is its subscription's window, which
// the server keeps current (SPEC §5.7).

import { batch, computed, signal } from '@preact/signals'
import type {
  Account,
  Conversation,
  ConversationAction,
  ConversationSet,
  Entry,
  EntryId,
  Item as ListedItem,
  Label,
  LabelId,
  ListScope,
  LogEntry,
  ImageRule,
  Listed,
  MessageContent,
  MessageId,
  RoomId,
} from '../generated/protocol'
import type { Requester, Subscriber } from '../connection'
import { before, compare, keyOf, rowOf, type Row } from '../items'
import { FAVOURITE } from '../roomTags'
import type { Ops } from './ops'
import { Loader, query } from './query'
import { LiveWindow, type Shown } from './window'

export const PAGE = 100

/** How long a list left is kept current, to show at once, where it was,
 * if gone back to. */
export const KEEP_FOR = 5 * 60_000
/** Lists left that are kept so at most, the latest left. */
export const KEEP_AT_MOST = 5

/** Where a list is scrolled: its row of `key` is `offset` pixels below
 * the top of what it scrolls in. */
export interface Scrolled {
  key: string
  offset: number
}

/** A list left, kept current. */
interface Kept {
  window: LiveWindow
  /** As it was left. */
  focus: string | null
  scrolled: Scrolled | null
  timer: ReturnType<typeof setTimeout>
}

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
  /** A search's items of other kinds than mail (SPEC §8.7), as listed,
   * among its conversations. */
  others: Other[]
  /** Whether its query matches more than are loaded. */
  more: boolean
  /** The entries its conversations are made of; known once loaded. */
  scope: ListScope | null
  /** The rooms a search looked through the messages of, of which not the
   * whole history is stored (SPEC §9.2): it may have missed some. */
  partialRooms: RoomId[]
}

export type Other = Exclude<ListedItem, { kind: 'mail' }>

/** The list's items, as listed: its conversations, and among them, by
 * when each was last active, its others. */
export function rowsOf(list: List): Row[] {
  const query = list.view.kind === 'search' ? list.view.query : ''
  const conversations = list.conversations.map((conversation): ListedItem => ({ kind: 'mail', conversation }))
  return [...conversations, ...list.others].sort(compare).map((item) => rowOf(item, query))
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
  return threadKey(conversation.latest)
}

/** The key of `entry`'s conversation, as `conversationKey`'s. */
export function threadKey(entry: Entry): string {
  return entry.thread_id === null ? `#${entry.id}` : `${entry.account}/${entry.thread_id}`
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

export class MailStore {
  readonly accounts = signal<Account[] | null>(null)
  readonly list = signal<List | null>(null)
  /** The list's items, as listed. */
  readonly rows = computed(() => (this.list.value ? rowsOf(this.list.value) : []))
  /** The row j/k move between; of a conversation, the one single actions
   * apply to. */
  readonly focus = signal<string | null>(null)
  readonly selected = signal<ReadonlySet<string>>(new Set())
  /** Whether actions apply to every conversation the list's query lists,
   * rather than the selection; then all those loaded are selected. */
  readonly matching = signal(false)
  readonly open = signal<Open | null>(null)
  readonly error = signal<string | null>(null)
  /** Whether the user has rules that show remote images unasked, as
   * changed since messages loaded, by `ruleKey`: newer than what those
   * say (`image_rules`). */
  readonly imageRuleChanges = signal<ReadonlyMap<string, boolean>>(new Map())

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
    return conversations.filter((c) => this.isTarget(conversationKey(c)))
  })

  /** A search's items of other kinds actions apply to, as `targets`:
   * those that, being selected with mail, limit the actions to what
   * applies to every kind selected. */
  readonly otherTargets = computed((): Other[] =>
    (this.list.value?.others ?? []).filter((item) => this.isTarget(keyOf(item))),
  )

  private isTarget(key: string): boolean {
    const selected = this.selected.value
    return selected.size > 0 ? selected.has(key) : key === this.focus.value
  }

  private accountsLoading = false
  private accountsStale = false
  /** The window of the list's query; `null` of a message's entries, and
   * until the labels are in, which its rows show. */
  private window: LiveWindow | null = null
  /** The windows of lists left lately, by query, the longest left first. */
  private kept = new Map<string, Kept>()
  /** The conversation open, as last listed, once its window no longer
   * has it: it stays listed until closed, as Gmail keeps one read in a
   * search of the unread. */
  private held: Listed | null = null
  /** Where the list on show was scrolled, as its view last said: to
   * scroll back to as it shows again, from a conversation, or as a list
   * kept. */
  scrolled: Scrolled | null = null
  /** Bumped when the view changes, so responses for an old one are dropped. */
  private generation = 0
  /** Likewise, for the conversation open. */
  private openGeneration = 0
  /** A message's entries, the list of a message's view. */
  private messageEntries = new Loader(() => this.loadMessageEntries())

  constructor(
    private readonly server: Requester & Subscriber,
    private readonly ops: Ops,
  ) {}

  // ── state from the server ───────────────────────────────────────────────

  /** Everything is void; load afresh. Windows are subscribed to anew by
   * the connection itself. */
  reset() {
    this.invalidateAccounts()
    this.invalidateMessageEntries()
    this.loadOpen()
  }

  apply(entry: LogEntry) {
    for (const event of entry.events) {
      switch (event.type) {
        case 'entries_added':
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
        case 'account_changed':
          // Nothing of its mail.
          this.invalidateAccounts()
          continue
        case 'image_rule_changed':
          this.imageRuleChanges.value = new Map(this.imageRuleChanges.value).set(ruleKey(event.rule), event.always)
          continue
        case 'rooms_changed':
        case 'timeline_added':
        case 'todos_changed':
        case 'calendar_changed':
        case 'seen_changed':
        case 'draft_edited':
        case 'drafts_changed':
        case 'outbox_changed':
        case 'timeline_filled':
        case 'matrix_sends_changed':
          // Not mail; no counts change.
          continue
      }
      // Every mail event can change the labels' counts, and a message's
      // entries.
      this.invalidateAccounts()
      this.invalidateMessageEntries()
    }
  }

  private removeEntries(gone: Set<EntryId>) {
    const open = this.open.value
    if (open?.entries) this.open.value = { ...open, entries: open.entries.filter((e) => !gone.has(e.id)) }
  }

  private applyLabelChanges(changes: { entry: EntryId; label: LabelId; desired: boolean }[]) {
    const byEntry = new Map<EntryId, typeof changes>()
    for (const change of changes) {
      byEntry.set(change.entry, [...(byEntry.get(change.entry) ?? []), change])
    }
    const relabel = (item: Entry): Entry => {
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
  }

  /** Keeps focus on the row it is on, or the next one still listed, and
   * the selection on the rows still listed, as the list changes from
   * `before`. */
  private keepFocus(before: List) {
    const [was, rows] = [rowsOf(before), this.rows.value]
    const shown = new Set(rows.map((row) => row.key))
    const focus = this.focus.value
    if (focus === null) {
      this.focus.value = rows[0]?.key ?? null
    } else if (!shown.has(focus)) {
      const index = was.findIndex((row) => row.key === focus)
      const next = was.slice(index + 1).find((row) => shown.has(row.key))
      this.focus.value = (next ?? rows.at(-1))?.key ?? null
    }
    // Every match is selected, as more are loaded too: of mail alone.
    if (this.matching.value) {
      const keys = new Set(this.list.value!.conversations.map(conversationKey))
      this.selected.value = keys
      this.matching.value = keys.size > 0
      return
    }
    const keys = new Set(this.rows.value.map((row) => row.key))
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
    // A list may be waiting for the labels.
    this.openWindow()
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
    if (this.window) this.keep(this.window)
    this.window = null
    this.held = null
    this.scrolled = null
    batch(() => {
      this.list.value = { view, loaded: false, conversations: [], others: [], more: false, scope: null, partialRooms: [] }
      this.focus.value = null
      this.select(new Set())
    })
    if (view.kind === 'message') this.messageEntries.invalidate()
    else this.openWindow()
  }

  loadMore() {
    // A message's list, or a label's gone, lists all it has.
    this.window?.extend()
  }

  /** Keeps the window of the list left current a while, unless the most
   * kept already are of lists left since. */
  private keep(window: LiveWindow) {
    window.listener = null
    const timer = setTimeout(() => this.forget(window.query), KEEP_FOR)
    this.kept.set(window.query, { window, focus: this.focus.value, scrolled: this.scrolled, timer })
    if (this.kept.size > KEEP_AT_MOST) this.forget(this.kept.keys().next().value!)
  }

  private forget(query: string) {
    const kept = this.kept.get(query)!
    clearTimeout(kept.timer)
    kept.window.close()
    this.kept.delete(query)
  }

  /** Subscribes to the list's query once the labels are in, which its
   * rows show (and a label's list names), unless subscribed already or
   * the list is a message's; or takes the window kept of it, to show at
   * once where it was. */
  private openWindow() {
    const list = this.list.value
    if (!list || list.view.kind === 'message' || this.window || !this.accounts.value) return
    const query = this.windowQuery(list.view)
    if (query === null) {
      this.list.value = { ...list, loaded: true }
      return
    }
    const kept = this.kept.get(query)
    if (kept) {
      clearTimeout(kept.timer)
      this.kept.delete(query)
      this.focus.value = kept.focus
      this.scrolled = kept.scrolled
    }
    const window =
      kept?.window ??
      new LiveWindow(this.server, query, 'invalid', PAGE, (error) => {
        // Kept, it is no longer: the list subscribes anew if gone back to.
        if (this.kept.get(query)?.window === window) this.forget(query)
        else this.error.value = error.message
      })
    this.window = window
    window.listener = (was) => this.takeWindow(was)
    // Kept, it is listed already.
    if (window.shown) this.showWindow()
  }

  /** What the window lists changed, from `was`. */
  private takeWindow(was: Shown | null) {
    const open = this.list.value!.conversations.find((c) => this.isOpen(c))
    const key = open && conversationKey(open)
    const items = this.window!.shown!.items
    // The conversation open stays, though it left.
    this.held = key !== undefined && !items.has(key) ? (was?.items.get(key) ?? this.held)! : null
    this.showWindow()
  }

  /** Lists the window's items, and the conversation held open. */
  private showWindow() {
    const { items, more, scope, partialRooms } = this.window!.shown!
    const listed = [...items.values(), ...(this.held ? [this.held] : [])].sort((a, b) =>
      before(a.position, b.position),
    )
    const current = this.list.value!
    batch(() => {
      this.list.value = {
        ...current,
        loaded: true,
        conversations: listed.flatMap(({ item }) => (item.kind === 'mail' ? [item.conversation] : [])),
        others: listed.flatMap(({ item }) => (item.kind === 'mail' ? [] : [item])),
        more,
        scope,
        partialRooms,
      }
      this.keepFocus(current)
    })
  }

  private invalidateMessageEntries() {
    if (this.list.value?.view.kind === 'message') this.messageEntries.invalidate()
  }

  private async loadMessageEntries() {
    const { view } = this.list.value!
    if (view.kind !== 'message') return
    const generation = this.generation
    let entries: Entry[]
    try {
      entries = (await query(this.server, { type: 'list_message_entries', message: view.message }, 'entries')).entries
    } catch (error) {
      this.error.value = (error as Error).message
      return
    }
    if (generation !== this.generation) return
    const current = this.list.value!
    batch(() => {
      this.list.value = { ...current, loaded: true, conversations: entries.map(alone), scope: EVERY }
      this.keepFocus(current)
    })
  }

  private isOpen(conversation: Conversation): boolean {
    const open = this.open.value
    return open !== null && hasEntry(conversation, open.entry)
  }

  /** The query of a list's window: a search's own; else of mail alone,
   * as a list of a label, an account or the Inboxes is; `null` if the
   * view's label does not exist. */
  private windowQuery(view: View & { kind: 'search' | 'all' | 'inbox' | 'label' }): string | null {
    if (view.kind === 'search') return view.query
    const query = this.queryOf(view)
    return query === null ? null : `is:mail ${query}`
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
    // The conversation that was open leaves, if its window no longer has it.
    if (this.held) {
      this.held = null
      this.showWindow()
    }
    this.loadOpen()
  }

  private async loadOpen() {
    const open = this.open.value
    if (!open) return
    const generation = this.openGeneration
    let entries: Entry[] | undefined
    try {
      entries = await this.conversation(open.entry)
    } catch (error) {
      this.error.value = (error as Error).message
    }
    if (generation !== this.openGeneration || !entries) return
    this.open.value = { ...open, entries }
  }

  /** The entries of the entry's conversation, oldest first. */
  async conversation(entry: EntryId): Promise<Entry[]> {
    return (await query(this.server, { type: 'list_conversation', entry }, 'entries')).entries
  }

  // ── focus and selection ─────────────────────────────────────────────────

  /** Moves the focus `by` rows. */
  moveFocus(by: number) {
    const rows = this.rows.value
    if (rows.length === 0) return
    const index = rows.findIndex((row) => row.key === this.focus.value)
    this.focusKey(rows[Math.min(Math.max(index + by, 0), rows.length - 1)].key)
  }

  /** Focuses a conversation of the list. */
  focusOn(conversation: Conversation) {
    this.focusKey(conversationKey(conversation))
  }

  /** Focuses the row, loading more as the end nears, so that moving on,
   * in the list or between conversations, finds more. */
  private focusKey(key: string) {
    const rows = this.rows.value
    this.focus.value = key
    if (rows.findIndex((row) => row.key === key) >= rows.length - 10) this.loadMore()
  }

  toggleSelected(key: string) {
    const selected = new Set(this.selected.value)
    if (!selected.delete(key)) selected.add(key)
    this.select(selected)
  }

  /** Selects the loaded rows. */
  selectAll() {
    this.select(new Set(this.rows.value.map((row) => row.key)))
  }

  /** Selects every conversation the list's query lists, loaded or not:
   * of a list with nothing else selected, since the server's ops on every
   * match are of mail. */
  selectMatching() {
    batch(() => {
      this.select(new Set((this.list.value?.conversations ?? []).map(conversationKey)))
      this.matching.value = true
    })
  }

  selectNone() {
    this.select(new Set())
  }

  /** Selects the loaded conversations with an entry that has the system
   * label `externalId`, and the rooms that are so (unread, or Favourite
   * as starred); or with `having` false, those not. To-dos and events
   * are neither read nor unread, starred nor not, so are of neither. */
  selectHaving(externalId: 'UNREAD' | 'STARRED', having: boolean) {
    const has = (item: ListedItem): boolean | null => {
      switch (item.kind) {
        case 'mail':
          return this.having(threadOf(item.conversation), externalId)
        case 'room':
          return externalId === 'UNREAD' ? item.room.unread > 0 : item.room.tags.includes(FAVOURITE)
        case 'todo':
        case 'event':
          return null
      }
    }
    const list = this.list.value
    const items: ListedItem[] = [
      ...(list?.conversations ?? []).map((conversation): ListedItem => ({ kind: 'mail', conversation })),
      ...(list?.others ?? []),
    ]
    this.select(new Set(items.filter((item) => has(item) === having).map(keyOf)))
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
    return this.countHaving(thread, externalId) > 0
  }

  /** How many of the thread's entries have the system label. */
  countHaving(thread: Thread, externalId: string): number {
    const label = this.systemLabel(thread.account, externalId).id
    return thread.entries.filter((e) => e.labels.includes(label)).length
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

  /** `shown`: by a conversation being opened, which reads it; quietly,
   * leaving the notice alone, and `z` passes over it (SPEC §12.15). */
  markRead(targets: Targets, read: boolean, shown = false) {
    return this.act(targets, { type: 'read', read }, shown)
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

  /** `shown`: done by a view being shown, quietly. */
  private act(targets: Targets, action: ConversationAction, shown = false) {
    const conversations: ConversationSet =
      targets === 'matching'
        ? { type: 'matching', query: this.listQuery() }
        : { type: 'listed', conversations: targets.map((t) => t.entries.map((e) => e.id)) }
    return this.ops.dispatch({ type: 'act_on_conversations', conversations, action, shown }, shown)
  }

  // ── messages ────────────────────────────────────────────────────────────

  /** The rules that would show some of the message's remote images
   * unasked, and whether the user has each: its sender's, of their
   * address and domain, and its images' hosts'. */
  imageRules(message: MessageContent): ImageRuleState[] {
    const sender = message.from_addr?.toLowerCase()
    const domain = sender?.split('@')[1]
    const rules: ImageRule[] = [
      ...(sender !== undefined ? [{ type: 'sender' as const, address: sender }] : []),
      ...(domain ? [{ type: 'domain' as const, domain }] : []),
      ...message.remote_images.map((h) => ({ type: 'host' as const, host: h.host })),
    ]
    const had = new Set(message.image_rules.map(ruleKey))
    return rules.map((rule) => ({
      rule,
      always: this.imageRuleChanges.value.get(ruleKey(rule)) ?? had.has(ruleKey(rule)),
    }))
  }

  /** The hosts whose images the message shows unasked: all of them, as
   * its sender's are; else those the user has rules for. */
  shownHosts(message: MessageContent): ReadonlySet<string> {
    const rules = this.imageRules(message).filter((r) => r.always)
    if (rules.some((r) => r.rule.type !== 'host')) return new Set(message.remote_images.map((h) => h.host))
    return new Set(rules.map((r) => (r.rule as { host: string }).host))
  }

  /** Has the rule show remote images unasked, or no longer. */
  async alwaysShowImages(rule: ImageRule, always: boolean) {
    await this.ops.dispatch({ type: 'always_show_images', rule, always })
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

/** A rule that would show some of a message's remote images unasked, and
 * whether the user has it. */
export interface ImageRuleState {
  rule: ImageRule
  always: boolean
}

/** What tells `rule` from others. */
export function ruleKey(rule: ImageRule): string {
  switch (rule.type) {
    case 'sender':
      return `sender:${rule.address}`
    case 'domain':
      return `domain:${rule.domain}`
    case 'host':
      return `host:${rule.host}`
  }
}

type AccountItem = Item & { account: string }

function all(threads: Thread[]): AccountItem[] {
  return threads.flatMap((t) => t.entries.map((e) => ({ ...e, account: t.account })))
}

