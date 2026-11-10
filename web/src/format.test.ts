import { describe, expect, it } from 'vitest'
import { brief, date, dateTime, time } from './format'

// The tests run in New York (vite.config.ts), here UTC−5.
describe('format', () => {
  const ms = Date.parse('2026-11-10T03:05:00Z')

  it('writes local dates and 24-hour times, whatever the locale', () => {
    expect(time(ms)).toBe('22:05')
    expect(date(new Date(ms))).toBe('2026-11-09')
    expect(dateTime(ms)).toBe('2026-11-09 22:05')
    expect(time(Date.parse('2026-11-10T17:30:00Z'))).toBe('12:30')
  })

  it('briefly gives the time today, and else the date', () => {
    expect(brief(ms, new Date('2026-11-09T23:59:00-05:00'))).toBe('22:05')
    expect(brief(ms, new Date('2026-11-10T00:00:00-05:00'))).toBe('2026-11-09')
  })
})
