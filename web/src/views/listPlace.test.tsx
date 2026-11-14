import { render, waitFor } from '@testing-library/preact'
import { useRef } from 'preact/hooks'
import { afterEach, beforeEach, expect, it, vi } from 'vitest'
import type { Scrolled } from '../stores/mail'
import { useListPlace } from './listPlace'

/** Rows of 20 pixels, the scroller's top at 0: jsdom lays nothing out. */
const ROW = 20
let scrollTop = 0

beforeEach(() => {
  scrollTop = 0
  vi.spyOn(Element.prototype, 'getBoundingClientRect').mockImplementation(function (this: Element) {
    const index = this.tagName === 'LI' ? [...this.parentElement!.children].indexOf(this) : 0
    const top = this.tagName === 'LI' ? index * ROW - scrollTop : 0
    return { top, bottom: top + ROW } as DOMRect
  })
  Object.defineProperty(HTMLElement.prototype, 'scrollTop', {
    configurable: true,
    get: () => scrollTop,
    set: (value: number) => {
      scrollTop = value
    },
  })
})

afterEach(() => {
  vi.restoreAllMocks()
  delete (HTMLElement.prototype as { scrollTop?: number }).scrollTop
})

function List(props: { keys: string[]; place: { scrolled: Scrolled | null } }) {
  const list = useRef<HTMLUListElement>(null)
  useListPlace(list, props.keys, 'view', props.place)
  return (
    <main>
      <ul ref={list}>
        {props.keys.map((key) => (
          <li key={key}>{key}</li>
        ))}
      </ul>
    </main>
  )
}

it('scrolls a list back to its row at the top, though rows came above it', async () => {
  const place: { scrolled: Scrolled | null } = { scrolled: null }
  const keys = [...'abcdefghij']
  const { container, unmount } = render(<List keys={keys} place={place} />)
  scrollTop = 45
  container.querySelector('main')!.dispatchEvent(new Event('scroll'))
  // `c` is at the top, 5 pixels of it above.
  await waitFor(() => expect(place.scrolled).toEqual({ key: 'c', offset: -5 }))
  unmount()

  // Shown again, a row before it, and scrolled elsewhere meanwhile.
  scrollTop = 0
  render(<List keys={['new', ...keys]} place={place} />)
  expect(scrollTop).toBe(3 * ROW + 5)
})
