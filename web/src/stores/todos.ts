// To-dos (SPEC §11), mirrored like drafts: a query, kept current by the
// log entries that follow it.

import { computed, signal } from '@preact/signals'
import type { ClientRequest, LogEntry, MessageId, Todo, TodoEdit, TodoId } from '../generated/protocol'
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

  create(title: string, message: MessageId | null = null) {
    return this.dispatch({ type: 'create_todo', title, notes: '', due: null, message })
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
    return todo ? this.edit(todo.id, { field: 'done', done: true }) : this.create(title, message)
  }

  private dispatch(request: ClientRequest) {
    return this.ops.dispatch(request)
  }
}
