// Mail state: accounts with their labels, and the message list on show,
// mirrored from query responses and the log entries that follow them.
//
// The server sends every entry a response reflects before the response
// itself (SPEC §5.4), so a response simply replaces what it covers, and
// each later entry applies on top.

import { batch, computed, signal } from '@preact/signals'
import type {
  Account,
  ClientRequest,
  Entry,
  EntryId,
  Label,
  LabelId,
  ListPosition,
  LogEntry,
  MessageContent,
  MessageId,
} from '../generated/protocol'
import type { Requester } from '../connection'
import type { Ops } from './ops'
import { query } from './query'

export const PAGE = 100
/** The server's limit on a page. */
const MAX_PAGE = 500
/** Labels whose entries no other list shows, nor search (SPEC §8.1). */
const HIDING = new Set(['TRASH', 'SPAM'])

export type View =
  | { kind: 'label'; account: string; label: string }
  /** The account's mail outside Trash and Spam. */
  | { kind: 'all'; account: string }
  | { kind: 'search'; query: string }
  /** A message's entries, in every account. */
  | { kind: 'message'; message: MessageId }

export function sameView(a: View, b: View): boolean {
  switch (a.kind) {
    case 'label':
      return b.kind === 'label' && a.account === b.account && a.label === b.label
    case 'all':
      return b.kind === 'all' && a.account === b.account
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
  entries: Entry[]
  next: ListPosition | null
}

type Load = 'reload' | 'more'

export class MailStore {
  readonly accounts = signal<Account[] | null>(null)
  readonly list = signal<List | null>(null)
  /** The entry j/k move between and single-entry actions apply to. */
  readonly focus = signal<EntryId | null>(null)
  readonly selected = signal<ReadonlySet<EntryId>>(new Set())
  readonly error = signal<string | null>(null)

  readonly labels = computed(() => {
    const byId = new Map<LabelId, Label>()
    for (const account of this.accounts.value ?? []) {
      for (const label of account.labels) byId.set(label.id, label)
    }
    return byId
  })

  /** The entries actions apply to: the selection, or else the focused one. */
  readonly targets = computed((): Entry[] => {
    const entries = this.list.value?.entries ?? []
    const selected = this.selected.value
    if (selected.size > 0) return entries.filter((e) => selected.has(e.id))
    return entries.filter((e) => e.id === this.focus.value)
  })

  private accountsLoading = false
  private accountsStale = false
  private listLoad: Load | null = null
  private listStale = false
  /** Bumped when the view changes, so responses for an old one are dropped. */
  private generation = 0

  constructor(
    private readonly server: Requester,
    private readonly ops: Ops,
  ) {}

  // ── state from the server ───────────────────────────────────────────────

  /** Everything is void; load afresh. */
  reset() {
    this.invalidateAccounts()
    this.invalidateList()
  }

  apply(entry: LogEntry) {
    for (const event of entry.events) {
      switch (event.type) {
        case 'entries_added':
          this.invalidateList()
          break
        case 'entries_removed': {
          const gone = new Set(event.entries)
          this.updateList((entries) => entries.filter((e) => !gone.has(e.id)))
          break
        }
        case 'labels_changed':
          this.applyLabelChanges(event.changes)
          break
        case 'label_list_changed':
          break
        case 'draft_edited':
        case 'drafts_changed':
        case 'outbox_changed':
        case 'rooms_changed':
        case 'timeline_added':
        case 'matrix_outbox_changed':
        case 'todos_changed':
          // Not mail; no counts change.
          continue
      }
      // Every mail event can change the labels' counts.
      this.invalidateAccounts()
    }
  }

  private applyLabelChanges(changes: { entry: EntryId; label: LabelId; desired: boolean }[]) {
    const list = this.list.value
    if (!list) return
    const shown = list.view.kind === 'label' ? this.labelOf(list.view) : null
    const byEntry = new Map<EntryId, typeof changes>()
    for (const change of changes) {
      byEntry.set(change.entry, [...(byEntry.get(change.entry) ?? []), change])
    }
    // Trash and Spam hide entries from every other list, and from search.
    const hides = list.view.kind !== 'message' && !(shown && HIDING.has(shown.external_id))
    const hiding = (label: LabelId) => HIDING.has(this.labels.value.get(label)!.external_id)
    const present = new Set(list.entries.map((e) => e.id))
    const joins = (c: (typeof changes)[number]) =>
      (!!shown && c.label === shown.id && c.desired) || (hides && hiding(c.label) && !c.desired)
    if (changes.some((c) => !present.has(c.entry) && joins(c))) {
      // An entry may have joined the list on show; only the server knows
      // whether, and where.
      this.invalidateList()
    }
    this.updateList((entries) =>
      entries.flatMap((entry) => {
        const own = byEntry.get(entry.id)
        if (!own) return [entry]
        const labels = new Set(entry.labels)
        for (const change of own) {
          if (change.desired) labels.add(change.label)
          else labels.delete(change.label)
        }
        if (shown && !labels.has(shown.id)) return []
        if (hides && [...labels].some(hiding)) return []
        return [{ ...entry, labels: [...labels] }]
      }),
    )
  }

  /** Replaces the list's entries, keeping focus and selection on the
   * entries that remain; focus moves to the next one if its entry left. */
  private updateList(update: (entries: Entry[]) => Entry[]) {
    const list = this.list.value
    if (!list) return
    const entries = update(list.entries)
    batch(() => {
      this.list.value = { ...list, entries }
      this.keepFocus(list.entries, entries)
    })
  }

  private keepFocus(before: Entry[], after: Entry[]) {
    const ids = new Set(after.map((e) => e.id))
    const focus = this.focus.value
    if (focus === null) {
      this.focus.value = after[0]?.id ?? null
    } else if (!ids.has(focus)) {
      // The first entry after the old focus that is still listed.
      const index = before.findIndex((e) => e.id === focus)
      const next = before.slice(index + 1).find((e) => ids.has(e.id))
      this.focus.value = next?.id ?? after.at(-1)?.id ?? null
    }
    const selected = this.selected.value
    if ([...selected].some((id) => !ids.has(id))) {
      this.selected.value = new Set([...selected].filter((id) => ids.has(id)))
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
      this.list.value = { view, loaded: false, entries: [], next: null }
      this.focus.value = null
      this.selected.value = new Set()
    })
    this.listLoad = null
    this.listStale = true
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
    let response
    try {
      response = await query(this.server, request, 'entries')
    } catch (error) {
      this.error.value = (error as Error).message
    }
    if (generation !== this.generation) return
    this.listLoad = null
    if (response) {
      const current = this.list.value!
      const entries =
        load === 'reload' ? response.entries : [...current.entries, ...response.entries]
      batch(() => {
        this.list.value = { ...current, loaded: true, entries, next: response.next }
        this.keepFocus(current.entries, entries)
      })
    }
    this.pumpList()
  }

  /** `null` if the view's label does not exist. */
  private listRequest(list: List, load: Load): ClientRequest | null {
    // A reload covers what is already listed, so the user's place stays.
    const limit =
      load === 'reload' ? Math.min(Math.max(list.entries.length, PAGE), MAX_PAGE) : PAGE
    const after = load === 'reload' ? null : list.next
    if (list.view.kind === 'search') {
      return { type: 'search', query: list.view.query, limit }
    }
    if (list.view.kind === 'message') {
      return { type: 'list_message_entries', message: list.view.message }
    }
    if (list.view.kind === 'all') {
      return { type: 'list_entries', list: { type: 'all_mail', account: list.view.account }, after, limit }
    }
    const label = this.labelOf(list.view)
    return label ? { type: 'list_entries', list: { type: 'label', label: label.id }, after, limit } : null
  }

  // ── focus and selection ─────────────────────────────────────────────────

  moveFocus(by: number) {
    const entries = this.list.value?.entries ?? []
    if (entries.length === 0) return
    const index = entries.findIndex((e) => e.id === this.focus.value)
    const next = Math.min(Math.max(index + by, 0), entries.length - 1)
    this.focus.value = entries[next].id
    if (next >= entries.length - 10) this.loadMore()
  }

  toggleSelected(id: EntryId) {
    const selected = new Set(this.selected.value)
    if (!selected.delete(id)) selected.add(id)
    this.selected.value = selected
  }

  selectAll() {
    this.selected.value = new Set((this.list.value?.entries ?? []).map((e) => e.id))
  }

  selectNone() {
    this.selected.value = new Set()
  }

  /** Selects the loaded entries that have the system label `externalId`,
   * or with `having` false, those that lack it. */
  selectHaving(externalId: string, having: boolean) {
    this.selected.value = new Set(
      (this.list.value?.entries ?? [])
        .filter((e) => e.labels.includes(this.systemLabel(e.account, externalId).id) === having)
        .map((e) => e.id),
    )
  }

  // ── ops ─────────────────────────────────────────────────────────────────

  archive(entries: Entry[]) {
    return this.modify(entries, (account) => ({
      add: [],
      remove: [this.systemLabel(account, 'INBOX').id],
    }))
  }

  /** Moves the entries to Trash, which hides them elsewhere. */
  trash(entries: Entry[]) {
    return this.modify(entries, (account) => ({ add: [this.systemLabel(account, 'TRASH').id], remove: [] }))
  }

  /** Reports the entries as spam, or, if all are spam already, moves them
   * back to the Inbox, as Gmail's "Not spam" does. */
  toggleSpam(entries: Entry[]) {
    const spam = entries.every((e) => e.labels.includes(this.systemLabel(e.account, 'SPAM').id))
    return this.modify(entries, (account) => {
      const label = this.systemLabel(account, 'SPAM').id
      return spam ? { add: [this.systemLabel(account, 'INBOX').id], remove: [label] } : { add: [label], remove: [] }
    })
  }

  /** `quiet` leaves the notice alone, as when opening a message reads it. */
  markRead(entries: Entry[], read: boolean, quiet = false) {
    return this.modify(
      entries,
      (account) => {
        const unread = this.systemLabel(account, 'UNREAD').id
        return read ? { add: [], remove: [unread] } : { add: [unread], remove: [] }
      },
      quiet,
    )
  }

  /** Stars the entries, or unstars them if all are starred already. */
  toggleStar(entries: Entry[]) {
    const starred = entries.every((e) => e.labels.includes(this.systemLabel(e.account, 'STARRED').id))
    return this.modify(entries, (account) => {
      const star = this.systemLabel(account, 'STARRED').id
      return starred ? { add: [], remove: [star] } : { add: [star], remove: [] }
    })
  }

  markImportant(entries: Entry[], important: boolean) {
    return this.modify(entries, (account) => {
      const label = this.systemLabel(account, 'IMPORTANT').id
      return important ? { add: [label], remove: [] } : { add: [], remove: [label] }
    })
  }

  /** Moves the entries, which are of `to`'s account, from `from` to `to`,
   * as Gmail's "Move to" does. */
  move(entries: Entry[], from: Label, to: Label) {
    return this.modify(entries, () => ({ add: [to.id], remove: [from.id] }))
  }

  /** Adds `label` to the entries, or removes it if all have it already. */
  toggleLabel(entries: Entry[], label: Label) {
    const all = entries.every((e) => e.labels.includes(label.id))
    return this.modify(entries, () => (all ? { add: [], remove: [label.id] } : { add: [label.id], remove: [] }))
  }

  /** One op per account, since labels belong to one. */
  private async modify(
    entries: Entry[],
    labels: (account: string) => { add: LabelId[]; remove: LabelId[] },
    quiet = false,
  ) {
    const byAccount = Map.groupBy(entries, (e) => e.account)
    await Promise.all(
      [...byAccount].map(([account, own]) =>
        this.ops.dispatch(
          { type: 'modify_labels', entries: own.map((e) => e.id), ...labels(account) },
          quiet,
        ),
      ),
    )
  }

  // ── messages ────────────────────────────────────────────────────────────

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
