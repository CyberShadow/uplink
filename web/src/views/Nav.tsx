import { useLocation } from 'preact-iso'
import type { Account, Label, ListingProgress, MailProvider } from '../generated/protocol'
import { useServices } from '../context'
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

/** A mail account's first sync, and how far it has got if known. Its
 * total includes Spam and Trash, which it skips: it ends short of 100%. */
function Syncing(props: { listing: ListingProgress | null }) {
  const { listing } = props
  if (!listing) return <span class={styles.syncing}> (syncing)</span>
  const percent = Math.min(99, Math.floor((listing.stored / Math.max(listing.total, 1)) * 100))
  const count = (n: number) => n.toLocaleString('en-US')
  return (
    <span class={styles.syncing} title={`${count(listing.stored)} of about ${count(listing.total)} messages`}>
      {' '}
      (syncing, {percent}%)
    </span>
  )
}

export function Nav() {
  const { mail, drafts, matrix, todos } = useServices()
  const { path } = useLocation()
  const link = (href: string, icon: IconName, name: string, count: number) => {
    const current = path === href || path.startsWith(`${href}/`)
    return (
      <li key={href}>
        <a href={href} class={current ? styles.current : undefined} aria-current={current ? 'page' : undefined}>
          <Icon name={icon} />
          <span class={styles.name}>{name}</span>
          {count > 0 && <span class={styles.count}>{count}</span>}
        </a>
      </li>
    )
  }
  const labelLink = (account: Account, label: Label) =>
    link(mailPath(account.id, label.external_id), labelIcon(label), labelName(account, label), label.unread)
  return (
    <nav class={styles.nav}>
      <ul>
        {(mail.accounts.value?.length ?? 0) > 1 &&
          link(
            inboxPath(),
            'inbox',
            'All inboxes',
            mail.accounts.value!.reduce(
              (unread, account) =>
                unread + (account.labels.find((l) => l.system && l.external_id === 'INBOX')?.unread ?? 0),
              0,
            ),
          )}
        {link('/drafts', 'file', 'Drafts', drafts.drafts.value?.size ?? 0)}
        {link('/outbox', 'clock-arrow-up', 'Outbox', drafts.outbox.value?.length ?? 0)}
        {link('/history', 'undo-2', 'History', 0)}
        {link('/todos', 'list-todo', 'To-dos', todos.todos.value?.filter((t) => t.done_at === null).length ?? 0)}
        {link('/calendar', 'calendar', 'Calendar', 0)}
      </ul>
      {mail.accounts.value?.map((account) => (
        <section key={account.id}>
          <h2 class={styles.account} title={account.id}>
            {account.address}
            {!account.synced && <Syncing listing={account.listing} />}
          </h2>
          <ul>
            {navLabels(account).system.map((label) => labelLink(account, label))}
            {link(allMailPath(account.id), 'mails', 'All Mail', 0)}
            {navLabels(account).user.map((label) => labelLink(account, label))}
          </ul>
        </section>
      ))}
      {matrix.accounts.value?.map((account) => (
        <section key={account.id} aria-label={`Rooms of ${account.mxid}`}>
          <h2 class={styles.account} title={account.id}>
            {account.mxid}
            {!account.synced && <span class={styles.syncing}> (syncing)</span>}
          </h2>
          <ul>
            {matrix.rooms.value
              ?.filter((room) => room.account === account.id)
              .map((room) => {
                const href = roomPath(room.id)
                const current = path === href
                return (
                  <li key={room.id}>
                    <a
                      href={href}
                      class={current ? styles.current : undefined}
                      aria-current={current ? 'page' : undefined}
                    >
                      <span class={styles.name}>
                        {room.name}
                        {room.membership === 'invited' && <span class={styles.syncing}> (invited)</span>}
                      </span>
                      {room.unread > 0 && <span class={styles.count}>{room.unread}</span>}
                    </a>
                  </li>
                )
              })}
          </ul>
        </section>
      ))}
    </nav>
  )
}
