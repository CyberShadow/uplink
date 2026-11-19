import { expect, it } from 'vitest'
import { loadEmoji } from '../emoji'
import { QUICK, vertically } from './ReactionPicker'

it('offers the quick reactions as Emojibase has them', async () => {
  const emoji = (await loadEmoji()).map((e) => e.unicode)
  for (const key of QUICK) expect(emoji).toContain(key)
})

const box = (left: number, top: number, width = 30, height = 30) => ({ left, top, right: left + width, bottom: top + height })

it('goes up or down to the nearest line that way, to the choice nearest across', () => {
  // The box, then a heading's gap, then two rows of choices, the last short.
  const input = box(0, 0, 300, 24)
  const first = [0, 34, 68, 102].map((left) => box(left, 40))
  const second = [0, 34].map((left) => box(left, 70))
  const boxes = [input, ...first, ...second]
  expect(vertically(input, boxes, 1)).toBe(1)
  expect(vertically(first[3], boxes, 1)).toBe(6)
  expect(vertically(first[0], boxes, 1)).toBe(5)
  expect(vertically(second[1], boxes, -1)).toBe(2)
  expect(vertically(first[2], boxes, -1)).toBe(0)
  expect(vertically(input, boxes, -1)).toBeUndefined()
  expect(vertically(second[0], boxes, 1)).toBeUndefined()
})

it('goes to the one most across, of touching ones', () => {
  const above = [0, 34, 68].map((left) => box(left, 0, 34))
  expect(vertically(box(34, 40, 34), above, -1)).toBe(1)
  expect(vertically(box(50, 40, 34), above, -1)).toBe(1)
  expect(vertically(box(60, 40, 34), above, -1)).toBe(2)
})
