import type { ReadonlySignal } from '@preact/signals'
import { useEffect, useLayoutEffect, useRef, useState } from 'preact/hooks'
import { lazy, LocationProvider, Route, Router, useLocation } from 'preact-iso'
import type { Status } from '../connection'
import type { Account } from '../generated/protocol'
import { EMPTY } from '../compose/reply'
import { ServicesContext, useServices } from '../context'
import { restoreFocus } from '../historyFocus'
import { keepFocus, useKeymap } from '../keymap'
import type { Client } from '../stores/client'
import { Calendar, calendarPath, today } from './Calendar'
import { Help, WhichKey } from './Help'
import { Icon } from './Icon'
import { History } from './History'
import { Home } from './Home'
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
import { useTitle } from '../title'
import styles from './App.module.css'

const Compose = lazy(() => import('./Compose'))

export function App(props: { client: Client; status: ReadonlySignal<Status> }) {
  const { ops, mail, drafts, matrix, todos, history, calendar, home } = props.client
  return (
    <ServicesContext.Provider value={{ ops, mail, drafts, matrix, todos, history, calendar, home, status: props.status }}>
      <LocationProvider>
        <Shell />
      </LocationProvider>
    </ServicesContext.Provider>
  )
}

function Shell() {
  const { mail, drafts, matrix, todos, history, calendar, home, status } = useServices()
  const { route, path } = useLocation()
  const [help, setHelp] = useState(false)
  const [goingToLabel, setGoingToLabel] = useState(false)
  const [switching, setSwitching] = useState(false)
  // Narrow, the nav is a drawer; going anywhere closes it.
  const [navOpen, setNavOpen] = useState(false)
  useEffect(() => setNavOpen(false), [path])
  useNotifications(path, route)
  // A view shown has the focus it had, if gone back or forward to; else
  // the focus it takes (a room's box), or none: not that of a field
  // outside it, as the search box, left from the view before. After its
  // own effects, as a parent's run after its children's.
  const main = useRef<HTMLElement>(null)
  useLayoutEffect(() => {
    if (restoreFocus()) return
    const focused = document.activeElement as HTMLElement
    if (!main.current!.contains(focused)) focused.blur()
  }, [path])

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
    goHome: () => route('/'),
    // As high as Esc goes, where views have nothing nearer (`up`).
    up: () => route('/'),
    goHistory: () => route('/history'),
    undo: () => history.undoLast(),
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

  const error = mail.error.value ?? drafts.error.value ?? matrix.error.value ?? todos.error.value ?? history.error.value ?? calendar.error.value ?? home.error.value
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
      <main ref={main} class={styles.content}>
        {error && (
          <div class={styles.error} role="alert">
            {error}
          </div>
        )}
        <Router>
          <Route path="/" component={Home} />
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
          <Route path="/rooms/:room/search/:words" component={RoomView} />
          <Route path="/matrix/:account/security" component={SecurityView} />
          <Route default component={NotFound} />
        </Router>
      </main>
      <NoticeBar />
      <RecordVisits />
      {help && <Help close={() => setHelp(false)} />}
      <WhichKey />
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

function NotFound() {
  useTitle('Not found')
  return (
    <p class={styles.placeholder}>
      Nothing is here. <a href="/">Home</a>
    </p>
  )
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
