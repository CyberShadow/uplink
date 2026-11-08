// An in-memory stand-in for uplink's mail queries and label ops, for the
// store and view tests. Like the real server, it delivers an op's log
// entry before the op's response (SPEC §5.4).

import type { Requester } from '../connection'
import { RequestError } from '../connection'
import type {
  Account,
  ClientRequest,
  Entry,
  EntryId,
  Label,
  LabelId,
  LabelState,
  LogEntry,
  ServerEvent,
  ServerResponse,
} from '../generated/protocol'
import type { MailStore } from '../stores/mail'

const SYSTEM = ['INBOX', 'UNREAD', 'STARRED', 'SENT', 'DRAFT', 'TRASH', 'SPAM']

interface Undoable {
  changes: LabelState[]
  undone: boolean
}

export class FakeServer implements Requester {
  readonly requests: ClientRequest[] = []
  private store!: MailStore
  private accounts: { id: string; address: string; labels: Label[] }[] = []
  private entries: Entry[] = []
  private log: LogEntry[] = []
  private undoable = new Map<number, Undoable>()
  private nextLabel = 1
  private nextEntry = 1

  attach(store: MailStore) {
    this.store = store
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

  labelsOf(entry: EntryId): LabelId[] {
    return this.entries.find((e) => e.id === entry)!.labels
  }

  async request(body: ClientRequest): Promise<ServerResponse> {
    this.requests.push(body)
    await Promise.resolve()
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
        this.undoable.set(id, { changes, undone: false })
        return { type: 'dispatched', tx_log_id: id }
      }
      case 'undo': {
        const op = this.undoable.get(body.tx_log_id)
        if (!op) throw new RequestError({ code: 'not_undoable', message: 'not undoable' })
        if (op.undone) throw new RequestError({ code: 'already_undone', message: 'already undone' })
        op.undone = true
        const changes = op.changes.map((c) => this.setLabel(c.entry, c.label, !c.desired)!)
        const id = this.commit({ type: 'user' }, 'Undid it', [{ type: 'labels_changed', changes }])
        this.undoable.set(id, { changes, undone: false })
        return { type: 'undone', tx_log_id: id }
      }
    }
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
    this.store.apply(entry)
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
