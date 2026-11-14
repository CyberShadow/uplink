import { describe, expect, it } from 'vitest'
import type { Identity } from './generated/protocol'
import { defaultAddress, identityOf, isOwn } from './identities'

const identities: Identity[] = [
  { address: 'alice@example.org', name: 'Alice' },
  { domain: 'example.com', name: 'Shop' },
  { address: 'boss@example.com', name: 'The Boss' },
]

describe('identities', () => {
  it('finds an address by its identity, else by its domain', () => {
    expect(identityOf(identities, 'Alice@Example.ORG')).toBe(identities[0])
    expect(identityOf(identities, 'anything@EXAMPLE.com')).toBe(identities[1])
    expect(identityOf(identities, 'boss@example.com')).toBe(identities[2])
    expect(identityOf(identities, 'bob@example.org')).toBeUndefined()
    expect(identityOf(identities, 'x@sub.example.com')).toBeUndefined()
    expect(isOwn(identities, 'example.com')).toBe(false)
    expect(defaultAddress(identities)).toBe('alice@example.org')
  })
})
