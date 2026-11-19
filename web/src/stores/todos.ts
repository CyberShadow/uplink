// To-dos (SPEC §11), mirrored like drafts: a query, kept current by the
// log entries that follow it.

import { computed, signal } from '@preact/signals'
import type { ClientRequest, LogEntry, MessageId, RoomEventId, Todo, TodoEdit, TodoId } from '../generated/protocol'
import type { Requester } from '../connection'
import type { Ops } from './ops'
import { Loader, query } from './query'

export class TodoStore {
  /** Open ones first, by due date; `null` until loaded. */
  readonly todos = signal<readonly Todo[] | null>(null)
  readonly error = signal<string | null>(null)
  /** Flagged messages: those with an open to-do (§11.2). */
  readonly flagged = computed(() => {
    const flagged = new Map<MessageId, Todo>()
    for (const todo of this.todos.value ?? []) {
      if (todo.message !== null && todo.done_at === null) flagged.set(todo.message, todo)
    }
    return flagged
  })
  /** Flagged messages of rooms, likewise. */
  readonly flaggedEvents = computed(() => {
    const flagged = new Map<RoomEventId, Todo>()
    for (const todo of this.todos.value ?? []) {
      if (todo.room_event !== null && todo.done_at === null) flagged.set(todo.room_event.event, todo)
    }
    return flagged
  })

  private loader = new Loader(async () => {
    try {
      this.todos.value = (await query(this.server, { type: 'list_todos' }, 'todos')).todos
    } catch (error) {
      this.error.value = (error as Error).message
    }
  })

  constructor(
    private readonly server: Requester,
    private readonly ops: Ops,
  ) {}

  reset() {
    this.loader.invalidate()
  }

  apply(entry: LogEntry) {
    if (entry.events.some((event) => event.type === 'todos_changed')) this.loader.invalidate()
  }

  /** `of`: the message it is flagged from, of mail or of a room, if any. */
  create(title: string, of: { message: MessageId } | { room_event: RoomEventId } | null = null) {
    return this.dispatch({ type: 'create_todo', title, notes: '', due: null, message: null, room_event: null, ...of })
  }

  edit(todo: TodoId, edit: TodoEdit) {
    return this.dispatch({ type: 'edit_todo', todo, edit })
  }

  delete(todo: TodoId) {
    return this.dispatch({ type: 'delete_todo', todo })
  }

  /** Flags the message for follow-up, or, if it is flagged, marks its
   * to-do done. */
  toggleFlag(message: MessageId, title: string) {
    const todo = this.flagged.value.get(message)
    return todo ? this.edit(todo.id, { field: 'done', done: true }) : this.create(title, { message })
  }

  /** Flags a room's message, likewise. */
  toggleEventFlag(event: RoomEventId, title: string) {
    const todo = this.flaggedEvents.value.get(event)
    return todo ? this.edit(todo.id, { field: 'done', done: true }) : this.create(title, { room_event: event })
  }

  private dispatch(request: ClientRequest) {
    return this.ops.dispatch(request)
  }
}
