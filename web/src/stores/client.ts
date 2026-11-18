// The tab's state: what each domain mirrors, fed by the connection.

import type { Blobs } from '../blobs'
import type { Requester, Subscriber } from '../connection'
import type { LogEntry, Notice, Place } from '../generated/protocol'
import { CalendarStore } from './calendar'
import { DraftStore } from './drafts'
import { HistoryStore } from './history'
import { HomeStore } from './home'
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
  readonly calendar: CalendarStore
  readonly home: HomeStore
  /** Says what the tab attends (SPEC §12.15). */
  readonly attend: (place: Place | null) => void

  constructor(server: Requester & Subscriber & Blobs) {
    this.attend = (place) => server.attend(place)
    this.ops = new Ops(server)
    this.mail = new MailStore(server, this.ops)
    this.drafts = new DraftStore(server, this.ops)
    this.matrix = new MatrixStore(server, this.ops)
    this.todos = new TodoStore(server, this.ops)
    this.history = new HistoryStore(server, this.ops)
    this.calendar = new CalendarStore(server)
    this.home = new HomeStore(server)
  }

  /** Everything is void; load afresh. */
  reset() {
    this.mail.reset()
    this.drafts.reset()
    this.matrix.reset()
    this.todos.reset()
    this.history.reset()
    this.calendar.reset()
    this.home.reset()
  }

  apply(entry: LogEntry) {
    this.ops.record(entry)
    this.mail.apply(entry)
    this.drafts.apply(entry)
    this.matrix.apply(entry)
    this.todos.apply(entry)
    this.history.apply(entry)
    this.calendar.apply(entry)
    this.home.apply(entry)
  }

  notice(notice: Notice) {
    switch (notice.type) {
      case 'home_changed':
        this.home.changed()
        break
      case 'calendar_configured':
        this.calendar.configured()
        break
      case 'typing':
      case 'receipts_changed':
      case 'security_changed':
        this.matrix.notice(notice)
        break
    }
  }
}
