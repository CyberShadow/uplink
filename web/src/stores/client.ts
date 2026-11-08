// The tab's state: what each domain mirrors, fed by the connection.

import type { Requester } from '../connection'
import type { LogEntry } from '../generated/protocol'
import { DraftStore } from './drafts'
import { MailStore } from './mail'
import { Ops } from './ops'

export class Client {
  readonly ops: Ops
  readonly mail: MailStore
  readonly drafts: DraftStore

  constructor(server: Requester) {
    this.ops = new Ops(server)
    this.mail = new MailStore(server, this.ops)
    this.drafts = new DraftStore(server, this.ops)
  }

  /** Everything is void; load afresh. */
  reset() {
    this.mail.reset()
    this.drafts.reset()
  }

  apply(entry: LogEntry) {
    this.ops.record(entry)
    this.mail.apply(entry)
    this.drafts.apply(entry)
  }
}
