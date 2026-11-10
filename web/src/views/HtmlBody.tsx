// An HTML mail body (SPEC §8.9), already sanitized by the server, shown in
// a sandboxed frame: no scripts, links open in a new tab, and nothing
// remote loads (tracking pixels, fonts) unless asked for.

import { useRef, useState } from 'preact/hooks'
import { keepFocus } from '../keymap'
import styles from './Mail.module.css'

const REMOTE = /<img[^>]+src=["']?https?:|url\(\s*["']?https?:/i

// Mail is written for white paper. In a dark theme, the page is inverted,
// hues kept, as Dark Reader's filter mode does, and pictures inverted back:
// a mail's colours become their dark counterparts, whatever it sets them
// to. Its paper is the inverse of the theme's background (#0d0e0e).
const STYLE = `
html { background: #fff; }
body { margin: 0; padding: 8px; font: 14px/1.4 'Google Sans', Roboto, RobotoDraft, Helvetica, Arial, sans-serif; overflow-wrap: anywhere; color: #000; background: #fff; }
@media (prefers-color-scheme: dark) {
  html { background: #0d0e0e; }
  body { background: #f2f1f1; filter: invert(1) hue-rotate(180deg); }
  img, video, [background], [style*="background-image"] { filter: invert(1) hue-rotate(180deg); }
}`

function document(html: string, remote: boolean): string {
  const images = remote ? 'data: https: http:' : 'data:'
  return `<!doctype html><html><head><meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; img-src ${images}; style-src 'unsafe-inline'">
<base target="_blank">
<style>${STYLE}</style>
</head><body>${html}</body></html>`
}

export function HtmlBody(props: { html: string }) {
  const frame = useRef<HTMLIFrameElement>(null)
  const [remote, setRemote] = useState(false)
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
        srcdoc={document(props.html, remote)}
        onLoad={fit}
      />
    </>
  )
}
