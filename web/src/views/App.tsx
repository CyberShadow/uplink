import type { ReadonlySignal } from '@preact/signals'
import { useEffect, useState } from 'preact/hooks'
import { lazy, LocationProvider, Route, Router, useLocation } from 'preact-iso'
import type { Status } from '../connection'
import { EMPTY } from '../compose/reply'
import { ServicesContext, useServices } from '../context'
import { useKeymap } from '../keymap'
import type { Client } from '../stores/client'
import { Help } from './Help'
import { Mail } from './Mail'
import { Nav } from './Nav'
import { NoticeBar } from './NoticeBar'
import { composePath, Drafts, Outbox } from './Outgoing'
import { SearchBox } from './SearchBox'
import styles from './App.module.css'

const Compose = lazy(() => import('./Compose'))

export function App(props: { client: Client; status: ReadonlySignal<Status> }) {
  const { ops, mail, drafts } = props.client
  return (
    <ServicesContext.Provider value={{ ops, mail, drafts, status: props.status }}>
      <LocationProvider>
        <Shell />
      </LocationProvider>
    </ServicesContext.Provider>
  )
}

function Shell() {
  const { mail, drafts, status } = useServices()
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
    goDrafts: () => route('/drafts'),
    compose: async () => {
      const id = account()
      if (!id) return
      const draft = await drafts.create(id, EMPTY, null)
      if (draft) route(composePath(draft))
    },
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
        {(mail.error.value ?? drafts.error.value) && (
          <div class={styles.error} role="alert">
            {mail.error.value ?? drafts.error.value}
          </div>
        )}
        <Router>
          <Route path="/mail/:account/:label/:entry?" component={Mail} />
          <Route path="/search/:terms/:entry?" component={Mail} />
          <Route path="/compose/:draft" component={Compose} />
          <Route path="/drafts" component={Drafts} />
          <Route path="/outbox" component={Outbox} />
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
  }, [first, route])
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
