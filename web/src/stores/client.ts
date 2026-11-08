// The tab's state: what each domain mirrors, fed by the connection.

import type { Blobs } from '../blobs'
import type { Requester } from '../connection'
import type { LogEntry } from '../generated/protocol'
import { DraftStore } from './drafts'
import { HistoryStore } from './history'
import { MailStore } from './mail'
import { MatrixStore } from './matrix'
import { Ops } from './ops'
import { TodoStore } from './todos'

export class Client {
  readonly ops: Ops
  readonly mail: MailStore
  readonly drafts: DraftStore
  readonly matrix: MatrixStore
  readonly todos: TodoStore
  readonly history: HistoryStore

  constructor(server: Requester & Blobs) {
    this.ops = new Ops(server)
    this.mail = new MailStore(server, this.ops)
    this.drafts = new DraftStore(server, this.ops)
    this.matrix = new MatrixStore(server, this.ops)
    this.todos = new TodoStore(server, this.ops)
    this.history = new HistoryStore(server)
  }

  /** Everything is void; load afresh. */
  reset() {
    this.mail.reset()
    this.drafts.reset()
    this.matrix.reset()
    this.todos.reset()
    this.history.reset()
  }

  apply(entry: LogEntry) {
    this.ops.record(entry)
    this.mail.apply(entry)
    this.drafts.apply(entry)
    this.matrix.apply(entry)
    this.todos.apply(entry)
    this.history.apply(entry)
  }
}
