import { signal } from '@preact/signals'
import { useEffect, useRef, useState } from 'preact/hooks'
import type { ComponentChildren, RefObject } from 'preact'
import { useLocation } from 'preact-iso'
import { duration } from '../format'
import { keepFocus, returnFocus, useKeymap } from '../keymap'
import type { Account, Label, ListingProgress, MailProvider, MatrixAccount, Room, RoomId } from '../generated/protocol'
import { bySpaceOrder, moveSpace } from '../spaceOrder'
import { useServices } from '../context'
import { outboxCount, outboxNeedsAttention } from '../stores/drafts'
import { allMailPath, inboxPath, mailPath } from './App'
import { roomPath } from './Room'
import { NotificationsItem } from './Notifications'
import { awaitsUser, securityPath } from './Security'
import styles from './Nav.module.css'
import { Icon, type IconName } from './Icon'
import { RoomAvatar } from './RoomAvatar'
import { RoomStarter } from './RoomStarter'

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
    { href: '/', icon: 'house', name: 'Home' },
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

function setCollapsed(next: ReadonlySet<string>) {
  collapsed.value = next
  localStorage.setItem(COLLAPSED_KEY, JSON.stringify([...next]))
}

function toggleCollapsed(key: string) {
  const next = new Set(collapsed.value)
  if (!next.delete(key)) next.add(key)
  setCollapsed(next)
}

function unfold(keys: readonly string[]) {
  const next = new Set(collapsed.value)
  for (const key of keys) next.delete(key)
  setCollapsed(next)
}

/** The folds' keys: of an account's section, of a label with labels
 * nested in it, of a joined space. */
const mailSection = (account: string) => `mail:${account}`
const matrixSection = (account: string) => `matrix:${account}`
const labelFold = (account: string, node: LabelNode) => `${account}:${node.path}`
const spaceFold = (room: Room) => `matrix:${room.account}:${room.room_id}`

/** Whether the nav links to the room: a joined space it only nests. */
export const linksTo = (room: Room) => !(room.space && room.membership === 'joined')

/** The folds over each place where the nav links to what `path` shows,
 * outermost first; none over a link at the top. */
export function foldsOver(
  path: string,
  mailAccounts: readonly Account[],
  matrixAccounts: readonly MatrixAccount[],
  rooms: readonly Room[],
): string[][] {
  const places: string[][] = views(mailAccounts)
    .filter((view) => shows(path, view.href))
    .map(() => [])
  for (const account of mailAccounts) {
    const section = mailSection(account.id)
    const { system, user } = navLabels(account)
    const hrefs = [...system.map((label) => mailPath(account.id, label.external_id)), allMailPath(account.id)]
    if (hrefs.some((href) => shows(path, href))) places.push([section])
    const walk = (node: LabelNode, folds: string[]) => {
      if (node.label && shows(path, mailPath(account.id, node.label.external_id))) places.push(folds)
      for (const child of node.children) walk(child, [...folds, labelFold(account.id, node)])
    }
    for (const node of labelTree(user)) walk(node, [section])
  }
  for (const account of matrixAccounts) {
    const section = matrixSection(account.id)
    if (shows(path, securityPath(account.id))) places.push([section])
    const walk = (node: RoomNode, folds: string[]) => {
      if (linksTo(node.room) && shows(path, roomPath(node.room.id))) places.push(folds)
      for (const child of node.children) walk(child, [...folds, spaceFold(node.room)])
    }
    for (const node of roomTree(rooms.filter((room) => room.account === account.id))) walk(node, [section])
  }
  return places
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

const isSpace = (room: Room) => room.space && room.membership === 'joined'

/** The account's rooms as its joined spaces nest them, as Element shows
 * them: a space has its subspaces, in its order, then its rooms, by
 * activity; at the top are the spaces in no other, in the user's order
 * (MSC3230), then the rooms in none. `rooms` is by activity. A room may be
 * in several spaces; a space is not nested in itself again, and spaces
 * only in each other go at the top. */
export function roomTree(rooms: readonly Room[]): RoomNode[] {
  const byId = new Map(rooms.map((room) => [room.room_id, room]))
  const spaces = rooms.filter(isSpace).sort(bySpaceOrder)
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
  top.sort((a, b) => bySpaceOrder(a.room, b.room))
  const others = rooms.filter((room) => !isSpace(room) && !listed.has(room.room_id))
  return [...top, ...others.map((room) => ({ room, children: [] }))]
}

/** The rooms of a space, each once. */
function roomsIn(node: RoomNode, seen = new Set<string>()): Room[] {
  if (seen.has(node.room.room_id)) return []
  seen.add(node.room.room_id)
  return [node.room, ...node.children.flatMap((child) => roomsIn(child, seen))]
}

/** A count's look: as any; red, as what needs the user's attention; or
 * as a mention of them. */
type Tone = 'plain' | 'alert' | 'mention'

/** Rooms count, as Element's, their unread messages that notify, as
 * mentions if any mentions them; other unread make the name bold. */
function roomBadge(rooms: readonly Room[]): { count: number; tone: Tone; bold: boolean } {
  return {
    count: rooms.reduce((n, room) => n + room.notifying, 0),
    tone: rooms.some((room) => room.highlighted > 0) ? 'mention' : 'plain',
    bold: rooms.some((room) => room.unread > 0),
  }
}

/** `open`: shown as a drawer, where the window is narrow. */
export function Nav(props: { open: boolean }) {
  const { mail, drafts, matrix, todos } = useServices()
  const { path, route } = useLocation()
  const nav = useRef<HTMLElement>(null)
  // The Matrix account a conversation is being started with.
  const [starting, setStarting] = useState<string | null>(null)
  // The top-level space being dragged, and where it would go.
  const [drag, setDrag] = useState<Drag | null>(null)
  useReveal(path, props.open, nav)
  /** Goes to the view `by` links after the one on show, in the nav as
   * shown (folded, it hides what is in the fold), or else from its end;
   * if `unread`, to the next with unread. */
  const step = (by: 1 | -1, unread: boolean) => () => {
    const links = [...nav.current!.querySelectorAll<HTMLAnchorElement>('a[href]')]
    const at = links.findIndex((l) => l.getAttribute('aria-current') === 'page')
    const from = at === -1 ? (by === 1 ? -1 : links.length) : at
    for (let i = from + by; i >= 0 && i < links.length; i += by) {
      if (unread && links[i].dataset.unread === undefined) continue
      route(links[i].getAttribute('href')!)
      return
    }
  }
  useKeymap({
    navNext: step(1, false),
    navPrevious: step(-1, false),
    navNextUnread: step(1, true),
    navPreviousUnread: step(-1, true),
  })
  const link = (
    href: string,
    lead: ComponentChildren,
    name: string,
    count: number,
    depth = 0,
    tone: Tone = 'plain',
    bold = false,
  ) => {
    const current = shows(path, href)
    return (
      <a
        href={href}
        class={[current && styles.current, bold && styles.bold].filter(Boolean).join(' ') || undefined}
        aria-current={current ? 'page' : undefined}
        // What Alt+Shift+arrows move to (`step`).
        data-unread={count > 0 || bold || undefined}
        style={{ '--depth': depth }}
      >
        {lead}
        <span class={styles.name}>{name}</span>
        <Count count={count} tone={tone} />
      </a>
    )
  }
  const item = (href: string, icon: IconName, name: string, count: number, alert = false) => (
    <li key={href}>{link(href, <Icon name={icon} />, name, count, 0, alert ? 'alert' : 'plain')}</li>
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
    const key = labelFold(account.id, node)
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
  // A top-level space moves among `spaces`, its account's, by dragging
  // its row; `at` marks where the space dragged would go.
  const reorderable = (space: Room, spaces: readonly Room[]) => {
    const over = drag?.space === space.id ? null : drag?.over
    const at = over?.space === space.id ? (over.after ? styles.dropAfter : styles.dropBefore) : undefined
    // Of the drag that `onDragOver` let drop here.
    const dropped = (after: boolean) => {
      const moved = spaces.find((s) => s.id === drag!.space)!
      const before = after ? (spaces[spaces.indexOf(space) + 1] ?? null) : space
      const changes = before === moved ? [] : moveSpace(spaces, moved, before)
      if (changes.length > 0) void matrix.orderSpaces(changes)
    }
    const handlers = {
      draggable: true,
      onDragStart: (e: DragEvent) => {
        // Not the rooms in it, which are links.
        if (!row(e).contains(e.target as Node)) return
        e.dataTransfer!.effectAllowed = 'move'
        e.dataTransfer!.setData('text/plain', space.name)
        setDrag({ space: space.id, over: null })
      },
      onDragOver: (e: DragEvent) => {
        if (!spaces.some((s) => s.id === drag?.space)) return
        e.preventDefault()
        const after = droppedAfter(e)
        if (drag!.over?.space !== space.id || drag!.over.after !== after) {
          setDrag({ space: drag!.space, over: { space: space.id, after } })
        }
      },
      onDrop: (e: DragEvent) => {
        e.preventDefault()
        dropped(droppedAfter(e))
        setDrag(null)
      },
      onDragEnd: () => setDrag(null),
    }
    return { at, handlers }
  }
  /** `spaces`: given at the top, its account's spaces there, among which
   * a space there moves. */
  const roomNode = (node: RoomNode, depth: number, spaces?: readonly Room[]) => {
    const { room } = node
    if (linksTo(room)) {
      const badge = roomBadge([room])
      return (
        <li key={room.id}>
          {link(
            roomPath(room.id),
            <RoomAvatar room={room} />,
            room.membership === 'invited' ? `${room.name} (invited)` : room.name,
            badge.count,
            depth,
            badge.tone,
            badge.bold,
          )}
        </li>
      )
    }
    const key = spaceFold(room)
    const folded = collapsed.value.has(key)
    const badge = folded ? roomBadge(roomsIn(node)) : null
    const row = (
      <>
        <RoomAvatar room={room} />
        <span class={styles.name}>{room.name}</span>
        {badge && <Count count={badge.count} tone={badge.tone} />}
      </>
    )
    const { at, handlers } = spaces ? reorderable(room, spaces) : { at: undefined, handlers: {} }
    return (
      <li key={room.id} {...handlers} class={at ? `${styles.node} ${at}` : styles.node}>
        {node.children.length > 0 ? (
          // The whole row folds the space, which is not a link.
          <button
            type="button"
            class={`${styles.parent} ${styles.toggle}`}
            style={{ '--depth': depth }}
            aria-expanded={!folded}
            // Those at the top are dragged too.
            onMouseDown={spaces ? returnFocus : keepFocus}
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
  // Counting verifications that wait for the user, as another device's
  // request does.
  const securityItem = (account: string) => {
    const awaiting = matrix.security(account).state.value?.verifications.filter(awaitsUser).length ?? 0
    return item(securityPath(account), 'shield', 'Security', awaiting, awaiting > 0)
  }
  const outbox = drafts.outbox.value ?? []
  const counts: Record<string, [number, boolean]> = {
    [inboxPath()]: [mail.accounts.value?.reduce((unread, account) => unread + inboxUnread(account), 0) ?? 0, false],
    '/': [0, false],
    '/drafts': [drafts.drafts.value?.size ?? 0, false],
    '/outbox': [outboxCount(outbox), outboxNeedsAttention(outbox)],
    '/history': [0, false],
    '/todos': [todos.todos.value?.filter((t) => t.done_at === null).length ?? 0, false],
    '/calendar': [0, false],
  }
  return (
    <nav ref={nav} class={props.open ? `${styles.nav} ${styles.open}` : styles.nav}>
      <ul>
        {views(mail.accounts.value ?? []).map((view) => {
          const [count, alert] = counts[view.href]!
          return item(view.href, view.icon, view.name, count, alert)
        })}
      </ul>
      {mail.accounts.value?.map((account) => (
        <Section
          key={account.id}
          id={mailSection(account.id)}
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
        const tree = roomTree(rooms)
        const spaces = tree.map((node) => node.room).filter(isSpace)
        return (
          <Section
            key={account.id}
            id={matrixSection(account.id)}
            title={account.mxid}
            tooltip={account.id}
            label={`Rooms of ${account.mxid}`}
            status={!account.synced && <span class={styles.syncing}> (syncing)</span>}
            unread={roomBadge(rooms).count}
          >
            {securityItem(account.id)}
            <NotificationsItem />
            <li>
              <button type="button" class={styles.action} onClick={() => setStarting(account.id)}>
                <Icon name="plus" />
                <span class={styles.name}>Start a conversation</span>
              </button>
            </li>
            {tree.map((node) => roomNode(node, 0, spaces))}
          </Section>
        )
      })}
      {starting !== null && <RoomStarter account={starting} close={() => setStarting(null)} />}
    </nav>
  )
}

/** Brings what `path` shows into view in the nav, once there: unfolding
 * the folds over it, unless it is in view somewhere already, and
 * scrolling to it. Again when the drawer opens or closes; scrolling while
 * the nav is hidden does nothing. */
function useReveal(path: string, open: boolean, nav: RefObject<HTMLElement>) {
  const { mail, matrix } = useServices()
  // What is to be done, once the nav has it: until then, a room just
  // joined, say, is not in it.
  const revealing = useRef(false)
  const scrolling = useRef(false)
  useEffect(() => {
    revealing.current = true
  }, [path])
  useEffect(() => {
    scrolling.current = true
  }, [open])
  // After every render: unfolding renders again, with the link.
  useEffect(() => {
    if (revealing.current) {
      const places = foldsOver(path, mail.accounts.value ?? [], matrix.accounts.value ?? [], matrix.rooms.value ?? [])
      if (places.length > 0) {
        revealing.current = false
        scrolling.current = true
        if (places.every((folds) => folds.some((key) => collapsed.value.has(key)))) unfold(places[0])
      }
    }
    if (!scrolling.current) return
    const current = nav.current!.querySelector('[aria-current=page]')
    if (!current) return
    scrolling.current = false
    current.scrollIntoView({ block: 'nearest' })
  })
}

function Count(props: { count: number; tone: Tone }) {
  if (props.count === 0) return null
  const { tone } = props
  return (
    <span
      class={tone === 'plain' ? styles.count : `${styles.count} ${styles[tone]}`}
      title={
        tone === 'alert' ? 'Something needs your attention' : tone === 'mention' ? 'You are mentioned' : undefined
      }
    >
      {props.count}
    </span>
  )
}

/** A top-level space being dragged, and the one it is over: to go before
 * it, or after. */
interface Drag {
  space: RoomId
  over: { space: RoomId; after: boolean } | null
}

/** The row of the space whose item handles `e`. */
function row(e: DragEvent): Element {
  return (e.currentTarget as HTMLElement).firstElementChild!
}

/** Whether a drag over a space's item drops after it: past the middle of
 * its row (not of the rooms in it). */
function droppedAfter(e: DragEvent): boolean {
  const { top, height } = row(e).getBoundingClientRect()
  return e.clientY > top + height / 2
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
