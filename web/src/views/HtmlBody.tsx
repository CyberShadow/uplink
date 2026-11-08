// An HTML mail body (SPEC §8.9), already sanitized by the server, shown in
// a sandboxed frame: no scripts, links open in a new tab, and nothing
// remote loads (tracking pixels, fonts) unless asked for.

import { useRef, useState } from 'preact/hooks'
import { keepFocus } from '../keymap'
import styles from './Mail.module.css'

const REMOTE = /<img[^>]+src=["']?https?:|url\(\s*["']?https?:/i

function document(html: string, remote: boolean): string {
  const images = remote ? 'data: https: http:' : 'data:'
  return `<!doctype html><html><head><meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; img-src ${images}; style-src 'unsafe-inline'">
<base target="_blank">
<style>body { margin: 0; font: 14px/1.4 system-ui, sans-serif; overflow-wrap: anywhere; }</style>
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
