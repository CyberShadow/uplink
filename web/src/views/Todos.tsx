// To-dos (SPEC §11): add, rename, date, finish and delete them, and open
// the message one was flagged from.

import { useEffect, useRef, useState } from 'preact/hooks'
import { useLocation } from 'preact-iso'
import type { Todo } from '../generated/protocol'
import { useServices } from '../context'
import { useTitle } from '../title'
import { keepFocus, lines, useKeymap } from '../keymap'
import { rest } from '../rest'
import { List, rowId } from './List'
import { roomEventPath } from './Room'
import styles from './Todos.module.css'

export function messagePath(message: number): string {
  return `/message/${message}`
}

/** What a to-do flagged from a message opens: the message, of mail or of
 * a room; `null` of one that is its own. */
export function flaggedPath(todo: Todo): string | null {
  if (todo.message !== null) return messagePath(todo.message)
  if (todo.room_event !== null) return roomEventPath(todo.room_event.room, todo.room_event.event)
  return null
}

export function Todos() {
  useTitle('To-dos')
  const { todos } = useServices()
  const { route } = useLocation()
  const list = todos.todos.value
  const [focus, setFocus] = useState(0)
  const [editing, setEditing] = useState<number | null>(null)
  const adder = useRef<HTMLInputElement>(null)
  const focused = useRef<HTMLLIElement>(null)
  const listed = useRef<HTMLUListElement>(null)
  useEffect(() => {
    focused.current?.scrollIntoView({ block: 'nearest' })
  }, [focus])
  // Keep the focus within the list as it shrinks.
  const count = list?.length ?? 0
  useEffect(() => setFocus((f) => Math.max(0, Math.min(f, count - 1))), [count])

  const current = list?.[focus]
  useKeymap({
    ...lines(
      () => setFocus((f) => Math.min(f + 1, count - 1)),
      () => setFocus((f) => Math.max(f - 1, 0)),
    ),
    // A flagged message's to-do is about the message: opening it opens that.
    open: () => {
      if (!current) return
      const flagged = flaggedPath(current)
      if (flagged === null) setEditing(current.id)
      else route(flagged)
    },
    done: () => current && todos.edit(current.id, { field: 'done', done: current.done_at === null }),
    delete: () => current && todos.delete(current.id),
    add: () => adder.current?.focus(),
    rename: () => current && setEditing(current.id),
  })

  return (
    <section class={styles.todos} aria-label="To-dos">
      <form
        class={styles.add}
        onSubmit={async (event) => {
          event.preventDefault()
          const input = adder.current!
          if (input.value.trim() === '') return
          if (await todos.create(input.value.trim())) input.value = ''
        }}
      >
        <input
          ref={adder}
          type="text"
          aria-label="New to-do"
          placeholder="Add a to-do"
          onKeyDown={(event) => event.key === 'Escape' && rest()}
        />
      </form>
      {list === null ? (
        <p class={styles.placeholder}>Loading…</p>
      ) : list.length === 0 ? (
        <p class={styles.placeholder}>Nothing to do.</p>
      ) : (
        <List tag="ul" list={listed} class={styles.list} label="To-do list" cursor={rowId(focus)} multi={false}>
          {list.map((todo, i) => (
            <Item
              key={todo.id}
              id={rowId(i)}
              todo={todo}
              focused={i === focus}
              itemRef={i === focus ? focused : undefined}
              editing={editing === todo.id}
              edit={() => setEditing(todo.id)}
              stopEditing={() => setEditing(null)}
              onFocus={() => setFocus(i)}
            />
          ))}
        </List>
      )}
    </section>
  )
}

function Item(props: {
  todo: Todo
  id: string
  focused: boolean
  itemRef: preact.Ref<HTMLLIElement> | undefined
  editing: boolean
  edit: () => void
  stopEditing: () => void
  onFocus: () => void
}) {
  const { todo } = props
  const { todos } = useServices()
  const done = todo.done_at !== null
  const flagged = flaggedPath(todo)
  return (
    // A row of the list, which has the focus for it, and the keys (j/k,
    // Enter): the click is the pointer's way to them.
    // eslint-disable-next-line jsx-a11y/click-events-have-key-events
    <li
      ref={props.itemRef}
      id={props.id}
      role="option"
      class={[styles.item, props.focused && styles.focused, done && styles.done].filter(Boolean).join(' ')}
      aria-current={props.focused || undefined}
      aria-selected={props.focused}
      onClick={props.onFocus}
    >
      <input
        type="checkbox"
        aria-label="Done"
        checked={done}
        onMouseDown={keepFocus}
        onClick={() => todos.edit(todo.id, { field: 'done', done: !done })}
      />
      {props.editing ? (
        <TitleEditor todo={todo} stop={props.stopEditing} />
      ) : (
        <span class={styles.title} onDblClick={props.edit}>
          {todo.title}
        </span>
      )}
      {/* A cell either way, so the columns line up. */}
      <span>
        {flagged !== null && (
          <a class={styles.message} href={flagged}>
            message
          </a>
        )}
      </span>
      <input
        type="date"
        aria-label="Due"
        class={styles.due}
        value={todo.due ?? ''}
        onChange={(event) => {
          const due = event.currentTarget.value || null
          if (due !== todo.due) todos.edit(todo.id, { field: 'due', due })
        }}
      />
      <button type="button" aria-label="Delete" onMouseDown={keepFocus} onClick={() => todos.delete(todo.id)}>
        ×
      </button>
    </li>
  )
}

function TitleEditor(props: { todo: Todo; stop: () => void }) {
  const { todos } = useServices()
  const input = useRef<HTMLInputElement>(null)
  useEffect(() => {
    input.current!.focus()
    input.current!.select()
  }, [])
  const save = () => {
    const title = input.current!.value.trim()
    if (title !== '' && title !== props.todo.title) todos.edit(props.todo.id, { field: 'title', title })
    props.stop()
  }
  return (
    <input
      ref={input}
      type="text"
      aria-label="Title"
      class={styles.title}
      defaultValue={props.todo.title}
      onBlur={save}
      onKeyDown={(event) => {
        if (event.key === 'Enter') {
          event.preventDefault()
          rest()
        }
        if (event.key === 'Escape') {
          input.current!.value = props.todo.title
          rest()
        }
      }}
    />
  )
}
