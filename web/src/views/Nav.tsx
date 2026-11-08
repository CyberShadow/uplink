import { useLocation } from 'preact-iso'
import type { Account, Label } from '../generated/protocol'
import { useServices } from '../context'
import { mailPath } from './App'
import styles from './Nav.module.css'

/** Gmail's system labels worth a place in the nav, in Gmail's order. */
const SYSTEM: Record<string, string> = {
  INBOX: 'Inbox',
  STARRED: 'Starred',
  IMPORTANT: 'Important',
  SENT: 'Sent',
  DRAFT: 'Drafts',
  SPAM: 'Spam',
  TRASH: 'Trash',
}

export function navLabels(account: Account): Label[] {
  const system = Object.keys(SYSTEM).flatMap(
    (id) => account.labels.filter((l) => l.system && l.external_id === id),
  )
  const user = account.labels
    .filter((l) => !l.system)
    .sort((a, b) => a.name.localeCompare(b.name))
  return [...system, ...user]
}

export function labelName(label: Label): string {
  return (label.system && SYSTEM[label.external_id]) || label.name
}

export function Nav() {
  const { mail } = useServices()
  const { path } = useLocation()
  return (
    <nav class={styles.nav}>
      {mail.accounts.value?.map((account) => (
        <section key={account.id}>
          <h2 class={styles.account} title={account.id}>
            {account.address}
            {!account.synced && <span class={styles.syncing}> (syncing)</span>}
          </h2>
          <ul>
            {navLabels(account).map((label) => {
              const href = mailPath(account.id, label.external_id)
              const current = path === href || path.startsWith(`${href}/`)
              return (
                <li key={label.id}>
                  <a
                    href={href}
                    class={current ? styles.current : undefined}
                    aria-current={current ? 'page' : undefined}
                  >
                    <span>{labelName(label)}</span>
                    {label.unread > 0 && <span class={styles.count}>{label.unread}</span>}
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
