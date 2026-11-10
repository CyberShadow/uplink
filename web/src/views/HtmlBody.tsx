// An HTML mail body (SPEC §8.9), already sanitized by the server, shown in
// a sandboxed frame: no scripts, links open in a new tab, and nothing
// remote loads (tracking pixels, fonts). Remote images, which the server
// fetches in the client's stead, load if asked for.

import { useEffect, useRef, useState } from 'preact/hooks'
import { keepFocus } from '../keymap'
import styles from './Mail.module.css'

/** The server's rewriting of a remote image's URL, to fetch it itself. */
const REMOTE = /\/api\/remote-image\?url=/

// Mail is shown as for white paper: the frame is in a light colour scheme
// (Mail.module.css), so that mail with a dark mode of its own, often
// partial, keeps to its light one. In a dark theme, the page is then
// inverted, hues kept, as Dark Reader's filter mode does, and pictures
// inverted back: a mail's colours become their dark counterparts,
// whatever it sets them to. Its paper is the inverse of the theme's
// background (#0d0e0e).
const STYLE = `
html { background: #fff; }
body { margin: 0; padding: 8px; font: 14px/1.4 'Google Sans', Roboto, RobotoDraft, Helvetica, Arial, sans-serif; overflow-wrap: anywhere; color: #000; background: #fff; }
html.dark { background: #0d0e0e; }
html.dark body { background: #f2f1f1; filter: invert(1) hue-rotate(180deg); }
html.dark :is(img, video, [background], [style*="background-image"]) { filter: invert(1) hue-rotate(180deg); }`

const DARK = '(prefers-color-scheme: dark)'

/** Whether the theme is dark, as the system's is, following it. */
function useDark(): boolean {
  const [dark, setDark] = useState(() => matchMedia(DARK).matches)
  useEffect(() => {
    const query = matchMedia(DARK)
    const changed = () => setDark(query.matches)
    query.addEventListener('change', changed)
    return () => query.removeEventListener('change', changed)
  }, [])
  return dark
}

function document(html: string, remote: boolean, dark: boolean): string {
  // The frame's origin is uplink's, the server's fetches of remote images.
  const images = remote ? "data: 'self'" : 'data:'
  return `<!doctype html><html${dark ? ' class="dark"' : ''}><head><meta charset="utf-8"><meta name="darkreader-lock">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; img-src ${images}; style-src 'unsafe-inline'">
<base target="_blank">
<style>${STYLE}</style>
</head><body>${html}</body></html>`
}

export function HtmlBody(props: { html: string }) {
  const frame = useRef<HTMLIFrameElement>(null)
  const [remote, setRemote] = useState(false)
  const dark = useDark()
  // Its own scrolling would nest in the message's: fit it to its content.
  const fit = () => {
    const body = frame.current?.contentDocument?.documentElement
    if (body) frame.current!.style.height = `${body.scrollHeight}px`
  }
  return (
    <>
      {!remote && REMOTE.test(props.html) && (
        <p class={styles.remote}>
          Remote images are not shown.{' '}
          <button type="button" onMouseDown={keepFocus} onClick={() => setRemote(true)}>
            Show them
          </button>
        </p>
      )}
      <iframe
        ref={frame}
        class={styles.html}
        title="Message body"
        // Same origin, without scripts, so that the frame can be measured;
        // popups, so that links open.
        sandbox="allow-same-origin allow-popups allow-popups-to-escape-sandbox"
        srcdoc={document(props.html, remote, dark)}
        onLoad={fit}
      />
    </>
  )
}
