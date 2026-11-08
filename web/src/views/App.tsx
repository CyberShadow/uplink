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
import { RoomView } from './Room'
import { composePath, Drafts, Outbox } from './Outgoing'
import { SearchBox } from './SearchBox'
import { Todos } from './Todos'
import styles from './App.module.css'

const Compose = lazy(() => import('./Compose'))

export function App(props: { client: Client; status: ReadonlySignal<Status> }) {
  const { ops, mail, drafts, matrix, todos } = props.client
  return (
    <ServicesContext.Provider value={{ ops, mail, drafts, matrix, todos, status: props.status }}>
      <LocationProvider>
        <Shell />
      </LocationProvider>
    </ServicesContext.Provider>
  )
}

function Shell() {
  const { mail, drafts, matrix, todos, status } = useServices()
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
    goTodos: () => route('/todos'),
    compose: async () => {
      const id = account()
      if (!id) return
      const draft = await drafts.create(id, EMPTY, null)
      if (draft) route(composePath(draft))
    },
    search: () => document.querySelector<HTMLInputElement>('input[type=search]')?.focus(),
    help: () => setHelp((shown) => !shown),
  })

  const error = mail.error.value ?? drafts.error.value ?? matrix.error.value ?? todos.error.value
  return (
    <div class={styles.shell}>
      <header class={styles.header}>
        <span class={styles.brand}>uplink</span>
        <SearchBox />
        {status.value === 'connecting' && <span class={styles.status}>Connecting…</span>}
      </header>
      <Nav />
      <main class={styles.content}>
        {error && (
          <div class={styles.error} role="alert">
            {error}
          </div>
        )}
        <Router>
          <Route path="/mail/:account/:label/:entry?" component={Mail} />
          <Route path="/search/:terms/:entry?" component={Mail} />
          <Route path="/message/:message" component={Mail} />
          <Route path="/todos" component={Todos} />
          <Route path="/compose/:draft" component={Compose} />
          <Route path="/drafts" component={Drafts} />
          <Route path="/outbox" component={Outbox} />
          <Route path="/rooms/:room" component={RoomView} />
          <Route default component={Home} />
        </Router>
      </main>
      <NoticeBar />
      {help && <Help close={() => setHelp(false)} />}
    </div>
  )
}

/** Shows the first mail account's Inbox. */
function Home() {
  const { mail, matrix } = useServices()
  const { route } = useLocation()
  const first = mail.accounts.value?.[0]?.id
  useEffect(() => {
    if (first) route(mailPath(first, 'INBOX'), true)
  }, [first, route])
  const loaded = mail.accounts.value && matrix.accounts.value
  const message = !loaded ? 'Loading…' : matrix.accounts.value!.length > 0 ? 'Pick a room.' : 'No accounts configured.'
  return <p class={styles.placeholder}>{message}</p>
}

export function mailPath(account: string, label: string, entry?: number): string {
  const base = `/mail/${encodeURIComponent(account)}/${encodeURIComponent(label)}`
  return entry === undefined ? base : `${base}/${entry}`
}

export function searchPath(query: string, entry?: number): string {
  const base = `/search/${encodeURIComponent(query)}`
  return entry === undefined ? base : `${base}/${entry}`
}
