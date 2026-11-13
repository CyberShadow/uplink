import { find } from 'linkifyjs'

/** Text, in parts: links, and the rest. */
export type Linked = Array<{ text: string; href: string | null }>

/** Plain text, in parts: links to what look like web addresses, and
 * email addresses; and the rest. A web address is linked with its scheme
 * or of a `www.` host, as Thunderbird links them: a name with a dot and
 * no scheme is as likely a file's (`README.md`). Of the schemes, only
 * those of the web and of mail: no `file:` links. */
export function linkify(text: string): Linked {
  const parts: Linked = []
  let last = 0
  for (const link of find(text, { defaultProtocol: 'https' })) {
    if (link.type === 'url' && !/^(?:[a-z][\da-z+.-]*:|www\.)/i.test(link.value)) continue
    if (!SCHEME.test(link.href)) continue
    if (link.start > last) parts.push({ text: text.slice(last, link.start), href: null })
    parts.push({ text: link.value, href: link.href })
    last = link.end
  }
  if (last < text.length) parts.push({ text: text.slice(last), href: null })
  return parts
}

const SCHEME = /^(?:https?|ftp|mailto):/i

/** HTML with the text of `linked`'s parts linked where it is not yet:
 * not in links, nor in code. The links open in a new tab, as those of
 * the HTML (sanitized by the server) do. */
export function linkifyHtml(html: string, linked: (text: string) => Linked): string {
  // Inert: nothing in it loads.
  const template = document.createElement('template')
  template.innerHTML = html
  const walker = document.createTreeWalker(template.content, NodeFilter.SHOW_TEXT)
  const texts: Text[] = []
  for (let node = walker.nextNode(); node !== null; node = walker.nextNode()) {
    // Of the root, a text has no parent element.
    if (!node.parentElement?.closest('a, code, pre')) texts.push(node as Text)
  }
  for (const node of texts) {
    const parts = linked(node.data)
    if (parts.every((part) => part.href === null)) continue
    node.replaceWith(
      ...parts.map((part) => {
        if (part.href === null) return part.text
        const anchor = document.createElement('a')
        anchor.href = part.href
        anchor.target = '_blank'
        anchor.rel = 'noopener noreferrer'
        anchor.textContent = part.text
        return anchor
      }),
    )
  }
  return template.innerHTML
}
