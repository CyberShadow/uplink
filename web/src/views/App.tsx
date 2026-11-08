import type { ReadonlySignal } from '@preact/signals'
import { useEffect, useState } from 'preact/hooks'
import { LocationProvider, Route, Router, useLocation } from 'preact-iso'
import type { Status } from '../connection'
import { ServicesContext, useServices } from '../context'
import { useKeymap } from '../keymap'
import type { MailStore } from '../stores/mail'
import { Help } from './Help'
import { Mail } from './Mail'
import { Nav } from './Nav'
import { NoticeBar } from './NoticeBar'
import { SearchBox } from './SearchBox'
import styles from './App.module.css'

export function App(props: { mail: MailStore; status: ReadonlySignal<Status> }) {
  return (
    <ServicesContext.Provider value={{ mail: props.mail, status: props.status }}>
      <LocationProvider>
        <Shell />
      </LocationProvider>
    </ServicesContext.Provider>
  )
}

function Shell() {
  const { mail, status } = useServices()
  const { route, path } = useLocation()
  const [help, setHelp] = useState(false)

  /** The account on show, or else the first. */
  const account = () => {
    const shown = path.match(/^\/mail\/([^/]+)/)?.[1]
    return shown ? decodeURIComponent(shown) : mail.accounts.value?.[0]?.id
  }
  const go = (label: string) => () => {
    const id = account()
    if (id) route(mailPath(id, label))
  }
  useKeymap({
    goInbox: go('INBOX'),
    goStarred: go('STARRED'),
    goSent: go('SENT'),
    goDrafts: go('DRAFT'),
    search: () => document.querySelector<HTMLInputElement>('input[type=search]')?.focus(),
    help: () => setHelp((shown) => !shown),
  })

  return (
    <div class={styles.shell}>
      <header class={styles.header}>
        <span class={styles.brand}>uplink</span>
        <SearchBox />
        {status.value === 'connecting' && <span class={styles.status}>Connecting…</span>}
      </header>
      <Nav />
      <main class={styles.content}>
        {mail.error.value && (
          <div class={styles.error} role="alert">
            {mail.error.value}
          </div>
        )}
        <Router>
          <Route path="/mail/:account/:label/:entry?" component={Mail} />
          <Route path="/search/:terms/:entry?" component={Mail} />
          <Route default component={Home} />
        </Router>
      </main>
      <NoticeBar />
      {help && <Help close={() => setHelp(false)} />}
    </div>
  )
}

/** Shows the first account's Inbox. */
function Home() {
  const { mail } = useServices()
  const { route } = useLocation()
  const first = mail.accounts.value?.[0]?.id
  useEffect(() => {
    if (first) route(mailPath(first, 'INBOX'), true)
  }, [first])
  return <p class={styles.placeholder}>{mail.accounts.value ? 'No accounts configured.' : 'Loading…'}</p>
}

export function mailPath(account: string, label: string, entry?: number): string {
  const base = `/mail/${encodeURIComponent(account)}/${encodeURIComponent(label)}`
  return entry === undefined ? base : `${base}/${entry}`
}

export function searchPath(query: string, entry?: number): string {
  const base = `/search/${encodeURIComponent(query)}`
  return entry === undefined ? base : `${base}/${entry}`
}
