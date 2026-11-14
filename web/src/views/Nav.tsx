import { signal } from '@preact/signals'
import { useEffect, useRef, useState } from 'preact/hooks'
import type { ComponentChildren, RefObject } from 'preact'
import { useLocation } from 'preact-iso'
import { date, dateTime, duration } from '../format'
import { historyProgress } from '../matrix'
import { keepFocus, returnFocus, useKeymap } from '../keymap'
import type { Account, Failing, Label, ListingProgress, MailProvider, Mark, MatrixAccount, Place, Room, RoomId } from '../generated/protocol'
import { isInbox, markOf, NONE, shows, strongest } from '../marks'
import { bySpaceOrder, moveSpace } from '../spaceOrder'
import { useServices } from '../context'
import { outboxCount, outboxNeedsAttention } from '../stores/drafts'
import { allMailPath, inboxPath, mailPath } from './App'
import { roomPath } from './Room'
import { NotificationsItem } from './Notifications'
import { awaitsUser, securityPath } from './Security'
import styles from './Nav.module.css'
import { Badge, type BadgeTone } from './Badge'
import { Icon, type IconName } from './Icon'
import { RoomAvatar } from './RoomAvatar'
import { RoomStarter } from './RoomStarter'

/** The system labels worth a place in the nav, in Gmail's order; Microsoft
 * 365's well-known folders, and IMAP servers' special-use ones, have the ids
 * of the Gmail labels that mean the same, and an Archive besides. */
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

const PROVIDER: Record<MailProvider, string> = {
  gmail: 'Gmail',
  m365: 'Outlook',
  imap: 'Server',
  pop3: 'Server',
  jmap: 'Server',
}

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

/** Gmail's icons for its system labels; one for user labels and
 * mailboxes, and one for folders. */
export function labelIcon(label: Label): IconName {
  if (label.system) return SYSTEM[label.external_id]!.icon
  return label.kind === 'folder' ? 'folder' : 'tag'
}

/** A view above the accounts, or below them. */
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
    { href: '/todos', icon: 'list-todo', name: 'To-dos' },
    { href: '/calendar', icon: 'calendar', name: 'Calendar' },
  ]
}

/** The views below the accounts: what was done in all of them. */
export const BELOW: readonly View[] = [{ href: '/history', icon: 'undo-2', name: 'History' }]

export { shows }

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

/** A mail account whose provider's watch is failing (SPEC §8.14): its
 * changes show when it is next polled, not as they happen. */
function Unwatched(props: { failing: Failing }) {
  const { failing } = props
  return (
    <span
      class={`${styles.syncing} ${styles.unwatched}`}
      title={`Not told of changes as they happen since ${dateTime(failing.since)}, so checked for every so often: ${failing.error}`}
    >
      {' '}
      (not watched)
    </span>
  )
}

/** A Matrix account's first sync, then, as a mail account's, how far
 * fetching its rooms' history has got (SPEC §9.2). */
function MatrixSyncing(props: { synced: boolean; rooms: readonly Room[] }) {
  if (!props.synced) return <span class={styles.syncing}> (syncing)</span>
  const progress = historyProgress(props.rooms.map((room) => room.history), Date.now())
  if (!progress) return null
  const rooms = progress.rooms === 1 ? '1 room' : `${progress.rooms} rooms`
  return (
    <span class={styles.syncing} title={`Fetching the history of ${rooms}: stored since ${date(new Date(progress.since))}`}>
      {' '}
      (syncing{progress.percent === null ? '' : `, ${progress.percent}%`})
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
  const places: string[][] = [...views(mailAccounts), ...BELOW]
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

/** A count's look: as any, quietly; or red, as what needs the user's
 * attention. */
/** A nav place's count: a `size` (how many there are) is a plain number,
 * the others a `Badge`'s. */
type Tone = 'size' | BadgeTone

const marked = (mark: Mark) => mark.count > 0 || mark.dot

/** `open`: shown as a drawer, where the window is narrow. */
export function Nav(props: { open: boolean }) {
  const { mail, drafts, matrix, todos } = useServices()
  const { path, route } = useLocation()
  const marks = useMarks()
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
    tone: Tone = 'size',
    bold = false,
    mark: Mark = NONE,
  ) => {
    const current = shows(path, href)
    return (
      <a
        href={href}
        class={[current && styles.current, bold && styles.bold].filter(Boolean).join(' ') || undefined}
        aria-current={current ? 'page' : undefined}
        // What Alt+Shift+arrows move to (`step`).
        data-unread={count > 0 || bold || marked(mark) || undefined}
        style={{ '--depth': depth }}
      >
        {lead}
        <span class={styles.name}>{name}</span>
        <MarkDot mark={mark} />
        <Count count={count} tone={tone} />
        <MarkCount mark={mark} />
      </a>
    )
  }
  const item = (href: string, icon: IconName, name: string, count: number, tone: Tone = 'size', mark: Mark = NONE) => (
    <li key={href}>{link(href, <Icon name={icon} />, name, count, 0, tone, false, mark)}</li>
  )
  const labelItem = (account: Account, label: Label) => (
    <li key={label.id}>
      {link(
        mailPath(account.id, label.external_id),
        <Icon name={labelIcon(label)} />,
        labelName(account, label),
        label.unread,
        0,
        'unread',
        false,
        marks.label(label),
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
            'unread',
            false,
            folded ? strongest(labelsIn(node).map(marks.label)) : marks.label(node.label),
          )
        ) : (
          <span class={styles.parent} style={{ '--depth': depth }}>
            <Icon name={node.children.some((c) => c.label?.kind === 'folder') ? 'folder' : 'tag'} />
            <span class={styles.name}>{node.name}</span>
            {folded && <PlaceMark mark={strongest(labelsIn(node).map(marks.label))} />}
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
      return (
        <li key={room.id}>
          {link(
            roomPath(room.id),
            <RoomAvatar room={room} />,
            room.membership === 'invited' ? `${room.name} (invited)` : room.name,
            0,
            depth,
            'size',
            room.unread > 0,
            marks.room(room),
          )}
        </li>
      )
    }
    const key = spaceFold(room)
    const folded = collapsed.value.has(key)
    const within = folded ? roomsIn(node) : []
    const row = (
      <>
        <RoomAvatar room={room} />
        <span class={within.some((r) => r.unread > 0) ? `${styles.name} ${styles.bold}` : styles.name}>
          {room.name}
        </span>
        {folded && <PlaceMark mark={strongest(within.map(marks.room))} />}
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
    return item(securityPath(account), 'shield', 'Security', awaiting, 'alert')
  }
  const outbox = drafts.outbox.value ?? []
  const counts: Record<string, [number, Tone]> = {
    [inboxPath()]: [mail.accounts.value?.reduce((unread, account) => unread + inboxUnread(account), 0) ?? 0, 'unread'],
    '/': [0, 'size'],
    '/drafts': [drafts.drafts.value?.size ?? 0, 'size'],
    // What of it is not yet at the providers.
    '/history': [outboxCount(outbox), outboxNeedsAttention(outbox) ? 'alert' : 'size'],
    '/todos': [todos.todos.value?.filter((t) => t.done_at === null).length ?? 0, 'size'],
    '/calendar': [0, 'size'],
  }
  const viewItems = (list: readonly View[]) => (
    <ul>
      {list.map((view) => {
        const [count, tone] = counts[view.href]!
        return item(view.href, view.icon, view.name, count, tone, marks.view(view.href))
      })}
    </ul>
  )
  return (
    <nav ref={nav} class={props.open ? `${styles.nav} ${styles.open}` : styles.nav}>
      {viewItems(views(mail.accounts.value ?? []))}
      {mail.accounts.value?.map((account) => (
        <Section
          key={account.id}
          id={mailSection(account.id)}
          title={account.address}
          tooltip={account.id}
          status={
            <>
              {!account.synced && <Syncing account={account.id} listing={account.listing} />}
              {account.watch_failing && <Unwatched failing={account.watch_failing} />}
            </>
          }
          unread={inboxUnread(account)}
          mark={marks.account(account.id)}
        >
          {navLabels(account).system.map((label) => labelItem(account, label))}
          {item(allMailPath(account.id), 'mails', 'All Mail', 0, 'size', marks.account(account.id))}
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
            status={<MatrixSyncing synced={account.synced} rooms={rooms} />}
            unread={0}
            bold={rooms.some((room) => room.unread > 0)}
            mark={strongest(rooms.map(marks.room))}
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
      {viewItems(BELOW)}
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
  switch (tone) {
    case 'size':
      return <span class={styles.count}>{props.count}</span>
    case 'alert':
      return <Badge count={props.count} tone="alert" title="Something needs your attention" />
    case 'unread':
      return <Badge count={props.count} tone="unread" title="Unread" />
  }
}

/** A place's mark (SPEC §12.14): of the count buckets' items in it, in
 * red; else a dot, while a dot bucket's item in it is unseen. */
export function PlaceMark(props: { mark: Mark }) {
  return (
    <>
      <MarkDot mark={props.mark} />
      <MarkCount mark={props.mark} />
    </>
  )
}

/** A mark's dot, before a place's own count: it comes and goes as the
 * place is seen, which would otherwise move the count. */
function MarkDot(props: { mark: Mark }) {
  if (props.mark.count > 0 || !props.mark.dot) return null
  return <span class={styles.dot} title="Something new" />
}

/** A mark's red count, after a place's own. */
function MarkCount(props: { mark: Mark }) {
  return <Badge count={props.mark.count} tone="alert" title="Needs your attention" />
}

/** The marks of the nav's places, as the server last said. */
function useMarks() {
  const { home, mail } = useServices()
  const badges = home.badges.value
  const accounts = mail.accounts.value ?? []
  const of = (place: Place) => markOf(badges, place, accounts)
  return {
    label: (label: Label) => of({ kind: 'label', label: label.id }),
    room: (room: Room) => of({ kind: 'room', room: room.id }),
    account: (account: string) => of({ kind: 'account', account }),
    view(href: string): Mark {
      const place = VIEW_PLACES[href]
      return place ? of(place) : NONE
    },
  }
}

/** The views above the accounts that are places. */
const VIEW_PLACES: Record<string, Place> = {
  '/': { kind: 'home' },
  [inboxPath()]: { kind: 'inboxes' },
  '/todos': { kind: 'todos' },
  '/calendar': { kind: 'calendar' },
}

/** The labels of a node and those nested in it. */
function labelsIn(node: LabelNode): Label[] {
  return [...(node.label ? [node.label] : []), ...node.children.flatMap(labelsIn)]
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

export function inboxUnread(account: Account): number {
  return account.labels.find(isInbox)?.unread ?? 0
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
  bold?: boolean
  mark: Mark
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
          <span class={folded && props.bold ? `${styles.name} ${styles.bold}` : styles.name}>{props.title}</span>
          {props.status}
          {folded && <MarkDot mark={props.mark} />}
          {folded && <Badge count={props.unread} tone="unread" title="Unread" />}
          {folded && <MarkCount mark={props.mark} />}
        </button>
      </h2>
      {!folded && <ul>{props.children}</ul>}
    </section>
  )
}
