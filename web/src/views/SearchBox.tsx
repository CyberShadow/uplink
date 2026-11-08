import { useLocation } from 'preact-iso'
import { searchPath } from './App'
import styles from './App.module.css'

export function SearchBox() {
  const { route, path } = useLocation()
  const shown = path.match(/^\/search\/([^/]+)/)?.[1]
  return (
    <form
      class={styles.search}
      role="search"
      onSubmit={(event) => {
        event.preventDefault()
        const input = event.currentTarget.elements.namedItem('q') as HTMLInputElement
        const query = input.value.trim()
        if (query) route(searchPath(query))
        input.blur()
      }}
    >
      <input
        type="search"
        name="q"
        placeholder="Search mail"
        aria-label="Search mail"
        // Keyed on the route, so the box shows the query on show.
        key={shown}
        defaultValue={shown ? decodeURIComponent(shown) : ''}
        onKeyDown={(event) => {
          if (event.key === 'Escape') event.currentTarget.blur()
        }}
      />
    </form>
  )
}
