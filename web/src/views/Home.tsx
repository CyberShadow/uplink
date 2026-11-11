// The home view (SPEC §12.13): everything at a glance. Today's and
// tomorrow's events above; below, the configured buckets, each listing
// what its query matches of every kind and earlier buckets do not: to-dos
// first, then mail and rooms, the most recently active first. The same
// keys act on any row: `e` is done with it, `s` stars it, `l` labels it.

import { useEffect, useRef, useState } from 'preact/hooks'
import { useLocation } from 'preact-iso'
import type { CalendarEvent, Conversation, HomeBucket, Room, Todo } from '../generated/protocol'
import { useServices } from '../context'
import { useKeymap } from '../keymap'
import { BUCKET_LIMIT } from '../stores/home'
import { conversationKey, threadOf } from '../stores/mail'
import { brief, date, time } from '../format'
import { searchPath } from './App'
import { calendarPath } from './Calendar'
import { Icon } from './Icon'
import { LabelPicker } from './LabelPicker'
import { senders } from './Mail'
import { roomPath } from './Room'
import { RoomAvatar } from './RoomAvatar'
import { RoomTagPicker } from './RoomTagPicker'
import { messagePath } from './Todos'
import { FAVOURITE } from '../roomTags'
import styles from './Home.module.css'

type Row =
  | { kind: 'todo'; key: string; todo: Todo }
  | { kind: 'event'; key: string; event: CalendarEvent }
  | { kind: 'mail'; key: string; conversation: Conversation; query: string }
  | { kind: 'room'; key: string; room: Room }

/** A bucket's rows: to-dos (as listed, by due date), events, then mail
 * and rooms, the most recently active first. */
export function rowsOf(bucket: HomeBucket): Row[] {
  const active = (row: Row) =>
    row.kind === 'mail' ? row.conversation.latest.received_at : row.kind === 'room' ? (row.room.last_event_ts ?? 0) : 0
  const mixed: Row[] = [
    ...bucket.conversations.map((conversation): Row => ({
      kind: 'mail',
      key: `mail:${conversationKey(conversation)}`,
      conversation,
      query: bucket.query,
    })),
    ...bucket.rooms.map((room): Row => ({ kind: 'room', key: `room:${room.id}`, room })),
  ].sort((a, b) => active(b) - active(a))
  return [
    ...bucket.todos.map((todo): Row => ({ kind: 'todo', key: `todo:${todo.id}`, todo })),
    ...bucket.events.map((event): Row => ({ kind: 'event', key: `event:${event.id}`, event })),
    ...mixed,
  ]
}

/** How many a bucket lists: `20+` if there may be more mail. */
function count(bucket: HomeBucket, rows: number): string {
  return `${rows}${bucket.more ? '+' : ''}`
}

export function Home() {
  const { home, mail, matrix, todos } = useServices()
  const { route } = useLocation()
  useEffect(() => home.show(), [home])
  const [focus, setFocus] = useState(0)
  const [picking, setPicking] = useState<Row | null>(null)
  const focused = useRef<HTMLLIElement>(null)
  useEffect(() => {
    focused.current?.scrollIntoView({ block: 'nearest' })
  }, [focus])

  const shown = home.home.value
  const buckets = (shown?.buckets ?? []).map((bucket) => {
    const rows = rowsOf(bucket)
    return { bucket, rows, visible: rows.slice(0, BUCKET_LIMIT) }
  })
  const flat = buckets.flatMap((b) => b.visible)
  // Keep the focus within the rows as they go.
  useEffect(() => setFocus((f) => Math.max(0, Math.min(f, flat.length - 1))), [flat.length])
  const current = flat[focus] as Row | undefined

  const open = (row: Row) => {
    switch (row.kind) {
      case 'todo':
        return route(row.todo.message === null ? '/todos' : messagePath(row.todo.message))
      case 'event':
        return route(calendarPath('agenda', date(new Date(row.event.start))))
      case 'mail':
        mail.focus.value = conversationKey(row.conversation)
        return route(searchPath(row.query, row.conversation.latest.id))
      case 'room':
        return route(roomPath(row.room.id))
    }
  }
  const done = (row: Row) => {
    switch (row.kind) {
      case 'todo':
        return todos.edit(row.todo.id, { field: 'done', done: true })
      case 'mail':
        return mail.archive([threadOf(row.conversation)])
      case 'room': {
        const { latest, unread } = row.room
        if (latest !== null && unread > 0) matrix.markRead(row.room.id, latest)
        return
      }
      case 'event':
        return
    }
  }
  const star = (row: Row) => {
    switch (row.kind) {
      case 'mail':
        return mail.toggleStar([threadOf(row.conversation)])
      case 'room':
        return matrix.toggleTag([row.room], FAVOURITE)
      case 'todo':
      case 'event':
        return
    }
  }
  useKeymap({
    next: () => setFocus((f) => Math.min(f + 1, flat.length - 1)),
    previous: () => setFocus((f) => Math.max(f - 1, 0)),
    open: () => current && open(current),
    done: () => current && done(current),
    star: () => current && star(current),
    label: () => current && (current.kind === 'mail' || current.kind === 'room') && setPicking(current),
  })

  if (!shown || !mail.accounts.value) return <p class={styles.placeholder}>Loading…</p>
  const id = (bucket: HomeBucket) => `bucket-${buckets.findIndex((b) => b.bucket === bucket)}`
  return (
    <section class={styles.home} aria-label="Home">
      <Agenda events={shown.agenda} />
      <nav class={styles.jump} aria-label="Buckets">
        {buckets.map(({ bucket, rows }) => (
          <button
            key={bucket.name}
            type="button"
            onClick={() => document.getElementById(id(bucket))?.scrollIntoView({ block: 'start' })}
          >
            {bucket.name} <span class={styles.count}>{count(bucket, rows.length)}</span>
          </button>
        ))}
      </nav>
      <div class={styles.buckets}>
        {buckets.map(({ bucket, rows, visible }) => (
          <section key={bucket.name} id={id(bucket)} class={styles.bucket} aria-label={bucket.name}>
            <h2>
              {bucket.name} <span class={styles.count}>{count(bucket, rows.length)}</span>
            </h2>
            {visible.length === 0 ? (
              <p class={styles.empty}>Nothing.</p>
            ) : (
              <ul class={styles.rows}>
                {visible.map((row) => {
                  const isFocused = row === current
                  return (
                    <li
                      key={row.key}
                      ref={isFocused ? focused : undefined}
                      class={[styles.row, isFocused && styles.focused, unread(row) && styles.unread].filter(Boolean).join(' ')}
                      aria-current={isFocused || undefined}
                      onClick={() => {
                        setFocus(flat.indexOf(row))
                        open(row)
                      }}
                    >
                      <RowView row={row} />
                    </li>
                  )
                })}
              </ul>
            )}
            {(rows.length > visible.length || bucket.more) && (
              <a class={styles.all} href={searchPath(bucket.query)}>
                Show all {count(bucket, rows.length)}
              </a>
            )}
          </section>
        ))}
      </div>
      {picking?.kind === 'mail' && (
        <LabelPicker targets={[threadOf(picking.conversation)]} close={() => setPicking(null)} />
      )}
      {picking?.kind === 'room' && <RoomTagPicker rooms={[picking.room]} close={() => setPicking(null)} />}
    </section>
  )

  function unread(row: Row): boolean {
    switch (row.kind) {
      case 'mail':
        return mail.having(threadOf(row.conversation), 'UNREAD')
      case 'room':
        return row.room.unread > 0
      case 'todo':
      case 'event':
        return false
    }
  }
}

function RowView(props: { row: Row }) {
  const { mail } = useServices()
  const { row } = props
  switch (row.kind) {
    case 'todo': {
      const { todo } = row
      const overdue = todo.due !== null && todo.due < date(new Date())
      return (
        <>
          <Icon name="list-todo" class={styles.kind} />
          <span class={styles.title}>{todo.title}</span>
          <span class={styles.detail}>{todo.notes}</span>
          <span class={[styles.when, overdue && styles.overdue].filter(Boolean).join(' ')}>{todo.due ?? ''}</span>
        </>
      )
    }
    case 'event': {
      const { event } = row
      return (
        <>
          <Icon name="calendar" class={styles.kind} />
          <span class={styles.title}>{event.subject}</span>
          <span class={styles.detail}>{event.location}</span>
          <span class={styles.when}>{brief(event.start)}</span>
        </>
      )
    }
    case 'mail': {
      const { conversation } = row
      const { latest } = conversation
      const own = mail.accounts.value?.find((a) => a.id === latest.account)?.address ?? ''
      return (
        <>
          <Icon name="mail" class={styles.kind} />
          <span class={styles.title}>{senders(conversation.senders, own)}</span>
          <span class={styles.detail}>
            <span class={styles.subject}>{latest.subject ?? '(no subject)'}</span> — {latest.snippet}
          </span>
          <time class={styles.when} dateTime={new Date(latest.received_at).toISOString()}>
            {brief(latest.received_at)}
          </time>
        </>
      )
    }
    case 'room': {
      const { room } = row
      const invited = room.membership === 'invited'
      return (
        <>
          <RoomAvatar room={room} class={styles.kind} />
          <span class={styles.title}>{room.name}</span>
          <span class={styles.detail}>
            {invited
              ? 'Invited'
              : room.highlighted > 0
                ? `${room.highlighted} mentioning you`
                : room.unread > 0
                  ? `${room.unread} unread`
                  : ''}
          </span>
          <span class={styles.when}>{room.last_event_ts === null ? '' : brief(room.last_event_ts)}</span>
        </>
      )
    }
  }
}

/** The events of now through tomorrow, by start. */
function Agenda(props: { events: CalendarEvent[] }) {
  const today = date(new Date())
  if (props.events.length === 0) return <p class={styles.agenda}>Nothing on today or tomorrow.</p>
  return (
    <ol class={styles.agenda} aria-label="Agenda">
      {props.events.map((event) => {
        const day = date(new Date(event.start))
        const when = event.all_day ? 'All day' : time(event.start)
        return (
          <li key={event.id}>
            <a href={calendarPath('agenda', day)}>
              <span class={styles.day}>{day <= today ? 'Today' : 'Tomorrow'}</span> {when}{' '}
              <span class={styles.subject}>{event.subject}</span>
            </a>
          </li>
        )
      })}
    </ol>
  )
}
