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
