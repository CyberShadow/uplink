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
  ServerEvent,
  ServerResponse,
} from '../generated/protocol'
import type { Client } from '../stores/client'
import { apply } from '../compose/splice'

const SYSTEM = ['INBOX', 'UNREAD', 'STARRED', 'SENT', 'DRAFT', 'TRASH', 'SPAM']

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
  private nextLabel = 1
  private nextEntry = 1

  readonly drafts = new Map<string, Draft>()
  readonly outbox: OutboxItem[] = []
  private nextOutbox = 1

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
        const all = this.sorted().filter((e) => e.labels.includes(body.label))
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
        const found = this.sorted().filter((e) =>
          `${e.subject} ${e.snippet}`.toLowerCase().includes(query),
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
        return { type: 'undone', tx_log_id: op.revert() }
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
      undoes: null,
      events,
    }
    this.log.push(entry)
    this.client.apply(entry)
    return entry.id
  }

  private head(): number {
    return this.log.at(-1)?.id ?? 0
  }

  private sorted(): Entry[] {
    return [...this.entries].sort((a, b) => b.received_at - a.received_at || b.id - a.id)
  }

  private listAccounts(): Account[] {
    return this.accounts.map((account) => {
      const unread = this.labelId(account.id, 'UNREAD')
      return {
        id: account.id,
        address: account.address,
        synced: true,
        labels: account.labels.map((label) => {
          const having = this.entries.filter((e) => e.labels.includes(label.id))
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
