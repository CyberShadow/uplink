// A row of an item that is not a mail list's, alike wherever items are
// listed (home's buckets, a search's other kinds, SPEC §12.13): its icon,
// its title, what it says (with its counts at the end), and when. The
// list makes the row (`ROW`, a grid of those) and what it does.

import type { ComponentChildren } from 'preact'
import type { CalendarEvent, Conversation, Room, Sender, Todo } from '../generated/protocol'
import { useServices } from '../context'
import { brief, date } from '../format'
import { gist } from '../matrix'
import { Badge } from './Badge'
import { Icon } from './Icon'
import { RoomAvatar } from './RoomAvatar'
import styles from './ItemRow.module.css'

/** The row's class: the cells' grid. */
export const ROW = styles.row

/** The row's cells. `detail`'s text is cut short at its end, before
 * `counts`; the title, the subject, are bold of what is unread. */
export function Cells(props: {
  icon: ComponentChildren
  title: ComponentChildren
  detail: ComponentChildren
  counts?: ComponentChildren
  when: ComponentChildren
  unread?: boolean
}) {
  return (
    <>
      {props.icon}
      <span class={props.unread ? `${styles.title} ${styles.bold}` : styles.title}>{props.title}</span>
      <span class={styles.detail}>
        <span class={styles.text}>{props.detail}</span>
        {props.counts}
      </span>
      {props.when}
    </>
  )
}

/** When it was, briefly. */
export function When(props: { at: number | null }) {
  return (
    <span class={styles.when}>
      {props.at === null ? '' : brief(props.at)}
    </span>
  )
}

/** The icon of a kind, at a row's start. */
export function KindIcon(props: { name: 'list-todo' | 'calendar' | 'mail' | 'messages-square' }) {
  return <Icon name={props.name} class={styles.kind} />
}

export function TodoCells(props: { todo: Todo }) {
  const { todo } = props
  const overdue = todo.due !== null && todo.due < date(new Date())
  return (
    <Cells
      icon={<KindIcon name="list-todo" />}
      title={todo.title}
      detail={todo.notes}
      when={<span class={overdue ? `${styles.when} ${styles.overdue}` : styles.when}>{todo.due ?? ''}</span>}
    />
  )
}

export function EventCells(props: { event: CalendarEvent }) {
  const { event } = props
  return (
    <Cells icon={<KindIcon name="calendar" />} title={event.subject} detail={event.location} when={<When at={event.start} />} />
  )
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
    <Cells
      icon={<KindIcon name="mail" />}
      unread={unread > 0}
      title={
        <>
          {senders(conversation.senders, own)}
          {members.length > 1 && <span class={styles.count}> {members.length}</span>}
        </>
      }
      detail={
        <>
          <span class={unread > 0 ? `${styles.subject} ${styles.bold}` : styles.subject}>
            {latest.subject ?? '(no subject)'}
          </span>{' '}
          — {latest.snippet}
        </>
      }
      counts={<UnreadOf unread={unread} one={members.length === 1} class={styles.badge} />}
      when={
        <time class={styles.when} dateTime={new Date(latest.received_at).toISOString()}>
          {brief(latest.received_at)}
        </time>
      }
    />
  )
}

/** Its name, what its latest message says (as mail's snippet), and at
 * the end its mentions, then how many are unread (SPEC §12.14). */
export function RoomCells(props: { room: Room }) {
  const { matrix } = useServices()
  const { room } = props
  const me = matrix.accounts.value!.find((a) => a.id === room.account)!.mxid
  return (
    <Cells
      icon={<RoomAvatar room={room} class={styles.kind} />}
      unread={room.unread > 0}
      title={room.name}
      detail={
        room.membership === 'invited' ? <span class={styles.subject}>Invited</span> : room.preview && gist(room.preview, me)
      }
      counts={
        <>
          <Badge count={room.highlighted} tone="alert" prefix="@" title="Mentioning you" />
          <UnreadOf unread={room.unread} one={false} class={styles.badge} />
        </>
      }
      when={<When at={room.last_event_ts} />}
    />
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
