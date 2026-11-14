// A row of an item, alike wherever items of every kind are listed (home's
// buckets, a search's, SPEC §12.13): its icon, its title, what it says
// (with its counts at the end), and when. The list makes the row (`ROW`,
// a grid of those, or a mail list's own) and what it does.

import type { ComponentChildren } from 'preact'
import type { CalendarEvent, Conversation, Room, RoomEvent, RoomHits, Sender, Todo } from '../generated/protocol'
import { useServices } from '../context'
import { brief, date } from '../format'
import type { Row } from '../items'
import { gist, shown } from '../matrix'
import { Badge } from './Badge'
import { calendarPath } from './Calendar'
import { Icon } from './Icon'
import { roomPath, roomSearchPath } from './Room'
import { RoomAvatar } from './RoomAvatar'
import { messagePath } from './Todos'
import styles from './ItemRow.module.css'

/** The row's class: the cells' grid. */
export const ROW = styles.row
/** Of when a to-do is due, past. */
export const OVERDUE = styles.overdue

export type OtherItemRow = Exclude<Row, { kind: 'mail' }>

/** What a row of an item shows. `detail`'s text is cut short at its end,
 * before `counts`; the title, the subject, are bold of what is unread. */
export interface Parts {
  icon: ComponentChildren
  title: ComponentChildren
  detail: ComponentChildren
  counts?: ComponentChildren
  /** When it was, briefly. */
  when: string
  /** A to-do's due date, past. */
  overdue?: boolean
  unread?: boolean
}

/** The row's cells, of `ROW`'s grid. */
export function Cells(props: Parts) {
  return (
    <>
      {props.icon}
      <span class={props.unread ? `${styles.title} ${styles.bold}` : styles.title}>{props.title}</span>
      <span class={styles.detail}>
        <span class={styles.text}>{props.detail}</span>
        {props.counts}
      </span>
      <span class={props.overdue ? `${styles.when} ${styles.overdue}` : styles.when}>{props.when}</span>
    </>
  )
}

/** The icon of a kind, at a row's start. */
export function KindIcon(props: { name: 'list-todo' | 'calendar' | 'mail' }) {
  return <Icon name={props.name} class={styles.kind} />
}

/** An item's row's cells. */
export function ItemCells(props: { row: Row }) {
  const { row } = props
  if (row.kind === 'mail') return <ConversationCells conversation={row.conversation} />
  return <OtherCells row={row} />
}

function OtherCells(props: { row: OtherItemRow }) {
  return <Cells {...useParts(props.row)} />
}

/** What a row of an item other than mail shows. */
export function useParts(row: OtherItemRow): Parts {
  const { matrix } = useServices()
  switch (row.kind) {
    case 'todo':
      return todoParts(row.todo)
    case 'event':
      return eventParts(row.event)
    case 'room':
      return roomParts(row.room, row.hits, matrix.accounts.value!.find((a) => a.id === row.room.account)!.mxid)
  }
}

/** Where an item other than mail opens: a room found by its messages,
 * with its search for them. */
export function pathOf(row: OtherItemRow): string {
  switch (row.kind) {
    case 'room':
      return row.hits === null ? roomPath(row.room.id) : roomSearchPath(row.room.id, row.query)
    case 'todo':
      return row.todo.message === null ? '/todos' : messagePath(row.todo.message)
    case 'event':
      return calendarPath('agenda', date(new Date(row.event.start)), row.event.id)
  }
}

function todoParts(todo: Todo): Parts {
  return {
    icon: <KindIcon name="list-todo" />,
    title: todo.title,
    detail: todo.notes,
    when: todo.due ?? '',
    overdue: todo.due !== null && todo.due < date(new Date()),
  }
}

function eventParts(event: CalendarEvent): Parts {
  return { icon: <KindIcon name="calendar" />, title: event.subject, detail: event.location, when: brief(event.start) }
}

/** Its name, what its latest message says (as mail's snippet), and at
 * the end its mentions, then how many are unread (SPEC §12.14); found by
 * its messages, how many, and what the latest says. */
function roomParts(room: Room, hits: RoomHits | null, me: string): Parts {
  const at = hits ? hits.latest.ts : room.last_event_ts
  return {
    icon: <RoomAvatar room={room} class={styles.kind} />,
    unread: room.unread > 0,
    title: (
      <>
        {room.name}
        {hits && <span class={styles.count}> {hits.count}</span>}
      </>
    ),
    detail: hits ? (
      said(hits.latest, me)
    ) : room.membership === 'invited' ? (
      <span class={styles.subject}>Invited</span>
    ) : (
      room.preview && gist(room.preview, me)
    ),
    counts: (
      <>
        <Badge count={room.highlighted} tone="alert" prefix="@" title="Mentioning you" />
        <UnreadOf unread={room.unread} one={false} class={styles.badge} />
      </>
    ),
    when: at === null ? '' : brief(at),
  }
}

/** What a message says, as a room's latest does. */
function said(event: RoomEvent, me: string): string {
  const { msgtype, body } = shown(event)
  return gist({ event: event.id, sender: event.sender, sender_name: event.sender_name, kind: event.kind, msgtype, body }, me)
}

/** Its senders and how many messages it has, its latest's subject and
 * snippet, and how many are unread at the end (SPEC §12.14). */
export function ConversationCells(props: { conversation: Conversation }) {
  const { mail } = useServices()
  const { conversation } = props
  const { latest } = conversation
  const own = mail.accounts.value?.find((a) => a.id === latest.account)?.address ?? ''
  const unreadLabel = mail.systemLabel(latest.account, 'UNREAD').id
  const members = [latest, ...conversation.earlier]
  const unread = members.filter((m) => m.labels.includes(unreadLabel)).length
  return (
    <>
      <KindIcon name="mail" />
      <span class={unread > 0 ? `${styles.title} ${styles.bold}` : styles.title}>
        {senders(conversation.senders, own)}
        {members.length > 1 && <span class={styles.count}> {members.length}</span>}
      </span>
      <span class={styles.detail}>
        <span class={styles.text}>
          <span class={unread > 0 ? `${styles.subject} ${styles.bold}` : styles.subject}>
            {latest.subject ?? '(no subject)'}
          </span>{' '}
          — {latest.snippet}
        </span>
        <UnreadOf unread={unread} one={members.length === 1} class={styles.badge} />
      </span>
      <time class={styles.when} dateTime={new Date(latest.received_at).toISOString()}>
        {brief(latest.received_at)}
      </time>
    </>
  )
}

/** Of a conversation (or a room), how many of its messages are unread,
 * at its row's end, apart from the count after its senders: unless it
 * has `one` message, whose being unread the row's bold says. */
export function UnreadOf(props: { unread: number; one: boolean; class: string }) {
  if (props.unread === 0 || props.one) return null
  return (
    <span class={props.class}>
      <Badge count={props.unread} tone="unread" title={props.unread === 1 ? '1 unread' : `${props.unread} unread`} />
    </span>
  )
}

/** A conversation's senders, as Gmail's rows name them: the one by name;
 * several by first names, `me` for the account's own address, the first
 * and the last two if there are many. */
export function senders(list: Sender[], own: string): string {
  const name = (s: Sender, brief: boolean) => {
    if (s.addr && s.addr.toLowerCase() === own.toLowerCase()) return 'me'
    const full = s.name ?? s.addr ?? '(unknown sender)'
    return brief && s.name ? s.name.split(/\s+/)[0] : full
  }
  if (list.length === 1) return name(list[0], false)
  const names = list.map((s) => name(s, true))
  return (names.length > 3 ? [names[0], '…', ...names.slice(-2)] : names).join(', ')
}
