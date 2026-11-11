// What a search found besides mail (SPEC §8.7): rooms, their messages,
// to-dos and events, above the mail it found.

import type { RoomEvent } from '../generated/protocol'
import { useServices } from '../context'
import type { Found as Items } from '../stores/mail'
import { brief, date } from '../format'
import { calendarPath } from './Calendar'
import { Icon } from './Icon'
import { roomPath } from './Room'
import { RoomAvatar } from './RoomAvatar'
import { messagePath } from './Todos'
import styles from './Found.module.css'

/** A message's text as last edited. */
function shownBody(event: RoomEvent): string {
  return event.edit ? ((JSON.parse(event.edit.content) as { body?: string }).body ?? '') : (event.body ?? '')
}

export function Found(props: { found: Items }) {
  const { matrix } = useServices()
  const { rooms, messages, todos, events } = props.found
  if (rooms.length + messages.length + todos.length + events.length === 0) return null
  const roomName = (id: number) => matrix.rooms.value?.find((r) => r.id === id)?.name ?? ''
  return (
    <div class={styles.found}>
      {rooms.length > 0 && (
        <section aria-label="Rooms found">
          <h3>Rooms</h3>
          <ul>
            {rooms.map((room) => (
              <li key={room.id}>
                <a href={roomPath(room.id)}>
                  <RoomAvatar room={room} class={styles.icon} />
                  <span class={styles.title}>{room.name}</span>
                  <span class={styles.detail}>{room.unread > 0 ? `${room.unread} unread` : ''}</span>
                  <span class={styles.when}>{room.last_event_ts === null ? '' : brief(room.last_event_ts)}</span>
                </a>
              </li>
            ))}
          </ul>
        </section>
      )}
      {messages.length > 0 && (
        <section aria-label="Messages found">
          <h3>Messages in rooms</h3>
          <ul>
            {messages.map(({ room, event }) => (
              <li key={event.id}>
                <a href={roomPath(room)}>
                  <Icon name="messages-square" class={styles.icon} />
                  <span class={styles.title}>
                    {event.sender_name ?? event.sender} in {roomName(room)}
                  </span>
                  <span class={styles.detail}>{shownBody(event)}</span>
                  <span class={styles.when}>{brief(event.ts)}</span>
                </a>
              </li>
            ))}
          </ul>
        </section>
      )}
      {todos.length > 0 && (
        <section aria-label="To-dos found">
          <h3>To-dos</h3>
          <ul>
            {todos.map((todo) => (
              <li key={todo.id}>
                <a href={todo.message === null ? '/todos' : messagePath(todo.message)}>
                  <Icon name="list-todo" class={styles.icon} />
                  <span class={styles.title}>{todo.title}</span>
                  <span class={styles.detail}>{todo.notes}</span>
                  <span class={styles.when}>{todo.due ?? ''}</span>
                </a>
              </li>
            ))}
          </ul>
        </section>
      )}
      {events.length > 0 && (
        <section aria-label="Events found">
          <h3>Events</h3>
          <ul>
            {events.map((event) => (
              <li key={event.id}>
                <a href={calendarPath('agenda', date(new Date(event.start)))}>
                  <Icon name="calendar" class={styles.icon} />
                  <span class={styles.title}>{event.subject}</span>
                  <span class={styles.detail}>{event.location}</span>
                  <span class={styles.when}>{brief(event.start)}</span>
                </a>
              </li>
            ))}
          </ul>
        </section>
      )}
      <h3>Mail</h3>
    </div>
  )
}
