import { describe, expect, it } from 'vitest'
import { apply, diff, movePosition, rebase, transform } from './splice'

describe('diff', () => {
  it('finds the changed span', () => {
    expect(diff('hello world', 'hello there world')).toEqual({ offset: 6, delete: '', insert: 'there ' })
    expect(diff('abc', 'axc')).toEqual({ offset: 1, delete: 'b', insert: 'x' })
    expect(diff('abc', '')).toEqual({ offset: 0, delete: 'abc', insert: '' })
    expect(diff('same', 'same')).toBeNull()
  })

  it('keeps surrogate pairs whole', () => {
    // 😀 and 😁 share their high surrogate; 😀 and 🙀 their low one.
    expect(diff('a😀', 'a😁')).toEqual({ offset: 1, delete: '😀', insert: '😁' })
    expect(diff('😀b', '🙀b')).toEqual({ offset: 0, delete: '😀', insert: '🙀' })
  })

  it('round-trips through apply', () => {
    for (const [from, to] of [
      ['', 'x'],
      ['abcabc', 'abc'],
      ['aaa', 'aaaa'],
      ['<p>Hi</p>', '<p>Hi <b>there</b></p>'],
    ]) {
      expect(apply(from, diff(from, to)!)).toBe(to)
    }
  })
})

describe('apply', () => {
  it('refuses an edit that does not match', () => {
    expect(() => apply('abc', { offset: 1, delete: 'x', insert: '' })).toThrow()
  })
})

describe('transform', () => {
  const base = 'The quick brown fox'
  const cases: [string, string, string, string][] = [
    // [local, remote, merged, why]
    ['The very quick brown fox', 'The quick brown fox jumps', 'The very quick brown fox jumps', 'disjoint'],
    ['The quick brown fox jumps', 'The very quick brown fox', 'The very quick brown fox jumps', 'disjoint, local after'],
    ['The quick red fox', 'The quick brown cat', 'The quick red cat', 'adjacent'],
    ['The quick brown foxes', 'The quick brown foxy', 'The quick brown foxesy', 'insertions at one point: local first'],
    ['The fox', 'The quick brown dog', 'The dog', 'overlapping deletion keeps the remote insertion'],
    ['The slow fox', 'The quick brown fox', 'The slow fox', 'identical deletion'],
  ]
  for (const [local, remote, merged, why] of cases) {
    it(why, () => {
      expect(rebase(base, local, remote)).toBe(merged)
    })
  }

  it('keeps both insertions when edits overlap', () => {
    const merged = rebase('abcdef', 'aXf', 'abYef')
    expect(merged).toContain('X')
    expect(merged).toContain('Y')
  })

  it('moves an edit after the remote one', () => {
    expect(transform({ offset: 5, delete: 'x', insert: 'y' }, { offset: 0, delete: 'ab', insert: 'abcd' })).toEqual({
      offset: 7,
      delete: 'x',
      insert: 'y',
    })
  })
})

describe('movePosition', () => {
  it('follows the text around the edit', () => {
    const edit = { offset: 2, delete: 'cd', insert: 'XYZ' }
    expect(movePosition(1, edit)).toBe(1)
    expect(movePosition(2, edit)).toBe(2)
    expect(movePosition(3, edit)).toBe(5)
    expect(movePosition(4, edit)).toBe(5)
    expect(movePosition(6, edit)).toBe(7)
  })
})
