import { useEffect } from 'preact/hooks'

/** Names, in the window's title, what the view shows: its parts, the most
 * particular first, then the app's name, as Gmail and Element do. */
export function useTitle(...parts: string[]) {
  const title = [...parts, 'uplink'].join(' – ')
  useEffect(() => {
    document.title = title
  }, [title])
}
