import { useLocation } from 'preact-iso'
import { useServices } from '../context'
import { searchPath } from './App'
import { roomSearchPath } from './Room'
import styles from './App.module.css'

/** The box at the top: it searches the room on show, or else everything:
 * mail, rooms and their messages, to-dos and events. */
export function SearchBox() {
  const { route, path } = useLocation()
  const { matrix } = useServices()
  const roomId = path.match(/^\/rooms\/(\d+)/)?.[1]
  const room = roomId === undefined ? undefined : matrix.rooms.value?.find((r) => String(r.id) === roomId)
  const shown = path.match(room ? /^\/rooms\/\d+\/search\/([^/]+)/ : /^\/search\/([^/]+)/)?.[1]
  const label = room ? `Search ${room.name}` : 'Search'
  return (
    <form
      class={styles.search}
      role="search"
      onSubmit={(event) => {
        event.preventDefault()
        const input = event.currentTarget.elements.namedItem('q') as HTMLInputElement
        const query = input.value.trim()
        if (query) route(room ? roomSearchPath(room.id, query) : searchPath(query))
        input.blur()
      }}
    >
      <input
        type="search"
        name="q"
        placeholder={label}
        aria-label={label}
        // Keyed on the route, so the box shows the query on show.
        key={`${roomId}:${shown}`}
        defaultValue={shown ? decodeURIComponent(shown) : ''}
        onKeyDown={(event) => {
          if (event.key === 'Escape') event.currentTarget.blur()
        }}
      />
      {/* A room's search has no operators. */}
      {!room && (
        <a href="/help/search" class={styles.searchHelp} title="Search help" aria-label="Search help">
          ?
        </a>
      )}
    </form>
  )
}
