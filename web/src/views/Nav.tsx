import { useLocation } from 'preact-iso'
import type { Account, Label } from '../generated/protocol'
import { useServices } from '../context'
import { allMailPath, mailPath } from './App'
import { roomPath } from './Room'
import styles from './Nav.module.css'

/** Gmail's system labels worth a place in the nav, in Gmail's order. */
const SYSTEM: Record<string, string> = {
  INBOX: 'Inbox',
  STARRED: 'Starred',
  IMPORTANT: 'Important',
  SENT: 'Sent',
  // uplink's own drafts are in Drafts, above the accounts.
  DRAFT: 'Gmail drafts',
  SPAM: 'Spam',
  TRASH: 'Trash',
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

export function labelName(label: Label): string {
  return (label.system && SYSTEM[label.external_id]) || label.name
}

export function Nav() {
  const { mail, drafts, matrix, todos } = useServices()
  const { path } = useLocation()
  const link = (href: string, name: string, count: number) => {
    const current = path === href || path.startsWith(`${href}/`)
    return (
      <li key={href}>
        <a href={href} class={current ? styles.current : undefined} aria-current={current ? 'page' : undefined}>
          <span>{name}</span>
          {count > 0 && <span class={styles.count}>{count}</span>}
        </a>
      </li>
    )
  }
  const labelLink = (account: Account, label: Label) =>
    link(mailPath(account.id, label.external_id), labelName(label), label.unread)
  return (
    <nav class={styles.nav}>
      <ul>
        {link('/drafts', 'Drafts', drafts.drafts.value?.size ?? 0)}
        {link('/outbox', 'Outbox', drafts.outbox.value?.length ?? 0)}
        {link('/history', 'History', 0)}
        {link('/todos', 'To-dos', todos.todos.value?.filter((t) => t.done_at === null).length ?? 0)}
      </ul>
      {mail.accounts.value?.map((account) => (
        <section key={account.id}>
          <h2 class={styles.account} title={account.id}>
            {account.address}
            {!account.synced && <span class={styles.syncing}> (syncing)</span>}
          </h2>
          <ul>
            {navLabels(account).system.map((label) => labelLink(account, label))}
            {link(allMailPath(account.id), 'All Mail', 0)}
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
