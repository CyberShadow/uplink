import { signal } from '@preact/signals'
import type { ComponentChildren } from 'preact'
import { useLocation } from 'preact-iso'
import { duration } from '../format'
import { keepFocus } from '../keymap'
import type { Account, Label, ListingProgress, MailProvider, Room } from '../generated/protocol'
import { useServices } from '../context'
import { outboxCount, outboxNeedsAttention } from '../stores/drafts'
import { allMailPath, inboxPath, mailPath } from './App'
import { roomPath } from './Room'
import styles from './Nav.module.css'
import { Icon, type IconName } from './Icon'
import { RoomAvatar } from './RoomAvatar'

/** The system labels worth a place in the nav, in Gmail's order; Microsoft
 * 365's well-known folders have the ids of the Gmail labels that mean the
 * same, and an Archive besides. */
const SYSTEM: Record<string, { name: string; icon: IconName }> = {
  INBOX: { name: 'Inbox', icon: 'inbox' },
  STARRED: { name: 'Starred', icon: 'star' },
  IMPORTANT: { name: 'Important', icon: 'chevrons-right' },
  SENT: { name: 'Sent', icon: 'send' },
  // uplink's own drafts are in Drafts, above the accounts.
  DRAFT: { name: 'drafts', icon: 'file' },
  ARCHIVE: { name: 'Archive', icon: 'archive' },
  SPAM: { name: 'Spam', icon: 'octagon-x' },
  TRASH: { name: 'Trash', icon: 'trash' },
}

const PROVIDER: Record<MailProvider, string> = { gmail: 'Gmail', m365: 'Outlook' }

/** The account's system labels worth listing, then its own; All Mail goes
 * between them. */
export function navLabels(account: Account): { system: Label[]; user: Label[] } {
  const system = Object.keys(SYSTEM).flatMap(
    (id) => account.labels.filter((l) => l.system && l.external_id === id),
  )
  const user = account.labels
    .filter((l) => !l.system)
    .sort((a, b) => a.name.localeCompare(b.name))
  return { system, user }
}

export function labelName(account: Account, label: Label): string {
  if (!label.system) return label.name
  if (label.external_id === 'DRAFT') return `${PROVIDER[account.provider]} drafts`
  return SYSTEM[label.external_id]?.name ?? label.name
}

/** Gmail's icons for its system labels; one for user labels, and one for
 * Microsoft 365's own folders. */
export function labelIcon(label: Label): IconName {
  if (label.system) return SYSTEM[label.external_id]!.icon
  return label.folder ? 'folder' : 'tag'
}

/** A view above the accounts. */
export interface View {
  href: string
  icon: IconName
  name: string
}

/** The views above the accounts: with more than one mail account, their
 * Inboxes as one first. */
export function views(accounts: readonly Account[]): View[] {
  return [
    ...(accounts.length > 1 ? [{ href: inboxPath(), icon: 'inbox' as const, name: 'All inboxes' }] : []),
    { href: '/drafts', icon: 'file', name: 'Drafts' },
    { href: '/outbox', icon: 'clock-arrow-up', name: 'Outbox' },
    { href: '/history', icon: 'undo-2', name: 'History' },
    { href: '/todos', icon: 'list-todo', name: 'To-dos' },
    { href: '/calendar', icon: 'calendar', name: 'Calendar' },
  ]
}

/** Whether `href`'s view, or something in it, is on show at `path`. */
export function shows(path: string, href: string): boolean {
  return path === href || path.startsWith(`${href}/`)
}

/** A first sync's progress as seen: messages stored, by when. */
export interface Sample {
  at: number
  stored: number
}

/** Over how long the pace is judged: long enough to even out batches and
 * Gmail's quota waits, short enough to follow a change of pace. */
const PACE_WINDOW_MS = 10 * 60_000

/** Samples seen, by account, while the page lasts. */
const samples = new Map<string, Sample[]>()

/** Records `sample`, keeping the window's, and the one just before it to
 * span the whole window. */
export function record(seen: Sample[], sample: Sample): Sample[] {
  const all = seen.at(-1)?.stored === sample.stored ? seen : [...seen, sample]
  const first = all.findIndex((s) => s.at >= sample.at - PACE_WINDOW_MS)
  return all.slice(Math.max(0, first - 1))
}

/** How long until `total` are stored, at the pace of `seen`; `null` until
 * there is a pace. */
export function remaining(seen: Sample[], total: number): number | null {
  const [first, last] = [seen[0], seen.at(-1)]
  if (!first || !last || last.stored <= first.stored) return null
  const pace = (last.stored - first.stored) / (last.at - first.at)
  return Math.max(0, total - last.stored) / pace
}

/** A mail account's first sync, and how far it has got if known. Its
 * total is as the sync began, so it shows 99% at most until done. */
function Syncing(props: { account: string; listing: ListingProgress | null }) {
  const { account, listing } = props
  if (!listing) return <span class={styles.syncing}> (syncing)</span>
  const seen = record(samples.get(account) ?? [], { at: Date.now(), stored: listing.stored })
  samples.set(account, seen)
  const percent = Math.min(99, Math.floor((listing.stored / Math.max(listing.total, 1)) * 100))
  const count = (n: number) => n.toLocaleString('en-US')
  const left = remaining(seen, listing.total)
  const eta = left === null ? '' : `; about ${duration(left)} left`
  return (
    <span class={styles.syncing} title={`${count(listing.stored)} of about ${count(listing.total)} messages${eta}`}>
      {' '}
      (syncing, {percent}%)
    </span>
  )
}

/** The tree's collapsed nodes, by key; kept in this browser. */
const COLLAPSED_KEY = 'uplink.nav.collapsed'
const collapsed = signal<ReadonlySet<string>>(new Set(JSON.parse(localStorage.getItem(COLLAPSED_KEY) ?? '[]')))

function toggleCollapsed(key: string) {
  const next = new Set(collapsed.value)
  if (!next.delete(key)) next.add(key)
  collapsed.value = next
  localStorage.setItem(COLLAPSED_KEY, JSON.stringify([...next]))
}

/** A node of the user's labels, nested as Gmail nests them: by `/`. A
 * parent need not be a label itself. */
interface LabelNode {
  path: string
  name: string
  label: Label | null
  children: LabelNode[]
}

export function labelTree(labels: Label[]): LabelNode[] {
  const roots: LabelNode[] = []
  const byPath = new Map<string, LabelNode>()
  for (const label of [...labels].sort((a, b) => a.name.localeCompare(b.name))) {
    const parts = label.name.split('/')
    let siblings = roots
    for (let i = 0; i < parts.length; i++) {
      const path = parts.slice(0, i + 1).join('/')
      let node = byPath.get(path)
      if (!node) {
        node = { path, name: parts[i], label: null, children: [] }
        byPath.set(path, node)
        siblings.push(node)
      }
      if (i === parts.length - 1) node.label = label
      siblings = node.children
    }
  }
  return roots
}

/** A room in the nav; a joined space with the rooms it lists. */
export interface RoomNode {
  room: Room
  children: RoomNode[]
}

/** The account's rooms as its joined spaces nest them, as Element shows
 * them: a space has its subspaces, in its order, then its rooms, by
 * activity; at the top are the spaces in no other, by name, then the
 * rooms in none. `rooms` is by activity. A room may be in several spaces;
 * a space is not nested in itself again, and spaces only in each other go
 * at the top. */
export function roomTree(rooms: readonly Room[]): RoomNode[] {
  const byId = new Map(rooms.map((room) => [room.room_id, room]))
  const isSpace = (room: Room) => room.space && room.membership === 'joined'
  const spaces = rooms.filter(isSpace).sort((a, b) => a.name.localeCompare(b.name))
  const listed = new Set(spaces.flatMap((space) => space.children))
  const reached = new Set<string>()
  const node = (room: Room, outer: ReadonlySet<string>): RoomNode => {
    reached.add(room.room_id)
    if (!isSpace(room)) return { room, children: [] }
    const path = new Set([...outer, room.room_id])
    const children = new Set(room.children)
    const subspaces = room.children
      .map((id) => byId.get(id))
      .filter((child) => child !== undefined && isSpace(child) && !path.has(child.room_id)) as Room[]
    const others = rooms.filter((child) => children.has(child.room_id) && !isSpace(child))
    return { room, children: [...subspaces, ...others].map((child) => node(child, path)) }
  }
  const top = spaces.filter((space) => !listed.has(space.room_id)).map((space) => node(space, new Set()))
  for (const space of spaces) if (!reached.has(space.room_id)) top.push(node(space, new Set()))
  const others = rooms.filter((room) => !isSpace(room) && !listed.has(room.room_id))
  return [...top, ...others.map((room) => ({ room, children: [] }))]
}

/** What is unread in the rooms of a space, each counted once. */
function unreadIn(node: RoomNode, seen = new Set<string>()): number {
  if (seen.has(node.room.room_id)) return 0
  seen.add(node.room.room_id)
  return node.room.unread + node.children.reduce((unread, child) => unread + unreadIn(child, seen), 0)
}

/** `open`: shown as a drawer, where the window is narrow. */
export function Nav(props: { open: boolean }) {
  const { mail, drafts, matrix, todos } = useServices()
  const { path } = useLocation()
  const link = (href: string, lead: ComponentChildren, name: string, count: number, depth = 0, alert = false) => {
    const current = shows(path, href)
    return (
      <a
        href={href}
        class={current ? styles.current : undefined}
        aria-current={current ? 'page' : undefined}
        style={{ '--depth': depth }}
      >
        {lead}
        <span class={styles.name}>{name}</span>
        {count > 0 && (
          <span
            class={alert ? `${styles.count} ${styles.alert}` : styles.count}
            title={alert ? 'Something needs your attention' : undefined}
          >
            {count}
          </span>
        )}
      </a>
    )
  }
  const item = (href: string, icon: IconName, name: string, count: number, alert = false) => (
    <li key={href}>{link(href, <Icon name={icon} />, name, count, 0, alert)}</li>
  )
  const labelItem = (account: Account, label: Label) => (
    <li key={label.id}>
      {link(
        mailPath(account.id, label.external_id),
        <Icon name={labelIcon(label)} />,
        labelName(account, label),
        label.unread,
      )}
    </li>
  )
  const labelNode = (account: Account, node: LabelNode, depth: number) => {
    const key = `${account.id}:${node.path}`
    const folded = collapsed.value.has(key)
    return (
      <li key={node.path} class={styles.node}>
        {node.children.length > 0 && (
          <button
            type="button"
            class={styles.fold}
            style={{ '--depth': depth }}
            aria-label={folded ? `Expand ${node.path}` : `Collapse ${node.path}`}
            aria-expanded={!folded}
            onMouseDown={keepFocus}
            onClick={() => toggleCollapsed(key)}
          >
            <Icon name={folded ? 'chevron-right' : 'chevron-down'} />
          </button>
        )}
        {node.label ? (
          link(
            mailPath(account.id, node.label.external_id),
            <Icon name={labelIcon(node.label)} />,
            node.name,
            node.label.unread,
            depth,
          )
        ) : (
          <span class={styles.parent} style={{ '--depth': depth }}>
            <Icon name={node.children.some((c) => c.label?.folder) ? 'folder' : 'tag'} />
            <span class={styles.name}>{node.name}</span>
          </span>
        )}
        {node.children.length > 0 && !folded && (
          <ul>{node.children.map((child) => labelNode(account, child, depth + 1))}</ul>
        )}
      </li>
    )
  }
  const roomNode = (node: RoomNode, depth: number) => {
    const { room } = node
    if (!(room.space && room.membership === 'joined')) {
      return (
        <li key={room.id}>
          {link(
            roomPath(room.id),
            <RoomAvatar room={room} />,
            room.membership === 'invited' ? `${room.name} (invited)` : room.name,
            room.unread,
            depth,
          )}
        </li>
      )
    }
    const key = `matrix:${room.account}:${room.room_id}`
    const folded = collapsed.value.has(key)
    const unread = folded ? unreadIn(node) : 0
    const row = (
      <>
        <RoomAvatar room={room} />
        <span class={styles.name}>{room.name}</span>
        {unread > 0 && <span class={styles.count}>{unread}</span>}
      </>
    )
    return (
      <li key={room.id} class={styles.node}>
        {node.children.length > 0 ? (
          // The whole row folds the space, which is not a link.
          <button
            type="button"
            class={`${styles.parent} ${styles.toggle}`}
            style={{ '--depth': depth }}
            aria-expanded={!folded}
            onMouseDown={keepFocus}
            onClick={() => toggleCollapsed(key)}
          >
            <span class={styles.fold} aria-hidden="true">
              <Icon name={folded ? 'chevron-right' : 'chevron-down'} />
            </span>
            {row}
          </button>
        ) : (
          <span class={styles.parent} style={{ '--depth': depth }}>
            {row}
          </span>
        )}
        {node.children.length > 0 && !folded && (
          <ul>{node.children.map((child) => roomNode(child, depth + 1))}</ul>
        )}
      </li>
    )
  }
  const outbox = drafts.outbox.value ?? []
  const counts: Record<string, [number, boolean]> = {
    [inboxPath()]: [mail.accounts.value?.reduce((unread, account) => unread + inboxUnread(account), 0) ?? 0, false],
    '/drafts': [drafts.drafts.value?.size ?? 0, false],
    '/outbox': [outboxCount(outbox), outboxNeedsAttention(outbox)],
    '/history': [0, false],
    '/todos': [todos.todos.value?.filter((t) => t.done_at === null).length ?? 0, false],
    '/calendar': [0, false],
  }
  return (
    <nav class={props.open ? `${styles.nav} ${styles.open}` : styles.nav}>
      <ul>
        {views(mail.accounts.value ?? []).map((view) => {
          const [count, alert] = counts[view.href]!
          return item(view.href, view.icon, view.name, count, alert)
        })}
      </ul>
      {mail.accounts.value?.map((account) => (
        <Section
          key={account.id}
          id={`mail:${account.id}`}
          title={account.address}
          tooltip={account.id}
          status={!account.synced && <Syncing account={account.id} listing={account.listing} />}
          unread={inboxUnread(account)}
        >
          {navLabels(account).system.map((label) => labelItem(account, label))}
          {item(allMailPath(account.id), 'mails', 'All Mail', 0)}
          {labelTree(navLabels(account).user).map((node) => labelNode(account, node, 0))}
        </Section>
      ))}
      {matrix.accounts.value?.map((account) => {
        const rooms = matrix.rooms.value?.filter((room) => room.account === account.id) ?? []
        return (
          <Section
            key={account.id}
            id={`matrix:${account.id}`}
            title={account.mxid}
            tooltip={account.id}
            label={`Rooms of ${account.mxid}`}
            status={!account.synced && <span class={styles.syncing}> (syncing)</span>}
            unread={rooms.reduce((unread, room) => unread + room.unread, 0)}
          >
            {roomTree(rooms).map((node) => roomNode(node, 0))}
          </Section>
        )
      })}
    </nav>
  )
}

function inboxUnread(account: Account): number {
  return account.labels.find((l) => l.system && l.external_id === 'INBOX')?.unread ?? 0
}

/** An account's subtree, which its header folds; folded, the header
 * counts what is unread in it. */
function Section(props: {
  id: string
  title: string
  tooltip: string
  label?: string
  status: ComponentChildren
  unread: number
  children: ComponentChildren
}) {
  const folded = collapsed.value.has(props.id)
  return (
    <section aria-label={props.label}>
      <h2 class={styles.account}>
        <button
          type="button"
          title={props.tooltip}
          aria-expanded={!folded}
          onMouseDown={keepFocus}
          onClick={() => toggleCollapsed(props.id)}
        >
          <Icon name={folded ? 'chevron-right' : 'chevron-down'} />
          <span class={styles.name}>{props.title}</span>
          {props.status}
          {folded && props.unread > 0 && <span class={styles.count}>{props.unread}</span>}
        </button>
      </h2>
      {!folded && <ul>{props.children}</ul>}
    </section>
  )
}
