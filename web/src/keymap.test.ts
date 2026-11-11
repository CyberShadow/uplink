import { afterEach, expect, it, vi } from 'vitest'

afterEach(() => {
  delete (navigator as { platform?: string }).platform
  vi.resetModules()
})

const binding = async (action: string) => {
  const { KEYMAP } = await import('./keymap')
  return KEYMAP.find((b) => b.action === action)!
}

it('goes to views by Alt+Shift and the key, and labels Ctrl', async () => {
  expect(await binding('goHome')).toMatchObject({ keys: ['g h', 'Alt+Shift+h'], label: 'g h, Alt+Shift+H' })
  expect((await binding('switch')).label).toBe('Ctrl+K')
})

it('on macOS, by Ctrl+Option and where the key is, and labels Cmd', async () => {
  Object.defineProperty(navigator, 'platform', { value: 'MacIntel', configurable: true })
  expect(await binding('goHome')).toMatchObject({ keys: ['g h', 'Control+Alt+KeyH'], label: 'g h, Ctrl+Option+H' })
  expect((await binding('switch')).label).toBe('Cmd+K')
})

it('leaves a box shown unfocused where the pointer is a finger, as on a phone', async () => {
  const box = document.body.appendChild(document.createElement('textarea'))
  const fine = window.matchMedia
  window.matchMedia = (query) => ({ ...fine(query), matches: query === '(pointer: coarse)' })
  try {
    const { focusShown } = await import('./keymap')
    focusShown(box)
    expect(document.activeElement).toBe(document.body)
  } finally {
    window.matchMedia = fine
  }
  vi.resetModules()
  const { focusShown } = await import('./keymap')
  focusShown(box)
  expect(document.activeElement).toBe(box)
  box.remove()
})
