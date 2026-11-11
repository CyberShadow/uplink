// The switcher (Ctrl+K): every view the nav links to, the most recently
// visited first, filtered as typed.

import type { ComponentChildren } from 'preact'
import { useEffect } from 'preact/hooks'
import { useLocation } from 'preact-iso'
import { useServices } from '../context'
import type { Label } from '../generated/protocol'
import { allMailPath, mailPath } from './App'
import { Icon } from './Icon'
import { labelIcon, labelName, linksTo, navLabels, shows, views } from './Nav'
import { Picker } from './Picker'
import { roomPath } from './Room'
import { RoomAvatar } from './RoomAvatar'
import styles from './Overlay.module.css'

/** A view to switch to. */
interface Destination {
  href: string
  name: string
  /** The account it is of, if one. */
  account: string | null
  lead: ComponentChildren
}

/** The nav's links, in its order. */
function useDestinations(): Destination[] {
  const { mail, matrix } = useServices()
  const accounts = mail.accounts.value ?? []
  return [
    ...views(accounts).map((view) => ({
      href: view.href,
      name: view.name,
      account: null,
      lead: <Icon name={view.icon} />,
    })),
    ...accounts.flatMap((account) => {
      const { system, user } = navLabels(account)
      const label = (l: Label) => ({
        href: mailPath(account.id, l.external_id),
        name: labelName(account, l),
        account: account.address,
        lead: <Icon name={labelIcon(l)} />,
      })
      const all = { href: allMailPath(account.id), name: 'All Mail', account: account.address, lead: <Icon name="mails" /> }
      return [...system.map(label), all, ...user.map(label)]
    }),
    ...(matrix.accounts.value ?? []).flatMap((account) =>
      (matrix.rooms.value ?? [])
        .filter((room) => room.account === account.id && linksTo(room))
        .map((room) => ({
          href: roomPath(room.id),
          name: room.membership === 'invited' ? `${room.name} (invited)` : room.name,
          account: account.mxid,
          lead: <RoomAvatar room={room} />,
        })),
    ),
  ]
}

/** The destinations visited, most recent first; kept in this browser. */
const RECENT_KEY = 'uplink.recent'
const RECENT_MAX = 100

function recent(): string[] {
  return JSON.parse(localStorage.getItem(RECENT_KEY) ?? '[]')
}

/** Records visits to destinations, for the switcher's order. */
export function RecordVisits() {
  const { path } = useLocation()
  const here = useDestinations().find((d) => shows(path, d.href))?.href
  useEffect(() => {
    // Not every path is a destination's: a search's, a draft's.
    if (here === undefined) return
    const others = recent().filter((href) => href !== here)
    localStorage.setItem(RECENT_KEY, JSON.stringify([here, ...others].slice(0, RECENT_MAX)))
  }, [here])
  return null
}

/** Switches to a destination: the most recently visited first, but the
 * one on show last, so that Ctrl+K Enter goes back to the one before. */
export function Switcher(props: { close: () => void }) {
  const { path, route } = useLocation()
  const order = recent()
  const rank = (d: Destination) => {
    if (shows(path, d.href)) return Infinity
    const index = order.indexOf(d.href)
    return index === -1 ? order.length : index
  }
  const destinations = useDestinations().sort((a, b) => rank(a) - rank(b))
  return (
    <Picker
      name="Switch to"
      placeholder="Switch to…"
      filterName="Filter views"
      listName="Views"
      empty="Nothing matches"
      items={destinations}
      id={(d) => d.href}
      text={(d) => `${d.name} ${d.account ?? ''}`}
      render={(d) => (
        <span class={styles.destination}>
          {d.lead}
          <span class={styles.name} title={d.name}>
            {d.name}
          </span>
          {d.account !== null && <span class={styles.account}>{d.account}</span>}
        </span>
      )}
      choose={(d) => route(d.href)}
      close={props.close}
    />
  )
}
