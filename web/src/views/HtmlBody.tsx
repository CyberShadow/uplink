// An HTML mail body (SPEC §8.9), already sanitized by the server, shown in
// a sandboxed frame: no scripts, links open in a new tab, and nothing
// remote loads (tracking pixels, fonts). Remote images, which the server
// fetches in the client's stead, load by host, as asked for; likely
// tracking pixels, never.

import { useEffect, useMemo, useState } from 'preact/hooks'
import type { RemoteHost } from '../generated/protocol'
import { keepFocus, takeKeys } from '../keymap'
import styles from './Mail.module.css'
import { MenuButton, type MenuItem } from './Menu'
import { QUOTE_CLASS, foldHtml } from './quote'

/** Where the server fetches a host's remote images for a mail:
 * `<path>?url=…`. */
function remotePath(host: string): string {
  return `/api/remote-image/${host}`
}

// Mail is shown as for white paper: the frame is in a light colour scheme
// (Mail.module.css), so that mail with a dark mode of its own, often
// partial, keeps to its light one. In a dark theme, the page is then
// inverted, hues kept, as Dark Reader's filter mode does, and pictures
// inverted back: a mail's colours become their dark counterparts,
// whatever it sets them to. Its paper is the inverse of the theme's
// background (#0d0e0e). The frame is fitted to its root, so the root is
// as tall as its content: a mail's own height for it, as the frame's
// (html, body { height: 100% }, common in mail), would keep the frame at
// its first height, scrolling.
const STYLE = `
html { height: auto !important; min-height: 0 !important; background: #fff; }
body { margin: 0; padding: 8px; font: 14px/1.4 'Google Sans', Roboto, RobotoDraft, Helvetica, Arial, sans-serif; overflow-wrap: anywhere; color: #000; background: #fff; }
html.dark { background: #0d0e0e; }
html.dark body { background: #f2f1f1; filter: invert(1) hue-rotate(180deg); }
html.dark :is(img, video, [background], [style*="background-image"]) { filter: invert(1) hue-rotate(180deg); }
html:not(.quote) .${QUOTE_CLASS} { display: none; }`

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

/** `html` with its `<style>` elements marked as shown with the remote
 * images of `hosts`. Firefox reuses a sheet it parsed for a document
 * before, of the same text, with its images as that document's CSP
 * refused them: the frame's, shown with fewer. */
function remoteShown(html: string, hosts: string[]): string {
  // Inert: nothing in it loads.
  const template = document.createElement('template')
  template.innerHTML = html
  for (const style of template.content.querySelectorAll('style')) {
    style.append(`/* remote images shown: ${hosts.join(' ')} */`)
  }
  return template.innerHTML
}

function frameDocument(html: string, shown: ReadonlySet<string>, classes: string): string {
  // The server's fetches of the remote images of the hosts shown, at
  // uplink's origin, by path (exactly: the query is not matched). The
  // origin by name, as Firefox does not take a srcdoc frame's 'self' for
  // its (inherited) origin. Likely tracking pixels' path is never here.
  const hosts = [...shown].sort()
  const images = ['data:', ...hosts.map((host) => location.origin + remotePath(host))].join(' ')
  return `<!doctype html><html class="${classes}"><head><meta charset="utf-8"><meta name="darkreader-lock">
<meta http-equiv="x-dns-prefetch-control" content="off">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; img-src ${images}; style-src 'unsafe-inline'">
<base target="_blank">
<style>${STYLE}</style>
</head><body>${hosts.length > 0 ? remoteShown(html, hosts) : html}</body></html>`
}

/** `fold`: whether to fold away the quote of the mail it answers, if it
 * ends with one (SPEC §12.8). `hosts`: those of its remote images;
 * `shown`, those shown unasked, by the user's rules; `rules`, rules to
 * have more shown so; `pixels`, how many likely tracking pixels it has,
 * which never load. */
export function HtmlBody(props: {
  html: string
  fold: boolean
  hosts: RemoteHost[]
  shown: ReadonlySet<string>
  rules: MenuItem[]
  pixels: number
}) {
  const [asked, setAsked] = useState(false)
  const shown = asked ? new Set(props.hosts.map((h) => h.host)) : props.shown
  const hidden = props.hosts.filter((h) => !shown.has(h.host))
  const [quoteShown, setQuoteShown] = useState(false)
  const dark = useDark()
  const [html, folded] = useMemo(
    () => (props.fold ? foldHtml(props.html) : [props.html, false]),
    [props.html, props.fold],
  )
  const classes = [dark && 'dark', quoteShown && 'quote'].filter(Boolean).join(' ')
  // Each document the frame loads: its keys are the app's, and it is
  // fitted to its content, its own scrolling nesting in the message's
  // otherwise. By the root's box, not its scrollHeight, which is never less
  // than the frame's height: the frame could grow but never shrink. (The
  // frame, by the event: it may load as it is inserted, before a ref is
  // set.)
  const loaded = (frame: HTMLIFrameElement) => {
    takeKeys(frame.contentWindow!)
    const root = frame.contentDocument!.documentElement
    frame.style.height = `${Math.ceil(root.getBoundingClientRect().height)}px`
  }
  return (
    <>
      {hidden.length > 0 && (
        <p class={styles.remote}>
          {shown.size === 0 ? 'Remote images are not shown.' : 'Some remote images are not shown.'}{' '}
          <button type="button" onMouseDown={keepFocus} onClick={() => setAsked(true)}>
            Show them
          </button>
          {props.rules.length > 0 && (
            <>
              {' · '}
              <MenuButton label="Always show" groups={[props.rules]} start />
            </>
          )}
          {props.pixels > 0 && ` · ${trackers(props.pixels)} blocked.`}
        </p>
      )}
      <iframe
        class={styles.html}
        title="Message body"
        // Same origin, without scripts, so that the frame can be measured
        // (with scripts too, the mail could lift its own sandbox); popups,
        // so that links open.
        sandbox="allow-same-origin allow-popups allow-popups-to-escape-sandbox"
        srcdoc={frameDocument(html, shown, classes)}
        onLoad={(event) => loaded(event.currentTarget)}
      />
      {folded && <QuoteToggle shown={quoteShown} toggle={() => setQuoteShown(!quoteShown)} />}
    </>
  )
}

function trackers(n: number): string {
  return n === 1 ? '1 likely tracker' : `${n} likely trackers`
}

/** Gmail's "•••", which shows or hides a message's quote. */
export function QuoteToggle(props: { shown: boolean; toggle: () => void }) {
  const label = props.shown ? 'Hide trimmed content' : 'Show trimmed content'
  return (
    <button
      type="button"
      class={styles.quoteToggle}
      aria-label={label}
      title={label}
      aria-expanded={props.shown}
      onMouseDown={keepFocus}
      onClick={props.toggle}
    >
      •••
    </button>
  )
}
