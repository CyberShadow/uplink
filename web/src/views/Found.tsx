// What a search found besides mail (SPEC §8.7): rooms, their messages,
// to-dos and events, above the mail it found.

import type { RoomEvent } from '../generated/protocol'
import { useServices } from '../context'
import type { Found as Items } from '../stores/mail'
import { date } from '../format'
import { calendarPath } from './Calendar'
import { Cells, EventCells, KindIcon, ROW, RoomCells, TodoCells, When } from './ItemRow'
import { roomPath } from './Room'
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
                <a class={ROW} href={roomPath(room.id)}>
                  <RoomCells room={room} />
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
                <a class={ROW} href={roomPath(room)}>
                  <Cells
                    icon={<KindIcon name="messages-square" />}
                    title={`${event.sender_name ?? event.sender} in ${roomName(room)}`}
                    detail={shownBody(event)}
                    when={<When at={event.ts} />}
                  />
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
                <a class={ROW} href={todo.message === null ? '/todos' : messagePath(todo.message)}>
                  <TodoCells todo={todo} />
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
                <a class={ROW} href={calendarPath('agenda', date(new Date(event.start)), event.id)}>
                  <EventCells event={event} />
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
