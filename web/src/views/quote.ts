// Where a message quotes the mail it answers, to fold it away as Gmail
// does (SPEC §12.8). The quote is what follows its first mark, if only
// quote follows: a quote with replies written in between stays.

/** Lines that end an attribution ("On …, Alice wrote:"), in a few languages. */
const WROTE = /(wrote|schrieb|a écrit|escribió|scrisse|написал|написала)\s*:\s*$/i

/** Lines from which the rest is the original: Outlook's. */
const SEPARATOR = /^\s*(-{2,}\s*Original Message\s*-{2,}|_{20,})\s*$/i

/** Whether a message with `subject` forwards mail, which it shows whole. */
export function forwards(subject: string | null): boolean {
  return /^\s*(fwd?|fw):/i.test(subject ?? '')
}

/** `text` split where its quote begins: what was written, and the quote,
 * if there is one. */
export function splitText(text: string): [string, string | null] {
  const lines = text.split('\n')
  const blank = (i: number) => lines[i].trim() === ''
  const quoted = (i: number) => lines[i].trimStart().startsWith('>')
  let cut = lines.findIndex((line) => SEPARATOR.test(line))
  // The trailing run of quoted lines.
  let end = lines.length
  while (end > 0 && blank(end - 1)) end--
  let start = end
  while (start > 0 && (quoted(start - 1) || blank(start - 1))) start--
  while (start < end && blank(start)) start++
  if (start < end && quoted(start)) {
    let first = start
    // Its attribution, which may wrap.
    let above = first - 1
    while (above >= 0 && blank(above)) above--
    if (above >= 0 && WROTE.test(lines[above])) {
      first = above
      if (above > 0 && !blank(above - 1) && /^\s*On\s/.test(lines[above - 1]) && !/^\s*On\s/.test(lines[above])) {
        first = above - 1
      }
    }
    if (cut === -1 || first < cut) cut = first
  }
  if (cut === -1) return [text, null]
  const written = lines.slice(0, cut).join('\n').trimEnd()
  if (written.trim() === '') return [text, null]
  return [written, lines.slice(cut).join('\n')]
}

/** Marks that the rest is the original, wherever it ends. */
const SEPARATORS = '#divRplyFwdMsg, #appendonsend'
/** Quotes, which must end the message. */
const QUOTES = '.gmail_quote, .gmail_quote_container, .yahoo_quoted, blockquote'
/** Attributions, which go with the quote after them. */
const ATTRIBUTIONS = '.moz-cite-prefix, .gmail_attr'

/** The class of the element `foldHtml` puts the quote in. */
export const QUOTE_CLASS = 'uplink-quote'

/** `html`, with its quote, if any, moved into an element of
 * `QUOTE_CLASS` at its end; and whether there was one. */
export function foldHtml(html: string): [string, boolean] {
  // Inert: nothing in it loads.
  const template = document.createElement('template')
  template.innerHTML = html
  const root = template.content
  const range = (from: Node, after: boolean) => {
    const r = document.createRange()
    if (after) r.setStartAfter(from)
    else r.setStartBefore(from)
    r.setEnd(root, root.childNodes.length)
    return r
  }
  const before = (node: Node) => {
    const r = document.createRange()
    r.setStart(root, 0)
    r.setEndBefore(node)
    return r.toString()
  }
  const candidates = [...root.querySelectorAll(`${SEPARATORS}, ${QUOTES}, div[style*="border-top"]`)]
  const mark = candidates.find((el) => {
    if (el.matches(SEPARATORS)) return true
    // Outlook's header of the original: a ruled box starting "From:".
    if (el.matches('div[style*="border-top"]')) return /^\s*From:/.test(el.textContent ?? '')
    return range(el, true).toString().trim() === ''
  })
  const start = mark && (attribution(mark) ?? mark)
  // A message of nothing but quote shows it.
  if (!start || before(start).trim() === '') return [html, false]
  const quote = document.createElement('div')
  quote.className = QUOTE_CLASS
  quote.append(range(start, false).extractContents())
  root.append(quote)
  return [template.innerHTML, true]
}

/** The attribution heading `quote`, if one does: the element before it,
 * but for line breaks; or Outlook's rule above its header. */
function attribution(quote: Element): Element | null {
  let node = quote.previousSibling
  while (
    node &&
    ((node.nodeType === Node.TEXT_NODE && node.textContent!.trim() === '') ||
      (node instanceof Element && node.tagName === 'BR'))
  ) {
    node = node.previousSibling
  }
  if (!(node instanceof Element)) return null
  return node.matches(`${ATTRIBUTIONS}, hr`) || WROTE.test(node.textContent ?? '') ? node : null
}
