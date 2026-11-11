import type { ReadonlySignal } from '@preact/signals'
import { useEffect, useState } from 'preact/hooks'
import { lazy, LocationProvider, Route, Router, useLocation } from 'preact-iso'
import type { Status } from '../connection'
import type { Account } from '../generated/protocol'
import { EMPTY } from '../compose/reply'
import { ServicesContext, useServices } from '../context'
import { keepFocus, useKeymap } from '../keymap'
import type { Client } from '../stores/client'
import { Calendar, calendarPath, today } from './Calendar'
import { Help } from './Help'
import { Icon } from './Icon'
import { History } from './History'
import { GoToLabel } from './LabelPicker'
import { Mail } from './Mail'
import { Nav } from './Nav'
import { NoticeBar } from './NoticeBar'
import { useNotifications } from './Notifications'
import { RoomView } from './Room'
import { composePath, Drafts, Outbox } from './Outgoing'
import { SearchBox } from './SearchBox'
import { SearchHelp } from './SearchHelp'
import { SecurityView } from './Security'
import { RecordVisits, Switcher } from './Switcher'
import { Todos } from './Todos'
import styles from './App.module.css'

const Compose = lazy(() => import('./Compose'))

export function App(props: { client: Client; status: ReadonlySignal<Status> }) {
  const { ops, mail, drafts, matrix, todos, history, calendar } = props.client
  return (
    <ServicesContext.Provider value={{ ops, mail, drafts, matrix, todos, history, calendar, status: props.status }}>
      <LocationProvider>
        <Shell />
      </LocationProvider>
    </ServicesContext.Provider>
  )
}

function Shell() {
  const { mail, drafts, matrix, todos, history, calendar, status } = useServices()
  const { route, path } = useLocation()
  const [help, setHelp] = useState(false)
  const [goingToLabel, setGoingToLabel] = useState(false)
  const [switching, setSwitching] = useState(false)
  // Narrow, the nav is a drawer; going anywhere closes it.
  const [navOpen, setNavOpen] = useState(false)
  useEffect(() => setNavOpen(false), [path])
  useNotifications(path, route)

  /** The account on show, or else the first. */
  const account = () => {
    const shown = path.match(/^\/(?:mail|all)\/([^/]+)/)?.[1]
    return shown ? decodeURIComponent(shown) : mail.accounts.value?.[0]?.id
  }
  const go = (label: string) => () => {
    const id = account()
    if (id) route(mailPath(id, label))
  }
  useKeymap({
    // The Inbox of the account on show, or else of every account.
    goInbox: () => {
      const home = mail.accounts.value && homeInbox(mail.accounts.value)
      if (/^\/(?:mail|all)\//.test(path)) go('INBOX')()
      else if (home) route(home)
    },
    goStarred: go('STARRED'),
    goSent: go('SENT'),
    goAll: () => {
      const id = account()
      if (id) route(allMailPath(id))
    },
    goLabel: () => setGoingToLabel(account() !== undefined),
    goDrafts: () => route('/drafts'),
    goTodos: () => route('/todos'),
    goHistory: () => route('/history'),
    goCalendar: () => route(calendarPath('week', today())),
    compose: async () => {
      const id = account()
      if (!id) return
      const draft = await drafts.create(id, EMPTY, null)
      if (draft) route(composePath(draft))
    },
    search: () => document.querySelector<HTMLInputElement>('input[type=search]')?.focus(),
    switch: () => setSwitching((shown) => !shown),
    help: () => setHelp((shown) => !shown),
  })

  const error = mail.error.value ?? drafts.error.value ?? matrix.error.value ?? todos.error.value ?? history.error.value ?? calendar.error.value
  return (
    <div class={styles.shell}>
      <header class={styles.header}>
        <button
          type="button"
          class={styles.menu}
          aria-label="Menu"
          aria-expanded={navOpen}
          onMouseDown={keepFocus}
          onClick={() => setNavOpen(!navOpen)}
        >
          <Icon name="menu" />
        </button>
        <span class={styles.brand}>
          <img src="/logo.svg" alt="" />
          uplink
        </span>
        <SearchBox />
        {status.value === 'connecting' && <span class={styles.status}>Connecting…</span>}
      </header>
      <Nav open={navOpen} />
      {navOpen && <div class={styles.scrim} onClick={() => setNavOpen(false)} />}
      <main class={styles.content}>
        {error && (
          <div class={styles.error} role="alert">
            {error}
          </div>
        )}
        <Router>
          <Route path="/inbox/:entry?" component={Inbox} />
          <Route path="/mail/:account/:label/:entry?" component={Mail} />
          <Route path="/all/:account/:entry?" component={Mail} />
          <Route path="/search/:terms/:entry?" component={Mail} />
          <Route path="/help/search" component={SearchHelp} />
          <Route path="/message/:message" component={Mail} />
          <Route path="/todos" component={Todos} />
          <Route path="/history" component={History} />
          <Route path="/calendar" component={Calendar} />
          <Route path="/calendar/:view/:date" component={Calendar} />
          <Route path="/compose/:draft" component={Compose} />
          <Route path="/drafts" component={Drafts} />
          <Route path="/outbox" component={Outbox} />
          <Route path="/rooms/:room" component={RoomView} />
          <Route path="/matrix/:account/security" component={SecurityView} />
          <Route default component={Home} />
        </Router>
      </main>
      <NoticeBar />
      <RecordVisits />
      {help && <Help close={() => setHelp(false)} />}
      {switching && <Switcher close={() => setSwitching(false)} />}
      {goingToLabel && (
        <GoToLabel
          account={mail.accounts.value!.find((a) => a.id === account())!}
          go={(label) => route(mailPath(account()!, label.external_id))}
          close={() => setGoingToLabel(false)}
        />
      )}
    </div>
  )
}

function Inbox(props: { entry?: string }) {
  return <Mail inbox entry={props.entry} />
}

/** Shows every account's Inbox, or the one account's. */
function Home() {
  const { mail, matrix } = useServices()
  const { route } = useLocation()
  const inbox = mail.accounts.value && homeInbox(mail.accounts.value)
  useEffect(() => {
    if (inbox) route(inbox, true)
  }, [inbox, route])
  const loaded = mail.accounts.value && matrix.accounts.value
  const message = !loaded ? 'Loading…' : matrix.accounts.value!.length > 0 ? 'Pick a room.' : 'No accounts configured.'
  return <p class={styles.placeholder}>{message}</p>
}

/** With more than one mail account, their Inboxes are one. */
function homeInbox(accounts: Account[]): string | null {
  if (accounts.length > 1) return inboxPath()
  return accounts.length === 1 ? mailPath(accounts[0].id, 'INBOX') : null
}

export function inboxPath(entry?: number): string {
  return entry === undefined ? '/inbox' : `/inbox/${entry}`
}

export function mailPath(account: string, label: string, entry?: number): string {
  const base = `/mail/${encodeURIComponent(account)}/${encodeURIComponent(label)}`
  return entry === undefined ? base : `${base}/${entry}`
}

export function allMailPath(account: string, entry?: number): string {
  const base = `/all/${encodeURIComponent(account)}`
  return entry === undefined ? base : `${base}/${entry}`
}

export function searchPath(query: string, entry?: number): string {
  const base = `/search/${encodeURIComponent(query)}`
  return entry === undefined ? base : `${base}/${entry}`
}
