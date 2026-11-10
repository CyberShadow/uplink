import { signal } from '@preact/signals'
import type { ComponentChildren } from 'preact'
import { useLocation } from 'preact-iso'
import { duration } from '../format'
import { keepFocus } from '../keymap'
import type { Account, Label, ListingProgress, MailProvider } from '../generated/protocol'
import { useServices } from '../context'
import { outboxCount, outboxNeedsAttention } from '../stores/drafts'
import { allMailPath, inboxPath, mailPath } from './App'
import { roomPath } from './Room'
import styles from './Nav.module.css'
import { Icon, type IconName } from './Icon'

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
function labelIcon(label: Label): IconName {
  if (label.system) return SYSTEM[label.external_id]!.icon
  return label.folder ? 'folder' : 'tag'
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

/** `open`: shown as a drawer, where the window is narrow. */
export function Nav(props: { open: boolean }) {
  const { mail, drafts, matrix, todos } = useServices()
  const { path } = useLocation()
  const link = (href: string, icon: IconName | null, name: string, count: number, depth = 0, alert = false) => {
    const current = path === href || path.startsWith(`${href}/`)
    return (
      <a
        href={href}
        class={current ? styles.current : undefined}
        aria-current={current ? 'page' : undefined}
        style={{ '--depth': depth }}
      >
        {icon && <Icon name={icon} />}
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
    <li key={href}>{link(href, icon, name, count, 0, alert)}</li>
  )
  const labelItem = (account: Account, label: Label) => (
    <li key={label.id}>
      {link(mailPath(account.id, label.external_id), labelIcon(label), labelName(account, label), label.unread)}
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
          link(mailPath(account.id, node.label.external_id), labelIcon(node.label), node.name, node.label.unread, depth)
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
  return (
    <nav class={props.open ? `${styles.nav} ${styles.open}` : styles.nav}>
      <ul>
        {(mail.accounts.value?.length ?? 0) > 1 &&
          item(
            inboxPath(),
            'inbox',
            'All inboxes',
            mail.accounts.value!.reduce((unread, account) => unread + inboxUnread(account), 0),
          )}
        {item('/drafts', 'file', 'Drafts', drafts.drafts.value?.size ?? 0)}
        {item(
          '/outbox',
          'clock-arrow-up',
          'Outbox',
          outboxCount(drafts.outbox.value ?? []),
          outboxNeedsAttention(drafts.outbox.value ?? []),
        )}
        {item('/history', 'undo-2', 'History', 0)}
        {item('/todos', 'list-todo', 'To-dos', todos.todos.value?.filter((t) => t.done_at === null).length ?? 0)}
        {item('/calendar', 'calendar', 'Calendar', 0)}
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
            {rooms.map((room) => (
              <li key={room.id}>
                {link(
                  roomPath(room.id),
                  null,
                  room.membership === 'invited' ? `${room.name} (invited)` : room.name,
                  room.unread,
                )}
              </li>
            ))}
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
